//! The dev-mgr thread: owns the device enumerators (wasapi for queries, a raw
//! IMMDeviceEnumerator for state-mask listing and endpoint notifications),
//! serves device RPCs, and forwards hotplug/default-change events.

use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use tracing::{debug, error, info, warn};
use wasapi::Direction;
use windows::Win32::Media::Audio::{
    DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, eCapture, eCommunications, eConsole,
    eMultimedia, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::core::{PCWSTR, implement};

use crate::capture::manager::ManagerCmd;
use crate::protocol::events::Event;
use crate::protocol::types::{
    AudioFormat, AudioProcessInfo, DefaultRole, DeviceInfo, DeviceKind, DeviceState,
};
use crate::protocol::{ErrorCode, RpcError};
use crate::rpc::writer::EventTx;
use crate::rpc::{DeviceService, SvcFuture};

use super::audio_sessions;

const DEFAULT_CHANGE_DEBOUNCE: Duration = Duration::from_millis(100);
/// All DEVICE_STATE_* bits: active | disabled | notpresent | unplugged.
const STATE_MASK_ALL: DEVICE_STATE = DEVICE_STATE(0xF);

pub enum DevCmd {
    List {
        kinds: Option<Vec<DeviceKind>>,
        reply: oneshot::Sender<Result<Vec<DeviceInfo>, RpcError>>,
    },
    GetDefault {
        kind: DeviceKind,
        role: DefaultRole,
        reply: oneshot::Sender<Result<DeviceInfo, RpcError>>,
    },
    ListAudioProcesses {
        reply: oneshot::Sender<Result<Vec<AudioProcessInfo>, RpcError>>,
    },
    Quit,
}

enum Notif {
    Added(String),
    Removed(String),
    State(String, u32),
    Default(DeviceKind, DefaultRole, Option<String>),
}

enum Msg {
    Cmd(DevCmd),
    Notif(Notif),
}

#[derive(Clone)]
pub struct DeviceMgrHandle {
    tx: std_mpsc::Sender<Msg>,
}

impl DeviceMgrHandle {
    pub fn quit(&self) {
        let _ = self.tx.send(Msg::Cmd(DevCmd::Quit));
    }

    fn request<T: Send + 'static>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, RpcError>>) -> DevCmd,
    ) -> SvcFuture<T> {
        let (reply, rx) = oneshot::channel();
        let sent = self.tx.send(Msg::Cmd(make(reply))).is_ok();
        Box::pin(async move {
            if !sent {
                return Err(RpcError::internal("device manager unavailable"));
            }
            rx.await
                .map_err(|_| RpcError::internal("device manager dropped request"))?
        })
    }
}

impl DeviceService for DeviceMgrHandle {
    fn list(&self, kinds: Option<Vec<DeviceKind>>) -> SvcFuture<Vec<DeviceInfo>> {
        self.request(|reply| DevCmd::List { kinds, reply })
    }

    fn get_default(&self, kind: DeviceKind, role: DefaultRole) -> SvcFuture<DeviceInfo> {
        self.request(|reply| DevCmd::GetDefault { kind, role, reply })
    }

    fn list_audio_processes(&self) -> SvcFuture<Vec<AudioProcessInfo>> {
        self.request(|reply| DevCmd::ListAudioProcesses { reply })
    }
}

pub struct DeviceMgr {
    pub handle: DeviceMgrHandle,
    pub join: std::thread::JoinHandle<()>,
}

pub fn spawn(events: EventTx, manager: tokio_mpsc::UnboundedSender<ManagerCmd>) -> DeviceMgr {
    let (tx, rx) = std_mpsc::channel::<Msg>();
    let callback_tx = tx.clone();
    let join = std::thread::Builder::new()
        .name("dev-mgr".into())
        .spawn(move || thread_main(rx, callback_tx, events, manager))
        .expect("failed to spawn dev-mgr thread");
    DeviceMgr {
        handle: DeviceMgrHandle { tx },
        join,
    }
}

// ---------------------------------------------------------------------------
// COM notification client
// ---------------------------------------------------------------------------

#[implement(IMMNotificationClient)]
struct NotifyClient {
    tx: std_mpsc::Sender<Msg>,
}

fn pcwstr_opt(id: &PCWSTR) -> Option<String> {
    if id.is_null() {
        None
    } else {
        unsafe { id.to_string().ok() }
    }
}

impl IMMNotificationClient_Impl for NotifyClient_Impl {
    fn OnDeviceStateChanged(
        &self,
        pwstrdeviceid: &PCWSTR,
        dwnewstate: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        if let Some(id) = pcwstr_opt(pwstrdeviceid) {
            let _ = self.tx.send(Msg::Notif(Notif::State(id, dwnewstate.0)));
        }
        Ok(())
    }

    fn OnDeviceAdded(&self, pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
        if let Some(id) = pcwstr_opt(pwstrdeviceid) {
            let _ = self.tx.send(Msg::Notif(Notif::Added(id)));
        }
        Ok(())
    }

    fn OnDeviceRemoved(&self, pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
        if let Some(id) = pcwstr_opt(pwstrdeviceid) {
            let _ = self.tx.send(Msg::Notif(Notif::Removed(id)));
        }
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        pwstrdefaultdeviceid: &PCWSTR,
    ) -> windows::core::Result<()> {
        let kind = if flow == eRender {
            DeviceKind::Render
        } else if flow == eCapture {
            DeviceKind::Capture
        } else {
            return Ok(());
        };
        let role = if role == eConsole {
            DefaultRole::Console
        } else if role == eMultimedia {
            DefaultRole::Multimedia
        } else if role == eCommunications {
            DefaultRole::Communications
        } else {
            return Ok(());
        };
        let _ = self.tx.send(Msg::Notif(Notif::Default(
            kind,
            role,
            pcwstr_opt(pwstrdefaultdeviceid),
        )));
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _pwstrdeviceid: &PCWSTR,
        _key: &windows::Win32::Foundation::PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Thread body
// ---------------------------------------------------------------------------

struct DevCtx {
    wasapi_enum: wasapi::DeviceEnumerator,
    raw: IMMDeviceEnumerator,
    notify: IMMNotificationClient,
}

impl DevCtx {
    fn new(callback_tx: std_mpsc::Sender<Msg>) -> Result<Self, String> {
        let raw: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|e| format!("CoCreateInstance(MMDeviceEnumerator): {e}"))?;
        let notify: IMMNotificationClient = NotifyClient { tx: callback_tx }.into();
        unsafe { raw.RegisterEndpointNotificationCallback(&notify) }
            .map_err(|e| format!("RegisterEndpointNotificationCallback: {e}"))?;
        let wasapi_enum =
            wasapi::DeviceEnumerator::new().map_err(|e| format!("DeviceEnumerator: {e}"))?;
        Ok(Self {
            wasapi_enum,
            raw,
            notify,
        })
    }
}

impl Drop for DevCtx {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .raw
                .UnregisterEndpointNotificationCallback(&self.notify);
        }
    }
}

fn thread_main(
    rx: std_mpsc::Receiver<Msg>,
    callback_tx: std_mpsc::Sender<Msg>,
    events: EventTx,
    manager: tokio_mpsc::UnboundedSender<ManagerCmd>,
) {
    let _ = wasapi::initialize_mta();
    let ctx = match DevCtx::new(callback_tx) {
        Ok(ctx) => ctx,
        Err(e) => {
            error!("device manager init failed: {e}");
            // Keep answering RPCs with errors until told to quit, so the
            // sidecar stays functional for capture/media.
            while let Ok(msg) = rx.recv() {
                match msg {
                    Msg::Cmd(DevCmd::Quit) => break,
                    Msg::Cmd(DevCmd::List { reply, .. }) => {
                        let _ = reply.send(Err(RpcError::new(ErrorCode::OsError, e.clone())));
                    }
                    Msg::Cmd(DevCmd::GetDefault { reply, .. }) => {
                        let _ = reply.send(Err(RpcError::new(ErrorCode::OsError, e.clone())));
                    }
                    Msg::Cmd(DevCmd::ListAudioProcesses { reply }) => {
                        let _ = reply.send(Err(RpcError::new(ErrorCode::OsError, e.clone())));
                    }
                    Msg::Notif(_) => {}
                }
            }
            return;
        }
    };
    info!("device manager running");

    let mut pending_defaults: HashMap<(DeviceKind, DefaultRole), (Option<String>, Instant)> =
        HashMap::new();
    loop {
        let timeout = pending_defaults
            .values()
            .map(|(_, deadline)| deadline.saturating_duration_since(Instant::now()))
            .min()
            .unwrap_or(Duration::from_millis(250));
        match rx.recv_timeout(timeout) {
            Ok(Msg::Cmd(DevCmd::Quit)) => break,
            Ok(Msg::Cmd(cmd)) => handle_cmd(&ctx, cmd),
            Ok(Msg::Notif(n)) => handle_notif(&ctx, n, &mut pending_defaults, &events, &manager),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        flush_pending_defaults(&mut pending_defaults, &events, &manager);
    }
    info!("device manager exiting");
}

fn flush_pending_defaults(
    pending: &mut HashMap<(DeviceKind, DefaultRole), (Option<String>, Instant)>,
    events: &EventTx,
    manager: &tokio_mpsc::UnboundedSender<ManagerCmd>,
) {
    let now = Instant::now();
    let due: Vec<(DeviceKind, DefaultRole)> = pending
        .iter()
        .filter(|(_, (_, deadline))| *deadline <= now)
        .map(|(k, _)| *k)
        .collect();
    for key in due {
        let (device_id, _) = pending.remove(&key).expect("key just seen");
        let (kind, role) = key;
        debug!(?kind, ?role, ?device_id, "default device changed");
        events.send_event(&Event::DeviceDefaultChanged {
            kind,
            role,
            device_id: device_id.clone(),
        });
        if role == DefaultRole::Multimedia {
            if let Some(device_id) = device_id {
                let _ = manager.send(ManagerCmd::DefaultChanged { kind, device_id });
            }
        }
    }
}

fn handle_notif(
    ctx: &DevCtx,
    notif: Notif,
    pending: &mut HashMap<(DeviceKind, DefaultRole), (Option<String>, Instant)>,
    events: &EventTx,
    manager: &tokio_mpsc::UnboundedSender<ManagerCmd>,
) {
    match notif {
        Notif::Added(id) => match device_info_by_id(ctx, &id) {
            Ok(device) => events.send_event(&Event::DeviceAdded { device }),
            Err(e) => warn!(id, "device added but unreadable: {}", e.message),
        },
        Notif::Removed(id) => {
            events.send_event(&Event::DeviceRemoved {
                device_id: id.clone(),
            });
            let _ = manager.send(ManagerCmd::DeviceGone { device_id: id });
        }
        Notif::State(id, raw_state) => {
            let state = map_raw_state(raw_state);
            events.send_event(&Event::DeviceStateChanged {
                device_id: id.clone(),
                state,
            });
            if state != DeviceState::Active {
                let _ = manager.send(ManagerCmd::DeviceGone { device_id: id });
            }
        }
        Notif::Default(kind, role, device_id) => {
            pending.insert(
                (kind, role),
                (device_id, Instant::now() + DEFAULT_CHANGE_DEBOUNCE),
            );
        }
    }
}

fn handle_cmd(ctx: &DevCtx, cmd: DevCmd) {
    match cmd {
        DevCmd::List { kinds, reply } => {
            let _ = reply.send(list_devices(ctx, kinds));
        }
        DevCmd::GetDefault { kind, role, reply } => {
            let _ = reply.send(get_default(ctx, kind, role));
        }
        DevCmd::ListAudioProcesses { reply } => {
            let _ = reply.send(audio_sessions::list(&ctx.raw));
        }
        DevCmd::Quit => unreachable!("handled by the loop"),
    }
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

#[derive(Default)]
struct DefaultIds {
    render_multimedia: Option<String>,
    render_comm: Option<String>,
    capture_multimedia: Option<String>,
    capture_comm: Option<String>,
}

fn default_id(ctx: &DevCtx, direction: &Direction, role: &wasapi::Role) -> Option<String> {
    ctx.wasapi_enum
        .get_default_device_for_role(direction, role)
        .ok()
        .and_then(|d| d.get_id().ok())
}

fn default_ids(ctx: &DevCtx) -> DefaultIds {
    DefaultIds {
        render_multimedia: default_id(ctx, &Direction::Render, &wasapi::Role::Multimedia),
        render_comm: default_id(ctx, &Direction::Render, &wasapi::Role::Communications),
        capture_multimedia: default_id(ctx, &Direction::Capture, &wasapi::Role::Multimedia),
        capture_comm: default_id(ctx, &Direction::Capture, &wasapi::Role::Communications),
    }
}

fn map_state(state: wasapi::DeviceState) -> DeviceState {
    match state {
        wasapi::DeviceState::Active => DeviceState::Active,
        wasapi::DeviceState::Disabled => DeviceState::Disabled,
        wasapi::DeviceState::NotPresent => DeviceState::NotPresent,
        wasapi::DeviceState::Unplugged => DeviceState::Unplugged,
    }
}

fn map_raw_state(raw: u32) -> DeviceState {
    match raw {
        1 => DeviceState::Active,
        2 => DeviceState::Disabled,
        4 => DeviceState::NotPresent,
        _ => DeviceState::Unplugged,
    }
}

fn device_info(device: &wasapi::Device, defaults: &DefaultIds) -> Result<DeviceInfo, RpcError> {
    let id = device
        .get_id()
        .map_err(|e| RpcError::new(ErrorCode::OsError, e.to_string()))?;
    let kind = match device.get_direction() {
        Direction::Render => DeviceKind::Render,
        Direction::Capture => DeviceKind::Capture,
    };
    let state = device
        .get_state()
        .map(map_state)
        .unwrap_or(DeviceState::NotPresent);
    let format = if state == DeviceState::Active {
        device
            .get_iaudioclient()
            .ok()
            .and_then(|c| c.get_mixformat().ok())
            .map(|f| AudioFormat {
                sample_rate: f.get_samplespersec(),
                channels: f.get_nchannels(),
            })
    } else {
        None
    };
    let (is_default, is_default_communications) = match kind {
        DeviceKind::Render => (
            defaults.render_multimedia.as_deref() == Some(id.as_str()),
            defaults.render_comm.as_deref() == Some(id.as_str()),
        ),
        DeviceKind::Capture => (
            defaults.capture_multimedia.as_deref() == Some(id.as_str()),
            defaults.capture_comm.as_deref() == Some(id.as_str()),
        ),
    };
    Ok(DeviceInfo {
        id,
        name: device
            .get_friendlyname()
            .unwrap_or_else(|_| "(unknown)".into()),
        kind,
        is_default,
        is_default_communications,
        state,
        format,
    })
}

fn device_info_by_id(ctx: &DevCtx, id: &str) -> Result<DeviceInfo, RpcError> {
    let device = ctx
        .wasapi_enum
        .get_device(id)
        .map_err(|e| RpcError::new(ErrorCode::DeviceNotFound, e.to_string()))?;
    device_info(&device, &default_ids(ctx))
}

fn list_devices(ctx: &DevCtx, kinds: Option<Vec<DeviceKind>>) -> Result<Vec<DeviceInfo>, RpcError> {
    let defaults = default_ids(ctx);
    let mut kinds = kinds.unwrap_or_else(|| vec![DeviceKind::Render, DeviceKind::Capture]);
    kinds.dedup();
    let mut out = Vec::new();
    for kind in kinds {
        let flow = match kind {
            DeviceKind::Render => eRender,
            DeviceKind::Capture => eCapture,
        };
        let collection = unsafe { ctx.raw.EnumAudioEndpoints(flow, STATE_MASK_ALL) }
            .map_err(|e| RpcError::new(ErrorCode::OsError, e.to_string()))?;
        let count = unsafe { collection.GetCount() }
            .map_err(|e| RpcError::new(ErrorCode::OsError, e.to_string()))?;
        for i in 0..count {
            let Ok(imm) = (unsafe { collection.Item(i) }) else {
                continue;
            };
            let Ok(device) = wasapi::Device::from_immdevice(imm) else {
                continue;
            };
            match device_info(&device, &defaults) {
                Ok(info) => out.push(info),
                Err(e) => debug!("skipping unreadable device: {}", e.message),
            }
        }
    }
    out.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

fn get_default(ctx: &DevCtx, kind: DeviceKind, role: DefaultRole) -> Result<DeviceInfo, RpcError> {
    let direction = match kind {
        DeviceKind::Render => Direction::Render,
        DeviceKind::Capture => Direction::Capture,
    };
    let wrole = match role {
        DefaultRole::Console => wasapi::Role::Console,
        DefaultRole::Multimedia => wasapi::Role::Multimedia,
        DefaultRole::Communications => wasapi::Role::Communications,
    };
    let device = ctx
        .wasapi_enum
        .get_default_device_for_role(&direction, &wrole)
        .map_err(|e| RpcError::new(ErrorCode::DeviceNotFound, format!("no default device: {e}")))?;
    device_info(&device, &default_ids(ctx))
}
