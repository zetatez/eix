# Voice input

Speech-to-text is off by default. When enabled, the client captures audio, the
daemon owns the recognition session, and text arrives as ordinary events. The
split is deliberate: capture needs a device the daemon must not touch, and the
engine choice should not change the client.

```toml
[voice]
enabled = true
backend = "local"  # or "cloud", or "fake" for a key-free smoke test
```

For the cloud backend, keep the key out of the config file:

```sh
eix auth login asr.deepgram
```

`[voice.cloud] credential` overrides the default slot.

## Backends

### `local`

The daemon spawns `[voice.local] command` once per utterance, writes raw PCM16
little-endian mono 16 kHz to its stdin, and reads one JSON object per line from
its stdout:

```text
{"type":"partial","text":"hel","stable_len":0}
{"type":"final","text":"hello","words":[]}
{"type":"error","kind":"unavailable","message":"no model"}
```

`stable_len` counts the characters of `text` that earlier partials already
agreed on. The UI renders that prefix normally and the rest dimmer; a helper
that cannot report it sends `0`, and the client replaces the whole line each
time.

The daemon passes `--protocol eix-asr/1 --rate 16000`, plus `--language` when
`[voice] language` is set. Keeping ONNX Runtime and whisper out of this binary
is what lets the default build stay free of C++ toolchains.

### `cloud`

One HTTPS request per utterance. The query asks for the canonical layout
(`encoding=linear16`,  `sample_rate=16000`,  `channels=1`,  `punctuate=true`) so the
endpoint does not fall back to telephony audio, and a configured `wss://`
listen endpoint has its scheme rewritten for the request. Two response shapes
are understood:

- Deepgram: `results.channels[0].alternatives[0].transcript` and `.words`.
- OpenAI-compatible: a top-level `text`.

There are no partials; the capability flags returned by `asr.start` say so.

### `fake`

No device, model or network. The final text is `[voice] <N> ms`, enough to
exercise a client end to end.

## Protocol

## Dictation in the TUI

There is no separate voice client: `/voice` in the normal TUI owns the
microphone and writes into the prompt like typing does.

- `/voice` starts listening. Speech opens an utterance (`asr.start`), audio is
  sent in 100 ms batches (`asr.pcm`), silence closes it (`asr.finish`), and the
  next sentence starts a new one without re-arming.
- `/voice off` stops; `/voice file PATH` transcribes raw PCM16 mono 16 kHz or a
  matching WAV file through the same path.
- The statusline shows a `mic ▮▮▮` level meter while capture runs.

Capture shells out to the first recorder found on `PATH` (`parec`,
`pw-record`, then `arecord`), so no audio crate is needed. Override it with
`[voice.capture] command`. Segmentation is energy based: `threshold` is the
RMS that counts as speech, `silence_ms` ends an utterance, and
`max_utterance_ms` stops one that never goes quiet. A quiet room matters;
raise `threshold` if background noise keeps the utterance open.

| Method | Params | Result |
| --- | --- | --- |
| `asr.start` | `session_id?` | `utterance_id`,  `format`,  `caps`,  `backend` |
| `asr.pcm` | `utterance_id`,  `pcm`  (base64) | `bytes`  (utterance total) |
| `asr.finish` | `utterance_id` | `ok` |
| `asr.cancel` | `utterance_id` | `ok` |
| `asr.status` | `utterance_id` | `bytes`, or `null` once finished |

`POST /asr/{utterance_id}/pcm` takes the same audio as a raw body, with either
`Content-Length` or `Transfer-Encoding: chunked`. Use it when base64 in JSON is
too expensive; the capture loop sends one chunk per 100 ms.

Audio must be PCM16, mono, 16 kHz. Odd lengths and other formats are rejected
with `invalid_params`, since silently resampling would hide a capture bug.

## Events

Events share the existing `/events` stream:

- `asr.partial`:  `{seq, text, stable_len}`.
- `asr.final`:  `{seq, text, words, speaker, degraded, interim}`.
- `asr.overrun`:  `{seq, dropped_ms}` when the engine fell behind and audio
  was dropped.
- `asr.error`:  `{seq, kind, message, retryable}`.

`seq` is monotonic per utterance. `interim: true` marks a final produced by a
cancel or a dropped stream: render it, but do not treat the utterance as
complete. `degraded: true` means the text is usable but a second pass failed.

One utterance is capped at five minutes of audio and 1 MiB per chunk.
