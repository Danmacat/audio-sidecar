//! The per-session io thread: owns every WASAPI object (they never cross
//! threads), reads event-driven capture packets, and feeds interleaved f32
//! samples into the ring buffer consumed by the DSP worker.

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender as StdSender;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};
use wasapi::{AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

use crate::capture::manager::ManagerCmd;
use crate::capture::session::{self, WorkerSetup};
use crate::capture::{
    CaptureSpec, ReadyInfo, ResolvedSource, SessionError, SessionStats, SessionThreads,
    SessionWiring,
};
use crate::protocol::RpcError;
use crate::protocol::types::AudioFormat;

const EVENT_WAIT_MS: u32 = 200;
const LIVENESS_INTERVAL: Duration = Duration::from_secs(1);
/// Fixed format for process loopback (there is no device mix format to query;
/// the system converts whatever the target renders).
const PROCESS_RATE: u32 = 48000;
const PROCESS_CHANNELS: u16 = 2;

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
        .name(format!("{capture_id}-io"))
        .spawn(move || io_main(spec, stats, ready, stop, manager, setup_tx))
        .map_err(|e| RpcError::internal(format!("failed to spawn io thread: {e}")))?;
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
        let msg = panic_message(&payload);
        warn!(capture_id, msg, "capture io thread panicked");
        let pending = ready_cell.lock().ok().and_then(|mut g| g.take());
        match pending {
            Some(tx) => {
                let _ = tx.send(Err(SessionError::Panic(msg)));
            }
            None => {
                let _ = manager.send(ManagerCmd::Fatal {
                    capture_id,
                    error: SessionError::Panic(msg),
                });
            }
        }
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn run_io(
    spec: &CaptureSpec,
    stats: &SessionStats,
    ready_cell: &std::sync::Mutex<Option<oneshot::Sender<Result<ReadyInfo, SessionError>>>>,
    stop: &AtomicBool,
    manager: &mpsc::UnboundedSender<ManagerCmd>,
    setup_tx: StdSender<WorkerSetup>,
) {
    let _ = wasapi::initialize_mta();
    let send_ready = |r: Result<ReadyInfo, SessionError>| {
        if let Ok(mut guard) = ready_cell.lock() {
            if let Some(tx) = guard.take() {
                let _ = tx.send(r);
            }
        }
    };

    // Endpoint activation is occasionally flaky right after another client
    // released the device (transient 0x80070002); retry briefly before
    // reporting failure.
    let mut ctx = None;
    let mut last_err = None;
    for attempt in 0..3 {
        if stop.load(Ordering::Relaxed) {
            send_ready(Err(SessionError::Activation("stopped during init".into())));
            return;
        }
        match init_capture(&spec.source) {
            Ok(c) => {
                ctx = Some(c);
                break;
            }
            Err(e @ SessionError::DeviceNotFound(_)) => {
                send_ready(Err(e));
                return;
            }
            Err(e) => {
                debug!(capture_id = spec.capture_id, attempt, error = %e, "init attempt failed");
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(150));
            }
        }
    }
    let mut ctx = match ctx {
        Some(c) => c,
        None => {
            send_ready(Err(
                last_err.unwrap_or(SessionError::Activation("init failed".into()))
            ));
            return;
        }
    };

    let ring_capacity = (ctx.format.sample_rate as usize * ctx.format.channels as usize).max(4096);
    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(ring_capacity);
    let _ = setup_tx.send(WorkerSetup {
        format: ctx.format,
        consumer,
    });
    drop(setup_tx);
    send_ready(Ok(ReadyInfo {
        device_id: ctx.device_id.clone(),
        format: ctx.format,
    }));
    debug!(
        capture_id = spec.capture_id,
        rate = ctx.format.sample_rate,
        channels = ctx.format.channels,
        "capture io running"
    );

    if let Err(e) = capture_loop(&mut ctx, producer, stop, stats) {
        if !stop.load(Ordering::Relaxed) {
            let _ = manager.send(ManagerCmd::Fatal {
                capture_id: spec.capture_id.clone(),
                error: e,
            });
        }
    }
    let _ = ctx.client.stop_stream();
}

struct IoCtx {
    /// Must stay alive for the whole capture; dropping it invalidates the rest.
    client: AudioClient,
    event: wasapi::Handle,
    capture: wasapi::AudioCaptureClient,
    format: AudioFormat,
    device_id: Option<String>,
    process: Option<ProcessHandle>,
}

fn activation(e: wasapi::WasapiError) -> SessionError {
    SessionError::Activation(e.to_string())
}

fn init_capture(source: &ResolvedSource) -> Result<IoCtx, SessionError> {
    match source {
        ResolvedSource::Device { device_id } => {
            let enumerator = DeviceEnumerator::new().map_err(activation)?;
            let device = enumerator
                .get_device(device_id)
                .map_err(|_| SessionError::DeviceNotFound(device_id.clone()))?;
            let mut client = device.get_iaudioclient().map_err(activation)?;
            let mix = client.get_mixformat().map_err(activation)?;
            let sample_rate = mix.get_samplespersec();
            let channels = mix.get_nchannels().clamp(1, 2);
            let desired = WaveFormat::new(
                32,
                32,
                &SampleType::Float,
                sample_rate as usize,
                channels as usize,
                None,
            );
            let (_default_period, min_period) = client.get_device_period().map_err(activation)?;
            // Direction::Capture on a render endpoint puts the shared-mode
            // client in loopback; on a capture endpoint it is a plain capture.
            let mode = StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: min_period,
            };
            client
                .initialize_client(&desired, &Direction::Capture, &mode)
                .map_err(activation)?;
            let event = client.set_get_eventhandle().map_err(activation)?;
            let capture = client.get_audiocaptureclient().map_err(activation)?;
            client.start_stream().map_err(activation)?;
            Ok(IoCtx {
                client,
                event,
                capture,
                format: AudioFormat {
                    sample_rate,
                    channels,
                },
                device_id: Some(device_id.clone()),
                process: None,
            })
        }
        ResolvedSource::Process { pid, exclude } => {
            // wasapi's `include_tree` flag selects between the OS's only two
            // modes: true = INCLUDE_TARGET_PROCESS_TREE (capture the process
            // and its children), false = EXCLUDE_TARGET_PROCESS_TREE (capture
            // everything else).
            let mut client = AudioClient::new_application_loopback_client(*pid, !*exclude)
                .map_err(activation)?;
            let desired = WaveFormat::new(
                32,
                32,
                &SampleType::Float,
                PROCESS_RATE as usize,
                PROCESS_CHANNELS as usize,
                None,
            );
            let mode = StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: 0,
            };
            client
                .initialize_client(&desired, &Direction::Capture, &mode)
                .map_err(activation)?;
            let event = client.set_get_eventhandle().map_err(activation)?;
            let capture = client.get_audiocaptureclient().map_err(activation)?;
            client.start_stream().map_err(activation)?;
            // Liveness only matters in include mode: an exited target means
            // permanent silence there, while exclude mode keeps capturing the
            // rest of the system just fine.
            let process = if *exclude {
                None
            } else {
                ProcessHandle::open(*pid)
            };
            Ok(IoCtx {
                client,
                event,
                capture,
                format: AudioFormat {
                    sample_rate: PROCESS_RATE,
                    channels: PROCESS_CHANNELS,
                },
                device_id: None,
                process,
            })
        }
    }
}

fn capture_loop(
    ctx: &mut IoCtx,
    mut producer: rtrb::Producer<f32>,
    stop: &AtomicBool,
    stats: &SessionStats,
) -> Result<(), SessionError> {
    let mut deque: VecDeque<u8> = VecDeque::new();
    let mut sample_bytes: Vec<u8> = Vec::new();
    let mut last_liveness = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Err covers both timeout and real failures; real failures surface
        // again from the read calls below, so just re-check the stop flag.
        let _ = ctx.event.wait_for_event(EVENT_WAIT_MS);
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        loop {
            match ctx.capture.get_next_packet_size() {
                Ok(Some(0)) | Ok(None) => break,
                Ok(Some(_)) => {
                    let before = deque.len();
                    match ctx.capture.read_from_device_to_deque(&mut deque) {
                        Ok(info) => {
                            if info.flags.silent {
                                // Buffer flagged silent: bytes are undefined,
                                // the spec says treat as zeros.
                                for b in deque.range_mut(before..) {
                                    *b = 0;
                                }
                            }
                        }
                        Err(e) => return Err(SessionError::Device(e.to_string())),
                    }
                }
                Err(e) => return Err(SessionError::Device(e.to_string())),
            }
        }

        let full = deque.len() - (deque.len() % 4);
        if full > 0 {
            sample_bytes.clear();
            sample_bytes.extend(deque.drain(..full));
            let mut dropped = 0u64;
            for c in sample_bytes.chunks_exact(4) {
                let s = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                if producer.push(s).is_err() {
                    dropped += 1;
                }
            }
            if dropped > 0 {
                stats.ring_overflows.fetch_add(dropped, Ordering::Relaxed);
            }
        }

        if let Some(ph) = &ctx.process {
            if last_liveness.elapsed() >= LIVENESS_INTERVAL {
                last_liveness = Instant::now();
                if ph.exited() {
                    return Err(SessionError::ProcessExited);
                }
            }
        }
    }
}

/// Owned process handle used to detect target exit for process loopback.
struct ProcessHandle(HANDLE);

impl ProcessHandle {
    fn open(pid: u32) -> Option<Self> {
        unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            )
            .ok()
            .map(Self)
        }
    }

    fn exited(&self) -> bool {
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
