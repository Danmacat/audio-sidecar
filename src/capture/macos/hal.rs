//! Thin helpers over the Core Audio HAL property API: typed reads, OSStatus
//! mapping, RAII property listeners and libproc path lookups. Everything here
//! is called from dedicated threads only (the HAL is not async-safe).

use std::ffi::{c_char, c_void};
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2_core_audio::AudioObjectPropertyListenerProc;
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertyScope, AudioObjectPropertySelector,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput,
};
use objc2_core_audio_types::AudioBufferList;
use objc2_core_audio_types::AudioStreamBasicDescription;
use objc2_core_foundation::CFString;

pub(crate) type CaResult<T> = Result<T, String>;

pub(crate) fn check(ctx: &str, status: i32) -> Result<(), String> {
    if status == 0 {
        return Ok(());
    }
    let be = (status as u32).to_be_bytes();
    if be.iter().all(|&b| (0x20..=0x7e).contains(&b)) {
        Err(format!(
            "{ctx}: OSStatus '{}'",
            String::from_utf8_lossy(&be)
        ))
    } else {
        Err(format!("{ctx}: OSStatus {status}"))
    }
}

pub(crate) fn addr(
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

unsafe fn read_raw(
    object: AudioObjectID,
    address: &AudioObjectPropertyAddress,
    qualifier_size: u32,
    qualifier: *const c_void,
    out_size: &mut u32,
    out: *mut c_void,
) -> CaResult<()> {
    check("AudioObjectGetPropertyData", unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(address),
            qualifier_size,
            qualifier,
            NonNull::from(out_size),
            NonNull::new(out).ok_or_else(|| "null out buffer".to_string())?,
        )
    })
}

pub(crate) unsafe fn get_u32(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> CaResult<u32> {
    let a = addr(selector, scope);
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    unsafe {
        read_raw(
            object,
            &a,
            0,
            std::ptr::null(),
            &mut size,
            &mut value as *mut u32 as *mut c_void,
        )?;
    }
    Ok(value)
}

pub(crate) unsafe fn get_cfstring(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> CaResult<String> {
    let a = addr(selector, scope);
    let mut ptr: *mut CFString = std::ptr::null_mut();
    let mut size = std::mem::size_of::<*mut CFString>() as u32;
    unsafe {
        read_raw(
            object,
            &a,
            0,
            std::ptr::null(),
            &mut size,
            &mut ptr as *mut *mut CFString as *mut c_void,
        )?;
    }
    if ptr.is_null() {
        return Ok(String::new());
    }
    let s =
        unsafe { Retained::from_raw(ptr) }.ok_or_else(|| "CFString already freed".to_string())?;
    Ok(s.to_string())
}

/// Reads a property that returns a raw C array of `AudioObjectID`s
/// (device lists, process-object lists, per-process device lists).
pub(crate) unsafe fn get_id_array(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> CaResult<Vec<AudioObjectID>> {
    let a = addr(selector, scope);
    let mut size = 0u32;
    check("AudioObjectGetPropertyDataSize", unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&a),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    })?;
    let count = size as usize / std::mem::size_of::<AudioObjectID>();
    let mut ids = vec![0u32; count];
    if count > 0 {
        unsafe {
            read_raw(
                object,
                &a,
                0,
                std::ptr::null(),
                &mut size,
                ids.as_mut_ptr() as *mut c_void,
            )?;
        }
    }
    Ok(ids)
}

pub(crate) unsafe fn get_asbd(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> CaResult<AudioStreamBasicDescription> {
    let a = addr(selector, scope);
    let mut asbd: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    unsafe {
        read_raw(
            object,
            &a,
            0,
            std::ptr::null(),
            &mut size,
            &mut asbd as *mut _ as *mut c_void,
        )?;
    }
    Ok(asbd)
}

/// Total channel count of a stream-configuration property (`AudioBufferList`,
/// variable length). Returns 0 when the direction has no streams.
pub(crate) unsafe fn get_stream_channels(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> CaResult<u32> {
    let a = addr(selector, scope);
    let mut size = 0u32;
    check("AudioObjectGetPropertyDataSize", unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&a),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    })?;
    if size < std::mem::size_of::<AudioBufferList>() as u32 {
        return Ok(0);
    }
    let mut buf = vec![0u8; size as usize];
    unsafe {
        read_raw(
            object,
            &a,
            0,
            std::ptr::null(),
            &mut size,
            buf.as_mut_ptr() as *mut c_void,
        )?;
        let list = buf.as_ptr() as *const AudioBufferList;
        let n = (*list).mNumberBuffers as usize;
        let buffers = std::slice::from_raw_parts((*list).mBuffers.as_ptr(), n);
        Ok(buffers.iter().map(|b| b.mNumberChannels).sum())
    }
}

pub(crate) fn scopes_input() -> AudioObjectPropertyScope {
    kAudioObjectPropertyScopeInput
}

pub(crate) fn scopes_output() -> AudioObjectPropertyScope {
    kAudioObjectPropertyScopeOutput
}

pub(crate) fn scope_global() -> AudioObjectPropertyScope {
    kAudioObjectPropertyScopeGlobal
}

unsafe extern "C" {
    fn sysctlbyname(
        name: *const c_char,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *mut c_void,
        newlen: usize,
    ) -> i32;
    fn proc_pidpath(pid: i32, buffer: *mut c_char, buffersize: u32) -> i32;
}

/// macOS product version, e.g. "26.6.2" (kern.osproductversion, 10.13.4+).
pub fn os_product_version() -> String {
    unsafe {
        let name = c"kern.osproductversion";
        let mut buf = [0u8; 32];
        let mut len = buf.len();
        if sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        ) == 0
            && len > 0
        {
            let end = buf[..len].iter().position(|&b| b == 0).unwrap_or(len);
            String::from_utf8_lossy(&buf[..end]).into_owned()
        } else {
            String::new()
        }
    }
}

/// Executable path of a pid via libproc; `None` when inaccessible.
pub fn process_executable(pid: u32) -> Option<String> {
    const PATH_MAX_DARWIN: usize = 4096;
    let mut buf = vec![0u8; PATH_MAX_DARWIN + 1];
    let n = unsafe {
        proc_pidpath(
            pid as i32,
            buf.as_mut_ptr() as *mut c_char,
            PATH_MAX_DARWIN as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    let end = buf[..n as usize]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(n as usize);
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// RAII HAL property listener. The callback must only enqueue messages; all
/// queries run on the owning thread when it wakes up.
pub(crate) struct PropertyListener {
    object: AudioObjectID,
    address: AudioObjectPropertyAddress,
    proc_fn: AudioObjectPropertyListenerProc,
    client_data: *mut c_void,
}

impl PropertyListener {
    /// `client_data` must point at storage that outlives the listener; the
    /// caller keeps the boxed payload alive until after `drop`.
    pub(crate) unsafe fn add(
        object: AudioObjectID,
        selector: AudioObjectPropertySelector,
        scope: AudioObjectPropertyScope,
        proc_fn: AudioObjectPropertyListenerProc,
        client_data: *mut c_void,
    ) -> CaResult<Self> {
        let address = addr(selector, scope);
        check("AudioObjectAddPropertyListener", unsafe {
            objc2_core_audio::AudioObjectAddPropertyListener(
                object,
                NonNull::from(&address),
                proc_fn,
                client_data,
            )
        })?;
        Ok(Self {
            object,
            address,
            proc_fn,
            client_data,
        })
    }
}

impl Drop for PropertyListener {
    fn drop(&mut self) {
        let _ = unsafe {
            objc2_core_audio::AudioObjectRemovePropertyListener(
                self.object,
                NonNull::from(&self.address),
                self.proc_fn,
                self.client_data,
            )
        };
    }
}

/// Selector names used by the device manager to classify callbacks.
pub(crate) const SELECTOR_DEVICES: AudioObjectPropertySelector =
    objc2_core_audio::kAudioHardwarePropertyDevices;
pub(crate) const SELECTOR_DEFAULT_OUTPUT: AudioObjectPropertySelector =
    objc2_core_audio::kAudioHardwarePropertyDefaultOutputDevice;
pub(crate) const SELECTOR_DEFAULT_INPUT: AudioObjectPropertySelector =
    objc2_core_audio::kAudioHardwarePropertyDefaultInputDevice;
pub(crate) const SELECTOR_DEFAULT_SYSTEM: AudioObjectPropertySelector =
    objc2_core_audio::kAudioHardwarePropertyDefaultSystemOutputDevice;
pub(crate) const SELECTOR_UID: AudioObjectPropertySelector =
    objc2_core_audio::kAudioDevicePropertyDeviceUID;
pub(crate) const SELECTOR_NAME: AudioObjectPropertySelector =
    objc2_core_audio::kAudioObjectPropertyName;
pub(crate) const SELECTOR_STREAM_CONFIG: AudioObjectPropertySelector =
    objc2_core_audio::kAudioDevicePropertyStreamConfiguration;
pub(crate) const SELECTOR_STREAM_FORMAT: AudioObjectPropertySelector =
    objc2_core_audio::kAudioDevicePropertyStreamFormat;
pub(crate) const SELECTOR_PROCESS_LIST: AudioObjectPropertySelector =
    objc2_core_audio::kAudioHardwarePropertyProcessObjectList;
pub(crate) const SELECTOR_PROCESS_PID: AudioObjectPropertySelector =
    objc2_core_audio::kAudioProcessPropertyPID;
pub(crate) const SELECTOR_PROCESS_BUNDLE: AudioObjectPropertySelector =
    objc2_core_audio::kAudioProcessPropertyBundleID;
pub(crate) const SELECTOR_PROCESS_DEVICES: AudioObjectPropertySelector =
    objc2_core_audio::kAudioProcessPropertyDevices;
pub(crate) const SELECTOR_PROCESS_RUNNING_OUTPUT: AudioObjectPropertySelector =
    objc2_core_audio::kAudioProcessPropertyIsRunningOutput;
