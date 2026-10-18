//! Background processes started by the agent.
//!
//! A process belongs to the daemon, not to a frontend: closing the TUI does
//! not stop a dev server. Output is streamed as notifications and mirrored to
//! a per-process log file; only start/exit are persisted as events so a chatty
//! process cannot flood the event log.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    Running,
    /// Exited on its own (including non-zero codes).
    Exited,
    /// Stopped by `process.kill` or daemon shutdown.
    Killed,
    /// Could not be waited on; the daemon lost track of it.
    Failed,
}

impl ProcessStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Killed => "killed",
            Self::Failed => "failed",
        }
    }

    pub fn is_running(self) -> bool {
        self == Self::Running
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub id: String,
    pub session_id: String,
    pub command: String,
    /// Working directory the command was started in.
    pub cwd: String,
    /// Process id of the direct child (the shell / bwrap wrapper).
    pub pid: u32,
    pub status: ProcessStatus,
    /// Exit code when the process exited normally; `None` when signalled.
    #[serde(default)]
    pub exit_code: Option<i32>,
    pub started_ms: i64,
    #[serde(default)]
    pub ended_ms: Option<i64>,
    /// Total bytes of stdout + stderr seen so far.
    #[serde(default)]
    pub bytes: u64,
    /// Output stopped being mirrored to the log file / live stream.
    #[serde(default)]
    pub truncated: bool,
    /// Absolute path of the log file.
    pub log: String,
}

impl ProcessInfo {
    /// Stable short form for tables; the full id remains the protocol key.
    pub fn short_id(&self) -> &str {
        self.id.get(..8).unwrap_or(&self.id)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessList {
    pub processes: Vec<ProcessInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessOutput {
    pub process_id: String,
    /// Tail of the output, capped by the caller's `max_bytes`.
    pub text: String,
    /// Total bytes seen by the process, not just the returned tail.
    #[serde(default)]
    pub bytes: u64,
    /// The process produced more output than the returned tail.
    #[serde(default)]
    pub truncated: bool,
    #[serde(default = "default_running")]
    pub status: ProcessStatus,
    #[serde(default)]
    pub exit_code: Option<i32>,
}

fn default_running() -> ProcessStatus {
    ProcessStatus::Running
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessKillResult {
    pub killed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_are_stable_strings() {
        for (status, text) in [
            (ProcessStatus::Running, "running"),
            (ProcessStatus::Exited, "exited"),
            (ProcessStatus::Killed, "killed"),
            (ProcessStatus::Failed, "failed"),
        ] {
            assert_eq!(status.as_str(), text);
            assert_eq!(status.is_running(), status == ProcessStatus::Running);
        }
    }

    #[test]
    fn short_id_is_a_prefix() {
        let mut info = ProcessInfo {
            id: "0123456789abcdef".into(),
            session_id: "s".into(),
            command: "true".into(),
            cwd: "/tmp".into(),
            pid: 1,
            status: ProcessStatus::Exited,
            exit_code: Some(0),
            started_ms: 0,
            ended_ms: Some(1),
            bytes: 0,
            truncated: false,
            log: "/tmp/x".into(),
        };
        assert_eq!(info.short_id(), "01234567");
        info.id = "tiny".into();
        assert_eq!(info.short_id(), "tiny");
    }
}
