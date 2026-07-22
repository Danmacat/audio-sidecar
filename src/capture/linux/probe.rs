use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use libpulse_binding as pulse;
use pulse::sample::{Format, Spec};
use pulse::stream::{FlagSet, PeekResult, State, Stream};

use super::pulse::PulseClient;

#[test]
#[ignore = "requires two live PulseAudio sink-inputs and a user session"]
fn monitor_stream_isolates_two_processes() {
    let pid_a = required_pid("AUDIO_SIDECAR_PROBE_PID_A");
    let pid_b = required_pid("AUDIO_SIDECAR_PROBE_PID_B");

    let a = capture_pid(pid_a).expect("capture 440 Hz process");
    let b = capture_pid(pid_b).expect("capture 4000 Hz process");
    let a440 = tone_power(&a, 440.0, 48_000.0);
    let a4000 = tone_power(&a, 4000.0, 48_000.0);
    let b440 = tone_power(&b, 440.0, 48_000.0);
    let b4000 = tone_power(&b, 4000.0, 48_000.0);
    eprintln!("monitor probe: A 440={a440:.5} 4000={a4000:.5}; B 440={b440:.5} 4000={b4000:.5}");
    assert!(a440 > a4000 * 10.0, "stream A leaked 4000 Hz");
    assert!(b4000 > b440 * 10.0, "stream B leaked 440 Hz");
}

fn required_pid(name: &str) -> u32 {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("set {name} to a live sink-input process PID"))
        .parse()
        .unwrap_or_else(|_| panic!("{name} must be a u32 PID"))
}

fn capture_pid(pid: u32) -> Result<Vec<f32>, String> {
    let client = PulseClient::connect("audio-sidecar-monitor-probe")?;
    let input = client
        .list_sink_inputs()?
        .into_iter()
        .filter(|input| input.pid == Some(pid))
        .min_by_key(|input| input.corked)
        .ok_or_else(|| format!("no sink-input for pid {pid}"))?;
    let monitor = client
        .list_sinks()?
        .into_iter()
        .find(|sink| sink.index == input.sink)
        .and_then(|sink| sink.monitor_source_name)
        .ok_or_else(|| format!("sink {} has no monitor source", input.sink))?;

    let spec = Spec {
        format: Format::FLOAT32NE,
        channels: 2,
        rate: 48_000,
    };
    let context = client.context();
    let stream = Rc::new(RefCell::new(
        Stream::new(
            &mut context.borrow_mut(),
            "process-monitor-probe",
            &spec,
            None,
        )
        .ok_or_else(|| "pa_stream_new failed".to_string())?,
    ));
    let (wake_tx, wake_rx) = mpsc::channel();
    stream.borrow_mut().set_state_callback(Some(Box::new({
        let wake_tx = wake_tx.clone();
        move || {
            let _ = wake_tx.send(());
        }
    })));
    stream
        .borrow_mut()
        .set_read_callback(Some(Box::new(move |_| {
            let _ = wake_tx.send(());
        })));

    client.lock();
    stream
        .borrow_mut()
        .set_monitor_stream(input.index)
        .map_err(|e| format!("set_monitor_stream({}): {e}", input.index))?;
    stream
        .borrow_mut()
        .connect_record(Some(&monitor), None, FlagSet::ADJUST_LATENCY)
        .map_err(|e| format!("connect_record({monitor}): {e}"))?;
    client.unlock();

    let ready_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        client.lock();
        let state = stream.borrow().get_state();
        client.unlock();
        match state {
            State::Ready => break,
            State::Failed | State::Terminated => {
                return Err(format!("monitor stream entered {state:?}"));
            }
            _ if Instant::now() >= ready_deadline => {
                return Err("monitor stream did not become ready".into());
            }
            _ => {
                let _ = wake_rx.recv_timeout(Duration::from_millis(20));
            }
        }
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut samples = Vec::new();
    while Instant::now() < deadline {
        let _ = wake_rx.recv_timeout(Duration::from_millis(100));
        client.lock();
        loop {
            let block = {
                let mut stream = stream.borrow_mut();
                match stream.peek().map_err(|e| format!("pa_stream_peek: {e}"))? {
                    PeekResult::Empty => None,
                    PeekResult::Hole(bytes) => Some(vec![0_u8; bytes]),
                    PeekResult::Data(bytes) => Some(bytes.to_vec()),
                }
            };
            let Some(block) = block else { break };
            for bytes in block.chunks_exact(4) {
                samples.push(f32::from_ne_bytes(bytes.try_into().expect("four bytes")));
            }
            stream
                .borrow_mut()
                .discard()
                .map_err(|e| format!("pa_stream_drop: {e}"))?;
        }
        client.unlock();
    }
    client.lock();
    {
        let mut stream = stream.borrow_mut();
        stream.set_read_callback(None);
        stream.set_state_callback(None);
        let _ = stream.disconnect();
    }
    client.unlock();
    if samples.is_empty() {
        return Err("monitor stream produced no samples".into());
    }
    Ok(samples)
}

fn tone_power(interleaved: &[f32], frequency: f64, sample_rate: f64) -> f64 {
    let mut re = 0.0;
    let mut im = 0.0;
    let mut count = 0usize;
    for (frame, sample) in interleaved.iter().step_by(2).enumerate() {
        let phase = std::f64::consts::TAU * frequency * frame as f64 / sample_rate;
        re += *sample as f64 * phase.cos();
        im -= *sample as f64 * phase.sin();
        count += 1;
    }
    (re * re + im * im).sqrt() / count.max(1) as f64
}
