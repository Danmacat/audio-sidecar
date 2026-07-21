//! Unsolicited events pushed sidecar -> host.

use serde::{Deserialize, Serialize};

use super::types::*;

/// Machine-readable reason attached to `capture.state` transitions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct StateReason {
    /// e.g. `deviceInvalidated`, `deviceRemoved`, `defaultDeviceChanged`,
    /// `processExited`, `initFailed`, `panic`.
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum MediaChangeKind {
    MediaProperties,
    PlaybackInfo,
    Timeline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum Event {
    #[serde(rename = "device.added")]
    DeviceAdded { device: DeviceInfo },
    #[serde(rename = "device.removed")]
    DeviceRemoved { device_id: String },
    #[serde(rename = "device.stateChanged")]
    DeviceStateChanged {
        device_id: String,
        state: DeviceState,
    },
    #[serde(rename = "device.defaultChanged")]
    DeviceDefaultChanged {
        kind: DeviceKind,
        role: DefaultRole,
        device_id: Option<String>,
    },

    #[serde(rename = "capture.state")]
    CaptureState {
        capture_id: String,
        state: CaptureState,
        #[serde(skip_serializing_if = "Option::is_none")]
        device_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        format: Option<AudioFormat>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<StateReason>,
    },
    #[serde(rename = "capture.spectrum")]
    CaptureSpectrum {
        capture_id: String,
        /// Per-capture counter; gaps mean frames were dropped under backpressure.
        #[cfg_attr(feature = "ts-export", ts(type = "number"))]
        seq: u64,
        #[cfg_attr(feature = "ts-export", ts(type = "number"))]
        timestamp_ms: u64,
        /// Per-channel RMS of this tick's time-domain samples, 0..1.
        rms: Vec<f32>,
        /// `bands[channel][band]`, low -> high frequency, values 0..1.
        bands: Vec<Vec<f32>>,
    },
    #[serde(rename = "capture.pcm")]
    CapturePcm {
        capture_id: String,
        #[cfg_attr(feature = "ts-export", ts(type = "number"))]
        seq: u64,
        #[cfg_attr(feature = "ts-export", ts(type = "number"))]
        timestamp_ms: u64,
        /// Cumulative frame index of the first frame in this chunk.
        #[cfg_attr(feature = "ts-export", ts(type = "number"))]
        first_sample_index: u64,
        sample_rate: u32,
        channels: u16,
        format: PcmFormat,
        data_base64: String,
    },

    #[serde(rename = "media.sessionsChanged")]
    MediaSessionsChanged {
        sessions: Vec<MediaSession>,
        current_session_id: Option<String>,
    },
    #[serde(rename = "media.currentChanged")]
    MediaCurrentChanged {
        current_session_id: Option<String>,
        session: Option<MediaSession>,
    },
    #[serde(rename = "media.sessionUpdated")]
    MediaSessionUpdated {
        session: MediaSession,
        changed: Vec<MediaChangeKind>,
    },

    #[serde(rename = "sidecar.exiting")]
    SidecarExiting { reason: ExitReason },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_envelope_shape() {
        let ev = Event::DeviceRemoved {
            device_id: "dev-1".into(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert_eq!(
            s,
            r#"{"event":"device.removed","data":{"deviceId":"dev-1"}}"#
        );
    }

    #[test]
    fn spectrum_event_roundtrip() {
        let ev = Event::CaptureSpectrum {
            capture_id: "cap-1".into(),
            seq: 7,
            timestamp_ms: 1234,
            rms: vec![0.5, 0.25],
            bands: vec![vec![0.1, 0.2], vec![0.3, 0.4]],
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""event":"capture.spectrum""#));
        assert!(s.contains(r#""captureId":"cap-1""#));
        let back: Event = serde_json::from_str(&s).unwrap();
        match back {
            Event::CaptureSpectrum { seq, bands, .. } => {
                assert_eq!(seq, 7);
                assert_eq!(bands.len(), 2);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn capture_state_omits_null_fields() {
        let ev = Event::CaptureState {
            capture_id: "cap-1".into(),
            state: CaptureState::Stopped,
            device_id: None,
            format: None,
            reason: None,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(!s.contains("reason"));
        assert!(!s.contains("format"));
    }
}
