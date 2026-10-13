// SPDX-License-Identifier: Apache-2.0

//! Regression coverage for Adaptive Hybrid Transform (AHT) decoding.
//!
//! Captured E-AC-3 frame (Dolby Digital Plus 2.0 web dub, 512 bytes, bsid=16,
//! independent stream type 0, 6 audio blocks) whose audio frame header sets
//! `ahte`. It is a deliberately mixed fixture: the coupling channel and the
//! right channel carry AHT payloads (all six blocks coded in block 0 as
//! VQ / GAQ pre-mantissas), while the left channel keeps the ordinary
//! per-block mantissa syntax, and the frame also uses spectral extension.
//!
//! Before AHT support this frame was rejected outright with
//! `UnsupportedFeature("aht")`. It additionally guards two bit-position bugs
//! that AHT exposed and that would silently desync any 2/0 stream:
//!   * the block syntax walker never read the stereo rematrixing fields;
//!   * the exponent stage picked each channel's end mantissa from the
//!     frame-wide `spx_in_use` instead of the per-channel `chinspx`, parsing
//!     `chbwcod` and then discarding it.

use eac3::{PcmDecoder, inspect_access_unit};

const FIXTURE: &[u8] = include_bytes!("data/aht_independent_stereo.bin");

#[test]
fn fixture_header_declares_mixed_aht_usage() {
    assert_eq!(FIXTURE.len(), 512);
    assert_eq!(&FIXTURE[..2], &[0x0B, 0x77], "sync word");

    let info = inspect_access_unit(FIXTURE).expect("AHT frame must inspect cleanly");
    assert_eq!(info.frame_size, 512);
    assert_eq!(info.num_blocks, 6, "AHT requires a 6-block frame");
    assert_eq!(info.channel_mode, 2, "fixture is 2/0 stereo");

    let audio_frame = &info.audio_frame;
    assert!(
        audio_frame.adaptive_hybrid_transform_enabled,
        "fixture must exercise the AHT path"
    );
    assert!(
        audio_frame.coupling_uses_aht,
        "coupling channel carries an AHT payload"
    );
    assert_eq!(
        audio_frame.channel_uses_aht,
        vec![false, true],
        "left channel stays on per-block mantissas, right channel uses AHT"
    );
    assert!(!audio_frame.lfe_uses_aht, "fixture has no LFE");
}

#[test]
fn pcm_decoder_decodes_aht_frame_to_plausible_audio() {
    let mut decoder = PcmDecoder::default();
    let result = decoder
        .push_access_unit(FIXTURE)
        .expect("AHT frame must decode instead of erroring out");
    assert_eq!(result.info.frame_size, 512);
    assert_eq!(result.pcm.fullband_channels.len(), 2);

    for (index, channel) in result.pcm.fullband_channels.iter().enumerate() {
        assert_eq!(channel.len(), 6 * 256, "channel {index} sample count");
        let max_abs = channel.iter().fold(0.0_f32, |m, &s| m.max(s.abs()));
        assert!(
            max_abs.is_finite() && max_abs < 1.0,
            "channel {index} PCM must be finite and in range, got max_abs={max_abs}"
        );
        assert!(
            max_abs > 1.0e-4,
            "channel {index} must not decode to silence, got max_abs={max_abs}"
        );
    }
}

#[test]
fn aht_pre_mantissas_vary_across_the_six_blocks() {
    // AHT codes all six blocks' coefficients in block 0 and separates them
    // with a 6-point IDCT. A decoder that ran the IDCT but then reused a
    // single block's output (or zeroed blocks 1..6) would still produce
    // in-range audio, so assert the blocks actually differ.
    let mut decoder = PcmDecoder::default();
    let result = decoder
        .push_access_unit(FIXTURE)
        .expect("AHT frame must decode");

    let aht_channel = &result.pcm.fullband_channels[1];
    let block_energy: Vec<f32> = (0..6)
        .map(|block| {
            aht_channel[block * 256..(block + 1) * 256]
                .iter()
                .map(|s| s * s)
                .sum::<f32>()
        })
        .collect();

    let distinct = block_energy
        .iter()
        .filter(|energy| **energy > 0.0)
        .collect::<Vec<_>>()
        .len();
    assert!(
        distinct >= 5,
        "AHT blocks should each carry audio, got energies {block_energy:?}"
    );
    let first = block_energy[0];
    assert!(
        block_energy
            .iter()
            .any(|e| (e - first).abs() > first * 0.01),
        "AHT blocks must differ after the IDCT, got energies {block_energy:?}"
    );
}
