//! IAMF (AOMedia Immersive Audio Model and Formats) over the raw transport.
//!
//! The host hands over the OBU stream in arbitrary chunks. The bridge frames
//! complete OBUs itself, collects the descriptor OBUs that open an IA
//! sequence, builds an iamf-rs [`StreamDecoder`] from them, and feeds it the
//! temporal units that follow. Every mix is rendered to BS.2051 System J
//! (7.1.4) and goes out as a labelled channel bed, which the renderer then
//! places on the actual speakers like any other bed.
//!
//! Framing the OBUs here rather than handing raw chunks to the decoder is what
//! lets the bridge start mid-stream (everything before the first sequence
//! header is dropped), skip the reserved OBU types the spec tells parsers to
//! ignore, ignore the redundant descriptor copies a stream repeats for random
//! access, and notice a new IA sequence starting.

use abi_stable::std_types::{RString, RVec};
use bridge_api::{RChannelLabel, RChannelPose, RDecodedFrame, RPushResult};
use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::presentation::Descriptors;
use iamf_dec::stream::{OutputSampleType, StreamDecoder, StreamSettings};
use iamf_obu::descriptors::{AudioElementConfig, CodecId};

use crate::bridge::AtmosBridge;
use crate::logging::panic_message;

/// The layout every mix is rendered to.
const OUTPUT_LAYOUT: SoundSystem = SoundSystem::J;

/// System J in the decoder's IAMF channel order: L, R, C, LFE, Lss, Rss, Lrs,
/// Rrs, Ltf, Rtf, Ltb, Rtb.
const OUTPUT_LABELS: [RChannelLabel; 12] = [
    RChannelLabel::L,
    RChannelLabel::R,
    RChannelLabel::C,
    RChannelLabel::LFE,
    RChannelLabel::Ls,
    RChannelLabel::Rs,
    RChannelLabel::Lb,
    RChannelLabel::Rb,
    RChannelLabel::Tfl,
    RChannelLabel::Tfr,
    RChannelLabel::Tbl,
    RChannelLabel::Tbr,
];

/// Bytes the framer may hold without completing an OBU. An OBU's size is a
/// leb128 the stream states up front, so a value past this is garbage, not
/// a large OBU still arriving.
const MAX_OBU_BYTES: usize = 4 << 20;

/// Descriptor bytes collected before the first temporal unit. Real sequences
/// carry a few hundred bytes of descriptors; this only bounds a stream that
/// never gets to its audio.
const MAX_DESCRIPTOR_BYTES: usize = 1 << 20;

/// OBU type numbers (IAMF §3.2).
const OBU_CODEC_CONFIG: u8 = 0;
const OBU_MIX_PRESENTATION: u8 = 2;
const OBU_RESERVED_FIRST: u8 = 25;
const OBU_RESERVED_LAST: u8 = 30;
const OBU_SEQUENCE_HEADER: u8 = 31;

/// What the bridge does with an OBU, by type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ObuKind {
    /// Opens an IA sequence.
    SequenceHeader,
    /// Codec config, audio element or mix presentation.
    Descriptor,
    /// Reserved: skipped, as the spec asks (§3.2).
    Reserved,
    /// Everything that belongs to a temporal unit.
    Data,
}

fn obu_kind(obu_type: u8) -> ObuKind {
    match obu_type {
        OBU_SEQUENCE_HEADER => ObuKind::SequenceHeader,
        OBU_CODEC_CONFIG..=OBU_MIX_PRESENTATION => ObuKind::Descriptor,
        OBU_RESERVED_FIRST..=OBU_RESERVED_LAST => ObuKind::Reserved,
        _ => ObuKind::Data,
    }
}

/// A complete OBU at the start of a buffer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ObuFrame {
    obu_type: u8,
    /// `obu_redundant_copy`: a repeat of a descriptor already sent.
    redundant: bool,
    /// Header, size field and payload.
    len: usize,
}

/// Frame the OBU at the start of `data`: `Ok(None)` while it is incomplete.
fn frame_obu(data: &[u8]) -> Result<Option<ObuFrame>, String> {
    let Some(&header) = data.first() else {
        return Ok(None);
    };
    // obu_size is a leb128 of at most 8 bytes (§2.4).
    let mut size = 0u64;
    let mut size_len = 0;
    loop {
        let Some(&byte) = data.get(1 + size_len) else {
            return Ok(None);
        };
        size |= u64::from(byte & 0x7F) << (7 * size_len);
        size_len += 1;
        if byte & 0x80 == 0 {
            break;
        }
        if size_len == 8 {
            return Err("iamf: obu_size leb128 longer than 8 bytes".into());
        }
    }
    let len = 1 + size_len + size as usize;
    if size as usize > MAX_OBU_BYTES {
        return Err(format!("iamf: obu_size {size} past the framing bound"));
    }
    if data.len() < len {
        return Ok(None);
    }
    Ok(Some(ObuFrame {
        obu_type: header >> 3,
        redundant: header & 0x04 != 0,
        len,
    }))
}

/// A [`StreamDecoder`] the bridge can hold: `FormatBridge` is `Send + Sync`,
/// and the decoder is neither only because its codec decoders sit behind
/// `Box<dyn SubstreamDecoder>`, a trait without a `Send` bound.
struct SendDecoder(StreamDecoder);

// SAFETY: every substream decoder `DefaultFactory` builds with the features
// this crate enables is `Send`: LPCM is plain data, Symphonia's FLAC and AAC
// decoders are `Send`, and iamf-rs declares its libopus wrapper `Send`. The
// rest of the `StreamDecoder` is owned plain data. So moving one across
// threads is sound; what is missing is only the bound on the trait object.
unsafe impl Send for SendDecoder {}
// SAFETY: the wrapper gives no access through `&self` — only `get` through
// `&mut self` — so a shared reference to it cannot reach the decoder at all.
unsafe impl Sync for SendDecoder {}

impl SendDecoder {
    fn get(&mut self) -> &mut StreamDecoder {
        &mut self.0
    }
}

/// Decoder state for one IAMF stream, boxed in [`AtmosBridge`].
#[derive(Default)]
pub(crate) struct IamfState {
    /// Bytes not yet framed into complete OBUs.
    buf: Vec<u8>,
    /// Descriptor OBUs of the IA sequence being opened, from its sequence
    /// header up to its first temporal unit. Kept once the decoder is built,
    /// so a reset can resume without waiting for the next sequence header.
    descriptors: Vec<u8>,
    decoder: Option<SendDecoder>,
    /// The current sequence's descriptors were refused: drop its audio until
    /// the next sequence header instead of failing every packet.
    refused: bool,
    /// What the decoder is decoding, for the source label.
    description: Option<String>,
    pub(crate) frame_count: u64,
}

impl IamfState {
    /// Forget the stream position (seek, flush): buffered bytes and decoded
    /// state go, the sequence's configuration stays.
    pub(crate) fn reset(&mut self) {
        self.buf.clear();
        self.frame_count = 0;
        if let Some(decoder) = &mut self.decoder {
            decoder.get().reset();
        }
    }

    /// A sequence is configured: temporal units can be decoded as they come.
    pub(crate) fn has_sequence(&self) -> bool {
        self.decoder.is_some()
    }

    /// `IAMF (Opus) ambisonics + stereo`, once a sequence decodes.
    pub(crate) fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Frame everything buffered and decode it, pushing the decoded temporal
    /// units to `out`. An error leaves the bytes it stopped at in the buffer.
    fn drain(&mut self, strict: bool, out: &mut RVec<RDecodedFrame>) -> Result<(), String> {
        let buf = std::mem::take(&mut self.buf);
        let mut pos = 0;
        // Start of the contiguous data OBUs not yet handed to the decoder:
        // forwarded in one call rather than one per OBU.
        let mut run: Option<usize> = None;
        let result = loop {
            let frame = match frame_obu(&buf[pos..]) {
                Ok(Some(frame)) => frame,
                Ok(None) => break Ok(()),
                Err(err) => break Err(err),
            };
            let obu = &buf[pos..pos + frame.len];
            let kind = obu_kind(frame.obu_type);
            let forwards = kind == ObuKind::Data && self.decoder.is_some();
            if !forwards {
                if let Some(start) = run.take() {
                    if let Err(err) = self.decode(&buf[start..pos], strict, out) {
                        break Err(err);
                    }
                }
            }
            match kind {
                ObuKind::SequenceHeader if frame.redundant && self.decoder.is_some() => {}
                ObuKind::SequenceHeader => {
                    if let Err(err) = self.end_sequence(out) {
                        break Err(err);
                    }
                    self.descriptors.clear();
                    self.descriptors.extend_from_slice(obu);
                    self.refused = false;
                }
                // Mid-sequence descriptors are redundant copies; before the
                // first sequence header there is no sequence to add them to.
                ObuKind::Descriptor if self.decoder.is_none() && !self.descriptors.is_empty() => {
                    if self.descriptors.len() + obu.len() > MAX_DESCRIPTOR_BYTES {
                        self.descriptors.clear();
                        break Err("iamf: descriptors past their bound".into());
                    }
                    self.descriptors.extend_from_slice(obu);
                }
                ObuKind::Descriptor | ObuKind::Reserved => {}
                ObuKind::Data if forwards => {
                    run.get_or_insert(pos);
                }
                ObuKind::Data => {
                    // The first temporal unit closes the descriptors.
                    if !self.descriptors.is_empty() && !self.refused {
                        if let Err(err) = self.open_sequence() {
                            self.refused = true;
                            break Err(err);
                        }
                        run = Some(pos);
                    }
                }
            }
            pos += frame.len;
        };
        let result = result.and_then(|()| match run {
            Some(start) => self.decode(&buf[start..pos], strict, out),
            None => Ok(()),
        });
        self.buf = buf;
        self.buf.drain(..pos);
        if result.is_err() {
            // Whatever stopped the framer is not decodable: drop it rather
            // than failing on the same bytes at the next packet.
            self.buf.clear();
        }
        result
    }

    /// Build the decoder from the collected descriptors.
    fn open_sequence(&mut self) -> Result<(), String> {
        let mut settings = StreamSettings::default();
        settings.layout = OUTPUT_LAYOUT;
        settings.sample_type = Some(OutputSampleType::Int32LittleEndian);
        let decoder =
            StreamDecoder::new_from_descriptors(&self.descriptors, settings, &DefaultFactory)
                .map_err(|err| format!("iamf: cannot decode this sequence: {err}"))?;
        let (mix_id, _) = decoder.selected_mix();
        let description = describe(&self.descriptors);
        log::info!(
            "atmos-bridge: iamf sequence: {description}, mix {mix_id} rendered to 7.1.4 \
             at {} Hz",
            decoder.sample_rate()
        );
        self.description = Some(description);
        self.decoder = Some(SendDecoder(decoder));
        Ok(())
    }

    /// Flush the units still in the decoder and close the sequence.
    fn end_sequence(&mut self, out: &mut RVec<RDecodedFrame>) -> Result<(), String> {
        if let Some(decoder) = &mut self.decoder {
            decoder.get().signal_end_of_decoding();
            self.pull(out)?;
        }
        self.decoder = None;
        Ok(())
    }

    /// Hand data OBUs to the decoder and collect what it completes. A corrupt
    /// packet is only fatal in strict mode; otherwise the decoder drops its
    /// buffered units and picks up at the next temporal unit.
    fn decode(
        &mut self,
        obus: &[u8],
        strict: bool,
        out: &mut RVec<RDecodedFrame>,
    ) -> Result<(), String> {
        let Some(decoder) = self.decoder.as_mut().map(SendDecoder::get) else {
            return Ok(());
        };
        if let Err(err) = decoder.decode(obus) {
            let msg = format!("iamf: decode error: {err}");
            if strict {
                return Err(msg);
            }
            log::warn!("{msg}");
            decoder.reset();
            return Ok(());
        }
        self.pull(out)
    }

    fn pull(&mut self, out: &mut RVec<RDecodedFrame>) -> Result<(), String> {
        let Some(decoder) = self.decoder.as_mut().map(SendDecoder::get) else {
            return Ok(());
        };
        let channels = decoder.num_output_channels();
        if channels != OUTPUT_LABELS.len() {
            return Err(format!(
                "iamf: decoder renders {channels} channels, the bed has {}",
                OUTPUT_LABELS.len()
            ));
        }
        let sample_rate = decoder.sample_rate();
        loop {
            let unit = match decoder.get_output_temporal_unit() {
                Ok(Some(unit)) => unit,
                Ok(None) => return Ok(()),
                Err(err) => return Err(format!("iamf: render error: {err}")),
            };
            out.push(build_frame(&unit, channels, sample_rate));
            self.frame_count += 1;
        }
    }
}

/// One rendered temporal unit, s32le interleaved, as a bed frame.
fn build_frame(unit: &[u8], channels: usize, sample_rate: u32) -> RDecodedFrame {
    // The decoder's full scale is 2^31, the bridge's 2^23.
    let pcm: RVec<i32> = unit
        .chunks_exact(4)
        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> 8)
        .collect();
    let sample_count = pcm.len() / channels;
    RDecodedFrame {
        sampling_frequency: sample_rate,
        sample_count: sample_count as u32,
        channel_count: channels as u32,
        pcm,
        channel_labels: OUTPUT_LABELS.iter().copied().collect(),
        metadata: RVec::new(),
        drc_gain: 1.0,
        drc_ramp_duration: 0,
        dialogue_level: None.into(),
        is_new_segment: false,
    }
}

/// The codec and audio elements of a sequence, for the source label:
/// `IAMF (Opus) 7.1.4`, `IAMF (Opus) ambisonics 3rd order + stereo`.
fn describe(descriptors: &[u8]) -> String {
    let mut label = String::from("IAMF");
    let Ok(parsed) = Descriptors::collect(descriptors) else {
        return label;
    };
    if let Some(codec) = parsed.codec_configs.first() {
        label.push_str(match codec.codec_id {
            CodecId::Opus => " (Opus)",
            CodecId::AacLc => " (AAC)",
            CodecId::Flac => " (FLAC)",
            CodecId::Lpcm => " (PCM)",
            CodecId::Unknown(_) => "",
        });
    }
    let elements: Vec<String> = parsed
        .audio_elements
        .iter()
        .map(|element| match &element.config {
            AudioElementConfig::ChannelBased { layers } => layers
                .last()
                .map_or("channels", |layer| layer_name(layer.loudspeaker_layout))
                .to_owned(),
            AudioElementConfig::AmbisonicsMono {
                output_channel_count,
                ..
            }
            | AudioElementConfig::AmbisonicsProjection {
                output_channel_count,
                ..
            } => ambisonics_name(*output_channel_count),
        })
        .collect();
    if !elements.is_empty() {
        label.push(' ');
        label.push_str(&elements.join(" + "));
    }
    label
}

/// IAMF `loudspeaker_layout` (§3.7.4).
fn layer_name(layout: u8) -> &'static str {
    match layout {
        0 => "mono",
        1 => "stereo",
        2 => "5.1",
        3 => "5.1.2",
        4 => "5.1.4",
        5 => "7.1",
        6 => "7.1.2",
        7 => "7.1.4",
        8 => "3.1.2",
        9 => "binaural",
        _ => "expanded layout",
    }
}

fn ambisonics_name(channels: u8) -> String {
    // (order + 1)² channels.
    let order = (f32::from(channels).sqrt() as u8).saturating_sub(1);
    let suffix = match order {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    };
    format!("ambisonics {order}{suffix} order")
}

/// BS.2051 System J (4+7+0), the nominal angles of the bed the decoder
/// renders: the main layer at 0°, ±30°, ±90° and ±135°, the upper layer at
/// ±45° and ±135°, 30° up.
pub(crate) fn declared_poses() -> RVec<RChannelPose> {
    use RChannelLabel::*;
    [
        (C, 0.0, 0.0),
        (L, -30.0, 0.0),
        (R, 30.0, 0.0),
        (Ls, -90.0, 0.0),
        (Rs, 90.0, 0.0),
        (Lb, -135.0, 0.0),
        (Rb, 135.0, 0.0),
        (Tfl, -45.0, 30.0),
        (Tfr, 45.0, 30.0),
        (Tbl, -135.0, 30.0),
        (Tbr, 135.0, 30.0),
    ]
    .into_iter()
    .map(|(label, azimuth_deg, elevation_deg)| RChannelPose {
        label,
        azimuth_deg,
        elevation_deg,
    })
    .collect()
}

/// Raw-transport entry point: buffer `data`, decode what completes, and apply
/// the pipeline's failure policy (strict mode surfaces and resets).
pub(crate) fn push_iamf(bridge: &mut AtmosBridge, data: &[u8], result: &mut RPushResult) {
    let strict = bridge.strict;
    let state = &mut *bridge.iamf;
    state.buf.extend_from_slice(data);
    let mut frames = RVec::new();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        state.drain(strict, &mut frames)
    }));
    let decoded: u64 = frames.iter().map(|f| u64::from(f.sample_count)).sum();
    bridge.total_samples += decoded;
    result.frames.extend(frames);
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => {
            log::warn!("{msg}");
            // An unsupported sequence is reported once, then its audio is
            // dropped until the next sequence header.
            result.error_message = RString::from(msg);
            if strict {
                bridge.reset_pipeline();
                result.did_reset = true;
            }
        }
        Err(panic_info) => {
            let msg = panic_message(&panic_info);
            log::warn!("iamf: panic caught while decoding: {msg}. Resetting pipeline.");
            // The decoder's state is unknown after a panic: rebuild it from
            // the next sequence header.
            *bridge.iamf = IamfState::default();
            bridge.reset_pipeline();
            result.did_reset = true;
            if strict {
                result.error_message = RString::from(msg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_a_complete_obu() {
        // Temporal delimiter (type 4), empty payload.
        assert_eq!(
            frame_obu(&[4 << 3, 0x00, 0xAA]),
            Ok(Some(ObuFrame {
                obu_type: 4,
                redundant: false,
                len: 2
            }))
        );
        // Redundant mix presentation, 130-byte payload (two-byte leb128).
        let mut obu = vec![(2 << 3) | 0x04, 0x82, 0x01];
        obu.resize(3 + 130, 0);
        assert_eq!(
            frame_obu(&obu),
            Ok(Some(ObuFrame {
                obu_type: 2,
                redundant: true,
                len: 133
            }))
        );
    }

    #[test]
    fn waits_for_an_incomplete_obu() {
        assert_eq!(frame_obu(&[]), Ok(None));
        // Size field cut in the middle of its leb128.
        assert_eq!(frame_obu(&[5 << 3, 0x82]), Ok(None));
        // Payload one byte short.
        assert_eq!(frame_obu(&[5 << 3, 0x03, 1, 2]), Ok(None));
    }

    #[test]
    fn refuses_a_size_past_the_bound() {
        assert!(frame_obu(&[5 << 3, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]).is_err());
        assert!(frame_obu(&[5 << 3, 0x80, 0x80, 0x80, 0x10]).is_err());
    }

    #[test]
    fn classifies_obu_types() {
        assert_eq!(obu_kind(31), ObuKind::SequenceHeader);
        assert_eq!(obu_kind(0), ObuKind::Descriptor);
        assert_eq!(obu_kind(2), ObuKind::Descriptor);
        assert_eq!(obu_kind(3), ObuKind::Data);
        assert_eq!(obu_kind(4), ObuKind::Data);
        assert_eq!(obu_kind(23), ObuKind::Data);
        assert_eq!(obu_kind(24), ObuKind::Data);
        assert_eq!(obu_kind(25), ObuKind::Reserved);
        assert_eq!(obu_kind(30), ObuKind::Reserved);
    }

    #[test]
    fn names_ambisonics_orders() {
        assert_eq!(ambisonics_name(1), "ambisonics 0th order");
        assert_eq!(ambisonics_name(4), "ambisonics 1st order");
        assert_eq!(ambisonics_name(16), "ambisonics 3rd order");
        assert_eq!(ambisonics_name(25), "ambisonics 4th order");
    }

    // ── Conformance vectors ─────────────────────────────────────────────
    //
    // The libiamf test vectors (AOMediaCodec/libiamf, `tests/`) are not
    // committed: point `HARLETTY_IAMF_VECTORS` at a directory holding them.
    // Each `.iamf` comes with `<name>_rendered_id_<mix>_sub_mix_0_layout_<n>.wav`
    // references, one per layout its mix declares.

    use crate::bridge::AtmosBridge;
    use abi_stable::std_types::RSlice;
    use bridge_api::{FormatBridge, RInputTransport};

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
    fn decode_raw(bridge: &mut AtmosBridge, stream: &[u8]) -> Vec<RDecodedFrame> {
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
        let mut bridge = AtmosBridge::new(false);
        let frames = decode_raw(&mut bridge, &std::fs::read(stream).unwrap());

        assert!(bridge.is_ready());
        assert!(!bridge.has_objects());
        assert_eq!(bridge.source_family().as_str(), "");
        assert_eq!(bridge.source_label().as_str(), "IAMF (PCM) 7.1.4");
        assert_eq!(bridge.fixed_channel_poses().len(), 11);
        for frame in &frames {
            assert_eq!(frame.channel_labels.as_slice(), OUTPUT_LABELS.as_slice());
            assert_eq!(frame.sampling_frequency, 48_000);
            assert!(frame.metadata.is_empty(), "a bed carries no metadata");
        }
        let (channels, expected) = read_wav_s16(&reference);
        assert_eq!(usize::from(channels), OUTPUT_LABELS.len());
        assert_eq!(interleaved(&frames), expected);
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
        let mut bridge = AtmosBridge::new(false);
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

        let mut whole = AtmosBridge::new(false);
        let all = interleaved(&decode_raw(&mut whole, &stream));

        let mut bridge = AtmosBridge::new(false);
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
        let channels = OUTPUT_LABELS.len();
        let units_after_seek = unit_starts.len() - unit_starts.len() / 2;
        let end_trim = unit_starts.len() * UNIT - all.len() / channels - PRE_SKIP;
        assert_eq!(resumed.len() / channels, units_after_seek * UNIT - end_trim);
        // Opus restarts from a cleared state, so the first units differ and
        // the rest reconverge to within rounding of the continuous decode.
        let tail = resumed.len() / 2;
        let psnr = psnr_db(&resumed[resumed.len() - tail..], &all[all.len() - tail..]);
        assert!(psnr > 90.0, "resumed tail PSNR {psnr:.1} dB");
    }

    #[test]
    fn data_before_any_sequence_header_is_dropped() {
        let mut state = IamfState::default();
        let mut out = RVec::new();
        // Temporal delimiter, then an audio frame (type 6), then a stray
        // codec config: nothing to decode yet, nothing collected.
        state
            .buf
            .extend_from_slice(&[4 << 3, 0x00, 6 << 3, 0x02, 0xAB, 0xCD, 0x00, 0x00]);
        assert_eq!(state.drain(false, &mut out), Ok(()));
        assert!(out.is_empty());
        assert!(state.descriptors.is_empty());
        assert!(state.buf.is_empty());
    }
}
