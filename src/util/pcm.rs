//! PCM accumulation and encoding for the optional raw stream.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

use crate::protocol::types::PcmFormat;

pub struct PcmChunk {
    pub seq: u64,
    /// Cumulative frame index of the first frame in this chunk.
    pub first_sample_index: u64,
    pub data_base64: String,
}

/// Accumulates interleaved f32 samples and emits fixed-duration chunks.
/// Chunk cadence is sample-count-driven, so no wall-clock drift accumulates.
pub struct PcmChunker {
    channels: usize,
    chunk_frames: usize,
    format: PcmFormat,
    acc: Vec<f32>,
    frames_out: u64,
    seq: u64,
}

impl PcmChunker {
    pub fn new(sample_rate: u32, channels: u16, chunk_ms: u32, format: PcmFormat) -> Self {
        let chunk_frames = ((sample_rate as u64 * chunk_ms as u64) / 1000).max(1) as usize;
        Self {
            channels: channels.max(1) as usize,
            chunk_frames,
            format,
            acc: Vec::with_capacity(chunk_frames * channels.max(1) as usize * 2),
            frames_out: 0,
            seq: 0,
        }
    }

    pub fn push(&mut self, interleaved: &[f32]) {
        self.acc.extend_from_slice(interleaved);
    }

    /// Append `frames` frames of silence (used when the capture is starved).
    pub fn push_silence(&mut self, frames: usize) {
        self.acc
            .resize(self.acc.len() + frames * self.channels, 0.0);
    }

    pub fn next_chunk(&mut self) -> Option<PcmChunk> {
        let need = self.chunk_frames * self.channels;
        if self.acc.len() < need {
            return None;
        }
        let rest = self.acc.split_off(need);
        let chunk_samples = std::mem::replace(&mut self.acc, rest);
        let data_base64 = encode(&chunk_samples, self.format);
        let chunk = PcmChunk {
            seq: self.seq,
            first_sample_index: self.frames_out,
            data_base64,
        };
        self.seq += 1;
        self.frames_out += self.chunk_frames as u64;
        Some(chunk)
    }
}

fn encode(samples: &[f32], format: PcmFormat) -> String {
    match format {
        PcmFormat::S16le => {
            let mut bytes = Vec::with_capacity(samples.len() * 2);
            for &s in samples {
                let clamped = if s.is_finite() {
                    s.clamp(-1.0, 1.0)
                } else {
                    0.0
                };
                let v = (clamped * 32767.0).round() as i16;
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            B64.encode(bytes)
        }
        PcmFormat::F32le => {
            let mut bytes = Vec::with_capacity(samples.len() * 4);
            for &s in samples {
                let v = if s.is_finite() { s } else { 0.0 };
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            B64.encode(bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s16_clamps_at_full_scale() {
        let b64 = encode(&[1.5, -1.5, 1.0, -1.0, 0.0], PcmFormat::S16le);
        let bytes = B64.decode(b64).unwrap();
        let vals: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(vals, vec![32767, -32767, 32767, -32767, 0]);
    }

    #[test]
    fn chunk_boundary_and_first_sample_index() {
        // 48 kHz stereo, 50 ms chunks -> 2400 frames = 4800 samples per chunk.
        let mut ch = PcmChunker::new(48000, 2, 50, PcmFormat::F32le);
        ch.push(&vec![0.1_f32; 4700]);
        assert!(ch.next_chunk().is_none());
        ch.push(&vec![0.1_f32; 5000]);
        let c0 = ch.next_chunk().unwrap();
        assert_eq!(c0.seq, 0);
        assert_eq!(c0.first_sample_index, 0);
        let c1 = ch.next_chunk().unwrap();
        assert_eq!(c1.seq, 1);
        assert_eq!(c1.first_sample_index, 2400);
        assert!(ch.next_chunk().is_none());
        // 4800 * 2 chunks consumed, 9700 - 9600 = 100 samples remain.
        ch.push_silence(2350); // 4700 samples -> total 4800
        let c2 = ch.next_chunk().unwrap();
        assert_eq!(c2.first_sample_index, 4800);
    }

    #[test]
    fn base64_length_matches() {
        let b64 = encode(&[0.0; 6], PcmFormat::S16le);
        let bytes = B64.decode(b64).unwrap();
        assert_eq!(bytes.len(), 12);
    }
}
