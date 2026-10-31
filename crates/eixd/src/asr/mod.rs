//! Speech-to-text sessions owned by the daemon.
//!
//! Audio arrives in PCM chunks, either as base64 in `asr.pcm` or as the body
//! of `POST /asr/{utterance}/pcm`. Recognition runs in a background task, so a
//! slow engine never blocks the RPC loop, and results are published on the
//! existing event bus as `asr.partial`, `asr.final` and `asr.error`.
//!
//! Three engines share the pipeline:
//!
//! - `fake`: deterministic output for tests and frontend work.
//! - `local`: an external helper process (`[voice.local] command`) speaking
//!   newline-delimited JSON. Keeping ONNX/whisper out of this binary is what
//!   lets the default build stay free of C++ toolchains.
//! - `cloud`: one HTTPS POST per utterance against a streaming-transcription
//!   REST endpoint, which both Deepgram and OpenAI-compatible gateways expose.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use eix_core::asr::{
    AsrError, AsrErrorKind, AsrEvent, BackendKind, Caps, CloudProvider, PcmFormat, Support,
    VoiceConfig, Word, MAX_UTTERANCE_SECS, SAMPLE_RATE,
};
use eix_core::config::ModelRoute;
use eix_core::ChatMessage;
use eix_core::Notification;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc, Mutex};

use crate::provider::Provider;

/// Audio queued ahead of the engine. 32 chunks of 100 ms covers a hiccup of
/// about three seconds before audio has to be dropped.
const QUEUE: usize = 32;
/// Largest accepted single upload. Larger bodies are rejected before they are
/// buffered, since one utterance is capped at five minutes of 16 kHz PCM.
pub const MAX_CHUNK_BYTES: usize = 1024 * 1024;
/// Hard cap on the audio kept for one utterance.
const MAX_UTTERANCE_BYTES: usize = SAMPLE_RATE as usize * 2 * MAX_UTTERANCE_SECS as usize;

const CLOUD_TIMEOUT: Duration = Duration::from_secs(60);
const LOCAL_FINAL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct StartInfo {
    pub utterance_id: String,
    pub format: PcmFormat,
    pub caps: Caps,
    pub backend: BackendKind,
}

struct Handle {
    session_id: Option<String>,
    tx: mpsc::Sender<Cmd>,
    bytes: Arc<AtomicU64>,
    /// Bytes dropped because the engine queue was full. The engine drains it
    /// on its next loop iteration and reports one `asr.overrun`.
    pending_drop: Arc<AtomicU64>,
}

enum Cmd {
    Audio(Vec<u8>),
    Finish,
    Cancel,
}

/// Sessions are keyed by utterance id, not session id: one conversation can
/// hold a new utterance before the previous one's final arrives.
pub struct Manager {
    bus: broadcast::Sender<Notification>,
    client: reqwest::Client,
    sessions: Mutex<HashMap<String, Handle>>,
}

/// Optional second pass over a finished transcript by a chat model.
///
/// Small ASR models mishear identifiers and drop punctuation, so the raw text
/// is sent through the configured chat model before it reaches the client.
/// Every failure keeps the raw transcript: dictation must never lose words to
/// a model that is slow, down, or answers the text instead of cleaning it.
#[derive(Clone)]
pub struct Refiner {
    provider: Arc<Provider>,
    route: ModelRoute,
    timeout: Duration,
    prompt: String,
}

impl Refiner {
    pub fn new(
        provider: Arc<Provider>,
        route: ModelRoute,
        config: &eix_core::asr::RefineConfig,
    ) -> Self {
        Self {
            provider,
            route,
            timeout: Duration::from_millis(u64::from(config.timeout_ms.clamp(500, 60_000))),
            prompt: config.prompt.clone(),
        }
    }

    async fn rewrite(&self, text: &str) -> Rewrite {
        let messages = refine_messages(text, &self.prompt);
        let cancel = AtomicBool::new(false);
        let completion = tokio::time::timeout(
            self.timeout,
            self.provider
                .stream_chat(&messages, &self.route, &[], &cancel, |_| {}, |_| {}),
        )
        .await;
        let Ok(Ok(completion)) = completion else {
            return Rewrite::Failed;
        };
        clean_refined(text, &completion.text)
    }
}

/// Outcome of one rewrite pass over a finished transcript.
#[derive(Debug, PartialEq, Eq)]
enum Rewrite {
    /// The model returned edited text to publish instead of the raw transcript.
    Edited(String),
    /// The model judged the transcript already correct. Not a failure.
    Unchanged,
    /// Timeout, provider error, or output unusable as an edit.
    Failed,
}

fn refine_messages(raw: &str, extra: &str) -> Vec<ChatMessage> {
    let mut system = String::from(
        "You edit raw speech-to-text into what the speaker meant to type into a coding agent's input box.\n\
Return only the edited transcript: no explanation, no quotes, no code fences.\n\
\n\
Accuracy:\n\
- Fix misheard words using the surrounding context: identifiers, file paths, CLI flags, product and API names.\n\
- Restore punctuation, capitalisation and spacing. Start a new paragraph only when the topic changes.\n\
- Keep the speaker's language. Never translate.\n\
\n\
Fluency:\n\
- Remove fillers (um, uh, er, like, you know, 嗯, 呃, 那个, 就是) and stutters.\n\
- Drop false starts the speaker abandoned mid-word, and collapse repetitions caused by a pause or by a\n\
  recogniser loop (\"the the config config\") into one copy.\n\
- Join fragments left by a broken train of thought into complete sentences, adding the connectives the\n\
  speaker left out (and, then, but, so), so each point reads as one coherent thought.\n\
\n\
Organisation:\n\
- Group points that belong together, and split unrelated topics into separate paragraphs.\n\
- When the speaker said the qualification before the request, put the main request first and the\n\
  qualification after it. Drop or reword nothing to do so.\n\
- Prefer reordering to rewriting: the speaker's own words are the safest carrier of intent.\n\
\n\
Meaning is fixed:\n\
- Do not answer questions, add facts, drop requests, weaken instructions, or invent detail.\n\
- Do not summarise: every distinct request and constraint must survive. Stay close to the original length.\n\
- When a word is ambiguous and the context does not resolve it, keep what the speaker said.\n\
- If nothing needs fixing, return the text unchanged.",
    );
    if !extra.trim().is_empty() {
        system.push_str("\n\nAdditional instructions:\n");
        system.push_str(extra.trim());
    }
    vec![ChatMessage::system(system), ChatMessage::user(raw)]
}

/// Accept a rewrite only when it plausibly is one: output that is empty, long
/// enough to be a model replying to the transcript, or short enough to be a
/// summary of it is a failure, and unchanged text is a pass with no edit.
fn clean_refined(raw: &str, output: &str) -> Rewrite {
    let mut text = output.trim();
    if let Some(rest) = text.strip_prefix("```") {
        // Drop an optional language tag and the closing fence.
        let rest = rest.split_once('\n').map(|(_, body)| body).unwrap_or(rest);
        text = rest.trim_end_matches("```").trim();
    }
    let text = text.trim().trim_matches('"').trim();
    let raw_len = raw.trim().chars().count();
    let edited_len = text.chars().count();
    if text.is_empty() {
        return Rewrite::Failed;
    }
    if text == raw.trim() {
        return Rewrite::Unchanged;
    }
    if edited_len > raw_len * 3 + 120 {
        return Rewrite::Failed;
    }
    // Tidying invites summarising, so a rewrite that keeps under a quarter of a
    // long transcript is treated as a summary and rejected. Short inputs are
    // exempt: a stutter-heavy fragment can legitimately shrink that far.
    if raw_len >= 80 && edited_len * 4 < raw_len {
        return Rewrite::Failed;
    }
    Rewrite::Edited(text.to_string())
}

/// Clean the finished transcript, returning the text to publish and whether the
/// rewrite failed. A failure keeps the raw text and is reported as `degraded`,
/// so a client can tell "heard correctly" from "heard correctly, not edited".
async fn finalize(refiner: Option<&Refiner>, text: String) -> (String, bool) {
    let Some(refiner) = refiner else {
        return (text, false);
    };
    if text.trim().is_empty() {
        return (text, false);
    }
    match refiner.rewrite(&text).await {
        Rewrite::Edited(edited) => (edited, false),
        Rewrite::Unchanged => (text, false),
        Rewrite::Failed => (text, true),
    }
}

impl Manager {
    pub fn new(bus: broadcast::Sender<Notification>) -> Self {
        Self {
            bus,
            client: reqwest::Client::builder()
                .timeout(CLOUD_TIMEOUT)
                .build()
                .unwrap_or_default(),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Open a new utterance. `api_key` is only read for the cloud backend.
    pub async fn start(
        &self,
        config: &VoiceConfig,
        session_id: Option<String>,
        api_key: Option<String>,
        refiner: Option<Refiner>,
    ) -> Result<StartInfo, AsrError> {
        config.validate()?;
        let caps = caps_for(config.backend);
        let emitter = Emitter {
            bus: self.bus.clone(),
            utterance_id: String::new(),
            session_id: session_id.clone(),
        };
        let engine = match config.backend {
            BackendKind::Fake => Engine::Fake,
            BackendKind::Cloud => Engine::Cloud(CloudConfig::from_voice(config, api_key)?),
            BackendKind::Local => Engine::Local(LocalConfig::from_voice(config)?),
        };

        let (tx, rx) = mpsc::channel(QUEUE);
        let bytes = Arc::new(AtomicU64::new(0));
        let pending_drop = Arc::new(AtomicU64::new(0));
        let utterance_id = uuid::Uuid::new_v4().to_string();
        let emitter = Emitter {
            utterance_id: utterance_id.clone(),
            ..emitter
        };
        let client = self.client.clone();
        let drops = Arc::clone(&pending_drop);
        tokio::spawn(async move {
            drive(engine, rx, emitter, caps, client, drops, refiner).await;
        });
        self.sessions.lock().await.insert(
            utterance_id.clone(),
            Handle {
                session_id,
                tx,
                bytes,
                pending_drop,
            },
        );
        Ok(StartInfo {
            utterance_id,
            format: PcmFormat::CANONICAL,
            caps,
            backend: config.backend,
        })
    }

    /// Queue one PCM chunk. Returns the utterance's total size afterwards.
    pub async fn push(&self, utterance_id: &str, pcm: Vec<u8>) -> Result<u64, AsrError> {
        if pcm.is_empty() {
            return Err(AsrError::format("audio chunk is empty"));
        }
        if pcm.len() > MAX_CHUNK_BYTES {
            return Err(AsrError::format(format!(
                "audio chunk exceeds {MAX_CHUNK_BYTES} bytes"
            )));
        }
        if !pcm.len().is_multiple_of(2) {
            return Err(AsrError::format(
                "PCM16 audio must contain an even number of bytes",
            ));
        }
        let (tx, bytes, pending_drop, total) = {
            let sessions = self.sessions.lock().await;
            let handle = sessions
                .get(utterance_id)
                .ok_or_else(|| AsrError::new(AsrErrorKind::Internal, "unknown utterance"))?;
            let total = handle.bytes.load(Ordering::Relaxed) + pcm.len() as u64;
            (
                handle.tx.clone(),
                Arc::clone(&handle.bytes),
                Arc::clone(&handle.pending_drop),
                total,
            )
        };
        if total as usize > MAX_UTTERANCE_BYTES {
            let _ = tx.send(Cmd::Finish).await;
            return Err(AsrError::new(
                AsrErrorKind::Format,
                format!("utterance exceeds {MAX_UTTERANCE_SECS} seconds"),
            ));
        }
        bytes.fetch_add(pcm.len() as u64, Ordering::Relaxed);
        // A full queue means the engine is behind. Dropping the chunk keeps
        // the socket reader responsive; the UI is told audio was lost.
        match tx.try_send(Cmd::Audio(pcm)) {
            Ok(()) => Ok(total),
            Err(mpsc::error::TrySendError::Full(chunk)) => {
                let dropped = match &chunk {
                    Cmd::Audio(pcm) => pcm.len(),
                    _ => 0,
                };
                pending_drop.fetch_add(dropped as u64, Ordering::Relaxed);
                Ok(total)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(AsrError::new(
                AsrErrorKind::Internal,
                "utterance already finished",
            )),
        }
    }

    pub async fn finish(&self, utterance_id: &str) -> Result<(), AsrError> {
        let tx = self.sender(utterance_id).await?;
        tx.send(Cmd::Finish)
            .await
            .map_err(|_| AsrError::new(AsrErrorKind::Internal, "utterance already finished"))
    }

    pub async fn cancel(&self, utterance_id: &str) -> Result<(), AsrError> {
        let tx = self.sender(utterance_id).await?;
        let _ = tx.send(Cmd::Cancel).await;
        Ok(())
    }

    /// Drop every utterance of a session, used when the session is deleted or
    /// the client disappears mid-sentence.
    pub async fn cancel_session(&self, session_id: &str) {
        let handles: Vec<mpsc::Sender<Cmd>> = {
            let sessions = self.sessions.lock().await;
            sessions
                .values()
                .filter(|handle| handle.session_id.as_deref() == Some(session_id))
                .map(|handle| handle.tx.clone())
                .collect()
        };
        for tx in handles {
            let _ = tx.send(Cmd::Cancel).await;
        }
    }

    pub async fn status(&self, utterance_id: &str) -> Option<Value> {
        let sessions = self.sessions.lock().await;
        sessions.get(utterance_id).map(|handle| {
            json!({
                "utterance_id": utterance_id,
                "session_id": handle.session_id,
                "bytes": handle.bytes.load(Ordering::Relaxed),
            })
        })
    }

    async fn sender(&self, utterance_id: &str) -> Result<mpsc::Sender<Cmd>, AsrError> {
        self.sessions
            .lock()
            .await
            .get(utterance_id)
            .map(|handle| handle.tx.clone())
            .ok_or_else(|| AsrError::new(AsrErrorKind::Internal, "unknown utterance"))
    }
}

fn caps_for(backend: BackendKind) -> Caps {
    match backend {
        BackendKind::Fake => Caps {
            partials: false,
            stable_prefix: false,
            punctuation: false,
            diarization: Support::No,
            ..Caps::default()
        },
        BackendKind::Local => Caps {
            partials: true,
            stable_prefix: true,
            word_timestamps: false,
            punctuation: true,
            diarization: Support::No,
            max_stream_secs: None,
        },
        BackendKind::Cloud => Caps {
            partials: false,
            stable_prefix: false,
            word_timestamps: true,
            punctuation: true,
            diarization: Support::FinalOnly,
            max_stream_secs: None,
        },
    }
}

#[derive(Clone)]
struct Emitter {
    bus: broadcast::Sender<Notification>,
    utterance_id: String,
    session_id: Option<String>,
}

impl Emitter {
    fn send(&self, method: &str, mut params: Value) {
        params["utterance_id"] = json!(self.utterance_id);
        if let Some(session_id) = &self.session_id {
            params["session_id"] = json!(session_id);
        }
        let _ = self.bus.send(Notification::from_value(method, params));
    }

    fn partial(&self, seq: u64, text: String, stable_len: usize) {
        self.send(
            "asr.partial",
            json!({ "seq": seq, "text": text, "stable_len": stable_len }),
        );
    }

    fn final_(&self, seq: u64, text: String, words: Vec<Word>, interim: bool, degraded: bool) {
        self.send(
            "asr.final",
            serde_json::to_value(AsrEvent::Final {
                seq,
                text,
                words,
                speaker: None,
                degraded,
                interim,
            })
            .unwrap_or(Value::Null),
        );
    }

    fn error(&self, seq: u64, err: AsrError) {
        let retryable = err.retryable();
        self.send(
            "asr.error",
            serde_json::to_value(AsrEvent::Error {
                seq,
                kind: err.kind,
                message: err.message,
                retryable,
            })
            .unwrap_or(Value::Null),
        );
    }

    fn overrun(&self, seq: u64, dropped_bytes: usize) {
        self.send(
            "asr.overrun",
            json!({ "seq": seq, "dropped_ms": bytes_to_ms(dropped_bytes) }),
        );
    }
}

fn bytes_to_ms(bytes: usize) -> u32 {
    (bytes as u64 * 1000 / (SAMPLE_RATE as u64 * 2)) as u32
}

enum Engine {
    Fake,
    Local(LocalConfig),
    Cloud(CloudConfig),
}

struct LocalConfig {
    command: std::path::PathBuf,
    args: Vec<String>,
}

impl LocalConfig {
    fn from_voice(config: &VoiceConfig) -> Result<Self, AsrError> {
        let command = match config.local.command.clone() {
            Some(command) => command,
            None => discover_helper()?,
        };
        if !command.exists() {
            return Err(AsrError::unavailable(format!(
                "local asr helper {} does not exist",
                command.display()
            )));
        }
        let mut args = config.local.args.clone();
        let engine = if config.local.engine.trim().is_empty() {
            // Auto: SenseVoice when it is installed, because it is far better
            // on Chinese than a whisper model small enough to dictate with.
            match sense_voice_model() {
                Some(_) => "sherpa",
                None => "whisper",
            }
        } else {
            config.local.engine.trim()
        };
        if args.is_empty() {
            args.push("--engine".into());
            args.push(engine.to_string());
        }
        // Independent of `--engine`: a model is useful with or without it, and
        // silently dropping it would leave the engine without a model file.
        if !args.iter().any(|arg| arg == "--model") {
            let model = config.local.model_dir.clone().or_else(|| match engine {
                "sherpa" | "sherpa-onnx" => sense_voice_model(),
                _ => discover_model(),
            });
            if let Some(model) = model {
                args.push("--model".into());
                args.push(model.display().to_string());
            }
        }
        if !config.language.trim().is_empty() {
            args.push("--language".into());
            args.push(config.language.trim().to_string());
        }
        Ok(Self { command, args })
    }
}

/// Locate the local helper the way a shipped install lays it out: `PATH` first
/// (covers `cargo install` and distro packages), then next to the daemon (a
/// build tree), then the per-user bin directory.
fn discover_helper() -> Result<std::path::PathBuf, AsrError> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            candidates.push(dir.join("eix-asr"));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("eix-asr"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(std::path::PathBuf::from(home).join(".local/bin/eix-asr"));
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            AsrError::unavailable(
                "no eix-asr helper found: build it with `cargo build -p eix-asr` \
                 and put it on PATH, or set [voice.local] command",
            )
        })
}

/// Best installed model when none is configured: the largest `ggml-*.bin` in
/// `<data-dir>/models`. Larger whisper models are more accurate, so "largest"
/// is the right guess without asking the user to name one.
fn discover_model() -> Option<std::path::PathBuf> {
    let dir = eix_core::config::data_dir().join("models");
    let mut best: Option<(u64, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "bin") {
            let size = entry.metadata().ok().map(|meta| meta.len()).unwrap_or(0);
            if best.as_ref().is_none_or(|(largest, _)| size > *largest) {
                best = Some((size, path));
            }
        }
    }
    best.map(|(_, path)| path)
}

/// The SenseVoice model, if one is installed. Its `tokens.txt` sits next to it,
/// which is the layout the published archive uses.
///
/// A truncated download leaves a file that exists but cannot load, so require
/// a plausible size and the vocabulary beside it. Missing either means "not
/// installed" and the engine falls back to whisper instead of failing at the
/// first utterance.
fn sense_voice_model() -> Option<std::path::PathBuf> {
    let dir = eix_core::config::data_dir().join("models/sense-voice");
    const MIN_BYTES: u64 = 10 * 1024 * 1024;
    if !dir.join("tokens.txt").is_file() {
        return None;
    }
    for name in ["model.int8.onnx", "model.onnx"] {
        let path = dir.join(name);
        if path
            .metadata()
            .is_ok_and(|meta| meta.is_file() && meta.len() >= MIN_BYTES)
        {
            return Some(path);
        }
    }
    None
}

struct CloudConfig {
    url: String,
    api_key: String,
    provider: CloudProvider,
}

impl CloudConfig {
    /// Deepgram expects `Token <key>`; OpenAI-style gateways expect `Bearer`.
    fn authorization(&self) -> String {
        if self.provider == CloudProvider::Deepgram {
            format!("Token {}", self.api_key)
        } else {
            format!("Bearer {}", self.api_key)
        }
    }

    fn from_voice(config: &VoiceConfig, api_key: Option<String>) -> Result<Self, AsrError> {
        let provider = config.cloud.provider;
        let api_key = api_key
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
            .ok_or_else(|| {
                let slot = config
                    .cloud
                    .credential
                    .clone()
                    .unwrap_or_else(|| provider.credential_key().to_string());
                AsrError::unavailable(format!(
                    "no API key for {slot}; run `eix auth login {slot}`"
                ))
            })?;
        let mut url = config
            .cloud
            .endpoint
            .clone()
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| provider.default_endpoint().to_string());
        if url.is_empty() {
            return Err(AsrError::unavailable(
                "cloud backend needs [voice.cloud] endpoint",
            ));
        }
        // Providers document a WebSocket listen endpoint; the same path also serves
        // one POST per utterance, which is what this adapter uses.
        if let Some(rest) = url.strip_prefix("wss://") {
            url = format!("https://{rest}");
        } else if let Some(rest) = url.strip_prefix("ws://") {
            url = format!("http://{rest}");
        }
        // Ask for the canonical PCM layout explicitly; the endpoint otherwise
        // assumes 8 kHz mu-law for telephony clients.
        let query =
            format!("encoding=linear16&sample_rate={SAMPLE_RATE}&channels=1&punctuate=true",);
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&query);
        if !config.cloud.model.trim().is_empty() {
            url.push_str("&model=");
            url.push_str(config.cloud.model.trim());
        }
        if !config.language.trim().is_empty() {
            url.push_str("&language=");
            url.push_str(config.language.trim());
        }
        Ok(Self {
            url,
            api_key,
            provider,
        })
    }
}

/// Consume the command queue until the utterance finishes or is cancelled.
async fn drive(
    engine: Engine,
    rx: mpsc::Receiver<Cmd>,
    emitter: Emitter,
    caps: Caps,
    client: reqwest::Client,
    drops: Arc<AtomicU64>,
    refiner: Option<Refiner>,
) {
    match engine {
        Engine::Fake => run_fake(rx, emitter, drops, refiner).await,
        Engine::Local(config) => run_local(config, rx, emitter, caps, drops, refiner).await,
        Engine::Cloud(config) => run_cloud(config, rx, emitter, client, drops, refiner).await,
    }
}

/// Bytes lost to a full queue since the last call.
fn take_dropped(drops: &AtomicU64) -> usize {
    drops.swap(0, Ordering::Relaxed) as usize
}

async fn run_fake(
    mut rx: mpsc::Receiver<Cmd>,
    emitter: Emitter,
    drops: Arc<AtomicU64>,
    refiner: Option<Refiner>,
) {
    let mut seq = 0;
    let mut total = 0usize;
    while let Some(command) = rx.recv().await {
        match command {
            Cmd::Audio(pcm) => {
                total += pcm.len();
                let dropped = take_dropped(&drops);
                if dropped > 0 {
                    seq += 1;
                    emitter.overrun(seq, dropped);
                }
            }
            Cmd::Finish => {
                seq += 1;
                let ms = bytes_to_ms(total);
                let (text, downgraded) =
                    finalize(refiner.as_ref(), format!("[voice] {ms} ms")).await;
                emitter.final_(seq, text, Vec::new(), false, downgraded);
                return;
            }
            Cmd::Cancel => {
                seq += 1;
                emitter.final_(seq, String::new(), Vec::new(), true, false);
                return;
            }
        }
    }
}

/// Helper protocol: the child reads raw PCM16 LE mono 16 kHz on stdin and
/// writes one JSON object per line on stdout:
///
/// ```text
/// {"type":"partial","text":"hel","stable_len":0}
/// {"type":"final","text":"hello","words":[]}
/// {"type":"error","kind":"unavailable","message":"no model"}
/// ```
async fn run_local(
    config: LocalConfig,
    mut rx: mpsc::Receiver<Cmd>,
    emitter: Emitter,
    _caps: Caps,
    drops: Arc<AtomicU64>,
    refiner: Option<Refiner>,
) {
    let mut child = match Command::new(&config.command)
        .args(&config.args)
        .arg("--protocol")
        .arg("eix-asr/1")
        .arg("--rate")
        .arg(SAMPLE_RATE.to_string())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            emitter.error(
                0,
                AsrError::unavailable(format!("cannot start {}: {err}", config.command.display())),
            );
            return;
        }
    };
    let Some(mut stdin) = child.stdin.take() else {
        emitter.error(
            0,
            AsrError::new(AsrErrorKind::Internal, "helper has no stdin"),
        );
        return;
    };
    let Some(stdout) = child.stdout.take() else {
        emitter.error(
            0,
            AsrError::new(AsrErrorKind::Internal, "helper has no stdout"),
        );
        return;
    };
    // Speech is written from a separate task so a helper that is slow to
    // consume stdin cannot starve the reader that publishes partials.
    let (done_tx, mut done_rx) = mpsc::channel::<()>(1);
    let writer = tokio::spawn(async move {
        while let Some(command) = rx.recv().await {
            match command {
                Cmd::Audio(pcm) => {
                    if stdin.write_all(&pcm).await.is_err() {
                        break;
                    }
                }
                Cmd::Finish => break,
                Cmd::Cancel => break,
            }
        }
        let _ = stdin.shutdown().await;
        drop(stdin);
        let _ = done_tx.send(()).await;
    });

    let mut lines = BufReader::new(stdout).lines();
    let mut seq = 0;
    let final_deadline = tokio::time::Instant::now() + LOCAL_FINAL_TIMEOUT;
    let mut finished = false;
    loop {
        let timeout = final_deadline.saturating_duration_since(tokio::time::Instant::now());
        let line = match tokio::time::timeout(timeout, lines.next_line()).await {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break,
            Ok(Err(_)) => break,
            Err(_) => {
                emitter.error(
                    seq + 1,
                    AsrError::new(AsrErrorKind::Transport, "local helper timed out"),
                );
                break;
            }
        };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match value["type"].as_str().unwrap_or_default() {
            "partial" => {
                let text = value["text"].as_str().unwrap_or_default().to_string();
                let stable = value["stable_len"].as_u64().unwrap_or(0) as usize;
                if text.is_empty() {
                    continue;
                }
                seq += 1;
                // Clamp to a char boundary: a helper may count UTF-8 bytes, and the
                // client slices text[..stable_len].
                let stable = stable.min(text.len());
                let stable = (0..=stable)
                    .rev()
                    .find(|i| text.is_char_boundary(*i))
                    .unwrap_or(0);
                emitter.partial(seq, text, stable);
                let dropped = take_dropped(&drops);
                if dropped > 0 {
                    seq += 1;
                    emitter.overrun(seq, dropped);
                }
            }
            "final" => {
                let text = value["text"].as_str().unwrap_or_default().to_string();
                let words = serde_json::from_value(value["words"].clone()).unwrap_or_default();
                let degraded = value["degraded"].as_bool().unwrap_or(false);
                seq += 1;
                let (text, downgraded) = finalize(refiner.as_ref(), text).await;
                emitter.final_(seq, text, words, false, degraded || downgraded);
                finished = true;
                break;
            }
            "error" => {
                let kind =
                    serde_json::from_value(value["kind"].clone()).unwrap_or(AsrErrorKind::Internal);
                let message = value["message"]
                    .as_str()
                    .unwrap_or("local helper failed")
                    .to_string();
                seq += 1;
                emitter.error(seq, AsrError::new(kind, message));
                break;
            }
            _ => {}
        }
    }
    if finished {
        let _ = child.wait().await;
    } else {
        let _ = child.kill().await;
    }
    let _ = done_rx.recv().await;
    writer.abort();
}

async fn run_cloud(
    config: CloudConfig,
    mut rx: mpsc::Receiver<Cmd>,
    emitter: Emitter,
    client: reqwest::Client,
    drops: Arc<AtomicU64>,
    refiner: Option<Refiner>,
) {
    let mut audio = Vec::new();
    let mut seq = 0;
    while let Some(command) = rx.recv().await {
        match command {
            Cmd::Audio(pcm) => {
                audio.extend_from_slice(&pcm);
                let dropped = take_dropped(&drops);
                if dropped > 0 {
                    seq += 1;
                    emitter.overrun(seq, dropped);
                }
            }
            Cmd::Cancel => {
                seq += 1;
                emitter.final_(seq, String::new(), Vec::new(), true, false);
                return;
            }
            Cmd::Finish => break,
        }
    }
    seq += 1;
    match transcribe(&client, &config, audio).await {
        Ok((text, words)) => {
            let (text, downgraded) = finalize(refiner.as_ref(), text).await;
            emitter.final_(seq, text, words, false, downgraded)
        }
        Err(err) => emitter.error(seq, err),
    }
}

async fn transcribe(
    client: &reqwest::Client,
    config: &CloudConfig,
    audio: Vec<u8>,
) -> Result<(String, Vec<Word>), AsrError> {
    let request = client
        .post(&config.url)
        .header("Authorization", config.authorization())
        .header("Content-Type", "application/octet-stream")
        .body(audio);
    let response = request.send().await.map_err(|err| {
        AsrError::new(
            AsrErrorKind::Transport,
            format!("transcription request failed: {err}"),
        )
    })?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|err| {
        AsrError::new(
            AsrErrorKind::Provider,
            format!("cannot read transcription response: {err}"),
        )
    })?;
    if !status.is_success() {
        let message = body["error"]["message"]
            .as_str()
            .or_else(|| body["error"].as_str())
            .unwrap_or("transcription rejected");
        return Err(AsrError::new(
            AsrErrorKind::Provider,
            format!("{message} ({status})"),
        ));
    }
    parse_cloud(&body, config.provider).ok_or_else(|| {
        AsrError::new(
            AsrErrorKind::Provider,
            format!("unexpected transcript payload: {}", truncate(&body, 200)),
        )
    })
}

/// Accept both the Deepgram (`results.channels[0].alternatives[0]`) and the
/// OpenAI-compatible (`text`) shapes, so a custom gateway only needs one.
fn parse_cloud(body: &Value, provider: CloudProvider) -> Option<(String, Vec<Word>)> {
    let alternative = if provider == CloudProvider::OpenAi {
        None
    } else {
        body["results"]["channels"][0]["alternatives"][0].as_object()
    };
    if let Some(alternative) = alternative {
        let text = alternative.get("transcript")?.as_str()?.to_string();
        let words = alternative
            .get("words")
            .and_then(Value::as_array)
            .map(|words| {
                words
                    .iter()
                    .filter_map(|word| {
                        Some(Word {
                            text: word["word"].as_str()?.to_string(),
                            start_ms: (word["start"].as_f64()? * 1000.0) as u32,
                            end_ms: (word["end"].as_f64()? * 1000.0) as u32,
                            confidence: word["confidence"].as_f64().map(|value| value as f32),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        return Some((text, words));
    }
    let text = body["text"].as_str()?.to_string();
    Some((text, Vec::new()))
}

fn truncate(value: &Value, max: usize) -> String {
    let rendered = value.to_string();
    if rendered.len() <= max {
        rendered
    } else {
        format!("{}…", &rendered[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(backend: BackendKind) -> VoiceConfig {
        VoiceConfig {
            enabled: true,
            backend,
            ..VoiceConfig::default()
        }
    }

    #[tokio::test]
    async fn fake_backend_reports_duration_and_final() {
        let (bus, mut events) = broadcast::channel(16);
        let manager = Manager::new(bus);
        let start = manager
            .start(&config(BackendKind::Fake), Some("s1".into()), None, None)
            .await
            .expect("start");
        assert_eq!(start.format, PcmFormat::CANONICAL);
        assert!(!start.caps.partials);
        // 3200 bytes = 1600 samples = 100 ms.
        manager
            .push(&start.utterance_id, vec![0; 3200])
            .await
            .expect("push");
        manager.finish(&start.utterance_id).await.expect("finish");
        let notification = loop {
            let event = events.recv().await.expect("event");
            if event.method == "asr.final" {
                break event;
            }
        };
        assert_eq!(notification.params["session_id"], "s1");
        assert_eq!(notification.params["text"], "[voice] 100 ms");
        assert_eq!(notification.params["type"], "final");
    }

    #[tokio::test]
    async fn odd_length_audio_is_rejected() {
        let (bus, _events) = broadcast::channel(16);
        let manager = Manager::new(bus);
        let start = manager
            .start(&config(BackendKind::Fake), None, None, None)
            .await
            .expect("start");
        let err = manager
            .push(&start.utterance_id, vec![0; 3])
            .await
            .expect_err("reject odd length");
        assert_eq!(err.kind, AsrErrorKind::Format);
    }

    #[tokio::test]
    async fn cancel_produces_an_interim_final() {
        let (bus, mut events) = broadcast::channel(16);
        let manager = Manager::new(bus);
        let start = manager
            .start(&config(BackendKind::Fake), None, None, None)
            .await
            .expect("start");
        manager.cancel(&start.utterance_id).await.expect("cancel");
        let notification = loop {
            let event = events.recv().await.expect("event");
            if event.method == "asr.final" {
                break event;
            }
        };
        assert_eq!(notification.params["interim"], true);
    }

    #[tokio::test]
    async fn cloud_backend_requires_a_key() {
        let (bus, _events) = broadcast::channel(16);
        let manager = Manager::new(bus);
        let err = manager
            .start(&config(BackendKind::Cloud), None, None, None)
            .await
            .expect_err("missing key");
        assert_eq!(err.kind, AsrErrorKind::Unavailable);
    }

    #[test]
    fn cloud_payload_shapes_are_both_understood() {
        let deepgram = json!({
            "results": { "channels": [ { "alternatives": [ {
                "transcript": "hello world",
                "words": [ { "word": "hello", "start": 0.1, "end": 0.4 } ]
            } ] } ] }
        });
        let (text, words) = parse_cloud(&deepgram, CloudProvider::Deepgram).expect("deepgram");
        assert_eq!(text, "hello world");
        assert_eq!(words[0].start_ms, 100);

        let openai = json!({ "text": "hi" });
        let (text, words) = parse_cloud(&openai, CloudProvider::OpenAi).expect("openai");
        assert_eq!(text, "hi");
        assert!(words.is_empty());
    }

    #[test]
    fn every_backend_negotiates_caps() {
        assert!(!caps_for(BackendKind::Fake).partials);
        assert!(caps_for(BackendKind::Local).stable_prefix);
        assert!(caps_for(BackendKind::Cloud).word_timestamps);
    }

    #[test]
    fn refined_text_replaces_the_raw_transcript() {
        let raw = "so we need to edit the vox config";
        assert_eq!(
            clean_refined(raw, "So we need to edit the [voice] config."),
            Rewrite::Edited("So we need to edit the [voice] config.".to_string())
        );
        // Fenced output is unwrapped rather than pasted with backticks.
        assert_eq!(
            clean_refined(raw, "```text\nSo we need to edit the [voice] config.\n```"),
            Rewrite::Edited("So we need to edit the [voice] config.".to_string())
        );
    }

    #[test]
    fn unusable_rewrites_fall_back_to_the_raw_text() {
        let raw = "run cargo test";
        // "Already correct" is a successful pass, not a degraded one.
        assert_eq!(clean_refined(raw, raw), Rewrite::Unchanged);
        assert_eq!(clean_refined(raw, "\"run cargo test\""), Rewrite::Unchanged);
        assert_eq!(clean_refined(raw, "   "), Rewrite::Failed);
        // A model that answers the transcript instead of editing it is
        // rejected: dictation never grows into a paragraph.
        let answer = "Sure, I can help with that. ".repeat(20);
        assert_eq!(clean_refined(raw, &answer), Rewrite::Failed);
    }

    #[test]
    fn a_summarised_transcript_is_rejected() {
        // 100 chars in, 20 out: tidying a rambling dictation must not become a
        // summary of the requests it contained.
        let raw = "so the idea is that we add a flag to the config, and also we should \
                   document it, and then make sure the tests cover the fallback";
        assert_eq!(
            clean_refined(raw, "Add a flag and document it."),
            Rewrite::Failed
        );
        // The same collapse is allowed on a short fragment, where it is just a
        // stutter being removed.
        assert_eq!(
            clean_refined("Run run the the test", "Run the test."),
            Rewrite::Edited("Run the test.".to_string())
        );
    }

    #[test]
    fn refine_prompt_keeps_extra_instructions() {
        let messages = refine_messages("hello wrold", "Prefer British spelling.");
        assert_eq!(messages.len(), 2);
        assert!(messages[0].content.contains("Prefer British spelling."));
        assert!(messages[0].content.contains("speech-to-text"));
        assert_eq!(messages[1].content, "hello wrold");

        // Empty extras do not leave an empty heading behind.
        let messages = refine_messages("hi", "  ");
        assert!(!messages[0].content.contains("Additional instructions"));
    }
}
