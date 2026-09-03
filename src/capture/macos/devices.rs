//! Core Audio device-manager thread: serves device RPCs, watches HAL
//! properties for hotplug/default changes and maps them to the platform
//! independent device events. Mirrors the Linux pulse-dev-mgr design: the
//! HAL callback only enqueues a wake-up, all queries run on this thread.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::mpsc as std_mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use objc2_core_audio::AudioObjectID;
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::capture::manager::ManagerCmd;
use crate::protocol::events::Event;
use crate::protocol::types::{
    AudioFormat, AudioProcessInfo, DefaultRole, DeviceInfo, DeviceKind, DeviceState,
    SessionActivity,
};
use crate::protocol::{ErrorCode, RpcError};
use crate::rpc::writer::EventTx;
use crate::rpc::{DeviceService, SvcFuture};

use super::hal::{
    self, CaResult, PropertyListener, SELECTOR_DEFAULT_INPUT, SELECTOR_DEFAULT_OUTPUT,
    SELECTOR_DEFAULT_SYSTEM, SELECTOR_DEVICES, SELECTOR_NAME, SELECTOR_PROCESS_BUNDLE,
    SELECTOR_PROCESS_DEVICES, SELECTOR_PROCESS_LIST, SELECTOR_PROCESS_PID,
    SELECTOR_PROCESS_RUNNING_OUTPUT, SELECTOR_STREAM_CONFIG, SELECTOR_STREAM_FORMAT, SELECTOR_UID,
    get_asbd, get_cfstring, get_id_array, get_stream_channels, get_u32, os_product_version,
    process_executable,
};

const DEFAULT_CHANGE_DEBOUNCE: Duration = Duration::from_millis(100);
const IDLE_POLL: Duration = Duration::from_millis(500);
const SYSTEM_OBJECT: AudioObjectID = 1; // kAudioObjectSystemObject

enum DevCmd {
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

enum Msg {
    Cmd(DevCmd),
    Hal(HalEvent),
}

#[derive(Debug, Clone, Copy)]
enum HalEvent {
    DevicesChanged,
    DefaultOutputChanged,
    DefaultInputChanged,
    DefaultSystemChanged,
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
    let (tx, rx) = std_mpsc::channel();
    let join = std::thread::Builder::new()
        .name("ca-dev-mgr".into())
        .spawn({
            let tx = tx.clone();
            move || thread_main(tx, rx, events, manager)
        })
        .expect("failed to spawn Core Audio device manager");
    DeviceMgr {
        handle: DeviceMgrHandle { tx },
        join,
    }
}

#[derive(Clone, Default)]
struct Defaults {
    render: Option<String>,
    capture: Option<String>,
}

struct Snapshot {
    devices: HashMap<String, DeviceInfo>,
    defaults: Defaults,
}

fn thread_main(
    tx: std_mpsc::Sender<Msg>,
    rx: std_mpsc::Receiver<Msg>,
    events: EventTx,
    manager: tokio_mpsc::UnboundedSender<ManagerCmd>,
) {
    let mut snapshot = match read_snapshot() {
        Ok(snapshot) => snapshot,
        Err(reason) => {
            error!(%reason, "Core Audio device snapshot failed");
            serve_init_error(rx, reason);
            return;
        }
    };
    let tx = tx.clone();
    // HAL callbacks run on a Core Audio thread; they only forward a wake-up
    // message through this channel and never touch HAL APIs themselves.
    let wake_ptr: *mut std_mpsc::Sender<Msg> = Box::into_raw(Box::new(tx));
    let listener = unsafe { install_listeners(wake_ptr) };
    if let Err(reason) = listener {
        unsafe { drop(Box::from_raw(wake_ptr)) };
        error!(%reason, "Core Audio property listeners failed");
        serve_init_error(rx, reason);
        return;
    }
    let _listener = listener;
    info!(
        "Core Audio device manager running (macOS {})",
        os_product_version()
    );

    let mut pending: HashMap<(DeviceKind, DefaultRole), (Option<String>, Instant)> = HashMap::new();
    loop {
        let timeout = pending
            .values()
            .map(|(_, deadline)| deadline.saturating_duration_since(Instant::now()))
            .min()
            .unwrap_or(IDLE_POLL);
        match rx.recv_timeout(timeout) {
            Ok(Msg::Cmd(DevCmd::Quit)) => break,
            Ok(Msg::Cmd(cmd)) => handle_cmd(cmd),
            Ok(Msg::Hal(event)) => {
                debug!(?event, "Core Audio property change");
                match event {
                    HalEvent::DevicesChanged => {
                        refresh_snapshot(&mut snapshot, &mut pending, &events, &manager)
                    }
                    HalEvent::DefaultOutputChanged
                    | HalEvent::DefaultInputChanged
                    | HalEvent::DefaultSystemChanged => {
                        refresh_defaults(&mut snapshot, &mut pending)
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        flush_pending_defaults(&mut pending, &events, &manager);
    }
    info!("Core Audio device manager exiting");
    let _ = tx;
}

/// The listeners hold `wake_ptr`; the guard keeps them registered until the
/// thread exits, then the boxed sender is reclaimed after drop.
unsafe fn install_listeners(
    wake_ptr: *mut std_mpsc::Sender<Msg>,
) -> CaResult<[PropertyListener; 4]> {
    unsafe extern "C-unwind" fn on_property(
        _id: AudioObjectID,
        _count: u32,
        address: std::ptr::NonNull<objc2_core_audio::AudioObjectPropertyAddress>,
        client: *mut c_void,
    ) -> i32 {
        let selector = unsafe { address.as_ref().mSelector };
        let event = if selector == SELECTOR_DEVICES {
            HalEvent::DevicesChanged
        } else if selector == SELECTOR_DEFAULT_OUTPUT || selector == SELECTOR_DEFAULT_SYSTEM {
            HalEvent::DefaultOutputChanged
        } else if selector == SELECTOR_DEFAULT_INPUT {
            HalEvent::DefaultInputChanged
        } else {
            return 0;
        };
        let sender = unsafe { &*(client as *const std_mpsc::Sender<Msg>) };
        let _ = sender.send(Msg::Hal(event));
        0
    }

    let global = hal::scope_global();
    let data = wake_ptr as *mut c_void;
    unsafe {
        Ok([
            PropertyListener::add(
                SYSTEM_OBJECT,
                SELECTOR_DEVICES,
                global,
                Some(on_property),
                data,
            )?,
            PropertyListener::add(
                SYSTEM_OBJECT,
                SELECTOR_DEFAULT_OUTPUT,
                global,
                Some(on_property),
                data,
            )?,
            PropertyListener::add(
                SYSTEM_OBJECT,
                SELECTOR_DEFAULT_INPUT,
                global,
                Some(on_property),
                data,
            )?,
            PropertyListener::add(
                SYSTEM_OBJECT,
                SELECTOR_DEFAULT_SYSTEM,
                global,
                Some(on_property),
                data,
            )?,
        ])
    }
}

fn serve_init_error(rx: std_mpsc::Receiver<Msg>, reason: String) {
    while let Ok(msg) = rx.recv() {
        let error = || RpcError::new(ErrorCode::OsError, reason.clone());
        match msg {
            Msg::Cmd(DevCmd::Quit) => break,
            Msg::Cmd(DevCmd::List { reply, .. }) => {
                let _ = reply.send(Err(error()));
            }
            Msg::Cmd(DevCmd::GetDefault { reply, .. }) => {
                let _ = reply.send(Err(error()));
            }
            Msg::Cmd(DevCmd::ListAudioProcesses { reply }) => {
                let _ = reply.send(Err(error()));
            }
            Msg::Hal(_) => {}
        }
    }
}

fn handle_cmd(cmd: DevCmd) {
    match cmd {
        DevCmd::List { kinds, reply } => {
            let _ = reply.send(list_devices(kinds));
        }
        DevCmd::GetDefault { kind, role, reply } => {
            let _ = reply.send(get_default(kind, role));
        }
        DevCmd::ListAudioProcesses { reply } => {
            let _ = reply.send(list_audio_processes());
        }
        DevCmd::Quit => unreachable!("handled by thread loop"),
    }
}

fn refresh_snapshot(
    snapshot: &mut Snapshot,
    pending: &mut HashMap<(DeviceKind, DefaultRole), (Option<String>, Instant)>,
    events: &EventTx,
    manager: &tokio_mpsc::UnboundedSender<ManagerCmd>,
) {
    let next = match read_snapshot() {
        Ok(next) => next,
        Err(reason) => {
            warn!(%reason, "cannot refresh Core Audio snapshot");
            return;
        }
    };

    for (id, device) in &next.devices {
        if !snapshot.devices.contains_key(id) {
            events.send_event(&Event::DeviceAdded {
                device: device.clone(),
            });
        }
    }
    for id in snapshot.devices.keys() {
        if !next.devices.contains_key(id) {
            events.send_event(&Event::DeviceRemoved {
                device_id: id.clone(),
            });
            let _ = manager.send(ManagerCmd::DeviceGone {
                device_id: id.clone(),
            });
        }
    }

    queue_default_changes(&snapshot.defaults, &next.defaults, pending);
    *snapshot = next;
}

fn refresh_defaults(
    snapshot: &mut Snapshot,
    pending: &mut HashMap<(DeviceKind, DefaultRole), (Option<String>, Instant)>,
) {
    let mut next = Defaults::default();
    if let Ok(defaults) = read_defaults() {
        next = defaults;
    }
    queue_default_changes(&snapshot.defaults, &next, pending);
    snapshot.defaults = next;
}

fn queue_default_changes(
    old: &Defaults,
    new: &Defaults,
    pending: &mut HashMap<(DeviceKind, DefaultRole), (Option<String>, Instant)>,
) {
    for (kind, old_id, new_id) in [
        (DeviceKind::Render, &old.render, &new.render),
        (DeviceKind::Capture, &old.capture, &new.capture),
    ] {
        if old_id != new_id {
            for role in [
                DefaultRole::Console,
                DefaultRole::Multimedia,
                DefaultRole::Communications,
            ] {
                pending.insert(
                    (kind, role),
                    (new_id.clone(), Instant::now() + DEFAULT_CHANGE_DEBOUNCE),
                );
            }
        }
    }
}

fn flush_pending_defaults(
    pending: &mut HashMap<(DeviceKind, DefaultRole), (Option<String>, Instant)>,
    events: &EventTx,
    manager: &tokio_mpsc::UnboundedSender<ManagerCmd>,
) {
    let now = Instant::now();
    let due: Vec<_> = pending
        .iter()
        .filter(|(_, (_, deadline))| *deadline <= now)
        .map(|(key, _)| *key)
        .collect();
    for (kind, role) in due {
        let (device_id, _) = pending
            .remove(&(kind, role))
            .expect("pending key disappeared");
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

// ---------------------------------------------------------------------------
// Snapshot queries
// ---------------------------------------------------------------------------

fn device_ids(kind: DeviceKind) -> CaResult<Vec<AudioObjectID>> {
    unsafe {
        get_id_array(SYSTEM_OBJECT, SELECTOR_DEVICES, hal::scope_global()).map(|ids| {
            ids.into_iter()
                .filter(|id| {
                    let (scope, selector) = match kind {
                        DeviceKind::Render => (hal::scopes_output(), SELECTOR_STREAM_CONFIG),
                        DeviceKind::Capture => (hal::scopes_input(), SELECTOR_STREAM_CONFIG),
                    };
                    get_stream_channels(*id, selector, scope)
                        .map(|channels| channels > 0)
                        .unwrap_or(false)
                })
                .collect()
        })
    }
}

fn device_uid(id: AudioObjectID) -> CaResult<String> {
    unsafe { get_cfstring(id, SELECTOR_UID, hal::scope_global()) }
}

/// Wire id for a direction of a device: `ca:out:<uid>` / `ca:in:<uid>`.
fn wire_id(uid: &str, kind: DeviceKind) -> String {
    match kind {
        DeviceKind::Render => format!("ca:out:{uid}"),
        DeviceKind::Capture => format!("ca:in:{uid}"),
    }
}

fn strip_wire_id(wire: &str) -> &str {
    wire.trim_start_matches("ca:out:")
        .trim_start_matches("ca:in:")
}

fn device_name(id: AudioObjectID) -> String {
    unsafe { get_cfstring(id, SELECTOR_NAME, hal::scope_global()).unwrap_or_default() }
        .trim()
        .to_string()
}

fn device_format(id: AudioObjectID, kind: DeviceKind) -> Option<AudioFormat> {
    let scope = match kind {
        DeviceKind::Render => hal::scopes_output(),
        DeviceKind::Capture => hal::scopes_input(),
    };
    let asbd = unsafe { get_asbd(id, SELECTOR_STREAM_FORMAT, scope) }.ok()?;
    let rate = asbd.mSampleRate;
    if !(1000.0..=768_000.0).contains(&rate) {
        return None;
    }
    Some(AudioFormat {
        sample_rate: rate as u32,
        channels: asbd.mChannelsPerFrame.clamp(1, u8::MAX as u32) as u16,
    })
}

fn read_defaults() -> CaResult<Defaults> {
    unsafe {
        let render = default_uid(objc2_core_audio::kAudioHardwarePropertyDefaultOutputDevice)
            .ok()
            .filter(|uid| !uid.is_empty())
            .map(|uid| wire_id(&uid, DeviceKind::Render));
        let capture = default_uid(objc2_core_audio::kAudioHardwarePropertyDefaultInputDevice)
            .ok()
            .filter(|uid| !uid.is_empty())
            .map(|uid| wire_id(&uid, DeviceKind::Capture));
        Ok(Defaults { render, capture })
    }
}

unsafe fn default_uid(selector: objc2_core_audio::AudioObjectPropertySelector) -> CaResult<String> {
    let id = unsafe { get_u32(SYSTEM_OBJECT, selector, hal::scope_global())? };
    if id == 0 {
        return Ok(String::new());
    }
    device_uid(id)
}

fn read_snapshot() -> CaResult<Snapshot> {
    let defaults = read_defaults()?;
    let mut devices = HashMap::new();
    for kind in [DeviceKind::Render, DeviceKind::Capture] {
        for id in device_ids(kind)? {
            let uid = match device_uid(id) {
                Ok(uid) if !uid.is_empty() => uid,
                _ => continue,
            };
            let wire = wire_id(&uid, kind);
            let is_default = match kind {
                DeviceKind::Render => defaults.render.as_deref() == Some(wire.as_str()),
                DeviceKind::Capture => defaults.capture.as_deref() == Some(wire.as_str()),
            };
            let device = DeviceInfo {
                id: wire.clone(),
                name: if device_name(id).is_empty() {
                    uid.clone()
                } else {
                    device_name(id)
                },
                kind,
                is_default,
                is_default_communications: is_default,
                state: DeviceState::Active,
                format: device_format(id, kind),
            };
            devices.insert(wire, device);
        }
    }
    Ok(Snapshot { devices, defaults })
}

fn list_devices(kinds: Option<Vec<DeviceKind>>) -> Result<Vec<DeviceInfo>, RpcError> {
    let selected: std::collections::HashSet<DeviceKind> = kinds
        .unwrap_or_else(|| vec![DeviceKind::Render, DeviceKind::Capture])
        .into_iter()
        .collect();
    let snapshot = read_snapshot().map_err(os_error)?;
    let mut devices: Vec<DeviceInfo> = snapshot
        .devices
        .into_values()
        .filter(|device| selected.contains(&device.kind))
        .collect();
    devices.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(devices)
}

fn get_default(kind: DeviceKind, _role: DefaultRole) -> Result<DeviceInfo, RpcError> {
    let snapshot = read_snapshot().map_err(os_error)?;
    let id = match kind {
        DeviceKind::Render => snapshot.defaults.render,
        DeviceKind::Capture => snapshot.defaults.capture,
    }
    .ok_or_else(|| RpcError::new(ErrorCode::DeviceNotFound, "no default Core Audio device"))?;
    snapshot
        .devices
        .into_values()
        .find(|device| device.id == id)
        .ok_or_else(|| RpcError::new(ErrorCode::DeviceNotFound, "default device disappeared"))
}

fn list_audio_processes() -> Result<Vec<AudioProcessInfo>, RpcError> {
    unsafe {
        let objects = get_id_array(SYSTEM_OBJECT, SELECTOR_PROCESS_LIST, hal::scope_global())
            .map_err(os_error)?;
        let mut processes = Vec::new();
        for object in objects {
            let pid = match get_u32(object, SELECTOR_PROCESS_PID, hal::scope_global()) {
                Ok(pid) if pid != 0 => pid,
                _ => continue,
            };
            let bundle = get_cfstring(object, SELECTOR_PROCESS_BUNDLE, hal::scope_global())
                .unwrap_or_default();
            let running = get_u32(
                object,
                SELECTOR_PROCESS_RUNNING_OUTPUT,
                hal::scopes_output(),
            )
            .unwrap_or(0);
            let device_ids: Vec<String> =
                get_id_array(object, SELECTOR_PROCESS_DEVICES, hal::scopes_output())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|dev| {
                        device_uid(dev)
                            .ok()
                            .map(|uid| wire_id(&uid, DeviceKind::Render))
                    })
                    .collect();
            let executable = process_executable(pid);
            let name = executable
                .as_deref()
                .and_then(|path| path.rsplit('/').next())
                .unwrap_or(bundle.as_str())
                .to_string();
            processes.push(AudioProcessInfo {
                pid,
                name,
                executable,
                state: if running != 0 {
                    SessionActivity::Active
                } else {
                    SessionActivity::Inactive
                },
                device_ids,
                display_name: if bundle.is_empty() {
                    None
                } else {
                    Some(bundle)
                },
            });
        }
        processes.sort_by(|a, b| {
            matches!(b.state, SessionActivity::Active)
                .cmp(&matches!(a.state, SessionActivity::Active))
                .then_with(|| a.pid.cmp(&b.pid))
        });
        Ok(processes)
    }
}

fn os_error(reason: String) -> RpcError {
    RpcError::new(ErrorCode::OsError, reason)
}

#[cfg(test)]
mod tests {
    use super::strip_wire_id;

    #[test]
    fn wire_id_round_trip() {
        assert_eq!(
            strip_wire_id("ca:out:BuiltInSpeakerDevice"),
            "BuiltInSpeakerDevice"
        );
        assert_eq!(
            strip_wire_id("ca:in:AppleHDAEngineInput"),
            "AppleHDAEngineInput"
        );
    }
}
