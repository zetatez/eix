//! Context ledger types shared by the daemon and every frontend.
//!
//! The daemon owns the actual ledger (it decides what enters a request); these
//! types describe the result so a client can render and edit it without knowing
//! how messages are reconstructed from the event log.

use serde::{Deserialize, Serialize};

/// One bucket of the context window. Budget percentages come from `DESIGN.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sector {
    /// The built-in system prompt.
    System,
    /// `AGENTS.md` and other configured instruction files.
    Instructions,
    /// A compact snapshot of the workspace (top-level entries).
    Workspace,
    /// Volatile environment facts: cwd, OS, shell, date, git, sandbox, mode.
    Environment,
    /// Conversation turns.
    History,
    /// Tool calls and their output.
    Tools,
    /// Cross-session notes plus the current task list (`memory` / `tasks`).
    Memory,
}

impl Sector {
    /// Fixed display/accounting order.
    pub const ALL: [Sector; 7] = [
        Sector::System,
        Sector::Instructions,
        Sector::Workspace,
        Sector::Environment,
        Sector::History,
        Sector::Tools,
        Sector::Memory,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Instructions => "instructions",
            Self::Workspace => "workspace",
            Self::Environment => "environment",
            Self::History => "history",
            Self::Tools => "tools",
            Self::Memory => "memory",
        }
    }

    /// Share of the total budget this sector may use when the request is over
    /// budget. Free budget is not redistributed away from recent tool output.
    pub fn budget_percent(self) -> f64 {
        match self {
            Self::System => 0.05,
            Self::Instructions => 0.05,
            Self::Workspace => 0.12,
            Self::Environment => 0.03,
            Self::History => 0.30,
            Self::Tools => 0.40,
            Self::Memory => 0.05,
        }
    }
}

impl std::str::FromStr for Sector {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "system" => Self::System,
            "instructions" => Self::Instructions,
            "workspace" => Self::Workspace,
            "environment" => Self::Environment,
            "history" => Self::History,
            "tools" => Self::Tools,
            "memory" => Self::Memory,
            _ => return Err(()),
        })
    }
}

/// One row of the ledger. `id` is stable for the lifetime of the underlying
/// source: `system`, `workspace`, `instructions:<file>` or `e<event seq>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEntryInfo {
    pub id: String,
    pub sector: Sector,
    /// `system`, `instructions`, `workspace`, `user`, `assistant`, `tool_step`,
    /// `tool`, `tool_schema` or `summary`.
    pub kind: String,
    pub tokens: usize,
    pub chars: usize,
    /// First line, truncated for display.
    pub preview: String,
    /// Longer text (still capped) for the expand view.
    pub detail: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub dropped: bool,
    /// Covered by a compaction; still in the log, not sent to the provider.
    #[serde(default)]
    pub compacted: bool,
    /// Dropped automatically to fit the budget rather than by an explicit
    /// policy event.
    #[serde(default)]
    pub auto: bool,
    /// Whether automatic trimming may target this entry.
    #[serde(default)]
    pub editable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSectorInfo {
    pub sector: Sector,
    pub tokens: usize,
    pub budget_tokens: usize,
    pub entries: usize,
    #[serde(default)]
    pub pinned: usize,
    #[serde(default)]
    pub dropped: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextLedgerResult {
    pub entries: Vec<ContextEntryInfo>,
    pub sectors: Vec<ContextSectorInfo>,
    pub total_tokens: usize,
    pub budget_tokens: usize,
    /// Entries dropped automatically to fit the budget (not a user policy).
    #[serde(default)]
    pub trimmed: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sectors_round_trip_and_budget_sums_to_one() {
        let sum: f64 = Sector::ALL.iter().map(|s| s.budget_percent()).sum();
        assert!((sum - 1.0).abs() < f64::EPSILON);
        for sector in Sector::ALL {
            assert_eq!(sector.as_str().parse::<Sector>(), Ok(sector));
        }
        assert!("nope".parse::<Sector>().is_err());
    }
}
