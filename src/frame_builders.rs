use bridge_api::RDecodedFrame;

#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct PcmStats {
    pub(crate) max_abs: i32,
    pub(crate) near_clip_count: usize,
}

impl PcmStats {
    pub(crate) fn from_frame(frame: &RDecodedFrame) -> Result<Self, String> {
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

        let near_clip_threshold = (i32::MAX as f32 * 0.999) as i32;
        let mut max_abs = 0i32;
        let mut near_clip_count = 0usize;
        for &sample in &frame.pcm {
            let abs = sample.saturating_abs();
            max_abs = max_abs.max(abs);
            if abs >= near_clip_threshold {
                near_clip_count += 1;
            }
        }

        if near_clip_count > expected / 16 {
            return Err(format!(
                "too_many_near_clip_samples near_clip={} total={} max_abs={}",
                near_clip_count, expected, max_abs
            ));
        }

        Ok(Self {
            max_abs,
            near_clip_count,
        })
    }
}

/// Convert a floating-point PCM sample to the bridge's i32 PCM convention
/// (24-bit signed, stored in i32, matching the TrueHD decoder's range).
pub(crate) fn float_to_pcm_i32(sample: f32) -> i32 {
    if !sample.is_finite() {
        return 0;
    }
    const SCALE: f32 = 8_388_607.0; // 2^23 - 1
    (sample.clamp(-1.0, 1.0) * SCALE) as i32
}

#[cfg(test)]
mod tests {
    use super::PcmStats;
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
            dialogue_level: None.into(),
            is_new_segment: false,
        }
    }

    #[test]
    fn pcm_stats_accepts_matching_frame() {
        let frame = decoded_frame(2, 2, vec![0, 10, -20, 30]);
        let stats = PcmStats::from_frame(&frame).expect("frame should be valid");
        assert_eq!(stats.max_abs, 30);
        assert_eq!(stats.near_clip_count, 0);
    }

    #[test]
    fn pcm_stats_rejects_length_mismatch() {
        let frame = decoded_frame(2, 2, vec![0, 10, -20]);
        let err = PcmStats::from_frame(&frame).expect_err("frame should be rejected");
        assert!(err.starts_with("pcm_len_mismatch"));
    }

    #[test]
    fn pcm_stats_rejects_many_clipped_samples() {
        let frame = decoded_frame(16, 2, vec![i32::MAX; 32]);
        let err = PcmStats::from_frame(&frame).expect_err("frame should be rejected");
        assert!(err.starts_with("too_many_near_clip_samples"));
    }
}
