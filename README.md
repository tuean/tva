# TVA · macOS 语音助手

按住一个键说话，说的话实时变成文字，文字变成指令，指令交给本机的 **pi / dsh / opencli**
去执行，本机动作和结果同时显示在窗口里。

- 语音采集：AVAudioEngine（48kHz 单声道，自适应静音门限）
- 语音识别：两套可切换引擎
  - **`whisper`（推荐）**：本地 whisper.cpp（whisper-rs），**完全离线**，音频不出机器
  - `macos`：系统 Speech 框架，零模型但需要「Siri 与听写」或联网（音频会发给 Apple）
- 桌面界面：Rust + [GPUI](https://gpui.rs)（Zed 的 GPU 加速 UI 框架）
- 指令执行：优先走 `pi --mode rpc`（长驻进程、流式文本 + 工具调用事件），
  可切换 `dsh --profile headless` / `opencli` / 直接 shell

---

## 1. 架构

界面把一条语音指令拆成六个步骤实时显示，当前进行到哪一步、每一步耗时多少都看得到：

```
① 录音 ✓0.8s › ② 识别 ✓1.4s › ③ 理解 ✓ › ④ 思考 ·· 2.1s › ⑤ 执行 ✓125ms › ⑥ 完成
                                                  当前：bash 完成，继续思考 · 已用 1.2s
```

步骤来自真实信号，不是估算：

| 步骤 | 触发来源 |
|---|---|
| 录音 | 麦克风打开（AsrState::Listening） |
| 识别 | 检测到语音 / 收到 partial |
| 理解 | 识别出完整一句，进入意图路由 |
| 思考 | pi RPC 的 agent_start / thinking_delta / text_delta |
| 执行 | tool_execution_start（显示具体工具名）→ tool_execution_end 后回到思考 |
| 完成 | agent_settled + 指令结束 |

思考阶段会显示子状态：`已提交，等待模型` → `模型开始处理` → `推理中` → `bash` → `bash 完成，继续思考` → `生成回复`。

**识别引擎和执行后端都可以在界面上一键切换**（工具栏两组按钮），不用改配置重启。

实测数据（本机 Apple Silicon，release 构建，CPU 推理）：

| 引擎 / 模型 | 大小 | 解码耗时 | 离线 | 实时预览 |
|---|---|---|---|---|
| macOS Speech | 0 | 流式 | ❌（Siri 关闭时走在线） | ~200ms |
| whisper tiny | 74MB | **0.15s** | ✅ | ✅ 很快 |
| whisper base | 141MB | **0.28s** | ✅ | ✅ |
| whisper small | 465MB | **0.9s** | ✅ | 用 tiny 出预览 |

关键发现：**whisper 每次解码的耗时几乎与音频长度无关**（内部固定 30 秒窗口），
3.2 秒和 8.4 秒的音频耗时几乎一样。所以「精准」模式用 **tiny 出实时预览 + small 出最终结果**
的双通道方案，既跟手又准：

| 界面按钮 | 引擎 | 预览 | 最终结果 |
|---|---|---|---|
| 系统 Speech | SFSpeechRecognizer | 原生流式 | 原生流式 |
| whisper 极速 | tiny | tiny | tiny |
| whisper 标准 | base | tiny | base |
| whisper 精准 | small | **tiny** | **small** |

```
┌──────────────────────────── tva-app (GPUI) ────────────────────────────┐
│  header: 状态 / 电平波形 / 后端 / 按住说话 / 打断 / 清空                │
│  ┌── 实时转写 ────────────┐  ┌── 执行动作 ───────────────────────────┐ │
│  │ partial（灰）          │  │ bash  ls -1 ~/Desktop | head -5      │ │
│  │ AGENT 指令 + 回复      │  │ 输出流 / 耗时 / 成功失败             │ │
│  └────────────────────────┘  └──────────────────────────────────────┘ │
└───────────▲───────────────────────────────▲───────────────────────────┘
            │ AppEvent（async_channel）      │ AppEvent
┌───────────┴───────────┐        ┌──────────┴─────────────────────────┐
│ tva-macos             │        │ tva-agent                          │
│  AVAudioEngine tap    │        │  意图路由（本地动作 / Agent）      │
│  SFSpeechRecognizer   │        │  pi RPC(JSONL) / dsh / opencli     │
│  静音断句 / 电平      │        │  本地动作: open / search / shell   │
└───────────▲───────────┘        └──────────┬─────────────────────────┘
            │ 麦克风                        │ stdin/stdout
      CoreAudio 实时线程              pi --mode rpc（本机 Agent）
```

数据流：

1. `AVAudioEngine` 在音频线程装 tap，把 PCM 拷成纯 Rust 采样 + RMS，经通道送给识别线程
   （ObjC 对象只在识别线程里，不需要 `unsafe impl Send`）。
2. 识别线程把采样装回 `AVAudioPCMBuffer` 追加到 `SFSpeechAudioBufferRecognitionRequest`；
   同时做能量门限 + 静音计时，说话停下约 1.2s 就 `endAudio()` 断句。
3. partial 结果实时推给界面；final 结果进入 `tva-agent` 路由：
   - 「打开 XX / 搜索 XX / 执行 XX / 截图」→ 本地直接执行；
   - 其余 → 交给 Agent，Agent 自己规划并调用工具改本机。
4. Agent 的 `text_delta` / `tool_execution_*` 事件映射成 UI 的「回复」与「动作卡片」。

## 2. 目录

```
crates/
  tva-core/    事件（AppEvent）、状态机、配置
  tva-macos/   音频采集 + 实时语音识别 + 权限申请（+ examples/asr_probe 自检）
  tva-agent/   意图路由 + pi RPC / CLI 后端（+ examples/agent_probe 自检）
  tva-app/     GPUI 界面、全局快捷键、组装
resources/Info.plist   麦克风 / 语音识别用途说明
scripts/bundle.sh      打成 TVA.app（权限按 bundle 授权，必须走它）
scripts/run.sh         开发时直接 cargo run
```

## 3. 快速开始

```bash
# 1. 编译
cargo build --release -p tva-app

# 2. 创建本地签名证书（只需一次，见下文"为什么权限会反复弹"）
./scripts/setup-signing.sh

# 3. 打包成 .app
./scripts/bundle.sh release

# 4. 运行
open dist/TVA.app
```

### 为什么权限会反复弹？

macOS 的麦克风 / 语音识别授权是**按 app 身份**记的。用 `codesign --sign -`（ad-hoc）
签名的 app **没有稳定身份**，macOS 只能拿二进制的 cdhash 来认它——每重新编译一次
cdhash 就变，系统就当成"一个新 app"重新问你。

`./scripts/setup-signing.sh` 会创建一个本地自签名证书 `TVA Local Signing` 并信任它，
之后 `bundle.sh` 自动用它签名。签名后的指定要求是：

```
designated => identifier "dev.tva.voice-assistant" and certificate root = H"bfdda5e2…"
```

只跟 bundle id + 证书有关，**与代码内容无关**，所以以后随便重新编译都不会再弹。

> 切换签名身份后需要重新授权**一次**（系统会认为是新 app），之后就一直记住了。
> 想回到 ad-hoc：`TVA_SIGN_IDENTITY= ./scripts/bundle.sh`。

首次运行会弹出几个系统授权框：

- **麦克风** → 必须允许（系统设置 → 隐私与安全性 → 麦克风 → TVA）
- **语音识别** → 必须允许（系统设置 → 隐私与安全性 → 语音识别 → TVA）
- **媒体与 Apple Music** → 可以直接拒绝。这是 macOS 上 `AVAudioEngine` 启动音频
  输入时的已知连带弹窗，TVA 不读音乐库，拒绝后录音与识别完全正常。

然后按住 `Alt+Space` 说话，松手后文字会先实时出现在左边，识别出完整一句话后自动交给 Agent 执行。

调试时也可以注入一段文字代替说话：

```bash
cargo run -p tva-app -- --say "列出我桌面上的前 5 个文件名"
```

## 4. 配置

首次运行生成 `~/Library/Application Support/tva/config.toml`：

```toml
[asr]
engine = "whisper"        # macos | whisper（界面上切换会自动写回这里）
locale = "zh-CN"          # 识别语言
silence_ms = 1200         # 静音多久算说完（断句）
silence_rms = 0.012       # 静音门限的"下限"，程序会按底噪自适应抬高
min_speech_ms = 300       # 低于此时长的片段丢弃

# 仅 engine = "macos" 时生效
on_device = true          # 优先设备端离线识别
allow_server_fallback = true  # 离线不可用时回退 Apple 在线识别

# 仅 whisper 引擎
model_path = "~/Library/Application Support/tva/models/ggml-small.bin"
partial_model_path = "~/Library/Application Support/tva/models/ggml-tiny.bin"  # 预览用快模型
whisper_threads = 0       # 0 = 按 CPU 核心自动
partial_interval_ms = 1200 # 预览重解码间隔
whisper_prompt = "以下是普通话的句子。"

[agent]
backend = "pi"            # pi | dsh | opencli | shell
pi_command = "pi"
pi_args = ["--mode", "rpc", "--no-session"]
dsh_command = "dsh"
dsh_args = ["--profile", "headless"]
working_dir = "/Users/you"
timeout_secs = 300
system_prompt = "你是一个 macOS 本机语音助手…"

[hotkey]
enabled = true
push_to_talk = "Alt+Space"   # 按住说话
toggle = "Alt+Enter"         # 开 / 关

[ui]
always_on_top = true
speak_replies = false        # 预留：用 say 朗读回复
confirm_shell = false        # true 时「执行 XX」需在界面点确认
max_actions = 200
```

改完重启生效。

## 5. 执行后端

**工具栏上可以直接切换**（「执行后端」那一组），切换会写回配置、重建后端进程，正在执行的任务会被终止。

| 后端 | 命令 | 中间过程可见 | 单条耗时 | 说明 |
|---|---|---|---|---|
| **pi** | `pi --mode rpc` | ✅ 文本增量 + 工具调用 | 2~5s | 长驻进程，能实时看到 bash 等工具在跑什么 |
| **dsh** | `dsh --profile headless "<指令>"` | ❌ 只有最终答复 | 1.5~3s | 一次性任务，每句话起一个新进程 |
| opencli | `opencli …` | ❌ | — | 一次性子进程 |
| shell | `zsh -lc "<指令>"` | ❌ | 毫秒级 | **危险**：每句话直接当命令执行，切换时会弹红色提示 |

两者实测对比（同一句「列出我桌面上的前 3 个文件名」）：

```
pi：  0.0s 提交 → 1.0s 模型开始 → 3.2s bash 开始 → 3.3s bash 完成 → 5.0s 回复
dsh： 0.0s 提交 → 2.6s 直接给出回复（中间看不到工具调用）
```

dsh 的 headless profile 设计上只把**最后一条非空 assistant 消息**写到 stdout
（见其 README 的 "One submitted task only"），所以步骤条在 dsh 模式下会停在
「④ 思考 · 计时中」，结束时直接跳到「⑥ 完成」——这是后端能力决定的，不是 UI 偷懒。
想看到工具级细节就用 pi。

pi 的 RPC 协议见 `@earendil-works/pi-coding-agent/docs/rpc.md`；本项目只用到
`prompt` / `abort` 两个命令，以及 `message_update`、`tool_execution_*`、
`message_end`、`agent_settled` 事件。

## 6. 切换到本地 whisper 引擎

```bash
# 1. 下载模型（走 hf-mirror，GitHub 不通也能下）
./scripts/download-model.sh small          # 或 large-v3-turbo

# 2. 改配置
#   ~/Library/Application Support/tva/config.toml
#   [asr]
#   engine = "whisper"
#   model_path = "~/Library/Application Support/tva/models/ggml-small.bin"

# 3. 重新打包运行
./scripts/bundle.sh release && open dist/TVA.app
```

whisper 不是流式模型，本项目的做法是：说话过程中每 0.9s 把"到目前为整段音频"
重新解码一次当实时预览（只解码最近 10 秒），停顿超过 silence_ms 再解码一次作为最终结果。
解码跑在独立线程上，不阻塞采集。

模型下载地址（如果脚本里的镜像不通，可以自己换）：

```
https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-small.bin
```

## 7. 自检工具

```bash
# 语音链路：打印权限状态、识别器能力、实时电平与转写
cargo run -p tva-macos --example asr_probe -- 15 zh-CN

# Agent 链路：把一句话当成语音指令，打印完整执行过程
cargo run -p tva-agent --example agent_probe -- "列出我桌面上的前 5 个文件名"

# 指定后端（pi / dsh / opencli / shell），或测试运行时切换
cargo run -p tva-agent --example backend_probe -- dsh "只回复四个字：链路正常"
cargo run -p tva-agent --example backend_probe -- switch

# 本地 whisper：直接对一个音频文件转写（自动用 ffmpeg 重采样）
say -v Tingting -o /tmp/t.aiff "你好，请帮我列出桌面上的文件"
cargo run -p tva-macos --example whisper_file -- /tmp/t.aiff
```

## 8. 已知限制

- **设备端离线识别**依赖系统「Siri 与听写」已开启。未开启时系统会返回
  `kLSRErrorDomain/201`，程序会自动回退到在线识别（音频会发到 Apple），界面会提示。
- 本机没装完整 Xcode（只有 CommandLineTools）时，`xcrun metal` 不存在，
  gpui 必须开 `runtime_shaders` 特性（已在 workspace 里配置）才能在运行时编译 Metal 着色器。
- 语音识别基于系统 Speech 框架，不是 whisper.cpp：中文标点、热词由系统决定；
  需要完全离线自有模型时，可在 `tva-macos` 里按同一套 `RecognizerHandle` 接口接 whisper-rs。
- 目前语音指令是「一句话一个任务」，不支持「停一下」「取消」这类对话式打断；
  打断靠界面按钮或 `Alt+Enter` 关掉聆听。

## 9. 下一步

- [ ] 界面内文本输入框（不说话也能下指令）
- [ ] 用 `say` 朗读 Agent 回复（配置项已预留）
- [ ] 热词表 / 上下文串（`contextualStrings`）提升专有名词识别
- [ ] 多轮会话视图（当前只展示最近若干条）
- [ ] 开机自启 + 菜单栏图标
