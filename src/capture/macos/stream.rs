//! Per-session Core Audio capture thread. Device loopback/input arrives in
//! M2 and process taps in M3 (PORTING.md §7); until then the backend reports
//! no capture support via capabilities.

use crate::protocol::RpcError;

use super::{CaptureSpec, SessionThreads, SessionWiring};

pub fn spawn(_spec: CaptureSpec, _wiring: SessionWiring) -> Result<SessionThreads, RpcError> {
    Err(RpcError::unsupported())
}
