//! Shared protocol data types. All wire names are camelCase.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum DeviceKind {
    Render,
    Capture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum DeviceState {
    Active,
    Disabled,
    NotPresent,
    Unplugged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct DeviceInfo {
    /// Stable OS endpoint id (WASAPI endpoint id on Windows).
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub is_default: bool,
    pub is_default_communications: bool,
    pub state: DeviceState,
    /// Shared-mode mix format; `null` unless the device is active.
    pub format: Option<AudioFormat>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum DefaultRole {
    Console,
    Multimedia,
    Communications,
}

impl Default for DefaultRole {
    fn default() -> Self {
        Self::Multimedia
    }
}

// ---------------------------------------------------------------------------
// Audio processes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum SessionActivity {
    Active,
    Inactive,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct AudioProcessInfo {
    pub pid: u32,
    /// Executable file name, e.g. `msedge.exe`.
    pub name: String,
    /// Full executable path when the process could be opened.
    pub executable: Option<String>,
    /// Max activity across the process's audio sessions (expired excluded).
    pub state: SessionActivity,
    /// Render endpoints this process has sessions on.
    pub device_ids: Vec<String>,
    pub display_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Capture configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum CaptureSource {
    /// A specific endpoint: render id -> loopback, capture id -> mic capture.
    Device { device_id: String },
    /// Follow the default render device (multimedia role), auto-switch.
    DefaultOutput,
    /// Follow the default capture device.
    DefaultInput,
    /// Capture one process (and its child-process tree — browsers render
    /// audio in utility children). Windows 10 2004+.
    Process { pid: u32 },
    /// Capture the whole system EXCEPT this process tree (e.g. everything
    /// but your own app). Windows 10 2004+.
    SystemExcludingProcess { pid: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum ChannelMode {
    Stereo,
    Mono,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum ScaleMode {
    Db,
    Sqrt,
    Linear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum BandMode {
    Max,
    Avg,
}

fn d_true() -> bool {
    true
}
fn d_bands() -> u32 {
    64
}
fn d_fps() -> u32 {
    30
}
fn d_channel_mode() -> ChannelMode {
    ChannelMode::Stereo
}
fn d_fft_size() -> u32 {
    2048
}
fn d_min_freq() -> f32 {
    25.0
}
fn d_max_freq() -> f32 {
    16000.0
}
fn d_scale() -> ScaleMode {
    ScaleMode::Db
}
fn d_db_floor() -> f32 {
    -60.0
}
fn d_band_mode() -> BandMode {
    BandMode::Max
}
fn d_attack() -> f32 {
    0.8
}
fn d_decay() -> f32 {
    0.2
}
fn d_gain() -> f32 {
    1.0
}

pub const FFT_SIZES: [u32; 5] = [512, 1024, 2048, 4096, 8192];

/// Spectrum processing configuration. Every field is optional on the wire;
/// the resolved values are echoed back in the `capture.start` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct SpectrumConfig {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_bands")]
    pub bands: u32,
    #[serde(default = "d_fps")]
    pub fps: u32,
    #[serde(default = "d_channel_mode")]
    pub channels: ChannelMode,
    #[serde(default = "d_fft_size")]
    pub fft_size: u32,
    #[serde(default = "d_min_freq")]
    pub min_freq: f32,
    /// Clamped to 0.95 * nyquist of the actual capture rate.
    #[serde(default = "d_max_freq")]
    pub max_freq: f32,
    #[serde(default = "d_scale")]
    pub scale: ScaleMode,
    #[serde(default = "d_db_floor")]
    pub db_floor: f32,
    #[serde(default = "d_band_mode")]
    pub band_mode: BandMode,
    /// Per-frame lerp factors referenced at 30 fps (converted for other rates).
    #[serde(default = "d_attack")]
    pub attack: f32,
    #[serde(default = "d_decay")]
    pub decay: f32,
    #[serde(default = "d_true")]
    pub auto_gain: bool,
    /// Fixed gain used when `autoGain` is false.
    #[serde(default = "d_gain")]
    pub gain: f32,
}

impl Default for SpectrumConfig {
    fn default() -> Self {
        serde_json::from_str("{}").expect("defaults")
    }
}

impl SpectrumConfig {
    pub fn validate(&self, limits: &Limits) -> Result<(), String> {
        if !(8..=limits.max_bands).contains(&self.bands) {
            return Err(format!("spectrum.bands must be 8..={}", limits.max_bands));
        }
        if !(1..=limits.max_fps).contains(&self.fps) {
            return Err(format!("spectrum.fps must be 1..={}", limits.max_fps));
        }
        if !FFT_SIZES.contains(&self.fft_size) {
            return Err(format!("spectrum.fftSize must be one of {FFT_SIZES:?}"));
        }
        if !self.min_freq.is_finite() || self.min_freq < 1.0 {
            return Err("spectrum.minFreq must be >= 1".into());
        }
        if !self.max_freq.is_finite() || self.max_freq <= self.min_freq {
            return Err("spectrum.maxFreq must be > minFreq".into());
        }
        if !(self.attack > 0.0 && self.attack <= 1.0 && self.attack.is_finite()) {
            return Err("spectrum.attack must be in (0, 1]".into());
        }
        if !(self.decay > 0.0 && self.decay <= 1.0 && self.decay.is_finite()) {
            return Err("spectrum.decay must be in (0, 1]".into());
        }
        if !(self.gain > 0.0 && self.gain.is_finite()) {
            return Err("spectrum.gain must be > 0".into());
        }
        if !(self.db_floor < 0.0 && self.db_floor.is_finite()) {
            return Err("spectrum.dbFloor must be < 0".into());
        }
        Ok(())
    }

    /// Clamp frequency range to what the actual sample rate can represent.
    pub fn resolve_for_rate(&mut self, sample_rate: u32) {
        let limit = 0.95 * sample_rate as f32 / 2.0;
        if self.max_freq > limit {
            self.max_freq = limit;
        }
        if self.min_freq >= self.max_freq {
            self.min_freq = (self.max_freq / 32.0).max(1.0);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum PcmFormat {
    S16le,
    F32le,
}

fn d_pcm_format() -> PcmFormat {
    PcmFormat::S16le
}
fn d_chunk_ms() -> u32 {
    50
}
fn d_false() -> bool {
    false
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct PcmConfig {
    #[serde(default = "d_false")]
    pub enabled: bool,
    #[serde(default = "d_pcm_format")]
    pub format: PcmFormat,
    #[serde(default = "d_chunk_ms")]
    pub chunk_ms: u32,
}

impl Default for PcmConfig {
    fn default() -> Self {
        serde_json::from_str("{}").expect("defaults")
    }
}

impl PcmConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(10..=500).contains(&self.chunk_ms) {
            return Err("pcm.chunkMs must be 10..=500".into());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Capture runtime state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum CaptureState {
    Starting,
    Running,
    Restarting,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct CaptureStats {
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub frames_emitted: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub frames_dropped: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub pcm_chunks_emitted: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub ring_overflows: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub starved_ticks: u64,
    pub restarts: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct CaptureInfo {
    pub capture_id: String,
    pub state: CaptureState,
    pub source: CaptureSource,
    pub device_id: Option<String>,
    pub format: Option<AudioFormat>,
    pub spectrum: SpectrumConfig,
    pub pcm: PcmConfig,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub started_at_ms: u64,
    pub stats: CaptureStats,
}

// ---------------------------------------------------------------------------
// Media sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum PlaybackStatus {
    Closed,
    Opened,
    Changing,
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum PlaybackType {
    Music,
    Video,
    Image,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum RepeatMode {
    None,
    Track,
    List,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct MediaTimeline {
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub position_ms: i64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub start_time_ms: i64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub end_time_ms: i64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub min_seek_ms: i64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub max_seek_ms: i64,
    /// Unix ms of the OS-side timeline update; extrapolate the live position
    /// as `positionMs + playbackRate * (now - lastUpdatedAtMs)` while playing.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub last_updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct MediaSession {
    /// `{appId}#{n}` — stable while the underlying OS session lives.
    pub session_id: String,
    /// Source app user model id on Windows; bus name on Linux.
    pub app_id: String,
    pub is_current: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track_number: Option<i32>,
    pub genres: Vec<String>,
    pub playback_type: PlaybackType,
    pub playback_status: PlaybackStatus,
    pub playback_rate: Option<f64>,
    pub shuffle: Option<bool>,
    pub repeat: Option<RepeatMode>,
    pub artwork_available: bool,
    /// Always null on Windows; Linux MPRIS may expose an art URL the host fetches.
    pub artwork_url: Option<String>,
    /// Set only when the sidecar runs with `--artwork-dir`: absolute path of
    /// the cached artwork file (content-hash named, written atomically).
    pub artwork_file: Option<String>,
    /// Content hash of the cached artwork; changes iff the image changes.
    pub artwork_hash: Option<String>,
    pub timeline: Option<MediaTimeline>,
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct Capabilities {
    pub device_capture: bool,
    pub device_loopback: bool,
    pub follow_default_output: bool,
    pub follow_default_input: bool,
    pub process_loopback: bool,
    pub process_loopback_exclude: bool,
    pub audio_process_list: bool,
    pub device_events: bool,
    pub media_sessions: bool,
    pub media_artwork: bool,
    pub spectrum: bool,
    pub pcm_stream: bool,
}

impl Capabilities {
    /// Used by builds for platforms without an implemented backend.
    #[cfg_attr(windows, allow(dead_code))]
    pub const NONE: Capabilities = Capabilities {
        device_capture: false,
        device_loopback: false,
        follow_default_output: false,
        follow_default_input: false,
        process_loopback: false,
        process_loopback_exclude: false,
        audio_process_list: false,
        device_events: false,
        media_sessions: false,
        media_artwork: false,
        spectrum: false,
        pcm_stream: false,
    };
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct Limits {
    pub max_captures: u32,
    pub max_fps: u32,
    pub max_bands: u32,
    pub fft_sizes: Vec<u32>,
    pub pcm_formats: Vec<PcmFormat>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_captures: 8,
            max_fps: 60,
            max_bands: 256,
            fft_sizes: FFT_SIZES.to_vec(),
            pcm_formats: vec![PcmFormat::S16le, PcmFormat::F32le],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum ExitReason {
    Shutdown,
    StdinClosed,
    Fatal,
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn spectrum_defaults() {
        let c = SpectrumConfig::default();
        assert!(c.enabled);
        assert_eq!(c.bands, 64);
        assert_eq!(c.fps, 30);
        assert_eq!(c.fft_size, 2048);
        assert_eq!(c.channels, ChannelMode::Stereo);
        assert_eq!(c.scale, ScaleMode::Db);
        assert!(c.auto_gain);
        c.validate(&Limits::default()).unwrap();
    }

    #[test]
    fn spectrum_partial_deserialize() {
        let c: SpectrumConfig = serde_json::from_str(r#"{"bands":32,"fps":60}"#).unwrap();
        assert_eq!(c.bands, 32);
        assert_eq!(c.fps, 60);
        assert_eq!(c.fft_size, 2048);
    }

    #[test]
    fn spectrum_validation_rejects() {
        let l = Limits::default();
        let mut c = SpectrumConfig::default();
        c.bands = 4;
        assert!(c.validate(&l).is_err());
        let mut c = SpectrumConfig::default();
        c.fft_size = 1000;
        assert!(c.validate(&l).is_err());
        let mut c = SpectrumConfig::default();
        c.fps = 0;
        assert!(c.validate(&l).is_err());
    }

    #[test]
    fn max_freq_clamps_to_nyquist() {
        let mut c = SpectrumConfig::default();
        c.max_freq = 30000.0;
        c.resolve_for_rate(44100);
        assert!(c.max_freq <= 0.95 * 22050.0 + 0.01);
        assert!(c.min_freq < c.max_freq);
    }

    #[test]
    fn capture_source_union_roundtrip() {
        let cases = [
            (
                r#"{"type":"device","deviceId":"x"}"#,
                CaptureSource::Device {
                    device_id: "x".into(),
                },
            ),
            (r#"{"type":"defaultOutput"}"#, CaptureSource::DefaultOutput),
            (r#"{"type":"defaultInput"}"#, CaptureSource::DefaultInput),
            (
                r#"{"type":"process","pid":42}"#,
                CaptureSource::Process { pid: 42 },
            ),
            (
                r#"{"type":"systemExcludingProcess","pid":42}"#,
                CaptureSource::SystemExcludingProcess { pid: 42 },
            ),
        ];
        for (json, expected) in cases {
            let parsed: CaptureSource = serde_json::from_str(json).unwrap();
            assert_eq!(parsed, expected);
            let back = serde_json::to_string(&parsed).unwrap();
            let reparsed: CaptureSource = serde_json::from_str(&back).unwrap();
            assert_eq!(reparsed, expected);
        }
    }

    #[test]
    fn enum_wire_names() {
        assert_eq!(
            serde_json::to_string(&DeviceState::NotPresent).unwrap(),
            r#""notPresent""#
        );
        assert_eq!(
            serde_json::to_string(&PcmFormat::S16le).unwrap(),
            r#""s16le""#
        );
        assert_eq!(serde_json::to_string(&ScaleMode::Db).unwrap(), r#""db""#);
        assert_eq!(
            serde_json::to_string(&ExitReason::StdinClosed).unwrap(),
            r#""stdinClosed""#
        );
        assert_eq!(
            serde_json::to_string(&RepeatMode::None).unwrap(),
            r#""none""#
        );
    }
}
