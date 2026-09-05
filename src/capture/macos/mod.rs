//! macOS capture backend built on the Core Audio process-tap API
//! (macOS 14.4+; see PORTING.md §4/§4b for the probe record).

pub mod devices;
pub mod hal;
pub(crate) mod sck;
pub(crate) mod stream;

use crate::protocol::RpcError;

use super::{CaptureBackend, CaptureSpec, SessionThreads, SessionWiring};

pub struct MacOsBackend;

impl CaptureBackend for MacOsBackend {
    fn spawn_capture(
        &self,
        spec: CaptureSpec,
        wiring: SessionWiring,
    ) -> Result<SessionThreads, RpcError> {
        stream::spawn(spec, wiring)
    }
}
