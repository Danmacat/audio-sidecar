//! macOS MediaRemote media worker.
//!
//! Apple blocks direct MediaRemote use from third-party processes since
//! macOS 15.4, so this worker drives the mediaremote-adapter helper
//! (BSD-3, ungive/mediaremote-adapter): `/usr/bin/perl` is an Apple-signed
//! platform binary that is allowed to load the framework, and the bundled
//! `mediaremote-adapter.pl` + `MediaRemoteAdapter.framework` pair streams
//! now-playing state as NDJSON. The helper ships as packaging assets next
//! to the sidecar binary; when they are missing or Apple breaks the trick,
//! the worker serves errors and media capabilities stay off — capture is
//! never affected (PORTING.md §4 best-effort discipline).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::protocol::events::{Event, MediaChangeKind};
use crate::protocol::methods::{
    MediaGetArtworkResult, MediaGetCurrentResult, MediaGetSessionsResult,
};
use crate::protocol::types::{
    MediaSession, MediaTimeline, PlaybackStatus, PlaybackType, RepeatMode,
};
use crate::protocol::{ErrorCode, RpcError};
use crate::rpc::writer::EventTx;
use crate::rpc::{MediaService, SvcFuture};
use crate::util::now_ms;

const STREAM_RESTART_MIN: Duration = Duration::from_millis(200);
const STREAM_RESTART_MAX: Duration = Duration::from_secs(5);
const TIMELINE_THROTTLE: Duration = Duration::from_millis(500);
const ARTWORK_CACHE_MAX: u64 = 10_000_000;
const ARTWORK_TIMEOUT: Duration = Duration::from_secs(3);
static CACHE_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Asset location & availability probe (drives hello capabilities)
// ---------------------------------------------------------------------------

/// Where the mediaremote-adapter packaging assets live: an explicit override,
/// a `mediaremote-adapter/` folder next to the binary, or directly next to it.
fn locate_assets() -> Option<(PathBuf, PathBuf)> {
    let candidates: Vec<PathBuf> = {
        let mut list = Vec::new();
        if let Ok(dir) = std::env::var("AUDIO_SIDECAR_MEDIAREMOTE_DIR") {
            list.push(PathBuf::from(dir));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                list.push(dir.join("mediaremote-adapter"));
                list.push(dir.to_path_buf());
            }
        }
        list
    };
    for dir in candidates {
        let script = dir.join("mediaremote-adapter.pl");
        let framework = dir.join("MediaRemoteAdapter.framework");
        if script.is_file() && framework.is_dir() {
            return Some((script, framework));
        }
    }
    None
}

/// File-level availability: assets present and the system perl exists. The
/// live framework load can still fail on any macOS update; that surfaces as
/// runtime errors, never as capture breakage.
pub fn adapter_assets_available() -> bool {
    locate_assets().is_some() && Path::new("/usr/bin/perl").is_file()
}

// ---------------------------------------------------------------------------
// Worker plumbing (mirrors media/linux.rs)
// ---------------------------------------------------------------------------

enum MediaCmd {
    GetSessions {
        reply: oneshot::Sender<Result<MediaGetSessionsResult, RpcError>>,
    },
    GetCurrent {
        reply: oneshot::Sender<Result<MediaGetCurrentResult, RpcError>>,
    },
    GetArtwork {
        session_id: String,
        max_bytes: u64,
        write_to: Option<String>,
        reply: oneshot::Sender<Result<MediaGetArtworkResult, RpcError>>,
    },
    ArtworkCached {
        session_id: String,
        generation: u64,
        hash: String,
        result: Result<CachedArtwork, RpcError>,
    },
    Quit,
}

#[derive(Clone)]
pub struct MediaHandle {
    tx: std::sync::mpsc::Sender<Msg>,
    shutdown: Arc<AtomicBool>,
}

impl MediaHandle {
    pub fn quit(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Msg::Cmd(MediaCmd::Quit));
    }

    fn request<T: Send + 'static>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, RpcError>>) -> MediaCmd,
    ) -> SvcFuture<T> {
        let (reply, rx) = oneshot::channel();
        let sent = self.tx.send(Msg::Cmd(make(reply))).is_ok();
        Box::pin(async move {
            if !sent {
                return Err(RpcError::internal("media worker unavailable"));
            }
            rx.await
                .map_err(|_| RpcError::internal("media worker dropped request"))?
        })
    }
}

impl MediaService for MediaHandle {
    fn get_sessions(&self) -> SvcFuture<MediaGetSessionsResult> {
        self.request(|reply| MediaCmd::GetSessions { reply })
    }

    fn get_current(&self) -> SvcFuture<MediaGetCurrentResult> {
        self.request(|reply| MediaCmd::GetCurrent { reply })
    }

    fn get_artwork(
        &self,
        session_id: String,
        max_bytes: u64,
        write_to: Option<String>,
    ) -> SvcFuture<MediaGetArtworkResult> {
        self.request(|reply| MediaCmd::GetArtwork {
            session_id,
            max_bytes,
            write_to,
            reply,
        })
    }
}

pub struct MediaWorker {
    pub handle: MediaHandle,
    pub join: std::thread::JoinHandle<()>,
}

pub fn spawn(events: EventTx, artwork_dir: Option<PathBuf>) -> MediaWorker {
    let (tx, rx) = std::sync::mpsc::channel::<Msg>();
    let shutdown = Arc::new(AtomicBool::new(false));
    let join = std::thread::Builder::new()
        .name("mediaremote-media-worker".into())
        .spawn({
            let tx = tx.clone();
            let shutdown = shutdown.clone();
            move || thread_main(rx, tx, shutdown, events, artwork_dir)
        })
        .expect("failed to spawn MediaRemote media worker");
    MediaWorker {
        handle: MediaHandle { tx, shutdown },
        join,
    }
}

enum Msg {
    Cmd(MediaCmd),
    Line(String),
    ReaderGone,
}

fn thread_main(
    rx: std::sync::mpsc::Receiver<Msg>,
    cmd_tx: std::sync::mpsc::Sender<Msg>,
    shutdown: Arc<AtomicBool>,
    events: EventTx,
    artwork_dir: Option<PathBuf>,
) {
    let Some((script, framework)) = locate_assets() else {
        info!("mediaremote-adapter assets not found; media service disabled");
        serve_init_error(rx, "mediaremote-adapter assets not found".into());
        return;
    };
    let mut worker = WorkerState {
        script,
        framework,
        cmd_tx,
        shutdown,
        events,
        artwork_dir: artwork_dir.and_then(|dir| match std::fs::create_dir_all(&dir) {
            Ok(()) => Some(dir),
            Err(err) => {
                error!(path = %dir.display(), %err, "cannot create artwork cache directory");
                None
            }
        }),
        tracked: None,
        payload: serde_json::Map::new(),
        stream: None,
        stream_rx: None,
        restart_backoff: STREAM_RESTART_MIN,
        last_timeline_emit: None,
        pending_timeline: None,
        artwork_gen: 0,
        announced_snapshot: false,
    };
    worker.run(rx);
    info!("MediaRemote media worker exiting");
}

fn serve_init_error(rx: std::sync::mpsc::Receiver<Msg>, message: String) {
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Cmd(MediaCmd::Quit) => break,
            Msg::Cmd(MediaCmd::GetSessions { reply }) => {
                let _ = reply.send(Err(os_error(&message)));
            }
            Msg::Cmd(MediaCmd::GetCurrent { reply }) => {
                let _ = reply.send(Err(os_error(&message)));
            }
            Msg::Cmd(MediaCmd::GetArtwork { reply, .. }) => {
                let _ = reply.send(Err(os_error(&message)));
            }
            Msg::Cmd(MediaCmd::ArtworkCached { .. }) => {}
            Msg::Line(_) | Msg::ReaderGone => {}
        }
    }
}

fn os_error(message: &str) -> RpcError {
    RpcError::new(ErrorCode::OsError, message.to_string())
}

struct Tracked {
    session: MediaSession,
    artwork_b64: Option<String>,
    /// `artworkMimeType` from the adapter; a hint only, magic bytes win.
    mime_hint: String,
    artwork_hash: Option<String>,
    artwork_gen: u64,
    artwork_pending: bool,
}

struct WorkerState {
    script: PathBuf,
    framework: PathBuf,
    cmd_tx: std::sync::mpsc::Sender<Msg>,
    shutdown: Arc<AtomicBool>,
    events: EventTx,
    artwork_dir: Option<PathBuf>,
    tracked: Option<Tracked>,
    /// Merged adapter payload (full frames + diffs applied).
    payload: serde_json::Map<String, serde_json::Value>,
    stream: Option<Child>,
    /// Marks "a stream (and its reader thread) is live"; frames/ReaderGone
    /// arrive on the shared worker channel.
    stream_rx: Option<()>,
    restart_backoff: Duration,
    last_timeline_emit: Option<Instant>,
    pending_timeline: Option<Instant>,
    artwork_gen: u64,
    announced_snapshot: bool,
}

#[derive(Clone)]
struct CachedArtwork {
    file: String,
    hash: String,
    content_type: String,
    byte_length: u64,
}

impl WorkerState {
    fn run(&mut self, rx: std::sync::mpsc::Receiver<Msg>) {
        if self.start_stream() {
            info!(script = %self.script.display(), "MediaRemote media worker running");
            // Empty snapshot at startup, mirroring the other platforms.
            self.emit_sessions_changed();
            self.announced_snapshot = true;
        } else {
            error!("cannot spawn mediaremote-adapter stream; media service degraded");
            self.restart_backoff = self.restart_backoff.mul_f64(2.0).min(STREAM_RESTART_MAX);
        }

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            if self.stream_rx.is_none() {
                // The stream died; bounded backoff before restarting.
                self.restart_stream();
                continue;
            }
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Msg::Cmd(MediaCmd::Quit)) => break,
                Ok(Msg::Cmd(cmd)) => self.handle_cmd(cmd),
                Ok(Msg::Line(line)) => {
                    self.restart_backoff = STREAM_RESTART_MIN;
                    self.handle_line(line);
                }
                Ok(Msg::ReaderGone) => {
                    warn!("mediaremote-adapter stream exited");
                    self.reap_child();
                    self.stream_rx = None;
                }
                Err(_) => {
                    // Idle tick: release a throttled timeline update, and
                    // extrapolate progress while playing (MediaRemote pushes
                    // no periodic updates; the frame carries its timestamp).
                    self.flush_pending_timeline();
                    self.tick_extrapolated_timeline();
                }
            }
        }
        self.reap_child();
    }

    fn reap_child(&mut self) {
        if let Some(mut child) = self.stream.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn restart_stream(&mut self) {
        let backoff = self.restart_backoff;
        self.restart_backoff = (self.restart_backoff.mul_f64(2.0)).min(STREAM_RESTART_MAX);
        std::thread::sleep(backoff);
        if self.start_stream() {
            info!("mediaremote-adapter stream restarted");
            // A fresh stream starts with a full frame; let it drive events.
        }
    }

    /// Spawns `perl mediaremote-adapter.pl <framework> stream --micros` with a
    /// dedicated stdout reader thread; frames arrive as `Msg::Line` on the
    /// same channel the worker loop services (commands share it).
    fn start_stream(&mut self) -> bool {
        let framework = match self.framework.canonicalize() {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(_) => return false,
        };
        let child = Command::new("/usr/bin/perl")
            .arg(&self.script)
            .arg(&framework)
            .arg("stream")
            .arg("--micros")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(err) => {
                error!(%err, "cannot spawn mediaremote-adapter");
                return false;
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => return false,
        };
        let tx = self.cmd_tx.clone();
        std::thread::Builder::new()
            .name("mediaremote-reader".into())
            .spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    match line {
                        Ok(line) => {
                            if tx.send(Msg::Line(line)).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let _ = tx.send(Msg::ReaderGone);
            })
            .expect("failed to spawn MediaRemote reader thread");
        self.stream = Some(child);
        self.stream_rx = Some(());
        true
    }

    // -- command handling ---------------------------------------------------

    fn handle_cmd(&mut self, cmd: MediaCmd) {
        match cmd {
            MediaCmd::GetSessions { reply } => {
                let sessions: Vec<MediaSession> = self
                    .tracked
                    .as_ref()
                    .map(|t| vec![t.session.clone()])
                    .unwrap_or_default();
                let _ = reply.send(Ok(MediaGetSessionsResult {
                    sessions,
                    current_session_id: self.tracked.as_ref().map(|t| t.session.session_id.clone()),
                }));
            }
            MediaCmd::GetCurrent { reply } => {
                let _ = reply.send(Ok(MediaGetCurrentResult {
                    session: self.tracked.as_ref().map(|t| t.session.clone()),
                }));
            }
            MediaCmd::GetArtwork {
                session_id,
                max_bytes,
                write_to,
                reply,
            } => {
                let _ = reply.send(self.get_artwork(&session_id, max_bytes, write_to.as_deref()));
            }
            MediaCmd::ArtworkCached {
                session_id,
                generation,
                hash,
                result,
            } => self.apply_artwork(&session_id, generation, &hash, result),
            MediaCmd::Quit => unreachable!("handled by the run loop"),
        }
    }

    fn get_artwork(
        &mut self,
        session_id: &str,
        max_bytes: u64,
        write_to: Option<&str>,
    ) -> Result<MediaGetArtworkResult, RpcError> {
        let Some(tracked) = self.tracked.as_ref() else {
            return Err(RpcError::new(
                ErrorCode::SessionNotFound,
                format!("no media session \"{session_id}\""),
            ));
        };
        if tracked.session.session_id != session_id {
            return Err(RpcError::new(
                ErrorCode::SessionNotFound,
                format!("no media session \"{session_id}\""),
            ));
        }
        let Some(b64) = tracked.artwork_b64.as_deref() else {
            return Err(RpcError::new(
                ErrorCode::ArtworkUnavailable,
                "session has no artwork",
            ));
        };
        let bytes = B64
            .decode(b64)
            .map_err(|err| RpcError::new(ErrorCode::ArtworkUnavailable, err.to_string()))?;
        if bytes.is_empty() {
            return Err(RpcError::new(
                ErrorCode::ArtworkUnavailable,
                "empty artwork",
            ));
        }
        if bytes.len() as u64 > max_bytes {
            return Err(artwork_too_large(bytes.len() as u64, max_bytes));
        }
        let content_type = sniff_content_type(&tracked.mime_hint, &bytes);
        match write_to {
            Some(dir) => {
                let cached = cache_bytes(Path::new(dir), &content_type, &bytes)?;
                Ok(MediaGetArtworkResult {
                    content_type: cached.content_type,
                    byte_length: cached.byte_length,
                    data_base64: None,
                    file: Some(cached.file),
                    hash: Some(cached.hash),
                })
            }
            None => Ok(MediaGetArtworkResult {
                content_type,
                byte_length: bytes.len() as u64,
                data_base64: Some(b64.to_string()),
                file: None,
                hash: None,
            }),
        }
    }

    // -- frame handling -----------------------------------------------------

    fn handle_line(&mut self, line: String) {
        let frame: serde_json::Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(err) => {
                debug!(%err, "ignoring malformed adapter line");
                return;
            }
        };
        if frame.get("type").and_then(|v| v.as_str()) != Some("data") {
            return;
        }
        let diff = frame.get("diff").and_then(|v| v.as_bool()).unwrap_or(false);
        let Some(payload) = frame.get("payload").and_then(|v| v.as_object()) else {
            return;
        };
        if diff {
            for (key, value) in payload {
                if value.is_null() {
                    self.payload.remove(key);
                } else {
                    self.payload.insert(key.clone(), value.clone());
                }
            }
        } else {
            self.payload = payload.clone();
        }
        self.apply_payload();
    }

    fn apply_payload(&mut self) {
        let payload = self.payload.clone();
        // An empty payload (or one without a title) means "no now-playing
        // client": drop the tracked session.
        let has_session = payload
            .get("bundleIdentifier")
            .or_else(|| payload.get("processIdentifier"))
            .is_some_and(|v| !v.is_null())
            && payload
                .get("title")
                .is_some_and(|v| !v.is_null() && v.as_str().is_some_and(|s| !s.is_empty()));
        if !has_session {
            if self.tracked.take().is_some() {
                self.emit_sessions_changed();
                self.events.send_event(&Event::MediaCurrentChanged {
                    current_session_id: None,
                    session: None,
                });
            }
            return;
        }

        let bundle = payload
            .get("bundleIdentifier")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| {
                payload
                    .get("processIdentifier")
                    .and_then(|v| v.as_i64())
                    .map(|pid| pid.to_string())
            })
            .unwrap_or_default();
        let session_id = format!("mr:{bundle}");

        // New app or first session?
        let different_session = self
            .tracked
            .as_ref()
            .is_none_or(|t| t.session.session_id != session_id);
        if different_session {
            let artwork_b64 = payload
                .get("artworkData")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let mime_hint = payload
                .get("artworkMimeType")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let mut session = build_session(&session_id, &bundle, &payload);
            session.artwork_available = artwork_b64.is_some();
            self.tracked = Some(Tracked {
                session,
                artwork_b64,
                mime_hint,
                artwork_hash: None,
                artwork_gen: self.artwork_gen,
                artwork_pending: false,
            });
            let snapshot = self.snapshot_session();
            self.events.send_event(&Event::MediaSessionsChanged {
                sessions: vec![snapshot.clone()],
                current_session_id: Some(session_id.clone()),
            });
            self.events.send_event(&Event::MediaCurrentChanged {
                current_session_id: Some(session_id.clone()),
                session: Some(snapshot),
            });
            if self
                .tracked
                .as_ref()
                .is_some_and(|t| t.artwork_b64.is_some())
            {
                self.spawn_artwork_cache();
            }
            self.emit_session_updated(vec![
                MediaChangeKind::MediaProperties,
                MediaChangeKind::PlaybackInfo,
            ]);
            return;
        }

        // Same session: rebuild the snapshot from the merged payload and diff
        // against the old one to derive change kinds.
        let old = self
            .tracked
            .as_ref()
            .expect("tracked exists")
            .session
            .clone();
        let old_artwork = self
            .tracked
            .as_ref()
            .expect("tracked exists")
            .artwork_b64
            .clone();
        let mut fresh = build_session(&session_id, &bundle, &self.payload);
        let mut kinds = Vec::new();
        if fresh.title != old.title
            || fresh.artist != old.artist
            || fresh.album != old.album
            || fresh.album_artist != old.album_artist
            || fresh.track_number != old.track_number
            || fresh.genres != old.genres
            || fresh.playback_type != old.playback_type
        {
            kinds.push(MediaChangeKind::MediaProperties);
        }
        if fresh.playback_status != old.playback_status
            || fresh.playback_rate != old.playback_rate
            || fresh.shuffle != old.shuffle
            || fresh.repeat != old.repeat
        {
            kinds.push(MediaChangeKind::PlaybackInfo);
        }
        if let (Some(a), Some(b)) = (&fresh.timeline, &old.timeline) {
            if a.position_ms != b.position_ms || a.end_time_ms != b.end_time_ms {
                kinds.push(MediaChangeKind::Timeline);
            }
        } else if fresh.timeline.is_some() != old.timeline.is_some() {
            kinds.push(MediaChangeKind::Timeline);
        }
        if payload.contains_key("artworkData") {
            let b64 = payload
                .get("artworkData")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if b64.as_deref() != old_artwork.as_deref() {
                fresh.artwork_available = b64.is_some();
                if let Some(tracked) = self.tracked.as_mut() {
                    tracked.artwork_b64 = b64.clone();
                    tracked.artwork_hash = None;
                }
                if b64.is_some() {
                    kinds.push(MediaChangeKind::MediaProperties);
                    self.spawn_artwork_cache();
                }
            }
        }
        if kinds.is_empty() {
            return;
        }
        let pending_artwork = self.tracked.as_mut().expect("tracked exists");
        pending_artwork.session = fresh;
        self.emit_session_updated(kinds);
    }

    fn snapshot_session(&self) -> MediaSession {
        self.tracked
            .as_ref()
            .map(|t| t.session.clone())
            .expect("tracked session missing")
    }

    fn spawn_artwork_cache(&mut self) {
        let Some(dir) = self.artwork_dir.clone() else {
            return;
        };
        let Some(tracked) = self.tracked.as_mut() else {
            return;
        };
        let Some(b64) = tracked.artwork_b64.clone() else {
            return;
        };
        self.artwork_gen += 1;
        tracked.artwork_gen = self.artwork_gen;
        tracked.artwork_pending = true;
        let generation = tracked.artwork_gen;
        let session_id = tracked.session.session_id.clone();
        let cmd_tx = self.cmd_tx.clone();
        std::thread::Builder::new()
            .name("mediaremote-artwork".into())
            .spawn(move || {
                let started = Instant::now();
                let result = match B64.decode(&b64) {
                    Ok(bytes) if bytes.is_empty() => Err(RpcError::new(
                        ErrorCode::ArtworkUnavailable,
                        "empty artwork data",
                    )),
                    Ok(bytes) => {
                        if bytes.len() as u64 > ARTWORK_CACHE_MAX {
                            Err(artwork_too_large(bytes.len() as u64, ARTWORK_CACHE_MAX))
                        } else {
                            cache_bytes(&dir, "", &bytes)
                        }
                    }
                    Err(err) => Err(RpcError::new(
                        ErrorCode::ArtworkUnavailable,
                        format!("invalid base64 artwork: {err}"),
                    )),
                };
                // Drop results that arrive after a newer track changed the
                // generation; guard against pathological spin.
                if started.elapsed() > ARTWORK_TIMEOUT {
                    return;
                }
                let hash = result
                    .as_ref()
                    .map(|cached| cached.hash.clone())
                    .unwrap_or_default();
                let _ = cmd_tx.send(Msg::Cmd(MediaCmd::ArtworkCached {
                    session_id,
                    generation,
                    hash,
                    result,
                }));
            })
            .expect("failed to spawn artwork cache thread");
    }

    fn apply_artwork(
        &mut self,
        session_id: &str,
        generation: u64,
        _hash: &str,
        result: Result<CachedArtwork, RpcError>,
    ) {
        let Some(tracked) = self.tracked.as_mut() else {
            return;
        };
        if tracked.session.session_id != session_id || tracked.artwork_gen != generation {
            return; // stale fetch for a previous track
        }
        tracked.artwork_pending = false;
        match result {
            Ok(cached) => {
                if tracked.artwork_hash.as_deref() == Some(cached.hash.as_str()) {
                    return; // same artwork, no event (content addressing)
                }
                tracked.artwork_hash = Some(cached.hash.clone());
                tracked.session.artwork_file = Some(cached.file);
                tracked.session.artwork_hash = Some(cached.hash);
                self.emit_session_updated(vec![MediaChangeKind::Artwork]);
            }
            Err(err) => {
                warn!(session_id, error = %err.message, "cannot cache artwork");
            }
        }
    }

    // -- events -------------------------------------------------------------

    fn emit_sessions_changed(&self) {
        let sessions: Vec<MediaSession> = self
            .tracked
            .as_ref()
            .map(|t| vec![t.session.clone()])
            .unwrap_or_default();
        let current = self.tracked.as_ref().map(|t| t.session.session_id.clone());
        self.events.send_event(&Event::MediaSessionsChanged {
            sessions,
            current_session_id: current,
        });
    }

    fn emit_session_updated(&mut self, mut kinds: Vec<MediaChangeKind>) {
        // Pure-timeline updates merge to ≤2/s per session (protocol §4); a
        // throttled update is parked with a deadline and flushed by the run
        // loop so the final position is never lost.
        if kinds.len() == 1 && kinds[0] == MediaChangeKind::Timeline {
            let now = Instant::now();
            if let Some(last) = self.last_timeline_emit {
                if now - last < TIMELINE_THROTTLE {
                    if self.pending_timeline.is_none() {
                        self.pending_timeline = Some(last + TIMELINE_THROTTLE);
                    }
                    return;
                }
            }
            self.last_timeline_emit = Some(now);
            self.pending_timeline = None;
        } else if self.pending_timeline.take().is_some() {
            // Piggyback a pending timeline onto the next non-timeline update.
            kinds.push(MediaChangeKind::Timeline);
            self.last_timeline_emit = Some(Instant::now());
        }
        if kinds.is_empty() {
            return;
        }
        let Some(tracked) = self.tracked.as_ref() else {
            return;
        };
        self.events.send_event(&Event::MediaSessionUpdated {
            session: tracked.session.clone(),
            changed: kinds,
        });
    }

    /// While playing, advance the timeline locally:
    /// `position = elapsed + (now - frameTimestamp) * rate`. Emitted through
    /// the same ≤2/s throttle as pushed updates.
    fn tick_extrapolated_timeline(&mut self) {
        const TICK: i64 = 500; // ms of movement worth reporting
        let Some(tracked) = self.tracked.as_mut() else {
            return;
        };
        if tracked.session.playback_status != PlaybackStatus::Playing {
            return;
        }
        let num = |key: &str| self.payload.get(key).and_then(|v| v.as_f64());
        let (Some(elapsed_us), Some(timestamp_us)) =
            (num("elapsedTimeMicros"), num("timestampEpochMicros"))
        else {
            return;
        };
        let rate = num("playbackRate").unwrap_or(1.0);
        if rate <= 0.0 {
            return;
        }
        let now_us = (now_ms() as f64) * 1000.0;
        let position_ms = ((elapsed_us / 1000.0) + (now_us - timestamp_us) / 1000.0 * rate) as i64;
        let Some(timeline) = tracked.session.timeline.as_mut() else {
            return;
        };
        if (position_ms - timeline.position_ms).abs() < TICK {
            return;
        }
        timeline.position_ms = position_ms.max(0);
        timeline.last_updated_at_ms = now_ms() as i64;
        self.emit_session_updated(vec![MediaChangeKind::Timeline]);
    }

    fn flush_pending_timeline(&mut self) {
        let Some(deadline) = self.pending_timeline else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        self.pending_timeline = None;
        self.last_timeline_emit = Some(Instant::now());
        let Some(tracked) = self.tracked.as_ref() else {
            return;
        };
        self.events.send_event(&Event::MediaSessionUpdated {
            session: tracked.session.clone(),
            changed: vec![MediaChangeKind::Timeline],
        });
    }
}

fn build_session(
    session_id: &str,
    bundle: &str,
    payload: &serde_json::Map<String, serde_json::Value>,
) -> MediaSession {
    let str_of = |key: &str| {
        payload
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let micros_of = |key: &str| {
        payload
            .get(key)
            .and_then(|v| v.as_f64())
            .map(|value| (value / 1000.0) as i64)
    };
    let playing = payload
        .get("playing")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let rate = payload.get("playbackRate").and_then(|v| v.as_f64());
    let timeline = micros_of("elapsedTimeMicros").map(|position| {
        let duration = micros_of("durationMicros").unwrap_or(0);
        MediaTimeline {
            position_ms: position.max(0),
            start_time_ms: 0,
            end_time_ms: duration.max(0),
            min_seek_ms: 0,
            max_seek_ms: duration.max(0),
            last_updated_at_ms: now_ms() as i64,
        }
    });
    MediaSession {
        session_id: session_id.to_string(),
        app_id: bundle.to_string(),
        is_current: true,
        title: str_of("title"),
        artist: str_of("artist"),
        album: str_of("album"),
        album_artist: str_of("composer"),
        track_number: payload
            .get("trackNumber")
            .and_then(|v| v.as_i64())
            .map(|n| n as i32),
        genres: {
            let genre = str_of("genre");
            if genre.is_empty() {
                Vec::new()
            } else {
                vec![genre]
            }
        },
        playback_type: match payload
            .get("mediaType")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
        {
            name if name.ends_with("Music") || name.ends_with("Audio") => PlaybackType::Music,
            name if name.ends_with("Video")
                || name.ends_with("Movie")
                || name.ends_with("TVShow")
                || name.ends_with("VideoPodcast") =>
            {
                PlaybackType::Video
            }
            _ => PlaybackType::Unknown,
        },
        playback_status: if playing {
            PlaybackStatus::Playing
        } else {
            PlaybackStatus::Paused
        },
        playback_rate: rate,
        shuffle: payload
            .get("shuffleMode")
            .and_then(|v| v.as_i64())
            .map(|m| m != 0),
        repeat: match payload.get("repeatMode").and_then(|v| v.as_i64()) {
            Some(0) | None => Some(RepeatMode::None),
            Some(1) => Some(RepeatMode::Track),
            Some(_) => Some(RepeatMode::List),
        },
        artwork_available: payload.get("artworkData").is_some_and(|v| !v.is_null()),
        artwork_url: None,
        artwork_file: None,
        artwork_hash: None,
        timeline,
    }
}

// ---------------------------------------------------------------------------
// Artwork cache (content-hash naming, atomic write — protocol §4a)
// ---------------------------------------------------------------------------

fn sniff_content_type(hint: &str, bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg".into()
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png".into()
    } else if bytes.starts_with(b"BM") {
        "image/bmp".into()
    } else if bytes.starts_with(b"GIF8") {
        "image/gif".into()
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp".into()
    } else {
        hint.split(',')
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("application/octet-stream")
            .to_string()
    }
}

fn ext_for(content_type: &str, bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "jpg"
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "png"
    } else if bytes.starts_with(b"BM") {
        "bmp"
    } else if bytes.starts_with(b"GIF8") {
        "gif"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "webp"
    } else if content_type == "image/jpeg" || content_type == "image/jpg" {
        "jpg"
    } else if content_type == "image/png" {
        "png"
    } else if content_type == "image/bmp" {
        "bmp"
    } else if content_type == "image/gif" {
        "gif"
    } else if content_type == "image/webp" {
        "webp"
    } else {
        "img"
    }
}

fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn artwork_too_large(byte_length: u64, max_bytes: u64) -> RpcError {
    RpcError::new(
        ErrorCode::ArtworkTooLarge,
        format!("artwork is {byte_length} bytes (maxBytes {max_bytes})"),
    )
    .with_data(serde_json::json!({ "byteLength": byte_length }))
}

fn cache_bytes(dir: &Path, hint: &str, bytes: &[u8]) -> Result<CachedArtwork, RpcError> {
    let content_type = sniff_content_type(hint, bytes);
    let hash = format!("{:016x}", fnv1a64(bytes));
    let path = dir.join(format!("{hash}.{}", ext_for(&content_type, bytes)));
    if !path.exists() {
        let seq = CACHE_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".tmp-{hash}-{}-{seq}", std::process::id()));
        std::fs::write(&tmp, bytes)
            .map_err(|err| RpcError::new(ErrorCode::OsError, err.to_string()))?;
        if let Err(err) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            if !path.exists() {
                return Err(RpcError::new(ErrorCode::OsError, err.to_string()));
            }
        }
    }
    let absolute_path = std::fs::canonicalize(&path).map_err(|err| {
        RpcError::new(
            ErrorCode::OsError,
            format!("cannot resolve artwork {}: {err}", path.display()),
        )
    })?;
    Ok(CachedArtwork {
        file: absolute_path.to_string_lossy().into_owned(),
        hash,
        content_type,
        byte_length: bytes.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn session_mapping_from_full_payload() {
        let p = payload(&[
            ("title", serde_json::json!("メルト")),
            ("artist", serde_json::json!("supercell")),
            ("album", serde_json::json!("初音ミク")),
            ("playing", serde_json::json!(true)),
            ("playbackRate", serde_json::json!(1.0)),
            ("elapsedTimeMicros", serde_json::json!(42_500_000.0)),
            ("durationMicros", serde_json::json!(300_000_000.0)),
            ("shuffleMode", serde_json::json!(1)),
            ("repeatMode", serde_json::json!(2)),
        ]);
        let s = build_session("mr:com.apple.Music", "com.apple.Music", &p);
        assert_eq!(s.title, "メルト");
        assert_eq!(s.playback_status, PlaybackStatus::Playing);
        assert_eq!(s.repeat, Some(RepeatMode::List));
        assert_eq!(s.shuffle, Some(true));
        let t = s.timeline.unwrap();
        assert_eq!(t.position_ms, 42_500);
        assert_eq!(t.end_time_ms, 300_000);
    }

    #[test]
    fn artwork_sniffing_matches_linux_rules() {
        assert_eq!(
            sniff_content_type("", &[0xff, 0xd8, 0xff, 0xE0]),
            "image/jpeg"
        );
        assert_eq!(
            sniff_content_type("", &[0x89, b'P', b'N', b'G']),
            "image/png"
        );
    }

    fn test_worker(artwork_dir: Option<PathBuf>) -> (WorkerState, std::sync::mpsc::Receiver<Msg>) {
        // The stdout writer is a tokio task; give the test a private runtime
        // (leaked — dropping it would cancel the task mid-test).
        let runtime = Box::leak(Box::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        ));
        let (events, _join) = runtime.block_on(async { crate::rpc::writer::spawn(64) });
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = WorkerState {
            script: PathBuf::from("/nonexistent"),
            framework: PathBuf::from("/nonexistent"),
            cmd_tx: tx.clone(),
            shutdown,
            events,
            artwork_dir,
            tracked: None,
            payload: serde_json::Map::new(),
            stream: None,
            stream_rx: None,
            restart_backoff: STREAM_RESTART_MIN,
            last_timeline_emit: None,
            pending_timeline: None,
            artwork_gen: 0,
            announced_snapshot: false,
        };
        (worker, rx)
    }

    const PNG_BYTES: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, b'I', b'H', b'D', b'R',
    ];

    #[test]
    fn full_frame_with_artwork_tracks_and_caches() {
        let dir = std::env::temp_dir().join(format!("sidecar-art-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut worker, rx) = test_worker(Some(dir.clone()));
        let artwork = B64.encode(PNG_BYTES);
        let frame = serde_json::json!({
            "type": "data", "diff": false,
            "payload": {
                "bundleIdentifier": "com.example.player",
                "title": "song",
                "playing": true,
                "playbackRate": 1.0,
                "elapsedTimeMicros": 10_000.0,
                "durationMicros": 60_000_000.0,
                "mediaType": "MRMediaRemoteMediaTypeMusic",
                "artworkData": artwork,
                "artworkMimeType": "image/png",
            }
        });
        worker.handle_line(frame.to_string());
        let tracked = worker.tracked.as_ref().expect("session tracked");
        assert_eq!(tracked.session.title, "song");
        assert_eq!(tracked.session.playback_type, PlaybackType::Music);
        assert!(tracked.session.artwork_available);
        // Drain the artwork result posted by the cache thread.
        let cached = loop {
            match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                Msg::Cmd(MediaCmd::ArtworkCached {
                    session_id,
                    generation,
                    hash,
                    result,
                }) => {
                    worker.apply_artwork(&session_id, generation, &hash, result.clone());
                    if let Ok(c) = &result {
                        break c.clone();
                    }
                    panic!("artwork cache failed");
                }
                Msg::Cmd(MediaCmd::Quit) => panic!("unexpected quit"),
                _ => continue,
            }
        };
        let tracked = worker.tracked.as_ref().expect("session tracked");
        assert_eq!(tracked.artwork_hash.as_deref(), Some(cached.hash.as_str()));
        assert_eq!(tracked.session.artwork_file, Some(cached.file.clone()));
        assert!(
            cached.file.ends_with(".png"),
            "sniffed extension: {}",
            cached.file
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_payload_drops_session() {
        let (mut worker, _rx) = test_worker(None);
        worker.handle_line(
            serde_json::json!({"type":"data","diff":false,"payload":{
                "bundleIdentifier":"com.example.player","title":"song"}})
            .to_string(),
        );
        assert!(worker.tracked.is_some());
        worker
            .handle_line(serde_json::json!({"type":"data","diff":false,"payload":{}}).to_string());
        assert!(worker.tracked.is_none());
    }

    #[test]
    fn timeline_extrapolation_advances_position() {
        let (mut worker, _rx) = test_worker(None);
        // elapsed=10s, frame timestamp=2s ago, rate 1 -> position ~12s
        let now_us = (crate::util::now_ms() as f64) * 1000.0;
        worker.handle_line(
            serde_json::json!({"type":"data","diff":false,"payload":{
                "bundleIdentifier":"com.example.player","title":"song","playing":true,
                "playbackRate":1.0,
                "elapsedTimeMicros": 10_000_000.0,
                "timestampEpochMicros": now_us - 2_000_000.0,
                "durationMicros": 60_000_000.0}})
            .to_string(),
        );
        worker.last_timeline_emit = None; // skip the initial emit's throttle
        worker.tick_extrapolated_timeline();
        let position = worker
            .tracked
            .as_ref()
            .unwrap()
            .session
            .timeline
            .as_ref()
            .unwrap()
            .position_ms;
        assert!((11_500..=13_000).contains(&position), "position {position}");
    }
}
