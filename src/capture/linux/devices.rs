//! PulseAudio device-manager thread: serves device RPCs and translates
//! subscription notifications into the platform-independent device events.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc as std_mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use libpulse_binding::context::subscribe::{Facility, InterestMaskSet, Operation};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::capture::manager::ManagerCmd;
use crate::protocol::events::Event;
use crate::protocol::types::{
    AudioFormat, AudioProcessInfo, DefaultRole, DeviceInfo, DeviceKind, DeviceState,
};
use crate::protocol::{ErrorCode, RpcError};
use crate::rpc::writer::EventTx;
use crate::rpc::{DeviceService, SvcFuture};

use super::{processes, pulse::PulseClient};

const DEFAULT_CHANGE_DEBOUNCE: Duration = Duration::from_millis(100);
const IDLE_POLL: Duration = Duration::from_millis(250);
const ROLES: [DefaultRole; 3] = [
    DefaultRole::Console,
    DefaultRole::Multimedia,
    DefaultRole::Communications,
];

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
    Pulse {
        facility: Option<Facility>,
        operation: Option<Operation>,
        index: u32,
    },
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
    let callback_tx = tx.clone();
    let join = std::thread::Builder::new()
        .name("pulse-dev-mgr".into())
        .spawn(move || thread_main(rx, callback_tx, events, manager))
        .expect("failed to spawn PulseAudio device manager");
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
    rx: std_mpsc::Receiver<Msg>,
    callback_tx: std_mpsc::Sender<Msg>,
    events: EventTx,
    manager: tokio_mpsc::UnboundedSender<ManagerCmd>,
) {
    let client = match PulseClient::connect("audio-sidecar-device-manager") {
        Ok(client) => client,
        Err(reason) => {
            error!(%reason, "PulseAudio device manager init failed");
            serve_init_error(rx, reason);
            return;
        }
    };
    let mut snapshot = match read_snapshot(&client) {
        Ok(snapshot) => snapshot,
        Err(reason) => {
            error!(%reason, "PulseAudio device snapshot failed");
            serve_init_error(rx, reason);
            return;
        }
    };
    let mask = InterestMaskSet::SINK | InterestMaskSet::SOURCE | InterestMaskSet::SERVER;
    if let Err(reason) = client.subscribe(mask, move |facility, operation, index| {
        let _ = callback_tx.send(Msg::Pulse {
            facility,
            operation,
            index,
        });
    }) {
        error!(%reason, "PulseAudio subscription failed");
        serve_init_error(rx, reason);
        return;
    }
    info!("PulseAudio device manager running");

    let mut pending_defaults: HashMap<(DeviceKind, DefaultRole), (Option<String>, Instant)> =
        HashMap::new();
    loop {
        let timeout = pending_defaults
            .values()
            .map(|(_, deadline)| deadline.saturating_duration_since(Instant::now()))
            .min()
            .unwrap_or(IDLE_POLL);
        match rx.recv_timeout(timeout) {
            Ok(Msg::Cmd(DevCmd::Quit)) => break,
            Ok(Msg::Cmd(cmd)) => handle_cmd(&client, cmd),
            Ok(Msg::Pulse {
                facility,
                operation,
                index,
            }) => {
                debug!(
                    ?facility,
                    ?operation,
                    index,
                    "PulseAudio subscription event"
                );
                if matches!(
                    facility,
                    Some(Facility::Sink | Facility::Source | Facility::Server)
                ) {
                    refresh_snapshot(
                        &client,
                        &mut snapshot,
                        &mut pending_defaults,
                        &events,
                        &manager,
                    );
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        flush_pending_defaults(&mut pending_defaults, &events, &manager);
    }
    info!("PulseAudio device manager exiting");
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
            Msg::Pulse { .. } => {}
        }
    }
}

fn handle_cmd(client: &PulseClient, cmd: DevCmd) {
    match cmd {
        DevCmd::List { kinds, reply } => {
            let _ = reply.send(list_devices(client, kinds));
        }
        DevCmd::GetDefault { kind, role, reply } => {
            let _ = reply.send(get_default(client, kind, role));
        }
        DevCmd::ListAudioProcesses { reply } => {
            let _ = reply.send(processes::list(client));
        }
        DevCmd::Quit => unreachable!("handled by thread loop"),
    }
}

fn refresh_snapshot(
    client: &PulseClient,
    snapshot: &mut Snapshot,
    pending: &mut HashMap<(DeviceKind, DefaultRole), (Option<String>, Instant)>,
    events: &EventTx,
    manager: &tokio_mpsc::UnboundedSender<ManagerCmd>,
) {
    let next = match read_snapshot(client) {
        Ok(next) => next,
        Err(reason) => {
            warn!(%reason, "cannot refresh PulseAudio device snapshot");
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
            for role in ROLES {
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

fn read_snapshot(client: &PulseClient) -> Result<Snapshot, String> {
    let devices = read_devices(client)?
        .into_iter()
        .map(|device| (device.id.clone(), device))
        .collect();
    let defaults = read_defaults(client)?;
    Ok(Snapshot { devices, defaults })
}

fn read_defaults(client: &PulseClient) -> Result<Defaults, String> {
    let server = client.server()?;
    Ok(Defaults {
        render: server.default_sink_name.as_deref().map(sink_id),
        capture: server.default_source_name.as_deref().map(source_id),
    })
}

fn read_devices(client: &PulseClient) -> Result<Vec<DeviceInfo>, String> {
    let defaults = read_defaults(client)?;
    let mut devices = Vec::new();
    for sink in client.list_sinks()? {
        let id = sink_id(&sink.name);
        let is_default = defaults.render.as_deref() == Some(id.as_str());
        devices.push(DeviceInfo {
            id,
            name: display_name(&sink.description, &sink.name),
            kind: DeviceKind::Render,
            is_default,
            is_default_communications: is_default,
            state: DeviceState::Active,
            format: Some(format(sink.sample_spec)),
        });
    }
    for source in client.list_sources()? {
        let id = source_id(&source.name);
        let is_default = defaults.capture.as_deref() == Some(id.as_str());
        devices.push(DeviceInfo {
            id,
            name: display_name(&source.description, &source.name),
            kind: DeviceKind::Capture,
            is_default,
            is_default_communications: is_default,
            state: DeviceState::Active,
            format: Some(format(source.sample_spec)),
        });
    }
    Ok(devices)
}

fn list_devices(
    client: &PulseClient,
    kinds: Option<Vec<DeviceKind>>,
) -> Result<Vec<DeviceInfo>, RpcError> {
    let selected: HashSet<_> = kinds
        .unwrap_or_else(|| vec![DeviceKind::Render, DeviceKind::Capture])
        .into_iter()
        .collect();
    let mut devices: Vec<_> = read_devices(client)
        .map_err(os_error)?
        .into_iter()
        .filter(|device| selected.contains(&device.kind))
        .collect();
    devices.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(devices)
}

fn get_default(
    client: &PulseClient,
    kind: DeviceKind,
    _role: DefaultRole,
) -> Result<DeviceInfo, RpcError> {
    let defaults = read_defaults(client).map_err(os_error)?;
    let id = match kind {
        DeviceKind::Render => defaults.render,
        DeviceKind::Capture => defaults.capture,
    }
    .ok_or_else(|| RpcError::new(ErrorCode::DeviceNotFound, "no default PulseAudio device"))?;
    read_devices(client)
        .map_err(os_error)?
        .into_iter()
        .find(|device| device.id == id)
        .ok_or_else(|| RpcError::new(ErrorCode::DeviceNotFound, "default device disappeared"))
}

fn format(spec: libpulse_binding::sample::Spec) -> AudioFormat {
    AudioFormat {
        sample_rate: spec.rate,
        channels: u16::from(spec.channels),
    }
}

fn display_name(description: &str, name: &str) -> String {
    if description.is_empty() {
        name.to_string()
    } else {
        description.to_string()
    }
}

pub(crate) fn sink_id(name: &str) -> String {
    format!("pulse:sink:{name}")
}

pub(crate) fn source_id(name: &str) -> String {
    format!("pulse:source:{name}")
}

fn os_error(reason: String) -> RpcError {
    RpcError::new(ErrorCode::OsError, reason)
}

#[cfg(test)]
mod tests {
    use super::{display_name, sink_id, source_id};

    #[test]
    fn pulse_ids_preserve_native_names() {
        assert_eq!(sink_id("alsa_output.pci"), "pulse:sink:alsa_output.pci");
        assert_eq!(
            source_id("alsa_input.usb:mic"),
            "pulse:source:alsa_input.usb:mic"
        );
    }

    #[test]
    fn empty_description_falls_back_to_native_name() {
        assert_eq!(display_name("", "source-name"), "source-name");
        assert_eq!(display_name("USB Mic", "source-name"), "USB Mic");
    }
}
