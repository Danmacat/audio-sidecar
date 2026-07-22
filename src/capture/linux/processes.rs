//! Enumerate PulseAudio sink-inputs and aggregate them into the protocol's
//! process picker rows.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::protocol::types::{AudioProcessInfo, SessionActivity};
use crate::protocol::{ErrorCode, RpcError};

use super::devices::sink_id;
use super::pulse::{PulseClient, SinkInput};

pub fn list(client: &PulseClient) -> Result<Vec<AudioProcessInfo>, RpcError> {
    let sink_names: HashMap<u32, String> = client
        .list_sinks()
        .map_err(os_error)?
        .into_iter()
        .map(|sink| (sink.index, sink.name))
        .collect();
    let mut by_pid = HashMap::<u32, AudioProcessInfo>::new();
    for input in client.list_sink_inputs().map_err(os_error)? {
        let Some(pid) = input.pid.filter(|pid| *pid != 0) else {
            continue;
        };
        let device_id = sink_names.get(&input.sink).map(|name| sink_id(name));
        let entry = by_pid
            .entry(pid)
            .or_insert_with(|| process_info(pid, &input));
        if !input.corked {
            entry.state = SessionActivity::Active;
        }
        if let Some(device_id) = device_id {
            if !entry.device_ids.contains(&device_id) {
                entry.device_ids.push(device_id);
            }
        }
        if entry.display_name.is_none() {
            entry.display_name = input.application_name;
        }
    }

    let mut processes: Vec<_> = by_pid.into_values().collect();
    processes.sort_by(|a, b| {
        (b.state == SessionActivity::Active)
            .cmp(&(a.state == SessionActivity::Active))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.pid.cmp(&b.pid))
    });
    Ok(processes)
}

fn process_info(pid: u32, input: &SinkInput) -> AudioProcessInfo {
    let executable = std::fs::read_link(PathBuf::from("/proc").join(pid.to_string()).join("exe"))
        .ok()
        .map(|path| path.to_string_lossy().into_owned());
    let name = executable
        .as_deref()
        .and_then(|path| path.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| input.process_binary.clone())
        .or_else(|| input.application_name.clone())
        .unwrap_or_else(|| format!("pid-{pid}"));
    AudioProcessInfo {
        pid,
        name,
        executable,
        state: if input.corked {
            SessionActivity::Inactive
        } else {
            SessionActivity::Active
        },
        device_ids: Vec::new(),
        display_name: input.application_name.clone(),
    }
}

fn os_error(reason: String) -> RpcError {
    RpcError::new(ErrorCode::OsError, reason)
}
