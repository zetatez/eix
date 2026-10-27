//! Permission assessment and decision pipeline.
//!
//! Four layers, in order:
//! 1. the session execution mode (`SessionMode::Plan` is read-only, ...),
//! 2. risk analysis of the concrete tool call,
//! 3. explicit rules (`deny` always wins, then the most specific rule),
//! 4. the mode's default-ask policy (only applies to unruled `ask` results).
//!
//! `Auto` leaves unruled default asks in place for the daemon's intent-aware
//! classifier; `Bypass` allows them directly. The classifier can only turn a
//! remaining `ask` into `allow`, and it never widens the workspace boundary.

use std::path::Path;

use eix_core::risk::{
    assess_command, assess_read_path, assess_write_path, RiskAssessment, RiskCategory, RiskLevel,
};
use eix_core::{Config, PermissionAction, PermissionRule, RuleSource, SandboxMode, SessionMode};
use serde_json::Value;

/// Internal capability ceiling derived from [`SessionMode`]. Not part of the
/// public core API; callers choose a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capability {
    ReadOnly,
    WorkspaceWrite,
    Full,
}

impl From<SessionMode> for Capability {
    fn from(mode: SessionMode) -> Self {
        match mode {
            SessionMode::Plan => Self::ReadOnly,
            SessionMode::Build | SessionMode::Auto => Self::WorkspaceWrite,
            SessionMode::Bypass => Self::Full,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PermissionDecision {
    pub action: PermissionAction,
    pub risk: RiskAssessment,
    /// The rule that produced the decision, if one matched.
    pub rule: Option<PermissionRule>,
    /// Human-readable explanation shown in the approval card / audit event.
    pub reason: String,
    /// `ask` that must not be auto-approved by the mode (for example a
    /// sandboxed build when bubblewrap is unavailable, or an explicit rule).
    pub force_ask: bool,
}

impl PermissionDecision {
    pub fn rule_source(&self) -> RuleSource {
        self.rule
            .as_ref()
            .map(|rule| rule.source)
            .unwrap_or(RuleSource::Builtin)
    }

    pub fn rule_id(&self) -> Option<&str> {
        self.rule
            .as_ref()
            .map(|rule| rule.id.as_str())
            .filter(|id| !id.is_empty())
    }
}

/// Whether a `exec_command` call is a low-risk, read-only command that can run
/// alongside other reads. This is deliberately stricter than "does not mutate
/// the workspace": network, git-publish, secret reads and anything the analyzer
/// cannot prove are left in the sequential path where approval still applies.
pub fn command_is_parallel_readonly(args: &Value, cwd: &Path) -> bool {
    let Some(command) = args
        .get("cmd")
        .or_else(|| args.get("command"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    if command.trim().is_empty() {
        return false;
    }
    let risk = assess_command(command, cwd);
    !risk.hard_deny
        && !risk.mutates_workspace()
        && risk.level <= RiskLevel::Low
        && risk
            .categories
            .iter()
            .all(|category| matches!(category, RiskCategory::Read | RiskCategory::ExecSafe))
}

/// Analyze a tool call before any rule or approval decision. This never
/// executes anything and is also used to render the approval card.
pub fn assess_tool(
    tool: &str,
    args: &Value,
    cwd: &Path,
    config: &Config,
    mcp_known: bool,
) -> RiskAssessment {
    match tool {
        "lsp_diagnostics" | "view_image" => {
            let raw = args
                .get("path")
                .and_then(Value::as_str)
                .filter(|path| !path.trim().is_empty())
                .unwrap_or(".");
            let allowed = crate::tools::resolve_path_in(cwd, raw, config)
                .ok()
                .is_some_and(|path| config.tools.external_path_allowed(&path));
            assess_read_path(raw, cwd, allowed)
        }
        "apply_patch" => {
            let mut risk = RiskAssessment::new();
            let text = ["input", "patchText", "patch", "text"]
                .iter()
                .find_map(|key| args.get(*key).and_then(Value::as_str))
                .unwrap_or_default();
            let mut found = false;
            for line in text.lines() {
                let raw = line
                    .strip_prefix("*** Add File: ")
                    .or_else(|| line.strip_prefix("*** Update File: "))
                    .or_else(|| line.strip_prefix("*** Delete File: "))
                    .or_else(|| line.strip_prefix("*** Move to: "));
                if let Some(raw) = raw {
                    found = true;
                    let allowed = crate::tools::resolve_path_in(cwd, raw.trim(), config)
                        .ok()
                        .is_some_and(|path| config.tools.external_path_allowed(&path));
                    risk.merge(assess_write_path(raw.trim(), cwd, allowed));
                }
            }
            if !found {
                risk.add(
                    RiskCategory::WriteLocal,
                    RiskLevel::Medium,
                    "apply_patch changes workspace files".to_string(),
                );
            }
            risk
        }
        "exec_command" => {
            let command = args
                .get("cmd")
                .or_else(|| args.get("command"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut risk = assess_command(command, cwd);
            if eix_core::redact::contains_placeholder(command) {
                risk.add(
                    RiskCategory::SecretExfil,
                    RiskLevel::Critical,
                    "shell command contains a withheld secret placeholder".to_string(),
                );
                risk.hard_deny = true;
            }
            risk
        }
        "write_stdin" => {
            let mut risk = RiskAssessment::new();
            risk.add(
                RiskCategory::ExecUnknown,
                RiskLevel::Low,
                "writes to an already-approved process".to_string(),
            );
            risk
        }
        "web_fetch" => {
            let url = args.get("url").and_then(Value::as_str).unwrap_or_default();
            let mut risk = RiskAssessment::new();
            if eix_core::redact::contains_placeholder(url) {
                risk.add(
                    RiskCategory::SecretExfil,
                    RiskLevel::Critical,
                    format!(
                        "network request contains a withheld secret placeholder: {}",
                        redact_url(url)
                    ),
                );
                risk.hard_deny = true;
            } else if config.tools.network_allow.is_empty() {
                risk.add(
                    RiskCategory::Network,
                    RiskLevel::Low,
                    format!("read-only network fetch: {}", redact_url(url)),
                );
            } else if config.tools.network_host_allowed(url) {
                risk.add(
                    RiskCategory::Network,
                    RiskLevel::Low,
                    format!("fetch is on the network allowlist: {}", redact_url(url)),
                );
            } else {
                risk.add(
                    RiskCategory::Network,
                    RiskLevel::High,
                    format!(
                        "fetch is not on the configured network allowlist: {}",
                        redact_url(url)
                    ),
                );
            }
            risk
        }
        "web_search" => {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut risk = RiskAssessment::new();
            if eix_core::redact::contains_placeholder(query) {
                risk.add(
                    RiskCategory::SecretExfil,
                    RiskLevel::Critical,
                    "search query contains a withheld secret placeholder".to_string(),
                );
                risk.hard_deny = true;
            } else if config.tools.network_allow.is_empty() {
                risk.add(
                    RiskCategory::Network,
                    RiskLevel::Low,
                    "read-only web search".to_string(),
                );
            } else if config.tools.network_host_allowed("https://duckduckgo.com/") {
                risk.add(
                    RiskCategory::Network,
                    RiskLevel::Low,
                    "web_search is on the network allowlist".to_string(),
                );
            } else {
                risk.add(
                    RiskCategory::Network,
                    RiskLevel::High,
                    "web_search is not on the configured network allowlist".to_string(),
                );
            }
            risk
        }
        tool if tool.starts_with(eix_core::PLUGIN_TOOL_PREFIX) => {
            let mut risk = RiskAssessment::new();
            if eix_core::redact::contains_placeholder(&args.to_string()) {
                risk.add(
                    RiskCategory::SecretExfil,
                    RiskLevel::Critical,
                    "plugin arguments contain a withheld secret placeholder".to_string(),
                );
                risk.hard_deny = true;
                return risk;
            }
            // The host knows the child process and capability grant, but it
            // cannot prove what the plugin will do with its arguments. Always
            // require approval; read-only tools can become low risk later.
            risk.add(
                RiskCategory::Plugin,
                RiskLevel::High,
                "calls an external plugin tool".to_string(),
            );
            risk
        }
        tool if tool.starts_with(crate::mcp::PREFIX) => {
            let mut risk = RiskAssessment::new();
            if eix_core::redact::contains_placeholder(&args.to_string()) {
                risk.add(
                    RiskCategory::SecretExfil,
                    RiskLevel::Critical,
                    "MCP arguments contain a withheld secret placeholder".to_string(),
                );
                risk.hard_deny = true;
                return risk;
            }
            if matches!(
                tool,
                crate::mcp::LIST_RESOURCES
                    | crate::mcp::LIST_PROMPTS
                    | crate::mcp::READ_RESOURCE
                    | crate::mcp::GET_PROMPT
            ) {
                risk.add(
                    RiskCategory::Read,
                    RiskLevel::Low,
                    "reads an MCP resource".to_string(),
                );
            } else if mcp_known && mcp_read_only_tool(tool) {
                risk.add(
                    RiskCategory::Read,
                    RiskLevel::Low,
                    "calls a read-only MCP tool".to_string(),
                );
            } else {
                risk.add(
                    RiskCategory::McpWrite,
                    RiskLevel::High,
                    if mcp_known {
                        "calls an external MCP tool".to_string()
                    } else {
                        "calls an unregistered MCP tool".to_string()
                    },
                );
            }
            risk
        }
        _ => {
            let mut risk = RiskAssessment::new();
            risk.add(
                RiskCategory::Read,
                RiskLevel::Low,
                "reads daemon or session state".to_string(),
            );
            risk
        }
    }
}

/// MCP tools whose names describe a read-only lookup. Unknown or unregistered
/// tools are never auto-approved: the server must have advertised the tool
/// first, and only common `get_*` / `list_*` style names qualify.
fn mcp_read_only_tool(tool: &str) -> bool {
    let Some((_, name)) = tool.rsplit_once("__") else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "get"
            | "list"
            | "read"
            | "search"
            | "find"
            | "query"
            | "fetch"
            | "lookup"
            | "describe"
            | "show"
            | "status"
            | "info"
    ) || [
        "get_",
        "list_",
        "read_",
        "search_",
        "find_",
        "query_",
        "fetch_",
        "lookup_",
        "describe_",
        "show_",
        "status_",
        "info_",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

/// Decide what to do with a tool call. `session_rules` are the in-memory
/// "Always" choices for this session and have `RuleSource::Session`.
pub fn decide(
    tool: &str,
    args: &Value,
    cwd: &Path,
    config: &Config,
    mode: SessionMode,
    session_rules: &[PermissionRule],
    mcp_known: bool,
) -> PermissionDecision {
    let mut risk = assess_tool(tool, args, cwd, config, mcp_known);
    let capability = Capability::from(mode);
    let full_access = capability == Capability::Full;
    let sandbox_available = match config.tools.sandbox {
        SandboxMode::Off => false,
        SandboxMode::Auto | SandboxMode::Required => eix_core::sandbox::bwrap_available(),
    };

    // Bypass lifts the workspace path boundary for file tools. Keep the hard
    // denial for secret-path combinations and all catastrophic commands.
    if full_access
        && risk.hard_deny
        && risk.categories.len() == 1
        && risk.categories.contains(&RiskCategory::WriteExternal)
    {
        risk.hard_deny = false;
        risk.level = RiskLevel::High;
    }

    // A shell argument that leaves the workspace through `..` or a symlink is
    // a boundary escape, not just a risky read. Refuse it in every mode; the
    // same `tools.external_directory` allowlist file tools use is the only
    // way to name the target explicitly.
    if let Some(escaped) = risk.workspace_escape.clone() {
        let target = eix_core::risk::normalize_path(&cwd.join(&escaped));
        let target = target.canonicalize().unwrap_or(target);
        if !config.tools.external_path_allowed(&target) {
            return PermissionDecision {
                action: PermissionAction::Deny,
                risk,
                rule: None,
                reason: format!("path escapes the session workspace: {escaped}"),
                force_ask: false,
            };
        }
    }

    let capability_denial = read_only_denial(capability, &risk, matches!(tool, "exec_command"));
    if let Some(reason) = capability_denial {
        return PermissionDecision {
            action: PermissionAction::Deny,
            risk,
            rule: None,
            reason,
            force_ask: false,
        };
    }
    if risk.hard_deny {
        let reason = risk
            .reasons
            .first()
            .cloned()
            .unwrap_or_else(|| "blocked by the built-in safety guard".to_string());
        return PermissionDecision {
            action: PermissionAction::Deny,
            risk,
            rule: None,
            reason,
            force_ask: false,
        };
    }

    let mut matching: Vec<PermissionRule> = config
        .tools
        .rules
        .iter()
        .chain(session_rules.iter())
        .filter(|rule| rule.matches(tool, args))
        .cloned()
        .collect();

    // A deny anywhere wins, regardless of source or specificity.
    if let Some(rule) = matching
        .iter()
        .filter(|rule| rule.action == PermissionAction::Deny)
        .max_by_key(|rule| (rule.specificity(), source_rank(rule.source)))
        .cloned()
    {
        return PermissionDecision {
            action: PermissionAction::Deny,
            risk,
            rule: Some(rule),
            reason: "permission rule denies this call".to_string(),
            force_ask: false,
        };
    }
    matching.retain(|rule| rule.action != PermissionAction::Deny);
    if let Some(rule) = matching.into_iter().max_by_key(|rule| {
        (
            rule.specificity(),
            source_rank(rule.source),
            action_rank(rule.action),
        )
    }) {
        let (action, force_ask, reason) = match rule.action {
            PermissionAction::Allow => (
                PermissionAction::Allow,
                false,
                format!("allowed by rule {}", rule_label(&rule)),
            ),
            PermissionAction::Ask => (
                PermissionAction::Ask,
                true,
                format!("rule {} requires approval", rule_label(&rule)),
            ),
            PermissionAction::Sandbox if sandbox_available && matches!(tool, "exec_command") => (
                PermissionAction::Sandbox,
                false,
                format!("rule {} requires the sandbox", rule_label(&rule)),
            ),
            PermissionAction::Sandbox if matches!(tool, "exec_command") => (
                PermissionAction::Ask,
                true,
                format!(
                    "rule {} requires a sandbox, but bubblewrap is unavailable",
                    rule_label(&rule)
                ),
            ),
            PermissionAction::Sandbox => (
                PermissionAction::Ask,
                true,
                format!(
                    "rule {} requested a sandbox for a tool that cannot be sandboxed",
                    rule_label(&rule)
                ),
            ),
            PermissionAction::Deny => unreachable!(),
        };
        return PermissionDecision {
            action,
            risk,
            rule: Some(rule),
            reason,
            force_ask,
        };
    }

    let default = risk.default_action();
    // Workspace-write modes can apply low/medium in-workspace edits without a
    // manual approval for every file. Snapshots and the recorded auto-patch
    // still provide an audit trail and undo. Secret paths, external paths, and
    // higher-risk writes continue to ask or deny.
    let workspace_edit = config.tools.auto_apply_workspace_edits
        && mode.applies_workspace_edits()
        && matches!(tool, "apply_patch")
        && default == PermissionAction::Ask
        && !risk.hard_deny
        && risk.level <= RiskLevel::Medium;
    let (action, force_ask, reason) = match default {
        PermissionAction::Ask if workspace_edit => (
            PermissionAction::Allow,
            false,
            "session mode allows in-workspace edits".to_string(),
        ),
        PermissionAction::Ask if mode.allows_unruled_asks() => {
            (PermissionAction::Allow, false, auto_allow_reason(mode))
        }
        PermissionAction::Deny => (
            PermissionAction::Deny,
            false,
            risk.reasons
                .first()
                .cloned()
                .unwrap_or_else(|| "risk analysis denied this call".to_string()),
        ),
        PermissionAction::Allow => (
            PermissionAction::Allow,
            false,
            risk.reasons
                .first()
                .cloned()
                .unwrap_or_else(|| "risk analysis allows this call".to_string()),
        ),
        PermissionAction::Ask => (
            PermissionAction::Ask,
            false,
            risk.reasons
                .first()
                .cloned()
                .unwrap_or_else(|| "risk analysis requires approval".to_string()),
        ),
        PermissionAction::Sandbox if sandbox_available => (
            PermissionAction::Sandbox,
            false,
            risk.reasons
                .first()
                .cloned()
                .unwrap_or_else(|| "runs inside the sandbox".to_string()),
        ),
        PermissionAction::Sandbox if mode.allows_unsandboxed_exec() => (
            PermissionAction::Allow,
            false,
            "session mode bypass allows an unsandboxed build command".to_string(),
        ),
        PermissionAction::Sandbox => (
            PermissionAction::Ask,
            true,
            "this command needs a sandbox but bubblewrap is unavailable".to_string(),
        ),
    };
    PermissionDecision {
        action,
        risk,
        rule: None,
        reason,
        force_ask,
    }
}

/// Why an unruled default `ask` was allowed. Only `Bypass` takes this path;
/// `Auto` is handled by the daemon's classifier.
fn auto_allow_reason(mode: SessionMode) -> String {
    match mode {
        SessionMode::Bypass => "session mode bypass allows unruled asks".to_string(),
        _ => "session mode allows unruled asks".to_string(),
    }
}

fn read_only_denial(
    capability: Capability,
    risk: &RiskAssessment,
    shell_command: bool,
) -> Option<String> {
    if capability != Capability::ReadOnly {
        return None;
    }
    if risk.categories.iter().any(|category| {
        matches!(
            category,
            RiskCategory::WriteLocal
                | RiskCategory::WriteExternal
                | RiskCategory::ExecUnknown
                | RiskCategory::ExecSandboxed
                | RiskCategory::Destructive
                | RiskCategory::GitPublish
                | RiskCategory::GitRewrite
                | RiskCategory::McpWrite
                | RiskCategory::Plugin
                | RiskCategory::Privilege
                | RiskCategory::SecretExfil
        ) || (shell_command && *category == RiskCategory::Network)
    }) {
        Some("session mode is plan (read-only); mutating tools are disabled".to_string())
    } else {
        None
    }
}

fn source_rank(source: RuleSource) -> usize {
    match source {
        RuleSource::Builtin => 0,
        RuleSource::Project => 1,
        RuleSource::User => 2,
        RuleSource::Session => 3,
    }
}

fn action_rank(action: PermissionAction) -> usize {
    match action {
        PermissionAction::Allow => 0,
        PermissionAction::Sandbox => 1,
        PermissionAction::Ask => 2,
        PermissionAction::Deny => 3,
    }
}

fn rule_label(rule: &PermissionRule) -> String {
    if rule.id.is_empty() {
        format!("{} ({})", rule.tool, rule.source.as_str())
    } else {
        rule.id.clone()
    }
}

/// Keep URLs in logs and approval cards free of query-string secrets.
pub fn redact_url(url: &str) -> String {
    let (base, has_query) = match url.split_once('?') {
        Some((base, _)) => (base, true),
        None => (url, false),
    };
    let base = if let Some((scheme, rest)) = base.split_once("://") {
        let (authority, tail) = rest.split_once('/').unwrap_or((rest, ""));
        match authority.rsplit_once('@') {
            Some((_, host)) if !tail.is_empty() => format!("{scheme}://…@{host}/{tail}"),
            Some((_, host)) => format!("{scheme}://…@{host}"),
            None => base.to_string(),
        }
    } else {
        base.to_string()
    };
    if has_query {
        format!("{base}?…")
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> Config {
        Config::default()
    }

    fn cwd() -> &'static Path {
        Path::new("/tmp")
    }

    /// A one-file `apply_patch` document, the only write tool now.
    fn patch(path: &str) -> serde_json::Value {
        json!({
            "patchText": format!(
                "*** Begin Patch\n*** Update File: {path}\n@@\n-x\n+y\n*** End Patch\n"
            )
        })
    }

    #[test]
    fn read_only_rejects_writes_but_allows_reads() {
        let config = config();
        let read = decide(
            "exec_command",
            &json!({"cmd": "cat src/main.rs"}),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Plan,
            &[],
            false,
        );
        assert_eq!(read.action, PermissionAction::Allow);
        let write = decide(
            "apply_patch",
            &patch("src/main.rs"),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Plan,
            &[],
            false,
        );
        assert_eq!(write.action, PermissionAction::Deny);
        let network = decide(
            "exec_command",
            &json!({"cmd": "git fetch"}),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Plan,
            &[],
            false,
        );
        assert_eq!(network.action, PermissionAction::Deny);
    }

    #[test]
    fn shell_workspace_escapes_are_denied() {
        let config = config();
        let escaped = decide(
            "exec_command",
            &json!({"cmd": "cat ../../etc/passwd"}),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Build,
            &[],
            false,
        );
        assert_eq!(escaped.action, PermissionAction::Deny, "{escaped:?}");
        assert!(
            escaped.reason.contains("escapes the session workspace"),
            "{escaped:?}"
        );

        // An absolute path outside is an explicit read: approval, not a denial.
        let external = decide(
            "exec_command",
            &json!({"cmd": "cat /etc/hosts"}),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Build,
            &[],
            false,
        );
        assert_eq!(external.action, PermissionAction::Ask, "{external:?}");

        // `external_directory` is the explicit escape hatch.
        let mut allowed = config;
        allowed.tools.external_directory = vec!["/etc".into()];
        let escaped = decide(
            "exec_command",
            &json!({"cmd": "cat ../../etc/hosts"}),
            Path::new("/tmp/eix-perm"),
            &allowed,
            SessionMode::Build,
            &[],
            false,
        );
        assert_eq!(escaped.action, PermissionAction::Ask, "{escaped:?}");
    }

    #[test]
    fn workspace_write_applies_in_workspace_edits() {
        let config = config();
        let decision = decide(
            "apply_patch",
            &patch("src/main.rs"),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Build,
            &[],
            false,
        );
        assert_eq!(decision.action, PermissionAction::Allow, "{decision:?}");
        assert!(decision.risk.categories.contains(&RiskCategory::WriteLocal));

        // Secret and external paths keep their existing guardrails.
        let secret = decide(
            "apply_patch",
            &patch(".env"),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Build,
            &[],
            false,
        );
        assert_eq!(secret.action, PermissionAction::Ask, "{secret:?}");
        let external = decide(
            "apply_patch",
            &patch("/etc/passwd"),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Build,
            &[],
            false,
        );
        assert_eq!(external.action, PermissionAction::Deny, "{external:?}");
        assert!(external.risk.hard_deny);

        // Read-only sessions still reject edits.
        let read_only = decide(
            "apply_patch",
            &patch("src/main.rs"),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Plan,
            &[],
            false,
        );
        assert_eq!(read_only.action, PermissionAction::Deny);

        // The automatic apply can be disabled for users who want to review
        // every patch.
        let mut manual = config;
        manual.tools.auto_apply_workspace_edits = false;
        let staged = decide(
            "apply_patch",
            &patch("src/main.rs"),
            Path::new("/tmp/eix-perm"),
            &manual,
            SessionMode::Build,
            &[],
            false,
        );
        assert_eq!(staged.action, PermissionAction::Ask, "{staged:?}");
    }

    #[test]
    fn common_read_only_shell_pipelines_do_not_ask() {
        let config = config();
        for command in [
            "cd /tmp/eix-perm && cat Cargo.toml | head -20",
            "cd /tmp/eix-perm && grep -rn TODO crates --include=*.rs | head -20",
            "cd /tmp/eix-perm && sed -n '1,80p' crates/eix-core/src/risk.rs",
            "cd /tmp/eix-perm && awk 'END{print NR}' crates/eix-core/src/risk.rs",
            "cd /tmp/eix-perm && git status --short",
            "git --version",
            "find . -name '*.rs' | xargs wc -l | sort -n | tail -1",
            "find . -name '*.rs' | xargs cat | wc -l",
            "echo \"lines: $(wc -l < Cargo.toml)\"",
            "for f in crates/*/Cargo.toml; do echo \"--- $f\"; cat \"$f\"; done",
            "for d in crates/*/; do printf '%-24s %6s\\n' \"$d\" \"$(find $d -name '*.rs' | xargs cat | wc -l)\"; done",
            "for c in eix-core eixd; do n=$(find crates/$c -name '*.rs' | xargs cat | wc -l); echo \"$c: $n lines\"; done",
            "for f in $(find crates -name '*.rs'); do cat \"./$f\"; done",
            "for f in $(find crates -name '*.rs'); do echo \"$f\"; done | sort",
            "env FOO=1 timeout 5 ls crates",
        ] {
            let decision = decide(
                "exec_command",
                &json!({"cmd": command}),
                Path::new("/tmp/eix-perm"),
                &config,
                SessionMode::Build,
                &[],
                false,
            );
            assert_eq!(
                decision.action,
                PermissionAction::Allow,
                "{command}: {decision:?}"
            );
            assert!(!decision.risk.hard_deny, "{command}: {decision:?}");
        }
    }

    #[test]
    fn parallel_readonly_commands_are_conservatively_classified() {
        let cwd = Path::new("/tmp/eix-perm");
        for command in [
            "cat Cargo.toml",
            "ls -la crates",
            "rg -n TODO crates",
            "git status --short",
            "git diff --stat",
            "cat Cargo.toml | head -20",
            "echo hi",
        ] {
            assert!(
                command_is_parallel_readonly(&json!({"cmd": command}), cwd),
                "{command} should be parallel read-only"
            );
        }
        for command in [
            "rm -rf target",
            "sed -i 's/x/y/' Cargo.toml",
            "git fetch",
            "curl https://example.com",
            "cat .env",
            "sleep 1",
            "make test",
        ] {
            assert!(
                !command_is_parallel_readonly(&json!({"cmd": command}), cwd),
                "{command} should stay sequential"
            );
        }
    }

    #[test]
    fn mutating_shell_commands_stay_gated() {
        let config = config();
        for command in [
            "sed -i 's/x/y/' file",
            "awk 'BEGIN{system(\"rm -rf /\")}'",
            "find . -name '*.rs' | xargs sort -o out",
            "for f in $(find crates -name '*.rs'); do cat \"$f\"; done",
            "for f in $(date); do echo \"$f\" | sh; done",
            "cd /tmp && cat passwd",
        ] {
            let decision = decide(
                "exec_command",
                &json!({"cmd": command}),
                Path::new("/tmp/eix-perm"),
                &config,
                SessionMode::Build,
                &[],
                false,
            );
            assert_ne!(
                decision.action,
                PermissionAction::Allow,
                "{command}: {decision:?}"
            );
        }
    }

    #[test]
    fn full_does_not_bypass_hard_deny_or_rules() {
        let mut config = config();
        config.tools.rules = vec![PermissionRule {
            tool: "exec_command".into(),
            command: Some(vec!["echo".into()]),
            action: PermissionAction::Deny,
            ..PermissionRule::default()
        }];
        let denied = decide(
            "exec_command",
            &json!({"cmd": "echo hi"}),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Bypass,
            &[],
            false,
        );
        assert_eq!(denied.action, PermissionAction::Deny);
        let hard = decide(
            "exec_command",
            &json!({"cmd": "rm -rf /"}),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Bypass,
            &[],
            false,
        );
        assert_eq!(hard.action, PermissionAction::Deny);
    }

    #[test]
    fn bypass_mode_allows_unruled_asks_and_external_writes() {
        let config = config();
        for (tool, args) in [
            ("apply_patch", patch("out.txt")),
            ("apply_patch", patch("/tmp/eix-perm-outside.txt")),
            ("exec_command", json!({"cmd": "git push"})),
            ("exec_command", json!({"cmd": "rm -f build/out"})),
        ] {
            let decision = decide(
                tool,
                &args,
                Path::new("/tmp/eix-perm"),
                &config,
                SessionMode::Bypass,
                &[],
                false,
            );
            assert_eq!(
                decision.action,
                PermissionAction::Allow,
                "{tool}: {decision:?}"
            );
        }

        // Explicit ask rules still win over the bypass mode.
        let mut config = config;
        config.tools.rules = vec![PermissionRule {
            tool: "exec_command".into(),
            command: Some(vec!["git".into(), "push".into()]),
            action: PermissionAction::Ask,
            ..PermissionRule::default()
        }];
        let explicit = decide(
            "exec_command",
            &json!({"cmd": "git push"}),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Bypass,
            &[],
            false,
        );
        assert_eq!(explicit.action, PermissionAction::Ask);
        assert!(explicit.force_ask);
    }

    #[test]
    fn auto_mode_defers_unruled_asks_to_the_classifier() {
        let config = config();
        let edit = decide(
            "apply_patch",
            &patch("out.txt"),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Auto,
            &[],
            false,
        );
        assert_eq!(edit.action, PermissionAction::Allow, "{edit:?}");

        for (tool, args) in [
            ("exec_command", json!({"cmd": "git push"})),
            ("exec_command", json!({"cmd": "rm -f build/out"})),
        ] {
            let decision = decide(
                tool,
                &args,
                Path::new("/tmp/eix-perm"),
                &config,
                SessionMode::Auto,
                &[],
                false,
            );
            assert_eq!(
                decision.action,
                PermissionAction::Ask,
                "{tool}: {decision:?}"
            );
            assert!(!decision.force_ask);
            assert!(decision.rule.is_none());
        }

        // Auto is workspace-write, not bypass: external file writes stay
        // blocked by the hard path guard.
        let external = decide(
            "apply_patch",
            &patch("/tmp/eix-perm-outside.txt"),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Auto,
            &[],
            false,
        );
        assert_eq!(external.action, PermissionAction::Deny, "{external:?}");
        assert!(external.risk.hard_deny);
    }

    #[test]
    fn build_mode_asks_for_unruled_risky_calls() {
        let config = config();
        let decision = decide(
            "exec_command",
            &json!({"cmd": "git push"}),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Build,
            &[],
            false,
        );
        assert_eq!(decision.action, PermissionAction::Ask, "{decision:?}");
        assert!(!decision.force_ask);

        let external = decide(
            "apply_patch",
            &patch("/tmp/eix-perm-outside.txt"),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Build,
            &[],
            false,
        );
        assert_eq!(external.action, PermissionAction::Deny, "{external:?}");
    }

    #[test]
    fn bypass_mode_runs_build_commands_without_a_sandbox() {
        let mut config = config();
        config.tools.sandbox = eix_core::SandboxMode::Off;
        let decision = decide(
            "exec_command",
            &json!({"cmd": "cargo test"}),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Bypass,
            &[],
            false,
        );
        assert_eq!(decision.action, PermissionAction::Allow);
    }

    #[test]
    fn explicit_ask_beats_auto_mode() {
        let mut config = config();
        config.tools.rules = vec![PermissionRule {
            id: "ask-push".into(),
            tool: "exec_command".into(),
            command: Some(vec!["git".into(), "push".into()]),
            action: PermissionAction::Ask,
            ..PermissionRule::default()
        }];
        let decision = decide(
            "exec_command",
            &json!({"cmd": "git push"}),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Auto,
            &[],
            false,
        );
        assert_eq!(decision.action, PermissionAction::Ask);
        assert!(decision.force_ask);
    }

    #[test]
    fn specific_allow_beats_generic_ask() {
        let mut config = config();
        config.tools.rules = vec![
            PermissionRule {
                tool: "exec_command".into(),
                action: PermissionAction::Ask,
                ..PermissionRule::default()
            },
            PermissionRule {
                tool: "exec_command".into(),
                command: Some(vec!["cargo".into(), "test".into()]),
                action: PermissionAction::Allow,
                ..PermissionRule::default()
            },
        ];
        let decision = decide(
            "exec_command",
            &json!({"cmd": "cargo test"}),
            Path::new("/tmp/eix-perm"),
            &config,
            SessionMode::Build,
            &[],
            false,
        );
        assert_eq!(decision.action, PermissionAction::Allow);
    }

    #[test]
    fn hard_deny_beats_auto_mode_and_allow_rules() {
        let mut config = config();
        config.tools.rules = vec![PermissionRule {
            tool: "*".to_string(),
            action: PermissionAction::Allow,
            ..Default::default()
        }];
        let decision = decide(
            "exec_command",
            &json!({"cmd": "rm -rf /"}),
            cwd(),
            &config,
            SessionMode::Bypass,
            &[],
            false,
        );
        assert_eq!(decision.action, PermissionAction::Deny, "{decision:?}");
        assert!(decision.risk.hard_deny);
    }

    #[test]
    fn deny_rule_beats_a_more_specific_allow_rule() {
        let mut config = config();
        config.tools.rules = vec![
            PermissionRule {
                id: "allow-git".to_string(),
                tool: "exec_command".to_string(),
                command: Some(vec!["git".to_string(), "status".to_string()]),
                action: PermissionAction::Allow,
                ..Default::default()
            },
            PermissionRule {
                id: "deny-shell".to_string(),
                tool: "exec_command".to_string(),
                action: PermissionAction::Deny,
                source: RuleSource::Project,
                ..Default::default()
            },
        ];
        let decision = decide(
            "exec_command",
            &json!({"cmd": "git status"}),
            cwd(),
            &config,
            SessionMode::Bypass,
            &[],
            false,
        );
        assert_eq!(decision.action, PermissionAction::Deny);
        assert_eq!(decision.rule_id(), Some("deny-shell"));
    }

    #[test]
    fn read_only_capability_wins_over_an_allow_rule() {
        let mut config = config();
        config.tools.rules = vec![PermissionRule {
            tool: "apply_patch".to_string(),
            action: PermissionAction::Allow,
            ..Default::default()
        }];
        let decision = decide(
            "apply_patch",
            &patch("out.txt"),
            cwd(),
            &config,
            SessionMode::Plan,
            &[],
            false,
        );
        assert_eq!(decision.action, PermissionAction::Deny);
        assert!(decision.reason.contains("read-only"));
    }

    #[test]
    fn network_tools_allow_read_only_requests_and_honour_an_opt_in_allowlist() {
        let mut config = config();
        let decision = decide(
            "web_fetch",
            &json!({"url": "https://example.com/docs?token=secret"}),
            cwd(),
            &config,
            SessionMode::Build,
            &[],
            false,
        );
        assert_eq!(decision.action, PermissionAction::Allow);
        assert_eq!(decision.risk.level, RiskLevel::Low);
        assert!(decision.risk.categories.contains(&RiskCategory::Network));
        assert!(!decision.risk.reasons.join(" ").contains("token=secret"));
        assert!(redact_url("https://example.com/docs?token=secret")
            .starts_with("https://example.com/docs"));
        assert_eq!(
            redact_url("https://user:pass@example.com/docs?token=secret"),
            "https://…@example.com/docs?…"
        );

        // Populating network_allow opts into an allowlist instead of allowing
        // every GET-style fetch.
        config.tools.network_allow = vec!["example.com".to_string()];
        let allowed = decide(
            "web_fetch",
            &json!({"url": "https://example.com/docs"}),
            cwd(),
            &config,
            SessionMode::Build,
            &[],
            false,
        );
        assert_eq!(allowed.action, PermissionAction::Allow);
        assert_eq!(allowed.risk.level, RiskLevel::Low);

        let blocked = decide(
            "web_fetch",
            &json!({"url": "https://other.example/docs"}),
            cwd(),
            &config,
            SessionMode::Build,
            &[],
            false,
        );
        assert_eq!(blocked.action, PermissionAction::Ask);
        assert_eq!(blocked.risk.level, RiskLevel::High);
    }

    #[test]
    fn unknown_mcp_tools_are_not_implicitly_read_only() {
        let config = config();
        let decision = decide(
            "mcp__server__delete_everything",
            &json!({}),
            cwd(),
            &config,
            SessionMode::Plan,
            &[],
            false,
        );
        assert_eq!(decision.action, PermissionAction::Deny, "{decision:?}");
        assert!(decision.risk.categories.contains(&RiskCategory::McpWrite));
        assert!(decision.reason.contains("read-only"));

        let unregistered = decide(
            "mcp__server__get_everything",
            &json!({}),
            cwd(),
            &config,
            SessionMode::Build,
            &[],
            false,
        );
        assert_eq!(unregistered.action, PermissionAction::Ask);
        assert!(unregistered
            .risk
            .categories
            .contains(&RiskCategory::McpWrite));
    }

    #[test]
    fn registered_read_only_mcp_tools_do_not_ask() {
        let config = config();
        for tool in [
            "mcp__server__get_item",
            "mcp__server__list_items",
            "mcp__server__search",
        ] {
            let decision = decide(
                tool,
                &json!({}),
                cwd(),
                &config,
                SessionMode::Plan,
                &[],
                true,
            );
            assert_eq!(
                decision.action,
                PermissionAction::Allow,
                "{tool}: {decision:?}"
            );
            assert!(decision.risk.categories.contains(&RiskCategory::Read));
        }
    }

    #[test]
    fn web_search_can_be_allowed_by_the_network_allowlist() {
        let mut config = config();
        config.tools.network_allow = vec!["duckduckgo.com".to_string()];
        let decision = decide(
            "web_search",
            &json!({"query": "rust ownership"}),
            cwd(),
            &config,
            SessionMode::Build,
            &[],
            false,
        );
        assert_eq!(decision.action, PermissionAction::Allow, "{decision:?}");
        assert_eq!(decision.risk.level, RiskLevel::Low);
    }

    #[test]
    fn withheld_secrets_cannot_enter_network_shell_or_mcp_calls() {
        let config = config();
        let placeholder = "{{eix_secret_deadbeef}}";
        for (tool, args) in [
            (
                "web_fetch",
                json!({"url": format!("https://example.com/?q={placeholder}")}),
            ),
            ("web_search", json!({"query": placeholder})),
            (
                "exec_command",
                json!({"cmd": format!("curl https://example.com/{placeholder}")}),
            ),
            (
                "mcp__server__send",
                json!({"text": format!("token={placeholder}")}),
            ),
        ] {
            let decision = decide(tool, &args, cwd(), &config, SessionMode::Build, &[], true);
            assert_eq!(decision.action, PermissionAction::Deny, "{tool}");
            assert!(decision.risk.hard_deny, "{tool}");
        }
    }

    #[test]
    fn auto_mode_does_not_bypass_an_unavailable_sandbox() {
        let mut config = config();
        config.tools.sandbox = SandboxMode::Off;
        let decision = decide(
            "exec_command",
            &json!({"cmd": "cargo test"}),
            cwd(),
            &config,
            SessionMode::Auto,
            &[],
            false,
        );
        assert_eq!(decision.action, PermissionAction::Ask, "{decision:?}");
        assert!(decision.force_ask);
    }
}
