//! Builds the provider context from the event log and reports it as a ledger.
//!
//! The ledger is the honest answer to "what is actually in the prompt": every
//! sector, entry and token is derived from the same events the request is built
//! from. Policies (drop / pin / compact) are ordinary events; replaying them is
//! deterministic, so a restart never changes what a session sends.
//!
//! Budgets are soft. `tools` and `history` are trimmed oldest-first to their
//! share of `context.budget_tokens`, but the newest few entries (and anything
//! pinned) always survive. `system`, `instructions` and `workspace` are only
//! reported, never dropped automatically: silently hiding an `AGENTS.md` would
//! change the agent's behaviour behind the user's back.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use eix_core::{
    AssistantMessagePayload, ChatMessage, Config, ContextCompactedPayload, ContextDropPayload,
    ContextEntryInfo, ContextLedgerResult, ContextPinPayload, ContextSectorInfo, Event, EventKind,
    ImagePart, Sector, ToolResultPayload, UserMessagePayload,
};
use serde::de::DeserializeOwned;
use tokio::time::{timeout, Duration};

use crate::blob::BlobStore;

/// Never auto-drop the newest few entries in a sector: the model still needs
/// the tool output it just received.
const KEEP_NEWEST: usize = 4;
const MAX_DETAIL_CHARS: usize = 2_000;
const MAX_PREVIEW_CHARS: usize = 160;
const MAX_INSTRUCTION_CHARS: usize = 64_000;

pub struct Ledger {
    entries: Vec<Entry>,
    compactions: Vec<Compaction>,
    result: ContextLedgerResult,
}

struct Entry {
    info: ContextEntryInfo,
    messages: Vec<ChatMessage>,
    /// Source event sequence for entries derived from the log.
    seq: Option<u64>,
    /// Compaction replacements and synthesized sectors are never trimmed.
    trimmable: bool,
    summary: bool,
}

#[derive(Debug, Clone)]
struct Compaction {
    from_seq: u64,
    to_seq: u64,
    messages: usize,
    tokens: usize,
    summary: String,
}

#[derive(Default)]
struct Policy {
    pinned: HashSet<String>,
    dropped: HashSet<String>,
    compactions: Vec<Compaction>,
    /// Messages at or before this sequence are excluded from the provider
    /// context (a `/clear` boundary).
    cleared_seq: Option<u64>,
}

impl Policy {
    fn from_events(events: &[Event]) -> Self {
        let mut policy = Self::default();
        for event in events {
            match event.kind {
                EventKind::ContextPinned => {
                    if let Some(value) = decode::<ContextPinPayload>(event) {
                        if value.pinned {
                            policy.pinned.insert(value.entry);
                        } else {
                            policy.pinned.remove(&value.entry);
                        }
                    }
                }
                EventKind::ContextDropped => {
                    if let Some(value) = decode::<ContextDropPayload>(event) {
                        if value.dropped {
                            policy.dropped.insert(value.entry);
                        } else {
                            policy.dropped.remove(&value.entry);
                        }
                    }
                }
                EventKind::ContextCompacted => {
                    if let Some(value) = decode::<ContextCompactedPayload>(event) {
                        policy.compactions.push(Compaction {
                            from_seq: value.from_seq,
                            to_seq: value.to_seq,
                            messages: value.messages,
                            tokens: value.tokens,
                            summary: value.summary,
                        });
                    }
                }
                EventKind::ContextCleared => {
                    // The clear boundary supersedes every earlier policy: all
                    // prior messages (and any summaries covering them) leave
                    // the context.
                    policy.cleared_seq = Some(event.seq);
                    policy.compactions.clear();
                }
                _ => {}
            }
        }
        policy
    }
}

impl Ledger {
    /// Load instructions and the workspace summary, then assemble the ledger.
    pub async fn build(config: &Config, system: String, cwd: &Path, events: &[Event]) -> Self {
        Self::build_with_blobs(config, system, cwd, events, None).await
    }

    /// Like [`build`](Self::build), but resolves image blob references so
    /// provider messages contain the original bytes.
    pub async fn build_with_blobs(
        config: &Config,
        system: String,
        cwd: &Path,
        events: &[Event],
        blobs: Option<&BlobStore>,
    ) -> Self {
        let instructions = load_instructions(config, cwd).await;
        let workspace = if config.context.workspace_summary {
            workspace_summary(cwd).await
        } else {
            None
        };
        let images = load_images(events, blobs).await;
        Self::assemble_with_images(
            system,
            instructions,
            workspace,
            events,
            config.context.budget_tokens,
            &images,
        )
    }

    /// Pure assembly, used by tests and by [`build`](Self::build).
    pub fn assemble(
        system: String,
        instructions: Vec<(String, String)>,
        workspace: Option<String>,
        events: &[Event],
        budget_tokens: usize,
    ) -> Self {
        Self::assemble_with_images(
            system,
            instructions,
            workspace,
            events,
            budget_tokens,
            &HashMap::new(),
        )
    }

    fn assemble_with_images(
        system: String,
        instructions: Vec<(String, String)>,
        workspace: Option<String>,
        events: &[Event],
        budget_tokens: usize,
        images: &HashMap<String, ImagePart>,
    ) -> Self {
        let policy = Policy::from_events(events);
        let mut entries = Vec::new();

        entries.push(synthesized(
            "system",
            Sector::System,
            "system",
            system,
            false,
            &policy,
        ));
        for (label, text) in instructions {
            entries.push(synthesized(
                &format!("instructions:{label}"),
                Sector::Instructions,
                "instructions",
                text,
                true,
                &policy,
            ));
        }
        if let Some(text) = workspace {
            entries.push(synthesized(
                "workspace",
                Sector::Workspace,
                "workspace",
                text,
                true,
                &policy,
            ));
        }
        entries.extend(event_entries(events, &policy, images));

        let entries = apply_compactions(entries, &policy.compactions);
        let mut entries = entries;
        let trimmed = trim(&mut entries, budget_tokens);
        let result = summarize(&entries, budget_tokens, trimmed);

        Self {
            entries,
            compactions: policy.compactions,
            result,
        }
    }

    /// Provider messages for everything still in the context.
    pub fn messages(&self) -> Vec<ChatMessage> {
        let mut messages = Vec::new();
        for entry in &self.entries {
            if entry.info.dropped || entry.info.compacted {
                continue;
            }
            messages.extend(entry.messages.iter().cloned());
        }
        messages
    }

    pub fn info(&self) -> ContextLedgerResult {
        self.result.clone()
    }

    pub fn has_entry(&self, id: &str) -> bool {
        self.entries.iter().any(|entry| entry.info.id == id)
    }

    pub fn is_editable(&self, id: &str) -> bool {
        self.entries
            .iter()
            .find(|entry| entry.info.id == id)
            .is_some_and(|entry| entry.info.editable)
    }

    /// Whether the range covers at least one message-bearing entry.
    pub fn range_stats(&self, from_seq: u64, to_seq: u64) -> (usize, usize) {
        let mut messages = 0;
        let mut tokens = 0;
        for entry in &self.entries {
            if let Some(seq) = entry.seq {
                if from_seq <= seq && seq <= to_seq {
                    messages += entry.messages.len();
                    tokens += entry.info.tokens;
                }
            }
        }
        (messages, tokens)
    }

    /// Inclusive `(first, last)` source sequence of everything still in the
    /// context. `None` when the context holds no messages yet.
    pub fn message_range(&self) -> Option<(u64, u64)> {
        let mut range: Option<(u64, u64)> = None;
        for entry in &self.entries {
            if entry.info.dropped || entry.info.compacted {
                continue;
            }
            if let Some(seq) = entry.seq {
                range = Some(match range {
                    Some((from, to)) => (from.min(seq), to.max(seq)),
                    None => (seq, seq),
                });
            }
        }
        range
    }

    /// Compactions may not overlap: two summaries over the same events would
    /// double-count and make the replay order-dependent.
    pub fn overlaps_compaction(&self, from_seq: u64, to_seq: u64) -> bool {
        self.compactions
            .iter()
            .any(|other| from_seq <= other.to_seq && other.from_seq <= to_seq)
    }

    /// Messages inside an inclusive source range, skipping dropped and already
    /// compacted entries.
    pub fn messages_in_range(&self, from_seq: u64, to_seq: u64) -> Vec<ChatMessage> {
        let mut messages = Vec::new();
        for entry in &self.entries {
            if entry.info.dropped || entry.info.compacted {
                continue;
            }
            if entry
                .seq
                .is_some_and(|seq| from_seq <= seq && seq <= to_seq)
            {
                messages.extend(entry.messages.iter().cloned());
            }
        }
        messages
    }

    /// Range an auto-compaction should summarize: everything before the last
    /// `keep_turns` user messages. `None` when there is not enough history.
    pub fn auto_compact_range(&self, keep_turns: usize) -> Option<(u64, u64)> {
        let keep_turns = keep_turns.max(1);
        let mut users: Vec<u64> = self
            .entries
            .iter()
            .filter(|entry| !entry.info.dropped && !entry.info.compacted)
            .filter(|entry| entry.info.kind == "user")
            .filter_map(|entry| entry.seq)
            .collect();
        users.sort_unstable();
        users.dedup();
        if users.len() <= keep_turns {
            return None;
        }
        let cutoff = users[users.len() - keep_turns];
        let (from, _) = self.message_range()?;
        if cutoff <= from {
            return None;
        }
        Some((from, cutoff - 1))
    }
}

fn synthesized(
    id: &str,
    sector: Sector,
    kind: &str,
    text: String,
    editable: bool,
    policy: &Policy,
) -> Entry {
    let message = ChatMessage::system(text.clone());
    let mut entry = make_entry(
        id.to_string(),
        sector,
        kind.to_string(),
        vec![message],
        None,
        editable,
    );
    entry.info.pinned = policy.pinned.contains(id);
    entry.info.dropped = policy.dropped.contains(id);
    entry
}

async fn load_images(events: &[Event], blobs: Option<&BlobStore>) -> HashMap<String, ImagePart> {
    use base64::Engine;
    let Some(blobs) = blobs else {
        return HashMap::new();
    };
    let mut mimes: HashMap<String, String> = HashMap::new();
    for event in events {
        if event.kind != EventKind::UserMessage {
            continue;
        }
        let Some(payload) = decode::<UserMessagePayload>(event) else {
            continue;
        };
        for image in payload.images {
            if let Some(blob) = image.blob {
                mimes.entry(blob).or_insert(image.mime);
            }
        }
    }
    let ids: HashSet<String> = mimes.keys().cloned().collect();
    let bytes = blobs.load_many(&ids).await;
    bytes
        .into_iter()
        .filter_map(|(id, data)| {
            let mime = mimes.remove(&id)?;
            Some((
                id,
                ImagePart {
                    mime,
                    data: base64::engine::general_purpose::STANDARD.encode(data),
                },
            ))
        })
        .collect()
}

fn event_entries(
    events: &[Event],
    policy: &Policy,
    blob_images: &HashMap<String, ImagePart>,
) -> Vec<Entry> {
    // Tool results are matched to the assistant message that requested them so
    // a dropped or compacted step never produces a `tool` message without its
    // preceding `tool_calls`.
    let mut results: HashMap<String, (u64, String, String)> = HashMap::new();
    for event in events {
        if event.kind == EventKind::ToolResult {
            if let Some(payload) = decode::<ToolResultPayload>(event) {
                results.insert(
                    payload.call_id.clone(),
                    (event.seq, payload.tool.clone(), payload.output.clone()),
                );
            }
        }
    }

    let mut claimed: HashSet<u64> = HashSet::new();
    let mut entries = Vec::new();
    for event in events {
        match event.kind {
            EventKind::UserMessage => {
                let Some(payload) = decode::<UserMessagePayload>(event) else {
                    continue;
                };
                let images: Vec<ImagePart> = payload
                    .images
                    .iter()
                    .filter_map(|image| {
                        if let Some(blob) = &image.blob {
                            return blob_images.get(blob).cloned();
                        }
                        (!image.data.is_empty()).then(|| ImagePart {
                            mime: image.mime.clone(),
                            data: image.data.clone(),
                        })
                    })
                    .collect();
                let message = if images.is_empty() {
                    ChatMessage::user(payload.text.clone())
                } else {
                    ChatMessage::user_with_images(payload.text.clone(), images)
                };
                let mut entry = make_entry(
                    format!("e{}", event.seq),
                    Sector::History,
                    "user".to_string(),
                    vec![message],
                    Some(event.seq),
                    true,
                );
                entry.info.preview = snippet(&first_line(&payload.text), MAX_PREVIEW_CHARS);
                entries.push(with_policy(entry, policy));
            }
            EventKind::AssistantMessage => {
                let Some(payload) = decode::<AssistantMessagePayload>(event) else {
                    continue;
                };
                if payload.text.trim().is_empty() && payload.tool_calls.is_empty() {
                    continue;
                }
                let mut tool_messages = Vec::new();
                let mut tool_previews = Vec::new();
                // An assistant `tool_calls` block is only valid to the
                // provider when every listed call has a matching tool
                // message. If the turn died mid-round, keep only the calls
                // that actually produced output.
                let mut completed = Vec::new();
                for call in &payload.tool_calls {
                    if let Some((seq, tool, output)) = results.remove(&call.id) {
                        claimed.insert(seq);
                        tool_previews.push(format!("{tool}: {}", first_line(&output)));
                        completed.push(call.clone());
                        tool_messages.push(ChatMessage::tool(call.id.clone(), output));
                    }
                }
                let used_tools = !completed.is_empty();
                let mut messages = Vec::new();
                if used_tools {
                    let call_count = completed.len();
                    messages.push(ChatMessage::assistant_with_tools(
                        payload.text.clone(),
                        completed,
                    ));
                    messages.extend(tool_messages);
                    let preview = format!("{call_count} call(s) · {}", tool_previews.join(" · "));
                    let mut entry = make_entry(
                        format!("e{}", event.seq),
                        Sector::Tools,
                        "tool_step".to_string(),
                        messages,
                        Some(event.seq),
                        true,
                    );
                    entry.info.preview = snippet(&preview, MAX_PREVIEW_CHARS);
                    entries.push(with_policy(entry, policy));
                } else {
                    // Never emit a bare `tool_calls` block without results.
                    if payload.text.trim().is_empty() {
                        continue;
                    }
                    let mut entry = make_entry(
                        format!("e{}", event.seq),
                        Sector::History,
                        "assistant".to_string(),
                        vec![ChatMessage::assistant(payload.text.clone())],
                        Some(event.seq),
                        true,
                    );
                    entry.info.preview = snippet(&first_line(&payload.text), MAX_PREVIEW_CHARS);
                    entries.push(with_policy(entry, policy));
                }
            }
            EventKind::ToolResult => {
                if claimed.contains(&event.seq) {
                    continue;
                }
                let Some(payload) = decode::<ToolResultPayload>(event) else {
                    continue;
                };
                let message = ChatMessage::tool(payload.call_id.clone(), payload.output.clone());
                let mut entry = make_entry(
                    format!("e{}", event.seq),
                    Sector::Tools,
                    "tool".to_string(),
                    vec![message],
                    Some(event.seq),
                    true,
                );
                entry.info.preview = snippet(
                    &format!("{}: {}", payload.tool, first_line(&payload.output)),
                    MAX_PREVIEW_CHARS,
                );
                entries.push(with_policy(entry, policy));
            }
            _ => {}
        }
    }
    if let Some(cleared) = policy.cleared_seq {
        for entry in &mut entries {
            if entry.seq.is_some_and(|seq| seq <= cleared) {
                entry.info.dropped = true;
            }
        }
    }
    entries
}

fn with_policy(mut entry: Entry, policy: &Policy) -> Entry {
    entry.info.pinned = policy.pinned.contains(&entry.info.id);
    entry.info.dropped = policy.dropped.contains(&entry.info.id);
    entry
}

fn make_entry(
    id: String,
    sector: Sector,
    kind: String,
    messages: Vec<ChatMessage>,
    seq: Option<u64>,
    editable: bool,
) -> Entry {
    let mut tokens = 0;
    let mut chars = 0;
    let mut parts = Vec::new();
    for message in &messages {
        // Role/formatting overhead, roughly what chat templates add.
        tokens += estimate_tokens(&message.content) + 4;
        chars += message.content.chars().count();
        if !message.content.is_empty() {
            parts.push(message.content.clone());
        }
    }
    let detail = snippet(&parts.join("\n\n"), MAX_DETAIL_CHARS);
    let preview = snippet(&first_line(&parts.join("\n")), MAX_PREVIEW_CHARS);
    let trimmable = editable && matches!(sector, Sector::History | Sector::Tools);
    Entry {
        info: ContextEntryInfo {
            id,
            sector,
            kind,
            tokens,
            chars,
            preview,
            detail,
            pinned: false,
            dropped: false,
            compacted: false,
            auto: false,
            editable,
        },
        messages,
        seq,
        trimmable,
        summary: false,
    }
}

fn apply_compactions(entries: Vec<Entry>, compactions: &[Compaction]) -> Vec<Entry> {
    if compactions.is_empty() {
        return entries;
    }
    let mut emitted: HashSet<(u64, u64)> = HashSet::new();
    let mut out = Vec::with_capacity(entries.len());
    for mut entry in entries {
        let covered = entry
            .seq
            .and_then(|seq| {
                compactions
                    .iter()
                    .find(|c| c.from_seq <= seq && seq <= c.to_seq)
            })
            .cloned();
        if let Some(compaction) = covered {
            entry.info.compacted = true;
            if emitted.insert((compaction.from_seq, compaction.to_seq)) {
                out.push(summary_entry(&compaction));
            }
            // Keep the original for the expand view; `messages()` skips it.
            out.push(entry);
            continue;
        }
        out.push(entry);
    }
    for compaction in compactions {
        if emitted.insert((compaction.from_seq, compaction.to_seq)) {
            out.push(summary_entry(compaction));
        }
    }
    out
}

fn summary_entry(compaction: &Compaction) -> Entry {
    let text = format!(
        "[compacted {} messages (~{} tokens)]\n{}",
        compaction.messages, compaction.tokens, compaction.summary
    );
    let mut entry = make_entry(
        format!("c{}-{}", compaction.from_seq, compaction.to_seq),
        Sector::History,
        "summary".to_string(),
        vec![ChatMessage::system(text)],
        None,
        false,
    );
    entry.summary = true;
    entry.trimmable = false;
    entry
}

/// Drop old entries until the sector fits `cap`, always keeping the newest
/// [`KEEP_NEWEST`] and anything pinned. Returns nothing; callers read the
/// `auto` flags afterwards.
fn trim_sector(entries: &mut [Entry], sector: Sector, cap: usize) {
    let mut total: usize = entries
        .iter()
        .filter(|entry| entry.info.sector == sector && !entry.info.dropped && !entry.info.compacted)
        .map(|entry| entry.info.tokens)
        .sum();
    if total <= cap {
        return;
    }
    let candidates: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry.trimmable
                && entry.info.sector == sector
                && !entry.summary
                && !entry.info.pinned
                && !entry.info.dropped
        })
        .map(|(index, _)| index)
        .collect();
    let keep = KEEP_NEWEST.min(candidates.len());
    for &index in &candidates[..candidates.len() - keep] {
        if total <= cap {
            break;
        }
        entries[index].info.dropped = true;
        entries[index].info.auto = true;
        total = total.saturating_sub(entries[index].info.tokens);
    }
}

/// Trim once more against the total budget, alternating sectors. This only
/// matters when the fixed sectors (system/instructions/workspace) are large.
fn drop_oldest(entries: &mut [Entry], sector: Sector) -> bool {
    let candidates: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry.trimmable
                && entry.info.sector == sector
                && !entry.summary
                && !entry.info.pinned
                && !entry.info.dropped
        })
        .map(|(index, _)| index)
        .collect();
    if candidates.len() <= KEEP_NEWEST {
        return false;
    }
    let index = candidates[0];
    entries[index].info.dropped = true;
    entries[index].info.auto = true;
    true
}

fn trim(entries: &mut [Entry], budget: usize) -> usize {
    trim_sector(
        entries,
        Sector::Tools,
        (budget as f64 * Sector::Tools.budget_percent()) as usize,
    );
    trim_sector(
        entries,
        Sector::History,
        (budget as f64 * Sector::History.budget_percent()) as usize,
    );

    let mut total: usize = entries
        .iter()
        .filter(|entry| !entry.info.dropped && !entry.info.compacted)
        .map(|entry| entry.info.tokens)
        .sum();
    let mut guard = 0;
    while total > budget && guard < entries.len() * 2 {
        guard += 1;
        let dropped = if drop_oldest(entries, Sector::Tools) {
            true
        } else {
            drop_oldest(entries, Sector::History)
        };
        if !dropped {
            break;
        }
        total = entries
            .iter()
            .filter(|entry| !entry.info.dropped && !entry.info.compacted)
            .map(|entry| entry.info.tokens)
            .sum();
    }

    entries.iter().filter(|entry| entry.info.auto).count()
}

fn summarize(entries: &[Entry], budget: usize, trimmed: usize) -> ContextLedgerResult {
    let mut sectors = Vec::new();
    for sector in Sector::ALL {
        let selected: Vec<&Entry> = entries
            .iter()
            .filter(|entry| entry.info.sector == sector)
            .collect();
        let active = selected
            .iter()
            .filter(|entry| !entry.info.dropped && !entry.info.compacted);
        sectors.push(ContextSectorInfo {
            sector,
            tokens: active.map(|entry| entry.info.tokens).sum(),
            budget_tokens: (budget as f64 * sector.budget_percent()) as usize,
            entries: selected.len(),
            pinned: selected.iter().filter(|entry| entry.info.pinned).count(),
            dropped: selected.iter().filter(|entry| entry.info.dropped).count(),
        });
    }
    let total_tokens = sectors.iter().map(|sector| sector.tokens).sum();
    ContextLedgerResult {
        entries: entries.iter().map(|entry| entry.info.clone()).collect(),
        sectors,
        total_tokens,
        budget_tokens: budget,
        trimmed,
    }
}

async fn load_instructions(config: &Config, cwd: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(dir) = eix_core::config::default_config_path().parent() {
        let path = dir.join("AGENTS.md");
        if let Ok(text) = tokio::fs::read_to_string(&path).await {
            if !text.trim().is_empty() {
                out.push(("global AGENTS.md".to_string(), cap_instruction(text)));
            }
        }
    }
    for name in &config.context.instructions_files {
        let path = cwd.join(name);
        if let Ok(text) = tokio::fs::read_to_string(&path).await {
            if !text.trim().is_empty() {
                out.push((name.clone(), cap_instruction(text)));
            }
        }
    }
    out
}

fn cap_instruction(text: String) -> String {
    if text.chars().count() <= MAX_INSTRUCTION_CHARS {
        return text;
    }
    let mut out: String = text.chars().take(MAX_INSTRUCTION_CHARS).collect();
    out.push_str("\n[truncated by eix: instruction file too large]");
    out
}

async fn workspace_summary(cwd: &Path) -> Option<String> {
    let mut read = tokio::fs::read_dir(cwd).await.ok()?;
    let mut names = Vec::new();
    while let Ok(Some(entry)) = read.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let is_dir = entry
            .file_type()
            .await
            .map(|kind| kind.is_dir())
            .unwrap_or(false);
        names.push(if is_dir { format!("{name}/") } else { name });
        if names.len() >= 80 {
            break;
        }
    }
    names.sort();

    let mut out = String::new();
    out.push_str(&format!("workspace: {}\n", cwd.display()));
    out.push_str("top-level: ");
    out.push_str(&names.join("  "));
    out.push('\n');

    if cwd.join(".git").exists() {
        // Run both queries together; `git status` can be slow on big repos.
        let (branch, status) = tokio::join!(
            git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]),
            git(cwd, &["status", "--porcelain"]),
        );
        if let Some(branch) = branch {
            out.push_str(&format!("git branch: {}\n", branch.trim()));
        }
        if let Some(status) = status {
            let lines: Vec<&str> = status.lines().take(20).collect();
            if lines.is_empty() {
                out.push_str("git status: clean\n");
            } else {
                out.push_str("git status:\n");
                out.push_str(&lines.join("\n"));
                if status.lines().count() > lines.len() {
                    out.push_str("\n…");
                }
                out.push('\n');
            }
        }
    }
    Some(out)
}

async fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = timeout(
        Duration::from_secs(2),
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Rough token estimate. ASCII averages ~4 characters per token; CJK and other
/// wide scripts are close to one token per character. Deliberately cheap: the
/// ledger only needs to be directionally right.
pub fn estimate_tokens(text: &str) -> usize {
    let mut wide = 0usize;
    let mut narrow = 0usize;
    for ch in text.chars() {
        if is_wide(ch) {
            wide += 1;
        } else {
            narrow += 1;
        }
    }
    wide + narrow.div_ceil(4)
}

fn is_wide(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD)
}

fn snippet(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn decode<T: DeserializeOwned>(event: &Event) -> Option<T> {
    serde_json::from_value(event.payload.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eix_core::{
        payload, AssistantMessagePayload, FunctionCall, Role, ToolCall, ToolResultPayload,
    };
    use serde_json::json;

    fn event(seq: u64, kind: EventKind, payload: serde_json::Value) -> Event {
        Event::new("s", seq, kind, payload)
    }

    fn user(seq: u64, text: &str) -> Event {
        event(
            seq,
            EventKind::UserMessage,
            payload(&UserMessagePayload {
                text: text.into(),
                images: Vec::new(),
            }),
        )
    }

    fn assistant(seq: u64, text: &str) -> Event {
        event(
            seq,
            EventKind::AssistantMessage,
            payload(&AssistantMessagePayload {
                text: text.into(),
                model: "m".into(),
                reasoning: None,
                cancelled: false,
                usage: None,
                tool_calls: Vec::new(),
            }),
        )
    }

    #[test]
    fn estimate_handles_ascii_and_cjk() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("你好"), 2);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn entries_follow_the_event_log_and_map_to_messages() {
        let events = vec![user(1, "hello"), assistant(2, "hi there")];
        let ledger = Ledger::assemble(
            "system prompt".into(),
            vec![("AGENTS.md".into(), "rules".into())],
            None,
            &events,
            10_000,
        );
        let info = ledger.info();
        let ids: Vec<&str> = info.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["system", "instructions:AGENTS.md", "e1", "e2"]);
        assert_eq!(ledger.messages().len(), 4);
        assert!(info.total_tokens > 0);
        assert_eq!(
            info.sectors
                .iter()
                .find(|s| s.sector == Sector::History)
                .unwrap()
                .entries,
            2
        );
    }

    #[test]
    fn drop_and_pin_events_change_the_request() {
        let mut events = vec![user(1, "one"), assistant(2, "two"), user(3, "three")];
        events.push(event(
            4,
            EventKind::ContextDropped,
            payload(&eix_core::ContextDropPayload {
                entry: "e2".into(),
                dropped: true,
            }),
        ));
        events.push(event(
            5,
            EventKind::ContextPinned,
            payload(&ContextPinPayload {
                entry: "e1".into(),
                pinned: true,
            }),
        ));
        let ledger = Ledger::assemble("sys".into(), vec![], None, &events, 10_000);
        assert_eq!(ledger.messages().len(), 3);
        let entries = ledger.info().entries;
        assert!(entries.iter().find(|e| e.id == "e2").unwrap().dropped);
        assert!(entries.iter().find(|e| e.id == "e1").unwrap().pinned);
    }

    #[test]
    fn compaction_replaces_a_range_with_one_summary() {
        let events = vec![
            user(1, "one"),
            assistant(2, "two"),
            user(3, "three"),
            event(
                4,
                EventKind::ContextCompacted,
                payload(&ContextCompactedPayload {
                    from_seq: 1,
                    to_seq: 2,
                    messages: 2,
                    tokens: 10,
                    summary: "they greeted each other".into(),
                }),
            ),
        ];
        let ledger = Ledger::assemble("sys".into(), vec![], None, &events, 10_000);
        let messages = ledger.messages();
        // system prompt + compaction summary + the surviving user message
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].role, eix_core::Role::System);
        assert!(messages[1].content.contains("they greeted each other"));
        assert_eq!(messages[2].content, "three");

        let entries = ledger.info().entries;
        assert!(entries.iter().find(|e| e.id == "e1").unwrap().compacted);
        assert!(entries.iter().any(|e| e.id == "c1-2"));
        assert!(ledger.overlaps_compaction(2, 3));
    }

    #[test]
    fn tools_are_trimmed_oldest_first_but_newest_survive() {
        let mut events = Vec::new();
        let mut seq = 1;
        for index in 0..12 {
            events.push(user(seq, "run it"));
            seq += 1;
            events.push(event(
                seq,
                EventKind::AssistantMessage,
                payload(&AssistantMessagePayload {
                    text: String::new(),
                    model: "m".into(),
                    reasoning: None,
                    cancelled: false,
                    usage: None,
                    tool_calls: vec![eix_core::ToolCall {
                        id: format!("call{index}"),
                        kind: "function".into(),
                        function: eix_core::FunctionCall {
                            name: "run_command".into(),
                            arguments: "{}".into(),
                        },
                    }],
                }),
            ));
            seq += 1;
            events.push(event(
                seq,
                EventKind::ToolResult,
                payload(&ToolResultPayload {
                    call_id: format!("call{index}"),
                    tool: "run_command".into(),
                    ok: true,
                    denied: false,
                    output: "x".repeat(4_000),
                    duration_ms: None,
                }),
            ));
            seq += 1;
        }
        let ledger = Ledger::assemble("sys".into(), vec![], None, &events, 2_000);
        let entries = ledger.info().entries;
        let tools: Vec<&ContextEntryInfo> = entries
            .iter()
            .filter(|entry| entry.sector == Sector::Tools)
            .collect();
        let dropped = tools.iter().filter(|entry| entry.dropped).count();
        assert!(dropped > 0, "expected tool entries to be trimmed");
        assert!(dropped < tools.len(), "newest tools must survive");
        assert!(ledger.info().trimmed > 0);
    }

    #[test]
    fn compaction_ranges_with_no_messages_are_detectable() {
        let events = vec![user(1, "hi")];
        let ledger = Ledger::assemble("sys".into(), vec![], None, &events, 10_000);
        assert_eq!(ledger.range_stats(1, 1).0, 1);
        assert_eq!(ledger.range_stats(9, 12).0, 0);
    }

    #[test]
    fn payloads_deserialize_leniently() {
        let drop: ContextDropPayload = serde_json::from_value(json!({"entry": "e1"})).unwrap();
        assert!(drop.dropped);
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read_file".into(),
                arguments: "{}".into(),
            },
        }
    }

    fn tool_step(seq: u64, text: &str, calls: Vec<ToolCall>) -> Event {
        event(
            seq,
            EventKind::AssistantMessage,
            payload(&AssistantMessagePayload {
                text: text.into(),
                model: "m".into(),
                reasoning: None,
                cancelled: false,
                usage: None,
                tool_calls: calls,
            }),
        )
    }

    fn tool_result(seq: u64, call_id: &str, output: &str) -> Event {
        event(
            seq,
            EventKind::ToolResult,
            payload(&ToolResultPayload {
                call_id: call_id.into(),
                tool: "read_file".into(),
                ok: true,
                denied: false,
                output: output.into(),
                duration_ms: None,
            }),
        )
    }

    #[test]
    fn tool_step_is_one_entry_with_calls_and_results_together() {
        let events = vec![
            user(1, "read it"),
            tool_step(2, "", vec![call("call_1")]),
            tool_result(3, "call_1", "fn main() {}"),
        ];
        let ledger = Ledger::assemble("sys".into(), vec![], None, &events, 10_000);
        let messages = ledger.messages();
        // system prompt + user + one assistant step + its tool result
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].content, "read it");
        assert_eq!(messages[2].role, Role::Assistant);
        assert_eq!(
            messages[2].tool_calls.as_ref().unwrap()[0].function.name,
            "read_file"
        );
        assert_eq!(messages[3].role, Role::Tool);
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(messages[3].content, "fn main() {}");

        let entries = ledger.info().entries;
        let step = entries.iter().find(|entry| entry.id == "e2").unwrap();
        assert_eq!(step.sector, Sector::Tools);
        assert_eq!(step.kind, "tool_step");
        // The result is folded into the step, not a second row.
        assert!(!entries.iter().any(|entry| entry.id == "e3"));
    }

    #[test]
    fn dangling_tool_calls_never_emit_an_invalid_request() {
        // One of two calls produced output: keep only the completed call.
        let events = vec![
            user(1, "do it"),
            tool_step(2, "working", vec![call("a"), call("b")]),
            tool_result(3, "a", "done"),
        ];
        let messages = Ledger::assemble("sys".into(), vec![], None, &events, 10_000).messages();
        let assistant = messages
            .iter()
            .find(|message| message.role == Role::Assistant)
            .unwrap();
        let calls = assistant.tool_calls.as_ref().expect("tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "a");
        assert!(messages
            .iter()
            .all(|message| message.tool_call_id.as_deref() != Some("b")));

        // No results and no text: drop the step instead of sending a bare
        // `tool_calls` block.
        let events = vec![
            user(1, "do it"),
            tool_step(2, "", vec![call("a")]),
            user(3, "next"),
        ];
        let messages = Ledger::assemble("sys".into(), vec![], None, &events, 10_000).messages();
        // system prompt + the two user messages; the empty step is gone.
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].content, "next");
        assert!(messages.iter().all(|message| message.tool_calls.is_none()));
    }

    #[test]
    fn auto_compact_keeps_the_newest_user_turns() {
        let events = vec![
            user(1, "one"),
            assistant(2, "a1"),
            user(3, "two"),
            assistant(4, "a2"),
            user(5, "three"),
            assistant(6, "a3"),
            user(7, "four"),
            assistant(8, "a4"),
        ];
        let ledger = Ledger::assemble("sys".into(), vec![], None, &events, 100_000);
        // Keep the two newest user turns (seq 5 and 7).
        assert_eq!(ledger.auto_compact_range(2), Some((1, 4)));
        assert_eq!(ledger.auto_compact_range(3), Some((1, 2)));
        // Nothing to compact when every turn must be kept.
        assert_eq!(ledger.auto_compact_range(4), None);

        let range = ledger.messages_in_range(1, 4);
        assert_eq!(range.len(), 4, "two user/assistant pairs");

        // After a compaction, the next range starts after the summary.
        let mut events = events;
        events.push(event(
            9,
            EventKind::ContextCompacted,
            payload(&ContextCompactedPayload {
                from_seq: 1,
                to_seq: 4,
                messages: 4,
                tokens: 10,
                summary: "earlier talk".into(),
            }),
        ));
        events.push(user(10, "five"));
        events.push(assistant(11, "a5"));
        let ledger = Ledger::assemble("sys".into(), vec![], None, &events, 100_000);
        assert_eq!(ledger.auto_compact_range(2), Some((5, 6)));
    }
}
