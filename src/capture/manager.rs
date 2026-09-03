//! CaptureManager actor: owns the session table, resolves default devices,
//! reacts to device notifications, and drives the restart/backoff state
//! machine. All mutation happens inside one tokio task; RPC handlers and
//! platform threads talk to it via `ManagerCmd`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::protocol::events::{Event, StateReason};
use crate::protocol::methods::{CaptureStartParams, CaptureStartResult};
use crate::protocol::types::{
    AudioFormat, Capabilities, CaptureInfo, CaptureSource, CaptureState, DefaultRole, DeviceKind,
    Limits, PcmConfig, SpectrumConfig,
};
use crate::protocol::{ErrorCode, RpcError};
use crate::rpc::DeviceService;
use crate::rpc::writer::EventTx;
use crate::util::now_ms;

use super::{
    CaptureBackend, CaptureSpec, ReadyInfo, ResolvedSource, SessionError, SessionStats,
    SessionThreads, SessionWiring,
};

const READY_TIMEOUT: Duration = Duration::from_secs(5);
const JOIN_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_CHANGE_DEBOUNCE: Duration = Duration::from_millis(250);
const STABLE_RESET: Duration = Duration::from_secs(30);

fn backoff(attempts: u32) -> Duration {
    const STEPS: [u64; 5] = [200, 500, 1000, 2000, 5000];
    Duration::from_millis(STEPS[(attempts as usize).min(STEPS.len() - 1)])
}

pub enum ManagerCmd {
    Start {
        params: CaptureStartParams,
        reply: oneshot::Sender<Result<CaptureStartResult, RpcError>>,
    },
    Stop {
        capture_id: String,
        reply: oneshot::Sender<Result<(), RpcError>>,
    },
    List {
        reply: oneshot::Sender<Vec<CaptureInfo>>,
    },
    StopAll {
        reply: oneshot::Sender<()>,
    },
    /// io thread reported readiness (or failure) for the current spawn.
    Ready {
        capture_id: String,
        result: Result<ReadyInfo, SessionError>,
    },
    /// io thread hit a fatal error mid-run.
    Fatal {
        capture_id: String,
        error: SessionError,
    },
    /// The join task finished reaping the session threads.
    TornDown {
        capture_id: String,
    },
    /// Backoff / debounce timer fired.
    TryRestart {
        capture_id: String,
    },
    /// From the platform device watcher: a default endpoint changed.
    DefaultChanged {
        kind: DeviceKind,
        device_id: String,
    },
    /// From the platform device watcher: a device was removed or left the
    /// active state.
    DeviceGone {
        device_id: String,
    },
}

#[derive(Clone)]
pub struct ManagerHandle {
    tx: mpsc::UnboundedSender<ManagerCmd>,
}

impl ManagerHandle {
    pub async fn start(&self, params: CaptureStartParams) -> Result<CaptureStartResult, RpcError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ManagerCmd::Start { params, reply })
            .map_err(|_| RpcError::internal("capture manager unavailable"))?;
        rx.await
            .map_err(|_| RpcError::internal("capture manager dropped request"))?
    }

    pub async fn stop(&self, capture_id: String) -> Result<(), RpcError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ManagerCmd::Stop { capture_id, reply })
            .map_err(|_| RpcError::internal("capture manager unavailable"))?;
        rx.await
            .map_err(|_| RpcError::internal("capture manager dropped request"))?
    }

    pub async fn list(&self) -> Result<Vec<CaptureInfo>, RpcError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ManagerCmd::List { reply })
            .map_err(|_| RpcError::internal("capture manager unavailable"))?;
        rx.await
            .map_err(|_| RpcError::internal("capture manager dropped request"))
    }

    /// Stop every session; resolves when teardown finished or `timeout` passed.
    pub async fn stop_all(&self, timeout: Duration) {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(ManagerCmd::StopAll { reply }).is_ok() {
            let _ = tokio::time::timeout(timeout, rx).await;
        }
    }
}

/// What to do once the session threads are reaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterTeardown {
    Remove,
    /// Keep the slot in `failed` state (process exited); host decides.
    KeepFailed,
    RestartNow,
    RestartAfterBackoff,
}

struct Slot {
    source: CaptureSource,
    spectrum: SpectrumConfig,
    pcm: PcmConfig,
    state: CaptureState,
    device_id: Option<String>,
    format: Option<AudioFormat>,
    started_at_ms: u64,
    stats: Arc<SessionStats>,
    threads: Option<SessionThreads>,
    stop: Arc<AtomicBool>,
    pending_start: Option<oneshot::Sender<Result<CaptureStartResult, RpcError>>>,
    stop_replies: Vec<oneshot::Sender<Result<(), RpcError>>>,
    after: AfterTeardown,
    attempts: u32,
    running_since: Option<Instant>,
    restart_timer_pending: bool,
    tearing_down: bool,
}

impl Slot {
    fn follow_kind(&self) -> Option<DeviceKind> {
        match self.source {
            CaptureSource::DefaultOutput => Some(DeviceKind::Render),
            CaptureSource::DefaultInput => Some(DeviceKind::Capture),
            _ => None,
        }
    }

    fn resolved_spectrum(&self) -> SpectrumConfig {
        let mut sp = self.spectrum.clone();
        if let Some(f) = &self.format {
            sp.resolve_for_rate(f.sample_rate);
        }
        sp
    }

    fn info(&self, capture_id: &str) -> CaptureInfo {
        CaptureInfo {
            capture_id: capture_id.to_string(),
            state: self.state,
            source: self.source.clone(),
            device_id: self.device_id.clone(),
            format: self.format,
            spectrum: self.resolved_spectrum(),
            pcm: self.pcm.clone(),
            started_at_ms: self.started_at_ms,
            stats: self.stats.snapshot(),
        }
    }
}

/// Used by platforms without a device watcher (the stub backends).
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
pub fn spawn(
    backend: Arc<dyn CaptureBackend>,
    resolver: Option<Arc<dyn DeviceService>>,
    events: EventTx,
    capabilities: Capabilities,
    limits: Limits,
) -> ManagerHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    spawn_with_channel(tx, rx, backend, resolver, events, capabilities, limits)
}

/// Like [`spawn`] but with an externally created command channel, so platform
/// device watchers can hold the sender before the actor exists.
pub fn spawn_with_channel(
    tx: mpsc::UnboundedSender<ManagerCmd>,
    rx: mpsc::UnboundedReceiver<ManagerCmd>,
    backend: Arc<dyn CaptureBackend>,
    resolver: Option<Arc<dyn DeviceService>>,
    events: EventTx,
    capabilities: Capabilities,
    limits: Limits,
) -> ManagerHandle {
    let handle = ManagerHandle { tx: tx.clone() };
    let mut manager = Manager {
        backend,
        resolver,
        events,
        capabilities,
        limits,
        tx,
        slots: HashMap::new(),
        next_id: 1,
    };
    tokio::spawn(async move {
        manager.run(rx).await;
    });
    handle
}

struct Manager {
    backend: Arc<dyn CaptureBackend>,
    resolver: Option<Arc<dyn DeviceService>>,
    events: EventTx,
    capabilities: Capabilities,
    limits: Limits,
    tx: mpsc::UnboundedSender<ManagerCmd>,
    slots: HashMap<String, Slot>,
    next_id: u64,
}

impl Manager {
    async fn run(&mut self, mut rx: mpsc::UnboundedReceiver<ManagerCmd>) {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                ManagerCmd::Start { params, reply } => self.handle_start(params, reply).await,
                ManagerCmd::Stop { capture_id, reply } => self.handle_stop(&capture_id, reply),
                ManagerCmd::List { reply } => {
                    let mut infos: Vec<CaptureInfo> =
                        self.slots.iter().map(|(id, s)| s.info(id)).collect();
                    infos.sort_by(|a, b| a.capture_id.cmp(&b.capture_id));
                    let _ = reply.send(infos);
                }
                ManagerCmd::StopAll { reply } => self.handle_stop_all(reply).await,
                ManagerCmd::Ready { capture_id, result } => self.handle_ready(&capture_id, result),
                ManagerCmd::Fatal { capture_id, error } => self.handle_fatal(&capture_id, error),
                ManagerCmd::TornDown { capture_id } => self.handle_torn_down(&capture_id).await,
                ManagerCmd::TryRestart { capture_id } => self.handle_try_restart(&capture_id).await,
                ManagerCmd::DefaultChanged { kind, device_id } => {
                    self.handle_default_changed(kind, device_id)
                }
                ManagerCmd::DeviceGone { device_id } => self.handle_device_gone(&device_id),
            }
        }
    }

    fn emit_state(
        &self,
        capture_id: &str,
        state: CaptureState,
        device_id: Option<String>,
        format: Option<AudioFormat>,
        reason: Option<StateReason>,
    ) {
        self.events.send_event(&Event::CaptureState {
            capture_id: capture_id.to_string(),
            state,
            device_id,
            format,
            reason,
        });
    }

    async fn resolve_source(&self, source: &CaptureSource) -> Result<ResolvedSource, RpcError> {
        match source {
            CaptureSource::Device { device_id } => Ok(ResolvedSource::Device {
                device_id: device_id.clone(),
            }),
            CaptureSource::DefaultOutput | CaptureSource::DefaultInput => {
                let kind = if matches!(source, CaptureSource::DefaultOutput) {
                    DeviceKind::Render
                } else {
                    DeviceKind::Capture
                };
                let resolver = self.resolver.as_ref().ok_or_else(RpcError::unsupported)?;
                let device = resolver.get_default(kind, DefaultRole::Multimedia).await?;
                Ok(ResolvedSource::Device {
                    device_id: device.id,
                })
            }
            CaptureSource::Process { pid } => Ok(ResolvedSource::Process {
                pid: *pid,
                exclude: false,
            }),
            CaptureSource::SystemExcludingProcess { pid } => Ok(ResolvedSource::Process {
                pid: *pid,
                exclude: true,
            }),
        }
    }

    /// (Re)spawn platform threads for `capture_id`. The slot must exist with a
    /// fresh state; readiness is reported back via `ManagerCmd::Ready`.
    fn spawn_threads(
        &mut self,
        capture_id: &str,
        resolved: ResolvedSource,
    ) -> Result<(), RpcError> {
        let backend = self.backend.clone();
        let events = self.events.clone();
        let tx = self.tx.clone();
        let Some(slot) = self.slots.get_mut(capture_id) else {
            return Err(RpcError::internal("slot vanished"));
        };
        slot.stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = oneshot::channel();
        let spec = CaptureSpec {
            capture_id: capture_id.to_string(),
            source: resolved,
            spectrum: slot.spectrum.clone(),
            pcm: slot.pcm.clone(),
        };
        let wiring = SessionWiring {
            events,
            stats: slot.stats.clone(),
            ready: ready_tx,
            stop: slot.stop.clone(),
            manager: tx.clone(),
        };
        let threads = backend.spawn_capture(spec, wiring)?;
        slot.threads = Some(threads);

        let id = capture_id.to_string();
        tokio::spawn(async move {
            let result = match tokio::time::timeout(READY_TIMEOUT, ready_rx).await {
                Ok(Ok(r)) => r,
                Ok(Err(_)) => Err(SessionError::ThreadDied),
                Err(_) => Err(SessionError::Timeout),
            };
            let _ = tx.send(ManagerCmd::Ready {
                capture_id: id,
                result,
            });
        });
        Ok(())
    }

    async fn handle_start(
        &mut self,
        params: CaptureStartParams,
        reply: oneshot::Sender<Result<CaptureStartResult, RpcError>>,
    ) {
        match self.try_start(params).await {
            Ok(capture_id) => {
                if let Some(slot) = self.slots.get_mut(&capture_id) {
                    slot.pending_start = Some(reply);
                }
                self.emit_state(&capture_id, CaptureState::Starting, None, None, None);
                info!(capture_id, "capture starting");
            }
            Err(e) => {
                let _ = reply.send(Err(e));
            }
        }
    }

    /// Validation + resolution + slot insertion + thread spawn.
    async fn try_start(&mut self, params: CaptureStartParams) -> Result<String, RpcError> {
        if self.slots.len() >= self.limits.max_captures as usize {
            return Err(RpcError::new(
                ErrorCode::CaptureLimitReached,
                format!("at most {} concurrent captures", self.limits.max_captures),
            ));
        }
        let spectrum = params.spectrum.unwrap_or_default();
        let pcm = params.pcm.unwrap_or_default();
        if !spectrum.enabled && !pcm.enabled {
            return Err(RpcError::invalid_params(
                "at least one of spectrum.enabled / pcm.enabled must be true",
            ));
        }
        spectrum
            .validate(&self.limits)
            .map_err(RpcError::invalid_params)?;
        pcm.validate().map_err(RpcError::invalid_params)?;

        match &params.source {
            CaptureSource::Device { .. } if !self.capabilities.device_capture => {
                return Err(RpcError::unsupported());
            }
            CaptureSource::DefaultOutput if !self.capabilities.follow_default_output => {
                return Err(RpcError::unsupported());
            }
            CaptureSource::DefaultInput if !self.capabilities.follow_default_input => {
                return Err(RpcError::unsupported());
            }
            CaptureSource::Process { .. } if !self.capabilities.process_loopback => {
                return Err(RpcError::new(
                    ErrorCode::Unsupported,
                    "process loopback requires Windows 10 build 19041+",
                ));
            }
            CaptureSource::SystemExcludingProcess { .. }
                if !self.capabilities.process_loopback_exclude =>
            {
                return Err(RpcError::new(
                    ErrorCode::Unsupported,
                    "process-exclude loopback requires Windows 10 build 19041+",
                ));
            }
            _ => {}
        }

        let resolved = self.resolve_source(&params.source).await?;
        let capture_id = format!("cap-{}", self.next_id);
        self.next_id += 1;

        let slot = Slot {
            source: params.source,
            spectrum,
            pcm,
            state: CaptureState::Starting,
            device_id: match &resolved {
                ResolvedSource::Device { device_id } => Some(device_id.clone()),
                ResolvedSource::Process { .. } => None,
            },
            format: None,
            started_at_ms: now_ms(),
            stats: Arc::new(SessionStats::default()),
            threads: None,
            stop: Arc::new(AtomicBool::new(false)),
            pending_start: None,
            stop_replies: Vec::new(),
            after: AfterTeardown::Remove,
            attempts: 0,
            running_since: None,
            restart_timer_pending: false,
            tearing_down: false,
        };
        self.slots.insert(capture_id.clone(), slot);
        if let Err(e) = self.spawn_threads(&capture_id, resolved) {
            self.slots.remove(&capture_id);
            return Err(e);
        }
        Ok(capture_id)
    }

    fn handle_ready(&mut self, capture_id: &str, result: Result<ReadyInfo, SessionError>) {
        let Some(slot) = self.slots.get_mut(capture_id) else {
            return;
        };
        if slot.tearing_down {
            return;
        }
        match result {
            Ok(ready) => {
                let was_restart = slot.pending_start.is_none();
                slot.state = CaptureState::Running;
                slot.device_id = ready.device_id.clone();
                slot.format = Some(ready.format);
                slot.running_since = Some(Instant::now());
                slot.attempts = 0;
                if was_restart {
                    slot.stats.restarts.fetch_add(1, Ordering::Relaxed);
                } else if let Some(reply) = slot.pending_start.take() {
                    let result = CaptureStartResult {
                        capture_id: capture_id.to_string(),
                        state: CaptureState::Running,
                        device_id: slot.device_id.clone(),
                        format: ready.format,
                        spectrum: slot.resolved_spectrum(),
                        pcm: slot.pcm.clone(),
                    };
                    let _ = reply.send(Ok(result));
                }
                let device_id = slot.device_id.clone();
                self.emit_state(
                    capture_id,
                    CaptureState::Running,
                    device_id,
                    Some(ready.format),
                    None,
                );
                info!(capture_id, "capture running");
            }
            Err(e) => {
                warn!(capture_id, error = %e, "capture start failed");
                let initial = slot.pending_start.is_some();
                if initial {
                    if let Some(reply) = slot.pending_start.take() {
                        let _ = reply.send(Err(e.to_rpc()));
                    }
                    slot.state = CaptureState::Failed;
                    self.emit_state(
                        capture_id,
                        CaptureState::Failed,
                        None,
                        None,
                        Some(e.reason()),
                    );
                    self.begin_teardown(capture_id, AfterTeardown::Remove);
                } else {
                    slot.attempts = slot.attempts.saturating_add(1);
                    self.begin_teardown(capture_id, AfterTeardown::RestartAfterBackoff);
                }
            }
        }
    }

    fn handle_fatal(&mut self, capture_id: &str, error: SessionError) {
        let Some(slot) = self.slots.get_mut(capture_id) else {
            return;
        };
        if slot.tearing_down || slot.state == CaptureState::Failed {
            return;
        }
        if matches!(error, SessionError::ProcessExited) {
            slot.state = CaptureState::Failed;
            self.emit_state(
                capture_id,
                CaptureState::Failed,
                None,
                None,
                Some(error.reason()),
            );
            self.begin_teardown(capture_id, AfterTeardown::KeepFailed);
            return;
        }
        if let Some(since) = slot.running_since {
            if since.elapsed() > STABLE_RESET {
                slot.attempts = 0;
            }
        }
        slot.state = CaptureState::Restarting;
        let device_id = slot.device_id.clone();
        self.emit_state(
            capture_id,
            CaptureState::Restarting,
            device_id,
            None,
            Some(error.reason()),
        );
        self.begin_teardown(capture_id, AfterTeardown::RestartAfterBackoff);
    }

    fn begin_teardown(&mut self, capture_id: &str, after: AfterTeardown) {
        let Some(slot) = self.slots.get_mut(capture_id) else {
            return;
        };
        slot.after = after;
        slot.tearing_down = true;
        slot.stop.store(true, Ordering::Relaxed);
        let threads = slot.threads.take();
        let tx = self.tx.clone();
        let id = capture_id.to_string();
        tokio::spawn(async move {
            if let Some(threads) = threads {
                let join = tokio::task::spawn_blocking(move || threads.join_both());
                if tokio::time::timeout(JOIN_TIMEOUT, join).await.is_err() {
                    warn!(capture_id = %id, "session threads did not exit in time; detaching");
                }
            }
            let _ = tx.send(ManagerCmd::TornDown { capture_id: id });
        });
    }

    async fn handle_torn_down(&mut self, capture_id: &str) {
        let Some(slot) = self.slots.get_mut(capture_id) else {
            return;
        };
        slot.tearing_down = false;
        match slot.after {
            AfterTeardown::Remove => {
                let was_failed = slot.state == CaptureState::Failed;
                for reply in slot.stop_replies.drain(..) {
                    let _ = reply.send(Ok(()));
                }
                self.slots.remove(capture_id);
                if !was_failed {
                    self.emit_state(capture_id, CaptureState::Stopped, None, None, None);
                }
                info!(capture_id, "capture stopped");
            }
            AfterTeardown::KeepFailed => {
                // Slot stays visible in `capture.list` as failed.
            }
            AfterTeardown::RestartNow => {
                self.respawn(capture_id).await;
            }
            AfterTeardown::RestartAfterBackoff => {
                let delay = backoff(slot.attempts);
                debug!(
                    capture_id,
                    attempts = slot.attempts,
                    ?delay,
                    "scheduling restart"
                );
                self.schedule_retry(capture_id, delay);
            }
        }
    }

    fn schedule_retry(&self, capture_id: &str, delay: Duration) {
        let tx = self.tx.clone();
        let id = capture_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(ManagerCmd::TryRestart { capture_id: id });
        });
    }

    async fn respawn(&mut self, capture_id: &str) {
        let Some(source) = self.slots.get(capture_id).map(|s| s.source.clone()) else {
            return;
        };
        let resolved = match self.resolve_source(&source).await {
            Ok(r) => r,
            Err(e) => {
                warn!(capture_id, error = %e.message, "restart resolution failed");
                if let Some(slot) = self.slots.get_mut(capture_id) {
                    slot.attempts = slot.attempts.saturating_add(1);
                    let delay = backoff(slot.attempts);
                    self.schedule_retry(capture_id, delay);
                }
                return;
            }
        };
        if let Some(slot) = self.slots.get_mut(capture_id) {
            if let ResolvedSource::Device { device_id } = &resolved {
                slot.device_id = Some(device_id.clone());
            }
            slot.state = CaptureState::Restarting;
        }
        if let Err(e) = self.spawn_threads(capture_id, resolved) {
            warn!(capture_id, error = %e.message, "restart spawn failed");
            if let Some(slot) = self.slots.get_mut(capture_id) {
                slot.attempts = slot.attempts.saturating_add(1);
                let delay = backoff(slot.attempts);
                self.schedule_retry(capture_id, delay);
            }
        }
    }

    async fn handle_try_restart(&mut self, capture_id: &str) {
        let Some(slot) = self.slots.get_mut(capture_id) else {
            return;
        };
        if slot.tearing_down {
            return;
        }
        match slot.state {
            CaptureState::Running if slot.restart_timer_pending => {
                slot.restart_timer_pending = false;
                let source = slot.source.clone();
                let current = slot.device_id.clone();
                // Re-check after the debounce: has the default really moved?
                let resolved = self.resolve_source(&source).await.ok();
                let moved = match (&resolved, &current) {
                    (Some(ResolvedSource::Device { device_id }), Some(cur)) => device_id != cur,
                    _ => false,
                };
                if moved {
                    let Some(slot) = self.slots.get_mut(capture_id) else {
                        return;
                    };
                    if slot.tearing_down || slot.state != CaptureState::Running {
                        return;
                    }
                    slot.state = CaptureState::Restarting;
                    let device_id = slot.device_id.clone();
                    self.emit_state(
                        capture_id,
                        CaptureState::Restarting,
                        device_id,
                        None,
                        Some(StateReason {
                            code: "defaultDeviceChanged".into(),
                            message: "default device changed; re-attaching".into(),
                        }),
                    );
                    self.begin_teardown(capture_id, AfterTeardown::RestartNow);
                }
            }
            CaptureState::Restarting if slot.threads.is_none() => {
                self.respawn(capture_id).await;
            }
            _ => {}
        }
    }

    fn handle_default_changed(&mut self, kind: DeviceKind, device_id: String) {
        let ids: Vec<String> = self
            .slots
            .iter()
            .filter(|(_, s)| {
                s.follow_kind() == Some(kind)
                    && s.state == CaptureState::Running
                    && !s.tearing_down
                    && !s.restart_timer_pending
                    && s.device_id.as_deref() != Some(device_id.as_str())
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(slot) = self.slots.get_mut(&id) {
                slot.restart_timer_pending = true;
            }
            debug!(capture_id = %id, "default device changed; debouncing re-attach");
            self.schedule_retry(&id, DEFAULT_CHANGE_DEBOUNCE);
        }
    }

    fn handle_device_gone(&mut self, device_id: &str) {
        let ids: Vec<String> = self
            .slots
            .iter()
            .filter(|(_, s)| {
                s.state == CaptureState::Running
                    && !s.tearing_down
                    && s.device_id.as_deref() == Some(device_id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let Some(slot) = self.slots.get_mut(&id) else {
                continue;
            };
            if let Some(since) = slot.running_since {
                if since.elapsed() > STABLE_RESET {
                    slot.attempts = 0;
                }
            }
            slot.state = CaptureState::Restarting;
            let dev = slot.device_id.clone();
            self.emit_state(
                &id,
                CaptureState::Restarting,
                dev,
                None,
                Some(StateReason {
                    code: "deviceRemoved".into(),
                    message: format!("device {device_id} removed or deactivated"),
                }),
            );
            self.begin_teardown(&id, AfterTeardown::RestartAfterBackoff);
        }
    }

    fn handle_stop(&mut self, capture_id: &str, reply: oneshot::Sender<Result<(), RpcError>>) {
        let Some(slot) = self.slots.get_mut(capture_id) else {
            let _ = reply.send(Err(RpcError::new(
                ErrorCode::CaptureNotFound,
                format!("no capture \"{capture_id}\""),
            )));
            return;
        };
        if slot.state == CaptureState::Failed && slot.threads.is_none() && !slot.tearing_down {
            let _ = reply.send(Ok(()));
            self.slots.remove(capture_id);
            info!(capture_id, "failed capture cleared");
            return;
        }
        slot.stop_replies.push(reply);
        if slot.tearing_down {
            // Override whatever the teardown was going to do: the user wins.
            slot.after = AfterTeardown::Remove;
        } else {
            self.begin_teardown(capture_id, AfterTeardown::Remove);
        }
    }

    async fn handle_stop_all(&mut self, reply: oneshot::Sender<()>) {
        let mut threads = Vec::new();
        for slot in self.slots.values_mut() {
            slot.stop.store(true, Ordering::Relaxed);
            if let Some(t) = slot.threads.take() {
                threads.push(t);
            }
        }
        self.slots.clear();
        let join = tokio::task::spawn_blocking(move || {
            for t in threads {
                t.join_both();
            }
        });
        let _ = tokio::time::timeout(JOIN_TIMEOUT, join).await;
        let _ = reply.send(());
    }
}
