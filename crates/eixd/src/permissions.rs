//! Permission assessment and decision pipeline.
//!
//! Four layers, in order:
//! 1. the session/agent capability ceiling (`read-only` etc.),
//! 2. risk analysis of the concrete tool call,
//! 3. explicit rules (`deny` always wins, then the most specific rule),
//! 4. approval mode (only applies to unruled `ask` results).

use std::path::Path;

use eix_core::risk::{
    assess_command, assess_read_path, assess_write_path, RiskAssessment, RiskCategory, RiskLevel,
};
use eix_core::{
    ApprovalMode, Config, PermissionAction, PermissionLevel, PermissionRule, RuleSource,
    SandboxMode,
};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct PermissionDecision {
    pub action: PermissionAction,
    pub risk: RiskAssessment,
    /// The rule that produced the decision, if one matched.
    pub rule: Option<PermissionRule>,
    /// Human-readable explanation shown in the approval card / audit event.
    pub reason: String,
    /// `ask` that must not be auto-approved by `approval = "auto"` (for
    /// example a sandboxed build when bubblewrap is unavailable).
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
        "read_file" | "list_dir" | "grep" | "glob" | "lsp_diagnostics" => {
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
        "write_file" | "edit" => {
            let raw = args.get("path").and_then(Value::as_str).unwrap_or_default();
            let allowed = crate::tools::resolve_path_in(cwd, raw, config)
                .ok()
                .is_some_and(|path| config.tools.external_path_allowed(&path));
            assess_write_path(raw, cwd, allowed)
        }
        "patch" => {
            let mut risk = RiskAssessment::new();
            let text = args
                .get("patchText")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut found = false;
            for line in text.lines() {
                let raw = line
                    .strip_prefix("*** Add File: ")
                    .or_else(|| line.strip_prefix("*** Update File: "))
                    .or_else(|| line.strip_prefix("*** Delete File: "));
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
                    "patch changes workspace files".to_string(),
                );
            }
            risk
        }
        "run_command" | "start_command" => {
            let command = args
                .get("command")
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
        "webfetch" => {
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
        "websearch" => {
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
                    "websearch is on the network allowlist".to_string(),
                );
            } else {
                risk.add(
                    RiskCategory::Network,
                    RiskLevel::High,
                    "websearch is not on the configured network allowlist".to_string(),
                );
            }
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
    permission: PermissionLevel,
    session_rules: &[PermissionRule],
    mcp_known: bool,
) -> PermissionDecision {
    let risk = assess_tool(tool, args, cwd, config, mcp_known);
    let sandbox_available = match config.tools.sandbox {
        SandboxMode::Off => false,
        SandboxMode::Auto | SandboxMode::Required => eix_core::sandbox::bwrap_available(),
    };

    let capability_denial = read_only_denial(
        permission,
        &risk,
        matches!(tool, "run_command" | "start_command"),
    );
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
            PermissionAction::Sandbox
                if sandbox_available && matches!(tool, "run_command" | "start_command") =>
            {
                (
                    PermissionAction::Sandbox,
                    false,
                    format!("rule {} requires the sandbox", rule_label(&rule)),
                )
            }
            PermissionAction::Sandbox if matches!(tool, "run_command" | "start_command") => (
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
    let (action, force_ask, reason) = match default {
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
        PermissionAction::Ask if config.tools.approval == ApprovalMode::Auto => (
            PermissionAction::Allow,
            false,
            "approval mode auto allows unruled asks".to_string(),
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

fn read_only_denial(
    permission: PermissionLevel,
    risk: &RiskAssessment,
    shell_command: bool,
) -> Option<String> {
    if permission != PermissionLevel::ReadOnly {
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
                | RiskCategory::Privilege
                | RiskCategory::SecretExfil
        ) || (shell_command && *category == RiskCategory::Network)
    }) {
        Some("session permission is read-only; mutating tools are disabled".to_string())
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

    #[test]
    fn read_only_rejects_writes_but_allows_reads() {
        let config = config();
        let read = decide(
            "read_file",
            &json!({"path": "src/main.rs"}),
            Path::new("/tmp/eix-perm"),
            &config,
            PermissionLevel::ReadOnly,
            &[],
            false,
        );
        assert_eq!(read.action, PermissionAction::Allow);
        let write = decide(
            "write_file",
            &json!({"path": "src/main.rs", "content": "x"}),
            Path::new("/tmp/eix-perm"),
            &config,
            PermissionLevel::ReadOnly,
            &[],
            false,
        );
        assert_eq!(write.action, PermissionAction::Deny);
        let network = decide(
            "run_command",
            &json!({"command": "git fetch"}),
            Path::new("/tmp/eix-perm"),
            &config,
            PermissionLevel::ReadOnly,
            &[],
            false,
        );
        assert_eq!(network.action, PermissionAction::Deny);
    }

    #[test]
    fn full_does_not_bypass_hard_deny_or_rules() {
        let mut config = config();
        config.tools.rules = vec![PermissionRule {
            tool: "run_command".into(),
            command: Some(vec!["echo".into()]),
            action: PermissionAction::Deny,
            ..PermissionRule::default()
        }];
        let denied = decide(
            "run_command",
            &json!({"command": "echo hi"}),
            Path::new("/tmp/eix-perm"),
            &config,
            PermissionLevel::Full,
            &[],
            false,
        );
        assert_eq!(denied.action, PermissionAction::Deny);
        let hard = decide(
            "run_command",
            &json!({"command": "rm -rf /"}),
            Path::new("/tmp/eix-perm"),
            &config,
            PermissionLevel::Full,
            &[],
            false,
        );
        assert_eq!(hard.action, PermissionAction::Deny);
    }

    #[test]
    fn explicit_ask_beats_auto_approval() {
        let mut config = config();
        config.tools.approval = ApprovalMode::Auto;
        config.tools.rules = vec![PermissionRule {
            id: "ask-push".into(),
            tool: "run_command".into(),
            command: Some(vec!["git".into(), "push".into()]),
            action: PermissionAction::Ask,
            ..PermissionRule::default()
        }];
        let decision = decide(
            "run_command",
            &json!({"command": "git push"}),
            Path::new("/tmp/eix-perm"),
            &config,
            PermissionLevel::WorkspaceWrite,
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
                tool: "run_command".into(),
                action: PermissionAction::Ask,
                ..PermissionRule::default()
            },
            PermissionRule {
                tool: "run_command".into(),
                command: Some(vec!["cargo".into(), "test".into()]),
                action: PermissionAction::Allow,
                ..PermissionRule::default()
            },
        ];
        let decision = decide(
            "run_command",
            &json!({"command": "cargo test"}),
            Path::new("/tmp/eix-perm"),
            &config,
            PermissionLevel::WorkspaceWrite,
            &[],
            false,
        );
        assert_eq!(decision.action, PermissionAction::Allow);
    }

    #[test]
    fn hard_deny_beats_auto_mode_and_allow_rules() {
        let mut config = config();
        config.tools.approval = ApprovalMode::Auto;
        config.tools.rules = vec![PermissionRule {
            tool: "*".to_string(),
            action: PermissionAction::Allow,
            ..Default::default()
        }];
        let decision = decide(
            "run_command",
            &json!({"command": "rm -rf /"}),
            cwd(),
            &config,
            PermissionLevel::Full,
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
                tool: "run_command".to_string(),
                command: Some(vec!["git".to_string(), "status".to_string()]),
                action: PermissionAction::Allow,
                ..Default::default()
            },
            PermissionRule {
                id: "deny-shell".to_string(),
                tool: "run_command".to_string(),
                action: PermissionAction::Deny,
                source: RuleSource::Project,
                ..Default::default()
            },
        ];
        let decision = decide(
            "run_command",
            &json!({"command": "git status"}),
            cwd(),
            &config,
            PermissionLevel::Full,
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
            tool: "write_file".to_string(),
            action: PermissionAction::Allow,
            ..Default::default()
        }];
        let decision = decide(
            "write_file",
            &json!({"path": "out.txt", "content": "x"}),
            cwd(),
            &config,
            PermissionLevel::ReadOnly,
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
            "webfetch",
            &json!({"url": "https://example.com/docs?token=secret"}),
            cwd(),
            &config,
            PermissionLevel::WorkspaceWrite,
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
            "webfetch",
            &json!({"url": "https://example.com/docs"}),
            cwd(),
            &config,
            PermissionLevel::WorkspaceWrite,
            &[],
            false,
        );
        assert_eq!(allowed.action, PermissionAction::Allow);
        assert_eq!(allowed.risk.level, RiskLevel::Low);

        let blocked = decide(
            "webfetch",
            &json!({"url": "https://other.example/docs"}),
            cwd(),
            &config,
            PermissionLevel::WorkspaceWrite,
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
            PermissionLevel::ReadOnly,
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
            PermissionLevel::WorkspaceWrite,
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
                PermissionLevel::ReadOnly,
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
    fn websearch_can_be_allowed_by_the_network_allowlist() {
        let mut config = config();
        config.tools.network_allow = vec!["duckduckgo.com".to_string()];
        let decision = decide(
            "websearch",
            &json!({"query": "rust ownership"}),
            cwd(),
            &config,
            PermissionLevel::WorkspaceWrite,
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
                "webfetch",
                json!({"url": format!("https://example.com/?q={placeholder}")}),
            ),
            ("websearch", json!({"query": placeholder})),
            (
                "run_command",
                json!({"command": format!("curl https://example.com/{placeholder}")}),
            ),
            (
                "mcp__server__send",
                json!({"text": format!("token={placeholder}")}),
            ),
        ] {
            let decision = decide(
                tool,
                &args,
                cwd(),
                &config,
                PermissionLevel::WorkspaceWrite,
                &[],
                true,
            );
            assert_eq!(decision.action, PermissionAction::Deny, "{tool}");
            assert!(decision.risk.hard_deny, "{tool}");
        }
    }

    #[test]
    fn auto_mode_does_not_bypass_an_unavailable_sandbox() {
        let mut config = config();
        config.tools.approval = ApprovalMode::Auto;
        config.tools.sandbox = SandboxMode::Off;
        let decision = decide(
            "run_command",
            &json!({"command": "cargo test"}),
            cwd(),
            &config,
            PermissionLevel::WorkspaceWrite,
            &[],
            false,
        );
        assert_eq!(decision.action, PermissionAction::Ask, "{decision:?}");
        assert!(decision.force_ask);
    }
}
