//! Capture subsystem: platform backends spawn per-session OS threads that
//! feed a lock-free ring into a platform-independent DSP worker.

pub mod manager;
pub mod session;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(windows)]
#[allow(unsafe_code)]
pub mod windows;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use tokio::sync::{mpsc, oneshot};

use crate::protocol::events::StateReason;
use crate::protocol::types::{AudioFormat, CaptureStats, PcmConfig, SpectrumConfig};
use crate::protocol::{ErrorCode, RpcError};
use crate::rpc::writer::EventTx;

/// Shared cumulative counters, written by the session threads and read by
/// `capture.list`.
#[derive(Debug, Default)]
pub struct SessionStats {
    pub frames_emitted: AtomicU64,
    pub frames_dropped: AtomicU64,
    pub pcm_chunks_emitted: AtomicU64,
    pub ring_overflows: AtomicU64,
    pub starved_ticks: AtomicU64,
    pub restarts: AtomicU32,
}

impl SessionStats {
    pub fn snapshot(&self) -> CaptureStats {
        CaptureStats {
            frames_emitted: self.frames_emitted.load(Ordering::Relaxed),
            frames_dropped: self.frames_dropped.load(Ordering::Relaxed),
            pcm_chunks_emitted: self.pcm_chunks_emitted.load(Ordering::Relaxed),
            ring_overflows: self.ring_overflows.load(Ordering::Relaxed),
            starved_ticks: self.starved_ticks.load(Ordering::Relaxed),
            restarts: self.restarts.load(Ordering::Relaxed),
        }
    }
}

/// Source after default-device resolution: what the io thread actually opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedSource {
    Device {
        device_id: String,
    },
    /// `exclude: false` captures the pid's process tree; `exclude: true`
    /// captures everything except it (the OS offers exactly these two modes).
    Process {
        pid: u32,
        exclude: bool,
    },
}

#[derive(Debug, Clone)]
pub struct ReadyInfo {
    pub device_id: Option<String>,
    pub format: AudioFormat,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum SessionError {
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    #[error("activation failed: {0}")]
    Activation(String),
    #[error("capture start timed out")]
    Timeout,
    #[error("capture thread terminated unexpectedly")]
    ThreadDied,
    #[error("target process exited")]
    ProcessExited,
    #[error("capture thread panicked: {0}")]
    Panic(String),
    #[error("audio device error: {0}")]
    Device(String),
}

impl SessionError {
    pub fn to_rpc(&self) -> RpcError {
        match self {
            SessionError::DeviceNotFound(id) => {
                RpcError::new(ErrorCode::DeviceNotFound, format!("device not found: {id}"))
            }
            SessionError::Activation(m) => {
                RpcError::new(ErrorCode::ActivationFailed, m.clone()).retryable()
            }
            SessionError::Timeout => {
                RpcError::new(ErrorCode::Timeout, "capture start timed out").retryable()
            }
            SessionError::ProcessExited => {
                RpcError::new(ErrorCode::ProcessNotFound, "target process exited")
            }
            SessionError::ThreadDied | SessionError::Panic(_) | SessionError::Device(_) => {
                RpcError::new(ErrorCode::OsError, self.to_string()).retryable()
            }
        }
    }

    pub fn reason(&self) -> StateReason {
        let code = match self {
            SessionError::DeviceNotFound(_) => "deviceNotFound",
            SessionError::Activation(_) => "initFailed",
            SessionError::Timeout => "timeout",
            SessionError::ThreadDied => "threadDied",
            SessionError::ProcessExited => "processExited",
            SessionError::Panic(_) => "panic",
            SessionError::Device(_) => "deviceInvalidated",
        };
        StateReason {
            code: code.into(),
            message: self.to_string(),
        }
    }
}

pub struct CaptureSpec {
    pub capture_id: String,
    pub source: ResolvedSource,
    pub spectrum: SpectrumConfig,
    pub pcm: PcmConfig,
}

/// Everything a backend needs to wire a spawned session into the system.
pub struct SessionWiring {
    pub events: EventTx,
    pub stats: Arc<SessionStats>,
    pub ready: oneshot::Sender<Result<ReadyInfo, SessionError>>,
    pub stop: Arc<AtomicBool>,
    /// Channel back to the manager actor for mid-run fatal errors.
    pub manager: mpsc::UnboundedSender<manager::ManagerCmd>,
}

pub struct SessionThreads {
    pub io: std::thread::JoinHandle<()>,
    pub worker: std::thread::JoinHandle<()>,
}

impl SessionThreads {
    pub fn join_both(self) {
        let _ = self.io.join();
        let _ = self.worker.join();
    }
}

pub trait CaptureBackend: Send + Sync + 'static {
    fn spawn_capture(
        &self,
        spec: CaptureSpec,
        wiring: SessionWiring,
    ) -> Result<SessionThreads, RpcError>;
}

/// Backend for platforms whose capture support is not implemented yet.
#[cfg_attr(windows, allow(dead_code))]
pub struct StubBackend;

impl CaptureBackend for StubBackend {
    fn spawn_capture(
        &self,
        _spec: CaptureSpec,
        _wiring: SessionWiring,
    ) -> Result<SessionThreads, RpcError> {
        Err(RpcError::unsupported())
    }
}
