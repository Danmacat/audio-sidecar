//! Log-spaced band mapping from FFT bins.

/// How one output band reads from the amplitude spectrum.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Band {
    /// Aggregate over bins `lo..hi` (bin center frequencies inside the band).
    Bins { lo: usize, hi: usize },
    /// Band narrower than one bin: linear interpolation at the band center.
    Interp { base: usize, frac: f32 },
}

#[derive(Debug, Clone)]
pub struct BandPlan {
    pub bands: Vec<Band>,
    /// Geometric band edges, `n_bands + 1` entries (read by tests).
    #[allow(dead_code)]
    pub edges: Vec<f32>,
}

/// Precompute the band plan for a given FFT size / rate / range.
///
/// Edges follow `f_i = fmin * (fmax/fmin)^(i/B)`. A bin `k` has center
/// frequency `k * rate / n`; usable bins are `1..=n/2` (DC excluded).
pub fn plan(
    fft_size: usize,
    sample_rate: u32,
    min_freq: f32,
    max_freq: f32,
    n_bands: usize,
) -> BandPlan {
    let n = fft_size as f32;
    let rate = sample_rate as f32;
    let max_bin = fft_size / 2;
    let ratio = max_freq / min_freq;

    let edges: Vec<f32> = (0..=n_bands)
        .map(|i| min_freq * ratio.powf(i as f32 / n_bands as f32))
        .collect();

    let to_bin = |f: f32| -> f32 { f * n / rate };

    let bands = (0..n_bands)
        .map(|i| {
            let lo = (to_bin(edges[i]).ceil() as usize).max(1);
            let hi = (to_bin(edges[i + 1]).ceil() as usize).clamp(1, max_bin + 1);
            if lo < hi {
                Band::Bins { lo, hi }
            } else {
                // Geometric center of the band, clamped into interpolable range.
                let fc = (edges[i] * edges[i + 1]).sqrt();
                let pos = to_bin(fc).clamp(1.0, (max_bin - 1) as f32);
                let base = pos.floor() as usize;
                Band::Interp {
                    base,
                    frac: pos - base as f32,
                }
            }
        })
        .collect();

    BandPlan { bands, edges }
}

impl BandPlan {
    /// Extract one band value from the amplitude spectrum (`amps[k]`, len n/2+1).
    pub fn value(&self, band: usize, amps: &[f32], use_max: bool) -> f32 {
        match self.bands[band] {
            Band::Bins { lo, hi } => {
                let slice = &amps[lo..hi];
                if use_max {
                    slice.iter().copied().fold(0.0_f32, f32::max)
                } else {
                    let sum_sq: f32 = slice.iter().map(|a| a * a).sum();
                    (sum_sq / slice.len() as f32).sqrt()
                }
            }
            Band::Interp { base, frac } => amps[base] * (1.0 - frac) + amps[base + 1] * frac,
        }
    }

    /// Which band contains frequency `f`, if any (used by tests).
    #[allow(dead_code)]
    pub fn band_of(&self, f: f32) -> Option<usize> {
        (0..self.bands.len()).find(|&i| self.edges[i] <= f && f < self.edges[i + 1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_are_log_spaced_and_increasing() {
        let p = plan(2048, 48000, 25.0, 16000.0, 64);
        assert_eq!(p.edges.len(), 65);
        assert!((p.edges[0] - 25.0).abs() < 0.01);
        assert!((p.edges[64] - 16000.0).abs() < 1.0);
        for w in p.edges.windows(2) {
            assert!(w[1] > w[0], "edges must be strictly increasing");
        }
        // Log spacing: constant ratio between consecutive edges.
        let r0 = p.edges[1] / p.edges[0];
        for w in p.edges.windows(2) {
            let r = w[1] / w[0];
            assert!((r - r0).abs() < 1e-3);
        }
    }

    #[test]
    fn every_band_is_readable() {
        for (fft, rate, bands) in [
            (512usize, 48000u32, 64usize),
            (2048, 44100, 64),
            (8192, 96000, 256),
        ] {
            let p = plan(fft, rate, 25.0, 0.95 * rate as f32 / 2.0, bands);
            let amps = vec![1.0_f32; fft / 2 + 1];
            for i in 0..bands {
                let v = p.value(i, &amps, true);
                assert!(v > 0.0, "band {i} unreadable for fft={fft} rate={rate}");
            }
        }
    }

    #[test]
    fn low_bands_interpolate() {
        // At 48 kHz / 2048, bin spacing is 23.4 Hz; 10.6%-wide bands below
        // ~220 Hz are narrower than one bin and must interpolate.
        let p = plan(2048, 48000, 25.0, 16000.0, 64);
        assert!(matches!(p.bands[0], Band::Interp { .. }));
        assert!(matches!(p.bands[63], Band::Bins { .. }));
    }
}
