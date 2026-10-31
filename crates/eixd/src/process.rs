//! Registry of processes started by `exec_command`.
//!
//! Processes belong to the daemon, so a frontend can disconnect without
//! stopping a dev server. Output is streamed as `process.output`
//! notifications and mirrored to `<data>/processes/<session>/<id>.log`; only
//! `ProcessStarted` / `ProcessExited` are persisted as events, so a chatty
//! process cannot flood the event log. `write_stdin` writes to the child and
//! drains output produced since the previous poll.
//!
//! Lifecycle: a process is spawned in its own process group so a kill reaches
//! its children too. It dies with the daemon (`kill_all` on shutdown, plus
//! `kill_on_drop`); the registry is in memory, so after a daemon restart the
//! event log may show a start without a matching exit.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use eix_core::{
    now_ms, payload, EventKind, Notification, ProcessExitedPayload, ProcessInfo, ProcessOutput,
    ProcessStartedPayload, ProcessStatus,
};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, oneshot, Mutex as AsyncMutex};

use crate::store::Store;
use crate::tools::CommandPlan;

/// Extra bytes read on each pipe read.
const READ_CHUNK: usize = 8192;
/// In-memory tail kept per process for `process.output` and the TUI.
const TAIL_BYTES: usize = 128 * 1024;
/// Stop mirroring output to the log file past this size.
const LOG_CAP_BYTES: u64 = 16 * 1024 * 1024;
/// Stop pushing live notifications past this size; the tail keeps updating.
const STREAM_CAP_BYTES: u64 = 8 * 1024 * 1024;
/// Finished processes kept in memory per session.
const KEEP_FINISHED: usize = 32;
/// Running processes allowed per session. Without a cap a long-lived daemon
/// accumulates a shell, pipes and file descriptors for every command the
/// model leaves running; refused calls name the oldest ones so the model can
/// stop them.
const MAX_RUNNING: usize = 16;
/// How long a killed process gets to exit before SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// Bounds for the advised delay between empty `write_stdin` polls. The upper
/// bound keeps a long download from waking the model more than twice a minute.
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// A percentage observation older than this no longer says anything useful
/// about the current rate (the process may be stuck or waiting for input).
const ETA_FRESHNESS: Duration = Duration::from_secs(60);

pub struct ProcessRegistry {
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    bus: broadcast::Sender<Notification>,
    store: Arc<Store>,
    data_dir: PathBuf,
}

struct Entry {
    info: Mutex<ProcessInfo>,
    /// Last [`TAIL_BYTES`] of stdout + stderr.
    tail: Mutex<String>,
    /// Bytes produced since the last `write_stdin` poll.
    pending: Mutex<String>,
    /// Stdin of the child; `None` once it closed or exited.
    stdin: AsyncMutex<Option<tokio::process::ChildStdin>>,
    /// Consumed by the monitor to switch it into kill mode.
    killer: Mutex<Option<oneshot::Sender<()>>>,
    log_file: AsyncMutex<tokio::fs::File>,
    progress: Mutex<Progress>,
}

/// Cheap completion tracking so an empty `write_stdin` poll can wait until the
/// process exits or a sensible next-check time instead of returning as soon as
/// a single progress frame arrives.
struct Progress {
    last_output: Instant,
    /// Latest completion percentage seen in the stream.
    percent: Option<f64>,
    /// The percentage and when it was observed, used for the ETA slope.
    mark: Option<(f64, Instant)>,
    /// Smoothed estimate of the time left.
    eta: Option<Duration>,
    /// Backoff used when the stream has no percentage to extrapolate from.
    poll_interval: Duration,
    /// Percentage at the previous empty poll, to detect forward progress.
    last_poll_percent: Option<f64>,
}

impl Progress {
    fn new(now: Instant) -> Self {
        Self {
            last_output: now,
            percent: None,
            mark: None,
            eta: None,
            poll_interval: MIN_POLL_INTERVAL,
            last_poll_percent: None,
        }
    }

    /// Record a percentage parsed from one output chunk. A backwards jump means
    /// a new phase (the next file in a batch), so the old ETA is discarded.
    fn observe(&mut self, percent: f64, now: Instant) {
        let percent = percent.clamp(0.0, 100.0);
        if let Some(old) = self.percent {
            if (percent - old).abs() < f64::EPSILON {
                return;
            }
            if percent + 5.0 < old {
                self.eta = None;
                self.mark = Some((percent, now));
                self.percent = Some(percent);
                return;
            }
        }
        if let Some((old, at)) = self.mark {
            let elapsed = now.saturating_duration_since(at).as_secs_f64();
            let advanced = percent - old;
            if elapsed >= 0.2 && advanced > 0.0 {
                let rate = advanced / elapsed;
                let remaining = ((100.0 - percent) / rate).clamp(0.0, 86_400.0);
                let sample = Duration::from_secs_f64(remaining);
                // Smooth the estimate so one burst of progress does not
                // promise an impossibly early finish.
                self.eta = Some(match self.eta {
                    Some(old) => Duration::from_secs_f64(
                        old.as_secs_f64() * 0.6 + sample.as_secs_f64() * 0.4,
                    ),
                    None => sample,
                });
            }
        }
        self.percent = Some(percent);
        self.mark = Some((percent, now));
    }

    /// Delay the next empty poll should sleep for.
    fn suggested_wait(&self) -> Duration {
        let eta = self
            .eta
            .filter(|eta| *eta <= Duration::from_secs(86_400))
            .filter(|_| {
                self.mark
                    .is_some_and(|(_, at)| at.elapsed() <= ETA_FRESHNESS)
            })
            .filter(|_| self.percent.is_some_and(|percent| percent < 100.0));
        let wait = match eta {
            Some(eta) => eta.max(MIN_POLL_INTERVAL),
            None => self.poll_interval,
        };
        // Silence is itself information: after a quiet stretch, checking less
        // often is safe and saves a wake-up (and its tokens).
        let quiet = self.last_output.elapsed();
        let quiet_wait = quiet.saturating_mul(2).min(MAX_POLL_INTERVAL);
        wait.max(quiet_wait)
            .clamp(MIN_POLL_INTERVAL, MAX_POLL_INTERVAL)
    }

    /// Update the backoff after a poll, and report the next suggested delay.
    fn finish_poll(&mut self, wrote: bool) -> Duration {
        if !wrote {
            if self.percent != self.last_poll_percent {
                self.poll_interval = MIN_POLL_INTERVAL;
            } else {
                self.poll_interval =
                    (self.poll_interval * 2).clamp(MIN_POLL_INTERVAL, MAX_POLL_INTERVAL);
            }
            self.last_poll_percent = self.percent;
        }
        self.suggested_wait()
    }
}

impl ProcessRegistry {
    pub fn new(store: Arc<Store>, bus: broadcast::Sender<Notification>, data_dir: PathBuf) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            bus,
            store,
            data_dir: data_dir.join("processes"),
        }
    }

    /// Spawn a process and return its metadata immediately.
    pub async fn start(
        self: &Arc<Self>,
        session_id: &str,
        command: &str,
        plan: CommandPlan,
    ) -> Result<ProcessInfo> {
        self.check_running_cap(session_id)?;
        let mut cmd = plan.command();
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("cannot start command in {}", plan.cwd.display()))?;
        let stdin = child.stdin.take();

        let pid = child.id().unwrap_or(0);
        let id = uuid::Uuid::new_v4().to_string();
        let log_path = self.data_dir.join(session_id).join(format!("{id}.log"));
        if let Some(parent) = log_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
            Store::set_permissions(parent)?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
            .await?;
        Store::set_permissions(&log_path)?;

        let info = ProcessInfo {
            id: id.clone(),
            session_id: session_id.to_string(),
            command: command.to_string(),
            cwd: plan.cwd.display().to_string(),
            pid,
            status: ProcessStatus::Running,
            exit_code: None,
            started_ms: now_ms(),
            ended_ms: None,
            bytes: 0,
            truncated: false,
            log: log_path.display().to_string(),
        };

        let entry = Arc::new(Entry {
            info: Mutex::new(info.clone()),
            tail: Mutex::new(String::new()),
            pending: Mutex::new(String::new()),
            stdin: AsyncMutex::new(stdin),
            killer: Mutex::new(None),
            log_file: AsyncMutex::new(file),
            progress: Mutex::new(Progress::new(Instant::now())),
        });
        let (kill_tx, kill_rx) = oneshot::channel();
        *entry.killer.lock().expect("process mutex poisoned") = Some(kill_tx);
        self.entries
            .lock()
            .expect("process mutex poisoned")
            .insert(id.clone(), Arc::clone(&entry));
        self.prune_finished(session_id);

        let started = Instant::now();

        // Persist the start before the monitor can append an exit, so the log
        // reads start -> exit even for an instantly failing command.
        let event = self.store.append_event(
            session_id,
            EventKind::ProcessStarted,
            payload(&ProcessStartedPayload {
                process_id: id.clone(),
                command: command.to_string(),
                cwd: info.cwd.clone(),
                pid,
            }),
        )?;
        let _ = self.bus.send(Notification::new("session.event", event));
        let _ = self
            .bus
            .send(Notification::new("process.started", info.clone()));

        spawn_pump(
            child.stdout.take(),
            Arc::clone(&entry),
            self.bus.clone(),
            session_id.to_string(),
            id.clone(),
            "stdout",
        );
        spawn_pump(
            child.stderr.take(),
            Arc::clone(&entry),
            self.bus.clone(),
            session_id.to_string(),
            id.clone(),
            "stderr",
        );

        let bus = self.bus.clone();
        let store = Arc::clone(&self.store);
        let session = session_id.to_string();
        let process_id = id.clone();
        tokio::spawn(async move {
            monitor(
                child, kill_rx, entry, bus, store, session, process_id, started,
            )
            .await;
        });

        Ok(info)
    }

    /// All processes, newest first, optionally limited to one session.
    pub fn list(&self, session_id: Option<&str>) -> Vec<ProcessInfo> {
        let entries = self.entries.lock().expect("process mutex poisoned");
        let mut infos: Vec<ProcessInfo> = entries
            .values()
            .filter(|entry| match session_id {
                Some(id) => {
                    entry
                        .info
                        .lock()
                        .expect("process mutex poisoned")
                        .session_id
                        == id
                }
                None => true,
            })
            .map(|entry| entry.info.lock().expect("process mutex poisoned").clone())
            .collect();
        infos.sort_by_key(|info| std::cmp::Reverse(info.started_ms));
        infos
    }

    /// Tail of a process named by full id or unique prefix.
    pub fn output(&self, needle: &str, max_bytes: usize) -> Result<ProcessOutput> {
        let entry = self.resolve(needle)?;
        Ok(entry_output(&entry, max_bytes))
    }

    /// Tail of a specific process, or of the newest one in the session.
    pub fn output_for_session(
        &self,
        session_id: &str,
        process_id: Option<&str>,
        max_bytes: usize,
    ) -> Result<ProcessOutput> {
        match process_id {
            Some(needle) => self.output(needle, max_bytes),
            None => {
                let entry = self.newest(session_id)?;
                Ok(entry_output(&entry, max_bytes))
            }
        }
    }

    /// Wait up to `wait` for the process to exit. Returns immediately when it
    /// is already finished.
    pub async fn wait_for_exit(
        &self,
        session_id: &str,
        needle: &str,
        wait: Duration,
    ) -> Result<()> {
        let entry = self.resolve_in(session_id, needle)?;
        let deadline = Instant::now() + wait;
        loop {
            if !entry
                .info
                .lock()
                .expect("process mutex poisoned")
                .status
                .is_running()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Write stdin and return the output produced since the previous poll.
    /// `chars` may be empty to poll without writing.
    ///
    /// A write returns as soon as the process answers (or `wait` elapses). An
    /// empty poll instead waits until the process exits or the progress-derived
    /// next-check time: waking the model for every progress frame wastes tokens
    /// without telling it anything new. `wait` remains a ceiling for callers
    /// that ask for a shorter yield.
    pub async fn write_stdin(
        &self,
        session_id: &str,
        needle: &str,
        chars: &str,
        wait: Duration,
        max_bytes: usize,
    ) -> Result<ProcessOutput> {
        let entry = self.resolve_in(session_id, needle)?;
        let wrote = !chars.is_empty();
        if wrote {
            let mut stdin = entry.stdin.lock().await;
            let Some(stdin) = stdin.as_mut() else {
                bail!("process stdin is closed");
            };
            stdin
                .write_all(chars.as_bytes())
                .await
                .context("cannot write process stdin")?;
            stdin.flush().await.context("cannot flush process stdin")?;
        }
        let wait = if wrote {
            wait
        } else {
            let suggested = entry
                .progress
                .lock()
                .expect("process mutex poisoned")
                .suggested_wait();
            wait.min(suggested)
        };
        let deadline = Instant::now() + wait;
        loop {
            let running = entry
                .info
                .lock()
                .expect("process mutex poisoned")
                .status
                .is_running();
            if !running || Instant::now() >= deadline {
                break;
            }
            if wrote
                && !entry
                    .pending
                    .lock()
                    .expect("process mutex poisoned")
                    .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let mut output = entry_pending(&entry, max_bytes);
        {
            let mut progress = entry.progress.lock().expect("process mutex poisoned");
            let next = progress.finish_poll(wrote);
            output.percent = progress.percent;
            output.eta_secs = progress
                .eta
                .filter(|_| output.status.is_running())
                .map(|eta| eta.as_secs().max(1));
            output.idle_secs = Some(progress.last_output.elapsed().as_secs());
            output.next_check_secs = Some(next.as_secs().max(1));
        }
        Ok(output)
    }

    /// Ask a process to stop. Returns `false` if it is not running.
    pub fn kill(&self, needle: &str) -> Result<bool> {
        let entry = self.resolve(needle)?;
        if !entry
            .info
            .lock()
            .expect("process mutex poisoned")
            .status
            .is_running()
        {
            return Ok(false);
        }
        let killer = entry.killer.lock().expect("process mutex poisoned").take();
        match killer {
            Some(killer) => {
                let _ = killer.send(());
                Ok(true)
            }
            None => Ok(false),
        }
    }
    /// Kill every running process; called on daemon shutdown.
    pub async fn kill_all(&self) {
        let entries: Vec<Arc<Entry>> = {
            let entries = self.entries.lock().expect("process mutex poisoned");
            entries.values().cloned().collect()
        };
        let mut killed = false;
        for entry in &entries {
            if entry
                .info
                .lock()
                .expect("process mutex poisoned")
                .status
                .is_running()
            {
                if let Some(killer) = entry.killer.lock().expect("process mutex poisoned").take() {
                    let _ = killer.send(());
                    killed = true;
                }
            }
        }
        if !killed {
            return;
        }
        let deadline = Instant::now() + KILL_GRACE + Duration::from_secs(1);
        while Instant::now() < deadline {
            let running = entries.iter().any(|entry| {
                entry
                    .info
                    .lock()
                    .expect("process mutex poisoned")
                    .status
                    .is_running()
            });
            if !running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn resolve(&self, needle: &str) -> Result<Arc<Entry>> {
        if needle.is_empty() {
            bail!("process id must not be empty");
        }
        let entries = self.entries.lock().expect("process mutex poisoned");
        if let Some(entry) = entries.get(needle) {
            return Ok(Arc::clone(entry));
        }
        let mut matches: Vec<Arc<Entry>> = entries
            .iter()
            .filter(|(id, _)| id.starts_with(needle))
            .map(|(_, entry)| Arc::clone(entry))
            .collect();
        match matches.len() {
            0 => bail!("unknown process {needle:?}"),
            1 => Ok(matches.pop().expect("checked len")),
            n => bail!("process id {needle:?} is ambiguous ({n} matches)"),
        }
    }

    /// Resolve an id and prove it belongs to `session_id`. A model cannot use
    /// `write_stdin` to drive another session's process.
    fn resolve_in(&self, session_id: &str, needle: &str) -> Result<Arc<Entry>> {
        let entry = self.resolve(needle)?;
        let owner = entry
            .info
            .lock()
            .expect("process mutex poisoned")
            .session_id
            .clone();
        if owner != session_id {
            bail!("unknown process {needle:?}");
        }
        Ok(entry)
    }

    fn newest(&self, session_id: &str) -> Result<Arc<Entry>> {
        let entries = self.entries.lock().expect("process mutex poisoned");
        entries
            .values()
            .filter(|entry| {
                entry
                    .info
                    .lock()
                    .expect("process mutex poisoned")
                    .session_id
                    == session_id
            })
            .max_by_key(|entry| {
                entry
                    .info
                    .lock()
                    .expect("process mutex poisoned")
                    .started_ms
            })
            .cloned()
            .with_context(|| format!("no processes in session {session_id:?}"))
    }

    /// Refuse a new long-running process once a session is holding too many.
    ///
    /// The message names the oldest ones because the model has no kill tool:
    /// it can stop a process with `kill <pid>` from a one-shot command, and a
    /// tty session also accepts Ctrl-C through `write_stdin`.
    fn check_running_cap(&self, session_id: &str) -> Result<()> {
        let mut running: Vec<(i64, String, u32, String)> = {
            let entries = self.entries.lock().expect("process mutex poisoned");
            entries
                .values()
                .filter_map(|entry| {
                    let info = entry.info.lock().expect("process mutex poisoned");
                    (info.status.is_running() && info.session_id == session_id).then(|| {
                        (
                            info.started_ms,
                            info.id.clone(),
                            info.pid,
                            info.command.clone(),
                        )
                    })
                })
                .collect()
        };
        if running.len() < MAX_RUNNING {
            return Ok(());
        }
        running.sort();
        let listing = running
            .iter()
            .take(5)
            .map(|(_, id, pid, command)| {
                format!("{id} (pid {pid}) `{}`", truncate_command(command, 60))
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "session already has {MAX_RUNNING} running background processes; stop one with \
             `kill <pid>`, send Ctrl-C to a tty session through `write_stdin`, or wait for it to \
             exit, then retry. Running: {listing}"
        )
    }

    /// Drop the oldest finished processes once a session accumulates many.
    fn prune_finished(&self, session_id: &str) {
        let mut entries = self.entries.lock().expect("process mutex poisoned");
        let mut finished: Vec<(i64, String)> = entries
            .values()
            .filter_map(|entry| {
                let info = entry.info.lock().expect("process mutex poisoned");
                (!info.status.is_running() && info.session_id == session_id)
                    .then(|| (info.started_ms, info.id.clone()))
            })
            .collect();
        if finished.len() <= KEEP_FINISHED {
            return;
        }
        finished.sort();
        for (_, id) in finished.iter().take(finished.len() - KEEP_FINISHED) {
            entries.remove(id);
        }
    }
}

/// Shorten a command for an error message without splitting a character.
fn truncate_command(command: &str, max_chars: usize) -> String {
    let mut out: String = command.chars().take(max_chars).collect();
    if command.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// Spawn a pipe reader, if the pipe exists.
fn spawn_pump<R>(
    reader: Option<R>,
    entry: Arc<Entry>,
    bus: broadcast::Sender<Notification>,
    session_id: String,
    process_id: String,
    stream: &'static str,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    if let Some(reader) = reader {
        tokio::spawn(pump(reader, entry, bus, session_id, process_id, stream));
    }
}

/// Read one pipe into the log file, the tail, and the notification bus.
async fn pump<R: AsyncRead + Unpin>(
    mut reader: R,
    entry: Arc<Entry>,
    bus: broadcast::Sender<Notification>,
    session_id: String,
    process_id: String,
    stream: &'static str,
) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
        let bytes = {
            let mut info = entry.info.lock().expect("process mutex poisoned");
            info.bytes += n as u64;
            if info.bytes > LOG_CAP_BYTES {
                info.truncated = true;
            }
            info.bytes
        };
        {
            let mut tail = entry.tail.lock().expect("process mutex poisoned");
            tail.push_str(&chunk);
            trim_front(&mut tail, TAIL_BYTES);
        }
        {
            let mut pending = entry.pending.lock().expect("process mutex poisoned");
            pending.push_str(&chunk);
            trim_front(&mut pending, TAIL_BYTES);
        }
        {
            let now = Instant::now();
            let mut progress = entry.progress.lock().expect("process mutex poisoned");
            progress.last_output = now;
            if let Some(percent) = last_percent(&chunk) {
                progress.observe(percent, now);
            }
        }
        if bytes <= LOG_CAP_BYTES {
            let mut file = entry.log_file.lock().await;
            let _ = file.write_all(chunk.as_bytes()).await;
        }
        if bytes <= STREAM_CAP_BYTES {
            let _ = bus.send(Notification::new(
                "process.output",
                json!({
                    "session_id": session_id,
                    "process_id": process_id,
                    "stream": stream,
                    "chunk": chunk,
                }),
            ));
        }
    }
}

/// Wait for the process to exit (or a kill request) and record the outcome.
#[allow(clippy::too_many_arguments)]
async fn monitor(
    mut child: tokio::process::Child,
    kill_rx: oneshot::Receiver<()>,
    entry: Arc<Entry>,
    bus: broadcast::Sender<Notification>,
    store: Arc<Store>,
    session_id: String,
    process_id: String,
    started: Instant,
) {
    let killed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&killed);
    let pid = entry.info.lock().expect("process mutex poisoned").pid;
    let status = tokio::select! {
        status = child.wait() => status,
        _ = kill_rx => {
            flag.store(true, Ordering::SeqCst);
            signal(pid, libc::SIGTERM);
            match tokio::time::timeout(KILL_GRACE, child.wait()).await {
                Ok(status) => status,
                Err(_) => {
                    signal(pid, libc::SIGKILL);
                    // Fallback for platforms without process groups (and for a
                    // signal that raced with the group teardown).
                    let _ = child.start_kill();
                    child.wait().await
                }
            }
        }
    };

    // Let the pipe pumps drain whatever the OS still has buffered.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let duration_ms = started.elapsed().as_millis() as u64;
    let (status_kind, exit_code) = match status {
        Ok(status) if killed.load(Ordering::SeqCst) => (ProcessStatus::Killed, status.code()),
        Ok(status) => (ProcessStatus::Exited, status.code()),
        Err(_) => (ProcessStatus::Failed, None),
    };
    {
        let mut info = entry.info.lock().expect("process mutex poisoned");
        info.status = status_kind;
        info.exit_code = exit_code;
        info.ended_ms = Some(now_ms());
    }
    entry.killer.lock().expect("process mutex poisoned").take();

    let exit = ProcessExitedPayload {
        process_id: process_id.clone(),
        status: status_kind.as_str().to_string(),
        exit_code,
        duration_ms,
    };
    if let Ok(event) = store.append_event(&session_id, EventKind::ProcessExited, payload(&exit)) {
        let _ = bus.send(Notification::new("session.event", event));
    }
    let _ = bus.send(Notification::new(
        "process.exited",
        json!({
            "session_id": session_id,
            "process_id": process_id,
            "status": status_kind.as_str(),
            "exit_code": exit_code,
            "duration_ms": duration_ms,
        }),
    ));
}

/// Keep at most `cap` bytes, cutting on a UTF-8 boundary.
fn trim_front(text: &mut String, cap: usize) {
    if text.len() <= cap {
        return;
    }
    let mut cut = text.len() - cap;
    while cut < text.len() && !text.is_char_boundary(cut) {
        cut += 1;
    }
    *text = text.split_off(cut);
}

/// Latest percentage in one output chunk. Progress bars usually separate frames
/// with `\r`, and the last match is the current state.
fn last_percent(chunk: &str) -> Option<f64> {
    static PERCENT: OnceLock<regex::Regex> = OnceLock::new();
    let re = PERCENT.get_or_init(|| {
        regex::Regex::new(r"(?:^|[^0-9.])([0-9]{1,3}(?:\.[0-9]+)?)\s*%")
            .expect("percent regex is valid")
    });
    re.captures_iter(chunk)
        .filter_map(|cap| cap.get(1)?.as_str().parse::<f64>().ok())
        .last()
}

/// Drain the output produced since the previous poll.
fn entry_pending(entry: &Arc<Entry>, max_bytes: usize) -> ProcessOutput {
    let info = entry.info.lock().expect("process mutex poisoned").clone();
    let pending = {
        let mut pending = entry.pending.lock().expect("process mutex poisoned");
        std::mem::take(&mut *pending)
    };
    let mut text = pending;
    let mut truncated = info.truncated;
    if text.len() > max_bytes {
        trimmed_tail(&mut text, max_bytes);
        truncated = true;
    }
    ProcessOutput {
        process_id: info.id.clone(),
        truncated,
        text,
        bytes: info.bytes,
        status: info.status,
        exit_code: info.exit_code,
        percent: None,
        eta_secs: None,
        next_check_secs: None,
        idle_secs: None,
    }
}

fn trimmed_tail(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut cut = text.len() - max_bytes;
    while cut < text.len() && !text.is_char_boundary(cut) {
        cut += 1;
    }
    *text = text.split_off(cut);
}

fn entry_output(entry: &Arc<Entry>, max_bytes: usize) -> ProcessOutput {
    let info = entry.info.lock().expect("process mutex poisoned").clone();
    let tail = entry.tail.lock().expect("process mutex poisoned").clone();
    let mut cut = tail.len().saturating_sub(max_bytes);
    while cut < tail.len() && !tail.is_char_boundary(cut) {
        cut += 1;
    }
    let text = tail[cut..].to_string();
    ProcessOutput {
        process_id: info.id.clone(),
        truncated: cut > 0 || info.truncated,
        text,
        bytes: info.bytes,
        status: info.status,
        exit_code: info.exit_code,
        percent: None,
        eta_secs: None,
        next_check_secs: None,
        idle_secs: None,
    }
}

/// Signal the process group led by `pid`, so children go down with the shell.
fn signal(pid: u32, sig: libc::c_int) {
    #[cfg(unix)]
    if pid > 0 {
        // SAFETY: `kill` takes an integer pid and signal, both valid here.
        unsafe {
            libc::kill(-(pid as libc::pid_t), sig);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::command_plan;
    use eix_core::{Config, EventKind, SandboxMode, Session};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Temp {
        base: PathBuf,
        store: Arc<Store>,
        registry: Arc<ProcessRegistry>,
        _bus: broadcast::Sender<Notification>,
        config: Config,
    }

    impl Temp {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let base = std::env::temp_dir().join(format!(
                "eix-process-{label}-{}-{nanos}",
                std::process::id()
            ));
            let store = Arc::new(Store::in_memory().unwrap());
            store
                .create_session(&Session::new("s1", "test", base.clone()))
                .unwrap();
            let (bus, _) = broadcast::channel(64);
            let registry = Arc::new(ProcessRegistry::new(
                Arc::clone(&store),
                bus.clone(),
                base.clone(),
            ));
            let mut config = Config::default();
            config.tools.sandbox = SandboxMode::Off;
            Self {
                base,
                store,
                registry,
                _bus: bus,
                config,
            }
        }

        async fn start(&self, session: &str, command: &str) -> ProcessInfo {
            let plan =
                command_plan(command, None, &std::env::temp_dir(), None, &self.config).unwrap();
            self.registry
                .start(session, command, plan)
                .await
                .expect("start process")
        }

        async fn wait_done(&self, id: &str) -> ProcessOutput {
            for _ in 0..150 {
                let output = self.registry.output(id, 64 * 1024).unwrap();
                if !output.status.is_running() {
                    return output;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("process {id} did not finish");
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[tokio::test]
    async fn captures_output_and_records_start_and_exit() {
        let temp = Temp::new("echo");
        let info = temp.start("s1", "echo hello").await;
        assert_eq!(info.status, ProcessStatus::Running);
        assert!(info.log.ends_with(".log"));

        let output = temp.wait_done(&info.id).await;
        assert_eq!(output.status, ProcessStatus::Exited);
        assert_eq!(output.exit_code, Some(0));
        assert!(output.text.contains("hello"), "{:?}", output.text);

        let events = temp.store.events("s1").unwrap();
        let started = events
            .iter()
            .position(|event| event.kind == EventKind::ProcessStarted)
            .expect("started event");
        let exited = events
            .iter()
            .position(|event| event.kind == EventKind::ProcessExited)
            .expect("exited event");
        assert!(started < exited);
    }

    #[tokio::test]
    async fn kill_marks_a_running_process() {
        let temp = Temp::new("kill");
        let info = temp.start("s1", "sleep 30").await;
        assert!(temp.registry.kill(&info.id).unwrap());
        let output = temp.wait_done(&info.id).await;
        assert_eq!(output.status, ProcessStatus::Killed);
        assert!(!temp.registry.kill(&info.id).unwrap());
    }

    #[tokio::test]
    async fn output_tail_is_capped_and_ids_resolve_by_prefix() {
        let temp = Temp::new("tail");
        let info = temp.start("s1", "printf 'abcdefghij'").await;
        temp.wait_done(&info.id).await;

        let output = temp
            .registry
            .output(&info.id[..8], 4)
            .expect("prefix lookup");
        assert_eq!(output.process_id, info.id);
        assert_eq!(output.text, "ghij");
        assert!(output.truncated);
        assert_eq!(output.bytes, 10);
    }

    #[tokio::test]
    async fn lists_processes_per_session() {
        let temp = Temp::new("list");
        temp.store
            .create_session(&Session::new("s2", "other", temp.base.clone()))
            .unwrap();
        let first = temp.start("s1", "echo one").await;
        temp.wait_done(&first.id).await;
        let second = temp.start("s2", "echo two").await;

        let all = temp.registry.list(None);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, second.id, "newest first");
        let only_s1 = temp.registry.list(Some("s1"));
        assert_eq!(only_s1.len(), 1);
        assert_eq!(only_s1[0].id, first.id);
        temp.registry.kill_all().await;
    }

    #[tokio::test]
    async fn unknown_and_empty_ids_are_errors() {
        let temp = Temp::new("ids");
        let error = temp.registry.output("nope", 100).unwrap_err();
        assert!(format!("{error:#}").contains("unknown process"));
        let error = temp.registry.output("", 100).unwrap_err();
        assert!(format!("{error:#}").contains("must not be empty"));
    }

    #[test]
    fn keeps_only_the_latest_percentage_in_a_chunk() {
        assert_eq!(last_percent("10%\r 55%\r 90%\r"), Some(90.0));
        assert_eq!(last_percent("no progress here"), None);
        assert_eq!(last_percent("transfer 12.5%"), Some(12.5));
    }

    #[test]
    fn estimates_remaining_time_from_the_progress_slope() {
        let now = Instant::now();
        let start = now - Duration::from_secs(10);
        let mut progress = Progress::new(start);
        progress.last_output = now;
        progress.observe(20.0, start + Duration::from_secs(2));
        progress.observe(30.0, start + Duration::from_secs(4));

        let eta = progress.eta.expect("eta");
        assert!((12..=16).contains(&eta.as_secs()), "{eta:?}");
        let wait = progress.suggested_wait();
        assert!(
            (12..=16).contains(&wait.as_secs()),
            "wait should follow the ETA: {wait:?}"
        );
    }

    #[test]
    fn a_backwards_jump_discards_the_stale_eta() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        progress.observe(20.0, now);
        progress.observe(80.0, now + Duration::from_secs(2));
        assert!(progress.eta.is_some());
        progress.observe(5.0, now + Duration::from_secs(3));
        assert!(
            progress.eta.is_none(),
            "a new phase must not inherit an ETA"
        );
    }

    #[test]
    fn empty_poll_backoff_grows_and_is_bounded() {
        let mut progress = Progress::new(Instant::now());
        assert_eq!(progress.poll_interval, MIN_POLL_INTERVAL);
        progress.finish_poll(false);
        assert_eq!(progress.poll_interval, Duration::from_secs(4));
        for _ in 0..8 {
            progress.finish_poll(false);
        }
        assert_eq!(progress.poll_interval, MAX_POLL_INTERVAL);
    }

    #[tokio::test]
    async fn empty_poll_waits_instead_of_returning_on_first_output() {
        let temp = Temp::new("poll");
        let info = temp.start("s1", "echo early; sleep 0.8; echo late").await;
        let started = Instant::now();
        let output = temp
            .registry
            .write_stdin("s1", &info.id, "", Duration::from_secs(30), 64 * 1024)
            .await
            .expect("poll");
        // The first line is already pending, but an empty poll must wait for
        // exit or the next check rather than hand it back immediately.
        assert!(
            started.elapsed() >= Duration::from_millis(500),
            "poll returned too early"
        );
        assert!(!output.status.is_running());
        assert!(output.text.contains("early"), "{:?}", output.text);
        assert!(output.text.contains("late"), "{:?}", output.text);
        assert!(output.next_check_secs.is_some());
    }
}
