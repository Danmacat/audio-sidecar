//! Enumerate processes that currently have audio sessions on render devices
//! (the picker data for process loopback).

use std::collections::HashMap;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::{DEVICE_STATE_ACTIVE, IMMDeviceEnumerator, eRender};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::PWSTR;

use crate::protocol::types::{AudioProcessInfo, SessionActivity};
use crate::protocol::{ErrorCode, RpcError};

pub fn list(raw: &IMMDeviceEnumerator) -> Result<Vec<AudioProcessInfo>, RpcError> {
    let collection = unsafe { raw.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }
        .map_err(|e| RpcError::new(ErrorCode::OsError, e.to_string()))?;
    let count = unsafe { collection.GetCount() }
        .map_err(|e| RpcError::new(ErrorCode::OsError, e.to_string()))?;

    let mut by_pid: HashMap<u32, AudioProcessInfo> = HashMap::new();
    for i in 0..count {
        let Ok(imm) = (unsafe { collection.Item(i) }) else {
            continue;
        };
        let Ok(device) = wasapi::Device::from_immdevice(imm) else {
            continue;
        };
        let Ok(device_id) = device.get_id() else {
            continue;
        };
        let Ok(manager) = device.get_iaudiosessionmanager() else {
            continue;
        };
        let Ok(enumerator) = manager.get_audiosessionenumerator() else {
            continue;
        };
        let session_count = enumerator.get_count().unwrap_or(0);
        for j in 0..session_count {
            let Ok(session) = enumerator.get_session(j) else {
                continue;
            };
            let Ok(state) = session.get_state() else {
                continue;
            };
            if matches!(state, wasapi::SessionState::Expired) {
                continue;
            }
            let Ok(pid) = session.get_process_id() else {
                continue;
            };
            if pid == 0 {
                // The system-sounds session; not a capturable app.
                continue;
            }
            let entry = by_pid.entry(pid).or_insert_with(|| {
                let (name, executable) = process_name(pid);
                AudioProcessInfo {
                    pid,
                    name,
                    executable,
                    state: SessionActivity::Inactive,
                    device_ids: Vec::new(),
                    display_name: None,
                }
            });
            if matches!(state, wasapi::SessionState::Active) {
                entry.state = SessionActivity::Active;
            }
            if !entry.device_ids.contains(&device_id) {
                entry.device_ids.push(device_id.clone());
            }
        }
    }

    let mut processes: Vec<AudioProcessInfo> = by_pid.into_values().collect();
    processes.sort_by(|a, b| {
        (b.state == SessionActivity::Active)
            .cmp(&(a.state == SessionActivity::Active))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(processes)
}

fn process_name(pid: u32) -> (String, Option<String>) {
    let fallback = (format!("pid-{pid}"), None);
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return fallback;
        };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(handle);
        if !ok || len == 0 {
            return fallback;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let name = path.rsplit(['\\', '/']).next().unwrap_or(&path).to_string();
        (name, Some(path))
    }
}
