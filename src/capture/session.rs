//! Platform-independent half of a capture session: the DSP worker thread.
//!
//! The platform io thread creates the ring buffer once it knows the actual
//! capture format and hands the consumer over through a std channel; the
//! worker then ticks at the configured frame rate. On a starved tick it
//! zero-fills exactly one tick of silence — that single rule covers startup,
//! silence (WASAPI loopback stops delivering packets) and stalls, keeps the
//! PCM stream gapless, and lets decay smoothing animate the fall-off.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::dsp::SpectrumAnalyzer;
use crate::protocol::events::Event;
use crate::protocol::types::{AudioFormat, PcmConfig, SpectrumConfig};
use crate::rpc::writer::EventTx;
use crate::util::now_ms;
use crate::util::pcm::PcmChunker;

use super::SessionStats;

pub struct WorkerSetup {
    pub format: AudioFormat,
    pub consumer: rtrb::Consumer<f32>,
}

/// How long the worker waits for the io thread to deliver the setup before
/// giving up (io-side ready timeout is 5 s).
const SETUP_TIMEOUT: Duration = Duration::from_secs(6);

pub fn spawn_worker(
    capture_id: String,
    spectrum: SpectrumConfig,
    pcm: PcmConfig,
    events: EventTx,
    stats: Arc<SessionStats>,
    stop: Arc<AtomicBool>,
    setup_rx: std::sync::mpsc::Receiver<WorkerSetup>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("{capture_id}-worker"))
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(
                    capture_id.clone(),
                    spectrum,
                    pcm,
                    events,
                    stats,
                    stop,
                    setup_rx,
                );
            }));
            if result.is_err() {
                warn!(capture_id, "capture worker thread panicked");
            }
        })
        .expect("failed to spawn worker thread")
}

fn run(
    capture_id: String,
    spectrum: SpectrumConfig,
    pcm: PcmConfig,
    events: EventTx,
    stats: Arc<SessionStats>,
    stop: Arc<AtomicBool>,
    setup_rx: std::sync::mpsc::Receiver<WorkerSetup>,
) {
    let setup = match setup_rx.recv_timeout(SETUP_TIMEOUT) {
        Ok(s) => s,
        Err(_) => {
            debug!(
                capture_id,
                "worker exiting: io thread never delivered setup"
            );
            return;
        }
    };
    let WorkerSetup {
        format,
        mut consumer,
    } = setup;
    let channels = format.channels.max(1) as usize;

    let mut resolved = spectrum.clone();
    resolved.resolve_for_rate(format.sample_rate);
    let mut analyzer = resolved
        .enabled
        .then(|| SpectrumAnalyzer::new(&resolved, format.sample_rate, format.channels));
    let mut chunker = pcm.enabled.then(|| {
        PcmChunker::new(
            format.sample_rate,
            format.channels,
            pcm.chunk_ms,
            pcm.format,
        )
    });

    // Without spectrum the tick only drains the ring for PCM chunking.
    let fps = if resolved.enabled { resolved.fps } else { 20 };
    let period = Duration::from_secs_f64(1.0 / fps as f64);
    let period_frames = (format.sample_rate as usize / fps as usize).max(1);
    let zeros = vec![0.0_f32; period_frames * channels];

    let mut drain_buf: Vec<f32> = Vec::with_capacity(format.sample_rate as usize * channels);
    let mut spectrum_seq: u64 = 0;
    let mut next = Instant::now() + period;

    debug!(
        capture_id,
        rate = format.sample_rate,
        channels,
        fps,
        "worker running"
    );
    loop {
        // Sleep to the next deadline in small slices so stop stays responsive.
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let now = Instant::now();
            if now >= next {
                break;
            }
            std::thread::sleep((next - now).min(Duration::from_millis(100)));
        }

        drain_buf.clear();
        while let Ok(s) = consumer.pop() {
            drain_buf.push(s);
        }
        let starved = drain_buf.is_empty();
        if starved {
            stats.starved_ticks.fetch_add(1, Ordering::Relaxed);
        }
        let samples: &[f32] = if starved { &zeros } else { &drain_buf };

        if let Some(an) = analyzer.as_mut() {
            let out = an.process(samples);
            let ev = Event::CaptureSpectrum {
                capture_id: capture_id.clone(),
                seq: spectrum_seq,
                timestamp_ms: now_ms(),
                rms: out.rms,
                bands: out.bands,
            };
            spectrum_seq += 1;
            if events.try_send_frame(&ev) {
                stats.frames_emitted.fetch_add(1, Ordering::Relaxed);
            } else {
                stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }

        if let Some(ch) = chunker.as_mut() {
            if starved {
                ch.push_silence(period_frames);
            } else {
                ch.push(samples);
            }
            while let Some(chunk) = ch.next_chunk() {
                let ev = Event::CapturePcm {
                    capture_id: capture_id.clone(),
                    seq: chunk.seq,
                    timestamp_ms: now_ms(),
                    first_sample_index: chunk.first_sample_index,
                    sample_rate: format.sample_rate,
                    channels: format.channels,
                    format: pcm.format,
                    data_base64: chunk.data_base64,
                };
                if events.try_send_frame(&ev) {
                    stats.pcm_chunks_emitted.fetch_add(1, Ordering::Relaxed);
                } else {
                    stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        next += period;
        let now = Instant::now();
        if now > next + 2 * period {
            // Fell far behind (system sleep, debugger): resync instead of
            // burst-firing catch-up ticks.
            next = now + period;
        }
    }
}
