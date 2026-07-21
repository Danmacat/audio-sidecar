//! SMTC (GlobalSystemMediaTransportControls) worker thread.
//!
//! All blocking WinRT calls happen on this dedicated MTA thread. Event
//! handlers fired by the OS only enqueue dirty markers — the worker loop
//! re-snapshots, diffs, and emits protocol events. Artwork is fetched on a
//! short-lived thread per request so a hung WinRT call cannot stall the
//! worker (the RPC side enforces a timeout).

use std::collections::{HashMap, HashSet};
use std::sync::mpsc as std_mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as SessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as WinPlaybackStatus,
};
use windows::Media::{MediaPlaybackAutoRepeatMode, MediaPlaybackType as WinPlaybackType};
use windows::Storage::Streams::{DataReader, IInputStream};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::core::{IUnknown, Interface};

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

/// Minimum spacing between timeline-only `media.sessionUpdated` events per
/// session (some players fire several per second).
const TIMELINE_THROTTLE: Duration = Duration::from_millis(500);
const ARTWORK_TIMEOUT: Duration = Duration::from_secs(3);

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
        reply: oneshot::Sender<Result<MediaGetArtworkResult, RpcError>>,
    },
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Dirty {
    Sessions,
    Current,
    One(usize, MediaChangeKind),
}

enum Msg {
    Cmd(MediaCmd),
    Dirty(Dirty),
}

#[derive(Clone)]
pub struct MediaHandle {
    tx: std_mpsc::Sender<Msg>,
}

impl MediaHandle {
    pub fn quit(&self) {
        let _ = self.tx.send(Msg::Cmd(MediaCmd::Quit));
    }
}

impl MediaService for MediaHandle {
    fn get_sessions(&self) -> SvcFuture<MediaGetSessionsResult> {
        let (reply, rx) = oneshot::channel();
        let sent = self
            .tx
            .send(Msg::Cmd(MediaCmd::GetSessions { reply }))
            .is_ok();
        Box::pin(async move {
            if !sent {
                return Err(RpcError::internal("media worker unavailable"));
            }
            rx.await
                .map_err(|_| RpcError::internal("media worker dropped request"))?
        })
    }

    fn get_current(&self) -> SvcFuture<MediaGetCurrentResult> {
        let (reply, rx) = oneshot::channel();
        let sent = self
            .tx
            .send(Msg::Cmd(MediaCmd::GetCurrent { reply }))
            .is_ok();
        Box::pin(async move {
            if !sent {
                return Err(RpcError::internal("media worker unavailable"));
            }
            rx.await
                .map_err(|_| RpcError::internal("media worker dropped request"))?
        })
    }

    fn get_artwork(&self, session_id: String, max_bytes: u64) -> SvcFuture<MediaGetArtworkResult> {
        let (reply, rx) = oneshot::channel();
        let sent = self
            .tx
            .send(Msg::Cmd(MediaCmd::GetArtwork {
                session_id,
                max_bytes,
                reply,
            }))
            .is_ok();
        Box::pin(async move {
            if !sent {
                return Err(RpcError::internal("media worker unavailable"));
            }
            match tokio::time::timeout(ARTWORK_TIMEOUT, rx).await {
                Err(_) => Err(RpcError::new(ErrorCode::Timeout, "artwork fetch timed out")),
                Ok(Err(_)) => Err(RpcError::internal("media worker dropped request")),
                Ok(Ok(r)) => r,
            }
        })
    }
}

pub struct MediaWorker {
    pub handle: MediaHandle,
    pub join: std::thread::JoinHandle<()>,
}

pub fn spawn(events: EventTx) -> MediaWorker {
    let (tx, rx) = std_mpsc::channel::<Msg>();
    let dirty_tx = tx.clone();
    let join = std::thread::Builder::new()
        .name("media-worker".into())
        .spawn(move || thread_main(rx, dirty_tx, events))
        .expect("failed to spawn media worker thread");
    MediaWorker {
        handle: MediaHandle { tx },
        join,
    }
}

// ---------------------------------------------------------------------------
// Worker state
// ---------------------------------------------------------------------------

struct Tracked {
    session: Session,
    id: String,
    snap: MediaSession,
    tokens: [i64; 3],
    last_timeline_emit: Instant,
}

struct Worker {
    manager: SessionManager,
    dirty_tx: std_mpsc::Sender<Msg>,
    events: EventTx,
    tracked: HashMap<usize, Tracked>,
    order: Vec<usize>,
    current_key: Option<usize>,
    /// Monotonic per-aumid counter so `{aumid}#{n}` ids are never reused.
    id_counter: HashMap<String, u32>,
    /// Timeline updates deferred by the throttle: key -> flush deadline.
    pending_timeline: HashMap<usize, Instant>,
}

fn session_key(session: &Session) -> usize {
    // COM identity: only the canonical IUnknown pointer is comparable.
    session
        .cast::<IUnknown>()
        .map(|u| u.as_raw() as usize)
        .unwrap_or(0)
}

/// Build a change-event handler for any (sender, args) pair — the generics
/// let one helper serve all three per-session event registrations.
fn dirty_handler<S, A>(
    key: usize,
    kind: MediaChangeKind,
    tx: std_mpsc::Sender<Msg>,
) -> TypedEventHandler<S, A>
where
    S: windows::core::RuntimeType + 'static,
    A: windows::core::RuntimeType + 'static,
{
    TypedEventHandler::new(move |_, _| {
        let _ = tx.send(Msg::Dirty(Dirty::One(key, kind)));
        Ok(())
    })
}

fn thread_main(rx: std_mpsc::Receiver<Msg>, dirty_tx: std_mpsc::Sender<Msg>, events: EventTx) {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    let manager = match SessionManager::RequestAsync().and_then(|op| op.join()) {
        Ok(m) => m,
        Err(e) => {
            error!("SMTC session manager unavailable: {e}");
            while let Ok(msg) = rx.recv() {
                match msg {
                    Msg::Cmd(MediaCmd::Quit) => break,
                    Msg::Cmd(MediaCmd::GetSessions { reply }) => {
                        let _ = reply.send(Err(RpcError::new(ErrorCode::OsError, e.to_string())));
                    }
                    Msg::Cmd(MediaCmd::GetCurrent { reply }) => {
                        let _ = reply.send(Err(RpcError::new(ErrorCode::OsError, e.to_string())));
                    }
                    Msg::Cmd(MediaCmd::GetArtwork { reply, .. }) => {
                        let _ = reply.send(Err(RpcError::new(ErrorCode::OsError, e.to_string())));
                    }
                    Msg::Dirty(_) => {}
                }
            }
            return;
        }
    };

    // Manager-level events. Tokens are never removed: the manager lives for
    // the process lifetime.
    {
        let tx = dirty_tx.clone();
        let _ = manager.SessionsChanged(&TypedEventHandler::new(move |_, _| {
            let _ = tx.send(Msg::Dirty(Dirty::Sessions));
            Ok(())
        }));
        let tx = dirty_tx.clone();
        let _ = manager.CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
            let _ = tx.send(Msg::Dirty(Dirty::Current));
            Ok(())
        }));
    }

    let mut worker = Worker {
        manager,
        dirty_tx,
        events,
        tracked: HashMap::new(),
        order: Vec::new(),
        current_key: None,
        id_counter: HashMap::new(),
        pending_timeline: HashMap::new(),
    };
    info!("media worker running");
    worker.full_resync(true);

    loop {
        let timeout = worker
            .pending_timeline
            .values()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .min()
            .unwrap_or(Duration::from_millis(500));
        let first = match rx.recv_timeout(timeout) {
            Ok(msg) => Some(msg),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        // Batch everything already queued so event storms coalesce.
        let mut sessions_dirty = false;
        let mut current_dirty = false;
        let mut per_session: HashMap<usize, HashSet<MediaChangeKind>> = HashMap::new();
        let mut quit = false;
        let mut queue: Vec<Msg> = Vec::new();
        if let Some(m) = first {
            queue.push(m);
        }
        while let Ok(m) = rx.try_recv() {
            queue.push(m);
        }
        for msg in queue {
            match msg {
                Msg::Cmd(MediaCmd::Quit) => {
                    quit = true;
                    break;
                }
                Msg::Cmd(cmd) => worker.handle_cmd(cmd),
                Msg::Dirty(Dirty::Sessions) => sessions_dirty = true,
                Msg::Dirty(Dirty::Current) => current_dirty = true,
                Msg::Dirty(Dirty::One(key, kind)) => {
                    per_session.entry(key).or_default().insert(kind);
                }
            }
        }
        if quit {
            break;
        }

        if sessions_dirty {
            worker.full_resync(true);
        }
        if current_dirty {
            worker.current_resync();
        }
        for (key, kinds) in per_session {
            if sessions_dirty {
                continue; // the full resync already re-snapshotted everything
            }
            worker.one_update(key, kinds);
        }
        worker.flush_pending_timeline();
    }

    worker.unregister_all();
    info!("media worker exiting");
}

impl Worker {
    fn handle_cmd(&mut self, cmd: MediaCmd) {
        match cmd {
            MediaCmd::GetSessions { reply } => {
                let _ = reply.send(Ok(self.snapshot_result()));
            }
            MediaCmd::GetCurrent { reply } => {
                let session = self
                    .current_key
                    .and_then(|k| self.tracked.get(&k))
                    .map(|t| t.snap.clone());
                let _ = reply.send(Ok(MediaGetCurrentResult { session }));
            }
            MediaCmd::GetArtwork {
                session_id,
                max_bytes,
                reply,
            } => {
                let found = self.tracked.values().find(|t| t.id == session_id);
                match found {
                    None => {
                        let _ = reply.send(Err(RpcError::new(
                            ErrorCode::SessionNotFound,
                            format!("no media session \"{session_id}\""),
                        )));
                    }
                    Some(t) => {
                        let session = t.session.clone();
                        std::thread::spawn(move || {
                            let _ = reply.send(fetch_artwork(&session, max_bytes));
                        });
                    }
                }
            }
            MediaCmd::Quit => unreachable!("handled by the loop"),
        }
    }

    fn snapshot_result(&self) -> MediaGetSessionsResult {
        let sessions: Vec<MediaSession> = self
            .order
            .iter()
            .filter_map(|k| self.tracked.get(k))
            .map(|t| t.snap.clone())
            .collect();
        let current_session_id = self
            .current_key
            .and_then(|k| self.tracked.get(&k))
            .map(|t| t.id.clone());
        MediaGetSessionsResult {
            sessions,
            current_session_id,
        }
    }

    fn register_session(&mut self, key: usize, session: &Session) -> [i64; 3] {
        let t0 = session
            .MediaPropertiesChanged(&dirty_handler(
                key,
                MediaChangeKind::MediaProperties,
                self.dirty_tx.clone(),
            ))
            .unwrap_or(0);
        let t1 = session
            .PlaybackInfoChanged(&dirty_handler(
                key,
                MediaChangeKind::PlaybackInfo,
                self.dirty_tx.clone(),
            ))
            .unwrap_or(0);
        let t2 = session
            .TimelinePropertiesChanged(&dirty_handler(
                key,
                MediaChangeKind::Timeline,
                self.dirty_tx.clone(),
            ))
            .unwrap_or(0);
        [t0, t1, t2]
    }

    fn unregister_session(session: &Session, tokens: [i64; 3]) {
        let _ = session.RemoveMediaPropertiesChanged(tokens[0]);
        let _ = session.RemovePlaybackInfoChanged(tokens[1]);
        let _ = session.RemoveTimelinePropertiesChanged(tokens[2]);
    }

    fn unregister_all(&mut self) {
        for t in self.tracked.values() {
            Self::unregister_session(&t.session, t.tokens);
        }
        self.tracked.clear();
        self.order.clear();
    }

    fn assign_id(&mut self, aumid: &str) -> String {
        let n = self.id_counter.entry(aumid.to_string()).or_insert(0);
        *n += 1;
        format!("{aumid}#{n}")
    }

    fn full_resync(&mut self, emit: bool) {
        let list = match self.manager.GetSessions() {
            Ok(l) => l,
            Err(e) => {
                warn!("GetSessions failed: {e}");
                return;
            }
        };
        let mut new_order: Vec<usize> = Vec::new();
        let mut seen: HashSet<usize> = HashSet::new();
        for session in &list {
            let key = session_key(&session);
            if key == 0 || seen.contains(&key) {
                continue;
            }
            seen.insert(key);
            new_order.push(key);
            if !self.tracked.contains_key(&key) {
                let aumid = session
                    .SourceAppUserModelId()
                    .map(|h| h.to_string())
                    .unwrap_or_else(|_| "unknown".into());
                let id = self.assign_id(&aumid);
                let tokens = self.register_session(key, &session);
                self.tracked.insert(
                    key,
                    Tracked {
                        session: session.clone(),
                        snap: placeholder_snapshot(&id, &aumid),
                        id,
                        tokens,
                        last_timeline_emit: Instant::now() - TIMELINE_THROTTLE,
                    },
                );
            }
        }
        // Drop sessions that disappeared.
        let removed: Vec<usize> = self
            .tracked
            .keys()
            .filter(|k| !seen.contains(k))
            .copied()
            .collect();
        for key in removed {
            if let Some(t) = self.tracked.remove(&key) {
                Self::unregister_session(&t.session, t.tokens);
                debug!(id = t.id, "media session removed");
            }
            self.pending_timeline.remove(&key);
        }
        self.order = new_order;
        self.current_key = self
            .manager
            .GetCurrentSession()
            .ok()
            .map(|s| session_key(&s));

        // Re-snapshot everything.
        let keys: Vec<usize> = self.order.clone();
        for key in keys {
            self.refresh_snapshot(key);
        }

        if emit {
            let snap = self.snapshot_result();
            self.events.send_event(&Event::MediaSessionsChanged {
                sessions: snap.sessions,
                current_session_id: snap.current_session_id,
            });
        }
    }

    fn current_resync(&mut self) {
        let new_key = self
            .manager
            .GetCurrentSession()
            .ok()
            .map(|s| session_key(&s));
        if new_key == self.current_key {
            return;
        }
        self.current_key = new_key;
        let keys: Vec<usize> = self.tracked.keys().copied().collect();
        for key in keys {
            let is_current = Some(key) == self.current_key;
            if let Some(t) = self.tracked.get_mut(&key) {
                t.snap.is_current = is_current;
            }
        }
        let (id, session) = match self.current_key.and_then(|k| self.tracked.get(&k)) {
            Some(t) => (Some(t.id.clone()), Some(t.snap.clone())),
            None => (None, None),
        };
        self.events.send_event(&Event::MediaCurrentChanged {
            current_session_id: id,
            session,
        });
    }

    /// Rebuild one session's snapshot; returns true when it changed.
    fn refresh_snapshot(&mut self, key: usize) -> bool {
        let Some(t) = self.tracked.get(&key) else {
            return false;
        };
        let is_current = Some(key) == self.current_key;
        match snapshot_session(&t.session, &t.id, is_current) {
            Ok(snap) => {
                let t = self.tracked.get_mut(&key).expect("tracked");
                let changed = t.snap != snap;
                t.snap = snap;
                changed
            }
            Err(e) => {
                debug!(
                    id = t.id,
                    "session snapshot failed ({e}); scheduling resync"
                );
                let _ = self.dirty_tx.send(Msg::Dirty(Dirty::Sessions));
                false
            }
        }
    }

    fn one_update(&mut self, key: usize, kinds: HashSet<MediaChangeKind>) {
        if !self.tracked.contains_key(&key) {
            return;
        }
        let timeline_only = kinds.len() == 1 && kinds.contains(&MediaChangeKind::Timeline);
        if timeline_only {
            let throttled = self
                .tracked
                .get(&key)
                .map(|t| t.last_timeline_emit.elapsed() < TIMELINE_THROTTLE)
                .unwrap_or(false);
            if throttled {
                self.pending_timeline
                    .entry(key)
                    .or_insert_with(|| Instant::now() + TIMELINE_THROTTLE);
                return;
            }
        }
        self.emit_update(key, kinds);
    }

    fn emit_update(&mut self, key: usize, kinds: HashSet<MediaChangeKind>) {
        let changed_snapshot = self.refresh_snapshot(key);
        let Some(t) = self.tracked.get_mut(&key) else {
            return;
        };
        if !changed_snapshot && !kinds.contains(&MediaChangeKind::Timeline) {
            return;
        }
        if kinds.contains(&MediaChangeKind::Timeline) {
            t.last_timeline_emit = Instant::now();
        }
        let mut changed: Vec<MediaChangeKind> = kinds.into_iter().collect();
        changed.sort_by_key(|k| match k {
            MediaChangeKind::MediaProperties => 0,
            MediaChangeKind::PlaybackInfo => 1,
            MediaChangeKind::Timeline => 2,
        });
        let session = t.snap.clone();
        self.events
            .send_event(&Event::MediaSessionUpdated { session, changed });
    }

    fn flush_pending_timeline(&mut self) {
        let now = Instant::now();
        let due: Vec<usize> = self
            .pending_timeline
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(k, _)| *k)
            .collect();
        for key in due {
            self.pending_timeline.remove(&key);
            self.emit_update(key, HashSet::from([MediaChangeKind::Timeline]));
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

fn placeholder_snapshot(id: &str, aumid: &str) -> MediaSession {
    MediaSession {
        session_id: id.to_string(),
        app_id: aumid.to_string(),
        is_current: false,
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        album_artist: String::new(),
        track_number: None,
        genres: Vec::new(),
        playback_type: PlaybackType::Unknown,
        playback_status: PlaybackStatus::Closed,
        playback_rate: None,
        shuffle: None,
        repeat: None,
        artwork_available: false,
        artwork_url: None,
        timeline: None,
    }
}

fn map_status(s: WinPlaybackStatus) -> PlaybackStatus {
    match s {
        WinPlaybackStatus::Opened => PlaybackStatus::Opened,
        WinPlaybackStatus::Changing => PlaybackStatus::Changing,
        WinPlaybackStatus::Stopped => PlaybackStatus::Stopped,
        WinPlaybackStatus::Playing => PlaybackStatus::Playing,
        WinPlaybackStatus::Paused => PlaybackStatus::Paused,
        _ => PlaybackStatus::Closed,
    }
}

fn map_ptype(t: WinPlaybackType) -> PlaybackType {
    match t {
        WinPlaybackType::Music => PlaybackType::Music,
        WinPlaybackType::Video => PlaybackType::Video,
        WinPlaybackType::Image => PlaybackType::Image,
        _ => PlaybackType::Unknown,
    }
}

fn map_repeat(m: MediaPlaybackAutoRepeatMode) -> RepeatMode {
    match m {
        MediaPlaybackAutoRepeatMode::Track => RepeatMode::Track,
        MediaPlaybackAutoRepeatMode::List => RepeatMode::List,
        _ => RepeatMode::None,
    }
}

/// 100 ns FILETIME-epoch (1601) to unix ms.
fn datetime_to_unix_ms(universal_time: i64) -> i64 {
    (universal_time - 116_444_736_000_000_000) / 10_000
}

fn timespan_ms(duration: i64) -> i64 {
    duration / 10_000
}

fn snapshot_session(
    session: &Session,
    id: &str,
    is_current: bool,
) -> Result<MediaSession, windows::core::Error> {
    let aumid = session
        .SourceAppUserModelId()
        .map(|h| h.to_string())
        .unwrap_or_else(|_| "unknown".into());
    let mut snap = placeholder_snapshot(id, &aumid);
    snap.is_current = is_current;

    if let Ok(props) = session
        .TryGetMediaPropertiesAsync()
        .and_then(|op| op.join())
    {
        snap.title = props.Title().map(|h| h.to_string()).unwrap_or_default();
        snap.artist = props.Artist().map(|h| h.to_string()).unwrap_or_default();
        snap.album = props
            .AlbumTitle()
            .map(|h| h.to_string())
            .unwrap_or_default();
        snap.album_artist = props
            .AlbumArtist()
            .map(|h| h.to_string())
            .unwrap_or_default();
        let tn = props.TrackNumber().unwrap_or(0);
        snap.track_number = (tn > 0).then_some(tn);
        if let Ok(genres) = props.Genres() {
            for g in &genres {
                snap.genres.push(g.to_string());
            }
        }
        if let Ok(r) = props.PlaybackType() {
            if let Ok(t) = r.Value() {
                snap.playback_type = map_ptype(t);
            }
        }
        snap.artwork_available = props.Thumbnail().is_ok();
    }

    let info = session.GetPlaybackInfo()?;
    snap.playback_status = info
        .PlaybackStatus()
        .map(map_status)
        .unwrap_or(PlaybackStatus::Closed);
    snap.playback_rate = info.PlaybackRate().ok().and_then(|r| r.Value().ok());
    snap.shuffle = info.IsShuffleActive().ok().and_then(|r| r.Value().ok());
    snap.repeat = info
        .AutoRepeatMode()
        .ok()
        .and_then(|r| r.Value().ok())
        .map(map_repeat);
    if snap.playback_type == PlaybackType::Unknown {
        if let Ok(r) = info.PlaybackType() {
            if let Ok(t) = r.Value() {
                snap.playback_type = map_ptype(t);
            }
        }
    }

    if let Ok(tl) = session.GetTimelineProperties() {
        let position_ms = tl.Position().map(|t| timespan_ms(t.Duration)).unwrap_or(0);
        let start_time_ms = tl.StartTime().map(|t| timespan_ms(t.Duration)).unwrap_or(0);
        let end_time_ms = tl.EndTime().map(|t| timespan_ms(t.Duration)).unwrap_or(0);
        let min_seek_ms = tl
            .MinSeekTime()
            .map(|t| timespan_ms(t.Duration))
            .unwrap_or(0);
        let max_seek_ms = tl
            .MaxSeekTime()
            .map(|t| timespan_ms(t.Duration))
            .unwrap_or(0);
        let last_updated_at_ms = tl
            .LastUpdatedTime()
            .map(|d| datetime_to_unix_ms(d.UniversalTime))
            .unwrap_or(0);
        if end_time_ms > 0 || position_ms > 0 {
            snap.timeline = Some(MediaTimeline {
                position_ms,
                start_time_ms,
                end_time_ms,
                min_seek_ms,
                max_seek_ms,
                last_updated_at_ms,
            });
        }
    }

    Ok(snap)
}

// ---------------------------------------------------------------------------
// Artwork
// ---------------------------------------------------------------------------

fn fetch_artwork(session: &Session, max_bytes: u64) -> Result<MediaGetArtworkResult, RpcError> {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    let props = session
        .TryGetMediaPropertiesAsync()
        .and_then(|op| op.join())
        .map_err(|e| RpcError::new(ErrorCode::ArtworkUnavailable, e.to_string()))?;
    let thumb = props
        .Thumbnail()
        .map_err(|_| RpcError::new(ErrorCode::ArtworkUnavailable, "session has no thumbnail"))?;

    let mut retried = false;
    loop {
        let stream = thumb
            .OpenReadAsync()
            .and_then(|op| op.join())
            .map_err(|e| RpcError::new(ErrorCode::ArtworkUnavailable, e.to_string()))?;
        let size = stream.Size().unwrap_or(0);
        if size == 0 {
            // Some apps populate the thumbnail a beat after the track change.
            if !retried {
                retried = true;
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            return Err(RpcError::new(
                ErrorCode::ArtworkUnavailable,
                "empty thumbnail stream",
            ));
        }
        if size > max_bytes {
            return Err(RpcError::new(
                ErrorCode::ArtworkTooLarge,
                format!("thumbnail is {size} bytes (maxBytes {max_bytes})"),
            )
            .with_data(serde_json::json!({ "byteLength": size })));
        }
        let content_type = stream
            .ContentType()
            .map(|h| h.to_string())
            .unwrap_or_else(|_| "application/octet-stream".into());
        let input: IInputStream = stream
            .cast()
            .map_err(|e| RpcError::new(ErrorCode::OsError, e.to_string()))?;
        let reader = DataReader::CreateDataReader(&input)
            .map_err(|e| RpcError::new(ErrorCode::OsError, e.to_string()))?;
        reader
            .LoadAsync(size as u32)
            .and_then(|op| op.join())
            .map_err(|e| RpcError::new(ErrorCode::OsError, e.to_string()))?;
        let mut buf = vec![0u8; size as usize];
        reader
            .ReadBytes(&mut buf)
            .map_err(|e| RpcError::new(ErrorCode::OsError, e.to_string()))?;
        return Ok(MediaGetArtworkResult {
            content_type,
            byte_length: size,
            data_base64: B64.encode(buf),
        });
    }
}
