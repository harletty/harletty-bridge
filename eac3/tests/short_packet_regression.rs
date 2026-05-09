// Captured E-AC3 frame (Dolby Digital Plus + Atmos, 1792 bytes, bsid=16,
// independent stream type 0, 5.1 side, 6 audio blocks). ffmpeg decodes
// it without issue, and the PcmDecoder now matches FFmpeg's bit-position
// trace through the whole audblk syntax for this fixture. The chain of
// coupling parser bugs that used to make this over-read by ~600 bits/block
// (cplabsexp shift, exponent indexing, group-count formula, fast-gain
// reset, bit-alloc defaults, and the absexp read on a Reuse strategy)
// has been fixed; this test guards against any of those regressing.

use eac3::{inspect_access_unit, PcmDecoder};

const FIXTURE: &[u8] = include_bytes!("data/short_packet_independent_joc.bin");

#[test]
fn fixture_matches_expected_header() {
    assert_eq!(FIXTURE.len(), 1792);
    assert_eq!(&FIXTURE[..2], &[0x0B, 0x77], "sync word");
    let info = inspect_access_unit(FIXTURE).expect("inspect should succeed");
    assert_eq!(info.frame_size, 1792);
}

#[test]
fn pcm_decoder_decodes_fixture_without_short_packet() {
    let mut decoder = PcmDecoder::default();
    let result = decoder
        .push_access_unit(FIXTURE)
        .expect("decoder must succeed once coupling parsing is correct");
    assert_eq!(result.info.frame_size, 1792);
    let max_abs = result
        .pcm
        .fullband_channels
        .iter()
        .chain(result.pcm.lfe_channel.iter())
        .flat_map(|ch| ch.iter())
        .fold(0.0_f32, |m, &s| m.max(s.abs()));
    assert!(
        max_abs.is_finite() && max_abs < 1.0,
        "decoded PCM must be plausible, got max_abs={max_abs}"
    );
}
