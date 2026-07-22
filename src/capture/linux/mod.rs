//! Linux capture backend built on the PulseAudio API. On PipeWire systems the
//! same API is provided by pipewire-pulse.

pub mod devices;
pub(crate) mod pulse;
mod stream;

use crate::protocol::RpcError;

use super::{CaptureBackend, CaptureSpec, SessionThreads, SessionWiring};

pub struct LinuxBackend;

impl CaptureBackend for LinuxBackend {
    fn spawn_capture(
        &self,
        spec: CaptureSpec,
        wiring: SessionWiring,
    ) -> Result<SessionThreads, RpcError> {
        stream::spawn(spec, wiring)
    }
}

#[cfg(test)]
mod probe;
