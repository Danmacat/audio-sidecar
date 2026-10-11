# audio-sidecar

**English** | [简体中文](./README.zh-CN.md)

[![CI](https://github.com/Danmacat/audio-sidecar/actions/workflows/ci.yml/badge.svg)](https://github.com/Danmacat/audio-sidecar/actions/workflows/ci.yml)
[![release](https://github.com/Danmacat/audio-sidecar/actions/workflows/release.yml/badge.svg)](https://github.com/Danmacat/audio-sidecar/actions/workflows/release.yml)
![License](https://img.shields.io/badge/license-MIT-blue.svg)

A cross-platform **audio sidecar**: a standalone subprocess that gives any host application system-audio capture, live spectrum data, PCM streams, and media-session (now-playing) information. Spawn it from any language — Electron/Node, Python, Go, C#… — and talk to it over a **stdio NDJSON** protocol. All audio DSP happens inside the sidecar; the host only consumes lightweight JSON (the default spectrum feed is ~27 KB/s).

## Features

- **Device capture** — loopback of any output device / capture from input devices, with a "follow the default device" mode that switches seamlessly
- **Process capture** — audio of a specific process (including its child-process tree), or "everything except this process" system-wide (e.g. exclude the host itself)
- **Device enumeration** — full-state lists (active/disabled/unplugged/notPresent) plus hotplug / default-device-changed events
- **Media sessions** — read-only snapshots and change events (title/artist/album/artwork/playback state/progress); artwork on demand or auto-cached to disk
- **Spectrum output** — FFT → log-spaced bands → AGC/noise gate → attack/decay smoothing; defaults to 2×64 bands @30fps in a Wallpaper Engine–compatible layout
- **PCM stream** — optional interleaved s16le/f32le, chunked by sample count (no clock drift); concatenate chunks to get a continuous stream
- **Multiple concurrent captures** — one process, several independent sessions (default limit 8), demultiplexed by `captureId`
- **Backpressure-safe** — a slow host drops frames instead of blocking the audio thread; stdout carries protocol frames only, logs go to stderr

## Platform capabilities

| Capability | Windows | Linux | macOS |
|---|---|---|---|
| Output-device loopback | ✅ WASAPI | ✅ PulseAudio / PipeWire monitor | ✅ Core Audio |
| Input-device capture | ✅ | ✅ | ✅ |
| Follow default device (auto-switch) | ✅ | ✅ | ✅ |
| Process capture (incl. child tree) | ✅ Win10 2004+ | ✅ | ✅ macOS 14.4+ |
| System-wide exclude-process capture | ✅ Win10 2004+ | ❌ `unsupported` | ✅ macOS 14.4+ |
| Device enumeration + hotplug events | ✅ | ✅ | ✅ |
| Audio-process list | ✅ | ✅ | ✅ |
| Media sessions + artwork | ✅ SMTC | ✅ MPRIS | ⚠️ MediaRemote† |
| Spectrum / PCM output | ✅ | ✅ | ✅ |

† macOS media support relies on the bundled [mediaremote-adapter](./assets/macos/mediaremote-adapter) assets (Apple has blocked third-party processes from the MediaRemote framework since macOS 15.4). When the assets are missing, `hello.capabilities` reports `false` and media RPCs return errors — **capture keeps working**. Packaging requirements: [docs/PACKAGING-macos.md](./docs/PACKAGING-macos.md).

Capability detection: call `hello` after startup and feature-detect on the returned `capabilities`; unsupported platform features return an `unsupported` error instead of crashing. The protocol is identical on every platform (see [PROTOCOL.md](./PROTOCOL.md), in Chinese).

Tested on: Windows 11 (26200), Ubuntu (PipeWire 1.6.2 via the PulseAudio compatibility layer), macOS 26.6.2 (arm64).

## Getting the binaries

- **Releases**: pushing a `v*` tag automatically creates a GitHub Release with per-platform archives — `audio-sidecar-linux-x64.tar.gz` / `audio-sidecar-windows-x64.zip` / `audio-sidecar-macos-arm64.tar.gz` (the macOS archive includes the `mediaremote-adapter/` assets; keep the extracted layout side by side).
- **CI artifacts**: every push to master, plus manual runs of the `release` workflow from the Actions tab (any branch), produces downloadable per-platform artifacts.
- **From source**: see [below](#building-from-source).

## Quick start

Host integration (Electron/Node example; full semantics in [PROTOCOL.md](./PROTOCOL.md)):

```ts
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

const sidecar = spawn(sidecarPath, ["--log-level", "info"], { stdio: ["pipe", "pipe", "pipe"] });
createInterface({ input: sidecar.stdout }).on("line", (l) => {
  const msg = JSON.parse(l);
  if (msg.event) onEvent(msg);   // capture.spectrum / device.* / media.* events
  else settlePending(msg.id, msg); // responses settle by id (may arrive out of order)
});
sidecar.stderr.pipe(logStream);  // logs go to stderr only
// Host exits → stdin EOF → the sidecar shuts itself down gracefully
```

The three message shapes (one JSON object per line):

```jsonc
{"id": 1, "method": "capture.start", "params": {"source": {"type": "defaultOutput"}}}   // request
{"id": 1, "result": {"captureId": "..."}}                                                // response
{"event": "capture.spectrum", "data": {"captureId": "...", "bands": [[...], [...]]}}     // event
```

CLI smoke-testing and debugging ([tools/dev-client.mjs](./tools/dev-client.mjs), zero-dependency Node ≥18):

```sh
node tools/dev-client.mjs hello                 # handshake & capabilities
node tools/dev-client.mjs devices               # device list
node tools/dev-client.mjs apps                  # processes with audio sessions
node tools/dev-client.mjs meter --default       # live terminal spectrum of the default output (play some music)
node tools/dev-client.mjs meter --pid 12345     # capture one process; --exclude-pid inverts
node tools/dev-client.mjs watch                 # print all events (try unplugging a device / skipping a track)
node tools/dev-client.mjs media --watch --artwork cover.jpg
node tools/dev-client.mjs pcm --seconds 5 --out cap.raw   # import as raw in Audacity to listen
```

Environment variables: `SIDECAR_BIN` (path to the executable, default `target/debug`) and `SIDECAR_LOG` (log level, default `warn`).

## Building from source

```sh
cargo build --release      # → target/release/audio-sidecar[.exe]
cargo test                 # unit tests (DSP math, protocol serde, PCM encoding)
cargo test --features ts-export   # also regenerate bindings/*.ts
cargo clippy --all-targets -- -D warnings
```

Linux needs the PulseAudio development package (`sudo apt-get install libpulse-dev`); on macOS the build automatically copies the mediaremote-adapter assets next to the binary (see `build.rs`).

## Architecture highlights

- Audio never touches tokio: each session runs two dedicated OS threads (platform audio I/O + DSP worker) connected by a lock-free ring; blocking platform calls are confined to dedicated threads
- Spectrum/PCM frames travel over a bounded channel — a stalled host drops frames instead of back-pressuring audio; responses and state events use a reliable, ordered channel
- "Silence = zero-fill each tick" is the single rule covering startup, idle, and pauses — decay animation and PCM continuity follow naturally
- Device invalidation / default-device changes restart captures automatically (200ms→5s backoff); process exit is left for the host to decide
- `deny(print_stdout)` plus a single-writer task makes stdout protocol pollution a compile-time error

## Project layout

```
src/
├── main.rs                 # CLI, tracing→stderr, platform wiring, graceful shutdown + watchdog
├── protocol/               # all wire types (serde camelCase, exported via ts-rs)
├── rpc/                    # stdin routing, single stdout writer (reliable + droppable frame channels)
├── capture/
│   ├── manager.rs          # session actor: lifecycle, follow-default, restart backoff
│   ├── session.rs          # platform-agnostic DSP worker thread (zero-fill, spectrum, PCM chunking)
│   ├── linux/              # libpulse threaded mainloop: devices / monitor / per-PID capture
│   ├── macos/              # Core Audio: process taps / aggregate devices / IOProc, libproc process trees
│   └── windows/            # WASAPI: device/process capture, dev-mgr thread, session enumeration
├── dsp/                    # pure functions: Hann+FFT, log band splitting, AGC+noise gate, smoothing
├── media/{linux,macos,windows}.rs   # MPRIS / MediaRemote / SMTC media workers
└── util/                   # PCM encoding, float sanitation
tools/dev-client.mjs        # debug/validation client
assets/macos/               # mediaremote-adapter packaging assets (shipped with the binary)
bindings/                   # generated TypeScript protocol types (for host integrations)
```

## Documentation

- [PROTOCOL.md](./PROTOCOL.md) — the stdio NDJSON protocol spec: methods, events, error codes, backpressure, lifecycle (in Chinese)
- [docs/PACKAGING-macos.md](./docs/PACKAGING-macos.md) — macOS packaging guide: asset layout, permissions, signing (in Chinese)
- [README.zh-CN.md](./README.zh-CN.md) — Chinese README
- [bindings/](./bindings) — TypeScript protocol types (generated by `cargo test --features ts-export`)

## License

MIT. The mediaremote-adapter assets bundled for macOS media support are BSD-3-Clause (see [assets/macos/mediaremote-adapter/LICENSE](./assets/macos/mediaremote-adapter/LICENSE)).
