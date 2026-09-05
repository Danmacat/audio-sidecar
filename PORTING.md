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

M5 媒体与封面验收结果：

- 正式实现使用 zbus match rule 监听 `NameOwnerChanged`、`PropertiesChanged` 与 `Seeked`；只有 MPRIS
  没有信号的 `Position` 以 250ms 周期轮询，另以 5 秒全量重同步兼容漏发信号的播放器。GNOME Decibels
  的 Play/Pause 可即时触发 `playbackInfo`，4 秒播放窗口收到 6 个纯 timeline 事件，事件间隔不小于约
  500ms，满足每会话不超过 2 次/秒。
- Decibels 实测快照保真返回日文标题 `メルト (かぐや ver.) [CPK! Remix]` 及混合日文作者，状态、时长、
  repeat/shuffle 与 current 映射正确。临时 MPRIS 标准探针的出现/退出分别触发全量 `sessionsChanged`，
  current 同步切入及回到 null，说明名称监听与会话生命周期有效。
- Decibels 本身不发布 `mpris:artUrl`（含内嵌封面的本地 MP3 也不发布），这属于播放器能力而非 zbus
  路线失效；使用发布 `file:` artUrl 的标准 MPRIS 探针补验。295 字节 PNG 经 base64、`writeTo` 与
  `--artwork-dir` 三条路径均返回 `image/png`，两种落盘得到同一 FNV 内容哈希
  `6516d5c0ced3f88c`；自动落盘先发无路径会话快照，再独立发一次 `changed=["artwork"]`。读取在后台
  执行并以 generation 丢弃过期结果。`http(s)` artUrl 只透传 `artworkUrl`，sidecar 不发起网络请求。

M6 全量验收结果（PipeWire 1.6.2 / Pulse API 17.0，2026-07-23）：

- **默认输出**：真实 440Hz 播放经 `tools/dev-client.mjs meter --default` 在约 band 28 出峰，峰值条到 `█`，
  连续 4 秒 `droppedGaps=0`。
- **进程隔离**：两个 PID 的 dev-client 行分别只在 band 28 与 band 50 出峰且各自 `droppedGaps=0`；
  libpulse 监视探针测得 A=`440:0.04422, 4000:0.00003`、B=`440:0.00003, 4000:0.04417`。
- **静音衰减**：停止 440Hz 播放后约 1 秒，连续输出 `rms=0.000`、所有 band 为空，满足 0.5 秒目标。
- **PCM**：`pcm --seconds 3` 得到 48kHz/2ch/s16le 的 576,000 字节（60×50ms，数学值精确）。
- **进程退出**：杀掉目标 paplay 后约 2 秒内收到 `capture.state failed`，reason=`processExited`，没有自动
  重启。
- **设备恢复**：真实默认 sink ↔ 临时 null sink 往返两次均为
  `restarting(defaultDeviceChanged) -> running`；临时显式 sink 卸载/重建为
  `restarting(deviceRemoved) -> running`。重启期间 dev-client 按 capture.state 开启新的 seq epoch，避免把
  合法重挂误报为负的 dropped gap。
- **媒体/封面**：M5 的 CJK、会话、事件、封面和 ≤2Hz timeline 结果同时通过。
- **背压**：4 路捕捉停读 5 秒后观察到 `droppedGaps=140`，恢复读后持续出帧、sidecar 仍响应。
- **生命周期**：立即 EOF 压测 30 次均 exit 0，`sidecar.exiting(stdinClosed)` 是最后一帧且无孤儿；正常
  shutdown 也经过 Linux 事件生产者 quiesce，`sidecar.exiting(shutdown)` 保持最终可靠消息。
- **协议回归**：hello 报告 Linux 全部已实现能力，`processLoopbackExclude=false`；排除进程调用返回
  `unsupported`；`--capture` 的 15fps/32-band 配置与 `capture.list` 回显一致；`writeTo`、裸
  `--artwork-dir` 和未知方法/会话错误码均符合 `PROTOCOL.md`。

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

### 4b. macOS 真机探针结论（2026-09-04，macOS 26.6.2 / arm64，探针代码在 /tmp/macos-spikes）

四项探针全部完成，主选路线全部成立，不启用替代梯队：

- **探针①（tap 路线）通过，采用 `objc2-core-audio` 0.3.2，不启用 cidre/FFI/SCK 替代**：
  - 绑定完整：`CATapDescription` 全部初始化器（进程 mixdown / 全局排除 / **设备级**
    `initWithProcesses:andDeviceUID:withStream:` 及其排除变体）、`AudioHardwareCreateProcessTap`、
  `kAudioHardwarePropertyProcessObjectList` / `TranslatePIDToProcessObject` / 进程属性（PID/BundleID/IsRunning），
  甚至 macOS 26 新增的 `bundleIDs`（按 bundle ID 匹配进程）与 `processRestoreEnabled`（tap 自动按
  bundle ID 恢复退出进程）都有绑定。依赖需按 flexaudio-os-macos 的版本钉住：
  `objc2=0.6.4, objc2-core-audio/-types/-foundation=0.3.2, objc2-foundation=0.3.2, block2=0.6.2`。
  - 拉流链路无需 AudioUnit/AudioToolbox：tap → 私有聚合设备
  （`TapList:[{SubTapUID,SubTapDriftCompensation}]` + `TapAutoStart:true`）→
  `AudioDeviceCreateIOProcIDWithBlock` → IOProc 回调收 interleaved f32。参考实现
  Studio-Sadola/flexaudio-os-macos（生产形态的 Rust 同类，验证了拆链顺序
  Stop→DestroyIOProc→DestroyAggregate→DestroyTap 与回调竞态防护）。
  - 实测数据：tap 格式 48kHz×2ch float；**双进程 440/4000Hz 隔离串扰为 0**（include A 时
  p440=5999.6 / p4000=0.000000，include B 与 exclude A 对称正确），优于 Linux 探针的 1e-5 量级；
  设备级 tap（空排除列表 + deviceUID = 该设备全部音频）同样通过。
  - **关于"优先 OBS 实现方式"的决策**：OBS 的应用音频采集走 ScreenCaptureKit（macOS 13+），但
  SCK **无法捕指定输出设备**（协议 `device` 源需要），且需要更重的 Screen Recording 权限；OBS
  分析文档自身（§29–32）也将 CATap 定位为音频专用路线并指出 OBS 选 SCK 是为了统一桌面+音频
  采集。CATap 在本机四模式全通过且权限更轻，故捕捉引擎采用 CATap（即 §4 原方案），SCK 保持
  §4a 替代梯队地位不启用。OBS 的成熟经验吸收进实现细节：destroy/recreate 式重建（macOS 26
  重建仅 ~10ms，见探针③）、错误码映射（权限拒绝 → activationFailed）、`excludesCurrentProcessAudio`
  对应 tap 场景下 sidecar 自身进程的排除语义。
  - 进程树边界条件（Safari 主进程 vs WebKit.GPU 子进程 bundle 不同）留 M3 用真实浏览器实测，
  macOS 26 可用 `bundleIDs` + `processRestoreEnabled` 缓解，14.4–15.x 需自枚举进程树重建。

- **探针②（MediaRemote）通过，采用 mediaremote-adapter（perl 载体）**：
  - 直接 dlopen 路线**否决**：`/System/Library/PrivateFrameworks/MediaRemote.framework` 在
  macOS 26.6.2 上 dlopen 成功、符号可见，但 `MRMediaRemoteGetNowPlayingInfo` 的回调**静默不触发**
  ——15.4 的 com.apple.* bundle ID 限制在 26 仍然生效，且失败无任何错误码。
  - ungive/mediaremote-adapter（BSD-3）路线有效：`/usr/bin/perl`（注册为 com.apple.perl）加载
  自建 `MediaRemoteAdapter.framework`，`test` 命令 exit 0（内部是真实的 MediaRemote API 回调
  往返），`stream` 以 NDJSON 输出 `{"type":"data","diff":...,"payload":{...}}`，`get` 一次性快照。
  侧车用法：spawn `/usr/bin/perl mediaremote-adapter.pl <framework> stream`，解析 stdout NDJSON；
  启动时先跑 `test` 探测，失败即 `media_sessions/media_artwork=false`（best-effort 纪律）。
  打包资产 = `mediaremote-adapter.pl` + 预编译 `MediaRemoteAdapter.framework`（ad-hoc 签名，双架构）。
  注意 framework 路径必须传**绝对路径**（相对路径 dlopen 失败）；`test` 需要 TestClient 路径作
  第二个位置参数（无真实播放器时可模拟），生产侧车探测可用 `get`（返回 null=加载成功但无播放，
  非空=有会话；dlopen 失败会打印 "Failed to load framework" 退出 1）。

- **探针③（重建空隙）通过**：tap+聚合设备完整建/拆周期实测 ~10ms（首次 51ms 冷启动），20 轮
  min=7/max=51ms，远低于 100ms 门槛。子进程树变化走 manager 重启路径重建 tap 完全可行；
  macOS 26 上 `processRestoreEnabled` 还可让同 bundle 进程退出/重启自动回归 tap，减少重建频率。

- **探针④（TCC 归属）**：macOS 26 存在 `AudioCapture` TCC 服务（tccutil 可识别重置），但实测
  从 GUI 应用（本机为 dev.zcode.app）spawn 的**未签名 CLI 子进程创建 process tap 不触发任何
  弹窗**，`tccutil reset AudioCapture` 后行为不变——tap 创建当前不检查 TCC（或仅对打包 .app
  生效）。结论：sidecar 以"宿主 spawn 的裸二进制"形态运行时无需权限交互；防御性要求仍写入
  打包文档：宿主 .app 的 Info.plist 应含 `NSAudioCaptureUsageDescription`（若未来 macOS 收紧，
  弹窗将归属宿主 .app）。麦克风输入采集（defaultInput）预期走 Microphone TCC，M2 实测补充。

  探针过程的操作痕迹：为验证服务存在执行过 `tccutil reset AudioCapture/ScreenCapture/
  Microphone/ListenEvent/PostEvent`（清除了全机这些服务的既有授权记录，受影响应用下次使用会
  重新弹窗）。

### 4c. macOS 实施与验收记录（2026-09-04，macOS 26.6.2 / arm64）

实现完全落在 `capture/macos/`（hal 属性助手 + devices 设备线程 + stream 捕获线程）与
`media/macos.rs`，平台无关代码未动（manager/session/dsp/protocol 原样复用）。依赖钉死
`objc2 0.6.4 / objc2-core-audio 0.3.2 / block2 0.6.2`。设备 wire id：`ca:out:<UID>` /
`ca:in:<UID>`。进程树用 libproc `proc_listallpids`+`proc_pidinfo`（`proc_bsdinfo` 的
pid@12/ppid@16 偏移来自 SDK 头文件，非猜测）。

已验证项（§5 清单口径）：

- **M1 设备**：`devices.list` 正确列出内置扬声器（默认）与 HDMI 显示器（`ca:out:` 前缀 +
  48kHz/2ch 格式）；`processes.listAudio` 以 output-scope `IsRunningOutput` 区分 active
  （afplay 播放中正确标 active，排序正确）。CI 增加 macos-latest runner。
- **M2 设备捕捉**：默认输出 440Hz 在 band 28 出峰（峰值 0.985，`droppedGaps=0`）；显式
  `--device` 路径 4000Hz 在 band 50 出峰；静音停止后 699ms 峰值 <0.01（含 tap 缓冲排空
  滞后，与 Linux ~1s 同量级）；PCM 5s 连续 99 块无空洞、`firstSampleIndex` 精确递增无钟漂。
  输入设备（`ca:in:`）走标准 HAL IOProc 直读，**本机无输入设备未实测**——路径与已实测的
  设备级 tap 输入同构（同一 IOProc 机制）。
- **M3 进程捕捉**：双进程 440/4000Hz 隔离在 AGC 收敛窗口 avg/max 均为
  A=0.985/0.000、B=0.000/0.985（零串扰）；exclude 模式对称正确；SIGKILL 目标后 ~170ms
  `failed:processExited` 不自动重启；树重建实测：父进程顺序 spawn 播放子进程时自动
  `restarting(deviceInvalidated) → running`（重建空隙 232ms，含 manager 200ms 首次退避；
  tap 本身建/拆 ~10ms），子进程切换后频率跟随正确。macOS 26 上 tap 打开
  `processRestoreEnabled`（版本守卫，<26 不调该 selector 避免 unrecognized selector）。
  排查记录：进程树成员比较必须排序——HashMap 遍历序随机曾导致成员集合"永远变化"→
  重建风暴（7s 内 8 次 restarts），排序后精确 1 次。
- **M4 健壮性**：默认输出在内置扬声器 ↔ HDMI 显示器间往返切换均
  `restarting(defaultDeviceChanged) → running`（deviceId 随之更新）；4 路 30fps 停读 5s
  总丢帧 139（Linux 为 140，帧通道行为一致）、进程存活且 `capture.list` 响应正常、恢复
  读后出帧；stdin EOF 立即关闭压测 30/30 exit 0 且 `sidecar.exiting(stdinClosed)` 恒为
  最后一帧、无孤儿；`shutdown` 优雅路径 exit 0。
- **M5 媒体（无音频部分）**：`media/macos.rs` spawn `/usr/bin/perl mediaremote-adapter.pl
  <framework> stream --micros`，NDJSON 帧合并（全量+diff）→ 会话映射 → 事件（timeline
  ≤2/s 节流 + pending flush，与 Linux 同语义）；封面 base64 → 魔数嗅探 → 内容哈希落盘
  （`--artwork-dir`/`writeTo` 共享缓存）。资产在 `assets/macos/mediaremote-adapter/` 入库
  （BSD-3 LICENSE 附带），build.rs 在 macos 构建时复制到二进制旁（framework 符号链接需
  重建）。无播放时实测：启动推空 `sessionsChanged`、`getSessions`/`getCurrent` 返回空、
  `getArtwork` 回 `sessionNotFound`；`perl … get` 仍 exit 0（探针②路线存活）。
- **协议回归**：`--print-hello` 报 platform=macos、osVersion=26.6.2、12 项 capability；
  methodNotFound / invalidParams（spectrum+pcm 全关、bands 超限）/ captureNotFound /
  deviceNotFound / processNotFound 全部正确；`--capture` 双路启动（15fps/32band 与默认）
  配置回显与 Linux 一致。

真机播放验证结果（2026-09-05 补充，M5/M6 收口）：

- **媒体（Music.app 播放本地文件实测）**：会话出现/切歌/播放状态事件全部正确
  （`sessionsChanged` 快照 + `mediaProperties,playbackInfo` 分类）；**MediaRemote 播放中不推
  周期进度**（实测 30s 播放 0 条进度 diff），worker 按帧内时间戳本地外推
  （`position = elapsed + (now - timestamp) × rate`），20s 实测 40 条 timeline 事件、最小
  间隔 502ms（≤2/s）；`mediaType` 为**字符串**（`MRMediaRemoteMediaTypeMusic`）而非数字，
  按后缀映射 Music/Video。封面：mediaremoted 对 Music 临时打开的文件**不提供 artworkData**
  （上游行为，与 m4a 是否内嵌封面无关）；封面管线（base64→魔数嗅探→哈希落盘→
  `changed=["artwork"]`→去重）由单测端到端覆盖（`media::macos::tests`，含 16 字节 PNG 嗅探
  出 .png 与内容寻址去重）。真实流媒体封面上线后可再观察。
- **Safari 子进程树——macOS 的 XPC 例外（重要事实）**：WebKit.GPU 渲染进程的 **ppid=1
  （launchd）**，Safari 主进程的 pid 父子树**枚举不到它**；实测 `bundleIDs=["com.apple.Safari",
  "com.apple.WebKit.GPU"]` 的 tap 也不投样本（该属性未文档化，不深挖）。**协议推荐路径完全
  工作并已实测**：`processes.listAudio` 给出发声 pid（WebKit.GPU，带 bundle 标识），直接捕
  该 pid 实测 p440=5999.5 零串扰。Chrome/Electron 等应用 renderer 是真子进程，pid 树语义
  正常；Safari 类 XPC 架构请宿主用发声 pid（PROTOCOL.md §5 本就推荐）。
- **"等待发声"语义修正**：目标进程存活但尚未连接 Core Audio（无进程对象）时，原实现误报
  `processExited`；已改为建立空成员 tap 进入 running（无声），由树监控在目标/子进程连上音频
  后自动重建挂入——与 Linux 的 target_loss 等待语义一致。Safari 主 pid 现在正确 running
  （静默等待）而非报错。
- **PCM 数学精确复测**：连续流中任意 60 块恰好 576,000 字节（48000×2ch×s16le×3s）、
  `firstSampleIndex` 跨度精确 141,600（59×2400）、无钟漂。
- **静音衰减复测**：SIGSTOP 暂停播放后 744ms 峰值 <0.01（rms=0.0000）。与 Linux（~1s 归零）
  同量级；比 Windows 慢的部分是 tap 管线排空残余样本（真实音频，平台层不得丢弃——WASAPI
  loopback 无声时不投包故 Windows 更快）。衰减动画观感一致。

### 4d. ScreenCaptureKit 进程引擎（A/B 可选，2026-09-05）

应用户要求补充了与 OBS Studio 完全同款的 SCK 进程捕捉引擎（`capture/macos/sck.rs`），
作为 CATap 的 A/B 选项：环境变量 `AUDIO_SIDECAR_PROCESS_ENGINE=sck` 时
`process`/`systemExcludingProcess` 源走 SCK，**默认仍为 CATap**；设备 loopback 恒走
CATap（SCK 无设备级能力）。链路照抄 OBS：`SCShareableContent` → `SCRunningApplication`
（按 pid 进程树解析）→ `SCContentFilter(including/excludingApplications:exceptingWindows:)`
→ `SCStreamConfiguration`（capturesAudio、excludesCurrentProcessAudio、channelCount=2、
queueDepth=8；采样率不设，从首帧 ASBD 读）→ `SCStream` + **dummy screen output**
（OBS 的"静默 SCK 错误"技巧）→ `CMSampleBuffer`（float 标志校验 + planar→interleaved）。

真机对比实测（macOS 26.6.2，Music+Safari 双 GUI 应用双频）：

- **Safari 主进程 pid 捕捉：SCK 强项**——按 GUI 应用聚合，能捕到 WebKit.GPU 渲染的网页
  音频（band28=0.985），这是 CATap 的 pid 树语义做不到的路径；Music(440) b28=0.955/b50=0.000、
  Safari(4000) b28=0.000/b50=0.985、排除 Music 对称正确——隔离与 CATap 同级（零串扰）。
- 静音衰减 633ms（SIGSTOP 后 peak<0.01）；SIGKILL 后 ~0.4s `failed:processExited`
  （io 循环轮询根 pid 存活，SCK 自身不因目标退出而报错）。
- **SCK 的代价**：①需要 Screen Recording TCC 授权（首次触发弹窗归属宿主 .app，本机
  ZCode 授权后生效；未授权时 `activationFailed` 且透传中文 TCC 消息）；②**只能捕 GUI
  应用**——`afplay` 等无窗口 CLI 进程没有 `SCRunningApplication` 条目，如实报
  activationFailed；③引擎切换仅影响进程类源，协议无变化。
- 实现备注：`define_class!`（objc2 0.6 语法）实现 `SCStreamOutput` 协议；修复过
  `RefCell` 双借用 panic（if-let 临时守卫存活期间 `borrow_mut`）。

仍待手动/长期观察项：

- 被捕捉 USB/蓝牙设备物理拔插 → `restarting(deviceRemoved)` 循环与插回自动恢复（机制与默认
  切换同源已验证，物理拔插留手动）；无线耳机首次激活唤醒（激活重试梯已就位）。
- 真实流媒体（Apple Music/网易云在线曲库）的封面与 CJK 元数据长跑观察。

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
