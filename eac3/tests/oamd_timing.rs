// SPDX-License-Identifier: Apache-2.0

//! Where the OAMD of an object frame applies in the frame's own PCM.
//!
//! The bitstream dates a payload on the encoder's input: its EMDF sample
//! offset counts from the first sample of the syncframe as the encoder was fed
//! it. The PCM of an object frame comes out later than that: the core one
//! transform block after the encoder's input, and every channel of the frame a
//! further `JOC_LATENCY_SAMPLES`, the objects through the reconstruction and
//! the bed held back to meet them. Read at the bitstream's own offset, the
//! metadata moves an object 833 samples, 17 ms, before its sound does.

use eac3::{
    CORE_OAMD_LATENCY_SAMPLES, JOC_LATENCY_SAMPLES, OAMD_LATENCY_SAMPLES, ObjectPcmDecoder,
    ParsedEmdfPayloadData,
};

const FIXTURE: &[u8] = include_bytes!("data/short_packet_independent_joc.bin");

#[test]
fn the_latency_is_one_core_block_plus_the_reconstruction() {
    assert_eq!(CORE_OAMD_LATENCY_SAMPLES, 256);
    assert_eq!(OAMD_LATENCY_SAMPLES, JOC_LATENCY_SAMPLES + 256);
    assert_eq!(OAMD_LATENCY_SAMPLES, 833);
}

/// Every payload of the frame is dated where it applies in the frame's PCM:
/// its own EMDF sample offset, 0 when it has none, plus the frame's latency.
#[test]
fn each_payload_is_dated_on_the_frame_clock() {
    let push = ObjectPcmDecoder::default()
        .push_access_unit(FIXTURE)
        .expect("fixture must decode")
        .expect("fixture carries JOC objects");

    let in_bitstream: Vec<u32> = push
        .info
        .payloads()
        .filter_map(|payload| match payload.parsed {
            ParsedEmdfPayloadData::Oamd(_) => {
                Some(u32::from(payload.info.sample_offset.unwrap_or(0)))
            }
            _ => None,
        })
        .collect();
    assert!(
        !in_bitstream.is_empty(),
        "the fixture must carry OAMD for this to test anything"
    );

    let dated: Vec<u32> = push
        .pcm
        .oamd_payloads
        .iter()
        .map(|(_, offset)| *offset)
        .collect();
    let expected: Vec<u32> = in_bitstream
        .iter()
        .map(|offset| offset + OAMD_LATENCY_SAMPLES as u32)
        .collect();
    assert_eq!(dated, expected);
}
