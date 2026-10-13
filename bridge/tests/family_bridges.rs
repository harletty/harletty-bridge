//! Each codec family as a bridge of its own, `PluginBridge<Family>`, decodes
//! a stream of its family exactly as the combined bridge does: the same
//! frames, field for field, and the same stream description after every
//! push. Raw and IEC 61937, from the fixtures in the repository and, when
//! their variables point at them, the local corpus streams.
#![cfg(all(feature = "dolby", feature = "dts"))]

use abi_stable::std_types::RSlice;
use bridge_api::{FormatBridge, RDecodedFrame, RInputTransport};
use bridge_common::family::PluginBridge;
use bridge_family_dolby::DolbyPipeline;
use bridge_family_dts::DtsPipeline;

const EAC3: &[u8] = include_bytes!("../../harletty/tests/fixtures/joc_atmos_1s.eac3");
const DTS: &[u8] = include_bytes!("../../harletty/tests/fixtures/dts_core_tone_10f.dts");

/// FNV-1a over everything a host reads.
#[derive(Default)]
struct Digest(u64);

impl Digest {
    fn write(&mut self, bytes: &[u8]) {
        if self.0 == 0 {
            self.0 = 0xcbf2_9ce4_8422_2325;
        }
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn frame(&mut self, frame: &RDecodedFrame) {
        self.write(&frame.sampling_frequency.to_le_bytes());
        self.write(&frame.sample_count.to_le_bytes());
        self.write(&frame.channel_count.to_le_bytes());
        for sample in frame.pcm.iter() {
            self.write(&sample.to_le_bytes());
        }
        for label in frame.channel_labels.iter() {
            self.write(&[*label as u8]);
        }
        self.write(&frame.drc_gain.to_le_bytes());
        self.write(&frame.drc_ramp_duration.to_le_bytes());
        self.write(&[
            u8::from(frame.is_new_segment),
            frame.dialogue_level.into_option().map_or(0x80, |l| l as u8),
        ]);
        for metadata in frame.metadata.iter() {
            self.write(&metadata.sample_pos.to_le_bytes());
            self.write(&metadata.ramp_duration.to_le_bytes());
            for event in metadata.events.iter() {
                self.write(&event.id.to_le_bytes());
                self.write(&event.sample_pos.to_le_bytes());
                self.write(&[u8::from(event.has_pos), event.gain_db as u8]);
                for axis in event.pos.iter().chain(event.size.iter()) {
                    self.write(&axis.to_le_bytes());
                }
            }
            for declared in metadata.object_channels.iter() {
                self.write(&declared.id.to_le_bytes());
                self.write(&declared.channel.to_le_bytes());
            }
            for gain in metadata.channel_gains.iter() {
                self.write(&gain.channel.to_le_bytes());
                self.write(&[gain.gain_db as u8]);
            }
            for update in metadata.name_updates.iter() {
                self.write(&update.id.to_le_bytes());
                self.write(update.name.as_bytes());
            }
        }
    }

    fn description(&mut self, bridge: &impl FormatBridge) {
        self.write(&[u8::from(bridge.is_ready()), u8::from(bridge.has_objects())]);
        self.write(bridge.source_family().as_bytes());
        self.write(&[0]);
        self.write(bridge.source_label().as_bytes());
        self.write(&[0]);
        for pose in bridge.fixed_channel_poses().iter() {
            self.write(&[pose.label as u8]);
            self.write(&pose.azimuth_deg.to_le_bytes());
            self.write(&pose.elevation_deg.to_le_bytes());
        }
        for tag in bridge.channel_tags().iter() {
            self.write(tag.kind.as_bytes());
            for channel in tag.channels.iter() {
                self.write(&channel.to_le_bytes());
            }
        }
    }
}

/// What one bridge made of a stream: a digest of every push's frames,
/// errors and resets and of the description after it, and the frame count.
fn run(bridge: &mut impl FormatBridge, packets: &[(&[u8], RInputTransport, u8)]) -> (u64, usize) {
    let mut digest = Digest::default();
    let mut frames = 0;
    for &(data, transport, data_type) in packets {
        let result = bridge.push_packet(RSlice::from_slice(data), transport, data_type);
        frames += result.frames.len();
        for frame in result.frames.iter() {
            digest.frame(frame);
        }
        digest.write(&[u8::from(result.did_reset)]);
        digest.write(result.error_message.as_bytes());
        digest.description(&*bridge);
    }
    (digest.0, frames)
}

fn raw(stream: &[u8], chunk: usize) -> Vec<(&[u8], RInputTransport, u8)> {
    stream
        .chunks(chunk)
        .map(|c| (c, RInputTransport::Raw, 0))
        .collect()
}

/// IEC 61937 bursts as the live input hands them over: one frame each, with
/// its burst type. Built from the raw stream's own frames.
fn bursts(frames: &[&'static [u8]], data_type: u8) -> Vec<(&'static [u8], RInputTransport, u8)> {
    frames
        .iter()
        .map(|&frame| (frame, RInputTransport::Iec61937, data_type))
        .collect()
}

fn assert_same<F: bridge_common::family::FamilyPipeline>(
    name: &str,
    packets: &[(&[u8], RInputTransport, u8)],
) {
    let mut combined = harletty_bridge::new_bridge(false);
    let mut family = PluginBridge::<F>::new(false);
    let expected = run(&mut combined, packets);
    let actual = run(&mut family, packets);
    assert!(expected.1 > 0, "{name}: nothing decoded");
    assert_eq!(actual, expected, "{name}: the family bridge differs");
}

/// The E-AC-3 syncframes of a raw stream (whose frame size the header
/// states), for the IEC 61937 runs.
fn eac3_frames(stream: &'static [u8]) -> Vec<&'static [u8]> {
    let mut frames = Vec::new();
    let mut at = 0;
    while at + 4 < stream.len() {
        let size =
            ((usize::from(stream[at + 2] & 0x07) << 8) | usize::from(stream[at + 3])) * 2 + 2;
        frames.push(&stream[at..(at + size).min(stream.len())]);
        at += size;
    }
    frames
}

#[test]
fn the_dolby_family_alone_decodes_as_the_combined_bridge() {
    // Not smaller: the combined bridge sniffs the first packet alone, and
    // takes a packet too short to hold a sync word for TrueHD.
    for chunk in [64 * 1024, 4096, 997] {
        assert_same::<DolbyPipeline>("E-AC-3 raw", &raw(EAC3, chunk));
    }
    assert_same::<DolbyPipeline>("TrueHD raw", &raw(truehd::process::EXAMPLE_DATA, 7));
    assert_same::<DolbyPipeline>("E-AC-3 IEC 61937", &bursts(&eac3_frames(EAC3), 0x15));
}

#[test]
fn the_dts_family_alone_decodes_as_the_combined_bridge() {
    for chunk in [64 * 1024, 4096, 997] {
        assert_same::<DtsPipeline>("DTS raw", &raw(DTS, chunk));
    }
    // Type I bursts: the core frames as they are, cut at the size their
    // header states (FSIZE, 14 bits from bit 46).
    let size = ((usize::from(DTS[5] & 0x03) << 12)
        | (usize::from(DTS[6]) << 4)
        | (usize::from(DTS[7]) >> 4))
        + 1;
    let frames: Vec<&'static [u8]> = DTS.chunks(size).collect();
    assert_same::<DtsPipeline>("DTS IEC 61937", &bursts(&frames, 0x0B));
}

/// The local corpus, when its variables are set (`corpus.env` of the
/// baseline kit): the streams that exercise the paths the fixtures do not.
#[test]
fn corpus_streams_decode_as_with_the_combined_bridge() {
    let corpus = |variable: &str| {
        let path = std::env::var(variable).ok()?;
        let bytes = std::fs::read(&path).ok()?;
        Some(bytes[..bytes.len().min(4_000_000)].to_vec())
    };
    let mut ran = 0;
    for variable in [
        "HARLETTY_DTS_CORE_CORPUS",
        "HARLETTY_LOSSY_X_CORPUS",
        "HARLETTY_DTSX_STANDARD_CORPUS",
        "HARLETTY_AURO_DTS_CORPUS",
        "HARLETTY_ALT_51_CORPUS",
    ] {
        let Some(stream) = corpus(variable) else {
            eprintln!("skipping {variable}: not set to a readable file");
            continue;
        };
        assert_same::<DtsPipeline>(variable, &raw(&stream, 64 * 1024));
        ran += 1;
    }
    eprintln!("{ran} corpus stream(s) compared");
}

#[cfg(feature = "iamf")]
#[test]
fn the_iamf_family_alone_decodes_as_the_combined_bridge() {
    use bridge_family_iamf::IamfPipeline;
    let Some(dir) = std::env::var_os("HARLETTY_IAMF_VECTORS") else {
        eprintln!("skipping: HARLETTY_IAMF_VECTORS is not set");
        return;
    };
    let mut vectors: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "iamf"))
        .collect();
    vectors.sort();
    let mut ran = 0;
    for path in vectors {
        let stream = std::fs::read(&path).unwrap();
        let packets = raw(&stream, 997);
        let mut combined = harletty_bridge::new_bridge(false);
        let expected = run(&mut combined, &packets);
        // A vector the combined bridge does not take as IAMF (no sequence
        // header at its first byte) is not a comparison of the IAMF path.
        if expected.1 == 0 || combined.source_family().as_str() != "iamf" {
            continue;
        }
        let mut family = PluginBridge::<IamfPipeline>::new(false);
        assert_eq!(run(&mut family, &packets), expected, "{}", path.display());
        ran += 1;
    }
    eprintln!("{ran} IAMF vector(s) compared");
    assert!(ran > 0, "no vector decoded");
}
