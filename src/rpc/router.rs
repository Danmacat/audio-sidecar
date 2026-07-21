//! stdin line reader and request dispatch. Requests run concurrently
//! (spawned per request); responses correlate by `id` and may interleave.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{debug, info, warn};

use crate::protocol::methods::*;
use crate::protocol::types::ExitReason;
use crate::protocol::{ErrorCode, RawRequest, Response, RpcError};

use super::AppState;
use super::writer::EventTx;

/// Run until stdin EOF or cancellation. Returns the exit reason.
pub async fn run(state: Arc<AppState>, tx: EventTx, max_line_bytes: usize) -> ExitReason {
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    loop {
        line.clear();
        let read = tokio::select! {
            _ = state.cancel.cancelled() => {
                return state
                    .shutdown_reason
                    .lock()
                    .expect("shutdown_reason poisoned")
                    .unwrap_or(ExitReason::Shutdown);
            }
            r = reader.read_line(&mut line) => r,
        };
        match read {
            Ok(0) => {
                info!("stdin closed; shutting down");
                return ExitReason::StdinClosed;
            }
            Ok(_) => {}
            Err(e) => {
                warn!("stdin read error: {e}");
                return ExitReason::Fatal;
            }
        }
        // Trim whitespace plus any BOM a shell may prepend when piping.
        let trimmed = line.trim().trim_start_matches('\u{feff}');
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.len() > max_line_bytes {
            tx.send_response(&Response::err(
                None,
                RpcError::new(
                    ErrorCode::ParseError,
                    format!("line exceeds {max_line_bytes} bytes"),
                ),
            ));
            continue;
        }
        let raw: RawRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                tx.send_response(&Response::err(
                    None,
                    RpcError::new(ErrorCode::ParseError, format!("invalid JSON: {e}")),
                ));
                continue;
            }
        };
        dispatch(state.clone(), tx.clone(), raw);
    }
}

fn dispatch(state: Arc<AppState>, tx: EventTx, raw: RawRequest) {
    let Some(method) = raw.method.clone() else {
        tx.send_response(&Response::err(
            raw.id,
            RpcError::new(ErrorCode::InvalidRequest, "missing \"method\""),
        ));
        return;
    };
    if raw.id.is_none() {
        tx.send_response(&Response::err(
            None,
            RpcError::new(
                ErrorCode::InvalidRequest,
                format!("request \"{method}\" has no id"),
            ),
        ));
        return;
    }
    tokio::spawn(async move {
        debug!(method = %method, "request");
        let result = route(&state, &method, raw.params).await;
        let is_shutdown = method == "shutdown" && result.is_ok();
        match result {
            Ok(value) => tx.send_response(&Response::ok(raw.id, value)),
            Err(e) => tx.send_response(&Response::err(raw.id, e)),
        }
        if is_shutdown {
            state.request_shutdown(ExitReason::Shutdown);
        }
    });
}

fn parse_params<T: serde::de::DeserializeOwned>(params: Option<Value>) -> Result<T, RpcError> {
    let value = params.unwrap_or_else(|| json!({}));
    serde_json::from_value(value).map_err(|e| RpcError::invalid_params(e.to_string()))
}

fn to_value<T: serde::Serialize>(v: T) -> Result<Value, RpcError> {
    serde_json::to_value(v).map_err(|e| RpcError::internal(e.to_string()))
}

async fn route(
    state: &Arc<AppState>,
    method: &str,
    params: Option<Value>,
) -> Result<Value, RpcError> {
    match method {
        "hello" => {
            let p: HelloParams = parse_params(params)?;
            if let Some(client) = &p.client {
                info!(name = %client.name, version = %client.version, "host connected");
            }
            to_value(state.hello_result())
        }
        "ping" => Ok(json!({})),
        "devices.list" => {
            let p: DevicesListParams = parse_params(params)?;
            let svc = state.devices.as_ref().ok_or_else(RpcError::unsupported)?;
            let devices = svc.list(p.kinds).await?;
            to_value(DevicesListResult { devices })
        }
        "devices.getDefault" => {
            let p: DevicesGetDefaultParams = parse_params(params)?;
            let svc = state.devices.as_ref().ok_or_else(RpcError::unsupported)?;
            let device = svc.get_default(p.kind, p.role).await?;
            to_value(DevicesGetDefaultResult { device })
        }
        "processes.listAudio" => {
            let svc = state.devices.as_ref().ok_or_else(RpcError::unsupported)?;
            if !state.capabilities.audio_process_list {
                return Err(RpcError::unsupported());
            }
            let processes = svc.list_audio_processes().await?;
            to_value(ProcessesListAudioResult { processes })
        }
        "capture.start" => {
            let p: CaptureStartParams = parse_params(params)?;
            let result = state.manager.start(p).await?;
            to_value(result)
        }
        "capture.stop" => {
            let p: CaptureStopParams = parse_params(params)?;
            state.manager.stop(p.capture_id).await?;
            Ok(json!({}))
        }
        "capture.list" => {
            let captures = state.manager.list().await?;
            to_value(CaptureListResult { captures })
        }
        "media.getSessions" => {
            let svc = state.media.as_ref().ok_or_else(RpcError::unsupported)?;
            to_value(svc.get_sessions().await?)
        }
        "media.getCurrent" => {
            let svc = state.media.as_ref().ok_or_else(RpcError::unsupported)?;
            to_value(svc.get_current().await?)
        }
        "media.getArtwork" => {
            let p: MediaGetArtworkParams = parse_params(params)?;
            let svc = state.media.as_ref().ok_or_else(RpcError::unsupported)?;
            to_value(svc.get_artwork(p.session_id, p.max_bytes).await?)
        }
        "shutdown" => Ok(json!({})),
        other => Err(RpcError::new(
            ErrorCode::MethodNotFound,
            format!("unknown method \"{other}\""),
        )),
    }
}
