//! Wire protocol: NDJSON envelopes shared by every transport.
//!
//! One JSON object per line. Three shapes, discriminated by key presence:
//! - request:  `{"id": 1, "method": "...", "params": {...}}`
//! - response: `{"id": 1, "result": {...}}` or `{"id": 1, "error": {...}}`
//! - event:    `{"event": "...", "data": {...}}`
//!
//! Unknown fields are ignored on both sides; additive changes do not bump
//! `PROTOCOL_VERSION`, breaking changes do.

pub mod events;
pub mod methods;
pub mod types;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

/// A request id: JSON number or string, echoed back verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Num(serde_json::Number),
    Str(String),
}

/// Incoming request envelope (loosely typed; validated during dispatch).
#[derive(Debug, Deserialize)]
pub struct RawRequest {
    #[serde(default)]
    pub id: Option<RequestId>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub id: Option<RequestId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: Option<RequestId>, result: Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Option<RequestId>, error: RpcError) -> Self {
        Self {
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct RpcError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional, type = "unknown"))]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub retryable: Option<bool>,
}

impl RpcError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
            retryable: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = Some(true);
        self
    }

    pub fn unsupported() -> Self {
        Self::new(ErrorCode::Unsupported, "not supported on this platform")
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidParams, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum ErrorCode {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    Unsupported,
    DeviceNotFound,
    ProcessNotFound,
    ActivationFailed,
    CaptureNotFound,
    CaptureLimitReached,
    SessionNotFound,
    ArtworkUnavailable,
    ArtworkTooLarge,
    Timeout,
    OsError,
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_roundtrip() {
        let r: RawRequest = serde_json::from_str(r#"{"id":1,"method":"ping"}"#).unwrap();
        assert!(matches!(r.id, Some(RequestId::Num(_))));
        let r: RawRequest = serde_json::from_str(r#"{"id":"a-1","method":"ping"}"#).unwrap();
        assert_eq!(r.id, Some(RequestId::Str("a-1".into())));
        let r: RawRequest = serde_json::from_str(r#"{"method":"ping"}"#).unwrap();
        assert!(r.id.is_none());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let r: RawRequest =
            serde_json::from_str(r#"{"id":2,"method":"ping","params":{},"extra":true}"#).unwrap();
        assert_eq!(r.method.as_deref(), Some("ping"));
    }

    #[test]
    fn error_serialization() {
        let resp = Response::err(
            Some(RequestId::Num(3.into())),
            RpcError::new(ErrorCode::DeviceNotFound, "no such device").retryable(),
        );
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains(r#""code":"deviceNotFound""#));
        assert!(s.contains(r#""retryable":true"#));
        assert!(!s.contains("result"));
    }
}
