// SPDX-License-Identifier: Apache-2.0

//! Splice detection for the object decoder.
//!
//! ETSI TS 103 420 clause 6.3.3.3 gives `joc_sequence_counter` one job - "The
//! frame sequence counter is used for splice detection in the decoder" - and
//! this decoder used to parse it and throw it away. Everything the
//! reconstruction carries between frames is what a splice invalidates: the
//! previous mixing matrix each object interpolates away from, the analysis and
//! synthesis QMF banks holding 577 samples of the outgoing programme, the
//! core's own overlap-add tail, and the differential OAMD and auxiliary syntax
//! state. Carried across a cut, all of it is applied to audio it has nothing to
//! do with.
//!
//! Reading a flag back proves nothing about any of that, so what these tests
//! assert is the audio: the same access unit decoded twice in a row has to come
//! out the same both times, because its counter cannot follow itself and the
//! second decode therefore starts cold.

use eac3::ObjectPcmDecoder;

const FIXTURE: &[u8] = include_bytes!("data/short_packet_independent_joc.bin");

/// Decode `FIXTURE` and return the object PCM plus the delayed core.
fn decode(decoder: &mut ObjectPcmDecoder) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
    let pcm = decoder
        .push_access_unit(FIXTURE)
        .expect("fixture must decode")
        .expect("fixture carries JOC objects")
        .pcm;
    (pcm.object_channels, pcm.core.fullband_channels)
}

/// The counter of a repeated frame cannot be its own successor, so the second
/// decode is a splice and must reproduce the first exactly.
///
/// Without the reset this fails on the objects rather than the core: the second
/// pass runs the QMF banks warm, so every object carries the tail of the first
/// pass through the filter bank's 577-sample memory.
#[test]
fn a_repeated_sequence_counter_cold_starts_the_reconstruction() {
    let mut decoder = ObjectPcmDecoder::default();
    let (first_objects, first_core) = decode(&mut decoder);
    let (second_objects, second_core) = decode(&mut decoder);

    assert_eq!(
        first_objects.len(),
        second_objects.len(),
        "the object count must not change between two decodes of one frame"
    );
    for (index, (first, second)) in first_objects.iter().zip(&second_objects).enumerate() {
        assert_eq!(
            first, second,
            "object {index} differs between two decodes of the same access unit, \
             so the second inherited reconstruction state from the first"
        );
    }
    assert_eq!(
        first_core, second_core,
        "the delayed core differs, so the alignment delay kept the first pass's tail"
    );
}

/// The cold start has to be a real one: after the splice the decoder must hold
/// exactly what a decoder that had never seen a frame holds.
#[test]
fn the_frame_after_a_splice_matches_a_decoder_that_never_saw_the_first() {
    let mut warm = ObjectPcmDecoder::default();
    let _ = decode(&mut warm);
    let (after_splice, after_splice_core) = decode(&mut warm);

    let mut cold = ObjectPcmDecoder::default();
    let (from_cold, from_cold_core) = decode(&mut cold);

    assert_eq!(
        after_splice, from_cold,
        "the frame after a splice must reconstruct as it would from a cold decoder"
    );
    assert_eq!(
        after_splice_core, from_cold_core,
        "the core after a splice must be delayed as it would be from a cold decoder"
    );
}

/// The pairing entry point has to cold-start on the same terms.
///
/// `push_access_unit_with_core` is handed core PCM decoded elsewhere, so it
/// never touches this decoder's own core state - but the previous mixing
/// matrix, both QMF banks and the alignment delay are all still this decoder's,
/// and all of them have to go at a splice. Nothing about the external core
/// exempts the path from the check.
#[test]
fn the_external_core_path_cold_starts_at_a_splice_too() {
    // A core to hand in, taken from a decode of the same access unit so it is
    // the shape the fixture's JOC expects.
    let core = {
        let mut source = ObjectPcmDecoder::default();
        source
            .push_access_unit(FIXTURE)
            .expect("fixture must decode")
            .expect("fixture carries JOC objects")
            .pcm
            .core
    };

    let mut decoder = ObjectPcmDecoder::default();
    let first = decoder
        .push_access_unit_with_core(FIXTURE, core.clone())
        .expect("fixture must decode")
        .expect("fixture carries JOC objects")
        .pcm;
    let second = decoder
        .push_access_unit_with_core(FIXTURE, core.clone())
        .expect("fixture must decode")
        .expect("fixture carries JOC objects")
        .pcm;

    for (index, (a, b)) in first
        .object_channels
        .iter()
        .zip(&second.object_channels)
        .enumerate()
    {
        assert_eq!(
            a, b,
            "object {index} differs on the paired path, so the second decode \
             inherited reconstruction state from the first"
        );
    }

    let mut cold = ObjectPcmDecoder::default();
    let from_cold = cold
        .push_access_unit_with_core(FIXTURE, core)
        .expect("fixture must decode")
        .expect("fixture carries JOC objects")
        .pcm;
    assert_eq!(
        second.object_channels, from_cold.object_channels,
        "the paired frame after a splice must reconstruct as it would from cold"
    );
}

/// `frames_seen` counts access units accepted, not programmes: a splice resets
/// the audio history and nothing else.
#[test]
fn a_splice_does_not_rewind_the_frame_count() {
    let mut decoder = ObjectPcmDecoder::default();
    let _ = decode(&mut decoder);
    let _ = decode(&mut decoder);
    assert_eq!(
        decoder.frames_seen(),
        2,
        "both access units were accepted, so both must be counted"
    );
}
