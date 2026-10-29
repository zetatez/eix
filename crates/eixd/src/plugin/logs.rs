//! Per-plugin log ring buffer.
//!
//! Logs arrive from three places: process stderr, `host.log` calls, and the
//! daemon's own lifecycle transitions. Each plugin keeps the most recent 500
//! lines. The buffer is intentionally in memory only: `plugin_kv` remains the
//! durable store, and logs are diagnostics for the current daemon run.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use eix_core::plugin::{PluginLogEntry, PluginLogsResult};
use eix_core::redact::redact;

/// Lines kept per plugin.
pub const LOG_LINES_PER_PLUGIN: usize = 500;
/// Largest single message, after redaction.
const MAX_LOG_MESSAGE: usize = 4096;
/// Cap for one `plugin.logs` response.
pub const MAX_LOG_PAGE: usize = 500;

#[derive(Default)]
pub struct PluginLogs {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next_seq: u64,
    by_plugin: BTreeMap<String, VecDeque<PluginLogEntry>>,
}

impl PluginLogs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one line. Never panics: a poisoned lock is recovered because a
    /// log line is not worth taking the daemon down for.
    pub fn push(&self, plugin: &str, level: &str, message: &str) {
        let message = cap(&redact(message), MAX_LOG_MESSAGE);
        let level = match level {
            "error" | "warn" | "info" | "debug" | "stderr" => level,
            _ => "info",
        };
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.next_seq = inner.next_seq.saturating_add(1);
        let entry = PluginLogEntry {
            seq: inner.next_seq,
            plugin: plugin.to_string(),
            timestamp_ms: now_ms(),
            level: level.to_string(),
            message,
        };
        let buffer = inner.by_plugin.entry(plugin.to_string()).or_default();
        buffer.push_back(entry);
        while buffer.len() > LOG_LINES_PER_PLUGIN {
            buffer.pop_front();
        }
    }

    /// Entries matching `name` (or every plugin when absent) with a sequence
    /// number greater than `since`.
    pub fn snapshot(
        &self,
        name: Option<&str>,
        since: Option<u64>,
        limit: Option<usize>,
    ) -> PluginLogsResult {
        let since = since.unwrap_or(0);
        let limit = limit.unwrap_or(MAX_LOG_PAGE).clamp(1, MAX_LOG_PAGE);
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut entries: Vec<PluginLogEntry> = match name {
            Some(name) => inner
                .by_plugin
                .get(name)
                .map(|buffer| {
                    buffer
                        .iter()
                        .filter(|entry| entry.seq > since)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default(),
            None => inner
                .by_plugin
                .values()
                .flat_map(|buffer| buffer.iter())
                .filter(|entry| entry.seq > since)
                .cloned()
                .collect(),
        };
        entries.sort_by_key(|entry| entry.seq);
        // Keep the newest page when the caller is catching up after a gap.
        if entries.len() > limit {
            entries.drain(..entries.len() - limit);
        }
        let next = entries
            .last()
            .map(|entry| entry.seq)
            .unwrap_or(inner.next_seq.max(since));
        PluginLogsResult { entries, next }
    }

    pub fn clear(&self, name: Option<&str>) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match name {
            Some(name) => {
                inner.by_plugin.remove(name);
            }
            None => inner.by_plugin.clear(),
        }
    }

    pub fn len(&self, name: &str) -> usize {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.by_plugin.get(name).map(VecDeque::len).unwrap_or(0)
    }

    /// Number of `error` lines, used by `plugin.doctor`.
    pub fn error_count(&self, name: &str) -> usize {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .by_plugin
            .get(name)
            .map(|buffer| buffer.iter().filter(|entry| entry.level == "error").count())
            .unwrap_or(0)
    }

    pub fn last(&self, name: &str) -> Option<PluginLogEntry> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .by_plugin
            .get(name)
            .and_then(|buffer| buffer.back())
            .cloned()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn cap(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_newest_page_and_advances_since() {
        let logs = PluginLogs::new();
        for index in 0..10 {
            logs.push("alpha", "info", &format!("line {index}"));
        }
        let first = logs.snapshot(Some("alpha"), None, Some(3));
        assert_eq!(first.entries.len(), 3);
        assert_eq!(first.entries[0].message, "line 7");
        assert_eq!(first.entries[2].message, "line 9");

        logs.push("alpha", "error", "boom");
        let next = logs.snapshot(Some("alpha"), Some(first.next), None);
        assert_eq!(next.entries.len(), 1);
        assert_eq!(next.entries[0].level, "error");
        assert!(next.next > first.next);
    }

    #[test]
    fn ring_is_bounded_and_redacts() {
        let logs = PluginLogs::new();
        for index in 0..(LOG_LINES_PER_PLUGIN + 20) {
            logs.push("alpha", "info", &format!("{index}"));
        }
        assert_eq!(logs.len("alpha"), LOG_LINES_PER_PLUGIN);
        let page = logs.snapshot(Some("alpha"), None, None);
        assert_eq!(page.entries.len(), LOG_LINES_PER_PLUGIN);
        assert_eq!(page.entries[0].message, "20");

        logs.push("alpha", "info", "key sk-abcdefghijklmnopqrstuvwxyz012345");
        let last = logs.last("alpha").unwrap();
        assert!(!last.message.contains("sk-abcdefghijklmnopqrstuvwxyz012345"));
    }

    #[test]
    fn filters_by_plugin_and_counts_errors() {
        let logs = PluginLogs::new();
        logs.push("alpha", "error", "one");
        logs.push("beta", "info", "two");
        assert_eq!(logs.snapshot(Some("beta"), None, None).entries.len(), 1);
        assert_eq!(logs.snapshot(None, None, None).entries.len(), 2);
        assert_eq!(logs.error_count("alpha"), 1);
        assert_eq!(logs.error_count("beta"), 0);
    }
}
