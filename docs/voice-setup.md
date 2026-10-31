# 语音输入配置指南

这篇讲怎么把 `/voice` 用起来：装哪个引擎、模型放哪、配置怎么改、出问题看什么。
协议细节（JSON-RPC 方法、事件字段、helper 输出格式）在
[voice.md](./voice.md)，这里只讲使用。

默认值是可以直接跑的状态：`[voice] enabled = true` 不需要改，静默会话不会占用麦克风。
真正要做的只有两件事，装识别引擎和它的模型。两件都装好后无需再配置。

## 先看清楚边界

- 录音由前端做（TUI 里的 `/voice`），daemon 从不打开音频设备。
- 识别由 `eix-asr` 子进程做，每说完一句起一个，通过 stdin 收 PCM、stdout 回 JSON。
- 只有识别出的文字会经过一次可选的模型润色；音频永远不出本机（`backend = "local"` 时）。
- 模型文件放在数据目录，不进配置文件。数据目录是 `~/.local/share/eix`
  （macOS 为 `~/Library/Application Support/eix`），`EIX_DATA_DIR` 可整体覆盖。

## 快速开始

确认三个前置已就位，然后 `/voice`：

```sh
eix-asr --list-engines          # fake / whisper / command / sherpa
sherpa-onnx-offline --help      # 或者 whisper-cli --help
ls ~/.local/share/eix/models/sense-voice/   # model.int8.onnx tokens.txt
```

```sh
cargo build --release -p eix-asr            # make install 不含它
install -m 0755 target/release/eix-asr ~/.local/bin/
```

然后输入 `/voice`，状态栏出现 `mic ▮▮▮` 电平条，说话即可。引擎和模型是每次
`asr.start` 现找的，换了模型或二进制不用重启；改 `config.toml` 用 `/reload` 让 daemon 重新读。

## 安装识别引擎

两个引擎任选其一；同时装了会优先 SenseVoice。中文场景下这个选择很重要：
同一段中文，`ggml-base.bin` 会把它转成英文臆想句
（`Time for the opening. Find 9.3.5.`），SenseVoice 给出正确结果。

### SenseVoice（推荐，中英日韩粤）

需要两样东西：`sherpa-onnx-offline` 可执行文件，和一套 SenseVoice 模型。

运行时用 pip 包里的 CLI。系统 Python 若受 PEP 668 保护（Arch、新版 Debian 都是），
装进独立 venv 最省事：

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

关键在于 `libonnxruntime.so` 必须和二进制同目录：pip 打好的 rpath 里带
`$ORIGIN`，同目录即可解析，不需要 `patchelf`。分开放会得到
`error while loading shared libraries: libonnxruntime.so`。

也可以用发行版包或 GitHub release 里的
`sherpa-onnx-v2.0.1-linux-x64-shared.tar.bz2`，只要能提供
`sherpa-onnx-offline` 即可。

模型 228 MiB，两个文件必须同目录，`tokens.txt` 就在模型旁边：

```sh
mkdir -p ~/.local/share/eix/models/sense-voice
cd ~/.local/share/eix/models/sense-voice
B=https://huggingface.co/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/main
curl -fL -O $B/model.int8.onnx -O $B/tokens.txt
```

下完必须校验。截断的 ONNX 在加载时报 `Protobuf parsing failed`，
而它停在这一句之前不会回落到 whisper：

```sh
sha256sum model.int8.onnx tokens.txt
# c71f0ce00bec95b07744e116345e33d8cbbe08cef896382cf907bf4b51a2cd51  model.int8.onnx
# f449eb28dc567533d7fa59be34e2abca8784f771850c78a47fb731a31429a1dc  tokens.txt
wc -c model.int8.onnx   # 239233841
```

网络不稳时用可续传的方式拿，别硬等单流：

```sh
curl -fL -C - --retry 5 -o model.int8.onnx $B/model.int8.onnx
```

### whisper（备选，体积小、中文弱）

需要 `whisper-cli` 加一个 `ggml-*.bin`。装法随发行版：Arch 有 `extra/whisper-cpp`，
其它平台按 [whisper.cpp](https://github.com/ggml-org/whisper.cpp) 的构建说明自己编译
（它的 release 页只有源码，没有现成二进制）。只要最终 `whisper-cli` 在 `PATH` 上即可。

```sh
mkdir -p ~/.local/share/eix/models
curl -fL -o ~/.local/share/eix/models/ggml-base.bin \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin
```

`ggml-base.bin` 是 141 MiB，比 SenseVoice 的 228 MiB 小一半，峰值内存约 285 MB。
但中文基本不可用，适合当 SenseVoice 没装好时的兜底，或者只做英文。

### fake（不装任何东西）

`backend = "fake"` 或 `--engine fake` 不发音频、不加载模型，final 文本是
`[voice] <N> ms`。用来确认采集、分段、RPC 通路是否正常，跟真实识别无关。

## 配置

配置在 `~/.config/eix/config.toml`（`EIX_CONFIG` 可覆盖），全部有默认值，
下面只列会动到的键：

| 键 | 默认 | 说明 |
| --- | --- | --- |
| `[voice] enabled` | `true` | 关掉后 `/voice` 报 `voice is disabled in [voice]`。 |
| `[voice] backend` | `"local"` | `local`、`cloud`、`fake`。 |
| `[voice] language` | `""` | 空=自动检测；填 `zh`、`en` 等可强制引擎语言。 |
| `[voice.local] engine` | `""` | 空=自动：装了 SenseVoice 用 `sherpa`，否则 `whisper`。 |
| `[voice.local] command` | 未设 | `eix-asr` 的路径。未设时依次找 `PATH`、`eixd` 同目录、`~/.local/bin`。 |
| `[voice.local] model_dir` | 未设 | 显式指定 `model.int8.onnx` 或 `ggml-*.bin`；`~` 不会展开。 |
| `[voice.local] args` | `[]` | 逃生口：整段替换推导出的参数，`--model`/`--language` 缺失时仍会补。 |
| `[voice.capture] command` | `""` | 空=依次找 `parec`、`pw-record`、`arecord`。 |
| `[voice.capture] threshold` | `0.004` | RMS 门限（0.0–1.0）。背景噪声让一句话关不上时才调高。 |
| `[voice.capture] silence_ms` | `800` | 静音多久算一句说完。 |
| `[voice.capture] max_utterance_ms` | `60000` | 单句硬上限，防止噪声环境里一直录。 |
| `[voice.refine] enabled` | `true` | 用聊天模型修标点、错别字、口癖。只发文本，不发音频。 |
| `[voice.refine] model` | `""` | `provider/model`；空=默认模型。 |
| `[voice.refine] timeout_ms` | `8000` | 超时则保留原文，不会卡住输入框。 |

自动发现的两条规则值得记住，因为它们解释“为什么没生效”：

- SenseVoice 只有在 `tokens.txt` 存在、且 `model.int8.onnx`（或 `model.onnx`）
  至少 10 MiB 时才算装上；否则按未安装处理，回落 whisper。
- whisper 取 `<数据目录>/models` 下最大的 `*.bin`，因为更大的模型更准。

几个常见改法：

```toml
# 中文为主，锁定语言省掉检测
[voice]
language = "zh"

# 两个人共用一台机器，模型放在别处
[voice.local]
model_dir = "/data/models/sense-voice/model.int8.onnx"

# sherpa 二进制不在 PATH 上，用 args 全量指定
[voice.local]
args = ["--engine", "sherpa", "--command", "/opt/sherpa/sherpa-onnx-offline",
        "--model", "/data/models/sense-voice/model.int8.onnx"]

# 不需要润色，想要引擎原样输出
[voice.refine]
enabled = false
```

`backend = "cloud"` 时音频会离开本机。密钥不写进配置，先让凭据槽成为合法的
provider（`eix auth login` 只接受 `[providers]` 里已存在的名字，点号要用引号），
再存 key：

```toml
[providers."asr.deepgram"]
base_url = "https://api.deepgram.com"
```

```sh
eix auth login asr.deepgram     # 或 asr.openai、asr.custom，同理先配 [providers."..."]
```

key 落在 `<数据目录>/auth/asr_deepgram.json`（点号换成下划线），只有属主可读。
daemon 用到时才读，改完不必重启。

## 使用

在 TUI 里，全部入口就这几个：

| 操作 | 效果 |
| --- | --- |
| `/voice` | 开始听，静音 800 ms 自动切句，可以连续说。 |
| `/voice off`（或 `/voice stop`） | 停止。 |
| `/voice file PATH` | 转写已有录音，走同一条通路。 |
| 按住 `Ctrl-g` | 按住说，松开停；终端不支持按键释放事件时变成按一下开、再按一下停。 |

识别结果写进输入框，不直接发送，可以先改再 `Enter`。

`/voice file` 接受裸 PCM16 小端单声道 16 kHz，或同样布局的 WAV；
格式不符会明确报出实际读到的格式，而不是偷偷重采样。其它来源先转一遍：

```sh
ffmpeg -i talk.m4a -ar 16000 -ac 1 -c:a pcm_s16le talk.wav
ffmpeg -i talk.m4a -f s16le -ar 16000 -ac 1 talk.pcm
```

不经过 TUI 也能用：`asr.start` / `asr.pcm` / `asr.finish` 三个 RPC，或
`POST /asr/{utterance_id}/pcm` 直接推音频。方法表和事件在 [voice.md](./voice.md)。

## 验证

先脱离 eix 单独跑通引擎，排错范围会小很多：

```sh
M=~/.local/share/eix/models/sense-voice
eix-asr --wav talk.wav --engine sherpa --model $M/model.int8.onnx
# {"type":"final","text":"开饭时间早上9点至下午5点。","words":[],"engine":"sherpa"}

eix-asr --wav talk.wav --engine sherpa --model $M/model.int8.onnx --format text
# 开饭时间早上9点至下午5点。
```

`--format text` 只打文字，适合脚本里用。想看每句耗时加上引擎自己的输出即可：
`sherpa-onnx-offline` 会打印 `Elapsed seconds` 和 `RTF`。

## 排错

报错文案和对应的原因：

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| `voice is disabled in [voice]` | `[voice] enabled = false` | 删掉或改 `true` |
| `no eix-asr helper found: ...` | `eix-asr` 不在三个搜索位置 | `install` 到 `~/.local/bin/`，或设 `[voice.local] command` |
| `local asr helper /path does not exist` | `command` 写错 | 用绝对路径并确认可执行 |
| `no recorder found: install parec, ...` | 没有录音程序 | 装 `pulseaudio-utils`/`pipewire`/`alsa-utils`，或设 `[voice.capture] command` |
| `error while loading shared libraries: libonnxruntime.so` | 运行时和 so 分开放了 | 把 `libonnxruntime.so` 放到二进制同目录 |
| `Protobuf parsing failed` | 模型下载被截断 | 核对 sha256 后重下 |
| `the sherpa engine needs --model ...` | 用了 `--engine sherpa` 但没给模型 | 加 `--model`，或删掉 `args` 让 eix 自动补 |
| `/voice` 无反应、电平条不动 | 录到的是静音，或门限过高 | 检查输入设备；再降 `threshold` |
| 说出来只识别到每句开头几个字 | 门限偏高，轻声被当成静音，句子被提前切断 | 调低 `threshold`，或加长 `silence_ms` |
| 文字对了但标点很乱 | 润色没生效 | 看 `asr.final` 的 `degraded` 字段；确认 `[voice.refine] enabled` 和 provider 可达 |

`degraded: true` 不是错：它表示润色那一步失败或超时，你看到的是引擎原文。

## 资源占用

一台笔记本单核实测（`zh.wav` 5.6 s、`en.wav` 7.2 s 官方样本）：

| 引擎 | 解码耗时 | RTF | 峰值 RSS |
| --- | --- | --- | --- |
| SenseVoice int8 | 120 ms / 165 ms | 0.02 / 0.023 | ~380 MB |
| whisper `ggml-base` | 相近 | ~0.02 | ~285 MB |

`eix-asr` 每句起一个进程，每次都要重新加载模型：SenseVoice 约 0.7 s。
所以短句的总延迟主要来自加载，而不是解码；连着说话时体会最明显。
内存也是按句计的峰值，进程退出即释放。

想更省：只装 `ggml-base` 并接受中文质量下降；或者保持 SenseVoice 但把
`silence_ms` 调长一点，让一句话包含更多内容，从而减少进程启动次数。
