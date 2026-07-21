//! RPC surface: stdout writer task, stdin router, and the service seams the
//! router talks to (platform backends implement these).

pub mod router;
pub mod writer;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::capture::manager::ManagerHandle;
use crate::protocol::methods::{
    HelloResult, MediaGetArtworkResult, MediaGetCurrentResult, MediaGetSessionsResult,
};
use crate::protocol::types::{
    AudioProcessInfo, Capabilities, DefaultRole, DeviceInfo, DeviceKind, ExitReason, Limits,
};
use crate::protocol::{PROTOCOL_VERSION, RpcError};

pub type SvcFuture<T> = Pin<Box<dyn Future<Output = Result<T, RpcError>> + Send>>;

/// Device queries served by a platform thread (COM-safe there, awaited here).
pub trait DeviceService: Send + Sync + 'static {
    fn list(&self, kinds: Option<Vec<DeviceKind>>) -> SvcFuture<Vec<DeviceInfo>>;
    fn get_default(&self, kind: DeviceKind, role: DefaultRole) -> SvcFuture<DeviceInfo>;
    fn list_audio_processes(&self) -> SvcFuture<Vec<AudioProcessInfo>>;
}

pub trait MediaService: Send + Sync + 'static {
    fn get_sessions(&self) -> SvcFuture<MediaGetSessionsResult>;
    fn get_current(&self) -> SvcFuture<MediaGetCurrentResult>;
    fn get_artwork(
        &self,
        session_id: String,
        max_bytes: u64,
        write_to: Option<String>,
    ) -> SvcFuture<MediaGetArtworkResult>;
}

pub struct AppState {
    pub platform: &'static str,
    pub os_version: String,
    pub started_at_ms: u64,
    pub capabilities: Capabilities,
    pub limits: Limits,
    pub devices: Option<Arc<dyn DeviceService>>,
    pub media: Option<Arc<dyn MediaService>>,
    pub manager: ManagerHandle,
    pub artwork_dir: Option<String>,
    pub cancel: CancellationToken,
    pub shutdown_reason: Mutex<Option<ExitReason>>,
}

impl AppState {
    pub fn hello_result(&self) -> HelloResult {
        HelloResult {
            name: env!("CARGO_PKG_NAME").to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
            platform: self.platform.to_string(),
            os_version: self.os_version.clone(),
            pid: std::process::id(),
            started_at_ms: self.started_at_ms,
            capabilities: self.capabilities,
            limits: self.limits.clone(),
            artwork_dir: self.artwork_dir.clone(),
        }
    }

    pub fn request_shutdown(&self, reason: ExitReason) {
        let mut guard = self
            .shutdown_reason
            .lock()
            .expect("shutdown_reason poisoned");
        if guard.is_none() {
            *guard = Some(reason);
        }
        drop(guard);
        self.cancel.cancel();
    }
}
