//! Builds the provider context from the event log and reports it as a ledger.
//!
//! The ledger is the honest answer to "what is actually in the prompt": every
//! sector, entry and token is derived from the same events the request is built
//! from. Policies (drop / pin / compact) are ordinary events; replaying them is
//! deterministic, so a restart never changes what a session sends.
//!
//! Budgets are enforced in two stages. `tools` and `history` are trimmed
//! oldest-first to their share of `context.budget_tokens`, but the newest few
//! entries (and anything pinned) always survive. If those are not enough,
//! instructions, workspace and memory are dropped oldest-first and marked
//! `auto` in the ledger; this is visible rather than silently hiding an
//! `AGENTS.md`. The system prompt is never dropped, but it is capped at 25% of
//! the budget with an explicit truncation marker.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use eix_core::{
    AssistantMessagePayload, ChatMessage, Config, ContextCompactedPayload, ContextDropPayload,
    ContextEntryInfo, ContextLedgerResult, ContextPinPayload, ContextSectorInfo, Event, EventKind,
    ImagePart, Sector, ToolDefinition, ToolResultPayload, UserMessagePayload,
};
use serde::de::DeserializeOwned;
use tokio::time::{timeout, Duration};

use crate::blob::BlobStore;

/// Never auto-drop the newest few entries in a sector: the model still needs
/// the tool output it just received.
const KEEP_NEWEST: usize = 4;
/// Default per-result context cap for callers that do not pass a config.
const DEFAULT_TOOL_OUTPUT_TOKENS: usize = 4_000;
/// Conservative per-image estimate. The exact vision token count depends on
/// the model, image dimensions and detail level; counting zero was much worse.
const IMAGE_TOKEN_ESTIMATE: usize = 1_000;
const MAX_DETAIL_CHARS: usize = 2_000;
const MAX_PREVIEW_CHARS: usize = 160;
const MAX_INSTRUCTION_CHARS: usize = 64_000;
const MAX_SYSTEM_CHARS: usize = 32_000;
const MAX_SUMMARY_CHARS: usize = 12_000;

/// The parts of the ledger that do not change between tool rounds in one turn.
/// Loading instructions walks the repository and `workspace_summary` runs git;
/// caching both avoids paying that cost for every tool batch.
#[derive(Debug, Clone, Default)]
pub struct StaticContext {
    instructions: Vec<(String, String)>,
    workspace: Option<String>,
}

impl StaticContext {
    pub async fn load(config: &Config, cwd: &Path) -> Self {
        let instructions = load_instructions(config, cwd).await;
        let workspace = if config.context.workspace_summary {
            workspace_summary(cwd).await
        } else {
            None
        };
        Self {
            instructions,
            workspace,
        }
    }
}

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
    /// Tokens before the per-model calibration scale. Kept so an incremental
    /// refresh can re-scale without rebuilding the entry.
    base_tokens: usize,
}

#[derive(Debug, Clone)]
struct Compaction {
    from_seq: u64,
    to_seq: u64,
    messages: usize,
    tokens: usize,
    summary: String,
    rolling: bool,
}

#[derive(Default)]
struct Policy {
    pinned: HashSet<String>,
    dropped: HashSet<String>,
    /// Same entries as `dropped`, but keyed by event sequence so the hot
    /// `hides_seq` check does not allocate a `format!("e{seq}")` per event.
    dropped_seqs: HashSet<u64>,
    compactions: Vec<Compaction>,
    /// Messages at or before this sequence are excluded from the provider
    /// context (a `/clear` boundary).
    cleared_seq: Option<u64>,
}

/// Parse a history entry id (`e12`) back into its event sequence.
fn entry_seq(id: &str) -> Option<u64> {
    id.strip_prefix('e')?.parse().ok()
}

impl Policy {
    /// True when this history entry can never reach the provider. Used to skip
    /// decoding and base64-encoding image blobs for dropped/compacted messages.
    fn hides_seq(&self, seq: u64) -> bool {
        if self.cleared_seq.is_some_and(|cleared| seq <= cleared) {
            return true;
        }
        if self.dropped_seqs.contains(&seq) {
            return true;
        }
        self.compactions
            .iter()
            .any(|compaction| seq >= compaction.from_seq && seq <= compaction.to_seq)
    }

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
                        let seq = entry_seq(&value.entry);
                        if value.dropped {
                            policy.dropped.insert(value.entry);
                            if let Some(seq) = seq {
                                policy.dropped_seqs.insert(seq);
                            }
                        } else {
                            policy.dropped.remove(&value.entry);
                            if let Some(seq) = seq {
                                policy.dropped_seqs.remove(&seq);
                            }
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
                            rolling: value.rolling,
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
        Self::build_with_state(config, system, cwd, events, None, None, None, &[], 1.0).await
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
        Self::build_with_state(config, system, cwd, events, blobs, None, None, &[], 1.0).await
    }

    /// Full assembly used by the daemon: instructions, workspace summary,
    /// cross-session memory, the current task list, image blobs and the tool
    /// schemas sent with the request. `token_scale` calibrates the cheap local
    /// estimate against provider-reported prompt usage.
    #[allow(clippy::too_many_arguments)]
    pub async fn build_with_state(
        config: &Config,
        system: String,
        cwd: &Path,
        events: &[Event],
        blobs: Option<&BlobStore>,
        memory: Option<String>,
        tasks: Option<String>,
        tools: &[ToolDefinition],
        token_scale: f64,
    ) -> Self {
        let static_context = StaticContext::load(config, cwd).await;
        Self::build_with_state_and_static(
            config,
            system,
            &static_context,
            events,
            blobs,
            memory,
            tasks,
            tools,
            token_scale,
            config.context.budget_tokens,
        )
        .await
    }

    /// Full assembly with preloaded instructions/workspace and an explicit
    /// budget. The daemon uses this to reuse static context across tool rounds
    /// and to clamp the budget to the selected model's real context window.
    #[allow(clippy::too_many_arguments)]
    pub async fn build_with_state_and_static(
        config: &Config,
        system: String,
        static_context: &StaticContext,
        events: &[Event],
        blobs: Option<&BlobStore>,
        memory: Option<String>,
        tasks: Option<String>,
        tools: &[ToolDefinition],
        token_scale: f64,
        budget_tokens: usize,
    ) -> Self {
        let images = load_images(events, blobs).await;
        let budget_tokens = budget_tokens.max(1);
        let system = cap_system(system, budget_tokens);
        Self::assemble_full(
            system,
            static_context.instructions.clone(),
            static_context.workspace.clone(),
            memory,
            tasks,
            events,
            budget_tokens,
            config.context.tool_output_tokens,
            &images,
            tools,
            token_scale,
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
        Self::assemble_full(
            system,
            instructions,
            workspace,
            None,
            None,
            events,
            budget_tokens,
            DEFAULT_TOOL_OUTPUT_TOKENS,
            &HashMap::new(),
            &[],
            1.0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble_full(
        system: String,
        instructions: Vec<(String, String)>,
        workspace: Option<String>,
        memory: Option<String>,
        tasks: Option<String>,
        events: &[Event],
        budget_tokens: usize,
        tool_output_tokens: usize,
        images: &HashMap<String, ImagePart>,
        tools: &[ToolDefinition],
        token_scale: f64,
    ) -> Self {
        let policy = Policy::from_events(events);
        let mut entries = fixed_entries(
            system,
            &instructions,
            workspace.as_deref(),
            memory,
            tasks,
            tools,
            &policy,
        );
        let (event_entries, _) = event_entries_with_claimed(events, images, tool_output_tokens);
        entries.extend(event_entries);

        let mut entries = apply_compactions(entries, &policy.compactions);
        prepare_entries(&mut entries, &policy, token_scale);
        let trimmed = trim(&mut entries, budget_tokens);
        let result = summarize(&entries, budget_tokens, trimmed);

        Self {
            entries,
            compactions: policy.compactions,
            result,
        }
    }

    /// Append history entries for events that arrived after the last build and
    /// refresh the fixed sectors, calibration and trim in place. Returns
    /// `false` when the batch needs a full rebuild; the caller must then throw
    /// this ledger away and build a new one.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_extend(
        &mut self,
        new_events: &[Event],
        all_events: &[Event],
        images: &HashMap<String, ImagePart>,
        tool_output_tokens: usize,
        system: String,
        static_context: &StaticContext,
        memory: Option<String>,
        tasks: Option<String>,
        tools: &[ToolDefinition],
        token_scale: f64,
        budget_tokens: usize,
    ) -> bool {
        // A compaction rewrites earlier entries; replaying it is the only way
        // to keep the ledger identical to a fresh build.
        if new_events
            .iter()
            .any(|event| event.kind == EventKind::ContextCompacted)
        {
            return false;
        }
        let policy = Policy::from_events(all_events);
        let appended = if new_events.is_empty() {
            None
        } else {
            let (entries, claimed) =
                event_entries_with_claimed(new_events, images, tool_output_tokens);
            // An unclaimed result belongs to a tool call from an earlier
            // round. Only a full rebuild can attach it to that assistant
            // message; appending it alone would leave a bare tool message.
            if new_events
                .iter()
                .any(|event| event.kind == EventKind::ToolResult && !claimed.contains(&event.seq))
            {
                return false;
            }
            Some(entries)
        };

        let budget_tokens = budget_tokens.max(1);
        let system = cap_system(system, budget_tokens);
        // Move the event-derived entries out so they can be recombined with a
        // freshly built fixed sector without cloning message contents. Old
        // summaries are dropped and regenerated by `apply_compactions` so the
        // result matches a full build, including entry order.
        let previous = std::mem::take(&mut self.entries);
        let mut history: Vec<Entry> = previous
            .into_iter()
            .filter(|entry| entry.seq.is_some() && !entry.summary)
            .collect();
        if let Some(mut appended) = appended {
            history.append(&mut appended);
        }

        let mut entries = fixed_entries(
            system,
            &static_context.instructions,
            static_context.workspace.as_deref(),
            memory,
            tasks,
            tools,
            &policy,
        );
        entries.extend(history);
        let mut entries = apply_compactions(entries, &policy.compactions);
        prepare_entries(&mut entries, &policy, token_scale);
        let trimmed = trim(&mut entries, budget_tokens);
        self.result = summarize(&entries, budget_tokens, trimmed);
        self.entries = entries;
        self.compactions = policy.compactions;
        true
    }

    /// Active compaction summaries, used as the starting point for the next
    /// rolling summary. Older rolling summaries are already suppressed, so
    /// this is normally one summary; a later manual compaction can add another.
    pub fn latest_summary(&self) -> Option<String> {
        let text = self
            .entries
            .iter()
            .filter(|entry| entry.summary && !entry.info.dropped && !entry.info.compacted)
            .flat_map(|entry| entry.messages.iter())
            .map(|message| message.content.trim())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        (!text.is_empty()).then_some(text)
    }

    /// Provider messages for everything still in the context.
    pub fn messages(&self) -> Vec<ChatMessage> {
        // Reserve up front: the turn loop rebuilds this vector after every
        // tool round, and repeated growth copied the whole history each time.
        let capacity = self
            .entries
            .iter()
            .filter(|entry| !entry.info.dropped && !entry.info.compacted)
            .map(|entry| entry.messages.len())
            .sum();
        let mut messages = Vec::with_capacity(capacity);
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
        let capacity = self
            .entries
            .iter()
            .filter(|entry| {
                !entry.info.dropped
                    && !entry.info.compacted
                    && entry
                        .seq
                        .is_some_and(|seq| from_seq <= seq && seq <= to_seq)
            })
            .map(|entry| entry.messages.len())
            .sum();
        let mut messages = Vec::with_capacity(capacity);
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

    /// Range an auto-compaction should summarize, keeping roughly
    /// `keep_tokens` of the newest conversation and at least `keep_turns` user
    /// turns. Cutting happens on a user-turn boundary so a tool call is never
    /// separated from its result.
    pub fn auto_compact_range_tokens(
        &self,
        keep_tokens: usize,
        keep_turns: usize,
    ) -> Option<(u64, u64)> {
        let keep_turns = keep_turns.max(1);
        let users: Vec<u64> = self
            .entries
            .iter()
            .filter(|entry| !entry.info.dropped && !entry.info.compacted)
            .filter(|entry| entry.info.kind == "user")
            .filter_map(|entry| entry.seq)
            .collect();
        if users.len() <= keep_turns {
            return None;
        }
        let turn_cutoff = users[users.len() - keep_turns].saturating_sub(1);

        let mut kept_tokens = 0usize;
        let mut token_cutoff = None;
        for entry in self.entries.iter().rev() {
            if entry.info.dropped || entry.info.compacted || entry.seq.is_none() {
                continue;
            }
            if entry.info.kind == "user" && kept_tokens >= keep_tokens {
                token_cutoff = entry.seq;
                break;
            }
            kept_tokens = kept_tokens.saturating_add(entry.info.tokens);
        }
        // The whole live conversation fits in the retention window.
        if token_cutoff.is_none() && kept_tokens < keep_tokens {
            return None;
        }
        let to = token_cutoff
            .map(|seq| turn_cutoff.min(seq.saturating_sub(1)))
            .unwrap_or(turn_cutoff);
        let (from, _) = self.message_range()?;
        (to >= from).then_some((from, to))
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

/// Build the fixed sectors (system, instructions, workspace, tools, memory and
/// tasks). These do not depend on the event log, so an incremental refresh can
/// rebuild them without touching history entries.
#[allow(clippy::too_many_arguments)]
fn fixed_entries(
    system: String,
    instructions: &[(String, String)],
    workspace: Option<&str>,
    memory: Option<String>,
    tasks: Option<String>,
    tools: &[ToolDefinition],
    policy: &Policy,
) -> Vec<Entry> {
    let mut entries = Vec::new();
    entries.push(synthesized(
        "system",
        Sector::System,
        "system",
        system,
        false,
        policy,
    ));
    for (label, text) in instructions {
        entries.push(synthesized(
            &format!("instructions:{label}"),
            Sector::Instructions,
            "instructions",
            text.clone(),
            true,
            policy,
        ));
    }
    if let Some(text) = workspace {
        entries.push(synthesized(
            "workspace",
            Sector::Workspace,
            "workspace",
            text.to_string(),
            true,
            policy,
        ));
    }
    if !tools.is_empty() {
        entries.push(tool_schema_entry(tools));
    }
    if let Some(text) = memory {
        entries.push(synthesized(
            "memory",
            Sector::Memory,
            "memory",
            format!(
                "Project memory (persisted across sessions; do not treat as user input):\n{text}"
            ),
            true,
            policy,
        ));
    }
    if let Some(text) = tasks {
        entries.push(synthesized(
            "tasks",
            Sector::Memory,
            "tasks",
            text,
            true,
            policy,
        ));
    }
    entries
}

fn tool_schema_entry(tools: &[ToolDefinition]) -> Entry {
    let tokens = estimate_tool_schema_tokens(tools);
    let mut entry = make_entry(
        "tools:schema".to_string(),
        Sector::Tools,
        "tool_schema".to_string(),
        Vec::new(),
        None,
        false,
    );
    entry.info.tokens = tokens;
    entry.base_tokens = tokens;
    entry.info.preview = format!("{} tool definitions", tools.len());
    entry.info.detail = format!(
        "Tool JSON schemas are sent with every request ({} tools).",
        tools.len()
    );
    entry
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

pub(crate) async fn load_images(
    events: &[Event],
    blobs: Option<&BlobStore>,
) -> HashMap<String, ImagePart> {
    let Some(blobs) = blobs else {
        return HashMap::new();
    };
    // Compaction and `/clear` are common in long sessions. Loading image bytes
    // for messages that are no longer in the request wastes time and memory,
    // and each blob would be base64-encoded again on every tool round.
    let policy = Policy::from_events(events);
    let mut mimes: HashMap<String, String> = HashMap::new();
    for event in events {
        if event.kind != EventKind::UserMessage || policy.hides_seq(event.seq) {
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
    let mut loaded = HashMap::with_capacity(mimes.len());
    for (id, mime) in mimes {
        // `image_part` caches the base64 payload; a miss reads and encodes once.
        if let Ok(part) = blobs.image_part(&id, &mime).await {
            loaded.insert(id, part);
        }
    }
    loaded
}

/// Decode history entries from `events`, also returning the sequences of tool
/// results that were attached to an assistant message in the same batch. An
/// incremental refresh uses the set to detect results whose call lives in an
/// earlier batch and therefore needs a full rebuild.
fn event_entries_with_claimed(
    events: &[Event],
    blob_images: &HashMap<String, ImagePart>,
    tool_output_tokens: usize,
) -> (Vec<Entry>, HashSet<u64>) {
    // Tool results are matched to the assistant message that requested them so
    // a dropped or compacted step never produces a `tool` message without its
    // preceding `tool_calls`. Keep every result for a call id: some providers
    // reuse ids like `call_1` across turns, so matching by id alone would pair
    // a new result with an old assistant message.
    let mut results: HashMap<String, Vec<(u64, String, String)>> = HashMap::new();
    for event in events {
        if event.kind == EventKind::ToolResult {
            if let Some(payload) = decode::<ToolResultPayload>(event) {
                results.entry(payload.call_id.clone()).or_default().push((
                    event.seq,
                    payload.tool.clone(),
                    payload.output.clone(),
                ));
            }
        }
    }
    for matches in results.values_mut() {
        matches.sort_by_key(|(seq, _, _)| *seq);
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
                entries.push(entry);
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
                    let matched = results.get_mut(&call.id).and_then(|matches| {
                        let index = matches.iter().position(|(seq, _, _)| *seq > event.seq)?;
                        Some(matches.remove(index))
                    });
                    if let Some((seq, tool, output)) = matched {
                        claimed.insert(seq);
                        tool_previews.push(format!("{tool}: {}", first_line(&output)));
                        completed.push(call.clone());
                        tool_messages.push(ChatMessage::tool(
                            call.id.clone(),
                            truncate_tool_output(&output, tool_output_tokens),
                        ));
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
                    entries.push(entry);
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
                    entries.push(entry);
                }
            }
            EventKind::ToolResult => {
                if claimed.contains(&event.seq) {
                    continue;
                }
                let Some(payload) = decode::<ToolResultPayload>(event) else {
                    continue;
                };
                let message = ChatMessage::tool(
                    payload.call_id.clone(),
                    truncate_tool_output(&payload.output, tool_output_tokens),
                );
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
                entries.push(entry);
            }
            _ => {}
        }
    }
    (entries, claimed)
}

/// Estimate one provider message, including tool-call arguments and images.
/// The old implementation only counted `content`, which made a tool-heavy
/// request look much smaller than the JSON the provider actually receives.
pub fn estimate_message_tokens(message: &ChatMessage) -> usize {
    let mut tokens = estimate_tokens(&message.content) + 4;
    if let Some(name) = &message.name {
        tokens += estimate_tokens(name) + 2;
    }
    if let Some(call_id) = &message.tool_call_id {
        tokens += estimate_tokens(call_id) + 4;
    }
    if let Some(calls) = &message.tool_calls {
        for call in calls {
            tokens += estimate_tokens(&call.id)
                + estimate_tokens(&call.function.name)
                + estimate_tokens(&call.function.arguments)
                + 8;
        }
    }
    tokens += message.images.len() * IMAGE_TOKEN_ESTIMATE;
    tokens
}

pub fn estimate_messages_tokens(messages: &[ChatMessage]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

/// Tool definitions are part of every request even though they are not chat
/// messages. Serialize them so names, descriptions and parameter schemas are
/// all accounted for.
pub fn estimate_tool_schema_tokens(tools: &[ToolDefinition]) -> usize {
    tools
        .iter()
        .map(|tool| {
            let text = serde_json::to_string(tool).unwrap_or_else(|_| {
                format!(
                    "{} {} {}",
                    tool.function.name, tool.function.description, tool.function.parameters
                )
            });
            estimate_tokens(&text) + 4
        })
        .sum()
}

/// Total prompt estimate used for calibration: messages plus tool schemas.
pub fn estimate_request_tokens(messages: &[ChatMessage], tools: &[ToolDefinition]) -> usize {
    estimate_messages_tokens(messages) + estimate_tool_schema_tokens(tools)
}

fn scale_tokens(tokens: usize, scale: f64) -> usize {
    if tokens == 0 || !scale.is_finite() || (scale - 1.0).abs() <= f64::EPSILON {
        return tokens;
    }
    let scale = scale.clamp(0.25, 4.0);
    (tokens as f64 * scale).ceil() as usize
}

/// Re-apply model calibration, pin/drop policy and the `/clear` boundary to a
/// set of entries. Automatic drops from a previous round are cleared so the
/// trimmers can make a fresh decision against the current budget; compaction
/// flags are left alone because only a full rebuild replays those events.
fn prepare_entries(entries: &mut [Entry], policy: &Policy, scale: f64) {
    let cleared = policy.cleared_seq;
    for entry in entries {
        entry.info.tokens = scale_tokens(entry.base_tokens, scale);
        entry.info.auto = false;
        entry.info.pinned = policy.pinned.contains(&entry.info.id);
        entry.info.dropped = policy.dropped.contains(&entry.info.id)
            || entry
                .seq
                .is_some_and(|seq| cleared.is_some_and(|cleared| seq <= cleared));
    }
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
    let mut detail = String::new();
    let mut detail_chars = 0usize;
    let mut preview = String::new();
    for message in &messages {
        tokens += estimate_message_tokens(message);
        chars += message.content.chars().count();
        if message.content.is_empty() {
            continue;
        }
        if preview.is_empty() {
            preview = first_line(&message.content);
        }
        // Build the detail inline instead of cloning every message into a
        // `parts` vector and joining it twice. Tool outputs are the bulk of a
        // long history, so the extra copies were paid on every rebuild.
        if detail_chars >= MAX_DETAIL_CHARS {
            continue;
        }
        if !detail.is_empty() {
            detail.push_str("\n\n");
            detail_chars += 2;
        }
        let room = MAX_DETAIL_CHARS.saturating_sub(detail_chars);
        let text = &message.content;
        let end = text
            .char_indices()
            .nth(room)
            .map(|(index, _)| index)
            .unwrap_or(text.len());
        detail.push_str(&text[..end]);
        detail_chars += text[..end].chars().count();
    }
    let detail = snippet(&detail, MAX_DETAIL_CHARS);
    let preview = snippet(&preview, MAX_PREVIEW_CHARS);
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
        base_tokens: tokens,
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
    // A rolling auto-compaction includes the previous summary, so older
    // summaries would be duplicate context. Keep only the newest rolling one.
    let latest_rolling = compactions
        .iter()
        .rposition(|compaction| compaction.rolling);
    let superseded: HashSet<(u64, u64)> = latest_rolling
        .map(|index| {
            compactions[..index]
                .iter()
                .map(|compaction| (compaction.from_seq, compaction.to_seq))
                .collect()
        })
        .unwrap_or_default();
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
            let key = (compaction.from_seq, compaction.to_seq);
            if !superseded.contains(&key) && emitted.insert(key) {
                out.push(summary_entry(&compaction));
            }
            // Keep the original for the expand view; `messages()` skips it.
            out.push(entry);
            continue;
        }
        out.push(entry);
    }
    for compaction in compactions {
        let key = (compaction.from_seq, compaction.to_seq);
        if !superseded.contains(&key) && emitted.insert(key) {
            out.push(summary_entry(compaction));
        }
    }
    out
}

fn summary_entry(compaction: &Compaction) -> Entry {
    let summary = cap_text(&compaction.summary, MAX_SUMMARY_CHARS);
    let text = format!(
        "[compacted {} messages (~{} tokens)]\n{summary}",
        compaction.messages, compaction.tokens
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
    // Tool schemas are fixed request overhead: the total budget counts them,
    // but they must not consume the trimmable tool-output share.
    let mut total: usize = entries
        .iter()
        .filter(|entry| {
            entry.info.sector == sector
                && entry.info.kind != "tool_schema"
                && !entry.info.dropped
                && !entry.info.compacted
        })
        .map(|entry| entry.info.tokens)
        .sum();
    if total <= cap {
        return;
    }
    let latest_user = latest_user_seq(entries);
    let candidates: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry.trimmable
                && entry.info.sector == sector
                && !entry.summary
                && !entry.info.pinned
                && !entry.info.dropped
                && !(entry.info.kind == "user" && entry.seq == latest_user)
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

/// Entries the global trim may drop, oldest first. Computing this once keeps
/// `trim` linear instead of rescanning the whole ledger for every drop.
fn trimmable_candidates(entries: &[Entry], sector: Sector, latest_user: Option<u64>) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry.trimmable
                && entry.info.sector == sector
                && !entry.summary
                && !entry.info.pinned
                && !entry.info.dropped
                && !(entry.info.kind == "user" && entry.seq == latest_user)
        })
        .map(|(index, _)| index)
        .collect()
}

fn active_tokens(entries: &[Entry]) -> usize {
    entries
        .iter()
        .filter(|entry| !entry.info.dropped && !entry.info.compacted)
        .map(|entry| entry.info.tokens)
        .sum()
}

fn drop_at(entries: &mut [Entry], index: usize, total: &mut usize) {
    entries[index].info.dropped = true;
    entries[index].info.auto = true;
    *total = total.saturating_sub(entries[index].info.tokens);
}

/// Drop the oldest entry from a normally-fixed sector. These entries are not
/// part of the normal tool/history budget, but when instructions or workspace
/// alone exceed the request budget the alternative is a guaranteed provider
/// context-length error. The `auto` flag makes the loss visible in the ledger.
fn drop_fixed(entries: &mut [Entry], sector: Sector) -> Option<usize> {
    let index = entries.iter().position(|entry| {
        entry.info.sector == sector
            && !entry.info.dropped
            && !entry.info.compacted
            && !entry.info.pinned
            && !entry.summary
    })?;
    entries[index].info.dropped = true;
    entries[index].info.auto = true;
    Some(entries[index].info.tokens)
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

    let mut total = active_tokens(entries);
    if total > budget {
        // Drop the oldest tool entries first, then history, keeping the newest
        // `KEEP_NEWEST` in each sector. Candidates are computed once and the
        // running total is decremented in place, so this is O(entries) rather
        // than re-scanning the ledger for every drop.
        let latest_user = latest_user_seq(entries);
        for sector in [Sector::Tools, Sector::History] {
            let candidates = trimmable_candidates(entries, sector, latest_user);
            let droppable = candidates.len().saturating_sub(KEEP_NEWEST);
            for &index in &candidates[..droppable] {
                if total <= budget {
                    break;
                }
                drop_at(entries, index, &mut total);
            }
            if total <= budget {
                break;
            }
        }
    }

    // The normal pass protects the newest four entries so the model does not
    // lose the tool result it just received. If four large results still exceed
    // the whole budget, keep the newest one plus the current user message and
    // drop older tool/history entries. Without this, a single noisy turn could
    // make every later provider request fail with a context-length error.
    if total > budget {
        emergency_trim(entries, budget, &mut total);
    }

    // Fixed sectors are normally protected, but a huge instruction file or
    // workspace summary must not defeat the budget entirely. Drop them
    // oldest-first and mark them `auto` so the UI can explain the loss.
    for sector in [Sector::Instructions, Sector::Workspace, Sector::Memory] {
        while total > budget {
            let Some(dropped) = drop_fixed(entries, sector) else {
                break;
            };
            total = total.saturating_sub(dropped);
        }
        if total <= budget {
            break;
        }
    }

    entries.iter().filter(|entry| entry.info.auto).count()
}

fn latest_user_seq(entries: &[Entry]) -> Option<u64> {
    entries
        .iter()
        .filter(|entry| entry.info.kind == "user" && !entry.info.dropped && !entry.info.compacted)
        .filter_map(|entry| entry.seq)
        .max()
}

fn emergency_trim(entries: &mut [Entry], budget: usize, total: &mut usize) {
    let latest_user = latest_user_seq(entries);
    let newest_active = entries
        .iter()
        .rposition(|entry| entry.trimmable && !entry.info.dropped && !entry.info.compacted);
    // Same candidate order the old repeated `find` produced, but built once.
    let candidates: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(index, entry)| {
            entry.trimmable
                && !entry.summary
                && !entry.info.pinned
                && !entry.info.dropped
                && !entry.info.compacted
                && Some(*index) != newest_active
                && !(entry.info.kind == "user" && entry.seq == latest_user)
        })
        .map(|(index, _)| index)
        .collect();
    for index in candidates {
        if *total <= budget {
            break;
        }
        drop_at(entries, index, total);
    }
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
    let mut seen = HashSet::new();

    if let Some(dir) = eix_core::config::default_config_path().parent() {
        let path = dir.join("AGENTS.md");
        if seen.insert(path.clone()) {
            if let Ok(text) = tokio::fs::read_to_string(&path).await {
                if !text.trim().is_empty() {
                    out.push(("global AGENTS.md".to_string(), cap_instruction(text)));
                }
            }
        }
    }
    for (path, label) in eix_core::compat::global_instruction_sources(&config.compations) {
        if !seen.insert(path.clone()) {
            continue;
        }
        if let Ok(text) = tokio::fs::read_to_string(&path).await {
            if !text.trim().is_empty() {
                out.push((label, cap_instruction(text)));
            }
        }
    }

    let mut names = config.context.instructions_files.clone();
    for name in eix_core::compat::project_instruction_names(&config.compations) {
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    }

    // Codex-style instruction hierarchy: load AGENTS.md from the repository
    // root down to the session cwd. More specific files come later, so they can
    // refine (or override) the outer instructions. Outside a repository only
    // the cwd is considered, avoiding unrelated files from `$HOME`.
    for dir in instruction_dirs(cwd) {
        for name in &names {
            let path = dir.join(name);
            if !seen.insert(path.clone()) {
                continue;
            }
            if let Ok(text) = tokio::fs::read_to_string(&path).await {
                if !text.trim().is_empty() {
                    let label = path
                        .strip_prefix(cwd)
                        .map(|relative| relative.display().to_string())
                        .unwrap_or_else(|_| path.display().to_string());
                    out.push((label, cap_instruction(text)));
                }
            }
        }
    }
    out
}

fn instruction_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    let mut found_root = false;
    let mut current = Some(cwd);
    while let Some(dir) = current {
        chain.push(dir.to_path_buf());
        if dir.join(".git").exists() {
            found_root = true;
            break;
        }
        current = dir.parent();
    }
    if !found_root {
        chain.truncate(1);
    }
    chain.reverse();
    chain
}

fn cap_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
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

/// Keep the system prompt inside a fixed share of the request budget. A custom
/// `provider_runtime.system_prompt` can otherwise be the one sector that makes every
/// request overflow before any history is even considered.
fn cap_system(text: String, budget_tokens: usize) -> String {
    let max_tokens = (budget_tokens / 4).max(256);
    // One char can be one token (CJK), so using the token budget as the char
    // cap is the only simple bound that holds for mixed-script prompts.
    let max_chars = MAX_SYSTEM_CHARS.min(max_tokens);
    if estimate_tokens(&text) <= max_tokens && text.chars().count() <= max_chars {
        return text;
    }
    let mut out = cap_text(&text, max_chars);
    out.push_str(&format!(
        "\n[truncated by eix: system prompt exceeded {} tokens]",
        max_tokens
    ));
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
    // Most prompt text is ASCII; byte length is an exact answer there and
    // avoids decoding and range-checking every character.
    if text.is_ascii() {
        return text.len().div_ceil(4);
    }
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

/// Keep a tool result bounded while preserving its beginning (usually the
/// command/result header) and its end (usually errors or the final summary).
/// The event log keeps the complete output; only the provider view is elided.
fn truncate_tool_output(text: &str, max_tokens: usize) -> String {
    let max_tokens = max_tokens.max(1);
    if estimate_tokens(text) <= max_tokens {
        return text.to_string();
    }

    // Work in quarter-token units so narrow ASCII (4 chars/token) and wide CJK
    // (1 char/token) share one budget.
    let max_quarters = max_tokens.saturating_mul(4);
    let head_quarters = max_quarters.saturating_mul(3) / 4;
    let tail_quarters = max_quarters.saturating_sub(head_quarters + 24).max(1);

    let mut head_end = 0;
    let mut used = 0;
    for (index, ch) in text.char_indices() {
        let cost = char_quarters(ch);
        if used + cost > head_quarters {
            break;
        }
        used += cost;
        head_end = index + ch.len_utf8();
    }

    let mut tail_start = text.len();
    let mut used = 0;
    for (index, ch) in text.char_indices().rev() {
        let cost = char_quarters(ch);
        if used + cost > tail_quarters {
            break;
        }
        used += cost;
        tail_start = index;
    }

    if tail_start <= head_end {
        let mut out: String = text
            .chars()
            .take(max_tokens.saturating_mul(4).max(256))
            .collect();
        out.push_str("\n… [tool output truncated to fit context]");
        return out;
    }

    let omitted = text[head_end..tail_start].chars().count();
    format!(
        "{}\n\n… [tool output truncated: {omitted} chars elided to fit context; \
         full output remains in the session log] …\n\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

fn char_quarters(ch: char) -> usize {
    if is_wide(ch) {
        4
    } else {
        1
    }
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
    // `&Value` is a deserializer, so decoding borrows the payload instead of
    // deep-cloning it first. History entries are decoded on every rebuild.
    T::deserialize(&event.payload).ok()
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
    fn message_estimate_counts_tool_arguments_and_images() {
        let plain = ChatMessage::assistant("");
        let mut call = call("call_1");
        call.function.arguments = format!("{{\"path\":\"{}\"}}", "x".repeat(4_000));
        let with_tool = ChatMessage::assistant_with_tools("", vec![call]);
        assert!(
            estimate_message_tokens(&with_tool) > estimate_message_tokens(&plain) + 900,
            "tool-call arguments must be part of the request estimate"
        );

        let text_only = ChatMessage::user("look");
        let image = ImagePart {
            mime: "image/png".into(),
            data: "aGVsbG8=".into(),
        };
        let with_image = ChatMessage::user_with_images("look", vec![image]);
        assert_eq!(
            estimate_message_tokens(&with_image),
            estimate_message_tokens(&text_only) + IMAGE_TOKEN_ESTIMATE
        );
    }

    #[test]
    fn tool_schemas_are_visible_but_not_editable_or_sent_as_messages() {
        let tools = vec![ToolDefinition::function(
            "read_file",
            "Read a file from disk",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        )];
        let events = vec![user(1, "hello")];
        let ledger = Ledger::assemble_full(
            "sys".into(),
            vec![],
            None,
            None,
            None,
            &events,
            10_000,
            4_000,
            &HashMap::new(),
            &tools,
            1.0,
        );
        let info = ledger.info();
        let schema = info
            .entries
            .iter()
            .find(|entry| entry.id == "tools:schema")
            .expect("tool schema entry");
        assert_eq!(schema.sector, Sector::Tools);
        assert_eq!(schema.kind, "tool_schema");
        assert!(!schema.editable);
        assert!(schema.tokens > 0);
        // It contributes tokens but no chat message.
        assert_eq!(ledger.messages().len(), 2);
        assert!(info.total_tokens >= schema.tokens);

        // Even under heavy pressure the schema stays: tools cannot be called
        // without it, so it must never be auto-dropped.
        let tight = Ledger::assemble_full(
            "sys".into(),
            vec![],
            None,
            None,
            None,
            &events,
            1_000,
            400,
            &HashMap::new(),
            &tools,
            1.0,
        );
        let schema = tight
            .info()
            .entries
            .into_iter()
            .find(|entry| entry.id == "tools:schema")
            .expect("tool schema entry");
        assert!(!schema.dropped);
    }

    #[test]
    fn calibration_scale_is_applied_before_trimming() {
        let events = vec![user(1, "hello")];
        let base = Ledger::assemble_full(
            "sys".into(),
            vec![],
            None,
            None,
            None,
            &events,
            10_000,
            4_000,
            &HashMap::new(),
            &[],
            1.0,
        );
        let scaled = Ledger::assemble_full(
            "sys".into(),
            vec![],
            None,
            None,
            None,
            &events,
            10_000,
            4_000,
            &HashMap::new(),
            &[],
            2.0,
        );
        let base_user = base
            .info()
            .entries
            .into_iter()
            .find(|entry| entry.id == "e1")
            .unwrap();
        let scaled_user = scaled
            .info()
            .entries
            .into_iter()
            .find(|entry| entry.id == "e1")
            .unwrap();
        assert_eq!(scaled_user.tokens, base_user.tokens * 2);
        assert!(scaled.info().total_tokens > base.info().total_tokens);
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

    #[tokio::test]
    async fn compat_instruction_files_are_opt_in() {
        let dir = std::env::temp_dir().join(format!(
            "eix-context-compat-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "Claude rules").unwrap();
        std::fs::write(dir.join("GEMINI.md"), "Gemini rules").unwrap();

        let mut config = Config::default();
        let base = load_instructions(&config, &dir).await;
        assert!(!base.iter().any(|(label, _)| label == "CLAUDE.md"));
        assert!(!base.iter().any(|(label, _)| label == "GEMINI.md"));

        config.compations.claude_code = true;
        let claude = load_instructions(&config, &dir).await;
        assert!(claude
            .iter()
            .any(|(label, text)| label == "CLAUDE.md" && text == "Claude rules"));

        config.compations.gemini_cli = true;
        let both = load_instructions(&config, &dir).await;
        assert!(both
            .iter()
            .any(|(label, text)| label == "GEMINI.md" && text == "Gemini rules"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hidden_history_is_known_before_loading_images() {
        let events = vec![
            user(1, "one"),
            user(2, "two"),
            user(3, "three"),
            event(
                4,
                EventKind::ContextDropped,
                payload(&eix_core::ContextDropPayload {
                    entry: "e1".into(),
                    dropped: true,
                }),
            ),
            event(
                5,
                EventKind::ContextCompacted,
                payload(&ContextCompactedPayload {
                    from_seq: 2,
                    to_seq: 2,
                    messages: 1,
                    tokens: 1,
                    summary: "two".into(),
                    rolling: false,
                }),
            ),
        ];
        let policy = Policy::from_events(&events);
        assert!(policy.hides_seq(1));
        assert!(policy.hides_seq(2));
        assert!(!policy.hides_seq(3));
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
                    rolling: false,
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
    fn rolling_compaction_supersedes_older_summaries() {
        let events = vec![
            user(1, "one"),
            assistant(2, "two"),
            user(3, "three"),
            assistant(4, "four"),
            user(5, "five"),
            assistant(6, "six"),
            event(
                7,
                EventKind::ContextCompacted,
                payload(&ContextCompactedPayload {
                    from_seq: 1,
                    to_seq: 2,
                    messages: 2,
                    tokens: 10,
                    summary: "old summary".into(),
                    rolling: true,
                }),
            ),
            event(
                8,
                EventKind::ContextCompacted,
                payload(&ContextCompactedPayload {
                    from_seq: 3,
                    to_seq: 4,
                    messages: 2,
                    tokens: 10,
                    summary: "new rolling summary".into(),
                    rolling: true,
                }),
            ),
        ];
        let ledger = Ledger::assemble("sys".into(), vec![], None, &events, 100_000);
        let messages = ledger.messages();
        assert!(!messages.iter().any(|m| m.content.contains("old summary")));
        assert!(messages
            .iter()
            .any(|m| m.content.contains("new rolling summary")));
        assert!(ledger
            .latest_summary()
            .is_some_and(|summary| summary.contains("new rolling summary")));
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
    fn system_prompt_cap_counts_wide_characters_as_tokens() {
        let capped = cap_system("你".repeat(20_000), 8_000);
        // 25% of an 8k budget is 2k tokens; 20k CJK chars would otherwise be
        // 20k tokens even though the character cap looked satisfied.
        assert!(
            estimate_tokens(&capped) <= 2_100,
            "{}",
            estimate_tokens(&capped)
        );
        assert!(capped.contains("truncated by eix"));
    }

    #[test]
    fn oversized_fixed_sectors_are_dropped_and_marked_auto() {
        let ledger = Ledger::assemble(
            "sys".into(),
            vec![("AGENTS.md".into(), "instructions ".repeat(4_000))],
            Some("workspace ".repeat(4_000)),
            &[user(1, "hello")],
            400,
        );
        let info = ledger.info();
        let instructions = info
            .entries
            .iter()
            .find(|entry| entry.id == "instructions:AGENTS.md")
            .expect("instruction entry");
        assert!(
            instructions.dropped && instructions.auto,
            "an instruction file that cannot fit must be visibly dropped"
        );
        let workspace = info
            .entries
            .iter()
            .find(|entry| entry.sector == Sector::Workspace)
            .expect("workspace entry");
        assert!(workspace.dropped && workspace.auto);
        assert!(info.total_tokens <= 400, "{}", info.total_tokens);
        assert!(info.trimmed > 0);
        // The newest user message still reaches the model.
        assert!(ledger
            .messages()
            .iter()
            .any(|message| message.content == "hello"));
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

    /// Helper for the incremental tests: extend with an empty static context
    /// and no images/tools, mirroring what `Daemon::extend_ledger` does at its
    /// simplest.
    fn extend_in_place(ledger: &mut Ledger, all: &[Event], new: &[Event]) -> bool {
        let static_context = StaticContext::default();
        ledger.try_extend(
            new,
            all,
            &HashMap::new(),
            DEFAULT_TOOL_OUTPUT_TOKENS,
            "sys".into(),
            &static_context,
            None,
            None,
            &[],
            1.0,
            10_000,
        )
    }

    fn entry_summary(ledger: &Ledger) -> Vec<(String, usize, bool)> {
        ledger
            .info()
            .entries
            .into_iter()
            .map(|entry| (entry.id, entry.tokens, entry.compacted))
            .collect()
    }

    #[test]
    fn incremental_extend_matches_a_full_build() {
        let first = vec![user(1, "read it")];
        let mut ledger = Ledger::assemble("sys".into(), vec![], None, &first, 10_000);
        let all = vec![
            user(1, "read it"),
            tool_step(2, "", vec![call("call_1")]),
            tool_result(3, "call_1", "fn main() {}"),
            assistant(4, "done"),
        ];
        assert!(extend_in_place(&mut ledger, &all, &all[1..]));
        let full = Ledger::assemble("sys".into(), vec![], None, &all, 10_000);
        assert_eq!(ledger.messages(), full.messages());
        assert_eq!(entry_summary(&ledger), entry_summary(&full));
    }

    #[test]
    fn incremental_extend_refuses_a_cross_batch_tool_result() {
        let first = vec![user(1, "read it"), tool_step(2, "", vec![call("call_1")])];
        let mut ledger = Ledger::assemble("sys".into(), vec![], None, &first, 10_000);
        let mut all = first.clone();
        all.push(tool_result(3, "call_1", "fn main() {}"));
        // The assistant call was built into the ledger by an earlier batch, so
        // the result cannot be attached without a full rebuild.
        assert!(!extend_in_place(&mut ledger, &all, &all[2..]));
    }

    #[test]
    fn incremental_extend_refuses_a_compaction() {
        let first = vec![user(1, "read it"), assistant(2, "done")];
        let mut ledger = Ledger::assemble("sys".into(), vec![], None, &first, 10_000);
        let mut all = first.clone();
        all.push(event(
            3,
            EventKind::ContextCompacted,
            payload(&ContextCompactedPayload {
                from_seq: 1,
                to_seq: 1,
                messages: 1,
                tokens: 1,
                summary: "one".into(),
                rolling: false,
            }),
        ));
        assert!(!extend_in_place(&mut ledger, &all, &all[2..]));
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
        // A tiny token budget keeps the same minimum turns; a budget larger
        // than the live conversation disables compaction entirely.
        assert_eq!(ledger.auto_compact_range_tokens(0, 2), Some((1, 4)));
        assert_eq!(ledger.auto_compact_range_tokens(usize::MAX, 2), None);

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
                rolling: false,
            }),
        ));
        events.push(user(10, "five"));
        events.push(assistant(11, "a5"));
        let ledger = Ledger::assemble("sys".into(), vec![], None, &events, 100_000);
        assert_eq!(ledger.auto_compact_range(2), Some((5, 6)));
    }

    #[test]
    fn tool_output_is_elided_for_the_provider_but_kept_whole_in_the_log() {
        let events = vec![
            user(1, "run it"),
            tool_step(2, "", vec![call("call_1")]),
            tool_result(3, "call_1", &format!("HEAD\n{}\nTAIL", "x".repeat(20_000))),
        ];
        let ledger = Ledger::assemble_full(
            "sys".into(),
            vec![],
            None,
            None,
            None,
            &events,
            100_000,
            100,
            &HashMap::new(),
            &[],
            1.0,
        );
        let tool = ledger
            .messages()
            .into_iter()
            .find(|message| message.role == Role::Tool)
            .unwrap();
        assert!(tool.content.contains("HEAD"));
        assert!(tool.content.contains("TAIL"));
        assert!(tool.content.contains("truncated"));
        assert!(tool.content.chars().count() < 1_000);
        // The event payload still has the full output.
        let original =
            serde_json::from_value::<ToolResultPayload>(events[2].payload.clone()).unwrap();
        assert!(original.output.chars().count() > 20_000);
    }

    #[test]
    fn memory_and_task_state_are_fixed_context_entries() {
        let events = vec![user(1, "do it")];
        let ledger = Ledger::assemble_full(
            "sys".into(),
            vec![],
            None,
            Some("prefers focused diffs".into()),
            Some("Current task list:\n[ ] fix it".into()),
            &events,
            10_000,
            4_000,
            &HashMap::new(),
            &[],
            1.0,
        );
        let info = ledger.info();
        assert!(info.entries.iter().any(|entry| entry.id == "memory"));
        assert!(info.entries.iter().any(|entry| entry.id == "tasks"));
        let memory = info
            .sectors
            .iter()
            .find(|sector| sector.sector == Sector::Memory)
            .unwrap();
        assert_eq!(memory.entries, 2);
        assert!(memory.tokens > 0);
    }

    #[test]
    fn emergency_trim_keeps_the_current_user_turn_and_newest_tool_result() {
        let mut events = Vec::new();
        let mut seq = 1;
        for index in 0..6 {
            events.push(user(seq, "go"));
            seq += 1;
            events.push(tool_step(seq, "", vec![call(&format!("c{index}"))]));
            seq += 1;
            events.push(tool_result(seq, &format!("c{index}"), &"x".repeat(20_000)));
            seq += 1;
        }
        let ledger = Ledger::assemble_full(
            "sys".into(),
            vec![],
            None,
            None,
            None,
            &events,
            1_500,
            400,
            &HashMap::new(),
            &[],
            1.0,
        );
        let info = ledger.info();
        assert!(info.total_tokens <= info.budget_tokens, "{info:?}");
        let entries = info.entries;
        assert!(entries
            .iter()
            .any(|entry| entry.kind == "user" && entry.id == format!("e{}", seq - 3)));
        assert!(entries
            .iter()
            .filter(|entry| entry.sector == Sector::Tools)
            .any(|entry| !entry.dropped));
    }

    #[test]
    fn instruction_chain_stops_at_the_repository_root() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("eix-instructions-{nanos}"));
        let crate_dir = root.join("crate");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&crate_dir).unwrap();
        let dirs = instruction_dirs(&crate_dir);
        assert_eq!(dirs, vec![root.clone(), crate_dir.clone()]);
        // A directory outside a repository only contributes itself.
        let loose = std::env::temp_dir().join(format!("eix-loose-{nanos}"));
        std::fs::create_dir_all(&loose).unwrap();
        assert_eq!(instruction_dirs(&loose), vec![loose.clone()]);
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(loose);
    }

    #[test]
    fn reused_tool_call_ids_match_the_following_result() {
        // Some providers return `call_1` on every turn. The result after each
        // assistant message must pair with that message, not with the first
        // turn's call of the same id.
        let events = vec![
            user(1, "first"),
            tool_step(2, "", vec![call("call_1")]),
            tool_result(3, "call_1", "first result"),
            user(4, "second"),
            tool_step(5, "", vec![call("call_1")]),
            tool_result(6, "call_1", "second result"),
        ];
        let messages = Ledger::assemble("sys".into(), vec![], None, &events, 10_000).messages();
        let tools: Vec<_> = messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(tools, vec!["first result", "second result"]);
        assert_eq!(messages.last().unwrap().content, "second result");
    }
}
