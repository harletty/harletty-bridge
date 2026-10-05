use bridge_api::RDecodedFrame;

/// Check that a decoded frame is shaped the way the host reads it: a sample
/// rate, samples and channels, and exactly `sample_count * channel_count`
/// interleaved samples.
///
/// It used to scan every sample as well, rejecting a frame with more than one
/// in sixteen within 0.1 % of `i32::MAX`. Bridge PCM is 24-bit in an `i32`
/// (`float_to_pcm_i32` clamps to +/-2^23), so no sample comes within a factor
/// of 256 of that threshold: the scan could never reject anything, and it cost
/// a tenth of the realtime E-AC-3 path.
pub fn validate_frame_shape(frame: &RDecodedFrame) -> Result<(), String> {
    if frame.sample_count == 0 {
        return Err("sample_count_zero".to_string());
    }
    if frame.sampling_frequency == 0 {
        return Err("sample_rate_zero".to_string());
    }
    if frame.channel_count == 0 {
        return Err("channel_count_zero".to_string());
    }

    let expected = frame.sample_count as usize * frame.channel_count as usize;
    if frame.pcm.len() != expected {
        return Err(format!(
            "pcm_len_mismatch expected={} actual={}",
            expected,
            frame.pcm.len()
        ));
    }
    Ok(())
}

/// Convert a floating-point PCM sample to the bridge's i32 PCM convention
/// (24-bit signed, stored in i32, matching the TrueHD decoder's range).
///
/// The decoders emit `int24 as f32 / 2^23`, so this scales by 2^23 and rounds:
/// scaling by 2^23 - 1 and truncating (as this used to) shaves one count off
/// every nonzero sample and breaks bit-exactness for lossless sources. +1.0
/// lands one past the positive maximum, hence the clamp; -1.0 is exactly
/// `I24_MIN`, which the decoders do emit, so it must survive.
#[inline]
pub fn float_to_pcm_i32(sample: f32) -> i32 {
    if !sample.is_finite() {
        return 0;
    }
    const SCALE: f32 = 8_388_608.0; // 2^23
    const I24_MAX: i32 = 8_388_607;
    const I24_MIN: i32 = -8_388_608;
    let scaled = sample.clamp(-1.0, 1.0) * SCALE;
    // `round_ties_even`, which baseline x86-64 can only do through a libm
    // call per sample. |scaled| <= 2^23, so moving it 2^23 further from zero
    // lands where consecutive f32 values are exactly 1 apart: the addition
    // itself rounds to the nearest integer, ties to even, and the
    // subtraction is exact. Same result as `round_ties_even` for every f32.
    let shift = SCALE.copysign(scaled);
    (((scaled + shift) - shift) as i32).clamp(I24_MIN, I24_MAX)
}

#[cfg(test)]
mod tests {
    use super::{float_to_pcm_i32, validate_frame_shape};
    use abi_stable::std_types::RVec;
    use bridge_api::{RChannelLabel, RDecodedFrame};

    fn decoded_frame(sample_count: u32, channel_count: u32, pcm: Vec<i32>) -> RDecodedFrame {
        RDecodedFrame {
            sampling_frequency: 48_000,
            sample_count,
            channel_count,
            pcm: pcm.into(),
            channel_labels: (0..channel_count)
                .map(|_| RChannelLabel::Unknown)
                .collect::<Vec<_>>()
                .into(),
            metadata: RVec::new(),
            drc_gain: 1.0,
            drc_ramp_duration: 0,
            dialogue_level: None.into(),
            is_new_segment: false,
        }
    }

    #[test]
    fn frame_shape_accepts_matching_frame() {
        let frame = decoded_frame(2, 2, vec![0, 10, -20, 30]);
        validate_frame_shape(&frame).expect("frame should be valid");
    }

    #[test]
    fn frame_shape_rejects_length_mismatch() {
        let frame = decoded_frame(2, 2, vec![0, 10, -20]);
        let err = validate_frame_shape(&frame).expect_err("frame should be rejected");
        assert!(err.starts_with("pcm_len_mismatch"));
    }

    /// Full scale is accepted, both ends of it: nothing about a loud frame is
    /// malformed.
    #[test]
    fn frame_shape_accepts_full_scale_samples() {
        let frame = decoded_frame(16, 2, vec![8_388_607; 32]);
        validate_frame_shape(&frame).expect("frame should be valid");
        let frame = decoded_frame(16, 2, vec![-8_388_608; 32]);
        validate_frame_shape(&frame).expect("frame should be valid");
    }

    /// The output boundary must map non-finite decoder output to silence and
    /// clamp everything else to 24-bit full scale — this is the last guard
    /// between a decoder bug and the user's speakers.
    #[test]
    fn float_to_pcm_i32_guards_non_finite_and_out_of_range_samples() {
        assert_eq!(float_to_pcm_i32(f32::NAN), 0);
        assert_eq!(float_to_pcm_i32(f32::INFINITY), 0);
        assert_eq!(float_to_pcm_i32(f32::NEG_INFINITY), 0);
        assert_eq!(float_to_pcm_i32(1.0e9), 8_388_607);
        assert_eq!(float_to_pcm_i32(-1.0e9), -8_388_608);
        assert_eq!(float_to_pcm_i32(1.0), 8_388_607);
        assert_eq!(float_to_pcm_i32(-1.0), -8_388_608);
        assert_eq!(float_to_pcm_i32(0.0), 0);
        assert_eq!(float_to_pcm_i32(0.5), 4_194_304);
    }

    /// The rounding matches `round_ties_even` off the 24-bit grid too: ties,
    /// near-ties and arbitrary values at every magnitude, past both ends, and
    /// the non-finite inputs. (Checked against it over all 2^32 f32 inputs
    /// once; this keeps a strided slice of that sweep.)
    #[test]
    fn float_to_pcm_i32_rounds_like_round_ties_even() {
        let reference = |sample: f32| -> i32 {
            if !sample.is_finite() {
                return 0;
            }
            ((sample.clamp(-1.0, 1.0) * 8_388_608.0).round_ties_even() as i32)
                .clamp(-8_388_608, 8_388_607)
        };
        for n in [
            0i32, 1, 2, 3, 1000, 4_194_303, 4_194_304, 8_388_606, 8_388_607,
        ] {
            for frac in [0.5f32, 0.25, 0.75, 0.499_999_97, 0.500_000_06] {
                for sign in [1.0f32, -1.0] {
                    let x = sign * (n as f32 + frac) / 8_388_608.0;
                    assert_eq!(float_to_pcm_i32(x), reference(x), "{x:e}");
                }
            }
        }
        for bits in (0..=u32::MAX).step_by(65_537) {
            let x = f32::from_bits(bits);
            assert_eq!(float_to_pcm_i32(x), reference(x), "{x:e} ({bits:#010x})");
        }
    }

    /// The decoders divide by 2^23; this must be the exact inverse, or the
    /// realtime path stops being bit-exact on lossless sources.
    #[test]
    fn float_to_pcm_i32_round_trips_24_bit_values() {
        for n in (-8_388_608..=8_388_607).step_by(97) {
            assert_eq!(
                float_to_pcm_i32(n as f32 / 8_388_608.0),
                n,
                "round trip of {n}"
            );
        }
        for n in -4096..=4096 {
            assert_eq!(
                float_to_pcm_i32(n as f32 / 8_388_608.0),
                n,
                "round trip of {n}"
            );
        }
    }
}
