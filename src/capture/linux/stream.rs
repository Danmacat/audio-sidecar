//! Per-session PulseAudio record thread. It owns the context and stream,
//! converts Pulse's native-endian f32 packets into the shared rtrb, and leaves
//! DSP cadence and starvation handling to `capture::session`.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender as StdSender, SyncSender};
use std::time::{Duration, Instant};

use libpulse_binding as pulse;
use pulse::def::BufferAttr;
use pulse::sample::{Format, Spec};
use pulse::stream::{FlagSet, PeekResult, State, Stream};
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

use super::pulse::PulseClient;

const STREAM_READY_TIMEOUT: Duration = Duration::from_secs(4);
const IO_WAIT: Duration = Duration::from_millis(200);
const FRAGMENT_MS: u64 = 20;

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
        .name(format!("{capture_id}-pulse-io"))
        .spawn(move || io_main(spec, stats, ready, stop, manager, setup_tx))
        .map_err(|e| RpcError::internal(format!("failed to spawn PulseAudio io thread: {e}")))?;
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
        warn!(capture_id, message, "PulseAudio capture thread panicked");
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
    stats: &SessionStats,
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

    let mut context = match init_capture(&spec.source, stop) {
        Ok(context) => context,
        Err(error) => {
            send_ready(Err(error));
            return;
        }
    };
    let ring_capacity =
        (context.format.sample_rate as usize * context.format.channels as usize).max(4096);
    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(ring_capacity);
    let _ = setup_tx.send(WorkerSetup {
        format: context.format,
        consumer,
    });
    drop(setup_tx);
    send_ready(Ok(ReadyInfo {
        device_id: Some(context.device_id.clone()),
        format: context.format,
    }));
    debug!(
        capture_id = spec.capture_id,
        device_id = context.device_id,
        rate = context.format.sample_rate,
        channels = context.format.channels,
        "PulseAudio capture running"
    );

    if let Err(error) = capture_loop(&mut context, producer, stop, stats) {
        if !stop.load(Ordering::Relaxed) {
            let _ = manager.send(ManagerCmd::Fatal {
                capture_id: spec.capture_id.clone(),
                error,
            });
        }
    }
}

struct IoContext {
    // `drop` takes this so Stream::drop runs while the mainloop is locked.
    stream: Option<Stream>,
    client: PulseClient,
    wake_rx: Receiver<()>,
    format: AudioFormat,
    device_id: String,
}

impl Drop for IoContext {
    fn drop(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            self.client.with_lock(|| {
                stream.set_read_callback(None);
                stream.set_state_callback(None);
                let _ = stream.disconnect();
                drop(stream);
            });
        }
    }
}

fn init_capture(source: &ResolvedSource, stop: &AtomicBool) -> Result<IoContext, SessionError> {
    let device_id = match source {
        ResolvedSource::Device { device_id } => device_id,
        ResolvedSource::Process { .. } => {
            return Err(SessionError::Activation(
                "process capture is not enabled".into(),
            ));
        }
    };
    let client = PulseClient::connect("audio-sidecar-capture").map_err(SessionError::Activation)?;
    let (source_name, native_spec) = resolve_device(&client, device_id)?;
    let format = AudioFormat {
        sample_rate: native_spec.rate,
        channels: u16::from(native_spec.channels.clamp(1, 2)),
    };
    let requested_spec = Spec {
        format: Format::FLOAT32NE,
        channels: format.channels as u8,
        rate: format.sample_rate,
    };
    if !requested_spec.is_valid() {
        return Err(SessionError::Activation(format!(
            "invalid PulseAudio sample spec for {device_id}"
        )));
    }

    let context = client.context();
    let mut stream = client
        .with_lock(|| {
            Stream::new(
                &mut context.borrow_mut(),
                "audio-sidecar-record",
                &requested_spec,
                None,
            )
        })
        .ok_or_else(|| SessionError::Activation("pa_stream_new failed".into()))?;
    let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel(1);
    set_callbacks(&mut stream, wake_tx);
    let bytes_per_second =
        u64::from(format.sample_rate) * u64::from(format.channels) * size_of::<f32>() as u64;
    let fragment_bytes = (bytes_per_second * FRAGMENT_MS / 1000).clamp(1, u64::from(u32::MAX));
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: fragment_bytes as u32,
    };
    client
        .with_lock(|| {
            stream.connect_record(Some(&source_name), Some(&attr), FlagSet::ADJUST_LATENCY)
        })
        .map_err(|error| {
            SessionError::Activation(format!("connect_record({source_name}): {error}"))
        })?;

    let context = IoContext {
        stream: Some(stream),
        client,
        wake_rx,
        format,
        device_id: device_id.clone(),
    };
    wait_ready(&context, stop)?;
    Ok(context)
}

fn set_callbacks(stream: &mut Stream, wake_tx: SyncSender<()>) {
    stream.set_state_callback(Some(Box::new({
        let wake_tx = wake_tx.clone();
        move || {
            let _ = wake_tx.try_send(());
        }
    })));
    stream.set_read_callback(Some(Box::new(move |_| {
        let _ = wake_tx.try_send(());
    })));
}

fn resolve_device(client: &PulseClient, device_id: &str) -> Result<(String, Spec), SessionError> {
    if let Some(name) = device_id.strip_prefix("pulse:sink:") {
        let sink = client
            .list_sinks()
            .map_err(SessionError::Activation)?
            .into_iter()
            .find(|sink| sink.name == name)
            .ok_or_else(|| SessionError::DeviceNotFound(device_id.to_string()))?;
        let monitor = sink.monitor_source_name.ok_or_else(|| {
            SessionError::Activation(format!("sink {name} has no monitor source"))
        })?;
        Ok((monitor, sink.sample_spec))
    } else if let Some(name) = device_id.strip_prefix("pulse:source:") {
        let source = client
            .list_sources()
            .map_err(SessionError::Activation)?
            .into_iter()
            .find(|source| source.name == name)
            .ok_or_else(|| SessionError::DeviceNotFound(device_id.to_string()))?;
        Ok((source.name, source.sample_spec))
    } else {
        Err(SessionError::DeviceNotFound(device_id.to_string()))
    }
}

fn wait_ready(context: &IoContext, stop: &AtomicBool) -> Result<(), SessionError> {
    let deadline = Instant::now() + STREAM_READY_TIMEOUT;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(SessionError::Activation("stopped during init".into()));
        }
        let state = context
            .client
            .with_lock(|| context.stream.as_ref().expect("stream missing").get_state());
        match state {
            State::Ready => return Ok(()),
            State::Failed | State::Terminated => {
                let error = context.client.with_context(|pulse| pulse.errno());
                return Err(SessionError::Activation(format!(
                    "PulseAudio stream entered {state:?}: {error}"
                )));
            }
            _ if Instant::now() >= deadline => {
                return Err(SessionError::Activation(
                    "timed out connecting PulseAudio stream".into(),
                ));
            }
            _ => {
                let _ = context.wake_rx.recv_timeout(Duration::from_millis(20));
            }
        }
    }
}

fn capture_loop(
    context: &mut IoContext,
    mut producer: rtrb::Producer<f32>,
    stop: &AtomicBool,
    stats: &SessionStats,
) -> Result<(), SessionError> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let _ = context.wake_rx.recv_timeout(IO_WAIT);
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let state = context
            .client
            .with_lock(|| context.stream.as_ref().expect("stream missing").get_state());
        match state {
            State::Ready => drain_packets(context, &mut producer, stats)?,
            State::Failed | State::Terminated => {
                let error = context.client.with_context(|pulse| pulse.errno());
                return Err(SessionError::Device(format!(
                    "PulseAudio stream entered {state:?}: {error}"
                )));
            }
            State::Unconnected | State::Creating => {}
        }
    }
}

fn drain_packets(
    context: &mut IoContext,
    producer: &mut rtrb::Producer<f32>,
    stats: &SessionStats,
) -> Result<(), SessionError> {
    let client = &context.client;
    let stream = context.stream.as_mut().expect("stream missing");
    client.with_lock(|| {
        loop {
            let packet = match stream.peek() {
                Ok(PeekResult::Empty) => break,
                Ok(PeekResult::Hole(bytes)) => Packet::Hole(bytes),
                Ok(PeekResult::Data(bytes)) => Packet::Data(bytes.to_vec()),
                Err(error) => {
                    return Err(SessionError::Device(format!("pa_stream_peek: {error}")));
                }
            };
            let mut dropped = 0u64;
            match packet {
                Packet::Hole(bytes) => {
                    for _ in 0..bytes / size_of::<f32>() {
                        if producer.push(0.0).is_err() {
                            dropped += 1;
                        }
                    }
                }
                Packet::Data(bytes) => {
                    for sample in bytes.chunks_exact(size_of::<f32>()) {
                        let value =
                            f32::from_ne_bytes([sample[0], sample[1], sample[2], sample[3]]);
                        if producer.push(value).is_err() {
                            dropped += 1;
                        }
                    }
                }
            }
            stream
                .discard()
                .map_err(|error| SessionError::Device(format!("pa_stream_drop: {error}")))?;
            if dropped > 0 {
                stats.ring_overflows.fetch_add(dropped, Ordering::Relaxed);
            }
        }
        Ok(())
    })
}

enum Packet {
    Hole(usize),
    Data(Vec<u8>),
}
