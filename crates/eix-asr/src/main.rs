//! `eix-asr` is the local speech-to-text helper spoken to by `eixd`.
//!
//! It reads raw PCM16 little-endian mono audio and writes one JSON object per
//! line. Keeping this out of the daemon binary means the shipped `eixd` stays
//! free of ONNX Runtime and whisper.cpp, and the engine can be swapped without
//! touching the protocol.

use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const PROTOCOL: &str = "eix-asr/1";
/// Five minutes at 16 kHz mono, matching `eix_core::asr::MAX_UTTERANCE_SECS`.
const MAX_AUDIO_BYTES: usize = 16_000 * 2 * 300;

const USAGE: &str = "\
eix-asr - offline speech to text for eix

Usage:
  eix-asr [OPTIONS]              read PCM16 mono from stdin, print NDJSON
  eix-asr --wav FILE [OPTIONS]   read a WAV file instead of stdin

Options:
  --engine NAME    fake | whisper | command | sherpa   [default: fake]
  --protocol ID    must be eix-asr/1
  --rate HZ        input sample rate                   [default: 16000]
  --language CODE  language hint passed to the engine
  --model PATH     model file or directory
  --command CMD    binary (whisper) or shell template (command, sherpa)
  --partial-ms MS  how often partials are reported     [default: 300]
  --format FMT     ndjson | text                       [default: ndjson]
  --keep-audio     keep the temporary WAV
  --list-engines   print the known engines
  -h, --help       print this help

Shell templates get {wav}, {model}, {language} and {rate} substituted. Without
{wav} the file is appended as the last argument.
";

fn main() {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => fail("format", &message),
    };
    if args.help {
        print!("{USAGE}");
        return;
    }
    if args.list_engines {
        println!("fake\nwhisper\ncommand\nsherpa");
        return;
    }
    let mut out = Out {
        text_only: args.text_only,
    };
    match run(&args, &mut out) {
        Ok(()) => {}
        Err(err) => {
            out.error(err.kind, &err.message);
            std::process::exit(1);
        }
    }
}

fn fail(kind: &'static str, message: &str) -> ! {
    let mut out = io::stdout().lock();
    let _ = writeln!(
        out,
        "{{\"type\":\"error\",\"kind\":{},\"message\":{}}}",
        json_str(kind),
        json_str(message)
    );
    let _ = out.flush();
    std::process::exit(1);
}

fn run(args: &Args, out: &mut Out) -> Result<(), Failure> {
    let engine = Engine::resolve(args)?;
    let mut previous = String::new();
    let mut audio = Vec::new();
    match &args.wav {
        Some(path) => {
            let pcm = read_wav(path, args.rate)?;
            feed(&engine, args, out, &mut previous, &mut audio, &pcm)?;
        }
        None => {
            let mut stdin = io::stdin().lock();
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                let read = stdin
                    .read(&mut chunk)
                    .map_err(|err| Failure::new("transport", format!("read stdin: {err}")))?;
                if read == 0 {
                    break;
                }
                feed(
                    &engine,
                    args,
                    out,
                    &mut previous,
                    &mut audio,
                    &chunk[..read],
                )?;
            }
        }
    }
    let text = engine.transcribe(args, &audio)?;
    out.final_(&text, engine.name());
    Ok(())
}

/// Consume one chunk: accumulate it, and let streaming engines report in
/// between chunks so the client has something to show while the speaker talks.
fn feed(
    engine: &Engine,
    args: &Args,
    out: &mut Out,
    previous: &mut String,
    audio: &mut Vec<u8>,
    chunk: &[u8],
) -> Result<(), Failure> {
    audio.extend_from_slice(chunk);
    if audio.len() > MAX_AUDIO_BYTES {
        return Err(Failure::new(
            "format",
            "utterance is longer than five minutes",
        ));
    }
    if !engine.streams() {
        return Ok(());
    }
    let step = (args.rate as usize / 1000).max(1) * 2 * args.partial_ms.max(50) as usize;
    if audio.len() / step <= (audio.len() - chunk.len()) / step {
        return Ok(());
    }
    let text = engine.preview(args, audio);
    if text.is_empty() || text == *previous {
        return Ok(());
    }
    let stable = common_prefix(previous, &text);
    out.partial(&text, stable);
    *previous = text;
    Ok(())
}

fn common_prefix(earlier: &str, later: &str) -> usize {
    let mut bytes = 0;
    for (a, b) in earlier.chars().zip(later.chars()) {
        if a != b {
            break;
        }
        bytes += a.len_utf8();
    }
    bytes
}

#[derive(Clone)]
enum Engine {
    Fake,
    Whisper {
        command: PathBuf,
        model: Option<PathBuf>,
    },
    /// SenseVoice through `sherpa-onnx-offline`. Multilingual by design, and
    /// far stronger on Chinese than a whisper model small enough to run on a
    /// laptop: on the same clip whisper-base returned word salad.
    Sherpa {
        command: PathBuf,
        model: Option<PathBuf>,
    },
    Shell {
        template: String,
        model: Option<PathBuf>,
    },
}

impl Engine {
    fn resolve(args: &Args) -> Result<Self, Failure> {
        match args.engine.as_str() {
            "fake" => Ok(Self::Fake),
            "whisper" => Ok(Self::Whisper {
                command: args
                    .command
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("whisper-cli")),
                model: args.model.clone(),
            }),
            "sherpa" | "sherpa-onnx" => Ok(Self::Sherpa {
                command: args
                    .command
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("sherpa-onnx-offline")),
                model: args.model.clone(),
            }),
            "command" | "custom" => {
                let template = args.command.as_ref().map(|path| path.display().to_string());
                match template {
                    Some(template) if !template.trim().is_empty() => Ok(Self::Shell {
                        template,
                        model: args.model.clone(),
                    }),
                    _ => Err(Failure::new(
                        "unavailable",
                        format!(
                            "engine {} needs --command with the engine's command line",
                            args.engine
                        ),
                    )),
                }
            }
            other => Err(Failure::new(
                "unavailable",
                format!("unknown engine {other:?}; try fake, whisper, sherpa or command"),
            )),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Fake => "fake",
            Self::Whisper { .. } => "whisper",
            Self::Sherpa { .. } => "sherpa",
            Self::Shell { .. } => "command",
        }
    }

    /// Only the fake engine can speak before the utterance ends. Batch engines
    /// need the whole file before they produce their first token.
    fn streams(&self) -> bool {
        matches!(self, Self::Fake)
    }

    fn preview(&self, args: &Args, audio: &[u8]) -> String {
        match self {
            Self::Fake => fake_text(args.rate, audio.len()),
            _ => String::new(),
        }
    }

    fn transcribe(&self, args: &Args, audio: &[u8]) -> Result<String, Failure> {
        if audio.len() < 2 {
            return Err(Failure::new("format", "no audio was received"));
        }
        match self {
            Self::Fake => Ok(fake_text(args.rate, audio.len())),
            Self::Whisper { command, model } => {
                let model = model.as_ref().ok_or_else(|| {
                    Failure::new("unavailable", "the whisper engine needs --model <ggml.bin>")
                })?;
                if !model.exists() {
                    return Err(Failure::new(
                        "unavailable",
                        format!("model {} does not exist", model.display()),
                    ));
                }
                let wav = temp_wav(args, audio)?;
                let prefix = wav.with_extension("").display().to_string();
                let mut cmd = Command::new(command);
                cmd.arg("-m")
                    .arg(model)
                    .arg("-f")
                    .arg(&wav)
                    .arg("-nt")
                    .arg("-otxt")
                    .arg("-of")
                    .arg(&prefix);
                if !args.language.is_empty() {
                    cmd.arg("-l").arg(&args.language);
                }
                let output = run_command(&mut cmd, "whisper")?;
                let text = std::fs::read_to_string(format!("{prefix}.txt"))
                    .unwrap_or_else(|_| String::from_utf8_lossy(&output).to_string());
                let _ = std::fs::remove_file(format!("{prefix}.txt"));
                cleanup(&wav, args);
                Ok(clean(&text))
            }
            Self::Shell { template, model } => {
                let wav = temp_wav(args, audio)?;
                let line = expand(template, &wav, model.as_deref(), args);
                let mut cmd = Command::new("sh");
                cmd.arg("-c").arg(&line);
                let output = run_command(&mut cmd, "engine")?;
                cleanup(&wav, args);
                Ok(clean(&String::from_utf8_lossy(&output)))
            }
            Self::Sherpa { command, model } => {
                let model = model.as_ref().ok_or_else(|| {
                    Failure::new(
                        "unavailable",
                        "the sherpa engine needs --model <model.int8.onnx>",
                    )
                })?;
                if !model.exists() {
                    return Err(Failure::new(
                        "unavailable",
                        format!("model {} does not exist", model.display()),
                    ));
                }
                // SenseVoice ships tokens.txt next to the model, which is how
                // the download lays it out and how we resolve it here.
                let tokens = model.with_file_name("tokens.txt");
                let wav = temp_wav(args, audio)?;
                let mut cmd = Command::new(command);
                cmd.arg(format!("--sense-voice-model={}", model.display()))
                    .arg(format!("--tokens={}", tokens.display()))
                    // Inverse text normalization turns spoken numbers and dates
                    // into digits and punctuation, which is what dictating into
                    // a prompt needs.
                    .arg("--sense-voice-use-itn=true")
                    // Two threads: measured ~0.02 RTF, so decoding a 5 s
                    // utterance costs about 120 ms. The daemon spawns one
                    // helper per utterance, so each sentence also pays model
                    // load (~0.7 s), which dominates on short speech.
                    .arg("--num-threads=2");
                if !args.language.is_empty() {
                    cmd.arg(format!("--sense-voice-language={}", args.language));
                }
                cmd.arg(&wav);
                let output = run_command(&mut cmd, "sherpa")?;
                cleanup(&wav, args);
                let stdout = String::from_utf8_lossy(&output);
                match sense_voice_text(&stdout) {
                    Some(text) => Ok(text),
                    // Models that are not SenseVoice print plain text, so an
                    // output without a result object is still readable.
                    None if !stdout
                        .lines()
                        .any(|line| line.trim_start().starts_with('{')) =>
                    {
                        Ok(clean(&stdout))
                    }
                    // Anything else would put a JSON blob into the prompt.
                    None => Err(Failure::new(
                        "provider",
                        "sherpa printed a result object without a \"text\" field",
                    )),
                }
            }
        }
    }
}

/// The transcript from `sherpa-onnx-offline` result lines.
///
/// SenseVoice prints one JSON object per utterance, carrying metadata (language,
/// emotion, token timings) around the field we want. Its banner goes to stderr,
/// but an invocation over several files prints several objects, so the last one
/// wins rather than the first.
fn sense_voice_text(stdout: &str) -> Option<String> {
    let mut transcript = None;
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Some(text) = json_string_field(line, "text") {
            transcript = Some(text);
        }
    }
    transcript
}

/// Value of a top-level string field, for the flat objects this reads. Enough
/// JSON to avoid a dependency; `serde_json` would drag in a registry the helper
/// is built without.
fn json_string_field(object: &str, name: &str) -> Option<String> {
    let needle = format!("\"{name}\":");
    let rest = &object[object.find(&needle)? + needle.len()..];
    unescape_json_string(rest.trim_start().strip_prefix('"')?)
}

fn unescape_json_string(input: &str) -> Option<String> {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                _ => return None,
            },
            ch => out.push(ch),
        }
    }
    None
}

fn fake_text(rate: u32, bytes: usize) -> String {
    let ms = bytes as u64 * 1000 / (rate as u64 * 2).max(1);
    format!("[voice] {ms} ms")
}

fn expand(template: &str, wav: &Path, model: Option<&Path>, args: &Args) -> String {
    let wav = wav.display().to_string();
    let model = model
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let mut line = template
        .replace("{wav}", &wav)
        .replace("{model}", &model)
        .replace("{language}", &args.language)
        .replace("{rate}", &args.rate.to_string());
    if !template.contains("{wav}") {
        line.push(' ');
        line.push_str(&shell_quote(&wav));
    }
    line
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn run_command(cmd: &mut Command, what: &str) -> Result<Vec<u8>, Failure> {
    let output = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|err| Failure::new("unavailable", format!("cannot start {what}: {err}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr.lines().rev().take(3).collect::<Vec<_>>().join("; ");
        return Err(Failure::new(
            "provider",
            format!("{what} exited with {}: {tail}", output.status),
        ));
    }
    Ok(output.stdout)
}

fn clean(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn temp_wav(args: &Args, audio: &[u8]) -> Result<PathBuf, Failure> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or_default();
    let path = std::env::temp_dir().join(format!("eix-asr-{}-{nanos}.wav", std::process::id()));
    write_wav(&path, args.rate, audio).map_err(|err| {
        Failure::new(
            "internal",
            format!("cannot write {}: {err}", path.display()),
        )
    })?;
    Ok(path)
}

fn cleanup(path: &Path, args: &Args) {
    if !args.keep_audio {
        let _ = std::fs::remove_file(path);
    }
}

fn write_wav(path: &Path, rate: u32, pcm: &[u8]) -> io::Result<()> {
    let data = &pcm[..pcm.len() & !1];
    let mut out = Vec::with_capacity(44 + data.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    std::fs::write(path, out)
}

fn read_wav(path: &Path, rate: u32) -> Result<Vec<u8>, Failure> {
    let bytes = std::fs::read(path).map_err(|err| {
        Failure::new(
            "unavailable",
            format!("cannot read {}: {err}", path.display()),
        )
    })?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(Failure::new(
            "format",
            format!("{} is not a RIFF/WAVE file", path.display()),
        ));
    }
    let mut pos = 12;
    let mut format: Option<(u16, u16, u32, u16)> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body = &bytes[pos + 8..(pos + 8 + len).min(bytes.len())];
        match id {
            b"fmt " if body.len() >= 16 => {
                let channels = u16::from_le_bytes([body[2], body[3]]);
                let sample_rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                let bits = u16::from_le_bytes([body[14], body[15]]);
                format = Some((
                    u16::from_le_bytes([body[0], body[1]]),
                    channels,
                    sample_rate,
                    bits,
                ));
            }
            b"data" => {
                let (encoding, channels, sample_rate, bits) = format.ok_or_else(|| {
                    Failure::new("format", format!("{} has no fmt chunk", path.display()))
                })?;
                if encoding != 1 || channels != 1 || bits != 16 || sample_rate != rate {
                    return Err(Failure::new(
                        "format",
                        format!(
                            "{} is {encoding}/{channels}ch/{bits}bit/{sample_rate}Hz; \
                             eix needs PCM16 mono {rate}Hz",
                            path.display()
                        ),
                    ));
                }
                let mut pcm = body.to_vec();
                pcm.truncate(pcm.len() & !1);
                return Ok(pcm);
            }
            _ => {}
        }
        pos += 8 + len + (len & 1);
    }
    Err(Failure::new(
        "format",
        format!("{} has no data chunk", path.display()),
    ))
}

struct Failure {
    kind: &'static str,
    message: String,
}

impl Failure {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

struct Out {
    text_only: bool,
}

impl Out {
    fn partial(&mut self, text: &str, stable_len: usize) {
        if self.text_only {
            return;
        }
        self.line(&format!(
            "{{\"type\":\"partial\",\"text\":{},\"stable_len\":{stable_len}}}",
            json_str(text)
        ));
    }

    fn final_(&mut self, text: &str, engine: &str) {
        if self.text_only {
            println!("{text}");
            let _ = io::stdout().flush();
            return;
        }
        self.line(&format!(
            "{{\"type\":\"final\",\"text\":{},\"words\":[],\"engine\":{}}}",
            json_str(text),
            json_str(engine)
        ));
    }

    fn error(&mut self, kind: &str, message: &str) {
        self.line(&format!(
            "{{\"type\":\"error\",\"kind\":{},\"message\":{}}}",
            json_str(kind),
            json_str(message)
        ));
    }

    fn line(&mut self, line: &str) {
        let mut out = io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

fn json_str(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[derive(Debug, Default)]
struct Args {
    engine: String,
    rate: u32,
    language: String,
    model: Option<PathBuf>,
    command: Option<PathBuf>,
    partial_ms: u32,
    wav: Option<PathBuf>,
    text_only: bool,
    keep_audio: bool,
    help: bool,
    list_engines: bool,
}

impl Args {
    fn parse(mut argv: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut args = Args {
            engine: "fake".into(),
            rate: 16_000,
            partial_ms: 300,
            ..Args::default()
        };
        while let Some(flag) = argv.next() {
            let mut value = |name: &str| argv.next().ok_or_else(|| format!("{name} needs a value"));
            match flag.as_str() {
                "--engine" => args.engine = value("--engine")?,
                "--protocol" => {
                    let protocol = value("--protocol")?;
                    if protocol != PROTOCOL {
                        return Err(format!(
                            "unsupported protocol {protocol:?}; this helper speaks {PROTOCOL}"
                        ));
                    }
                }
                "--rate" => args.rate = parse_num(&value("--rate")?, "--rate")?,
                "--language" => args.language = value("--language")?,
                "--model" => args.model = Some(PathBuf::from(value("--model")?)),
                "--command" => args.command = Some(PathBuf::from(value("--command")?)),
                "--partial-ms" => {
                    args.partial_ms = parse_num(&value("--partial-ms")?, "--partial-ms")?
                }
                "--wav" => args.wav = Some(PathBuf::from(value("--wav")?)),
                "--format" => {
                    args.text_only = match value("--format")?.as_str() {
                        "ndjson" => false,
                        "text" => true,
                        other => return Err(format!("unknown --format {other:?}")),
                    }
                }
                "--keep-audio" => args.keep_audio = true,
                "--list-engines" => args.list_engines = true,
                "-h" | "--help" => args.help = true,
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        if args.rate == 0 {
            return Err("--rate must be positive".into());
        }
        Ok(args)
    }
}

fn parse_num<T: std::str::FromStr>(value: &str, name: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("{name} expects a number, got {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_prefix_only_covers_agreed_text() {
        assert_eq!(common_prefix("[voice] 30", "[voice] 300 ms"), 10);
        assert_eq!(common_prefix("", "hello"), 0);
        assert_eq!(common_prefix("你好", "你好世界"), 6);
    }

    #[test]
    fn text_is_joined_and_trimmed() {
        assert_eq!(clean("  hello \n\n world  \n"), "hello world");
    }

    #[test]
    fn templates_substitute_and_quote() {
        let args = Args {
            language: "zh".into(),
            ..Args::default()
        };
        let line = expand(
            "engine --tokens={model}/tokens.txt --wav={wav} -l {language}",
            Path::new("/tmp/a b.wav"),
            Some(Path::new("/models/x")),
            &args,
        );
        assert_eq!(
            line,
            "engine --tokens=/models/x/tokens.txt --wav=/tmp/a b.wav -l zh"
        );
        let appended = expand("engine", Path::new("/tmp/a'b.wav"), None, &args);
        assert_eq!(appended, "engine '/tmp/a'\\''b.wav'");
    }

    #[test]
    fn fake_text_counts_milliseconds() {
        assert_eq!(fake_text(16_000, 32_000), "[voice] 1000 ms");
    }

    #[test]
    fn protocol_must_match() {
        let err = Args::parse(["--protocol", "eix-asr/2"].into_iter().map(str::to_string))
            .expect_err("mismatched protocol");
        assert!(err.contains("eix-asr/1"));
    }

    #[test]
    fn sense_voice_result_keeps_only_the_text() {
        let output = "\
Decoding method: greedy_search
{\"lang\": \"<|zh|>\", \"emotion\": \"<|NEUTRAL|>\", \"text\": \"开饭时间早上9点至下午5点。\", \"tokens\": [\"开\"]}
";
        assert_eq!(
            sense_voice_text(output).as_deref(),
            Some("开饭时间早上9点至下午5点。")
        );
    }

    #[test]
    fn sense_voice_result_unescapes_quotes_and_newlines() {
        let line = r#"{"text": "say \"hi\"\nthen stop", "tokens": []}"#;
        assert_eq!(
            sense_voice_text(line).as_deref(),
            Some("say \"hi\"\nthen stop")
        );
    }

    #[test]
    fn sense_voice_result_without_text_is_rejected() {
        // A changed result shape must not reach the prompt as a JSON blob.
        assert_eq!(sense_voice_text(r#"{"lang": "<|en|>"}"#), None);
        assert_eq!(sense_voice_text("plain text output"), None);
    }
}
