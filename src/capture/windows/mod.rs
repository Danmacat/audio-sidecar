//! Windows capture backend: WASAPI device loopback/capture and process
//! loopback (Win10 2004+), plus the device-manager thread and audio session
//! enumeration.

pub mod audio_sessions;
pub mod devices;
pub mod stream;

use crate::protocol::RpcError;

use super::{CaptureBackend, CaptureSpec, SessionThreads, SessionWiring};

pub struct WindowsBackend;

impl CaptureBackend for WindowsBackend {
    fn spawn_capture(
        &self,
        spec: CaptureSpec,
        wiring: SessionWiring,
    ) -> Result<SessionThreads, RpcError> {
        stream::spawn(spec, wiring)
    }
}
