# 跨平台移植指南（Linux / macOS）

本文档是平台适配工作的**权威依据**：目标不变量、实施方案、验收标准、已踩过的坑。
换机器/换平台开发时，以本文档 + `PROTOCOL.md` + 现有 Windows 实现为准，防止目标漂移。

## 文档效力分级（先读这个）

本文档两类内容效力不同：

- **不可协商**：§0 目标、§1 统一性契约、§5 验收标准、`PROTOCOL.md` 全部。改这些需要与项目所有者重新确认。
- **推荐路径**：§3/§4 的具体技术选型（crate、API、方案）。这些基于 **2026-07 的文档调研**，
  **未经目标平台真机验证**。它们是默认起点，不是教条——实机不符时，实施者应当**主动寻找并验证替代方案**，
  而不是硬磕本文档写下的路线。判断替代方案是否合格的唯一标准：满足 §1 契约 + 通过 §5 验收 + capability 如实。

**实施纪律**：
1. **风险探针先行**：动手全量实现前，先对 §3a/§4a 列出的高风险假设各写一个最小可运行验证（几十行的 spike），
   确认真机行为再铺开。假设塌了 → 立即转入替代路线，沉没成本为零。
2. **受阻即换道**：推荐路径卡住超过约一个工作日，停止死磕——这些领域（尤其 PipeWire 与 macOS 私有 API）
   社区方案迭代很快，检索当下最新的做法（关键词见各节），用 spike 验证后采纳。
3. **回写义务**：无论采纳还是否决了某条路线，把结论（含"为什么不行"）写回本文档并提交——
   本文档是活文档，它的价值取决于是否反映真机事实。

## 0. 项目目标（不可漂移的原点）

为 Electron 直播工具（danmacat-desktop）提供音频 sidecar，四项能力：

1. 指定输出/输入设备的 loopback/采集（含跟随默认设备）
2. 指定进程的音频捕捉（+"全系统排除某进程"）
3. 设备枚举 + 热插拔/默认变更事件
4. 系统媒体会话**只读**（元数据/封面/状态/进度 + 事件；**明确不做播放控制**——用户已决策）

数据形态：sidecar 内置 DSP 推送 WE 风格频谱帧（默认 2×64 band @30fps）+ 可选 PCM。
IPC：stdio NDJSON，协议传输无关。以上均为用户确认过的决策，改动需重新确认。

## 1. 统一性契约 —— 各平台必须逐字一致的部分

以下代码**平台无关，禁止在平台移植中复制或分叉**，新平台只做 `capture/<os>/` 与 `media/<os>.rs` 两块：

- `protocol/`（全部 wire 类型 + ts-rs 绑定）、`rpc/`、`capture/manager.rs`（状态机/重启退避）、`capture/session.rs`（DSP worker）、`dsp/`、`util/`
- 频谱观感必须一致：同一段音频在三平台产出的 band 值应当可比（同一 DSP 链保证）；**静音时频谱须在 ~0.5s 内平滑归零**（依赖"饥饿 tick 补零"规则——平台 io 线程停止投样本即可，勿在平台层自造静音逻辑）
- 事件语义一致：`capture.state` 状态机（starting→running→restarting⇄running→stopped/failed）、timeline 事件 ≤2 次/秒节流、帧通道可丢+seq 空洞、封面 generation 防串曲
- **capability 诚实原则**：做不到的能力如实报 false + 调用回 `unsupported`，绝不降级模拟；平台差异只允许通过 `hello.capabilities` 表达
- 生命周期：stdin EOF → 优雅退出；日志只走 stderr；stdout 单写者

## 2. 平台能力对照（预期）

| 能力 | Windows✅ | Linux | macOS |
|---|---|---|---|
| 设备枚举/热插拔/默认跟随 | ✅ | ✅ 等效 | ✅ 等效 |
| 输出 loopback | ✅ | ✅ monitor source | ⚠️ 14.4+（CA tap）；13.x 可 SCK 兜底 |
| 进程捕捉 | ✅ 自动含子进程树 | ⚠️ 按 sink-input 流，无树概念 | ⚠️ 14.4+，树需自枚举、新子进程需重建 tap |
| 排除进程 | ✅ | ❌ 报 false | ✅ 14.4+ 原生 |
| 音频进程列表 | ✅ | ✅（带应用名，更丰富） | ✅ 14.4+ |
| 媒体会话+事件 | ✅ SMTC | ✅ MPRIS（唯一公开标准） | ⚠️ MediaRemote 私有 API，best-effort |
| 封面 | ✅ | ✅（http 封面回 `artworkUrl` 由宿主取） | ⚠️ 随媒体能力 |
| 权限 | 无 | 无 | TCC 弹窗（`NSAudioCaptureUsageDescription` 挂宿主 .app） |

## 3. Linux 实施方案

**依赖**（加入 `[target.'cfg(target_os = "linux")'.dependencies]`）：`libpulse-binding = "2"`（经 pipewire-pulse 同时覆盖 PulseAudio 与 PipeWire）、`zbus = "5"`。

**capture/linux/**：
- 独立线程跑 libpulse **threaded mainloop**，回调只发消息（同 Windows dev-mgr 模式），接现有 rtrb/worker 装配（`session::spawn_worker` 原样复用）
- 设备 loopback：sink 的 `monitor_source_name` 上 `connect_record`；输入设备直接 connect_record
- 按应用捕捉：introspect 由 pid 找 sink-input index（`application.process.id` 属性）→ **`Stream::set_monitor_stream(index)` 必须在 connect_record 之前调用**（已在 docs.rs 核实，libpulse-binding 2.30 存在此 API）
- 应用重建音频流（新 sink-input）时需自动重挂：订阅 sink_input 事件 → 走现有 manager 重启路径（复用 `Fatal`/`TryRestart` 状态机，勿另造）
- 热插拔/默认变更：`pa_context_subscribe` 掩码（sink/source/server）→ 映射到现有 `device.*` 事件与 `ManagerCmd::DefaultChanged/DeviceGone`
- 边角：Flatpak 应用的 pid 属性可能是沙箱 pid（对不上宿主视角）——按属性缺失处理，`processes.listAudio` 里如实呈现拿到的值

**media/linux.rs**：
- zbus 会话总线，watch `org.mpris.MediaPlayer2.*` 名称增减 → 映射 sessionsChanged；`PropertiesChanged`（Metadata/PlaybackStatus）+ `Seeked` → sessionUpdated
- **进度需轮询**（MPRIS 无 position 变更信号）：sidecar 内部轮询 Position，折算成与 Windows 完全相同的 timeline 事件（含 ≤2 次/秒节流），宿主无感知
- 封面：`mpris:artUrl` 为 `file://`/`data:` 时 sidecar 读取并走现有 `cache_artwork`（哈希/落盘/事件全复用）；`http(s)` 时回填 `artwork_url` 字段（协议已预留），**不要给 sidecar 加 HTTP 客户端**

**capabilities**：`process_loopback_exclude: false`，其余 true。

### 3a. Linux 风险假设与替代梯队

| 风险假设（先 spike 验证） | 主选 | 替代梯队 |
|---|---|---|
| `set_monitor_stream` 在 **PipeWire 的 pulse 兼容层**上行为正确（现代发行版默认 PipeWire，纸面结论只对原生 PulseAudio 有把握） | libpulse-binding | ① `pipewire` crate 原生 API（stream 直连目标 node，PipeWire 下可能反而更干净）；② 都不行则 `process_loopback: false`，只交付设备级捕捉 |
| sink-input 的 `application.process.id` 属性普遍存在 | 按 pid 匹配 | 属性缺失的应用按 `application.name` 辅助展示（仅影响 `processes.listAudio` 的呈现，不扩协议） |
| zbus 与 MPRIS 各播放器兼容性 | zbus 手写 proxy | `mpris` crate（dbus-rs 阻塞式，放专用线程与现有模式一致） |

检索关键词：`pipewire rust capture application stream`、`pw-stream target-object`、`libpulse set_monitor_stream pipewire`。

### 3b. Linux 真机探针结论（2026-07-23）

目标机为 PipeWire 1.6.2 的 PulseAudio 兼容服务（PulseAudio API 17.0，libpulse 17.0）。以下主选路线均已用
最小 Rust spike 真机验证，因此 Linux 实现继续采用 §3 的 libpulse + zbus 路线，不启用替代梯队：

- **`set_monitor_stream` 可用且隔离正确**：两个独立 `paplay` 进程同时播放 440Hz/4000Hz，分别按
  `application.process.id` 找到 sink-input，并在 `connect_record` 前设置目标 index。两路目标频率功率均约
  0.044，非目标频率仅 0.00001--0.00003（远优于 10 倍隔离门槛），说明 PipeWire-Pulse 没有退化成
  整个 sink monitor。
- **PID 属性在已测客户端存在**：`paplay` 与 GNOME Decibels 的 sink-input 均提供
  `application.process.id`、`application.name`、`application.process.binary`。仍保留属性缺失时跳过 PID
  匹配、只按已有字段展示的约定，不做猜测。
- **zbus MPRIS 可用**：会话总线可枚举 `org.mpris.MediaPlayer2.org.gnome.Decibels`，并正确读取
  `PlaybackStatus`、`Position` 与含 CJK 标题/作者的 `Metadata`。继续采用 zbus 手写 proxy。

探针同时确认一个 libpulse 生命周期要求：`Stream` 的 Rust 回调闭包必须在 `disconnect` 前用
`set_read_callback(None)` / `set_state_callback(None)` 注销，并保证 stream 先于 context/mainloop 销毁；否则
context 断连期间仍可能回调已释放闭包并造成 SIGSEGV。正式捕捉线程的所有退出路径都必须遵守此顺序。

M3 正式实现继续验证了两个运行时细节：

- `processes.listAudio` 可按 PID 聚合 sink-input；双 `paplay` 的 dev-client 捕捉分别只在 440Hz（约 band 28）
  与 4000Hz（约 band 50）出峰，`droppedGaps=0`，底层探针的目标功率约 0.044、串扰约 0.00001。
- sink-input 移除通知必须使用独立于音频 read wake 的有界通道。二者共用容量 1 的通道时，连续 read wake
  会令 `try_send` 丢掉控制事件。同 PID 的 libpulse-simple 探针将 sink-input 从 `#573` 重建为 `#586` 后，
  独立控制通道可稳定触发 `running -> restarting -> running` 并恢复捕捉；进程真正退出则在 2 秒内进入
  `failed:processExited`。流消失但 PID 仍存活时等待新 sink-input，不用固定超时误判进程退出。

M4 健壮性验收结果：

- Linux io 线程采用与 Windows 相同的 150/250/400/700/1200ms 激活重试梯；单次 Pulse stream ready
  等待限制为 1 秒，使最坏重试窗口仍落在 manager 的 5 秒 ready timeout 内。`deviceNotFound` 与
  `processExited` 是永久错误，不进入该重试梯。
- 默认输出在真实 ALSA sink 与临时 null sink 之间往返时，两次均产生
  `running -> restarting(defaultDeviceChanged) -> running`，回到原 sink 后 440Hz 捕捉恢复。显式 null
  sink 卸载后产生 `restarting(deviceRemoved)`，以同名 sink 重建后经退避自动恢复 `running`。
- 4 路 30fps 捕捉停读 stdout 5 秒后累计观察到 138 个 seq 空洞，恢复读取后持续响应；单路相同测试因
  150 帧积压未超过 256 帧共享队列而不丢帧。stdin EOF 退出码为 0，stdout 仅含合法
  `sidecar.exiting(stdinClosed)` 帧，无 sidecar 孤儿进程。

## 4. macOS 实施方案

**依赖**：`objc2-core-audio`（process tap 绑定）、`coreaudio-rs`（输入/枚举），参考实现 insidegui/AudioCap。

**capture/macos/**：
- 14.4+：`AudioHardwareCreateProcessTap` + 聚合设备。全系统 loopback = tap 全部进程；进程捕捉 = tap 指定 pid 集；排除 = CATapDescription 的 excluding 模式
- 子进程树：枚举进程树得 pid 集建 tap；**新生子进程不会自动加入**——定时对比进程树，变化则重建 tap（复用 manager 重启路径，接受切换瞬间的短暂空隙）
- 版本门槛：启动时查 OS 版本设 capability；13.x 若做 SCK 兜底为独立后续项，先按 14.4+ 交付
- 设备枚举/热插拔/默认变更：AudioObject 属性监听，映射到现有事件
- 音频进程列表：`kAudioHardwarePropertyProcessObjectList`（14.4+）

**media/macos.rs**：
- mediaremote-adapter 方案（ungive/mediaremote-adapter：借系统内已授权解释器加载 MediaRemote 框架，15.4+ 私有 API 限制的社区解法）；helper 脚本作为**打包资产**随 sidecar 分发
- **best-effort 纪律**：启动时探测可用性，失败则 `media_sessions/media_artwork: false`，不 panic 不重试轰炸；私有 API 每次 macOS 大版本都可能失效，失效即翻 capability，捕捉功能不受牵连

**打包注意**：`NSAudioCaptureUsageDescription` 写进宿主 Electron 应用 Info.plist；TCC 授权弹窗归属宿主 .app。

### 4a. macOS 风险假设与替代梯队（本平台纸面成分最高，全部先 spike）

| 风险假设（先 spike 验证） | 主选 | 替代梯队 |
|---|---|---|
| `objc2-core-audio` 对 process tap 的绑定完整可用（这是三平台里最没把握的一条） | objc2-core-audio | ① `cidre` crate（AudioCap 作者生态，覆盖 CA tap）；② `coreaudio-sys` 原始 FFI 手写缺失部分；③ ScreenCaptureKit（13+，牺牲权限体验）。**不采纳**虚拟声卡方案（要求用户装第三方驱动，违背零依赖交付——除非项目所有者改变决策） |
| mediaremote-adapter 在当前 macOS 版本仍有效（私有 API，每个大版本都可能被封） | mediaremote-adapter | ① 检索当下社区最新 now-playing 方案（该生态与 Apple 处于猫鼠状态，本文档写下的方案可能已过时）；② AppleScript/JXA 轮询主流播放器（Music/Spotify）作降级；③ 都不行 → `media_sessions: false`，捕捉功能不受牵连 |
| 新子进程需重建 tap 的空隙可接受（<100ms） | 定时对比进程树重建 | 空隙不可接受时研究 tap 的动态更新 API；仍不行则文档化该限制 |
| TCC 权限在"宿主 spawn 的子进程"场景正确归属宿主 .app | 权限挂宿主 | spike 确认弹窗归属与授权持久性；异常时研究 entitlement/签名要求并写入打包文档 |

检索关键词：`AudioHardwareCreateProcessTap rust`、`cidre process tap`、`CATapDescription exclude`、
`macOS now playing API <当前版本号>`、`mediaremote-adapter alternative`。

## 5. 验收标准（每个平台完成时必须全过）

自动化（`cargo test` 平台无关部分本来就过，以下是平台实测，参考本仓库 Windows 验证时的探针方法——生成双频正弦 WAV 由两个进程分别循环播放）：

1. **默认输出 loopback**：播放 440Hz 测试音，频谱 argmax 落在 ~band28，峰值 >0.9，30fps 连续无 seq 空洞
2. **进程隔离**：两进程分放 440/4000Hz，捕 A 时 band50 < 0.15，捕 B 时 band28 < 0.15（Linux 按 sink-input 等价验证）
3. **静音衰减**：暂停播放后 ≤0.5s 频谱峰值 < 0.01
4. **PCM 数学**：3 秒 s16le 默认配置恰好 60 块 / 字节数 = rate×ch×2×3
5. **进程退出**：杀掉目标 ≤2s 内 `capture.state failed:processExited`（Linux：流消失语义等价）
6. **设备切换**：切默认输出 → `restarting→running` 自动续播；拔插设备 → 退避重启恢复
7. **媒体**：本地播放器验证会话列表/元数据（含 CJK）/事件/封面落盘 + `artwork` 事件；进度事件 ≤2 次/秒
8. **背压**：停读 5s 不死、丢帧计数增长、恢复后响应正常
9. **生命周期**：kill 宿主进程（stdin EOF）→ sidecar 自行退出，无孤儿
10. **协议回归**：`hello` capability 如实、不支持项回 `unsupported`、`--capture`/`--artwork-dir`/`writeTo` 行为与 PROTOCOL.md 一致

## 6. 已踩过的坑（移植时会再遇到的）

- **wasapi crate `include_tree=false` = EXCLUDE 模式**（Windows 进程 loopback 只有含树/排除树两种，无"仅本进程"）——协议因此设计为 `process`（恒含树）+ `systemExcludingProcess` 两个源；其他平台映射时保持这两个语义
- **SMTC 的 contentType 不可信**（实测有播放器报 `"image/jpeg,image/jpe,image/jpg"` 逗号串）——封面扩展名一律魔数嗅探，contentType 只作兜底；MPRIS/MediaRemote 同样别信 MIME
- **timeline 事件轰炸**：有播放器每秒发多次进度——节流逻辑在 media worker 里，是**必须**不是优化
- **封面抓取异步 + 换曲竞态**：`artwork_gen` 代际计数丢弃过期结果——新平台 media 后端接入 `cache_artwork` 时保持该模式
- **阻塞式系统调用只能在专用线程**（Windows 是 COM/WinRT MTA；Linux 是 pulse mainloop 线程；macOS 是 CA 回调约束）——tokio 上永远只跑 IPC 与协调，音频/系统回调只发消息
- **事件驱动捕捉的静音行为**：无声时平台可能不投样本包（WASAPI 如此）——这是预期，worker 补零机制处理，平台层不要造样本
- **激活瞬态失败**：Windows 0x80070002 有两个已确认成因——设备快速开合后的短瞬态，以及**无线耳机端点从省电休眠唤醒**（首次激活即唤醒触发器，链路起来要 1~2s；表现为"第一次命令失败、第二次就好"）——io 线程内置递增重试梯（~2.7s 窗口）覆盖两者；其他平台的无线/蓝牙设备预期有同类行为，init 失败让错误走 `SessionError::Activation`（可重试语义）并给足唤醒时间
- **stdout 纯洁性**：一行非协议输出就毁掉宿主解析——`deny(clippy::print_stdout)` + 单写者已从编译期防住，平台代码勿引入直接打印的依赖
- **残留进程排查**：手动测试时用 Ctrl+C 或 `shutdown` 退出；直接关终端窗口偶见 sidecar 等不到 EOF 而滞留（表现为 exe 被占无法重编译）

## 7. 交付节奏建议

每平台按里程碑推进并逐项打勾 §5 清单：M1 设备枚举+事件 → M2 设备 loopback+频谱 → M3 按应用/进程捕捉 → M4 健壮性（重启/切换） → M5 媒体+封面 → M6 全量验收+capability 定稿。CI 加对应平台 runner 跑 `cargo build + test`（libpulse/Core Audio 依赖系统库，无法从 Windows 交叉验证）。
