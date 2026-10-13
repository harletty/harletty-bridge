//! IAMF over the private IEC 61937 burst type decodes exactly as the raw
//! stream does (Omniphony `docs/iamf-transport-plan.md`): every conformance
//! vector the sender can carry is cut into bursts by Omniphony's framer,
//! read back by its parser the way a host does, and compared frame for
//! frame with the raw decode. A receiver that joins late is checked too.
//!
//! Needs `HARLETTY_IAMF_VECTORS` (the libiamf test vectors); skips without.
#![cfg(feature = "iamf")]

use abi_stable::std_types::RSlice;
use bridge_api::{RDecodedFrame, RInputTransport};
use spdif::SpdifParser;
use spdif::iamf::{Carrier, IamfSequence, write_bursts};

/// Wide enough for every vector that is not many channels of LPCM.
const CARRIER: Carrier = Carrier {
    rate_hz: 48_000,
    channels: 16,
};

fn vectors() -> Vec<std::path::PathBuf> {
    let Some(dir) = std::env::var_os("HARLETTY_IAMF_VECTORS") else {
        eprintln!("skipping: set HARLETTY_IAMF_VECTORS to the libiamf test vectors");
        return Vec::new();
    };
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "iamf"))
        .collect();
    paths.sort();
    paths
}

/// What a host reads of the frames: the samples, their shape, and the
/// object metadata.
fn heard(frames: &[RDecodedFrame]) -> Vec<String> {
    frames
        .iter()
        .map(|frame| {
            let metadata: Vec<String> = frame
                .metadata
                .iter()
                .map(|m| {
                    let names: Vec<(u32, &str)> = m
                        .name_updates
                        .iter()
                        .map(|update| (update.id, update.name.as_str()))
                        .collect();
                    format!(
                        "@{} ramp {} {:?} {:?} {:?} {names:?}",
                        m.sample_pos, m.ramp_duration, m.events, m.object_channels, m.channel_gains
                    )
                })
                .collect();
            format!(
                "{} Hz {} x {} {:?} {:?} {metadata:?}",
                frame.sampling_frequency,
                frame.sample_count,
                frame.channel_count,
                frame.channel_labels,
                frame.pcm,
            )
        })
        .collect()
}

/// The same OBUs on the raw transport, a push for the descriptors and for
/// each unit: `None` when the bridge refuses the sequence.
fn decode_raw(sequence: &IamfSequence) -> Option<Vec<RDecodedFrame>> {
    let mut bridge = harletty_bridge::new_bridge(false);
    let mut frames = Vec::new();
    let pushes = std::iter::once(&sequence.descriptors).chain(&sequence.units);
    for push in pushes {
        let result = bridge.push_packet(RSlice::from_slice(push), RInputTransport::Raw, 0);
        if !result.error_message.is_empty() {
            return None;
        }
        frames.extend(result.frames);
    }
    Some(frames)
}

/// Decode a carrier's bytes as a host does: its parser, then the bridge,
/// burst by burst.
fn decode_carried(name: &str, carried: &[u8]) -> Vec<RDecodedFrame> {
    let mut bridge = harletty_bridge::new_bridge(false);
    let mut parser = SpdifParser::new();
    let mut frames = Vec::new();
    // Odd-sized reads: bursts straddle them.
    for chunk in carried.chunks(4093) {
        parser.push_bytes(chunk);
        while let Some(burst) = parser.get_next_packet() {
            let result = bridge.push_packet(
                RSlice::from_slice(&burst.payload),
                RInputTransport::Iec61937,
                burst.data_type,
            );
            assert!(
                result.error_message.is_empty(),
                "{name}: {}",
                result.error_message
            );
            frames.extend(result.frames);
        }
    }
    frames
}

#[test]
fn every_carriable_vector_decodes_as_its_raw_stream() {
    let mut carried_count = 0;
    let mut frames_count = 0;
    for path in vectors() {
        let stream = std::fs::read(&path).unwrap();
        // What the sender cannot carry (several sequences, a unit wider
        // than the carrier, a rate that does not divide) is its to refuse.
        let Ok(sequence) = IamfSequence::parse(&stream) else {
            continue;
        };
        let Ok(period) = CARRIER.period_bytes(sequence.timing, Some(48_000)) else {
            continue;
        };
        let mut carried = Vec::new();
        if write_bursts(&mut carried, &sequence, period, 5).is_err() {
            continue;
        }
        // The reference is the same OBUs raw, unit by unit. A vector the
        // bridge refuses raw (a profile it does not decode) or that no
        // family claims (a wrong `ia_code`) has nothing to compare.
        let Some(raw) = decode_raw(&sequence) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy();
        let ours = decode_carried(&name, &carried);
        assert_eq!(ours.len(), raw.len(), "{name}: frames");
        assert_eq!(heard(&ours), heard(&raw), "{name}");
        carried_count += 1;
        frames_count += raw.len();
    }
    eprintln!("{carried_count} vector(s) carried, {frames_count} frame(s) compared");
    if std::env::var_os("HARLETTY_IAMF_VECTORS").is_some() {
        assert!(carried_count > 100, "{carried_count} vectors carried");
    }
}

/// A receiver that starts reading mid-stream, inside a burst, locks on the
/// next copy of the descriptors and decodes the stream's own samples.
#[test]
fn a_late_receiver_locks_on_the_next_copy() {
    let Some(dir) = std::env::var_os("HARLETTY_IAMF_VECTORS") else {
        eprintln!("skipping: set HARLETTY_IAMF_VECTORS to the libiamf test vectors");
        return;
    };
    // LPCM: its units stand alone, so the tail is comparable sample for
    // sample.
    let stream = std::fs::read(std::path::Path::new(&dir).join("test_000800.iamf")).unwrap();
    let sequence = IamfSequence::parse(&stream).unwrap();
    let period = CARRIER.period_bytes(sequence.timing, None).unwrap();
    let copy_every = 10;
    let mut carried = Vec::new();
    write_bursts(&mut carried, &sequence, period, copy_every).unwrap();

    let raw = decode_raw(&sequence).unwrap();
    let pcm = |frames: &[RDecodedFrame]| -> Vec<i32> {
        frames.iter().flat_map(|f| f.pcm.iter().copied()).collect()
    };
    let whole = pcm(&raw);
    // Joined 23 bursts and a bit in: the bursts up to the next copy (the
    // 30th) are heard without a sequence.
    let late = pcm(&decode_carried("late", &carried[23 * period + 1234..]));
    assert!(!late.is_empty());
    assert!(late.len() < whole.len());
    assert_eq!(late, whole[whole.len() - late.len()..]);
    let samples_per_unit = whole.len() / sequence.units.len();
    assert_eq!(
        (whole.len() - late.len()) / samples_per_unit,
        30,
        "locked at the first copy after joining"
    );
}

/// A damaged burst costs its own unit and no other. In a unit of several
/// substreams, the OBUs of a truncated burst that still frame must not be
/// decoded: their substreams would wait in the decoder for the rest of
/// their unit and be matched with the next one's, shifting every unit after.
#[test]
fn a_damaged_burst_costs_its_unit_only() {
    let Some(dir) = std::env::var_os("HARLETTY_IAMF_VECTORS") else {
        eprintln!("skipping: set HARLETTY_IAMF_VECTORS to the libiamf test vectors");
        return;
    };
    // LPCM 5.1: four substreams to a unit.
    let stream = std::fs::read(std::path::Path::new(&dir).join("test_000038.iamf")).unwrap();
    let sequence = IamfSequence::parse(&stream).unwrap();
    assert!(sequence.units.len() > 12);

    // How unit 5 is damaged: its last byte lost (the last OBU no longer
    // frames, the ones before it still do), or its last OBU lost whole (what
    // is left is whole OBUs, of an incomplete unit).
    let last_obu = |unit: &[u8]| -> usize {
        let (mut pos, mut last) = (0, 0);
        while let Ok(Some(obu)) = bridge_family_iamf::frame_obu(&unit[pos..]) {
            last = pos;
            pos += obu.len;
        }
        last
    };
    let decode = |damaged: Option<usize>, whole_obu: bool| -> Vec<Vec<i32>> {
        let mut bridge = harletty_bridge::new_bridge(false);
        let mut units = Vec::new();
        for (index, unit) in sequence.units.iter().enumerate() {
            let mut payload = if index == 0 {
                sequence.descriptors.clone()
            } else {
                Vec::new()
            };
            payload.extend_from_slice(unit);
            if damaged == Some(index) {
                let keep = if whole_obu {
                    payload.len() - (unit.len() - last_obu(unit))
                } else {
                    payload.len() - 1
                };
                payload.truncate(keep);
            }
            let result = bridge.push_packet(
                RSlice::from_slice(&payload),
                RInputTransport::Iec61937,
                spdif::DATA_TYPE_IAMF_PRIVATE,
            );
            assert!(result.error_message.is_empty(), "{}", result.error_message);
            units.push(
                result
                    .frames
                    .iter()
                    .flat_map(|f| f.pcm.iter().copied())
                    .collect(),
            );
        }
        units
    };

    let intact = decode(None, false);
    assert!(intact.iter().all(|unit| !unit.is_empty()));
    for whole_obu in [false, true] {
        let hurt = decode(Some(5), whole_obu);
        assert!(hurt[5].is_empty(), "the damaged unit is not heard");
        for (index, (ours, reference)) in hurt.iter().zip(&intact).enumerate() {
            if index != 5 {
                assert_eq!(ours, reference, "unit {index}, whole OBU lost: {whole_obu}");
            }
        }
    }
}

fn vector_sequence(name: &str) -> Option<IamfSequence> {
    let dir = std::env::var_os("HARLETTY_IAMF_VECTORS").or_else(|| {
        eprintln!("skipping: set HARLETTY_IAMF_VECTORS to the libiamf test vectors");
        None
    })?;
    let stream = std::fs::read(std::path::Path::new(&dir).join(name)).unwrap();
    Some(IamfSequence::parse(&stream).unwrap())
}

/// `unit` short of its last OBU: whole OBUs still, of an incomplete unit.
fn without_last_obu(unit: &[u8]) -> &[u8] {
    let (mut pos, mut last) = (0, 0);
    while let Ok(Some(obu)) = bridge_family_iamf::frame_obu(&unit[pos..]) {
        last = pos;
        pos += obu.len;
    }
    &unit[..last]
}

/// Push bursts and collect each one's samples.
fn burst_samples(bridge: &mut bridge_api::FormatBridgeBox, bursts: &[Vec<u8>]) -> Vec<Vec<i32>> {
    bursts
        .iter()
        .map(|payload| {
            let result = bridge.push_packet(
                RSlice::from_slice(payload),
                RInputTransport::Iec61937,
                spdif::DATA_TYPE_IAMF_PRIVATE,
            );
            assert!(result.error_message.is_empty(), "{}", result.error_message);
            result
                .frames
                .iter()
                .flat_map(|f| f.pcm.iter().copied())
                .collect()
        })
        .collect()
}

/// The intact decode of a sequence, a burst to a unit, the descriptors
/// leading the first.
fn intact_units(sequence: &IamfSequence) -> Vec<Vec<i32>> {
    let mut bursts = sequence.units.clone();
    bursts[0] = [sequence.descriptors.as_slice(), &sequence.units[0]].concat();
    burst_samples(&mut harletty_bridge::new_bridge(false), &bursts)
}

/// The burst that opens a sequence is damaged: its unit is lost, its
/// descriptors are not. A sender need not repeat them (mpv with
/// `--ad-iamf-iec61937-copy-interval=0`), so dropping them with the unit
/// would silence the stream until the next seek.
#[test]
fn a_damaged_opening_burst_still_opens_the_sequence() {
    // LPCM 5.1: four substreams to a unit.
    let Some(sequence) = vector_sequence("test_000038.iamf") else {
        return;
    };
    let intact = intact_units(&sequence);
    for whole_obu in [false, true] {
        let unit = &sequence.units[0];
        let cut = if whole_obu {
            without_last_obu(unit)
        } else {
            &unit[..unit.len() - 1]
        };
        let mut bursts = sequence.units.clone();
        bursts[0] = [sequence.descriptors.as_slice(), cut].concat();
        let hurt = burst_samples(&mut harletty_bridge::new_bridge(false), &bursts);
        assert!(hurt[0].is_empty(), "the damaged unit is not heard");
        assert_eq!(hurt[1..], intact[1..], "whole OBU lost: {whole_obu}");
    }
}

/// The descriptors may come in a burst of their own. The first unit after
/// them is checked against them like any other: no decoder exists yet, but
/// the sequence is known.
#[test]
fn the_first_unit_after_descriptors_alone_is_checked() {
    let Some(sequence) = vector_sequence("test_000038.iamf") else {
        return;
    };
    let intact = intact_units(&sequence);
    let mut bursts = vec![sequence.descriptors.clone()];
    bursts.push(without_last_obu(&sequence.units[0]).to_vec());
    bursts.extend_from_slice(&sequence.units[1..]);
    let hurt = burst_samples(&mut harletty_bridge::new_bridge(false), &bursts);
    assert!(hurt[0].is_empty() && hurt[1].is_empty());
    assert_eq!(hurt[2..], intact[1..]);

    // Intact, the same shape decodes as the stream.
    let mut bursts = vec![sequence.descriptors.clone()];
    bursts.extend_from_slice(&sequence.units);
    let whole = burst_samples(&mut harletty_bridge::new_bridge(false), &bursts);
    assert_eq!(whole[1..], intact[..]);
}

/// A sender may lead the burst that changes sequence with its copy of the
/// old one's descriptors. The unit belongs to the sequence the last
/// non-redundant header opens, not to the one the copy repeats.
#[test]
fn a_new_sequence_led_by_a_copy_of_the_old_is_the_new_one() {
    // Four substreams, then one.
    let (Some(old), Some(new)) = (
        vector_sequence("test_000038.iamf"),
        vector_sequence("test_000800.iamf"),
    ) else {
        return;
    };
    let intact_new = intact_units(&new);
    let mut bursts = old.units[..6].to_vec();
    bursts[0] = [old.descriptors.as_slice(), &old.units[0]].concat();
    bursts.push(
        [
            old.redundant_descriptors().as_slice(),
            &new.descriptors,
            &new.units[0],
        ]
        .concat(),
    );
    bursts.extend_from_slice(&new.units[1..]);
    let heard = burst_samples(&mut harletty_bridge::new_bridge(false), &bursts);
    assert!(heard[..6].iter().all(|unit| !unit.is_empty()));
    assert_eq!(heard[6..], intact_new[..]);
}
