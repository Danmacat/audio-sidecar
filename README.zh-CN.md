# audio-sidecar

[English](./README.md) | **简体中文**

[![CI](https://github.com/Danmacat/audio-sidecar/actions/workflows/ci.yml/badge.svg)](https://github.com/Danmacat/audio-sidecar/actions/workflows/ci.yml)
[![release](https://github.com/Danmacat/audio-sidecar/actions/workflows/release.yml/badge.svg)](https://github.com/Danmacat/audio-sidecar/actions/workflows/release.yml)
![License](https://img.shields.io/badge/license-MIT-blue.svg)

跨平台**音频 sidecar**：一个独立子进程，为宿主应用提供系统音频捕捉、实时频谱、PCM 流与媒体会话（正在播放）信息。宿主用任意语言（Electron/Node、Python、Go、C#……）spawn 本进程，通过 **stdio NDJSON** 协议交互——所有音频 DSP 在 sidecar 内完成，宿主只消费轻量的 JSON 数据（默认频谱输出仅 ~27 KB/s）。

## 特性

- **设备捕捉** —— 任意输出设备的 loopback / 输入设备采集，支持"跟随默认设备"自动无缝切换
- **进程捕捉** —— 指定进程（含子进程树）的声音，或"全系统排除某进程"（反选，如排除宿主自身）
- **设备枚举** —— 全状态列表（active/disabled/unplugged/notPresent）+ 热插拔/默认设备变更事件
- **系统媒体** —— 媒体会话（标题/歌手/专辑/封面/播放状态/进度）只读快照 + 变更事件，封面支持按需拉取或自动落盘
- **频谱输出** —— FFT → 对数分 band → AGC/噪声门 → 攻击衰减平滑，默认 2×64 band @30fps，Wallpaper Engine 兼容布局
- **PCM 流** —— 可选 s16le/f32le 交错流，按样本数切块无钟漂积累，拼接即得连续音频
- **多路并发** —— 单进程同时捕捉多路（默认上限 8），按 `captureId` 分流，无需多开
- **背压安全** —— 宿主卡顿只丢帧不反压音频线程；stdout 永远只有协议帧，日志走 stderr

## 平台能力

| 能力 | Windows | Linux | macOS |
|---|---|---|---|
| 输出设备 loopback | ✅ WASAPI | ✅ PulseAudio / PipeWire monitor | ✅ Core Audio |
| 输入设备采集 | ✅ | ✅ | ✅ |
| 跟随默认设备（自动切换） | ✅ | ✅ | ✅ |
| 进程捕捉（含进程树） | ✅ 需 Win10 2004+ | ✅ | ✅ 需 macOS 14.4+ |
| 全系统排除进程（反选） | ✅ 需 Win10 2004+ | ❌ `unsupported` | ✅ 需 macOS 14.4+ |
| 设备枚举 + 热插拔事件 | ✅ | ✅ | ✅ |
| 有声进程列表 | ✅ | ✅ | ✅ |
| 媒体会话 + 封面 | ✅ SMTC | ✅ MPRIS | ⚠️ MediaRemote† |
| 频谱 / PCM 输出 | ✅ | ✅ | ✅ |

† macOS 的媒体功能依赖随附的 [mediaremote-adapter](./assets/macos/mediaremote-adapter) 资产（Apple 自 macOS 15.4 起限制第三方进程直接访问 MediaRemote 框架）。资产缺失时 `hello.capabilities` 如实上报 `false`、媒体 RPC 返回错误，**捕捉功能完全不受影响**。打包要求见 [docs/PACKAGING-macos.md](./docs/PACKAGING-macos.md)。

能力探测：启动后调 `hello`，按返回的 `capabilities` 做 feature-detect；不支持的平台/方法返回 `unsupported` 错误而非崩溃，协议在各平台完全一致（见 [PROTOCOL.md](./PROTOCOL.md)）。

实测环境：Windows 11（26200）、Ubuntu（PipeWire 1.6.2 的 PulseAudio 兼容层）、macOS 26.6.2（arm64）。

## 获取二进制

- **Release**：推送 `v*` tag 后自动创建 GitHub Release，附三平台压缩包——`audio-sidecar-linux-x64.tar.gz` / `audio-sidecar-windows-x64.zip` / `audio-sidecar-macos-arm64.tar.gz`（macOS 包内含 `mediaremote-adapter/` 资产，解压后保持同目录布局即可）。
- **CI 产物**：master 每次推送，或在 Actions 页手动触发 `release` 工作流（可选任意分支），都会产出三平台 Artifacts 供下载。
- **源码构建**：见[下文](#从源码构建)。

## 快速上手

宿主集成（Electron/Node 示例，完整语义见 [PROTOCOL.md](./PROTOCOL.md)）：

```ts
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

const sidecar = spawn(sidecarPath, ["--log-level", "info"], { stdio: ["pipe", "pipe", "pipe"] });
createInterface({ input: sidecar.stdout }).on("line", (l) => {
  const msg = JSON.parse(l);
  if (msg.event) onEvent(msg);   // capture.spectrum / device.* / media.* 事件
  else settlePending(msg.id, msg); // 响应按 id 结算（可乱序）
});
sidecar.stderr.pipe(logStream);  // 日志只走 stderr
// 宿主退出 → stdin EOF → sidecar 自动优雅退出，无需显式清理
```

协议帧三形态（每行一个 JSON 对象）：

```jsonc
{"id": 1, "method": "capture.start", "params": {"source": {"type": "defaultOutput"}}}   // 请求
{"id": 1, "result": {"captureId": "..."}}                                                // 响应
{"event": "capture.spectrum", "data": {"captureId": "...", "bands": [[...], [...]]}}     // 事件
```

命令行冒烟与调试（[tools/dev-client.mjs](./tools/dev-client.mjs)，零依赖 Node ≥18）：

```sh
node tools/dev-client.mjs hello                 # 握手与能力
node tools/dev-client.mjs devices               # 设备列表
node tools/dev-client.mjs apps                  # 有音频会话的进程
node tools/dev-client.mjs meter --default       # 默认输出的实时终端频谱（放音乐看效果）
node tools/dev-client.mjs meter --pid 12345     # 只捕某进程；--exclude-pid 反选
node tools/dev-client.mjs watch                 # 打印全部事件（拔插设备/切歌试试）
node tools/dev-client.mjs media --watch --artwork cover.jpg
node tools/dev-client.mjs pcm --seconds 5 --out cap.raw   # Audacity 按 raw 导入试听
```

环境变量：`SIDECAR_BIN`（指定 exe 路径，默认 target/debug）、`SIDECAR_LOG`（日志级别，默认 warn）。

## 从源码构建

```sh
cargo build --release      # → target/release/audio-sidecar[.exe]
cargo test                 # 单元测试（DSP 数学、协议 serde、PCM 编码）
cargo test --features ts-export   # 同时重新生成 bindings/*.ts
cargo clippy --all-targets -- -D warnings
```

Linux 需要 PulseAudio 开发包（`sudo apt-get install libpulse-dev`）；macOS 构建会自动把 mediaremote-adapter 资产复制到二进制旁（见 `build.rs`）。

## 架构要点

- 音频永不进 tokio：每会话两条专用 OS 线程（平台音频 I/O + DSP worker），无锁环形缓冲连接；阻塞式平台调用严格限定在专用线程
- 频谱/PCM 走有界帧通道，宿主卡顿只丢帧不反压；响应与状态事件走可靠通道，保序不丢
- 静音 = 按 tick 补零这一条规则，覆盖启动/无声/停顿，衰减动画与 PCM 连续性自然成立
- 设备失效/默认切换自动重启（200ms→5s 退避），进程退出交宿主决策
- `deny(print_stdout)` + 单写者任务，从编译期杜绝 stdout 协议污染

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
├── media/{linux,macos,windows}.rs   # MPRIS / MediaRemote / SMTC 媒体 worker
└── util/                   # PCM 编码、浮点清洗
tools/dev-client.mjs        # 调试/验证客户端
assets/macos/               # mediaremote-adapter 打包资产（随二进制分发）
bindings/                   # 生成的 TypeScript 协议类型（供宿主使用）
```

## 文档

- [PROTOCOL.md](./PROTOCOL.md) —— stdio NDJSON 协议规范（方法/事件/错误码/背压/生命周期）
- [docs/PACKAGING-macos.md](./docs/PACKAGING-macos.md) —— macOS 打包指南（资产布局/权限/签名）
- [bindings/](./bindings) —— TypeScript 协议类型（`cargo test --features ts-export` 生成）

## 许可证

MIT。macOS 媒体功能随附的 mediaremote-adapter 资产为 BSD-3-Clause（见 [assets/macos/mediaremote-adapter/LICENSE](./assets/macos/mediaremote-adapter/LICENSE)）。
