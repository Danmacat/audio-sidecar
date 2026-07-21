# audio-sidecar 协议规范（protocolVersion = 1）

与主进程（Electron/Node）的协商接口。类型定义的机器可读版本在 [`bindings/`](./bindings)（由 Rust 源码经 ts-rs 生成，`cargo test --features ts-export` 重新生成），本文档描述类型之外的语义：帧格式、顺序保证、背压、生命周期。

## 1. 传输与帧格式

- 传输：sidecar 子进程的 **stdin/stdout**，NDJSON——每行一个 UTF-8 JSON 对象，`\n` 结尾（输入行兼容 `\r\n` 与 BOM）。**日志只走 stderr**，stdout 永远只有协议帧。
- 单行上限默认 1 MiB（`--max-line-bytes` 可调），超限回 `parseError`。
- 协议按传输无关设计，未来可在 `--transport` 后加 WebSocket 通道，帧格式不变。

三种消息形态，按字段判别：

| 形态 | 方向 | 结构 |
|---|---|---|
| 请求 | 宿主 → sidecar | `{"id": 1, "method": "capture.start", "params": {...}}`（`id` 为数字或字符串，必填；`params` 可省略） |
| 响应 | sidecar → 宿主 | `{"id": 1, "result": {...}}` 或 `{"id": 1, "error": {"code": "...", "message": "...", "data"?, "retryable"?}}` |
| 事件 | sidecar → 宿主 | `{"event": "capture.spectrum", "data": {...}}`（sidecar 永不主动发"请求"） |

**顺序与并发**：请求并发处理，响应可乱序——按 `id` 关联。可靠消息（全部响应 + `capture.state` / `device.*` / `media.*` / `sidecar.exiting`）之间保序且不丢；`capture.spectrum` / `capture.pcm` 走**有界帧通道**（256 深），宿主读得慢时**丢帧不阻塞**，靠 `seq` 空洞感知（丢帧计数见 `capture.list` 的 `stats.framesDropped`）。

**版本规则**：双方忽略未知字段/未知事件（serde 与宿主侧都应如此）。新增方法、事件、可选字段、capability 不升 `protocolVersion`；破坏性变更才升。平台差异只通过 `hello` 的 `capabilities` 表达，调用不支持的方法返回 `unsupported` 而非崩溃。

## 2. 生命周期

- 宿主 `spawn(audio-sidecar.exe)`，建议参数：`--log-level info`（或 `--log-file <path>`）。
- 可选启动参数：
  - `--capture <json>`（可重复）：启动即发起捕捉的语法糖，值为 `CaptureSource`（如 `{"type":"defaultOutput"}`）或完整 `CaptureStartParams`（含 spectrum/pcm 配置）。等价于宿主发 `capture.start`，captureId 通过 `capture.state` 事件与 `capture.list` 获知；单条失败只记日志不致命。
  - `--artwork-dir <path>`：开启封面落盘（见 §4a）。
- 启动即用：无须等待任何 ready 信号，直接发 `hello`。（Windows 上启动时会主动推一次 `media.sessionsChanged` 全量快照。）
- **stdin EOF（宿主退出/管道断开）→ sidecar 优雅退出**；也可显式调 `shutdown`（响应刷出 → `sidecar.exiting` 事件 → 清理 → exit 0）。清理挂死时 3 秒看门狗强杀（exit 1）。
- CLI：`--print-hello` 打印 hello 结果后退出（打包后冒烟检查用）。

## 3. 方法

完整参数/返回类型见 `bindings/*.ts`；此处列语义要点。

| 方法 | 说明 |
|---|---|
| `hello` | `{client?: {name, version}}` → `HelloResult`：`protocolVersion`、`platform`、`osVersion`、`pid`、`capabilities`、`limits`。宿主应先 feature-detect。 |
| `ping` | 心跳，回 `{}`。 |
| `devices.list` | `{kinds?: ("render"\|"capture")[]}` → 全状态设备列表（active/disabled/unplugged/notPresent 由 `state` 区分）；`format` 仅 active 设备有。默认设备排前。 |
| `devices.getDefault` | `{kind, role? = "multimedia"}` → 当前默认设备。 |
| `processes.listAudio` | 列出**有音频会话的进程**（按 pid 聚合，active 优先排序，排除系统音 pid 0）——进程捕捉选择器的数据源。注意浏览器等应用的音频常在**子进程**渲染，此列表给出的正是实际发声的 pid。 |
| `capture.start` | `{source, spectrum?, pcm?}`。`source` 联合类型见 §5。`spectrum`/`pcm` 所有字段可省略（TS 侧可传 `Partial<SpectrumConfig>`），解析后的最终值在结果中回显。**等捕捉线程真正就绪才回包**（≤5s），结果含 `captureId`、实际 `format`。`spectrum.enabled=false && pcm.enabled=false` → `invalidParams`。 |
| `capture.stop` | `{captureId}` → `{}`。对 `failed` 状态的会话调用可将其从列表清除。 |
| `capture.list` | 全部会话及统计：`framesEmitted/framesDropped/pcmChunksEmitted/ringOverflows/starvedTicks/restarts`。 |
| `media.getSessions` | 系统媒体会话全量快照 + `currentSessionId`。 |
| `media.getCurrent` | 当前会话或 `null`。 |
| `media.getArtwork` | `{sessionId, maxBytes? = 2000000}` → `{contentType, byteLength, dataBase64}`。3s 超时；空流自动重试一次（部分应用切歌后封面晚到）。错误：`sessionNotFound` / `artworkUnavailable` / `artworkTooLarge`（`data.byteLength` 告知实际大小）。 |
| `shutdown` | 优雅退出。 |

**capabilities**（Windows 全量为 true；`processLoopback*` 需 Win10 build 19041+）：`deviceCapture, deviceLoopback, followDefaultOutput, followDefaultInput, processLoopback, processLoopbackExclude, audioProcessList, deviceEvents, mediaSessions, mediaArtwork, spectrum, pcmStream`。
**limits**：`maxCaptures: 8, maxFps: 60, maxBands: 256, fftSizes: [512..8192], pcmFormats: ["s16le","f32le"]`。

## 4. 事件

| 事件 | 语义 |
|---|---|
| `device.added` / `device.removed` / `device.stateChanged` | 热插拔与状态变化（非 active 状态的被捕捉设备会自动触发重启逻辑）。 |
| `device.defaultChanged` | `{kind, role, deviceId\|null}`，100ms 去抖，所有 role 都转发。 |
| `capture.state` | `starting → running →（restarting ⇄ running）→ stopped / failed`。`running` 带 `deviceId`+`format`（重启后可能变化！）；`restarting`/`failed` 带 `reason: {code, message}`，code ∈ `deviceInvalidated / deviceRemoved / defaultDeviceChanged / processExited / initFailed / timeout / panic / threadDied`。**进程退出 → `failed`，不自动重启**（宿主决策）；设备失效/移除/默认切换 → 自动重启（退避 200ms→5s，无限重试，稳定 30s 后计数复位）。 |
| `capture.spectrum` | 见 §6。 |
| `capture.pcm` | 见 §7。 |
| `media.sessionsChanged` | 会话增删时的全量快照（启动时也发一次）。 |
| `media.currentChanged` | 当前会话切换。 |
| `media.sessionUpdated` | `{session, changed: ("mediaProperties"\|"playbackInfo"\|"timeline"\|"artwork")[]}`。**纯 timeline 更新每会话合并至 ≤2 次/秒**；未开封面落盘时，`mediaProperties` 变化且 `artworkAvailable=true` 应重新调 `media.getArtwork`；开了落盘则等 `changed=["artwork"]` 事件即可（见 §4a）。 |
| `sidecar.exiting` | `{reason: "shutdown"\|"stdinClosed"\|"fatal"}`。 |

## 4a. 封面落盘（`--artwork-dir`）

以该参数启动后，sidecar 在切歌/会话出现时自动抓取封面并**原子写入**（临时文件 + rename）目录，文件名为**内容哈希**：`<fnv1a64hex>.<jpg|png|bmp|gif|webp|img>`（扩展名按魔数嗅探）。语义：

- `MediaSession` 增加 `artworkFile`（绝对路径）与 `artworkHash` 两个字段；未开启时恒为 `null`。
- 封面就绪/变化时推 `media.sessionUpdated`，`changed=["artwork"]`——**哈希变了才推**，同一张专辑封面跨曲目不会重复通知、也只落盘一次（内容寻址天然去重）。
- 渲染进程可直接以 `file://` 引用 `artworkFile`，无需经主进程转发图片数据；`media.getArtwork`（base64）仍可用。
- 抓取相对元数据事件是**异步**的：先收到 `mediaProperties` 更新（此时 `artworkFile` 可能还是旧值或 null），随后收到 `artwork` 更新。换曲期间过期的抓取结果会被自动丢弃。
- sidecar 不清理目录（缓存语义）；宿主可按需清理，正在引用的文件不删即可。

## 5. 捕捉源（CaptureSource）

```ts
{ type: "device", deviceId }          // render 设备 → WASAPI loopback；capture 设备 → 普通采集
{ type: "defaultOutput" }             // 跟随默认输出（multimedia role），默认设备变更自动无缝切换
{ type: "defaultInput" }              // 跟随默认输入
{ type: "process", pid }              // 捕捉该进程及其子进程树的声音（Win10 2004+）
{ type: "systemExcludingProcess", pid } // 捕捉除该进程树之外的全系统声音（如：排除宿主自身）
```

> Windows 的进程 loopback 只有"包含进程树 / 排除进程树"两种模式，不存在"仅该进程不含子进程"。浏览器音频在 utility 子进程渲染——用根 pid 也能捕到（含树），或直接用 `processes.listAudio` 给出的实际发声 pid。

**多路捕捉**：`capture.start` 可并发调用多次（上限 `limits.maxCaptures`，默认 8），每路独立线程、独立配置，`capture.spectrum`/`capture.pcm` 事件按 `captureId` 分流。同时捕多个进程/多个设备无需多个 sidecar 进程。

## 6. 频谱（capture.spectrum）

`{captureId, seq, timestampMs, rms: number[/*每声道*/], bands: number[声道][band]}`

- `bands[channel][band]`：0..1，**低频 → 高频**，3 位小数，声道 0 = 左。默认 2×64 band ≈ Wallpaper Engine `wallpaperRegisterAudioListener` 的 128 值布局（`[...bands[0], ...bands[1]]` 摊平即可；WE 的右声道倒序镜像由宿主自行排列）。
- 处理链：fftSize 滑动窗（默认 2048）→ Hann → 实数 FFT → 对数分 band（默认 25Hz–16kHz，`maxFreq` 钳到 0.95×Nyquist）→ autoGain（5s 峰值跟踪归一到 0.9，**-80dBFS 噪声门**保证静音时归零）→ 标度（默认 dB，地板 -60）→ 每 band 攻击/衰减平滑（30fps 基准 attack 0.8 / decay 0.2，按实际 fps 自动换算）。
- 静音期（WASAPI 不投包）按 tick 补零 → 频谱经衰减平滑自然回落（约 0.3–0.5s 降到 0），不会冻结在最后一帧。
- 带宽参考：30fps 立体声 64 band ≈ 27 KB/s。

配置建议：跟随 WE 观感用默认值即可；要"原始幅度"用 `{scale:"linear", autoGain:false, gain:N}`。

## 7. PCM（capture.pcm）

`{captureId, seq, timestampMs, firstSampleIndex, sampleRate, channels, format, dataBase64}`

- 默认关闭；`format`: `s16le`（默认，48k 立体声 ≈ 192KB/s base64 前）或 `f32le`。交错声道，小端。
- 按**样本数**切块（`chunkMs`，默认 50ms），无钟漂积累；`firstSampleIndex` 为自捕捉起的累计帧号，静音期以零样本无缝填充——拼接 `dataBase64` 即得连续音频流。

## 8. 错误码

`parseError, invalidRequest, methodNotFound, invalidParams, unsupported, deviceNotFound, processNotFound, activationFailed, captureNotFound, captureLimitReached, sessionNotFound, artworkUnavailable, artworkTooLarge, timeout, osError, internal`。
`retryable: true` 表示宿主可稍后重试（激活失败/超时等瞬态；sidecar 内部已对激活失败做 3 次×150ms 重试）。

## 9. 宿主集成示例（Electron 主进程）

```ts
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import type { Event, HelloResult, CaptureStartParams } from "./bindings"; // 由 bindings/ 目录聚合导出

const sidecar = spawn(sidecarPath, ["--log-level", "info"], { stdio: ["pipe", "pipe", "pipe"] });
createInterface({ input: sidecar.stdout }).on("line", (l) => {
  const msg = JSON.parse(l);
  if (msg.event) onEvent(msg as Event);       // 频谱帧转发给渲染进程
  else settlePending(msg.id, msg);            // 响应按 id 结算
});
sidecar.stderr.pipe(logStream);               // 日志
// 退出时无须显式清理：主进程退出 → stdin EOF → sidecar 自动退出
```

## 10. 平台矩阵

| 能力 | Windows | Linux（规划） | macOS（规划） |
|---|---|---|---|
| 设备 loopback/采集 | ✅ WASAPI | PulseAudio/PipeWire monitor | Core Audio |
| 进程捕捉（含/排除） | ✅ Win10 2004+ | `set_monitor_stream`（无排除模式） | 14.4+ process tap |
| 设备枚举+热插拔 | ✅ | subscribe 掩码 | 属性监听 |
| 媒体会话（只读+事件） | ✅ SMTC | MPRIS (zbus) | MediaRemote-adapter（best-effort） |
| 封面 | ✅ base64 | `file://` 内联 / http 回 `artworkUrl` | 视情况 |

未实现平台一律 `capabilities=false` + `unsupported`，协议不变。
