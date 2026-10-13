// Hardening guards for the stateful decoders:
//
// 1. A decode error must reset the decoder's cross-frame state (IMDCT
//    overlap-add delay, coupling/SPX reuse flags, JOC differential state).
//    A failed decode leaves that state partially mutated; carrying it into
//    the next frame corrupts every frame that follows — in the field this
//    turned one bad frame after a corrupt seek into a full-scale burst.
// 2. The decoders must never panic on hostile input. They run in-process
//    inside the player, so a panic kills playback entirely. The mutation
//    sweep below is a deterministic, CI-friendly complement to the fuzz
//    targets in `fuzz/`.

use eac3::{ObjectPcmDecoder, PcmDecoder};

const FIXTURE: &[u8] = include_bytes!("data/short_packet_independent_joc.bin");

#[test]
fn pcm_decoder_error_resets_cross_frame_state() {
    let mut decoder = PcmDecoder::default();
    decoder
        .push_access_unit(FIXTURE)
        .expect("fixture must decode");
    assert!(
        !decoder.last_chinspx().is_empty(),
        "a decoded frame must leave block syntax state behind"
    );
    let frames_before = decoder.frames_seen();

    // Truncated frame: rejected with an error after the header parse.
    decoder
        .push_access_unit(&FIXTURE[..FIXTURE.len() - 64])
        .expect_err("truncated frame must be rejected");

    assert!(
        decoder.last_chinspx().is_empty(),
        "an error must reset cross-frame decode state"
    );
    assert_eq!(
        decoder.frames_seen(),
        frames_before,
        "the accepted-frame count must survive an error"
    );

    // The decoder must accept clean frames again after the error.
    decoder
        .push_access_unit(FIXTURE)
        .expect("decoder must recover after an error");
}

#[test]
fn object_decoder_recovers_after_error() {
    let mut decoder = ObjectPcmDecoder::default();
    decoder
        .push_access_unit(FIXTURE)
        .expect("fixture must decode")
        .expect("fixture carries JOC objects");

    decoder
        .push_access_unit(&FIXTURE[..FIXTURE.len() - 64])
        .expect_err("truncated frame must be rejected");

    decoder
        .push_access_unit(FIXTURE)
        .expect("decoder must recover after an error")
        .expect("fixture carries JOC objects");
}

/// Minimal deterministic PRNG (xorshift64*) so the sweep is reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

/// Feed byte-flipped and truncated variants of a real frame into every push
/// entry point. Any input may be rejected; none may panic, and the decoder
/// must still accept the clean fixture afterwards.
#[test]
fn mutated_frames_never_panic() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut pcm = PcmDecoder::default();
    let mut object = ObjectPcmDecoder::default();
    let mut legacy = PcmDecoder::default();

    for _ in 0..1500 {
        let mut frame = FIXTURE.to_vec();
        for _ in 0..=rng.below(8) {
            let index = rng.below(frame.len());
            frame[index] ^= rng.next() as u8;
        }
        // Half the runs also truncate, mimicking a corrupt seek that hands
        // the parser a partial access unit.
        if rng.below(2) == 0 {
            frame.truncate(2 + rng.below(frame.len() - 1));
        }

        let _ = pcm.push_access_unit(&frame);
        let _ = object.push_access_unit(&frame);
        let _ = legacy.push_legacy_ac3_access_unit(&frame);
    }

    pcm.push_access_unit(FIXTURE)
        .expect("PCM decoder must still work after the sweep");
    object
        .push_access_unit(FIXTURE)
        .expect("object decoder must still work after the sweep");
}
