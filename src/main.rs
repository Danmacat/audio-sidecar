//! audio-sidecar: audio capture + spectrum + media session sidecar.
//!
//! NDJSON over stdio: requests in on stdin, responses/events out on stdout,
//! logs on stderr only. Exits when stdin closes.

#![deny(unsafe_code)]
#![deny(clippy::print_stdout, clippy::dbg_macro)]

mod capture;
mod dsp;
mod media;
mod protocol;
mod rpc;
mod util;

use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::capture::manager::ManagerHandle;
use crate::protocol::events::Event;
use crate::protocol::types::{Capabilities, ExitReason, Limits};
use crate::rpc::writer::EventTx;
use crate::rpc::{AppState, DeviceService, MediaService};

#[derive(Parser, Debug)]
#[command(
    name = "audio-sidecar",
    version,
    about = "Audio capture/spectrum/media sidecar"
)]
struct Args {
    /// Log level filter (error|warn|info|debug|trace); RUST_LOG overrides.
    #[arg(long, default_value = "info")]
    log_level: String,
    /// Also append logs to this file.
    #[arg(long)]
    log_file: Option<std::path::PathBuf>,
    /// IPC transport (only "stdio" in protocol v1).
    #[arg(long, default_value = "stdio")]
    transport: String,
    /// Reject request lines longer than this many bytes.
    #[arg(long, default_value_t = 1_048_576)]
    max_line_bytes: usize,
    /// Start a capture at boot (repeatable). Value is JSON: either a
    /// CaptureSource (e.g. '{"type":"defaultOutput"}') or full
    /// CaptureStartParams ('{"source":{...},"spectrum":{...}}'). Sugar over
    /// `capture.start` — resulting captureIds arrive via `capture.state`
    /// events and `capture.list`.
    #[arg(long = "capture", value_name = "JSON")]
    captures: Vec<String>,
    /// Cache media artwork into this directory as `<hash>.<ext>` (atomic
    /// writes); sessions then carry `artworkFile`/`artworkHash` and emit
    /// `media.sessionUpdated` with changed=["artwork"].
    #[arg(long)]
    artwork_dir: Option<std::path::PathBuf>,
    /// Print the hello result to stdout and exit (smoke test).
    #[arg(long)]
    print_hello: bool,
}

#[cfg(windows)]
const PLATFORM: &str = "windows";
#[cfg(target_os = "linux")]
const PLATFORM: &str = "linux";
#[cfg(target_os = "macos")]
const PLATFORM: &str = "macos";
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
const PLATFORM: &str = "unknown";

#[cfg(windows)]
fn os_version() -> String {
    let v = windows_version::OsVersion::current();
    format!("{}.{}.{}", v.major, v.minor, v.build)
}

#[cfg(not(windows))]
fn os_version() -> String {
    String::new()
}

#[cfg(windows)]
fn platform_capabilities() -> Capabilities {
    let process_loopback = windows_version::OsVersion::current().build >= 19041;
    Capabilities {
        device_capture: true,
        device_loopback: true,
        follow_default_output: true,
        follow_default_input: true,
        process_loopback,
        process_loopback_exclude: process_loopback,
        audio_process_list: true,
        device_events: true,
        media_sessions: true,
        media_artwork: true,
        spectrum: true,
        pcm_stream: true,
    }
}

#[cfg(not(windows))]
fn platform_capabilities() -> Capabilities {
    Capabilities::NONE
}

/// Everything platform-specific that main wires together.
struct Platform {
    devices: Option<Arc<dyn DeviceService>>,
    media: Option<Arc<dyn MediaService>>,
    manager: ManagerHandle,
    /// Ask platform threads to quit and reap them (blocking, bounded by the
    /// caller's timeout + the shutdown watchdog).
    shutdown: Box<dyn FnOnce() + Send>,
}

#[cfg(windows)]
fn init_platform(
    events: EventTx,
    capabilities: Capabilities,
    limits: Limits,
    artwork_dir: Option<std::path::PathBuf>,
) -> Platform {
    let (mgr_tx, mgr_rx) = tokio::sync::mpsc::unbounded_channel();
    let dev = capture::windows::devices::spawn(events.clone(), mgr_tx.clone());
    let devices: Arc<dyn DeviceService> = Arc::new(dev.handle.clone());
    let manager = capture::manager::spawn_with_channel(
        mgr_tx,
        mgr_rx,
        Arc::new(capture::windows::WindowsBackend),
        Some(devices.clone()),
        events.clone(),
        capabilities,
        limits,
    );
    let media_worker = media::windows::spawn(events, artwork_dir);
    let media: Arc<dyn MediaService> = Arc::new(media_worker.handle.clone());

    let dev_handle = dev.handle.clone();
    let media_handle = media_worker.handle.clone();
    let shutdown = Box::new(move || {
        dev_handle.quit();
        media_handle.quit();
        let _ = dev.join.join();
        let _ = media_worker.join.join();
    });

    Platform {
        devices: Some(devices),
        media: Some(media),
        manager,
        shutdown,
    }
}

#[cfg(not(windows))]
fn init_platform(
    events: EventTx,
    capabilities: Capabilities,
    limits: Limits,
    _artwork_dir: Option<std::path::PathBuf>,
) -> Platform {
    let manager = capture::manager::spawn(
        Arc::new(capture::StubBackend),
        None,
        events,
        capabilities,
        limits,
    );
    Platform {
        devices: None,
        media: None,
        manager,
        shutdown: Box::new(|| {}),
    }
}

fn init_tracing(args: &Args) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(args.log_level.clone()));
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false);
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer);
    match &args.log_file {
        Some(path) => match std::fs::File::create(path) {
            Ok(file) => {
                let (writer, guard) = tracing_appender::non_blocking(file);
                registry
                    .with(
                        tracing_subscriber::fmt::layer()
                            .with_writer(writer)
                            .with_ansi(false)
                            .with_target(false),
                    )
                    .init();
                Some(guard)
            }
            Err(e) => {
                registry.init();
                warn!("cannot open log file {}: {e}", path.display());
                None
            }
        },
        None => {
            registry.init();
            None
        }
    }
}

/// Accept either a bare `CaptureSource` or full `CaptureStartParams`.
fn parse_capture_arg(raw: &str) -> Result<protocol::methods::CaptureStartParams, String> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    if value.get("source").is_some() {
        serde_json::from_value(value).map_err(|e| e.to_string())
    } else {
        let source = serde_json::from_value(value).map_err(|e| e.to_string())?;
        Ok(protocol::methods::CaptureStartParams {
            source,
            spectrum: None,
            pcm: None,
        })
    }
}

fn print_hello() {
    let hello = protocol::methods::HelloResult {
        name: env!("CARGO_PKG_NAME").to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: protocol::PROTOCOL_VERSION,
        platform: PLATFORM.to_string(),
        os_version: os_version(),
        pid: std::process::id(),
        started_at_ms: util::now_ms(),
        capabilities: platform_capabilities(),
        limits: Limits::default(),
    };
    let json = serde_json::to_string(&hello).expect("hello serializes");
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{json}");
}

fn main() {
    let args = Args::parse();
    let _log_guard = init_tracing(&args);
    if args.print_hello {
        print_hello();
        return;
    }
    if args.transport != "stdio" {
        tracing::error!(
            "unsupported transport \"{}\" (only \"stdio\")",
            args.transport
        );
        std::process::exit(2);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    let code = runtime.block_on(async_main(args));
    std::process::exit(code);
}

async fn async_main(args: Args) -> i32 {
    info!(
        version = env!("CARGO_PKG_VERSION"),
        platform = PLATFORM,
        os = %os_version(),
        "audio-sidecar starting"
    );
    let (events, _writer_join) = rpc::writer::spawn(256);
    let capabilities = platform_capabilities();
    let limits = Limits::default();
    let platform = init_platform(
        events.clone(),
        capabilities,
        limits.clone(),
        args.artwork_dir.clone(),
    );

    let state = Arc::new(AppState {
        platform: PLATFORM,
        os_version: os_version(),
        started_at_ms: util::now_ms(),
        capabilities,
        limits,
        devices: platform.devices,
        media: platform.media,
        manager: platform.manager,
        cancel: CancellationToken::new(),
        shutdown_reason: Mutex::new(None),
    });

    // `--capture` sugar: start the requested captures as if the host had sent
    // `capture.start`. Failures are logged, not fatal — the host still gets
    // the full protocol and can inspect/retry.
    for (i, raw) in args.captures.iter().enumerate() {
        match parse_capture_arg(raw) {
            Ok(params) => {
                let manager = state.manager.clone();
                tokio::spawn(async move {
                    match manager.start(params).await {
                        Ok(r) => info!(capture_id = r.capture_id, "--capture #{i} started"),
                        Err(e) => {
                            tracing::error!(code = ?e.code, "--capture #{i} failed: {}", e.message)
                        }
                    }
                });
            }
            Err(e) => tracing::error!("--capture #{i} is not valid JSON: {e}"),
        }
    }

    let reason = rpc::router::run(state.clone(), events.clone(), args.max_line_bytes).await;

    // Watchdog: if graceful teardown hangs, kill the process anyway.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(3));
        std::process::exit(1);
    });

    info!(?reason, "shutting down");
    events.send_event(&Event::SidecarExiting { reason });
    state.manager.stop_all(Duration::from_millis(2500)).await;
    let shutdown = platform.shutdown;
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(shutdown),
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_millis(500), events.flush()).await;
    if reason == ExitReason::Fatal { 1 } else { 0 }
}
