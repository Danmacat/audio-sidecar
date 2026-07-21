//! Temporal smoothing (attack/decay) and auto-gain tracking.

/// Convert a per-frame lerp factor referenced at 30 fps to the actual frame
/// rate, preserving the half-life: `a_f = 1 - (1 - a_30)^(30/fps)`.
pub fn fps_alpha(alpha_at_30: f32, fps: f32) -> f32 {
    1.0 - (1.0 - alpha_at_30).powf(30.0 / fps)
}

/// Per-band exponential smoothing with separate attack (rise) and decay (fall).
pub struct Smoother {
    attack: f32,
    decay: f32,
    state: Vec<f32>,
}

impl Smoother {
    pub fn new(n: usize, attack: f32, decay: f32) -> Self {
        Self {
            attack,
            decay,
            state: vec![0.0; n],
        }
    }

    /// Smooth `values` in place.
    pub fn apply(&mut self, values: &mut [f32]) {
        for (v, s) in values.iter_mut().zip(self.state.iter_mut()) {
            let a = if *v > *s { self.attack } else { self.decay };
            *s += a * (*v - *s);
            *v = *s;
        }
    }
}

/// Peak tracker for auto-gain: follows the recent frame maximum with a ~5 s
/// release so quiet passages get boosted and loud ones don't clip.
pub struct AgcTracker {
    peak: f32,
    release: f32, // multiplicative decay per frame: e^(-dt/5s)
}

pub const AGC_FLOOR: f32 = 0.05;
pub const AGC_CEIL: f32 = 4.0;
pub const AGC_TARGET: f32 = 0.9;

impl AgcTracker {
    pub fn new(fps: f32) -> Self {
        let dt = 1.0 / fps.max(1.0);
        Self {
            peak: AGC_FLOOR,
            release: (-dt / 5.0).exp(),
        }
    }

    /// Feed the current frame's max band amplitude; returns the gain factor.
    pub fn update(&mut self, frame_max: f32) -> f32 {
        let decayed = self.peak * self.release;
        self.peak = decayed.max(frame_max).clamp(AGC_FLOOR, AGC_CEIL);
        AGC_TARGET / self.peak
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps_alpha_preserves_half_life() {
        // n frames at 30 fps reach the same residual as 2n frames at 60 fps.
        let a30 = 0.2_f32;
        let a60 = fps_alpha(a30, 60.0);
        let residual_30 = (1.0 - a30).powi(10);
        let residual_60 = (1.0 - a60).powi(20);
        assert!((residual_30 - residual_60).abs() < 1e-4);
        // Identity at the reference rate.
        assert!((fps_alpha(a30, 30.0) - a30).abs() < 1e-6);
        // Degenerate full-strength alpha stays 1.
        assert_eq!(fps_alpha(1.0, 60.0), 1.0);
    }

    #[test]
    fn attack_is_fast_decay_is_slow() {
        let mut s = Smoother::new(1, 0.8, 0.2);
        let mut v = [1.0_f32];
        s.apply(&mut v);
        assert!((v[0] - 0.8).abs() < 1e-6);
        let mut v = [1.0_f32];
        s.apply(&mut v);
        assert!(v[0] > 0.95);
        // Now silence: decays by 20% per frame.
        let mut last = v[0];
        for _ in 0..30 {
            let mut v = [0.0_f32];
            s.apply(&mut v);
            assert!(v[0] < last);
            last = v[0];
        }
        assert!(
            last < 0.01,
            "should decay below 0.01 within 30 frames, got {last}"
        );
    }

    #[test]
    fn agc_boosts_quiet_and_caps_loud() {
        let mut agc = AgcTracker::new(30.0);
        let g_quiet = agc.update(0.01);
        assert!((g_quiet - AGC_TARGET / AGC_FLOOR).abs() < 1e-3);
        let mut agc = AgcTracker::new(30.0);
        let g_loud = agc.update(10.0);
        assert!((g_loud - AGC_TARGET / AGC_CEIL).abs() < 1e-3);
    }
}
