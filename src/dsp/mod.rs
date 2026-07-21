//! Spectrum pipeline: sliding window -> Hann -> real FFT -> log bands ->
//! auto-gain -> scaling -> temporal smoothing. Pure code, no OS types.

pub mod bands;
pub mod smoothing;

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

use crate::protocol::types::{BandMode, ChannelMode, ScaleMode, SpectrumConfig};
use crate::util::json::round3;

use bands::BandPlan;
use smoothing::{AgcTracker, Smoother, fps_alpha};

pub struct SpectrumOut {
    /// `bands[channel][band]`, 0..1, rounded to 3 decimals.
    pub bands: Vec<Vec<f32>>,
    /// Per-channel RMS of the samples fed this tick, 0..1.
    pub rms: Vec<f32>,
}

pub struct SpectrumAnalyzer {
    cfg: SpectrumConfig,
    src_channels: usize,
    /// Lanes actually analyzed: 1 for mono mode or mono source, else 2.
    compute_channels: usize,
    /// Lanes emitted: `cfg.channels` (mono source duplicated for stereo).
    emit_channels: usize,
    fft: Arc<dyn RealToComplex<f32>>,
    fft_size: usize,
    hann: Vec<f32>,
    windows: Vec<Vec<f32>>,
    input_scratch: Vec<f32>,
    output_scratch: Vec<Complex<f32>>,
    amps: Vec<f32>,
    plan: BandPlan,
    smoothers: Vec<Smoother>,
    agc: Option<AgcTracker>,
    fixed_gain: f32,
}

impl SpectrumAnalyzer {
    /// `cfg` must already be validated and rate-resolved.
    pub fn new(cfg: &SpectrumConfig, sample_rate: u32, src_channels: u16) -> Self {
        let src_channels = (src_channels.max(1) as usize).min(2);
        let emit_channels = match cfg.channels {
            ChannelMode::Mono => 1,
            ChannelMode::Stereo => 2,
        };
        let compute_channels = emit_channels.min(src_channels);
        let fft_size = cfg.fft_size as usize;

        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(fft_size);
        let n = fft_size as f32;
        let hann: Vec<f32> = (0..fft_size)
            .map(|i| 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / (n - 1.0)).cos()))
            .collect();

        let plan = bands::plan(
            fft_size,
            sample_rate,
            cfg.min_freq,
            cfg.max_freq,
            cfg.bands as usize,
        );
        let fps = cfg.fps as f32;
        let attack = fps_alpha(cfg.attack, fps);
        let decay = fps_alpha(cfg.decay, fps);

        Self {
            src_channels,
            compute_channels,
            emit_channels,
            fft_size,
            input_scratch: vec![0.0; fft_size],
            output_scratch: vec![Complex::default(); fft_size / 2 + 1],
            amps: vec![0.0; fft_size / 2 + 1],
            windows: vec![vec![0.0; fft_size]; compute_channels],
            smoothers: (0..compute_channels)
                .map(|_| Smoother::new(cfg.bands as usize, attack, decay))
                .collect(),
            agc: cfg.auto_gain.then(|| AgcTracker::new(fps)),
            fixed_gain: cfg.gain,
            hann,
            fft,
            plan,
            cfg: cfg.clone(),
        }
    }

    /// Feed one tick's worth of interleaved samples (zeros when starved) and
    /// produce a frame. `interleaved.len()` must be a multiple of the source
    /// channel count.
    pub fn process(&mut self, interleaved: &[f32]) -> SpectrumOut {
        let frames = interleaved.len() / self.src_channels;
        let mut rms = vec![0.0_f32; self.compute_channels];

        // De-interleave (averaging to mono when needed) into the sliding windows.
        for (lane, lane_rms) in rms.iter_mut().enumerate() {
            let mut lane_buf: Vec<f32> = Vec::with_capacity(frames);
            match (self.src_channels, self.compute_channels) {
                (1, _) => lane_buf.extend_from_slice(interleaved),
                (2, 1) => lane_buf.extend(interleaved.chunks_exact(2).map(|p| 0.5 * (p[0] + p[1]))),
                (2, 2) => lane_buf.extend(interleaved.iter().skip(lane).step_by(2)),
                _ => unreachable!("src_channels clamped to 1..=2"),
            }
            if !lane_buf.is_empty() {
                let sum_sq: f32 = lane_buf.iter().map(|s| s * s).sum();
                *lane_rms = round3((sum_sq / lane_buf.len() as f32).sqrt());
            }
            slide_in(&mut self.windows[lane], &lane_buf);
        }

        // FFT each lane and aggregate bands.
        let n_bands = self.cfg.bands as usize;
        let use_max = self.cfg.band_mode == BandMode::Max;
        let mut lane_bands: Vec<Vec<f32>> = Vec::with_capacity(self.compute_channels);
        let mut frame_max = 0.0_f32;
        for lane in 0..self.compute_channels {
            for i in 0..self.fft_size {
                self.input_scratch[i] = self.windows[lane][i] * self.hann[i];
            }
            // Only errors on length mismatch, which construction rules out.
            let _ = self
                .fft
                .process(&mut self.input_scratch, &mut self.output_scratch);
            let scale = 4.0 / self.fft_size as f32; // one-sided + Hann coherent gain
            for (a, c) in self.amps.iter_mut().zip(self.output_scratch.iter()) {
                *a = c.norm() * scale;
            }
            let vals: Vec<f32> = (0..n_bands)
                .map(|b| self.plan.value(b, &self.amps, use_max))
                .collect();
            frame_max = vals.iter().copied().fold(frame_max, f32::max);
            lane_bands.push(vals);
        }

        // Gain, scale to 0..1, smooth. Auto-gain would boost the noise floor
        // of near-silence into visible bars, so gate it: below -80 dBFS the
        // frame renders as silence and the AGC peak stays frozen.
        const AGC_GATE: f32 = 1e-4;
        let gain = match &mut self.agc {
            Some(_) if frame_max < AGC_GATE => 0.0,
            Some(agc) => agc.update(frame_max),
            None => self.fixed_gain,
        };
        for (lane, vals) in lane_bands.iter_mut().enumerate() {
            for v in vals.iter_mut() {
                let x = *v * gain;
                *v = match self.cfg.scale {
                    ScaleMode::Linear => x.clamp(0.0, 1.0),
                    ScaleMode::Sqrt => x.max(0.0).sqrt().clamp(0.0, 1.0),
                    ScaleMode::Db => {
                        let db = 20.0 * (x + 1e-10).log10();
                        ((db - self.cfg.db_floor) / -self.cfg.db_floor).clamp(0.0, 1.0)
                    }
                };
            }
            self.smoothers[lane].apply(vals);
            for v in vals.iter_mut() {
                *v = round3(*v);
            }
        }

        // Duplicate the single computed lane when stereo output was requested
        // from a mono source.
        while lane_bands.len() < self.emit_channels {
            lane_bands.push(lane_bands[0].clone());
        }
        while rms.len() < self.emit_channels {
            let first = rms[0];
            rms.push(first);
        }

        SpectrumOut {
            bands: lane_bands,
            rms,
        }
    }
}

/// Shift `new` into the fixed-size sliding window (keep the most recent).
fn slide_in(window: &mut [f32], new: &[f32]) {
    let n = window.len();
    if new.len() >= n {
        window.copy_from_slice(&new[new.len() - n..]);
    } else if !new.is_empty() {
        window.copy_within(new.len().., 0);
        window[n - new.len()..].copy_from_slice(new);
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::protocol::types::Limits;

    fn base_cfg() -> SpectrumConfig {
        let mut c = SpectrumConfig::default();
        c.scale = ScaleMode::Linear;
        c.auto_gain = false;
        c.gain = 1.0;
        c.channels = ChannelMode::Mono;
        c.validate(&Limits::default()).unwrap();
        c
    }

    fn sine(freq: f32, rate: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / rate).sin())
            .collect()
    }

    #[test]
    fn full_scale_sine_peaks_near_one_in_the_right_band() {
        let cfg = base_cfg();
        let rate = 48000;
        // Bin-centered frequency to avoid scalloping: k=43 -> 1007.8 Hz.
        let freq = 43.0 * rate as f32 / 2048.0;
        let mut an = SpectrumAnalyzer::new(&cfg, rate, 1);
        let signal = sine(freq, rate as f32, 2048 * 4);
        let mut out = None;
        for chunk in signal.chunks(2048) {
            out = Some(an.process(chunk));
        }
        let out = out.unwrap();
        assert_eq!(out.bands.len(), 1);
        let bands = &out.bands[0];
        let expected = an.plan.band_of(freq).expect("freq inside range");
        let argmax = (0..bands.len())
            .max_by(|&a, &b| bands[a].partial_cmp(&bands[b]).unwrap())
            .unwrap();
        assert_eq!(
            argmax, expected,
            "peak must land in the band containing {freq} Hz"
        );
        assert!(
            (bands[expected] - 1.0).abs() <= 0.1,
            "full-scale sine should read ~1.0, got {}",
            bands[expected]
        );
        // RMS of a full-scale sine is ~0.707.
        assert!((out.rms[0] - 0.707).abs() < 0.02);
    }

    #[test]
    fn silence_decays_below_threshold_within_a_second() {
        let cfg = base_cfg(); // 30 fps, decay 0.2
        let rate = 48000;
        let freq = 43.0 * rate as f32 / 2048.0;
        let mut an = SpectrumAnalyzer::new(&cfg, rate, 1);
        for chunk in sine(freq, rate as f32, 2048 * 3).chunks(2048) {
            an.process(chunk);
        }
        let mut peak_after = 0.0_f32;
        let tick = vec![0.0_f32; 1600]; // one 30 fps tick of silence
        let mut decreasing = true;
        let mut last = f32::MAX;
        for _ in 0..30 {
            let out = an.process(&tick);
            peak_after = out.bands[0].iter().copied().fold(0.0, f32::max);
            if peak_after > last + 1e-6 {
                decreasing = false;
            }
            last = peak_after;
        }
        assert!(decreasing, "decay must be monotonic");
        assert!(
            peak_after < 0.01,
            "should fall below 0.01 within 30 frames, got {peak_after}"
        );
    }

    #[test]
    fn stereo_out_from_mono_source_duplicates() {
        let mut cfg = base_cfg();
        cfg.channels = ChannelMode::Stereo;
        let mut an = SpectrumAnalyzer::new(&cfg, 48000, 1);
        let out = an.process(&sine(1000.0, 48000.0, 2048));
        assert_eq!(out.bands.len(), 2);
        assert_eq!(out.bands[0], out.bands[1]);
        assert_eq!(out.rms.len(), 2);
    }

    #[test]
    fn mono_mode_mixes_down_and_cancels_inverted_channels() {
        let cfg = base_cfg();
        let mut an = SpectrumAnalyzer::new(&cfg, 48000, 2);
        let l = sine(1000.0, 48000.0, 2048);
        let interleaved: Vec<f32> = l.iter().flat_map(|&s| [s, -s]).collect();
        let out = an.process(&interleaved);
        let peak = out.bands[0].iter().copied().fold(0.0, f32::max);
        assert!(
            peak < 0.01,
            "L/R cancellation should produce near-silence, got {peak}"
        );
        assert!(out.rms[0] < 0.01);
    }

    #[test]
    fn stereo_lanes_are_independent() {
        let mut cfg = base_cfg();
        cfg.channels = ChannelMode::Stereo;
        let mut an = SpectrumAnalyzer::new(&cfg, 48000, 2);
        let l = sine(43.0 * 48000.0 / 2048.0, 48000.0, 2048 * 3);
        let interleaved: Vec<f32> = l.iter().flat_map(|&s| [s, 0.0]).collect();
        let mut out = None;
        for chunk in interleaved.chunks(4096) {
            out = Some(an.process(chunk));
        }
        let out = out.unwrap();
        let peak_l = out.bands[0].iter().copied().fold(0.0, f32::max);
        let peak_r = out.bands[1].iter().copied().fold(0.0, f32::max);
        assert!(peak_l > 0.8);
        assert!(peak_r < 0.05);
    }

    #[test]
    fn db_scale_maps_full_scale_to_top() {
        let mut cfg = base_cfg();
        cfg.scale = ScaleMode::Db;
        let rate = 48000;
        let freq = 43.0 * rate as f32 / 2048.0;
        let mut an = SpectrumAnalyzer::new(&cfg, rate, 1);
        let mut out = None;
        for chunk in sine(freq, rate as f32, 2048 * 4).chunks(2048) {
            out = Some(an.process(chunk));
        }
        let bands = &out.unwrap().bands[0];
        let peak = bands.iter().copied().fold(0.0, f32::max);
        assert!(peak > 0.9, "0 dBFS should map near 1.0, got {peak}");
    }

    #[test]
    fn output_is_sanitized_and_rounded() {
        let cfg = base_cfg();
        let mut an = SpectrumAnalyzer::new(&cfg, 48000, 1);
        let out = an.process(&vec![0.0; 1600]);
        for v in &out.bands[0] {
            assert!(v.is_finite());
            let scaled = v * 1000.0;
            assert!(
                (scaled - scaled.round()).abs() < 1e-3,
                "not 3-decimal rounded: {v}"
            );
        }
    }

    #[test]
    fn auto_gain_gates_near_silence() {
        let mut cfg = base_cfg();
        cfg.auto_gain = true;
        cfg.scale = ScaleMode::Db;
        let mut an = SpectrumAnalyzer::new(&cfg, 48000, 1);
        // -100 dBFS hiss: without the gate, AGC + dB scaling would show bars.
        let hiss: Vec<f32> = sine(1000.0, 48000.0, 2048)
            .iter()
            .map(|s| s * 1e-5)
            .collect();
        let mut peak = 0.0_f32;
        for _ in 0..5 {
            let out = an.process(&hiss);
            peak = out.bands[0].iter().copied().fold(0.0, f32::max);
        }
        assert!(peak < 0.01, "near-silence must stay gated, got {peak}");
        // A real signal still gets through.
        let loud = sine(43.0 * 48000.0 / 2048.0, 48000.0, 2048 * 3);
        let mut peak = 0.0_f32;
        for chunk in loud.chunks(2048) {
            let out = an.process(chunk);
            peak = out.bands[0].iter().copied().fold(0.0, f32::max);
        }
        assert!(peak > 0.5, "signal above the gate must render, got {peak}");
    }

    #[test]
    fn window_slide_keeps_most_recent() {
        let mut w = vec![0.0; 4];
        slide_in(&mut w, &[1.0, 2.0]);
        assert_eq!(w, vec![0.0, 0.0, 1.0, 2.0]);
        slide_in(&mut w, &[3.0]);
        assert_eq!(w, vec![0.0, 1.0, 2.0, 3.0]);
        slide_in(&mut w, &[4.0, 5.0, 6.0, 7.0, 8.0]);
        assert_eq!(w, vec![5.0, 6.0, 7.0, 8.0]);
    }
}
