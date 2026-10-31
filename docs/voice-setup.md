# Voice input setup

This page covers getting `/voice` working: which engine to install, where the models go, which
settings to change, and what to check when something breaks. Protocol details (JSON-RPC methods,
event fields, helper output format) live in [voice.md](./voice.md); this is the usage guide.

The defaults already run: `[voice] enabled = true` needs no change, and an idle session never
touches a microphone. Only two things are actually required, a recognition engine and its model.
Once both are in place, no further configuration is needed.

## Boundaries first

- Recording happens in the frontend (`/voice` in the TUI); the daemon never opens an audio device.
- Recognition runs in an `eix-asr` child process, one per utterance, receiving PCM on stdin and
  writing JSON to stdout.
- Only the recognised text goes through an optional model pass that tidies it up; audio never
  leaves the machine when `backend = "local"`.
- Model files live in the data directory, not in the config file. The data directory is
 `~/.local/share/eix` (on macOS `~/Library/Application Support/eix`), and `EIX_DATA_DIR` overrides it
  wholesale.

## Quick start

Confirm the three prerequisites, then type `/voice`:

```sh
eix-asr --list-engines     # fake / whisper / command / sherpa
sherpa-onnx-offline --help      # or whisper-cli --help
ls ~/.local/share/eix/models/sense-voice/   # model.int8.onnx tokens.txt
```

```sh
cargo build --release -p eix-asr            # make install does not include it
install -m 0755 target/release/eix-asr ~/.local/bin/
```

Then type `/voice`; a `mic ▮▮▮` level meter appears in the status bar and you can start talking.
The engine and model are resolved fresh on every `asr.start`, so swapping either needs no restart;
after editing `config.toml`, run `/reload` to make the daemon re-read it.

## Installing a recognition engine

Pick one of the two engines; with both installed, SenseVoice wins. For Chinese this choice matters
a lot: on the same audio, `ggml-base.bin` invents English sentences
(`Time for the opening. Find 9.3.5.`) while SenseVoice transcribes it correctly.

### SenseVoice (recommended; Chinese, English, Japanese, Korean, Cantonese)

Two pieces are needed: the `sherpa-onnx-offline` executable and a SenseVoice model.

The runtime comes from the pip package. If the system Python is protected by PEP 668 (Arch and
recent Debian both are), a dedicated venv is the least painful route:

```sh
python3 -m venv ~/.local/share/eix/sherpa-runtime
~/.local/share/eix/sherpa-runtime/bin/pip install -q \
  -i https://mirrors.aliyun.com/pypi/simple/ sherpa-onnx-bin sherpa-onnx-core

mkdir -p ~/.local/lib/eix-asr ~/.local/bin
cp ~/.local/share/eix/sherpa-runtime/bin/sherpa-onnx-offline ~/.local/lib/eix-asr/
cp ~/.local/share/eix/sherpa-runtime/lib/python*/site-packages/sherpa_onnx/lib/libonnxruntime.so \
  ~/.local/lib/eix-asr/
ln -sf ~/.local/lib/eix-asr/sherpa-onnx-offline ~/.local/bin/sherpa-onnx-offline

sherpa-onnx-offline --help >/dev/null && echo runtime ok
```

The important part is that `libonnxruntime.so` sits next to the binary: the pip wheel bakes
`$ORIGIN` into its rpath, so the same directory resolves it without `patchelf`. Separating them
yields `error while loading shared libraries: libonnxruntime.so`.

A distro package or the GitHub release
`sherpa-onnx-v2.0.1-linux-x64-shared.tar.bz2` works too, as long as it provides
`sherpa-onnx-offline`.

The model is 228 MiB. Both files must share a directory, with `tokens.txt` next to the model:

```sh
mkdir -p ~/.local/share/eix/models/sense-voice
cd ~/.local/share/eix/models/sense-voice
B=https://huggingface.co/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/main
curl -fL -O $B/model.int8.onnx -O $B/tokens.txt
```

Always verify after downloading. A truncated ONNX file fails to load with
`Protobuf parsing failed`, and that failure happens before any fallback to whisper:

```sh
sha256sum model.int8.onnx tokens.txt
# c71f0ce00bec95b07744e116345e33d8cbbe08cef896382cf907bf4b51a2cd51  model.int8.onnx
# f449eb28dc567533d7fa59be34e2abca8784f771850c78a47fb731a31429a1dc  tokens.txt
wc -c model.int8.onnx   # 239233841
```

On an unreliable network, resume rather than waiting out a single stream:

```sh
curl -fL -C - --retry 5 -o model.int8.onnx $B/model.int8.onnx
```

### whisper (fallback: smaller, weak on Chinese)

Needs `whisper-cli` plus one `ggml-*.bin`. Installation varies by distribution: Arch has
`extra/whisper-cpp`; elsewhere build it following the
[whisper.cpp](https://github.com/ggml-org/whisper.cpp) instructions (its release page ships source
only, no binaries). All that matters is that `whisper-cli` ends up on `PATH`.

```sh
mkdir -p ~/.local/share/eix/models
curl -fL -o ~/.local/share/eix/models/ggml-base.bin \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin
```

`ggml-base.bin` is 141 MiB, half of SenseVoice's 228 MiB, with a peak of roughly 285 MB. Chinese
output is close to unusable, so treat it as the fallback for a missing SenseVoice install or for
English-only work.

### fake (installs nothing)

With `backend = "fake"` or `--engine fake`, no audio is sent and no model is loaded; the final
text is `[voice] <N> ms`. Use it to verify capture, segmentation, and the RPC path, independent
of real recognition.

## Configuration

Configuration lives in `~/.config/eix/config.toml` (overridable with `EIX_CONFIG`). Everything has a
default; only the keys you are likely to touch are listed.

| Key | Default | Meaning |
| --- | --- | --- |
| `[voice] enabled` | `true` | When off, `/voice` reports `voice is disabled in [voice]`. |
| `[voice] backend` | `"local"` | `local`, `cloud`, or `fake`. |
| `[voice] language` | `""` | Empty means auto-detect; `zh`, `en`, and so on force the engine language. |
| `[voice.local] engine` | `""` | Empty means automatic: `sherpa` when SenseVoice is installed, otherwise `whisper`. |
| `[voice.local] command` | unset | Path to `eix-asr`. When unset, eix tries `PATH`, then the `eixd` directory, then `~/.local/bin`. |
| `[voice.local] model_dir` | unset | Explicit `model.int8.onnx` or `ggml-*.bin`; `~` is not expanded. |
| `[voice.local] args` | `[]` | Escape hatch: replaces the derived arguments wholesale, though a missing `--model`/`--language` is still filled in. |
| `[voice.capture] command` | `""` | Empty tries `parec`, then `pw-record`, then `arecord`. |
| `[voice.capture] threshold` | `0.004` | RMS floor (0.0–1.0) for the speech gate. The gate itself follows the measured room at 1.6x, so noise alone cannot open an utterance. Raise it when noise still does, lower it when a quiet voice is missed. |
| `[voice.capture] silence_ms` | `800` | How much silence ends an utterance. |
| `[voice.capture] max_utterance_ms` | `60000` | Hard per-utterance ceiling, so a noisy room cannot record forever. |
| `[voice.refine] enabled` | `true` | Use a chat model to fix punctuation, typos, and filler words. Text only, never audio. |
| `[voice.refine] model` | `""` | `provider/model`; empty uses the default model. |
| `[voice.refine] timeout_ms` | `8000` | On timeout the raw text is kept, so the prompt never stalls. |

Two discovery rules are worth remembering, since they explain "why did nothing happen":

- SenseVoice counts as installed only when `tokens.txt` exists **and** `model.int8.onnx` (or
 `model.onnx`) is at least 10 MiB. Otherwise it is treated as absent and eix falls back to
  whisper.
- whisper picks the largest `*.bin` under `<data dir>/models`, because bigger models are more
  accurate.

Common edits:

```toml
# Mostly Chinese: pin the language and skip detection
[voice]
language = "zh"

# Two people share the machine; models live elsewhere
[voice.local]
model_dir = "/data/models/sense-voice/model.int8.onnx"

# The sherpa binary is not on PATH; spell everything out through args
[voice.local]
args = ["--engine", "sherpa", "--command", "/opt/sherpa/sherpa-onnx-offline",
        "--model", "/data/models/sense-voice/model.int8.onnx"]

# No cleanup pass; use the engine's output verbatim
[voice.refine]
enabled = false
```

With `backend = "cloud"`, audio leaves the machine. Keep the key out of the config: first make the
credential slot a real provider (`eix auth login` only accepts names that exist in `[providers]`,
and dotted names need quotes), then store the key:

```toml
[providers."asr.deepgram"]
base_url = "https://api.deepgram.com"
```

```sh
eix auth login asr.deepgram     # likewise asr.openai or asr.custom, each with its own [providers."…"]
```

The key lands in `<data dir>/auth/asr_deepgram.json` (dots become underscores) and is readable only
by its owner. The daemon reads it when needed, so no restart is required.

## Using it

In the TUI these are all the entry points:

| Action | Effect |
| --- | --- |
| `/voice` | Start listening; 800 ms of silence ends an utterance, and you can keep talking. |
| `/voice off` (or `/voice stop`) | Stop. |
| `/voice file PATH` | Transcribe an existing recording through the same path. |
| `F2` | Hold to talk, release to stop; on terminals without key-release events it toggles on and off. |

Results are written into the prompt, not sent directly, so you can edit them before pressing
`Enter`.

`/voice file` accepts raw PCM16 little-endian mono 16 kHz, or WAV with the same layout. A
mismatched format is reported with the actual format read, never silently resampled. Convert other
sources first:

```sh
ffmpeg -i talk.m4a -ar 16000 -ac 1 -c:a pcm_s16le talk.wav
ffmpeg -i talk.m4a -f s16le -ar 16000 -ac 1 talk.pcm
```

It also works without the TUI, through the `asr.start` / `asr.pcm` / `asr.finish` RPCs, or by pushing
audio straight to `POST /asr/{utterance_id}/pcm`. Methods and events are in [voice.md](./voice.md).

## Verifying

Run the engine on its own first; it narrows the search space considerably:

```sh
M=~/.local/share/eix/models/sense-voice
eix-asr --wav talk.wav --engine sherpa --model $M/model.int8.onnx
# {"type":"final","text":"...","words":[],"engine":"sherpa"}

eix-asr --wav talk.wav --engine sherpa --model $M/model.int8.onnx --format text
# ...
```

`--format text` prints only the text, which suits scripts. For per-utterance timing, read the
engine's own output: `sherpa-onnx-offline` prints `Elapsed seconds` and `RTF`.

## Troubleshooting

Messages and their causes:

| Symptom | Cause | Fix |
| --- | --- | --- |
| `voice is disabled in [voice]` | `[voice] enabled = false` | Remove it or set `true` |
| `no eix-asr helper found: …` | `eix-asr` is in none of the three search locations | Install into `~/.local/bin/`, or set `[voice.local] command` |
| `local asr helper /path does not exist` | `command` is wrong | Use an absolute path and confirm it is executable |
| `no recorder found: install parec, …` | No recording program | Install `pulseaudio-utils`/`pipewire`/`alsa-utils`, or set `[voice.capture] command` |
| `error while loading shared libraries: libonnxruntime.so` | Runtime and shared object were separated | Put `libonnxruntime.so` next to the binary |
| `Protobuf parsing failed` | The model download was truncated | Check the sha256 and download again |
| `the sherpa engine needs --model  …` | `--engine sherpa` without a model | Add `--model`, or drop `args` and let eix fill it in |
| `/voice` does nothing and the meter stays flat | The capture is silent, or the gate is too high | Check the input device, then lower `threshold` |
| Transcripts come back empty or as a stray `.` | The room is loud enough to open an utterance, but no voice follows it | Raise `threshold` above the room level, or quieten the microphone input |
| Speech is cut to the first few characters of each sentence | The gate is high enough to treat quiet speech as silence, ending utterances early | Lower `threshold`, or raise `silence_ms` |
| Words are right but punctuation is a mess | The refinement pass did not run | Check `degraded` on `asr.final`, and confirm `[voice.refine] enabled` and a reachable provider |

`degraded: true` is not an error: it means the refinement step failed or timed out and you are
seeing the engine's raw text.

## Resource usage

Single-core measurements on a laptop (official samples: `zh.wav` at 5.6 s, `en.wav` at 7.2 s):

| Engine | Decode time | RTF | Peak RSS |
| --- | --- | --- | --- |
| SenseVoice int8 | 120 ms / 165 ms | 0.02 / 0.023 | ~380 MB |
| whisper `ggml-base` | comparable | ~0.02 | ~285 MB |

`eix-asr` starts one process per utterance and reloads the model each time: about 0.7 s for
SenseVoice. Total latency for short utterances is therefore dominated by loading rather than
decoding, which is most noticeable when speaking continuously. Memory is a per-utterance peak and
is released when the process exits.

To cut costs further, install only `ggml-base` and accept the drop in Chinese quality; or keep
SenseVoice and raise `silence_ms` a little so each utterance holds more content and fewer processes
start.
