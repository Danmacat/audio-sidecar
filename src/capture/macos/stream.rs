//! Per-session Core Audio capture thread.
//!
//! Render devices are looped back through a private process tap scoped to the
//! device (`CATapDescription` exclude-none + deviceUID, macOS 14.4+); capture
//! devices are read with a plain HAL IOProc on the device itself. The IOProc
//! callback runs on a HAL real-time thread and only converts planar/interleaved
//! float into the shared rtrb; DSP cadence and starvation handling stay in
//! `capture::session`. Teardown order (stop flag → Stop → DestroyIOProc →
//! DestroyAggregate → DestroyTap) is enforced by RAII so an in-flight late
//! callback never touches freed state.

use std::cell::RefCell;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender as StdSender;
use std::time::Duration;

use block2::RcBlock;
use objc2::AnyThread as _;
use objc2::rc::Retained;
use objc2_core_audio::{
    AudioDeviceCreateIOProcIDWithBlock, AudioDeviceIOProcID, AudioDeviceStart,
    AudioHardwareCreateAggregateDevice, AudioHardwareCreateProcessTap,
    AudioHardwareDestroyProcessTap, AudioObjectGetPropertyData, AudioObjectID,
    AudioObjectPropertyAddress, CATapDescription, kAudioAggregateDeviceIsPrivateKey,
    kAudioAggregateDeviceIsStackedKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioHardwarePropertyTranslatePIDToProcessObject,
    kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey,
    kAudioTapPropertyFormat,
};
use objc2_core_audio::{
    AudioDeviceDestroyIOProcID, AudioDeviceStop, AudioHardwareDestroyAggregateDevice,
};
use objc2_core_audio_types::kAudioFormatFlagIsFloat;
use objc2_core_audio_types::{AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSObject, NSString};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::capture::manager::ManagerCmd;
use crate::capture::session::{self, WorkerSetup};
use crate::capture::{
    CaptureSpec, ReadyInfo, ResolvedSource, SessionError, SessionStats, SessionThreads,
    SessionWiring,
};
use crate::protocol::RpcError;
use crate::protocol::types::AudioFormat;

use super::hal::{CaResult, check, get_asbd, scope_global};

const IO_POLL: Duration = Duration::from_millis(200);
const INIT_RETRY_DELAYS_MS: [u64; 5] = [150, 250, 400, 700, 1200];
/// `kAudioHardwareIllegalOperationError` ('nope') — tap creation refused.
const ILLEGAL_OPERATION: i32 = 0x6e6f7065;
const FALLBACK_FORMAT: AudioFormat = AudioFormat {
    sample_rate: 48_000,
    channels: 2,
};

/// A fully wired capture: the chain owns the IOProc (and tap/aggregate if
/// any); `consumer` feeds the platform-independent worker.
struct ActiveCapture {
    chain: CaptureChain,
    format: AudioFormat,
    device_id: Option<String>,
    consumer: rtrb::Consumer<f32>,
    /// Process-tree captures must watch for tree changes (new children do not
    /// join an existing tap); device captures need nothing.
    process_watch: Option<(u32, bool)>,
}

pub fn spawn(spec: CaptureSpec, wiring: SessionWiring) -> Result<SessionThreads, RpcError> {
    let (setup_tx, setup_rx) = std::sync::mpsc::channel::<WorkerSetup>();
    let worker = session::spawn_worker(
        spec.capture_id.clone(),
        spec.spectrum.clone(),
        spec.pcm.clone(),
        wiring.events.clone(),
        wiring.stats.clone(),
        wiring.stop.clone(),
        setup_rx,
    );
    let SessionWiring {
        stats,
        ready,
        stop,
        manager,
        ..
    } = wiring;
    let capture_id = spec.capture_id.clone();
    let io = std::thread::Builder::new()
        .name(format!("{capture_id}-ca-io"))
        .spawn(move || io_main(spec, stats, ready, stop, manager, setup_tx))
        .map_err(|e| RpcError::internal(format!("failed to spawn Core Audio io thread: {e}")))?;
    Ok(SessionThreads { io, worker })
}

fn io_main(
    spec: CaptureSpec,
    stats: Arc<SessionStats>,
    ready: oneshot::Sender<Result<ReadyInfo, SessionError>>,
    stop: Arc<AtomicBool>,
    manager: mpsc::UnboundedSender<ManagerCmd>,
    setup_tx: StdSender<WorkerSetup>,
) {
    let capture_id = spec.capture_id.clone();
    let ready_cell = std::sync::Mutex::new(Some(ready));
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        run_io(&spec, &stats, &ready_cell, &stop, &manager, setup_tx);
    }));
    if let Err(payload) = result {
        let message = panic_message(&payload);
        warn!(capture_id, message, "Core Audio capture thread panicked");
        let pending = ready_cell.lock().ok().and_then(|mut guard| guard.take());
        match pending {
            Some(reply) => {
                let _ = reply.send(Err(SessionError::Panic(message)));
            }
            None => {
                let _ = manager.send(ManagerCmd::Fatal {
                    capture_id,
                    error: SessionError::Panic(message),
                });
            }
        }
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn run_io(
    spec: &CaptureSpec,
    stats: &Arc<SessionStats>,
    ready_cell: &std::sync::Mutex<Option<oneshot::Sender<Result<ReadyInfo, SessionError>>>>,
    stop: &AtomicBool,
    manager: &mpsc::UnboundedSender<ManagerCmd>,
    setup_tx: StdSender<WorkerSetup>,
) {
    let send_ready = |result| {
        if let Ok(mut guard) = ready_cell.lock() {
            if let Some(reply) = guard.take() {
                let _ = reply.send(result);
            }
        }
    };

    // Core Audio can reject activation while a device wakes from sleep or
    // another stream is being released; bounded ladder inside the manager's
    // ready timeout, while permanent target disappearance reports instantly.
    let mut active = None;
    let mut last_error = None;
    for attempt in 0..=INIT_RETRY_DELAYS_MS.len() {
        if stop.load(Ordering::Relaxed) {
            send_ready(Err(SessionError::Activation("stopped during init".into())));
            return;
        }
        match init_capture(&spec.source, stats) {
            Ok(value) => {
                active = Some(value);
                break;
            }
            Err(error @ SessionError::DeviceNotFound(_))
            | Err(error @ SessionError::ProcessExited) => {
                send_ready(Err(error));
                return;
            }
            Err(error) => {
                debug!(
                    capture_id = spec.capture_id,
                    attempt,
                    error = %error,
                    "Core Audio capture init attempt failed"
                );
                last_error = Some(error);
                if let Some(delay) = INIT_RETRY_DELAYS_MS.get(attempt) {
                    std::thread::sleep(Duration::from_millis(*delay));
                }
            }
        }
    }
    let Some(active) = active else {
        send_ready(Err(last_error.unwrap_or_else(|| {
            SessionError::Activation("Core Audio capture init failed".into())
        })));
        return;
    };

    let ActiveCapture {
        chain,
        format,
        device_id,
        consumer,
        process_watch,
    } = active;
    let _ = setup_tx.send(WorkerSetup { format, consumer });
    drop(setup_tx);
    send_ready(Ok(ReadyInfo {
        device_id: device_id.clone(),
        format,
    }));
    debug!(
        capture_id = spec.capture_id,
        device_id = ?device_id,
        rate = format.sample_rate,
        channels = format.channels,
        "Core Audio capture running"
    );

    // The IOProc feeds the ring from a HAL real-time thread; this loop waits
    // for stop and watches process trees for membership changes. Rebuilding
    // rides the manager restart path (probe: full rebuild ~10 ms).
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(IO_POLL);
        let Some((pid, _exclude)) = process_watch else {
            continue;
        };
        if !process_alive(pid) {
            let _ = manager.send(ManagerCmd::Fatal {
                capture_id: spec.capture_id.clone(),
                error: SessionError::ProcessExited,
            });
            break;
        }
        match tree_membership_changed(pid, &chain.members) {
            Ok(Some(_)) => {
                let _ = manager.send(ManagerCmd::Fatal {
                    capture_id: spec.capture_id.clone(),
                    error: SessionError::Device(
                        "target process tree changed; rebuilding tap".into(),
                    ),
                });
                break;
            }
            Ok(None) => {}
            Err(SessionError::ProcessExited) => {
                let _ = manager.send(ManagerCmd::Fatal {
                    capture_id: spec.capture_id.clone(),
                    error: SessionError::ProcessExited,
                });
                break;
            }
            Err(_) => {}
        }
    }
    drop(chain);
}

// ---------------------------------------------------------------------------
// Target resolution and chain construction
// ---------------------------------------------------------------------------

enum Target {
    /// Everything routed to this output device (loopback).
    DeviceLoopback {
        uid: String,
        wire: String,
    },
    /// Microphone-style capture from an input device.
    DeviceInput {
        uid: String,
        wire: String,
    },
    Process {
        pid: u32,
        exclude: bool,
    },
}

fn parse_target(source: &ResolvedSource) -> Result<Target, SessionError> {
    match source {
        ResolvedSource::Device { device_id } => {
            if let Some(uid) = device_id.strip_prefix("ca:out:") {
                Ok(Target::DeviceLoopback {
                    uid: uid.to_string(),
                    wire: device_id.clone(),
                })
            } else if let Some(uid) = device_id.strip_prefix("ca:in:") {
                Ok(Target::DeviceInput {
                    uid: uid.to_string(),
                    wire: device_id.clone(),
                })
            } else {
                Err(SessionError::DeviceNotFound(device_id.clone()))
            }
        }
        ResolvedSource::Process { pid, exclude } => Ok(Target::Process {
            pid: *pid,
            exclude: *exclude,
        }),
    }
}

fn init_capture(
    source: &ResolvedSource,
    stats: &Arc<SessionStats>,
) -> Result<ActiveCapture, SessionError> {
    match parse_target(source)? {
        Target::DeviceLoopback { uid, wire } => {
            // Presence check so an unknown UID reports deviceNotFound, not a
            // tap creation failure; the tap itself addresses the device by UID.
            translate_uid(&uid).map_err(|_| SessionError::DeviceNotFound(wire.clone()))?;
            let (chain, format, consumer) = build_tap(
                TapKind::DeviceLoopback { uid: &uid },
                format!("audio-sidecar-{wire}"),
                stats,
            )?;
            Ok(ActiveCapture {
                chain,
                format,
                device_id: Some(wire),
                consumer,
                process_watch: None,
            })
        }
        Target::DeviceInput { uid, wire } => {
            let device =
                translate_uid(&uid).map_err(|_| SessionError::DeviceNotFound(wire.clone()))?;
            let format = input_format(device).ok_or_else(|| {
                SessionError::Activation(format!("input device {wire} has no input stream format"))
            })?;
            let (chain, consumer) = build_input_capture(device, format, stats)?;
            Ok(ActiveCapture {
                chain,
                format,
                device_id: Some(wire),
                consumer,
                process_watch: None,
            })
        }
        Target::Process { pid, exclude } => {
            let (chain, format, consumer) = build_tap(
                TapKind::ProcessTree { pid, exclude },
                format!("audio-sidecar-pid{pid}"),
                stats,
            )?;
            Ok(ActiveCapture {
                chain,
                format,
                device_id: None,
                consumer,
                process_watch: Some((pid, exclude)),
            })
        }
    }
}

unsafe fn system_object() -> AudioObjectID {
    objc2_core_audio::kAudioObjectSystemObject as AudioObjectID
}

fn translate_uid(uid: &str) -> CaResult<AudioObjectID> {
    unsafe {
        let address = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyTranslateUIDToDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut cf_uid = Retained::into_raw(NSString::from_str(uid));
        let mut device: AudioObjectID = 0;
        let mut size = std::mem::size_of::<AudioObjectID>() as u32;
        let status = AudioObjectGetPropertyData(
            system_object(),
            NonNull::from(&address),
            std::mem::size_of::<*mut NSString>() as u32,
            NonNull::from(&mut cf_uid).cast::<c_void>().as_ptr(),
            NonNull::from(&mut size),
            NonNull::new(&mut device as *mut AudioObjectID as *mut c_void).ok_or("null")?,
        );
        drop(Retained::from_raw(cf_uid));
        check("TranslateUIDToDevice", status)?;
        if device == 0 {
            return Err("unknown device UID".into());
        }
        Ok(device)
    }
}

fn input_format(device: AudioObjectID) -> Option<AudioFormat> {
    let asbd = unsafe {
        get_asbd(
            device,
            super::hal::SELECTOR_STREAM_FORMAT,
            super::hal::scopes_input(),
        )
    }
    .ok()?;
    asbd_to_format(&asbd)
}

fn asbd_to_format(asbd: &AudioStreamBasicDescription) -> Option<AudioFormat> {
    if !(1000.0..=768_000.0).contains(&asbd.mSampleRate) || asbd.mChannelsPerFrame == 0 {
        return None;
    }
    Some(AudioFormat {
        sample_rate: asbd.mSampleRate as u32,
        channels: asbd.mChannelsPerFrame.clamp(1, u8::MAX as u32) as u16,
    })
}

enum TapKind<'a> {
    DeviceLoopback { uid: &'a str },
    ProcessTree { pid: u32, exclude: bool },
}

fn map_tap_error(ctx: &str, status: i32) -> SessionError {
    if status == ILLEGAL_OPERATION {
        SessionError::Activation(format!("{ctx}: refused ('nope') — permission or policy"))
    } else {
        let be = (status as u32).to_be_bytes();
        if be.iter().all(|&b| (0x20..=0x7e).contains(&b)) {
            SessionError::Activation(format!(
                "{ctx}: OSStatus '{}'",
                String::from_utf8_lossy(&be)
            ))
        } else {
            SessionError::Activation(format!("{ctx}: OSStatus {status}"))
        }
    }
}

fn build_tap(
    kind: TapKind<'_>,
    name: String,
    stats: &Arc<SessionStats>,
) -> Result<(CaptureChain, AudioFormat, rtrb::Consumer<f32>), SessionError> {
    let mut members = Vec::new();
    if let TapKind::ProcessTree { pid, exclude } = &kind {
        members = current_tree_members(*pid);
        if !*exclude && members.is_empty() {
            return Err(SessionError::ProcessExited);
        }
    }
    let desc: Retained<CATapDescription> = unsafe {
        let d = match kind {
            TapKind::DeviceLoopback { uid } => {
                CATapDescription::initExcludingProcesses_andDeviceUID_withStream(
                    CATapDescription::alloc(),
                    &NSArray::from_retained_slice(&[]),
                    &NSString::from_str(uid),
                    0,
                )
            }
            TapKind::ProcessTree { exclude, .. } => {
                let arr = NSArray::from_retained_slice(
                    &members
                        .iter()
                        .map(|o| NSNumber::numberWithUnsignedInt(*o))
                        .collect::<Vec<_>>(),
                );
                if exclude {
                    CATapDescription::initStereoGlobalTapButExcludeProcesses(
                        CATapDescription::alloc(),
                        &arr,
                    )
                } else {
                    CATapDescription::initStereoMixdownOfProcesses(CATapDescription::alloc(), &arr)
                }
            }
        };
        d.setPrivate(true);
        d.setName(&NSString::from_str(&name));
        // macOS 26 added bundle-ID auto-restore for tapped processes; the
        // selector does not exist on older systems (unrecognized selector
        // aborts), so only touch it there.
        if os_major_at_least(26) {
            d.setProcessRestoreEnabled(true);
        }
        d
    };
    let tap_uuid: Retained<NSString> = unsafe { desc.UUID().UUIDString() };

    let mut tap_id: AudioObjectID = 0;
    unsafe {
        let status = AudioHardwareCreateProcessTap(Some(&desc), &mut tap_id);
        if status != 0 {
            return Err(map_tap_error("AudioHardwareCreateProcessTap", status));
        }
    }
    let tap_asbd = unsafe { get_asbd(tap_id, kAudioTapPropertyFormat, scope_global()).ok() };
    if let Some(asbd) = &tap_asbd {
        // The IOProc reads mData as f32; refuse a non-float tap rather than
        // reinterpret garbage.
        if asbd.mFormatFlags & kAudioFormatFlagIsFloat == 0 {
            unsafe { AudioHardwareDestroyProcessTap(tap_id) };
            return Err(SessionError::Activation("tap format is not float".into()));
        }
    }
    let format = tap_asbd
        .as_ref()
        .and_then(asbd_to_format)
        .unwrap_or(FALLBACK_FORMAT);

    let aggregate = create_aggregate(&name, &tap_uuid)
        .map_err(|e| SessionError::Activation(format!("aggregate device: {e}")))?;
    let (chain, consumer) =
        attach_ioproc(aggregate, Some(tap_id), Some(desc), format, stats, members)
            .map_err(SessionError::Activation)?;
    Ok((chain, format, consumer))
}

fn build_input_capture(
    device: AudioObjectID,
    format: AudioFormat,
    stats: &Arc<SessionStats>,
) -> Result<(CaptureChain, rtrb::Consumer<f32>), SessionError> {
    attach_ioproc(device, None, None, format, stats, Vec::new()).map_err(SessionError::Activation)
}

fn create_aggregate(name: &str, tap_uuid: &NSString) -> CaResult<AudioObjectID> {
    unsafe {
        let sub_tap: Retained<NSDictionary<NSString, NSObject>> =
            NSDictionary::from_slices::<NSString>(
                &[
                    key_ns(kAudioSubTapUIDKey).as_ref(),
                    key_ns(kAudioSubTapDriftCompensationKey).as_ref(),
                ],
                &[
                    tap_uuid.as_ref() as &NSObject,
                    NSNumber::numberWithBool(true).as_ref() as &NSObject,
                ],
            );
        let tap_list = NSArray::from_retained_slice(&[Retained::into_super(sub_tap)]);
        let keys = [
            key_ns(kAudioAggregateDeviceNameKey),
            key_ns(kAudioAggregateDeviceUIDKey),
            key_ns(kAudioAggregateDeviceIsPrivateKey),
            key_ns(kAudioAggregateDeviceIsStackedKey),
            key_ns(kAudioAggregateDeviceTapAutoStartKey),
            key_ns(kAudioAggregateDeviceTapListKey),
        ];
        let values: [Retained<NSObject>; 6] = [
            Retained::into_super(NSString::from_str(name)),
            Retained::into_super(NSString::from_str(&format!(
                "audio-sidecar-agg-{}",
                agg_counter_next()
            ))),
            bool_ns(true),
            bool_ns(false),
            bool_ns(true),
            Retained::into_super(tap_list),
        ];
        let dict: Retained<NSDictionary<NSString, NSObject>> = NSDictionary::from_slices::<NSString>(
            &[
                keys[0].as_ref(),
                keys[1].as_ref(),
                keys[2].as_ref(),
                keys[3].as_ref(),
                keys[4].as_ref(),
                keys[5].as_ref(),
            ],
            &[
                values[0].as_ref(),
                values[1].as_ref(),
                values[2].as_ref(),
                values[3].as_ref(),
                values[4].as_ref(),
                values[5].as_ref(),
            ],
        );
        let cf: &CFDictionary = &*(Retained::as_ptr(&dict) as *const CFDictionary);
        let mut device_id: AudioObjectID = 0;
        check(
            "AudioHardwareCreateAggregateDevice",
            AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut device_id)),
        )?;
        Ok(device_id)
    }
}

fn bool_ns(value: bool) -> Retained<NSObject> {
    // NSNumber's declared superclass is NSValue; cast straight to NSObject.
    #[allow(deprecated)] // upcast to NSObject; `cast` is exactly that
    {
        unsafe { Retained::cast::<NSObject>(NSNumber::numberWithBool(value)) }
    }
}

fn key_ns(key: &std::ffi::CStr) -> Retained<NSString> {
    NSString::from_str(key.to_str().unwrap_or(""))
}

fn agg_counter_next() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Owns a running IOProc plus whatever backs it (tap + aggregate). `drop`
/// stops everything in reverse order; late callbacks see `stopped` first.
struct CaptureChain {
    device: AudioObjectID,
    io_proc: AudioDeviceIOProcID,
    tap_id: Option<AudioObjectID>,
    aggregate: Option<AudioObjectID>,
    stopped: Arc<AtomicBool>,
    /// Process-object ids currently covered by the tap (process captures).
    members: Vec<AudioObjectID>,
    _block: RcBlock<
        dyn Fn(
            NonNull<AudioTimeStamp>,
            NonNull<AudioBufferList>,
            NonNull<AudioTimeStamp>,
            NonNull<AudioBufferList>,
            NonNull<AudioTimeStamp>,
        ),
    >,
    _desc: Option<Retained<CATapDescription>>,
}

impl Drop for CaptureChain {
    fn drop(&mut self) {
        // Fence before Stop so an in-flight late callback bails out instead of
        // touching state that is about to be freed.
        self.stopped.store(true, Ordering::Release);
        unsafe {
            if !self.io_proc.is_none() {
                let _ = AudioDeviceStop(self.device, self.io_proc);
                let _ = AudioDeviceDestroyIOProcID(self.device, self.io_proc);
            }
            if let Some(aggregate) = self.aggregate {
                let _ = AudioHardwareDestroyAggregateDevice(aggregate);
            }
            if let Some(tap) = self.tap_id {
                let _ = AudioHardwareDestroyProcessTap(tap);
            }
        }
    }
}

fn attach_ioproc(
    device: AudioObjectID,
    tap_id: Option<AudioObjectID>,
    desc: Option<Retained<CATapDescription>>,
    format: AudioFormat,
    stats: &Arc<SessionStats>,
    members: Vec<AudioObjectID>,
) -> CaResult<(CaptureChain, rtrb::Consumer<f32>)> {
    let capacity = (format.sample_rate as usize * format.channels as usize).max(4096);
    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(capacity);

    let stopped = Arc::new(AtomicBool::new(false));
    let stopped_cb = stopped.clone();
    let stats_cb = stats.clone();
    // Planar input is interleaved through a reusable scratch; sized once here
    // so the real-time callback never allocates.
    let scratch = RefCell::new(Vec::with_capacity(4096));
    let sink = RefCell::new(producer);

    let block = RcBlock::new(
        move |_now: NonNull<AudioTimeStamp>,
              input: NonNull<AudioBufferList>,
              _in_time: NonNull<AudioTimeStamp>,
              _out: NonNull<AudioBufferList>,
              _out_time: NonNull<AudioTimeStamp>| {
            // Never let a panic cross the FFI boundary into Core Audio.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                if stopped_cb.load(Ordering::Acquire) {
                    return;
                }
                let Ok(mut sink) = sink.try_borrow_mut() else {
                    return;
                };
                // SAFETY: HAL supplies a valid AudioBufferList for the input.
                unsafe { push_input(&mut sink, &scratch, &stats_cb, input.as_ptr()) };
            }));
        },
    );

    let mut io_proc: AudioDeviceIOProcID = None;
    unsafe {
        let status = AudioDeviceCreateIOProcIDWithBlock(
            NonNull::from(&mut io_proc),
            device,
            None,
            RcBlock::as_ptr(&block).cast(),
        );
        check("AudioDeviceCreateIOProcIDWithBlock", status)?;
        if io_proc.is_none() {
            return Err("no IOProc id returned".into());
        }
        check("AudioDeviceStart", AudioDeviceStart(device, io_proc))?;
    }

    Ok((
        CaptureChain {
            device,
            io_proc,
            tap_id,
            aggregate: if tap_id.is_some() { Some(device) } else { None },
            stopped,
            members,
            _block: block,
            _desc: desc,
        },
        consumer,
    ))
}

fn os_major_at_least(major: u32) -> bool {
    super::hal::os_product_version()
        .split('.')
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .is_some_and(|v| v >= major)
}

/// Pushes one IOProc input buffer (interleaved or planar float) into the
/// ring as interleaved samples, counting overflowed samples.
///
/// # Safety
/// `list` must point at the HAL-provided `AudioBufferList`.
unsafe fn push_input(
    sink: &mut rtrb::Producer<f32>,
    scratch: &RefCell<Vec<f32>>,
    stats: &SessionStats,
    list: *const AudioBufferList,
) {
    unsafe {
        if list.is_null() {
            return;
        }
        let count = (*list).mNumberBuffers as usize;
        if count == 0 {
            return;
        }
        let buffers = std::slice::from_raw_parts((*list).mBuffers.as_ptr(), count);
        let mut overflow = 0u64;
        if count == 1 {
            let buffer = &buffers[0];
            let samples = buffer.mDataByteSize as usize / std::mem::size_of::<f32>();
            if buffer.mData.is_null() || samples == 0 {
                return;
            }
            let data = std::slice::from_raw_parts(buffer.mData as *const f32, samples);
            for &sample in data {
                if sink.push(sample).is_err() {
                    overflow += 1;
                }
            }
        } else {
            let frames = buffers
                .iter()
                .map(|b| b.mDataByteSize as usize / std::mem::size_of::<f32>())
                .min()
                .unwrap_or(0);
            if frames == 0 {
                return;
            }
            let mut scratch = scratch.borrow_mut();
            scratch.clear();
            scratch.reserve(frames * count);
            for frame in 0..frames {
                for buffer in buffers {
                    if buffer.mData.is_null() {
                        continue;
                    }
                    let sample = *(buffer.mData as *const f32).add(frame);
                    scratch.push(sample);
                }
            }
            for &sample in scratch.iter() {
                if sink.push(sample).is_err() {
                    overflow += 1;
                }
            }
        }
        if overflow > 0 {
            stats.ring_overflows.fetch_add(overflow, Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// Process-tree enumeration (libproc; layouts from sys/proc_info.h)
// ---------------------------------------------------------------------------

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn proc_listpids(kind: u32, typeinfo: u32, buffer: *mut c_void, buffersize: i32) -> i32;
    fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut c_void, buffersize: i32) -> i32;
}

const PROC_ALL_PIDS: u32 = 1;
const PROC_PIDTBSDINFO: i32 = 3;
const PROC_PIDTBSDINFO_SIZE: i32 = std::mem::size_of::<[u32; 34]>() as i32;
const PBI_PID_OFF: usize = 12;
const PBI_PPID_OFF: usize = 16;

fn process_alive(pid: u32) -> bool {
    let result = unsafe { kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(1) // EPERM: exists
}

/// pid → AudioObjectID of its Core Audio process object (0 when absent).
fn pid_to_object(pid: u32) -> CaResult<AudioObjectID> {
    unsafe {
        let address = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyTranslatePIDToProcessObject,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut pid_val = pid as i32;
        let mut object: AudioObjectID = 0;
        let mut size = std::mem::size_of::<AudioObjectID>() as u32;
        check(
            "TranslatePIDToProcessObject",
            AudioObjectGetPropertyData(
                system_object(),
                NonNull::from(&address),
                std::mem::size_of::<i32>() as u32,
                (&mut pid_val as *mut i32).cast_const().cast(),
                NonNull::from(&mut size),
                NonNull::new(&mut object as *mut AudioObjectID as *mut c_void).ok_or("null out")?,
            ),
        )?;
        Ok(object)
    }
}

fn parent_map() -> std::collections::HashMap<u32, u32> {
    use std::collections::HashMap;
    let mut map = HashMap::new();
    unsafe {
        let needed = proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0);
        if needed <= 0 {
            return map;
        }
        let mut pids = vec![0i32; needed as usize];
        let got = proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr() as *mut c_void,
            (pids.len() * std::mem::size_of::<i32>()) as i32,
        );
        if got <= 0 {
            return map;
        }
        pids.truncate(got as usize);
        let mut info = [0u8; PROC_PIDTBSDINFO_SIZE as usize];
        for &pid in &pids {
            if pid <= 0 {
                continue;
            }
            let ok = proc_pidinfo(
                pid,
                PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr() as *mut c_void,
                PROC_PIDTBSDINFO_SIZE,
            );
            if ok as usize >= PBI_PPID_OFF + 4 {
                let self_pid =
                    i32::from_ne_bytes(info[PBI_PID_OFF..PBI_PID_OFF + 4].try_into().unwrap());
                let ppid =
                    i32::from_ne_bytes(info[PBI_PPID_OFF..PBI_PPID_OFF + 4].try_into().unwrap());
                if self_pid == pid {
                    map.insert(self_pid as u32, ppid as u32);
                }
            }
        }
    }
    map
}

fn tree_pids(root: u32, parents: &std::collections::HashMap<u32, u32>) -> Vec<u32> {
    let mut tree = vec![root];
    let mut index = 0;
    while index < tree.len() {
        let current = tree[index];
        index += 1;
        for (&pid, &ppid) in parents.iter() {
            if ppid == current && !tree.contains(&pid) {
                tree.push(pid);
            }
        }
    }
    tree
}

fn current_tree_members(root: u32) -> Vec<AudioObjectID> {
    let parents = parent_map();
    tree_pids(root, &parents)
        .into_iter()
        .filter(|pid| process_alive(*pid))
        .filter_map(|pid| pid_to_object(pid).ok().filter(|o| *o != 0))
        .collect()
}

/// Ok(Some(members)) when membership differs from `previous`.
fn tree_membership_changed(
    pid: u32,
    previous: &[AudioObjectID],
) -> Result<Option<Vec<AudioObjectID>>, SessionError> {
    if !process_alive(pid) {
        return Err(SessionError::ProcessExited);
    }
    let members = current_tree_members(pid);
    if members.len() == previous.len() && members.iter().zip(previous.iter()).all(|(a, b)| a == b) {
        Ok(None)
    } else {
        Ok(Some(members))
    }
}
