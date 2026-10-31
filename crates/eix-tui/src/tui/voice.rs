//! Microphone capture and utterance segmentation for `/voice`.
//!
//! Capture stays out of the daemon: a terminal client owns the device, and the
//! daemon only ever sees PCM over `asr.pcm`. There is no audio crate in the
//! dependency set, so the device is opened by an external recorder whose stdout
//! is raw PCM. Energy-based segmentation then decides where an utterance ends;
//! a real VAD model would slot in behind [`Vad`] without touching the rest.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc::UnboundedSender;

pub use eix_core::asr::CaptureConfig;

use super::net::Requester;

/// One frame is 20 ms: small enough to notice silence promptly, large enough
/// that the RMS is not dominated by a single click.
pub const FRAME_MS: u32 = 20;
pub const SAMPLE_RATE: u32 = 16_000;
const FRAME_BYTES: usize = (SAMPLE_RATE as usize / 1000) * 2 * FRAME_MS as usize;
/// RPC and uploads are batched at this size; one call per 20 ms frame would be
/// 50 requests a second for no benefit.
const SEND_BYTES: usize = (SAMPLE_RATE as usize / 1000) * 2 * 100;

/// What the capture task reports to the UI and the daemon client.
#[derive(Debug)]
pub enum CaptureEvent {
    /// Utterance audio began; the caller runs `asr.start`.
    Speech,
    /// A batch of PCM16 for the open utterance.
    Pcm(Vec<u8>),
    /// Silence or a hard limit closed the utterance; the caller runs
    /// `asr.finish`.
    End {
        spoken_ms: u32,
        /// The hard duration limit closed it rather than silence.
        capped: bool,
    },
    /// Input level for the status indicator, 0.0-1.0.
    Level(f32),
    /// Capture ended on its own (EOF, recorder exit, or stop request).
    Done,
    Error(String),
}

/// Coarse state the statusline shows. Audio itself never reaches the UI.
#[derive(Debug, PartialEq)]
pub enum VoiceEvent {
    /// The user started talking; `asr.start` has been sent.
    Speech,
    /// An utterance closed and its `asr.finish` was sent.
    End {
        spoken_ms: u32,
        capped: bool,
    },
    /// Input level, `0.0-1.0`.
    Level(f32),
    /// Capture stopped; the button can go back to idle.
    Done,
    Error(String),
}

/// Run one dictation session: open the recorder, start an utterance on speech,
/// stream PCM to the daemon and finish it on silence.
pub async fn dictate(
    requester: Requester,
    capture_config: CaptureConfig,
    session_id: Option<String>,
    file: Option<PathBuf>,
    stop: tokio::sync::oneshot::Receiver<()>,
    tx: UnboundedSender<VoiceEvent>,
) {
    let (audio_tx, mut audio_rx) = tokio::sync::mpsc::unbounded_channel();
    if let Some(path) = file {
        // A recording is framed and segmented exactly like live audio, so only
        // the byte source differs.
        let source = match open_file(&path).await {
            Ok(source) => source,
            Err(err) => {
                let _ = tx.send(VoiceEvent::Error(format!("{err:#}")));
                return;
            }
        };
        let reader = tokio::spawn(capture(source, capture_config, stop, audio_tx));
        forward(requester, session_id, &mut audio_rx, &tx).await;
        reader.abort();
        let _ = tx.send(VoiceEvent::Done);
        return;
    }
    let (program, args) = match capture_config.recorder() {
        Ok(recorder) => recorder,
        Err(err) => {
            let _ = tx.send(VoiceEvent::Error(err.message));
            return;
        }
    };
    let mut child = match tokio::process::Command::new(&program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            let _ = tx.send(VoiceEvent::Error(format!(
                "cannot start {}: {err}",
                program.display()
            )));
            return;
        }
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = tx.send(VoiceEvent::Error("recorder has no stdout".into()));
        return;
    };
    let reader = tokio::spawn(capture(stdout, capture_config, stop, audio_tx));
    forward(requester, session_id, &mut audio_rx, &tx).await;
    reader.abort();
    let _ = child.start_kill();
    let _ = tx.send(VoiceEvent::Done);
}

/// Push captured audio to the daemon: `asr.start` on the first speech frame,
/// `asr.pcm` per batch, `asr.finish` when the utterance closes.
async fn forward(
    requester: Requester,
    session_id: Option<String>,
    audio_rx: &mut tokio::sync::mpsc::UnboundedReceiver<CaptureEvent>,
    tx: &UnboundedSender<VoiceEvent>,
) {
    let mut utterance: Option<String> = None;
    while let Some(event) = audio_rx.recv().await {
        match event {
            CaptureEvent::Speech => match start(&requester, session_id.clone()).await {
                Ok(id) => {
                    utterance = Some(id);
                    let _ = tx.send(VoiceEvent::Speech);
                }
                Err(err) => {
                    let _ = tx.send(VoiceEvent::Error(format!("{err:#}")));
                    break;
                }
            },
            CaptureEvent::Pcm(pcm) => {
                let Some(id) = utterance.clone() else {
                    continue;
                };
                let encoded = base64::engine::general_purpose::STANDARD.encode(&pcm);
                let sent = requester
                    .request("asr.pcm", json!({ "utterance_id": id, "pcm": encoded }))
                    .await;
                if let Err(err) = sent {
                    let _ = tx.send(VoiceEvent::Error(format!("{err:#}")));
                    utterance = None;
                }
            }
            CaptureEvent::End { spoken_ms, capped } => {
                if let Some(id) = utterance.take() {
                    let _ = requester
                        .request("asr.finish", json!({ "utterance_id": id }))
                        .await;
                }
                let _ = tx.send(VoiceEvent::End { spoken_ms, capped });
            }
            CaptureEvent::Level(level) => {
                let _ = tx.send(VoiceEvent::Level(level));
            }
            CaptureEvent::Done => break,
            CaptureEvent::Error(message) => {
                let _ = tx.send(VoiceEvent::Error(message));
                break;
            }
        }
    }
    if let Some(id) = utterance.take() {
        let _ = requester
            .request("asr.cancel", json!({ "utterance_id": id }))
            .await;
    }
}

async fn start(requester: &Requester, session_id: Option<String>) -> Result<String> {
    let value = requester
        .request("asr.start", json!({ "session_id": session_id }))
        .await
        .context("asr.start failed")?;
    value["utterance_id"]
        .as_str()
        .map(str::to_string)
        .context("asr.start returned no utterance_id")
}

/// Silence gate. Pure energy: speech and noise both pass, so a quiet room is
/// required, but it needs no model file and never blocks the first word.
pub struct Vad {
    threshold: f32,
    silence_ms: u32,
    max_ms: u32,
    active: bool,
    silent: u32,
    spoken: u32,
    /// Frames of pre-roll kept so the first syllable is not clipped.
    preroll: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Still silent.
    Idle,
    /// An utterance began with this frame; include the pre-roll.
    Start,
    /// Speech continues.
    Speech,
    /// Silence closed the utterance.
    End,
    /// The hard duration limit closed the utterance.
    Capped,
}

impl Vad {
    pub fn new(config: &CaptureConfig) -> Self {
        Self {
            threshold: config.threshold.clamp(0.0005, 1.0),
            silence_ms: config.silence_ms.clamp(100, 5_000),
            max_ms: config.max_utterance_ms.clamp(1_000, 300_000),
            active: false,
            silent: 0,
            spoken: 0,
            preroll: Vec::new(),
        }
    }

    pub fn push(&mut self, frame: &[u8], ms: u32) -> Step {
        let loud = rms(frame) >= self.threshold;
        if !self.active {
            if !loud {
                // Keep a short tail of quiet audio so the utterance does not
                // start mid-word.
                self.preroll.extend_from_slice(frame);
                let keep = SEND_BYTES.min(self.preroll.len());
                if self.preroll.len() > keep {
                    let drop = self.preroll.len() - keep;
                    self.preroll.drain(..drop);
                }
                return Step::Idle;
            }
            self.active = true;
            self.silent = 0;
            self.spoken = ms;
            self.preroll.clear();
            return Step::Start;
        }
        self.spoken = self.spoken.saturating_add(ms);
        if loud {
            self.silent = 0;
        } else {
            self.silent = self.silent.saturating_add(ms);
            if self.silent >= self.silence_ms {
                self.active = false;
                return Step::End;
            }
        }
        if self.spoken >= self.max_ms {
            self.active = false;
            return Step::Capped;
        }
        Step::Speech
    }

    pub fn spoken_ms(&self) -> u32 {
        self.spoken
    }
}

fn rms(frame: &[u8]) -> f32 {
    if frame.len() < 2 {
        return 0.0;
    }
    let samples = frame.len() / 2;
    let mut sum = 0f64;
    for chunk in frame[..samples * 2].chunks_exact(2) {
        let sample = i16::from_le_bytes([chunk[0], chunk[1]]) as f64 / 32768.0;
        sum += sample * sample;
    }
    (sum / samples as f64).sqrt() as f32
}

/// Read a source frame by frame and report utterance boundaries. `source` is
/// the recorder's stdout, or a file for `/voice file`.
pub async fn capture<R>(
    mut source: R,
    config: CaptureConfig,
    mut stop: tokio::sync::oneshot::Receiver<()>,
    tx: UnboundedSender<CaptureEvent>,
) where
    R: AsyncRead + Unpin,
{
    let mut vad = Vad::new(&config);
    let mut frame = vec![0u8; FRAME_BYTES];
    let mut pending = Vec::with_capacity(SEND_BYTES);
    loop {
        let read = tokio::select! {
            _ = &mut stop => break,
            read = source.read(&mut frame) => read,
        };
        let read = match read {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) => {
                let _ = tx.send(CaptureEvent::Error(format!("recorder read failed: {err}")));
                return;
            }
        };
        let chunk = &frame[..read];
        let level = (rms(chunk) * 4.0).min(1.0);
        let _ = tx.send(CaptureEvent::Level(level));
        match vad.push(chunk, FRAME_MS) {
            Step::Idle => continue,
            Step::Start => {
                // The pre-roll is already inside the VAD; nothing to replay.
                pending.clear();
                pending.extend_from_slice(chunk);
                let _ = tx.send(CaptureEvent::Speech);
            }
            Step::Speech => pending.extend_from_slice(chunk),
            step @ (Step::End | Step::Capped) => {
                pending.extend_from_slice(chunk);
                let spoken_ms = vad.spoken_ms();
                if !pending.is_empty() {
                    let _ = tx.send(CaptureEvent::Pcm(std::mem::take(&mut pending)));
                }
                let _ = tx.send(CaptureEvent::End {
                    spoken_ms,
                    capped: step == Step::Capped,
                });
                // Either way the utterance is over; keep listening for the next
                // one rather than forcing the user to re-arm dictation.
                continue;
            }
        }
        if pending.len() >= SEND_BYTES {
            let _ = tx.send(CaptureEvent::Pcm(std::mem::take(&mut pending)));
        }
    }
    if !pending.is_empty() {
        let _ = tx.send(CaptureEvent::Pcm(std::mem::take(&mut pending)));
    }
    if vad.active {
        let _ = tx.send(CaptureEvent::End {
            spoken_ms: vad.spoken_ms(),
            capped: false,
        });
    }
    let _ = tx.send(CaptureEvent::Done);
}

/// Raw PCM16 mono 16 kHz, or a WAV file of the same layout. A WAV header is
/// skipped rather than rejected: `/voice file` is a convenience, not a format
/// converter.
pub async fn open_file(path: &Path) -> Result<Box<dyn AsyncRead + Unpin + Send>> {
    let file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("cannot open {}", path.display()))?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut head = [0u8; 12];
    let read = reader.read(&mut head).await?;
    if read == 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WAVE" {
        skip_wav_chunks(&mut reader).await?;
        return Ok(Box::new(reader));
    }
    // Raw PCM, or a file shorter than a header: rewind so the peeked bytes are
    // read again rather than dropped.
    reader.seek(std::io::SeekFrom::Start(0)).await?;
    Ok(Box::new(reader))
}

async fn skip_wav_chunks<R>(reader: &mut R) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    loop {
        let mut header = [0u8; 8];
        if reader.read_exact(&mut header).await.is_err() {
            bail!("WAV file ends before its data chunk");
        }
        let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as u64;
        match &header[..4] {
            b"fmt " => {
                let mut body = vec![0u8; len.min(64) as usize];
                reader.read_exact(&mut body).await?;
                if body.len() >= 16 {
                    let format = u16::from_le_bytes([body[0], body[1]]);
                    let channels = u16::from_le_bytes([body[2], body[3]]);
                    let rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                    let bits = u16::from_le_bytes([body[14], body[15]]);
                    if format != 1 || channels != 1 || bits != 16 || rate != SAMPLE_RATE {
                        bail!(
                            "WAV is format {format}, {channels} channel(s), {bits}-bit, \
                             {rate} Hz; eix needs PCM16 mono {SAMPLE_RATE} Hz"
                        );
                    }
                }
                let extra = len.saturating_sub(body.len() as u64);
                drain(reader, extra).await?;
            }
            b"data" => return Ok(()),
            _ => drain(reader, len).await?,
        }
        if len % 2 == 1 {
            drain(reader, 1).await?;
        }
    }
}

async fn drain<R>(reader: &mut R, mut bytes: u64) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut scratch = [0u8; 4096];
    while bytes > 0 {
        let want = bytes.min(scratch.len() as u64) as usize;
        let read = reader.read(&mut scratch[..want]).await?;
        if read == 0 {
            return Ok(());
        }
        bytes -= read as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(amplitude: i16) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..(SAMPLE_RATE / 1000 * FRAME_MS) {
            out.extend_from_slice(&amplitude.to_le_bytes());
        }
        out
    }

    #[test]
    fn silence_never_starts_an_utterance() {
        let mut vad = Vad::new(&CaptureConfig::default());
        let quiet = frame(0);
        for _ in 0..100 {
            assert_eq!(vad.push(&quiet, FRAME_MS), Step::Idle);
        }
        assert_eq!(vad.spoken_ms(), 0);
    }

    #[test]
    fn speech_then_silence_closes_the_utterance() {
        let mut vad = Vad::new(&CaptureConfig::default());
        assert_eq!(vad.push(&frame(4000), FRAME_MS), Step::Start);
        assert_eq!(vad.push(&frame(4000), FRAME_MS), Step::Speech);
        // 800 ms of configured silence, at 20 ms per frame.
        let quiet = frame(0);
        for _ in 0..39 {
            assert_eq!(vad.push(&quiet, FRAME_MS), Step::Speech);
        }
        assert_eq!(vad.push(&quiet, FRAME_MS), Step::End);
    }

    #[test]
    fn the_hard_limit_caps_a_noisy_utterance() {
        // One second is the smallest cap the VAD accepts, so a noisy room is
        // closed after 50 frames rather than streaming forever.
        let config = CaptureConfig {
            max_utterance_ms: 1_000,
            ..CaptureConfig::default()
        };
        let mut vad = Vad::new(&config);
        assert_eq!(vad.push(&frame(4000), FRAME_MS), Step::Start);
        let capped = (0..60)
            .map(|_| vad.push(&frame(4000), FRAME_MS))
            .any(|step| step == Step::Capped);
        assert!(capped, "a loud utterance must hit the duration cap");
    }

    #[test]
    fn rms_separates_quiet_from_loud_audio() {
        assert!(rms(&frame(0)) < 0.001);
        assert!(rms(&frame(8000)) > 0.2);
    }

    #[test]
    fn the_default_gate_hears_quiet_speech() {
        // About -45 dBFS: a soft voice across a laptop microphone, well under
        // the old 0.02 default, which missed it entirely.
        let soft = frame(200);
        assert!(rms(&soft) > 0.005 && rms(&soft) < 0.01);
        assert_eq!(
            Vad::new(&CaptureConfig::default()).push(&soft, FRAME_MS),
            Step::Start
        );
    }

    #[test]
    fn the_default_gate_ignores_a_quiet_room() {
        let mut vad = Vad::new(&CaptureConfig::default());
        // Digital silence and a faint room hum both stay below the gate; 0.04
        // counts is about -48 dBFS, quieter than any voice reaches the mic.
        for _ in 0..100 {
            assert_eq!(vad.push(&frame(0), FRAME_MS), Step::Idle);
            assert_eq!(vad.push(&frame(40), FRAME_MS), Step::Idle);
        }
    }
}
