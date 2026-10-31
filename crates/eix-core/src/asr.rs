//! Speech-to-text protocol shared by the daemon, the TUI and the voice helper.
//!
//! This module is deliberately engine-free: it defines the audio contract, the
//! capability flags backends negotiate, and the event stream frontends render.
//! Engines live behind `eixd::asr::AsrBackend` so a cloud provider and a local
//! model differ only in their adapter.

use serde::{Deserialize, Serialize};

/// The only sample rate the pipeline accepts end to end. Capture resamples
/// once; nothing downstream renegotiates it.
pub const SAMPLE_RATE: u32 = 16_000;
/// Upper bound for one utterance before the daemon asks for a `finish`.
pub const MAX_UTTERANCE_SECS: u32 = 300;

/// PCM layout on the wire: signed 16-bit little endian, mono.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PcmFormat {
    pub rate: u32,
    pub channels: u8,
    pub bits: u8,
}

impl PcmFormat {
    pub const CANONICAL: Self = Self {
        rate: SAMPLE_RATE,
        channels: 1,
        bits: 16,
    };

    /// Reject anything we would otherwise have to resample silently. A caller
    /// that gets this error has a capture bug, not a preference.
    pub fn validate(&self) -> Result<(), AsrError> {
        if self.rate != SAMPLE_RATE {
            return Err(AsrError::new(
                AsrErrorKind::Format,
                format!("sample rate must be {SAMPLE_RATE} Hz, got {}", self.rate),
            ));
        }
        if self.channels != 1 {
            return Err(AsrError::new(
                AsrErrorKind::Format,
                format!("audio must be mono, got {} channels", self.channels),
            ));
        }
        if self.bits != 16 {
            return Err(AsrError::new(
                AsrErrorKind::Format,
                format!("audio must be 16-bit, got {}", self.bits),
            ));
        }
        Ok(())
    }
}

impl Default for PcmFormat {
    fn default() -> Self {
        Self::CANONICAL
    }
}

/// What a backend can deliver, so the UI never asks for more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Caps {
    /// Emits interim hypotheses before the utterance ends.
    pub partials: bool,
    /// Partials carry a usable `stable_len`.
    pub stable_prefix: bool,
    pub word_timestamps: bool,
    pub punctuation: bool,
    pub diarization: Support,
    /// Cloud providers cap a single stream; `None` means unbounded.
    pub max_stream_secs: Option<u32>,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            partials: true,
            stable_prefix: false,
            word_timestamps: false,
            punctuation: false,
            diarization: Support::No,
            max_stream_secs: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    No,
    FinalOnly,
    Streaming,
}

impl Support {
    pub fn is_supported(self) -> bool {
        !matches!(self, Self::No)
    }
}

/// One recognized word with audio-relative timing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub start_ms: u32,
    pub end_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

/// Backend output. Every variant carries `seq`, which is monotonic per
/// utterance, so a frontend can drop stale partials that arrive after the
/// upload and SSE connections reorder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AsrEvent {
    /// Interim hypothesis. `stable_len` counts characters of `text` that
    /// earlier partials already agreed on; zero means "replace everything".
    Partial {
        seq: u64,
        text: String,
        #[serde(default)]
        stable_len: usize,
    },
    /// Final text for one utterance. `interim` marks a final produced by a
    /// cancel or a dropped stream: render it, but do not treat it as complete.
    Final {
        seq: u64,
        text: String,
        #[serde(default)]
        words: Vec<Word>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        speaker: Option<u16>,
        #[serde(default)]
        degraded: bool,
        #[serde(default)]
        interim: bool,
    },
    /// The daemon dropped audio because the backend fell behind.
    Overrun { seq: u64, dropped_ms: u32 },
    Error {
        seq: u64,
        kind: AsrErrorKind,
        message: String,
        retryable: bool,
    },
}

impl AsrEvent {
    pub fn seq(&self) -> u64 {
        match self {
            Self::Partial { seq, .. }
            | Self::Final { seq, .. }
            | Self::Overrun { seq, .. }
            | Self::Error { seq, .. } => *seq,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsrErrorKind {
    /// Audio does not match [`PcmFormat::CANONICAL`].
    Format,
    /// Model files, helper binary or credentials are missing.
    Unavailable,
    /// Upstream rejected the request (auth, quota, bad model).
    Provider,
    /// Transport failed; retrying the same utterance may work.
    Transport,
    /// We cancelled the utterance.
    Cancelled,
    /// Backend bug or unparsable output.
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsrError {
    pub kind: AsrErrorKind,
    pub message: String,
}

impl AsrError {
    pub fn new(kind: AsrErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(AsrErrorKind::Unavailable, message)
    }

    pub fn format(message: impl Into<String>) -> Self {
        Self::new(AsrErrorKind::Format, message)
    }

    pub fn retryable(&self) -> bool {
        matches!(self.kind, AsrErrorKind::Transport)
    }
}

impl std::fmt::Display for AsrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AsrError {}

/// Which adapter a session uses. `Fake` exists for tests and for frontend work
/// without a microphone or a model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    #[default]
    Local,
    Cloud,
    Fake,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudProvider {
    #[default]
    Deepgram,
    /// OpenAI's `realtime` transcription session (`gpt-4o-transcribe`).
    OpenAi,
    /// Any endpoint that speaks the same query-parameter/`Message` shape.
    Custom,
}

impl CloudProvider {
    /// Default credential slot in `eix auth`.
    pub fn credential_key(self) -> &'static str {
        match self {
            Self::Deepgram => "asr.deepgram",
            Self::OpenAi => "asr.openai",
            Self::Custom => "asr.custom",
        }
    }

    pub fn default_endpoint(self) -> &'static str {
        match self {
            Self::Deepgram => "wss://api.deepgram.com/v1/listen",
            Self::OpenAi => "wss://api.openai.com/v1/realtime",
            Self::Custom => "",
        }
    }
}

/// Local models run in a helper process, so the daemon never links ONNX
/// Runtime or CUDA. The helper protocol is documented in `docs/voice.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalAsrConfig {
    /// `whisper` | `sherpa` | `fake`, or empty to pick automatically: sherpa
    /// (SenseVoice) when a model is installed, whisper otherwise. Only selects
    /// default arguments; the helper binary is what understands them.
    pub engine: String,
    /// Empty searches `PATH`, then the daemon's own directory, then
    /// `~/.local/bin` for `eix-asr`.
    pub command: Option<std::path::PathBuf>,
    /// Replaces the derived arguments wholesale. Advanced escape hatch; leave
    /// empty unless the helper needs flags eix does not know about.
    pub args: Vec<String>,
    /// A `ggml-*.bin` (whisper) or `model.int8.onnx` (sherpa). Empty picks the
    /// most accurate installed model, preferring SenseVoice over whisper.
    pub model_dir: Option<std::path::PathBuf>,
}

impl Default for LocalAsrConfig {
    fn default() -> Self {
        Self {
            engine: String::new(),
            command: None,
            args: Vec::new(),
            model_dir: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudAsrConfig {
    pub provider: CloudProvider,
    pub model: String,
    /// Overrides the provider default. Must be `wss://` (or `ws://` for a
    /// local gateway).
    pub endpoint: Option<String>,
    /// Credential slot; defaults to [`CloudProvider::credential_key`].
    pub credential: Option<String>,
}

/// Optional second pass over a finished transcript by a chat model.
///
/// A small local ASR model mishears names and drops punctuation, and dictation
/// itself repeats words, abandons sentences and jumps between topics. With this
/// enabled the final transcript makes one extra provider call before it reaches
/// the client; the raw text is always the fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RefineConfig {
    /// On by default: raw dictation is repetitive and mistyped, and a chat
    /// pass is what makes it usable. Set `false` to send the transcript
    /// exactly as the engine heard it.
    pub enabled: bool,
    /// Model selection id (`provider/model`). Empty uses the default model.
    pub model: String,
    /// Extra instruction appended to the built-in rewrite prompt.
    pub prompt: String,
    /// Hard cap; past this the raw transcript is kept, so a slow model can
    /// never hold the text hostage.
    pub timeout_ms: u32,
}

impl Default for RefineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: String::new(),
            prompt: String::new(),
            timeout_ms: 8_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    /// On by default, but inert: nothing is captured and nothing is sent until
    /// the user asks for `/voice`, so an idle session never touches a
    /// microphone. Set `false` to make `/voice` report that it is disabled.
    pub enabled: bool,
    pub backend: BackendKind,
    /// Empty means "let the engine detect it".
    pub language: String,
    pub local: LocalAsrConfig,
    pub cloud: CloudAsrConfig,
    pub refine: RefineConfig,
    pub capture: CaptureConfig,
}

/// Client-side microphone settings. The daemon never reads a device, but the
/// defaults live here so the TUI and any other client segment speech the same
/// way.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    /// Recorder command line whose stdout is raw PCM16 mono 16 kHz. Empty means
    /// "pick whichever of parec, pw-record, arecord exists".
    pub command: String,
    /// RMS gate on a 0.0-1.0 scale. The default sits just above a silent room
    /// and below a soft voice, so a normal laptop microphone needs no tuning.
    /// Raise it only if background noise keeps an utterance open.
    pub threshold: f32,
    /// Silence that closes an utterance.
    pub silence_ms: u32,
    /// Hard cap so a noisy room cannot upload forever.
    pub max_utterance_ms: u32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            threshold: 0.004,
            silence_ms: 800,
            max_utterance_ms: 60_000,
        }
    }
}

impl CaptureConfig {
    /// Resolve the recorder to spawn. An explicit `command` always wins; the
    /// fallbacks go from the most portable to the most old-fashioned, so the
    /// error can name what was actually tried.
    pub fn recorder(&self) -> Result<(std::path::PathBuf, Vec<String>), AsrError> {
        if !self.command.trim().is_empty() {
            let mut parts = self.command.split_whitespace();
            let program = parts.next().unwrap_or_default();
            return Ok((
                std::path::PathBuf::from(program),
                parts.map(str::to_string).collect(),
            ));
        }
        let candidates: [(&str, &[&str]); 3] = [
            (
                "parec",
                &[
                    "--format=s16le",
                    "--rate=16000",
                    "--channels=1",
                    "--raw",
                    "--latency-msec=20",
                ],
            ),
            (
                "pw-record",
                &["--format=s16", "--rate=16000", "--channels=1", "-"],
            ),
            (
                "arecord",
                &[
                    "-q", "-t", "raw", "-f", "S16_LE", "-r", "16000", "-c", "1", "-",
                ],
            ),
        ];
        for (program, args) in candidates {
            if on_path(program) {
                return Ok((
                    std::path::PathBuf::from(program),
                    args.iter().map(|arg| arg.to_string()).collect(),
                ));
            }
        }
        Err(AsrError::unavailable(
            "no recorder found: install parec, pw-record or arecord, \
             or set [voice.capture] command",
        ))
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: BackendKind::Local,
            language: String::new(),
            local: LocalAsrConfig::default(),
            cloud: CloudAsrConfig::default(),
            refine: RefineConfig::default(),
            capture: CaptureConfig::default(),
        }
    }
}

impl VoiceConfig {
    /// Reject unusable combinations before a session starts, not when the
    /// first audio frame arrives.
    pub fn validate(&self) -> Result<(), AsrError> {
        PcmFormat::CANONICAL.validate()?;
        if !self.enabled {
            return Err(AsrError::unavailable("voice is disabled in [voice]"));
        }
        if self.backend == BackendKind::Cloud {
            let endpoint = self
                .cloud
                .endpoint
                .clone()
                .unwrap_or_else(|| self.cloud.provider.default_endpoint().to_string());
            if !(endpoint.starts_with("wss://") || endpoint.starts_with("ws://")) {
                return Err(AsrError::unavailable(format!(
                    "cloud endpoint must be ws:// or wss://, got {endpoint:?}"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_format_is_accepted_and_others_are_rejected() {
        assert!(PcmFormat::CANONICAL.validate().is_ok());
        for bad in [
            PcmFormat {
                rate: 48_000,
                ..PcmFormat::CANONICAL
            },
            PcmFormat {
                channels: 2,
                ..PcmFormat::CANONICAL
            },
            PcmFormat {
                bits: 32,
                ..PcmFormat::CANONICAL
            },
        ] {
            let err = bad.validate().expect_err("format should be rejected");
            assert_eq!(err.kind, AsrErrorKind::Format);
        }
    }

    #[test]
    fn events_round_trip_with_a_type_tag() {
        let event = AsrEvent::Final {
            seq: 3,
            text: "你好".into(),
            words: vec![Word {
                text: "你好".into(),
                start_ms: 0,
                end_ms: 240,
                confidence: None,
            }],
            speaker: Some(1),
            degraded: false,
            interim: false,
        };
        let value = serde_json::to_value(&event).expect("serialize");
        assert_eq!(value["type"], "final");
        assert_eq!(value["seq"], 3);
        let back: AsrEvent = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back, event);
    }

    #[test]
    fn voice_config_is_on_and_local_by_default() {
        let config = VoiceConfig::default();
        assert!(config.enabled);
        assert_eq!(config.backend, BackendKind::Local);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn cloud_backend_requires_a_websocket_endpoint() {
        let mut config = VoiceConfig {
            enabled: true,
            backend: BackendKind::Cloud,
            ..VoiceConfig::default()
        };
        config.cloud.provider = CloudProvider::Deepgram;
        assert!(config.validate().is_ok());
        config.cloud.endpoint = Some("https://api.deepgram.com/v1/listen".into());
        assert_eq!(
            config.validate().unwrap_err().kind,
            AsrErrorKind::Unavailable
        );
    }

    #[test]
    fn local_backend_does_not_need_network_configuration() {
        let config = VoiceConfig {
            enabled: true,
            backend: BackendKind::Local,
            ..VoiceConfig::default()
        };
        assert!(config.validate().is_ok());
    }
}
