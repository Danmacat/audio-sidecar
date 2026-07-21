//! Float hygiene for outgoing JSON: `serde_json` writes non-finite floats as
//! `null`, which would break hosts expecting numbers — sanitize instead.

pub fn sanitize(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

/// Round to 3 decimals (short JSON) after sanitizing.
pub fn round3(x: f32) -> f32 {
    (sanitize(x) * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_non_finite() {
        assert_eq!(round3(f32::NAN), 0.0);
        assert_eq!(round3(f32::INFINITY), 0.0);
        assert_eq!(round3(f32::NEG_INFINITY), 0.0);
    }

    #[test]
    fn rounds_to_three_decimals() {
        assert_eq!(round3(0.123_456), 0.123);
        assert_eq!(round3(0.999_9), 1.0);
        assert_eq!(round3(-0.000_4), 0.0);
    }
}
