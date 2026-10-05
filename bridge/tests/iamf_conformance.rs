//! The bridge on the libiamf conformance vectors, through the host's API:
//! what a host sees of an IAMF stream (frames, labels, poses, tags, the
//! source description) and how it behaves across a reset.
#![cfg(feature = "iamf")]

use abi_stable::std_types::RSlice;
use bridge_api::{FormatBridgeBox, RDecodedFrame, RInputTransport};
use bridge_family_iamf::{BED_714_LABELS, frame_obu};

// ── Conformance vectors ─────────────────────────────────────────────
//
// The libiamf test vectors (AOMediaCodec/libiamf, `tests/`) are not
// committed: point `HARLETTY_IAMF_VECTORS` at a directory holding them.
// Each `.iamf` comes with `<name>_rendered_id_<mix>_sub_mix_0_layout_<n>.wav`
// references, one per layout its mix declares.

fn vector(name: &str) -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("HARLETTY_IAMF_VECTORS")?;
    let path = std::path::Path::new(&dir).join(name);
    path.exists().then_some(path)
}

/// 16-bit PCM WAV samples, interleaved, at the bridge's 2^23 scale.
fn read_wav_s16(path: &std::path::Path) -> (u16, Vec<i32>) {
    let bytes = std::fs::read(path).unwrap();
    let mut pos = 12;
    let mut channels = 0;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &bytes[pos + 8..pos + 8 + len];
        if id == b"fmt " {
            channels = u16::from_le_bytes([body[2], body[3]]);
            assert_eq!(u16::from_le_bytes([body[14], body[15]]), 16, "16-bit only");
        } else if id == b"data" {
            let samples = body
                .chunks_exact(2)
                .map(|b| i32::from(i16::from_le_bytes([b[0], b[1]])) << 8)
                .collect();
            return (channels, samples);
        }
        pos += 8 + len + (len & 1);
    }
    panic!("no data chunk in {}", path.display());
}

/// Push a whole stream through the raw transport in odd-sized chunks, so
/// OBUs straddle packet boundaries the way a pipe delivers them.
fn decode_raw(bridge: &mut FormatBridgeBox, stream: &[u8]) -> Vec<RDecodedFrame> {
    let mut frames = Vec::new();
    for chunk in stream.chunks(997) {
        let result = bridge.push_packet(RSlice::from_slice(chunk), RInputTransport::Raw, 0);
        assert!(result.error_message.is_empty(), "{}", result.error_message);
        frames.extend(result.frames);
    }
    frames
}

fn interleaved(frames: &[RDecodedFrame]) -> Vec<i32> {
    frames.iter().flat_map(|f| f.pcm.iter().copied()).collect()
}

fn psnr_db(ours: &[i32], reference: &[i32]) -> f64 {
    let full_scale = f64::from(1 << 23);
    let mse = ours
        .iter()
        .zip(reference)
        .map(|(&a, &b)| ((f64::from(a) - f64::from(b)) / full_scale).powi(2))
        .sum::<f64>()
        / ours.len() as f64;
    10.0 * (1.0 / mse).log10()
}

#[test]
fn lossless_714_decodes_bit_exact_through_the_raw_transport() {
    // A 7.1.4 LPCM stream with demixing parameter blocks; its second
    // declared layout is System J.
    let (Some(stream), Some(reference)) = (
        vector("test_000082.iamf"),
        vector("test_000082_rendered_id_42_sub_mix_0_layout_1.wav"),
    ) else {
        eprintln!("skipping: set HARLETTY_IAMF_VECTORS to the libiamf test vectors");
        return;
    };
    let mut bridge = harletty_bridge::new_bridge(false);
    let frames = decode_raw(&mut bridge, &std::fs::read(stream).unwrap());

    assert!(bridge.is_ready());
    assert!(!bridge.has_objects());
    assert_eq!(bridge.source_family().as_str(), "iamf");
    assert_eq!(bridge.source_label().as_str(), "IAMF (PCM) 7.1.4");
    assert_eq!(bridge.fixed_channel_poses().len(), 11);
    for frame in &frames {
        assert_eq!(frame.channel_labels.as_slice(), BED_714_LABELS.as_slice());
        assert_eq!(frame.sampling_frequency, 48_000);
        assert!(frame.metadata.is_empty(), "a bed carries no metadata");
    }
    let (channels, expected) = read_wav_s16(&reference);
    assert_eq!(usize::from(channels), BED_714_LABELS.len());
    assert_eq!(interleaved(&frames), expected);
}

#[test]
fn an_expanded_7154_stream_renders_to_the_714_bed() {
    // IAMF v2.0 expanded layout 16: a 7.1.5.4 LPCM element beside a
    // stereo one (advanced profile), rendered to System J through
    // 7.1.5.4's own matrix; its second declared layout is System J.
    let (Some(stream), Some(reference)) = (
        vector("test_000833.iamf"),
        vector("test_000833_rendered_id_42_sub_mix_0_layout_1.wav"),
    ) else {
        eprintln!("skipping: set HARLETTY_IAMF_VECTORS to the libiamf test vectors");
        return;
    };
    let mut bridge = harletty_bridge::new_bridge(false);
    let frames = decode_raw(&mut bridge, &std::fs::read(stream).unwrap());
    assert_eq!(
        bridge.source_label().as_str(),
        "IAMF (PCM) 7.1.5.4 + stereo"
    );
    assert!(
        frames
            .iter()
            .all(|f| f.channel_labels.as_slice() == BED_714_LABELS.as_slice())
    );
    let ours = interleaved(&frames);
    let (_, expected) = read_wav_s16(&reference);
    assert_eq!(ours.len(), expected.len());
    // A matrix render, compared at the reference's 16 bits.
    let max_diff = ours
        .iter()
        .zip(&expected)
        .map(|(a, b)| ((a - b).abs() + 128) >> 8)
        .max();
    assert!(max_diff <= Some(1), "max diff {max_diff:?} (16-bit steps)");
}

#[test]
fn opus_714_decodes_within_the_lossy_tolerance() {
    let (Some(stream), Some(reference)) = (
        vector("test_000220.iamf"),
        vector("test_000220_rendered_id_42_sub_mix_0_layout_1.wav"),
    ) else {
        eprintln!("skipping: set HARLETTY_IAMF_VECTORS to the libiamf test vectors");
        return;
    };
    let mut bridge = harletty_bridge::new_bridge(false);
    let frames = decode_raw(&mut bridge, &std::fs::read(stream).unwrap());
    assert_eq!(bridge.source_label().as_str(), "IAMF (Opus) 7.1.4");
    let ours = interleaved(&frames);
    let (_, expected) = read_wav_s16(&reference);
    assert_eq!(ours.len(), expected.len());
    // The libiamf suite's bar for lossy codecs is an average PSNR above 30.
    let psnr = psnr_db(&ours, &expected);
    assert!(psnr > 30.0, "PSNR {psnr:.1} dB");
}

#[test]
fn a_reset_resumes_at_the_next_temporal_unit_without_a_sequence_header() {
    let Some(stream) = vector("test_000220.iamf") else {
        eprintln!("skipping: set HARLETTY_IAMF_VECTORS to the libiamf test vectors");
        return;
    };
    let stream = std::fs::read(stream).unwrap();
    // Temporal-unit starts, where a host's packets begin after a seek.
    // This vector carries no temporal delimiters (they are optional):
    // each unit opens with the audio frame of substream 0 (OBU type 6).
    let mut unit_starts = Vec::new();
    let mut pos = 0;
    while let Ok(Some(frame)) = frame_obu(&stream[pos..]) {
        if frame.obu_type == 6 {
            unit_starts.push(pos);
        }
        pos += frame.len;
    }
    assert_eq!(pos, stream.len(), "the vector frames end to end");
    let seek_to = unit_starts[unit_starts.len() / 2];

    let mut whole = harletty_bridge::new_bridge(false);
    let all = interleaved(&decode_raw(&mut whole, &stream));

    let mut bridge = harletty_bridge::new_bridge(false);
    decode_raw(&mut bridge, &stream[..seek_to / 2]);
    bridge.reset();
    let resumed = interleaved(&decode_raw(&mut bridge, &stream[seek_to..]));
    assert!(!resumed.is_empty(), "decoding resumed after the reset");
    // The resumed stream lines up with the continuous one: every
    // 960-sample unit from the seek point on, less the stream's end trim.
    // The continuous decode also lost the Opus pre-skip, 312 samples
    // trimmed from the first unit, which the resumed one never reaches.
    const UNIT: usize = 960;
    const PRE_SKIP: usize = 312;
    let channels = BED_714_LABELS.len();
    let units_after_seek = unit_starts.len() - unit_starts.len() / 2;
    let end_trim = unit_starts.len() * UNIT - all.len() / channels - PRE_SKIP;
    assert_eq!(resumed.len() / channels, units_after_seek * UNIT - end_trim);
    // Opus restarts from a cleared state, so the first units differ and
    // the rest reconverge to within rounding of the continuous decode.
    let tail = resumed.len() / 2;
    let psnr = psnr_db(&resumed[resumed.len() - tail..], &all[all.len() - tail..]);
    assert!(psnr > 90.0, "resumed tail PSNR {psnr:.1} dB");
}

/// All metadata events of a decode, in order.
#[test]
fn a_mix_of_unmarked_elements_tags_nothing() {
    // Two-layer 5.1 + stereo, neither annotated nor adjustable: the bed
    // as before.
    let Some(path) = vector("test_000087.iamf") else {
        eprintln!("skipping: set HARLETTY_IAMF_VECTORS to the libiamf test vectors");
        return;
    };
    let stream = std::fs::read(path).unwrap();
    let mut bridge = harletty_bridge::new_bridge(false);
    let frames = decode_raw(&mut bridge, &stream);
    assert_eq!(frames[0].channel_labels.as_slice(), BED_714_LABELS);
    assert!(bridge.channel_tags().is_empty());
}
