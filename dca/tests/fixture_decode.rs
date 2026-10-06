// SPDX-License-Identifier: Apache-2.0
//
// The stereo tone fixture, decoded end to end through the decoder's own
// choice of vector code. Every path gives the same samples (`src/cpu.rs`),
// so one checksum holds for all of them: CI runs this under
// `--cfg dca_force_scalar` and `--cfg dca_force_avx2` as well as on the
// runner's own instruction set, so the portable and the AVX2 code are
// tested whatever the runner's CPU.

use dca::{CorePcmFrame, Extractor, PcmDecoder};

const TONE: &[u8] = include_bytes!("../../harletty/tests/fixtures/dts_core_tone_10f.dts");

/// FNV-1a over the bits of every sample, channel by channel, frame by frame.
fn checksum(data: &[u8]) -> (usize, u64) {
    let mut extractor = Extractor::default();
    extractor.push_bytes(data);
    let mut decoder = PcmDecoder::new();
    let mut pcm = CorePcmFrame::default();
    let mut frames = 0;
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    while let Some(frame) = extractor.next_frame().expect("extract") {
        decoder
            .decode_into(frame.as_bytes(), &mut pcm)
            .expect("decode");
        frames += 1;
        for channel in pcm.fullband_channels.iter().chain(&pcm.lfe_channel) {
            for sample in channel {
                for byte in sample.to_bits().to_le_bytes() {
                    hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
        }
    }
    (frames, hash)
}

#[test]
fn tone_fixture_decodes_to_the_same_samples_on_every_path() {
    let (frames, hash) = checksum(TONE);
    assert_eq!(frames, 10);
    assert_eq!(hash, 0xe5a7_37ff_5fc2_f395, "checksum {hash:#018x}");
}
