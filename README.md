# audio-sidecar

为 danmacat-desktop（Electron 直播工具）提供音频能力的 Rust sidecar：

1. **设备捕捉** —— 任意输出设备的 loopback / 输入设备采集，支持"跟随默认设备"自动切换
2. **进程捕捉** —— 指定进程（含子进程树）的声音，或"全系统排除某进程"（Windows 10 2004+，同 OBS win-capture-audio 原理）
3. **设备枚举** —— 全状态列表 + 热插拔/默认设备变更事件
4. **系统媒体** —— SMTC 会话（标题/歌手/专辑/封面/播放状态/进度）只读快照 + 变更事件

音频在 sidecar 内完成 DSP（FFT → 对数分 band → AGC/噪声门 → 攻击衰减平滑），以 Wallpaper Engine 风格的频谱帧（默认 2×64 band @30fps，仅 ~27KB/s）推送给宿主；可选原始 PCM 流。协议为 stdio NDJSON，规范见 [PROTOCOL.md](./PROTOCOL.md)，TypeScript 类型见 [bindings/](./bindings)。

当前状态：**Windows 与 Linux 全功能已实现并验证**（Linux 在 PipeWire 1.6.2 的 PulseAudio 兼容层实测）；**macOS 已实现**（macOS 26.6.2 实测：设备 loopback/进程 tap/排除/树重建/背压/生命周期通过，Core Audio process tap + mediaremote-adapter 媒体，媒体与拔插等需播放的项待真机收口，见 PORTING.md §4b/§4c）。

## 构建与测试

```sh
cargo build                # 调试构建 → target/debug/audio-sidecar[.exe]
cargo build --release      # 发布构建（thin-LTO）
cargo test                 # 单元测试（DSP 数学、协议 serde、PCM 编码）
cargo test --features ts-export   # 同时重新生成 bindings/*.ts
cargo clippy --all-targets -- -D warnings
```

## 快速验证（tools/dev-client.mjs，零依赖 Node ≥18）

```sh
node tools/dev-client.mjs hello                 # 握手与能力
node tools/dev-client.mjs devices               # 设备列表
node tools/dev-client.mjs apps                  # 有音频会话的进程（捕捉选择器数据源）
node tools/dev-client.mjs meter --default       # 默认输出的实时终端频谱表（放音乐看效果）
node tools/dev-client.mjs meter --pid 12345     # 只捕某进程；--exclude-pid 反选
node tools/dev-client.mjs meter --pid 111 --pid 222 --default   # 单进程同时开多路，按 captureId 分行显示
node tools/dev-client.mjs watch                 # 打印全部事件（拔插设备/切歌试试）
node tools/dev-client.mjs media --watch --artwork cover.jpg
node tools/dev-client.mjs media --artwork-dir .\artcache        # 封面落盘模式（哈希命名，事件带路径）
node tools/dev-client.mjs media --artwork-dir                   # 不带值 = 落到系统临时目录（自动过期清理）
node tools/dev-client.mjs pcm --seconds 5 --out cap.raw   # Audacity 按 raw 导入试听
node tools/dev-client.mjs raw '{"method":"devices.getDefault","params":{"kind":"render"}}'
```

环境变量：`SIDECAR_BIN`（exe 路径，默认 target/debug）、`SIDECAR_LOG`（日志级别，默认 warn）。

## 验证状态

已自动化验证（本机 Windows 11 26200）：

- ✅ 默认输出 loopback：440Hz 测试音落在正确频带（峰值 0.985），30fps 无丢帧
- ✅ 进程隔离：双进程分放 440Hz/4000Hz，include 模式互不可见（旁路 <0.11），exclude 模式反选正确
- ✅ PCM：3 秒精确 60 块 / 576,000 字节（50ms×48kHz×2ch×s16le）
- ✅ SMTC：会话快照（含真实 PotPlayer 会话）、timeline 事件节流至 2 次/秒、1MB BMP 封面完整拉取、会话消失自动移除
- ✅ 进程退出 1 秒内 → `capture.state failed:processExited`，`capture.stop` 可清除
- ✅ 背压：停读 6 秒丢 203 帧但进程存活、seq 有洞、恢复后响应正常
- ✅ 错误码：methodNotFound / invalidParams / captureNotFound / sessionNotFound / deviceNotFound
- ✅ 生命周期：stdin EOF 干净退出（exit 0）、stdout 无日志污染、启动即推媒体快照
- ✅ 多路并发：单进程同时捕 2 个进程 + 默认输出，三路各自频率正确、零丢帧互不干扰
- ✅ `--capture` 启动参数：免 RPC 自启动捕捉，含每路独立 spectrum 配置（30fps/64band 与 15fps/32band 并行）
- ✅ `--artwork-dir` 封面落盘：真实播放器（SPlayer）封面以哈希名原子写出（魔数嗅探出 .jpg），`changed=["artwork"]` 事件带路径与哈希

Linux / PipeWire 1.6.2（Pulse API 17.0）实测：

- ✅ 默认输出 440Hz 在 band 28 出峰，连续 4 秒 30fps 无 seq 空洞；静音停止后约 1 秒 RMS 与频谱归零
- ✅ 双 PID 440/4000Hz 监视流隔离：目标功率约 0.044，串扰不超过 0.00003；`processLoopbackExclude=false` 且调用返回 `unsupported`
- ✅ PCM：3 秒 s16le 精确 576,000 字节；进程退出在 2 秒内 `failed:processExited`
- ✅ 默认 sink 切换、显式 sink 卸载/重建均自动恢复；4 路停读 5 秒丢 140 帧后恢复
- ✅ MPRIS：真实 Decibels CJK 元数据/Play-Pause/Position 轮询与事件节流；标准探针验证 file/data 封面、base64、`writeTo`、`--artwork-dir` 与 artwork 事件
- ✅ 30 次 EOF 生命周期压测 exit 0、无孤儿；`--capture` 15fps/32-band 配置回显正确

补充手动验证（不影响上述 M6 自动化验收，需要物理操作或长期观测）：

- ⬜ 播放音乐时在系统设置切默认输出 → `restarting → running` 后频谱在新设备继续
- ⬜ 拔掉被捕捉的 USB 设备 → 退避重启循环，插回自动恢复
- ⬜ 长时间运行的 CPU/内存平稳性（目标：单路 30fps ≤1–2% CPU）

## 目录结构

```
src/
├── main.rs                 # CLI、tracing→stderr、平台装配、优雅关停+看门狗
├── protocol/               # 全部 wire 类型（serde camelCase，ts-rs 导出）
├── rpc/                    # stdin 路由、唯一 stdout 写者（可靠通道+可丢帧通道）
├── capture/
│   ├── manager.rs          # 会话 actor：生命周期、follow-default、重启退避
│   ├── session.rs          # 平台无关 DSP worker 线程（补零、频谱、PCM 切块）
│   ├── linux/              # libpulse threaded mainloop：设备/monitor/按 PID 捕捉
│   ├── macos/              # Core Audio：进程 tap/聚合设备/IOProc、libproc 进程树
│   └── windows/            # WASAPI：设备/进程捕捉、dev-mgr 线程、会话枚举
├── dsp/                    # 纯函数：Hann+FFT、对数分 band、AGC+噪声门、平滑
├── media/linux.rs          # zbus MPRIS worker（信号驱动 + Position 轮询 + 封面缓存）
├── media/macos.rs          # MediaRemote worker（mediaremote-adapter perl 子进程 + NDJSON）
├── media/windows.rs        # SMTC worker 线程（事件脏标记→快照 diff→节流推送）
└── util/                   # PCM 编码、浮点清洗
tools/dev-client.mjs        # 调试/验证客户端
assets/macos/               # mediaremote-adapter 打包资产（媒体功能，随二进制分发）
bindings/                   # 生成的 TypeScript 协议类型（交给宿主）
PROTOCOL.md                 # 协议规范
```

## 架构要点

- 音频永不进 tokio：每会话两条专用 OS 线程（WASAPI io + DSP worker），rtrb 无锁环连接；COM/WinRT 阻塞调用严格限定在专用 MTA 线程
- 频谱/PCM 走有界通道，宿主卡顿只丢帧不反压音频线程；响应与状态事件走可靠通道
- 静音=按 tick 补零一条规则，覆盖启动/无声/停顿，衰减动画与 PCM 连续性自然成立
- 设备失效/默认切换自动重启（200ms→5s 退避），进程退出交宿主决策
- Linux PulseAudio/PipeWire I/O 固定在 threaded mainloop；MPRIS zbus 调用固定在专用 worker，tokio 只做协调
- `deny(print_stdout)` + 单写者任务，从编译期杜绝 stdout 协议污染

## 路线图

Linux 与 macOS 适配的**目标不变量、实施方案、验收清单与踩坑记录**统一维护在 [PORTING.md](./PORTING.md)——换平台开发时以它为准，防止目标漂移。概要：

- **Linux**：libpulse threaded mainloop（monitor source + `set_monitor_stream` 按应用捕捉）、subscribe 热插拔、zbus MPRIS
- **macOS**：Core Audio process tap（14.4+，`objc2-core-audio`，设备级/进程/排除三模式）、HAL IOProc 输入采集、mediaremote-adapter 媒体（best-effort）；打包要求见 [docs/PACKAGING-macos.md](./docs/PACKAGING-macos.md)
- 可选：WebSocket 传输通道（渲染进程直连）、`capture.setSpectrumConfig` 热重配
