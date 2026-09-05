//! ScreenCaptureKit process capture — the same pipeline OBS Studio uses
//! (see "OBS Studio macOS 应用程序音频采集源码级实现分析与开发规格"):
//! SCShareableContent → SCRunningApplication → SCContentFilter
//! (including/excludingApplications) → SCStreamConfiguration(capturesAudio)
//! → SCStream (+ a dummy screen output, an OBS trick to silence SCK errors)
//! → CMSampleBuffer/ASBD → Float32.
//!
//! Opt-in engine for A/B testing against the default Core Audio process tap:
//! set `AUDIO_SIDECAR_PROCESS_ENGINE=sck`. Differences vs the tap engine:
//! requires Screen Recording permission (TCC prompt attributed to the host
//! app on first use), targets GUI applications only (a headless CLI process
//! like `afplay` has no SCRunningApplication entry), and has no device-level
//! loopback (that stays on the tap engine).

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender as StdSender;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::{AnyThread as _, DefinedClass as _, define_class};
use objc2_core_audio_types::AudioStreamBasicDescription;
use objc2_core_media::CMAudioFormatDescriptionGetStreamBasicDescription;
use objc2_foundation::{NSArray, NSError, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCRunningApplication, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamOutput, SCStreamOutputType,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::capture::manager::ManagerCmd;
use crate::capture::session::{self, WorkerSetup};
use crate::capture::{
    CaptureSpec, ReadyInfo, SessionError, SessionStats, SessionThreads, SessionWiring,
};
use crate::protocol::RpcError;
use crate::protocol::types::AudioFormat;

use super::stream::{process_alive, tree_pids_of};

const START_TIMEOUT: Duration = Duration::from_secs(4);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(4);

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
        .name(format!("{capture_id}-sck-io"))
        .spawn(move || io_main(spec, stats, ready, stop, manager, setup_tx))
        .map_err(|e| RpcError::internal(format!("failed to spawn SCK io thread: {e}")))?;
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
        warn!(capture_id, message, "SCK capture thread panicked");
        let pending = ready_cell.lock().ok().and_then(|mut guard| guard.take());
        if let Some(reply) = pending {
            let _ = reply.send(Err(SessionError::Panic(message)));
        }
    }
    let _ = manager;
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

#[allow(clippy::too_many_arguments)]
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

    let (pid, exclude) = match &spec.source {
        crate::capture::ResolvedSource::Process { pid, exclude } => (*pid, *exclude),
        other => {
            send_ready(Err(SessionError::Activation(format!(
                "SCK engine only supports process sources, got {other:?}"
            ))));
            return;
        }
    };
    if !process_alive(pid) {
        send_ready(Err(SessionError::ProcessExited));
        return;
    }

    // 1) Shareable content (OBS: getShareableContentExcludingDesktopWindows).
    let content = match shareable_content() {
        Ok(content) => content,
        Err(err) => {
            send_ready(Err(SessionError::Activation(format!(
                "SCShareableContent: {err}"
            ))));
            return;
        }
    };
    // 2) Resolve the target pid (plus its process tree) to GUI applications.
    let tree = tree_pids_of(pid);
    let applications = unsafe { content.applications() };
    let matched: Vec<Retained<SCRunningApplication>> = unsafe {
        applications
            .iter()
            .filter(|app| tree.contains(&(app.processID() as u32)))
            .collect()
    };
    if matched.is_empty() {
        send_ready(Err(SessionError::Activation(format!(
            "pid {pid} has no ScreenCaptureKit application entry (headless CLI processes \
             cannot be captured by SCK)"
        ))));
        return;
    }
    // 3) Main display (OBS: CGMainDisplayID).
    let displays = unsafe { content.displays() };
    let main_id = unsafe { CGMainDisplayID() };
    let display = unsafe {
        displays
            .iter()
            .find(|d| d.displayID() == main_id)
            .or_else(|| displays.iter().next())
            .expect("SCK reported no displays")
    };

    let apps = NSArray::from_retained_slice(&matched);
    let filter = unsafe {
        if exclude {
            SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                SCContentFilter::alloc(),
                &display,
                &apps,
                &NSArray::from_retained_slice(&[]),
            )
        } else {
            SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                SCContentFilter::alloc(),
                &display,
                &apps,
                &NSArray::from_retained_slice(&[]),
            )
        }
    };

    // 4) Stream configuration — OBS-identical: capturesAudio, exclude self,
    //    channel count only (the sample rate comes from the first ASBD).
    let config = unsafe {
        let config = SCStreamConfiguration::new();
        config.setCapturesAudio(true);
        config.setExcludesCurrentProcessAudio(true);
        config.setChannelCount(2);
        config.setQueueDepth(8);
        config
    };

    // 5) Ring + delegate. The callback interleaves planar float into the ring
    //    and reports the format from the first buffer's ASBD.
    let ring_capacity = 48_000 * 2; // ~1 s at the SCK default 48k stereo
    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(ring_capacity);
    let (format_tx, format_rx) = std::sync::mpsc::channel::<AudioFormat>();
    let stopped = Arc::new(AtomicBool::new(false));
    let output = SckOutput::new(producer, stats.clone(), stopped.clone(), format_tx);

    // 6) Stream + outputs. The dummy screen output mirrors OBS ("add a dummy
    //    video stream output to silence errors from SCK").
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None)
    };
    unsafe {
        let proto: Retained<objc2::runtime::ProtocolObject<dyn SCStreamOutput>> =
            objc2::runtime::ProtocolObject::from_retained(output.clone());
        if let Err(err) = stream.addStreamOutput_type_sampleHandlerQueue_error(
            &proto,
            SCStreamOutputType::Audio,
            None,
        ) {
            let message = err.localizedDescription();
            send_ready(Err(SessionError::Activation(format!(
                "addStreamOutput(audio): {message}"
            ))));
            return;
        }
        if let Err(err) = stream.addStreamOutput_type_sampleHandlerQueue_error(
            &proto,
            SCStreamOutputType::Screen,
            None,
        ) {
            debug!(error = %err.localizedDescription().to_string(), "dummy screen output rejected");
        }
    }

    // 7) Start (synchronous wait on the completion handler).
    match wait_block("startCapture", |done| unsafe {
        stream.startCaptureWithCompletionHandler(Some(done));
    }) {
        Ok(()) => {}
        Err(err) => {
            send_ready(Err(SessionError::Activation(format!(
                "startCapture: {err}"
            ))));
            return;
        }
    }

    // 8) Wait for the first buffer to learn the actual format.
    let format = match format_rx.recv_timeout(FIRST_FRAME_TIMEOUT) {
        Ok(format) => format,
        Err(_) => {
            stopped.store(true, Ordering::Release);
            let _ = wait_block("stopCapture", |done| unsafe {
                stream.stopCaptureWithCompletionHandler(Some(done));
            });
            send_ready(Err(SessionError::Activation(
                "SCK stream delivered no audio buffers (permission denied? — Screen Recording \
                 must be granted to the host app)"
                    .into(),
            )));
            return;
        }
    };
    let _ = setup_tx.send(WorkerSetup { format, consumer });
    drop(setup_tx);
    send_ready(Ok(ReadyInfo {
        device_id: None,
        format,
    }));
    debug!(
        capture_id = spec.capture_id,
        pid,
        exclude,
        rate = format.sample_rate,
        channels = format.channels,
        "SCK capture running"
    );

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
        // A dead root pid is fatal (host decides); SCK itself keeps the
        // stream alive otherwise.
        if !process_alive(pid) {
            let _ = manager.send(ManagerCmd::Fatal {
                capture_id: spec.capture_id.clone(),
                error: SessionError::ProcessExited,
            });
            break;
        }
    }
    stopped.store(true, Ordering::Release);
    let _ = wait_block("stopCapture", |done| unsafe {
        stream.stopCaptureWithCompletionHandler(Some(done));
    });
    unsafe {
        let proto: Retained<objc2::runtime::ProtocolObject<dyn SCStreamOutput>> =
            objc2::runtime::ProtocolObject::from_retained(output);
        let _ = stream.removeStreamOutput_type_error(&proto, SCStreamOutputType::Audio);
        let _ = stream.removeStreamOutput_type_error(&proto, SCStreamOutputType::Screen);
    }
}

unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
}

/// Runs a `…WithCompletionHandler:` call to completion with a bounded wait.
fn wait_block(
    ctx: &str,
    call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>),
) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Option<Retained<NSError>>>(1);
    let block = block2::RcBlock::new(move |error: *mut NSError| {
        let error = (!error.is_null())
            .then(|| unsafe { Retained::retain(error) })
            .flatten();
        let _ = tx.try_send(error);
    });
    call(unsafe { &*block2::RcBlock::as_ptr(&block) });
    match rx.recv_timeout(START_TIMEOUT) {
        Ok(None) => Ok(()),
        Ok(Some(error)) => {
            let message = error.localizedDescription();
            Err(format!("{ctx}: {message}"))
        }
        Err(_) => Err(format!("{ctx}: timed out")),
    }
}

fn shareable_content() -> Result<Retained<SCShareableContent>, String> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Retained<SCShareableContent>, String>>(1);
    let block = block2::RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            if let Some(error) = (!error.is_null())
                .then(|| unsafe { Retained::retain(error) })
                .flatten()
            {
                let _ = tx.try_send(Err(error.localizedDescription().to_string()));
            } else if content.is_null() {
                let _ = tx.try_send(Err("no shareable content".into()));
            } else if let Some(content) = (!content.is_null())
                .then(|| unsafe { Retained::retain(content) })
                .flatten()
            {
                let _ = tx.try_send(Ok(content));
            } else {
                let _ = tx.try_send(Err("shareable content was null".into()));
            }
        },
    );
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false,
            false,
            &*block2::RcBlock::as_ptr(&block),
        );
    }
    rx.recv_timeout(START_TIMEOUT)
        .map_err(|_| "SCShareableContent timed out".to_string())?
}

// ---------------------------------------------------------------------------
// Delegate/output class: receives sample buffers on an SCK queue thread.
// ---------------------------------------------------------------------------

struct SckIvars {
    stopped: Arc<AtomicBool>,
    producer: RefCell<Option<rtrb::Producer<f32>>>,
    stats: Arc<SessionStats>,
    format_tx: RefCell<Option<StdSender<AudioFormat>>>,
    scratch: RefCell<Vec<f32>>,
}

define_class!(
    #[unsafe(super(objc2_foundation::NSObject))]
    #[name = "AudioSidecarSckOutput"]
    #[ivars = SckIvars]
    struct SckOutput;

    unsafe impl NSObjectProtocol for SckOutput {}

    unsafe impl SCStreamOutput for SckOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        #[allow(non_snake_case)]
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &objc2_core_media::CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Screen {
                return; // dummy output, frames dropped (OBS trick)
            }
            // Never let a panic unwind into Objective-C.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                self.handle_audio(sample_buffer);
            }));
        }
    }
);

impl SckOutput {
    fn new(
        producer: rtrb::Producer<f32>,
        stats: Arc<SessionStats>,
        stopped: Arc<AtomicBool>,
        format_tx: StdSender<AudioFormat>,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(SckIvars {
            stopped,
            producer: RefCell::new(Some(producer)),
            stats,
            format_tx: RefCell::new(Some(format_tx)),
            scratch: RefCell::new(Vec::with_capacity(8192)),
        });
        unsafe { objc2::msg_send![super(this), init] }
    }

    fn handle_audio(&self, buffer: &objc2_core_media::CMSampleBuffer) {
        if self.ivars().stopped.load(Ordering::Acquire) {
            return;
        }
        let mut producer_cell = self.ivars().producer.borrow_mut();
        let Some(producer) = producer_cell.as_mut() else {
            return;
        };
        unsafe {
            let Some(description) = buffer.format_description() else {
                return;
            };
            let description = description.as_ref();
            let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(description).as_ref();
            let Some(asbd) = asbd else {
                return;
            };
            if asbd.mSampleRate > 0.0 && asbd.mChannelsPerFrame > 0 {
                // Single mutable borrow: report once, then clear the sender.
                if let Some(tx) = self.ivars().format_tx.borrow_mut().take() {
                    let _ = tx.send(AudioFormat {
                        sample_rate: asbd.mSampleRate as u32,
                        channels: asbd.mChannelsPerFrame.clamp(1, u8::MAX as u32) as u16,
                    });
                }
            }
            let Some(data) = buffer.data_buffer() else {
                return;
            };
            let data: &objc2_core_media::CMBlockBuffer = data.as_ref();
            let length = data.data_length();
            if length == 0 {
                return;
            }
            let mut data_pointer: *mut std::ffi::c_char = std::ptr::null_mut();
            let status = data.data_pointer(
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut data_pointer,
            );
            if status != 0 || data_pointer.is_null() {
                return;
            }
            let bytes = std::slice::from_raw_parts(data_pointer as *const u8, length);
            push_samples(self, producer, asbd, bytes);
        }
    }
}

/// Pushes one buffer's bytes as interleaved f32. SCK delivers non-interleaved
/// float planar (OBS treats it as `AUDIO_FORMAT_FLOAT_PLANAR`); verify the
/// ASBD flags and interleave planar into the ring.
///
/// # Safety
/// `bytes` must hold `length` valid bytes in the layout described by `asbd`.
unsafe fn push_samples(
    output: &SckOutput,
    producer: &mut rtrb::Producer<f32>,
    asbd: &AudioStreamBasicDescription,
    bytes: &[u8],
) {
    const FLOAT: u32 = objc2_core_audio_types::kAudioFormatFlagIsFloat;
    if asbd.mFormatFlags & FLOAT == 0 {
        return; // refuse to reinterpret non-float data
    }
    let channels = asbd.mChannelsPerFrame.max(1) as usize;
    let interleaved =
        asbd.mFormatFlags & objc2_core_audio_types::kAudioFormatFlagIsNonInterleaved == 0;
    let sample_count = bytes.len() / std::mem::size_of::<f32>();
    let data = unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const f32, sample_count) };
    let mut overflow = 0u64;
    if interleaved || channels == 1 {
        for &sample in data {
            if producer.push(sample).is_err() {
                overflow += 1;
            }
        }
    } else {
        let frames = sample_count / channels;
        let mut scratch = output.ivars().scratch.borrow_mut();
        scratch.clear();
        scratch.reserve(frames * channels);
        for frame in 0..frames {
            for channel in 0..channels {
                scratch.push(data[channel * frames + frame]);
            }
        }
        for &sample in scratch.iter() {
            if producer.push(sample).is_err() {
                overflow += 1;
            }
        }
    }
    if overflow > 0 {
        output
            .ivars()
            .stats
            .ring_overflows
            .fetch_add(overflow, Ordering::Relaxed);
    }
}
