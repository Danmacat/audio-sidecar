//! Per-method params/result payloads.

use serde::{Deserialize, Serialize};

use super::types::*;

// hello ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct HelloParams {
    #[serde(default)]
    pub client: Option<ClientInfo>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct HelloResult {
    pub name: String,
    pub version: String,
    pub protocol_version: u32,
    pub platform: String,
    pub os_version: String,
    pub pid: u32,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub started_at_ms: u64,
    pub capabilities: Capabilities,
    pub limits: Limits,
    /// Effective artwork cache directory (`--artwork-dir`), or null when
    /// caching is disabled. Bare `--artwork-dir` resolves to a sidecar-managed
    /// temp location reported here.
    pub artwork_dir: Option<String>,
}

// devices -------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct DevicesListParams {
    #[serde(default)]
    pub kinds: Option<Vec<DeviceKind>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct DevicesListResult {
    pub devices: Vec<DeviceInfo>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct DevicesGetDefaultParams {
    pub kind: DeviceKind,
    #[serde(default)]
    pub role: DefaultRole,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct DevicesGetDefaultResult {
    pub device: DeviceInfo,
}

// processes -----------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct ProcessesListAudioResult {
    pub processes: Vec<AudioProcessInfo>,
}

// capture -------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct CaptureStartParams {
    pub source: CaptureSource,
    #[serde(default)]
    pub spectrum: Option<SpectrumConfig>,
    #[serde(default)]
    pub pcm: Option<PcmConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct CaptureStartResult {
    pub capture_id: String,
    pub state: CaptureState,
    pub device_id: Option<String>,
    pub format: AudioFormat,
    pub spectrum: SpectrumConfig,
    pub pcm: PcmConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct CaptureStopParams {
    pub capture_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct CaptureListResult {
    pub captures: Vec<CaptureInfo>,
}

// media ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct MediaGetSessionsResult {
    pub sessions: Vec<MediaSession>,
    pub current_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct MediaGetCurrentResult {
    pub session: Option<MediaSession>,
}

fn d_max_artwork_bytes() -> u64 {
    2_000_000
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct MediaGetArtworkParams {
    pub session_id: String,
    #[serde(default = "d_max_artwork_bytes")]
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub max_bytes: u64,
    /// When set: instead of returning base64, write the artwork into this
    /// DIRECTORY as `<hash>.<ext>` (atomic, content-addressed — same scheme
    /// and shared cache as `--artwork-dir`) and return `file` + `hash`.
    #[serde(default)]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub write_to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct MediaGetArtworkResult {
    pub content_type: String,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub byte_length: u64,
    /// Present unless `writeTo` was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub data_base64: Option<String>,
    /// Absolute path of the written file (only with `writeTo`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub file: Option<String>,
    /// Content hash — identical images yield identical hashes/paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub hash: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_start_params_minimal() {
        let p: CaptureStartParams =
            serde_json::from_str(r#"{"source":{"type":"defaultOutput"}}"#).unwrap();
        assert_eq!(p.source, CaptureSource::DefaultOutput);
        assert!(p.spectrum.is_none());
        assert!(p.pcm.is_none());
    }

    #[test]
    fn artwork_params_default_max_bytes() {
        let p: MediaGetArtworkParams = serde_json::from_str(r#"{"sessionId":"a#1"}"#).unwrap();
        assert_eq!(p.max_bytes, 2_000_000);
    }
}
