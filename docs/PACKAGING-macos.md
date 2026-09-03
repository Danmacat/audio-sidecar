# macOS 打包指南（audio-sidecar）

面向把 audio-sidecar 打进宿主 Electron 应用（danmacat-desktop）的打包场景。
Windows/Linux 打包不受本文影响。

## 1. 二进制与资产布局

媒体会话功能依赖 mediaremote-adapter 打包资产（见
`assets/macos/mediaremote-adapter/README.md`）。发布布局必须是：

```
<宿主 .app>/Contents/Resources/
├── audio-sidecar                          # 二进制
└── mediaremote-adapter/
    ├── mediaremote-adapter.pl
    └── MediaRemoteAdapter.framework/
```

sidecar 运行时按以下顺序查找（首个命中生效）：

1. 环境变量 `AUDIO_SIDECAR_MEDIAREMOTE_DIR`
2. `<sidecar 二进制目录>/mediaremote-adapter/`
3. `<sidecar 二进制目录>/`（平铺）

资产缺失时 `hello.capabilities.mediaSessions/mediaArtwork = false`，媒体 RPC 返回
`osError`，捕捉功能完全不受影响（best-effort 纪律，PORTING.md §4）。

开发态无需手工摆放：`cargo build` 在 macOS 上自动把
`assets/macos/mediaremote-adapter/` 复制到 `target/<profile>/mediaremote-adapter/`
（build.rs，含 framework 符号链接重建）。

## 2. 系统版本门槛（capability 如实原则）

- **进程捕捉 / 排除进程 / 设备 loopback（process tap）**：需要 macOS 14.4+
  （`AudioHardwareCreateProcessTap`）。当前实现按构建目标直接使用该 API；低于 14.4
  的系统上 tap 创建失败会走 `activationFailed`（可重试错误）而不是崩溃——若需对
  13.x 报 `unsupported`，在宿主侧按 `hello.osVersion` 前缀判断（"13."）后不发起
  对应 `capture.start`，或等待后续版本把 capability 探测内置到启动时。
- **媒体（MediaRemote）**：无版本门槛，但 15.4+ 必须走 mediaremote-adapter 资产
  （Apple 限制直接加载，PORTING.md §4b 探针②）。
- 其余能力（设备枚举/事件、输入采集、频谱、PCM）无门槛。

## 3. TCC 权限与 Info.plist

实测（macOS 26.6.2，2026-09-04，PORTING.md §4b 探针④）：从 GUI 应用 spawn 的
**未签名 CLI 子进程**创建 Core Audio process tap **不触发 TCC 弹窗**，`AudioCapture`
服务重置后行为不变。即当前形态下用户无感知。

防御性要求（Apple 历史上逐版本收紧权限检查，15.4 的 MediaRemote 封锁是前车之鉴）——
宿主 `.app` 的 `Info.plist` 应包含：

```xml
<key>NSAudioCaptureUsageDescription</key>
<string>用于音频可视化与音频捕捉。</string>
<key>NSMicrophoneUsageDescription</key>
<string>用于麦克风采集（defaultInput 捕捉）。</string>
```

- `NSAudioCaptureUsageDescription`：若未来 macOS 对 process tap 启用 TCC 检查，
  弹窗将归属宿主 `.app`（spawn 的 sidecar 子进程的"负责进程"是宿主）。
- `NSMicrophoneUsageDescription`：`defaultInput`/`ca:in:` 输入设备采集预期触发
  麦克风权限，同样归属宿主 `.app`。首次使用输入采集时宿主应准备好处理首次弹窗
  （授权后可能需重启捕捉会话）。

**不要**给 sidecar 裸二进制嵌 Info.plist 声明这些键——TCC 归宿主，sidecar 声明无效。

## 4. 签名与公证

- `MediaRemoteAdapter.framework` 是 ad-hoc 签名的；宿主对 `.app` 做 Developer ID
  签名时需连同 `Contents/Resources/` 下的 framework 一起 `codesign --force --deep
  --sign "Developer ID Application: …"`（或显式对 framework 先签再签 app）。
- 公证（notarization）时把 framework 一并提交即可；perl 脚本是纯文本无需签名。
- sidecar 二进制本身随宿主签名（作为资源嵌入时）或独立 ad-hoc 签名均可。

## 5. 已知限制（如实呈现）

- 输入设备采集（`ca:in:`）在本机（无输入设备的 Mac mini）未实测，走标准 HAL
  IOProc 输入路径。
- 进程树新子进程加入捕捉需要重建 tap（实测空隙 ~230ms，含 200ms 退避）；macOS 26
  起 tap 启用 `processRestoreEnabled` 减少同 bundle 进程退出/重启造成的重建。
- 媒体是单一 now-playing 模型：会话列表最多 1 个（当前播放应用），无 SMTC 的多
  会话概念；封面直接来自 now-playing 数据（无 `artworkUrl` 字段，恒 null）。
