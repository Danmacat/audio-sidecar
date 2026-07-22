//! Linux MPRIS media worker.
//!
//! MPRIS has no position-changed signal, so the worker subscribes to session
//! and property signals but polls Position on a dedicated thread. All zbus
//! calls stay on that thread; the async RPC surface only exchanges messages
//! with it. Artwork is deliberately limited to local `file:` and `data:` URLs.
//! HTTP URLs remain host-owned.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info};
use url::Url;
use zbus::message::Type as MessageType;
use zbus::zvariant::OwnedValue;
use zbus::{Connection, MatchRule, MessageStream};

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

const POSITION_POLL_INTERVAL: Duration = Duration::from_millis(250);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
const TIMELINE_THROTTLE: Duration = Duration::from_millis(500);
const ARTWORK_CACHE_MAX: u64 = 10_000_000;
const ARTWORK_TIMEOUT: Duration = Duration::from_secs(3);
static CACHE_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

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
        artwork_url: String,
        result: Result<CachedArtwork, RpcError>,
    },
    Quit,
}

#[derive(Clone)]
pub struct MediaHandle {
    tx: mpsc::UnboundedSender<MediaCmd>,
    shutdown: Arc<Mutex<bool>>,
}

impl MediaHandle {
    pub fn quit(&self) {
        *self
            .shutdown
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        let _ = self.tx.send(MediaCmd::Quit);
    }
}

impl MediaService for MediaHandle {
    fn get_sessions(&self) -> SvcFuture<MediaGetSessionsResult> {
        let (reply, rx) = oneshot::channel();
        let sent = self.tx.send(MediaCmd::GetSessions { reply }).is_ok();
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
        let sent = self.tx.send(MediaCmd::GetCurrent { reply }).is_ok();
        Box::pin(async move {
            if !sent {
                return Err(RpcError::internal("media worker unavailable"));
            }
            rx.await
                .map_err(|_| RpcError::internal("media worker dropped request"))?
        })
    }

    fn get_artwork(
        &self,
        session_id: String,
        max_bytes: u64,
        write_to: Option<String>,
    ) -> SvcFuture<MediaGetArtworkResult> {
        let (reply, rx) = oneshot::channel();
        let sent = self
            .tx
            .send(MediaCmd::GetArtwork {
                session_id,
                max_bytes,
                write_to,
                reply,
            })
            .is_ok();
        Box::pin(async move {
            if !sent {
                return Err(RpcError::internal("media worker unavailable"));
            }
            match tokio::time::timeout(ARTWORK_TIMEOUT, rx).await {
                Err(_) => Err(RpcError::new(ErrorCode::Timeout, "artwork fetch timed out")),
                Ok(Err(_)) => Err(RpcError::internal("media worker dropped request")),
                Ok(Ok(result)) => result,
            }
        })
    }
}

pub struct MediaWorker {
    pub handle: MediaHandle,
    pub join: std::thread::JoinHandle<()>,
}

pub fn spawn(events: EventTx, artwork_dir: Option<PathBuf>) -> MediaWorker {
    let (tx, rx) = mpsc::unbounded_channel();
    let worker_tx = tx.clone();
    let shutdown = Arc::new(Mutex::new(false));
    let worker_shutdown = shutdown.clone();
    let join = std::thread::Builder::new()
        .name("mpris-media-worker".into())
        .spawn(move || thread_main(rx, worker_tx, worker_shutdown, events, artwork_dir))
        .expect("failed to spawn MPRIS media worker");
    MediaWorker {
        handle: MediaHandle { tx, shutdown },
        join,
    }
}

#[derive(Clone)]
struct Tracked {
    session: MediaSession,
    artwork_url: Option<String>,
    artwork_gen: u64,
    artwork_pending: bool,
}

struct WorkerState {
    connection: Connection,
    cmd_tx: mpsc::UnboundedSender<MediaCmd>,
    shutdown: Arc<Mutex<bool>>,
    events: EventTx,
    artwork_dir: Option<PathBuf>,
    sessions: HashMap<String, Tracked>,
    order: Vec<String>,
    current: Option<String>,
    last_timeline_emit: HashMap<String, Instant>,
    pending_timeline: HashMap<String, Instant>,
    initialized: bool,
}

fn thread_main(
    rx: mpsc::UnboundedReceiver<MediaCmd>,
    cmd_tx: mpsc::UnboundedSender<MediaCmd>,
    shutdown: Arc<Mutex<bool>>,
    events: EventTx,
    artwork_dir: Option<PathBuf>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            error!(%error, "cannot create MPRIS tokio runtime");
            serve_init_error_blocking(rx, format!("MPRIS runtime unavailable: {error}"));
            return;
        }
    };
    runtime.block_on(async move {
        let connection = match Connection::session().await {
            Ok(connection) => connection,
            Err(error) => {
                error!(%error, "MPRIS session bus unavailable");
                serve_init_error(rx, format!("MPRIS session bus unavailable: {error}")).await;
                return;
            }
        };
        let worker = WorkerState {
            connection,
            cmd_tx,
            shutdown,
            events,
            artwork_dir: artwork_dir.and_then(|dir| match std::fs::create_dir_all(&dir) {
                Ok(()) => Some(dir),
                Err(error) => {
                    error!(path = %dir.display(), %error, "cannot create artwork cache directory");
                    None
                }
            }),
            sessions: HashMap::new(),
            order: Vec::new(),
            current: None,
            last_timeline_emit: HashMap::new(),
            pending_timeline: HashMap::new(),
            initialized: false,
        };
        info!("MPRIS media worker running");
        worker.run(rx).await;
        info!("MPRIS media worker exiting");
    });
}

async fn serve_init_error(mut rx: mpsc::UnboundedReceiver<MediaCmd>, message: String) {
    while let Some(cmd) = rx.recv().await {
        if !reply_init_error(cmd, &message) {
            break;
        }
    }
}

fn serve_init_error_blocking(mut rx: mpsc::UnboundedReceiver<MediaCmd>, message: String) {
    while let Some(cmd) = rx.blocking_recv() {
        if !reply_init_error(cmd, &message) {
            break;
        }
    }
}

fn reply_init_error(cmd: MediaCmd, message: &str) -> bool {
    match cmd {
        MediaCmd::Quit => false,
        MediaCmd::GetSessions { reply } => {
            let _ = reply.send(Err(RpcError::new(ErrorCode::OsError, message)));
            true
        }
        MediaCmd::GetCurrent { reply } => {
            let _ = reply.send(Err(RpcError::new(ErrorCode::OsError, message)));
            true
        }
        MediaCmd::GetArtwork { reply, .. } => {
            let _ = reply.send(Err(RpcError::new(ErrorCode::OsError, message)));
            true
        }
        MediaCmd::ArtworkCached { .. } => true,
    }
}

impl WorkerState {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<MediaCmd>) {
        let mut name_changes = signal_stream(
            &self.connection,
            "org.freedesktop.DBus",
            "NameOwnerChanged",
            SignalFilter::Arg0Namespace("org.mpris.MediaPlayer2"),
        )
        .await;
        let mut property_changes = signal_stream(
            &self.connection,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            SignalFilter::Arg0("org.mpris.MediaPlayer2.Player"),
        )
        .await;
        let mut seeked = signal_stream(
            &self.connection,
            "org.mpris.MediaPlayer2.Player",
            "Seeked",
            SignalFilter::None,
        )
        .await;
        let mut queued = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(MediaCmd::Quit) => return,
                Ok(cmd) => queued.push(cmd),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => return,
            }
        }
        self.refresh_all().await;
        for cmd in queued {
            self.handle_cmd(cmd);
        }

        let mut position_tick = tokio::time::interval(POSITION_POLL_INTERVAL);
        position_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut reconcile_tick = tokio::time::interval(RECONCILE_INTERVAL);
        reconcile_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first full refresh above covers both intervals' immediate first tick.
        position_tick.tick().await;
        reconcile_tick.tick().await;

        loop {
            let keep_running = tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(MediaCmd::Quit) | None => false,
                    Some(cmd) => {
                        self.handle_cmd(cmd);
                        true
                    }
                },
                signal = next_signal(&mut name_changes) => {
                    handle_signal_result("NameOwnerChanged", signal, &mut name_changes);
                    self.refresh_all().await;
                    true
                },
                signal = next_signal(&mut property_changes) => {
                    handle_signal_result("PropertiesChanged", signal, &mut property_changes);
                    self.refresh_all().await;
                    true
                },
                signal = next_signal(&mut seeked) => {
                    handle_signal_result("Seeked", signal, &mut seeked);
                    self.refresh_all().await;
                    true
                },
                _ = position_tick.tick() => {
                    self.refresh_positions().await;
                    true
                },
                _ = reconcile_tick.tick() => {
                    self.refresh_all().await;
                    true
                },
            };
            if !keep_running {
                break;
            }
            self.flush_pending_timeline();
        }
    }

    fn handle_cmd(&mut self, cmd: MediaCmd) {
        match cmd {
            MediaCmd::GetSessions { reply } => {
                let _ = reply.send(Ok(self.snapshot_result()));
            }
            MediaCmd::GetCurrent { reply } => {
                let session = self
                    .current
                    .as_ref()
                    .and_then(|id| self.sessions.get(id))
                    .map(|tracked| tracked.session.clone());
                let _ = reply.send(Ok(MediaGetCurrentResult { session }));
            }
            MediaCmd::GetArtwork {
                session_id,
                max_bytes,
                write_to,
                reply,
            } => {
                self.spawn_artwork_request(session_id, max_bytes, write_to, reply);
            }
            MediaCmd::ArtworkCached {
                session_id,
                generation,
                artwork_url,
                result,
            } => {
                self.apply_artwork(&session_id, generation, &artwork_url, result);
            }
            MediaCmd::Quit => unreachable!("handled by worker loop"),
        }
    }

    fn snapshot_result(&self) -> MediaGetSessionsResult {
        MediaGetSessionsResult {
            sessions: self
                .order
                .iter()
                .filter_map(|id| self.sessions.get(id))
                .map(|tracked| tracked.session.clone())
                .collect(),
            current_session_id: self.current.clone(),
        }
    }

    async fn refresh_all(&mut self) {
        let names = match list_mpris_names(&self.connection).await {
            Ok(names) => names,
            Err(error) => {
                debug!(%error, "MPRIS name enumeration failed");
                return;
            }
        };
        let mut names = names;
        names.sort();
        let mut next = HashMap::new();
        let mut order = Vec::new();
        for name in names {
            let tracked = match snapshot_player(&self.connection, &name).await {
                Ok((mut session, artwork_url)) => {
                    let (artwork_file, artwork_hash, artwork_gen, artwork_pending) = self
                        .sessions
                        .get(&name)
                        .map(|previous| {
                            if previous.artwork_url == artwork_url {
                                (
                                    previous.session.artwork_file.clone(),
                                    previous.session.artwork_hash.clone(),
                                    previous.artwork_gen,
                                    previous.artwork_pending,
                                )
                            } else {
                                (None, None, previous.artwork_gen + 1, false)
                            }
                        })
                        .unwrap_or((None, None, 0, false));
                    // Cache fields are updated asynchronously. Keeping the prior
                    // values here matches the Windows backend during track changes.
                    session.artwork_file = artwork_file;
                    session.artwork_hash = artwork_hash;
                    Tracked {
                        session,
                        artwork_url,
                        artwork_gen,
                        artwork_pending,
                    }
                }
                Err(error) => {
                    debug!(session_id = name, %error, "MPRIS snapshot failed");
                    let Some(previous) = self.sessions.get(&name).cloned() else {
                        continue;
                    };
                    previous
                }
            };
            order.push(name.clone());
            next.insert(name, tracked);
        }

        let new_current = choose_current(&next, &order);
        for (name, tracked) in next.iter_mut() {
            tracked.session.is_current = new_current.as_deref() == Some(name.as_str());
        }
        let names_changed = order != self.order;
        let current_changed = new_current != self.current;
        let old = std::mem::replace(&mut self.sessions, next);
        self.order = order;
        self.current = new_current;
        self.last_timeline_emit
            .retain(|name, _| self.sessions.contains_key(name));
        self.pending_timeline
            .retain(|name, _| self.sessions.contains_key(name));

        if !self.initialized {
            self.initialized = true;
            let snapshot = self.snapshot_result();
            self.send_event(&Event::MediaSessionsChanged {
                sessions: snapshot.sessions,
                current_session_id: snapshot.current_session_id,
            });
            self.trigger_pending_artwork(&old);
            return;
        }
        if names_changed {
            let snapshot = self.snapshot_result();
            self.send_event(&Event::MediaSessionsChanged {
                sessions: snapshot.sessions,
                current_session_id: snapshot.current_session_id,
            });
        }
        if current_changed {
            self.emit_current_changed();
        }

        for name in self.order.clone() {
            let Some(previous) = old.get(&name) else {
                continue;
            };
            let Some(current) = self.sessions.get(&name) else {
                continue;
            };
            let mut changed = changed_kinds(&previous.session, &current.session);
            if changed.is_empty() {
                continue;
            }
            changed.sort_by_key(change_order);
            self.queue_update(&name, changed);
        }
        self.trigger_pending_artwork(&old);
    }

    async fn refresh_positions(&mut self) {
        for name in self.order.clone() {
            let Ok(position_us) = player_position(&self.connection, &name).await else {
                continue;
            };
            let position_ms = position_us.max(0) / 1000;
            let Some(tracked) = self.sessions.get_mut(&name) else {
                continue;
            };
            if tracked
                .session
                .timeline
                .as_ref()
                .map(|timeline| timeline.position_ms)
                == Some(position_ms)
            {
                continue;
            }
            let end_time_ms = tracked
                .session
                .timeline
                .as_ref()
                .map(|timeline| timeline.end_time_ms)
                .unwrap_or(0);
            if position_ms == 0 && end_time_ms == 0 && tracked.session.timeline.is_none() {
                continue;
            }
            tracked.session.timeline = Some(MediaTimeline {
                position_ms,
                start_time_ms: 0,
                end_time_ms,
                min_seek_ms: 0,
                max_seek_ms: end_time_ms,
                last_updated_at_ms: now_ms() as i64,
            });
            self.queue_update(&name, vec![MediaChangeKind::Timeline]);
        }
    }

    fn queue_update(&mut self, name: &str, changed: Vec<MediaChangeKind>) {
        let timeline_only = changed.len() == 1 && changed[0] == MediaChangeKind::Timeline;
        if timeline_only
            && self
                .last_timeline_emit
                .get(name)
                .is_some_and(|at| at.elapsed() < TIMELINE_THROTTLE)
        {
            self.pending_timeline
                .entry(name.to_string())
                .or_insert_with(|| Instant::now() + TIMELINE_THROTTLE);
            return;
        }
        if changed.contains(&MediaChangeKind::Timeline) {
            self.last_timeline_emit
                .insert(name.to_string(), Instant::now());
            self.pending_timeline.remove(name);
        }
        self.emit_update(name, changed);
    }

    fn emit_current_changed(&self) {
        let session = self
            .current
            .as_ref()
            .and_then(|id| self.sessions.get(id))
            .map(|tracked| tracked.session.clone());
        self.send_event(&Event::MediaCurrentChanged {
            current_session_id: self.current.clone(),
            session,
        });
    }

    fn emit_update(&self, name: &str, changed: Vec<MediaChangeKind>) {
        if let Some(tracked) = self.sessions.get(name) {
            self.send_event(&Event::MediaSessionUpdated {
                session: tracked.session.clone(),
                changed,
            });
        }
    }

    fn send_event(&self, event: &Event) {
        let shutdown = self
            .shutdown
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !*shutdown {
            self.events.send_event(event);
        }
    }

    fn flush_pending_timeline(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self
            .pending_timeline
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(name, _)| name.clone())
            .collect();
        for name in due {
            self.pending_timeline.remove(&name);
            self.last_timeline_emit.insert(name.clone(), Instant::now());
            self.emit_update(&name, vec![MediaChangeKind::Timeline]);
        }
    }

    fn trigger_pending_artwork(&mut self, previous: &HashMap<String, Tracked>) {
        if self.artwork_dir.is_none() {
            return;
        }
        let pending: Vec<String> = self
            .order
            .iter()
            .filter(|name| {
                self.sessions.get(*name).is_some_and(|tracked| {
                    tracked.artwork_url.as_deref().is_some_and(is_local_artwork)
                        && !tracked.artwork_pending
                        && (tracked.session.artwork_file.is_none()
                            || previous.get(*name).map(|old| &old.artwork_url)
                                != Some(&tracked.artwork_url))
                })
            })
            .cloned()
            .collect();
        for session_id in pending {
            self.trigger_artwork(session_id);
        }
    }

    fn trigger_artwork(&mut self, session_id: String) {
        let Some(dir) = self.artwork_dir.clone() else {
            return;
        };
        let Some(tracked) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let Some(artwork_url) = tracked
            .artwork_url
            .clone()
            .filter(|url| is_local_artwork(url))
        else {
            return;
        };
        tracked.artwork_pending = true;
        let generation = tracked.artwork_gen;
        let tx = self.cmd_tx.clone();
        std::thread::spawn(move || {
            let result = read_local_artwork(&artwork_url, ARTWORK_CACHE_MAX)
                .and_then(|(content_type, bytes)| cache_bytes(&dir, &content_type, &bytes));
            let _ = tx.send(MediaCmd::ArtworkCached {
                session_id,
                generation,
                artwork_url,
                result,
            });
        });
    }

    fn apply_artwork(
        &mut self,
        session_id: &str,
        generation: u64,
        artwork_url: &str,
        result: Result<CachedArtwork, RpcError>,
    ) {
        let Some(tracked) = self.sessions.get_mut(session_id) else {
            return;
        };
        if tracked.artwork_gen != generation || tracked.artwork_url.as_deref() != Some(artwork_url)
        {
            return;
        }
        tracked.artwork_pending = false;
        let cached = match result {
            Ok(cached) => cached,
            Err(error) => {
                debug!(session_id, reason = %error.message, "local MPRIS artwork cache failed");
                return;
            }
        };
        if tracked.session.artwork_hash.as_deref() == Some(cached.hash.as_str()) {
            return;
        }
        tracked.session.artwork_file = Some(cached.file);
        tracked.session.artwork_hash = Some(cached.hash);
        let session = tracked.session.clone();
        self.send_event(&Event::MediaSessionUpdated {
            session,
            changed: vec![MediaChangeKind::Artwork],
        });
    }

    fn spawn_artwork_request(
        &self,
        session_id: String,
        max_bytes: u64,
        write_to: Option<String>,
        reply: oneshot::Sender<Result<MediaGetArtworkResult, RpcError>>,
    ) {
        let Some(tracked) = self.sessions.get(&session_id) else {
            let _ = reply.send(Err(RpcError::new(
                ErrorCode::SessionNotFound,
                "media session not found",
            )));
            return;
        };
        let Some(artwork_url) = tracked
            .artwork_url
            .clone()
            .filter(|url| is_local_artwork(url))
        else {
            let _ = reply.send(Err(RpcError::new(
                ErrorCode::ArtworkUnavailable,
                "session artwork is remote or unavailable",
            )));
            return;
        };
        std::thread::spawn(move || {
            let _ = reply.send(fetch_artwork(&artwork_url, max_bytes, write_to));
        });
    }
}

#[derive(Clone, Copy)]
enum SignalFilter {
    None,
    Arg0(&'static str),
    Arg0Namespace(&'static str),
}

async fn signal_stream(
    connection: &Connection,
    interface: &'static str,
    member: &'static str,
    filter: SignalFilter,
) -> Option<MessageStream> {
    let result = async {
        let mut builder = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .interface(interface)?
            .member(member)?;
        builder = if interface == "org.freedesktop.DBus" {
            builder
                .sender("org.freedesktop.DBus")?
                .path("/org/freedesktop/DBus")?
        } else {
            builder.path("/org/mpris/MediaPlayer2")?
        };
        builder = match filter {
            SignalFilter::None => builder,
            SignalFilter::Arg0(value) => builder.arg(0, value)?,
            SignalFilter::Arg0Namespace(value) => builder.arg0ns(value)?,
        };
        MessageStream::for_match_rule(builder.build(), connection, Some(64)).await
    }
    .await;
    match result {
        Ok(stream) => Some(stream),
        Err(error) => {
            error!(%error, interface, member, "cannot subscribe to MPRIS signal");
            None
        }
    }
}

async fn next_signal(
    stream: &mut Option<MessageStream>,
) -> Option<Result<zbus::Message, zbus::Error>> {
    match stream {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

fn handle_signal_result(
    signal_name: &str,
    result: Option<Result<zbus::Message, zbus::Error>>,
    stream: &mut Option<MessageStream>,
) {
    match result {
        Some(Ok(_)) => {}
        Some(Err(error)) => {
            debug!(%error, signal_name, "MPRIS signal stream failed");
            *stream = None;
        }
        None => {
            debug!(signal_name, "MPRIS signal stream ended");
            *stream = None;
        }
    }
}

fn fetch_artwork(
    artwork_url: &str,
    max_bytes: u64,
    write_to: Option<String>,
) -> Result<MediaGetArtworkResult, RpcError> {
    let (content_type, bytes) = read_local_artwork(artwork_url, max_bytes)?;
    if let Some(dir) = write_to {
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir).map_err(|error| {
            RpcError::new(
                ErrorCode::OsError,
                format!("cannot create writeTo dir {}: {error}", dir.display()),
            )
        })?;
        let cached = cache_bytes(&dir, &content_type, &bytes)?;
        return Ok(MediaGetArtworkResult {
            content_type: cached.content_type,
            byte_length: cached.byte_length,
            data_base64: None,
            file: Some(cached.file),
            hash: Some(cached.hash),
        });
    }
    Ok(MediaGetArtworkResult {
        content_type,
        byte_length: bytes.len() as u64,
        data_base64: Some(B64.encode(bytes)),
        file: None,
        hash: None,
    })
}

async fn list_mpris_names(connection: &Connection) -> Result<Vec<String>, String> {
    let proxy = zbus::fdo::DBusProxy::new(connection)
        .await
        .map_err(|error| error.to_string())?;
    let names = proxy
        .list_names()
        .await
        .map_err(|error| error.to_string())?;
    Ok(names
        .into_iter()
        .map(|name| name.to_string())
        .filter(|name| name.starts_with("org.mpris.MediaPlayer2."))
        .collect())
}

async fn snapshot_player(
    connection: &Connection,
    name: &str,
) -> Result<(MediaSession, Option<String>), String> {
    let player = zbus::Proxy::new(
        connection,
        name,
        "/org/mpris/MediaPlayer2",
        "org.mpris.MediaPlayer2.Player",
    )
    .await
    .map_err(|error| error.to_string())?;
    let status = player
        .get_property::<String>("PlaybackStatus")
        .await
        .unwrap_or_else(|_| "Stopped".into());
    let metadata = player
        .get_property::<HashMap<String, OwnedValue>>("Metadata")
        .await
        .unwrap_or_default();
    let position = player.get_property::<i64>("Position").await.unwrap_or(0);
    let rate = player.get_property::<f64>("Rate").await.ok();
    let shuffle = player.get_property::<bool>("Shuffle").await.ok();
    let loop_status = player.get_property::<String>("LoopStatus").await.ok();
    let title = metadata_string(&metadata, "xesam:title");
    let artist = metadata_strings(&metadata, "xesam:artist");
    let album = metadata_string(&metadata, "xesam:album");
    let album_artist = metadata_strings(&metadata, "xesam:albumArtist")
        .into_iter()
        .next()
        .unwrap_or_default();
    let track_number = metadata_i32(&metadata, "xesam:trackNumber");
    let genres = metadata_strings(&metadata, "xesam:genre");
    let artwork_url = metadata_string(&metadata, "mpris:artUrl");
    let artwork_url = (!artwork_url.is_empty()).then_some(artwork_url);
    let length_us = metadata_i64(&metadata, "mpris:length").unwrap_or(0);
    let timeline = (position > 0 || length_us > 0).then(|| MediaTimeline {
        position_ms: (position.max(0)) / 1000,
        start_time_ms: 0,
        end_time_ms: (length_us.max(0)) / 1000,
        min_seek_ms: 0,
        max_seek_ms: (length_us.max(0)) / 1000,
        last_updated_at_ms: now_ms() as i64,
    });
    let playback_type = if metadata.is_empty() {
        PlaybackType::Unknown
    } else {
        PlaybackType::Music
    };
    let snap = MediaSession {
        session_id: name.to_string(),
        app_id: name.to_string(),
        is_current: false,
        title,
        artist: artist.join(", "),
        album,
        album_artist,
        track_number,
        genres,
        playback_type,
        playback_status: map_status(&status),
        playback_rate: rate,
        shuffle,
        repeat: loop_status.as_deref().map(map_repeat),
        artwork_available: artwork_url.is_some(),
        artwork_url: artwork_url
            .as_deref()
            .filter(|url| is_remote_artwork(url))
            .map(str::to_string),
        artwork_file: None,
        artwork_hash: None,
        timeline,
    };
    Ok((snap, artwork_url))
}

async fn player_position(connection: &Connection, name: &str) -> Result<i64, String> {
    let player = zbus::Proxy::new(
        connection,
        name,
        "/org/mpris/MediaPlayer2",
        "org.mpris.MediaPlayer2.Player",
    )
    .await
    .map_err(|error| error.to_string())?;
    player
        .get_property::<i64>("Position")
        .await
        .map_err(|error| error.to_string())
}

fn choose_current(sessions: &HashMap<String, Tracked>, order: &[String]) -> Option<String> {
    order
        .iter()
        .find(|name| {
            sessions
                .get(*name)
                .is_some_and(|tracked| tracked.session.playback_status == PlaybackStatus::Playing)
        })
        .or_else(|| {
            order.iter().find(|name| {
                sessions.get(*name).is_some_and(|tracked| {
                    matches!(
                        tracked.session.playback_status,
                        PlaybackStatus::Opened | PlaybackStatus::Paused | PlaybackStatus::Changing
                    )
                })
            })
        })
        .cloned()
}

fn changed_kinds(old: &MediaSession, new: &MediaSession) -> Vec<MediaChangeKind> {
    let mut changed = Vec::new();
    let mut old_without_current = old.clone();
    old_without_current.is_current = new.is_current;
    if old_without_current.title != new.title
        || old_without_current.artist != new.artist
        || old_without_current.album != new.album
        || old_without_current.album_artist != new.album_artist
        || old_without_current.track_number != new.track_number
        || old_without_current.genres != new.genres
        || old_without_current.playback_type != new.playback_type
        || old_without_current.artwork_available != new.artwork_available
        || old_without_current.artwork_url != new.artwork_url
    {
        changed.push(MediaChangeKind::MediaProperties);
    }
    if old_without_current.playback_status != new.playback_status
        || old_without_current.playback_rate != new.playback_rate
        || old_without_current.shuffle != new.shuffle
        || old_without_current.repeat != new.repeat
    {
        changed.push(MediaChangeKind::PlaybackInfo);
    }
    if old_without_current.timeline != new.timeline {
        changed.push(MediaChangeKind::Timeline);
    }
    changed
}

fn change_order(kind: &MediaChangeKind) -> u8 {
    match kind {
        MediaChangeKind::MediaProperties => 0,
        MediaChangeKind::PlaybackInfo => 1,
        MediaChangeKind::Timeline => 2,
        MediaChangeKind::Artwork => 3,
    }
}

fn map_status(status: &str) -> PlaybackStatus {
    match status {
        "Playing" => PlaybackStatus::Playing,
        "Paused" => PlaybackStatus::Paused,
        "Stopped" => PlaybackStatus::Stopped,
        "Opening" => PlaybackStatus::Opened,
        _ => PlaybackStatus::Closed,
    }
}

fn map_repeat(status: &str) -> RepeatMode {
    match status {
        "Track" => RepeatMode::Track,
        "Playlist" => RepeatMode::List,
        _ => RepeatMode::None,
    }
}

fn metadata_string(metadata: &HashMap<String, OwnedValue>, key: &str) -> String {
    metadata
        .get(key)
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| String::try_from(value).ok())
        .unwrap_or_default()
}

fn metadata_strings(metadata: &HashMap<String, OwnedValue>, key: &str) -> Vec<String> {
    metadata
        .get(key)
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| Vec::<String>::try_from(value).ok())
        .or_else(|| {
            let value = metadata_string(metadata, key);
            (!value.is_empty()).then_some(vec![value])
        })
        .unwrap_or_default()
}

fn metadata_i32(metadata: &HashMap<String, OwnedValue>, key: &str) -> Option<i32> {
    metadata
        .get(key)
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| i32::try_from(value).ok())
}

fn metadata_i64(metadata: &HashMap<String, OwnedValue>, key: &str) -> Option<i64> {
    metadata
        .get(key)
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| i64::try_from(value).ok())
}

fn is_remote_artwork(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn is_local_artwork(url: &str) -> bool {
    url.starts_with("file:") || url.starts_with("data:")
}

struct CachedArtwork {
    file: String,
    hash: String,
    content_type: String,
    byte_length: u64,
}

fn read_local_artwork(url: &str, max_bytes: u64) -> Result<(String, Vec<u8>), RpcError> {
    let (hint, bytes) = if let Some(data) = url.strip_prefix("data:") {
        decode_data_url(data)?
    } else {
        let parsed = Url::parse(url).map_err(|error| {
            RpcError::new(
                ErrorCode::ArtworkUnavailable,
                format!("invalid artwork URL: {error}"),
            )
        })?;
        let path = parsed.to_file_path().map_err(|_| {
            RpcError::new(
                ErrorCode::ArtworkUnavailable,
                "artwork URL is not a local file",
            )
        })?;
        if let Ok(metadata) = std::fs::metadata(&path) {
            if metadata.len() > max_bytes {
                return Err(artwork_too_large(metadata.len(), max_bytes));
            }
        }
        let mut bytes = std::fs::read(&path).map_err(|error| {
            RpcError::new(
                ErrorCode::ArtworkUnavailable,
                format!("cannot read artwork {}: {error}", path.display()),
            )
        })?;
        if bytes.is_empty() {
            std::thread::sleep(Duration::from_millis(500));
            bytes = std::fs::read(&path).map_err(|error| {
                RpcError::new(
                    ErrorCode::ArtworkUnavailable,
                    format!("cannot retry artwork {}: {error}", path.display()),
                )
            })?;
            if bytes.is_empty() {
                return Err(RpcError::new(
                    ErrorCode::ArtworkUnavailable,
                    "empty artwork file",
                ));
            }
        }
        (content_type_from_path(&path).to_string(), bytes)
    };
    let byte_length = bytes.len() as u64;
    if byte_length > max_bytes {
        return Err(artwork_too_large(byte_length, max_bytes));
    }
    let content_type = sniff_content_type(&hint, &bytes);
    Ok((content_type, bytes))
}

fn artwork_too_large(byte_length: u64, max_bytes: u64) -> RpcError {
    RpcError::new(
        ErrorCode::ArtworkTooLarge,
        format!("artwork is {byte_length} bytes (maxBytes {max_bytes})"),
    )
    .with_data(serde_json::json!({ "byteLength": byte_length }))
}

fn content_type_from_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" | "jpe" => "image/jpeg",
        "png" => "image/png",
        "bmp" => "image/bmp",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

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

fn decode_data_url(data: &str) -> Result<(String, Vec<u8>), RpcError> {
    let (meta, payload) = data.split_once(',').ok_or_else(|| {
        RpcError::new(ErrorCode::ArtworkUnavailable, "malformed data artwork URL")
    })?;
    let mut parts = meta.split(';');
    let content_type = parts
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("text/plain");
    let is_base64 = parts.any(|part| part.eq_ignore_ascii_case("base64"));
    let bytes = if is_base64 {
        B64.decode(payload).map_err(|error| {
            RpcError::new(
                ErrorCode::ArtworkUnavailable,
                format!("invalid base64 artwork: {error}"),
            )
        })?
    } else {
        percent_decode(payload.as_bytes())?
    };
    Ok((content_type.to_string(), bytes))
}

fn percent_decode(input: &[u8]) -> Result<Vec<u8>, RpcError> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' {
            if i + 2 >= input.len() {
                return Err(RpcError::new(
                    ErrorCode::ArtworkUnavailable,
                    "invalid percent escape",
                ));
            }
            let hi = hex(input[i + 1]).ok_or_else(|| {
                RpcError::new(ErrorCode::ArtworkUnavailable, "invalid percent escape")
            })?;
            let lo = hex(input[i + 2]).ok_or_else(|| {
                RpcError::new(ErrorCode::ArtworkUnavailable, "invalid percent escape")
            })?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    Ok(out)
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn cache_bytes(dir: &Path, content_type: &str, bytes: &[u8]) -> Result<CachedArtwork, RpcError> {
    let content_type = sniff_content_type(content_type, bytes);
    let hash = format!("{:016x}", fnv1a64(bytes));
    let path = dir.join(format!("{hash}.{}", ext_for(&content_type, bytes)));
    if !path.exists() {
        let seq = CACHE_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".tmp-{hash}-{}-{seq}", std::process::id()));
        std::fs::write(&tmp, bytes)
            .map_err(|error| RpcError::new(ErrorCode::OsError, error.to_string()))?;
        if let Err(error) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            if !path.exists() {
                return Err(RpcError::new(ErrorCode::OsError, error.to_string()));
            }
        }
    }
    let absolute_path = std::fs::canonicalize(&path).map_err(|error| {
        RpcError::new(
            ErrorCode::OsError,
            format!("cannot resolve artwork {}: {error}", path.display()),
        )
    })?;
    Ok(CachedArtwork {
        file: absolute_path.to_string_lossy().into_owned(),
        hash,
        content_type,
        byte_length: bytes.len() as u64,
    })
}

fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
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

#[cfg(test)]
mod tests {
    use super::{
        decode_data_url, ext_for, map_repeat, map_status, percent_decode, read_local_artwork,
        sniff_content_type,
    };
    use crate::protocol::ErrorCode;
    use crate::protocol::types::{PlaybackStatus, RepeatMode};

    #[test]
    fn maps_mpris_status_and_repeat() {
        assert_eq!(map_status("Playing"), PlaybackStatus::Playing);
        assert_eq!(map_status("Paused"), PlaybackStatus::Paused);
        assert_eq!(map_repeat("Playlist"), RepeatMode::List);
    }

    #[test]
    fn decodes_local_artwork_forms() {
        let (content_type, bytes) = decode_data_url("image/png;base64,aGVsbG8=").unwrap();
        assert_eq!(content_type, "image/png");
        assert_eq!(bytes, b"hello");
        assert_eq!(percent_decode(b"a%20b").unwrap(), b"a b");
        assert_eq!(ext_for("image/png", b"not-png"), "png");
        assert_eq!(ext_for("text/plain", b"BMfake"), "bmp");
        assert_eq!(
            sniff_content_type("image/jpeg,image/jpg", b"BMfake"),
            "image/bmp"
        );
    }

    #[test]
    fn sniffs_data_artwork_and_reports_actual_oversize() {
        let (content_type, bytes) =
            read_local_artwork("data:text/plain;base64,Qk1mYWtl", 32).unwrap();
        assert_eq!(content_type, "image/bmp");
        assert_eq!(bytes, b"BMfake");

        let error = read_local_artwork("data:image/png;base64,aGVsbG8=", 4).unwrap_err();
        assert_eq!(error.code, ErrorCode::ArtworkTooLarge);
        assert_eq!(error.data.unwrap()["byteLength"], 5);
    }
}
