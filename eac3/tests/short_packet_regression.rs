// Captured E-AC3 frame (Dolby Digital Plus + Atmos, 1792 bytes, bsid=16,
// independent stream type 0, 5.1 side, 6 audio blocks). ffmpeg decodes it
// without issue. Our PcmDecoder over-reads the audio block syntax by
// ~600 bits per block and runs out of bits in block 4 channel 1
// (allocation.rs decode_transform_coeffs default branch). Bug not yet
// isolated to a single field; root cause is in the audblk parsing path.
//
// When the parser bug is fixed, flip `pcm_decoder_returns_short_packet`
// to assert success.

use eac3::{inspect_access_unit, AccessUnitParseError, PcmDecoder};

const FIXTURE: &[u8] = include_bytes!("data/short_packet_independent_joc.bin");

#[test]
fn fixture_matches_expected_header() {
    assert_eq!(FIXTURE.len(), 1792);
    assert_eq!(&FIXTURE[..2], &[0x0B, 0x77], "sync word");
    let info = inspect_access_unit(FIXTURE).expect("inspect should succeed");
    assert_eq!(info.frame_size, 1792);
}

#[test]
fn pcm_decoder_returns_short_packet() {
    let mut decoder = PcmDecoder::default();
    let err = decoder
        .push_access_unit(FIXTURE)
        .expect_err("decoder must currently fail");
    match err {
        AccessUnitParseError::ShortPacket => {}
        other => panic!("expected ShortPacket, got {other:?}"),
    }
}
