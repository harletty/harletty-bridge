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
//!
//! A mix that codes its dialogue as an element of its own (harlettizer's
//! `--voices-to-bed`: an M&E element and a `Dialogue` one) has that element
//! handed out by the decoder instead of rendered into the bed: its channels
//! follow the bed in the frame, in the element's own layout, tagged
//! `dialogue` (`FormatBridge::channel_tags`) so the renderer can set its
//! level. A renderer that ignores the tag sums them with the bed, which is
//! the mix again.

use abi_stable::std_types::{RString, RVec};
use bridge_api::{
    RChannelLabel, RChannelPose, RChannelTag, RDecodedFrame, REvent, RMetadataFrame, RNameUpdate,
    RObjectChannel, RPushResult,
};
use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::position::ObjectPosition;
use iamf_dec::presentation::Descriptors;
use iamf_dec::stream::{
    DecodedElement, DecodedObject, OutputSampleType, StreamDecoder, StreamSettings,
};
use iamf_obu::descriptors::{AudioElementConfig, CodecId, ElementGainOffset};

use crate::bridge::AtmosBridge;
use crate::frame_builders::float_to_pcm_i32;
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

/// Samples between two object positions: the IAMF decoder evaluates each
/// object's animated position this often (5.3 ms at 48 kHz), and the
/// renderer ramps between them.
const POSITION_INTERVAL: u32 = 256;

/// Position heartbeat: static objects are re-announced this often, so a
/// client joining mid-stream sees them.
const HEARTBEAT_HZ: u64 = 2;

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
    /// Objects the selected mix hands out (IAMF v2.0), 0 for a bed only.
    num_objects: usize,
    /// Absolute sample position of the next frame, for metadata events.
    sample_pos: u64,
    /// The temporal unit being pulled, interleaved: kept from one unit to
    /// the next so the decoder renders into the same buffer.
    unit: Vec<f32>,
    objects: ObjectState,
    /// The mix's dialogue element, handed out beside the bed.
    dialogue: Option<Dialogue>,
}

/// A dialogue element the decoder hands out rather than renders.
struct Dialogue {
    audio_element_id: u32,
    /// Its channels, in the decoder's rendering order.
    labels: &'static [RChannelLabel],
    /// What the mix calls it (`Dialogue`).
    label: String,
    /// The mix's `content_language` tag, if it has one.
    language: String,
}

/// What the renderer was last told about the objects, so declarations and
/// positions are only re-sent when they change.
#[derive(Default)]
struct ObjectState {
    /// The last object↔channel declaration sent.
    declared: Option<RVec<RObjectChannel>>,
    /// The last position sent per object (ADM cartesian).
    emitted: Vec<Option<[f64; 3]>>,
}

impl IamfState {
    /// Forget the stream position (seek, flush): buffered bytes and decoded
    /// state go, the sequence's configuration stays.
    pub(crate) fn reset(&mut self) {
        self.buf.clear();
        self.frame_count = 0;
        // The renderer forgets the objects on a reset: declare them again.
        self.objects = ObjectState::default();
        if let Some(decoder) = &mut self.decoder {
            decoder.get().reset();
        }
    }

    /// A sequence is configured: temporal units can be decoded as they come.
    pub(crate) fn has_sequence(&self) -> bool {
        self.decoder.is_some()
    }

    /// The decoded mix has objects (IAMF v2.0).
    pub(crate) fn has_objects(&self) -> bool {
        self.decoder.is_some() && self.num_objects > 0
    }

    /// `IAMF (Opus) ambisonics + stereo`, once a sequence decodes.
    pub(crate) fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// The dialogue channels, when the mix codes its dialogue apart: they
    /// follow the bed (or open the frame when there is no bed).
    pub(crate) fn channel_tags(&self) -> RVec<RChannelTag> {
        let (Some(decoder), Some(dialogue)) = (&self.decoder, &self.dialogue) else {
            return RVec::new();
        };
        let first = if decoder.0.has_rendered_elements() {
            OUTPUT_LABELS.len()
        } else {
            0
        };
        let mut tags = RVec::with_capacity(1);
        tags.push(RChannelTag {
            kind: RString::from("dialogue"),
            language: RString::from(dialogue.language.as_str()),
            label: RString::from(dialogue.label.as_str()),
            channels: (first..first + dialogue.labels.len())
                .map(|channel| channel as u32)
                .collect(),
        });
        tags
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
        // Objects are rendered by orender, not by the decoder.
        settings.object_passthrough = true;
        settings.object_position_interval = POSITION_INTERVAL;
        let mut decoder =
            StreamDecoder::new_from_descriptors(&self.descriptors, settings, &DefaultFactory)
                .map_err(|err| format!("iamf: cannot decode this sequence: {err}"))?;
        let (mix_id, _) = decoder.selected_mix();
        let description = describe(&self.descriptors);
        self.num_objects = decoder.num_objects();
        self.dialogue = find_dialogue(&self.descriptors, mix_id)
            .filter(|dialogue| decoder.split_element(dialogue.audio_element_id));
        log::info!(
            "atmos-bridge: iamf sequence: {description}, mix {mix_id}: {}{} at {} Hz",
            match (decoder.has_rendered_elements(), self.num_objects) {
                (true, 0) => "rendered to 7.1.4".to_owned(),
                (true, n) => format!("7.1.4 bed + {n} object(s)"),
                (false, n) => format!("{n} object(s)"),
            },
            match &self.dialogue {
                Some(dialogue) => format!(
                    " + dialogue element {} ({} ch)",
                    dialogue.audio_element_id,
                    dialogue.labels.len()
                ),
                None => String::new(),
            },
            decoder.sample_rate()
        );
        self.objects = ObjectState::default();
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
        self.dialogue = None;
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
        let bed = decoder.has_rendered_elements();
        loop {
            match decoder.get_output_temporal_unit_f32(&mut self.unit) {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(err) => return Err(format!("iamf: render error: {err}")),
            }
            // Left in the decoder, which reuses their buffers.
            let objects = decoder.objects();
            let dialogue = self.dialogue.as_ref().and_then(|dialogue| {
                let element = decoder
                    .elements()
                    .iter()
                    .find(|e| e.audio_element_id == dialogue.audio_element_id)?;
                Some((element, dialogue.labels))
            });
            let mut frame = build_frame(
                &self.unit,
                bed.then_some(channels),
                dialogue,
                objects,
                sample_rate,
            );
            let first_object_channel = frame.channel_count as usize - objects.len();
            frame.metadata = object_metadata(
                objects,
                first_object_channel,
                self.sample_pos,
                sample_rate,
                frame.sample_count as usize,
                &mut self.objects,
            );
            self.sample_pos += u64::from(frame.sample_count);
            out.push(frame);
            self.frame_count += 1;
        }
    }
}

/// A rendered bed sample at the bridge's 24-bit scale: the decoder's own
/// 32-bit quantization (`iamf_dec::post::quantize_s32`, full scale 2^31,
/// ties to even) shifted down to 2^23, which is what the bed has always
/// been — computed in f32, four samples at a time, instead of through f64.
///
/// With `y` the sample at the 24-bit scale, rounding at 32 bits then
/// flooring the shift is `floor(y + 2^-9)`: the 32-bit rounding only shows
/// when it carries into bit 8, a tie never does, and past 2^15 `y` has no
/// bits below 2^-8 to round. The sum is not exact in f32, so it is not
/// formed: `y` is split into its integer part and an exact remainder, and
/// the remainder compared with the two thresholds.
#[inline]
fn bed_to_pcm_i32(sample: f32) -> i32 {
    const CARRY: f32 = 1.0 - 1.0 / 512.0;
    const BORROW: f32 = -1.0 / 512.0;
    let scaled = sample * 8_388_608.0;
    // NaN is silence, as the decoder's quantizer has it.
    let scaled = if scaled.is_nan() {
        0.0
    } else {
        scaled.clamp(-8_388_608.0, 8_388_607.0)
    };
    // `as i32` saturates, which baseline x86-64 only does one sample at a
    // time; nothing is left to saturate.
    // SAFETY: `scaled` is not NaN and lies within ±2^23, well inside i32.
    let whole = unsafe { scaled.to_int_unchecked::<i32>() };
    let rest = scaled - whole as f32;
    whole + i32::from(rest >= CARRY) - i32::from(rest < BORROW)
}

/// One temporal unit as a frame: the rendered bed (interleaved, when the
/// mix has elements rendered into it), the dialogue element's channels when
/// it is handed out, then one channel per object.
fn build_frame(
    unit: &[f32],
    bed_channels: Option<usize>,
    dialogue: Option<(&DecodedElement, &'static [RChannelLabel])>,
    objects: &[DecodedObject],
    sample_rate: u32,
) -> RDecodedFrame {
    let bed = bed_channels.unwrap_or(0);
    let sample_count = match (bed_channels, dialogue) {
        (Some(channels), _) => unit.len() / channels,
        (None, Some((element, _))) => element.planes.first().map_or(0, Vec::len),
        (None, None) => objects.first().map_or(0, |o| o.samples.len()),
    };
    let dialogue_channels = dialogue.map_or(0, |(_, labels)| labels.len());
    let channels = bed + dialogue_channels + objects.len();
    let first_object = bed + dialogue_channels;
    let mut pcm: Vec<i32>;
    if objects.is_empty() && dialogue.is_none() {
        pcm = Vec::with_capacity(sample_count * channels);
        pcm.extend(
            unit[..sample_count * channels]
                .iter()
                .map(|&sample| bed_to_pcm_i32(sample)),
        );
    } else {
        pcm = vec![0i32; sample_count * channels];
        if bed > 0 {
            for (out, source) in pcm.chunks_exact_mut(channels).zip(unit.chunks_exact(bed)) {
                for (out, &sample) in out.iter_mut().zip(source) {
                    *out = bed_to_pcm_i32(sample);
                }
            }
        }
        if let Some((element, _)) = dialogue {
            for (index, plane) in element.planes.iter().enumerate().take(dialogue_channels) {
                for (out, &sample) in pcm.chunks_exact_mut(channels).zip(plane) {
                    out[bed + index] = bed_to_pcm_i32(sample);
                }
            }
        }
        for (index, object) in objects.iter().enumerate() {
            for (out, &sample) in pcm.chunks_exact_mut(channels).zip(&object.samples) {
                out[first_object + index] = float_to_pcm_i32(sample);
            }
        }
    }
    let pcm: RVec<i32> = pcm.into();
    let mut channel_labels: RVec<RChannelLabel> = RVec::with_capacity(channels);
    if bed_channels.is_some() {
        channel_labels.extend(OUTPUT_LABELS.iter().copied());
    }
    if let Some((_, labels)) = dialogue {
        channel_labels.extend(labels.iter().copied());
    }
    channel_labels.extend(std::iter::repeat_n(RChannelLabel::Object, objects.len()));
    RDecodedFrame {
        sampling_frequency: sample_rate,
        sample_count: sample_count as u32,
        channel_count: channels as u32,
        pcm,
        channel_labels,
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
            AudioElementConfig::ObjectBased { .. } => String::new(),
        })
        .filter(|name| !name.is_empty())
        .collect();
    let objects: usize = parsed
        .audio_elements
        .iter()
        .map(|element| match element.config {
            AudioElementConfig::ObjectBased { num_objects } => usize::from(num_objects),
            _ => 0,
        })
        .sum();
    let mut parts = elements;
    match objects {
        0 => {}
        1 => parts.push("1 object".to_owned()),
        n => parts.push(format!("{n} objects")),
    }
    if !parts.is_empty() {
        label.push(' ');
        label.push_str(&parts.join(" + "));
    }
    label
}

/// An IAMF object position in the renderer's cartesian (ADM) coordinates.
/// IAMF's cartesian positions already are (x right, y front, z up); its
/// polar azimuth counts positive to the left, ADM's x to the right.
fn adm_position(position: ObjectPosition) -> [f64; 3] {
    match position {
        ObjectPosition::Cartesian { x, y, z } => [f64::from(x), f64::from(y), f64::from(z)],
        ObjectPosition::Polar {
            azimuth,
            elevation,
            distance,
        } => {
            let azimuth = f64::from(azimuth).to_radians();
            let elevation = f64::from(elevation).to_radians();
            let distance = f64::from(distance);
            let horizontal = distance * elevation.cos();
            [
                horizontal * (-azimuth).sin(),
                horizontal * azimuth.cos(),
                distance * elevation.sin(),
            ]
        }
    }
}

/// The metadata frame of one unit's objects: the object↔channel
/// declaration when it changed (or after a reset), and their positions at
/// every point where one of them moved, plus a heartbeat. Every batch of
/// events carries all the objects, since the renderer clears the ones a
/// metadata frame leaves out. A bed-only unit has none.
fn object_metadata(
    objects: &[DecodedObject],
    first_channel: usize,
    frame_pos: u64,
    sample_rate: u32,
    frame_len: usize,
    state: &mut ObjectState,
) -> RVec<RMetadataFrame> {
    if objects.is_empty() {
        return RVec::new();
    }
    let current: RVec<RObjectChannel> = (0..objects.len())
        .map(|i| RObjectChannel {
            id: i as u32,
            channel: (first_channel + i) as u32,
        })
        .collect();
    let declaration_changed = state.declared.as_deref() != Some(current.as_slice());
    if state.emitted.len() != objects.len() {
        state.emitted = vec![None; objects.len()];
    }
    let heartbeat_period = u64::from(sample_rate) / HEARTBEAT_HZ;
    let heartbeat_due = heartbeat_period > 0 && frame_pos % heartbeat_period < frame_len as u64;

    let mut events: RVec<REvent> = RVec::new();
    for k in 0..objects[0].positions.len() {
        let offset = objects[0].positions[k].0;
        let at: Vec<[f64; 3]> = objects
            .iter()
            .map(|o| {
                o.positions
                    .get(k)
                    .map_or([0.0, 1.0, 0.0], |p| adm_position(p.1))
            })
            .collect();
        let moved = at
            .iter()
            .zip(&state.emitted)
            .any(|(p, last)| last.is_none_or(|l| (0..3).any(|i| (p[i] - l[i]).abs() > 1e-6)));
        let announce = k == 0 && (declaration_changed || heartbeat_due);
        if !(moved || announce) {
            continue;
        }
        for (id, (pos, last)) in at.iter().zip(state.emitted.iter_mut()).enumerate() {
            events.push(REvent {
                id: id as u32,
                sample_pos: frame_pos + u64::from(offset),
                has_pos: true,
                pos: *pos,
                gain_db: 0,
                size: [0.0; 3],
                // A first sighting jumps there; a move ramps over the
                // interval the decoder evaluated it at.
                ramp_duration: if last.is_some() { POSITION_INTERVAL } else { 0 },
            });
            *last = Some(*pos);
        }
    }
    if !declaration_changed && events.is_empty() {
        return RVec::new();
    }
    let (object_channels, name_updates) = if declaration_changed {
        let names = objects
            .iter()
            .enumerate()
            .map(|(i, o)| RNameUpdate {
                id: i as u32,
                name: RString::from(if o.index == 0 {
                    format!("IAMF {}", o.audio_element_id)
                } else {
                    format!("IAMF {}.{}", o.audio_element_id, o.index)
                }),
            })
            .collect();
        (
            crate::metadata::declare_object_channels(&mut state.declared, current),
            names,
        )
    } else {
        (RVec::new(), RVec::new())
    };
    let mut frames = RVec::with_capacity(1);
    frames.push(RMetadataFrame {
        events,
        object_channels,
        channel_gains: RVec::new(),
        name_updates,
        sample_pos: frame_pos,
        ramp_duration: 0,
    });
    frames
}

/// The mix's dialogue element, if it codes one apart: a channel-based
/// element beside others in the selected mix, annotated as dialogue (what
/// harlettizer's `--voices-to-bed` writes: `Dialogue`) or, with no such
/// annotation, the only one a listener may adjust (an element gain offset
/// of type RANGE, IAMF v2.0's mark of a dialogue level control).
fn find_dialogue(descriptors: &[u8], mix_id: u32) -> Option<Dialogue> {
    let parsed = Descriptors::collect(descriptors).ok()?;
    let mix = parsed
        .mix_presentations
        .iter()
        .find(|mix| mix.mix_presentation_id == mix_id)?;
    let elements = &mix.sub_mixes.first()?.elements;
    if elements.len() < 2 {
        return None;
    }
    // A channel-based element whose top layer the renderer has labels for.
    let candidates: Vec<(&_, &'static [RChannelLabel])> = elements
        .iter()
        .filter_map(|sub| {
            let element = parsed
                .audio_elements
                .iter()
                .find(|e| e.audio_element_id == sub.audio_element_id)?;
            let AudioElementConfig::ChannelBased { layers } = &element.config else {
                return None;
            };
            let top = layers.last()?;
            if top.expanded_loudspeaker_layout.is_some() {
                return None;
            }
            Some((sub, layout_labels(top.loudspeaker_layout)?))
        })
        .collect();
    let is_dialogue = |annotation: &String| annotation.to_ascii_lowercase().contains("dialog");
    let (sub, labels) = match candidates
        .iter()
        .find(|(sub, _)| sub.localized_annotations.iter().any(is_dialogue))
    {
        Some(found) => *found,
        None => {
            let mut adjustable = candidates.iter().filter(|(sub, _)| {
                matches!(
                    sub.element_gain_offset,
                    Some(ElementGainOffset::Range { .. })
                )
            });
            let only = adjustable.next()?;
            if adjustable.next().is_some() {
                return None;
            }
            *only
        }
    };
    Some(Dialogue {
        audio_element_id: sub.audio_element_id,
        labels,
        label: sub
            .localized_annotations
            .first()
            .cloned()
            .unwrap_or_default(),
        language: mix
            .tags
            .iter()
            .find(|(name, _)| name == "content_language")
            .map(|(_, value)| value.clone())
            .unwrap_or_default(),
    })
}

/// The channels of an IAMF `loudspeaker_layout` (§3.7.4) in the decoder's
/// rendering order (§7.2 `channel_layout`), as renderer labels. The upper
/// pair of the x.y.2 layouts and of 3.1.2 is the front one of the 7.1.4
/// bed, 30° up.
fn layout_labels(layout: u8) -> Option<&'static [RChannelLabel]> {
    use RChannelLabel::*;
    Some(match layout {
        0 => &[C],
        1 => &[L, R],
        2 => &[L, R, C, LFE, Ls, Rs],
        3 => &[L, R, C, LFE, Ls, Rs, Tfl, Tfr],
        4 => &[L, R, C, LFE, Ls, Rs, Tfl, Tfr, Tbl, Tbr],
        5 => &[L, R, C, LFE, Ls, Rs, Lb, Rb],
        6 => &[L, R, C, LFE, Ls, Rs, Lb, Rb, Tfl, Tfr],
        7 => &[L, R, C, LFE, Ls, Rs, Lb, Rb, Tfl, Tfr, Tbl, Tbr],
        8 => &[L, R, C, LFE, Tfl, Tfr],
        _ => return None,
    })
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
    // Metadata events are stamped on the bridge's running sample position.
    state.sample_pos = bridge.total_samples;
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

    /// All metadata events of a decode, in order.
    fn events(frames: &[RDecodedFrame]) -> Vec<REvent> {
        frames
            .iter()
            .flat_map(|f| f.metadata.iter().flat_map(|m| m.events.iter().cloned()))
            .collect()
    }

    fn close(a: [f64; 3], b: [f64; 3]) -> bool {
        (0..3).all(|i| (a[i] - b[i]).abs() < 1e-3)
    }

    #[test]
    fn an_object_only_mix_reaches_the_renderer_as_objects() {
        // IAMF v2.0 base-advanced: one polar object, step at the front, then
        // inter-linear to the left, the rear and the right over 1024-sample
        // units, then the default (front) again.
        let Some(stream) = vector("test_000800.iamf") else {
            eprintln!("skipping: set HARLETTY_IAMF_VECTORS to the libiamf test vectors");
            return;
        };
        let mut bridge = AtmosBridge::new(false);
        let frames = decode_raw(&mut bridge, &std::fs::read(stream).unwrap());
        assert!(bridge.has_objects());
        assert_eq!(bridge.source_label().as_str(), "IAMF (PCM) 1 object");
        for frame in &frames {
            // No bed: the mix renders nothing in the decoder.
            assert_eq!(frame.channel_labels.as_slice(), [RChannelLabel::Object]);
        }
        let first = &frames[0].metadata[0];
        assert_eq!(first.object_channels.len(), 1);
        assert_eq!(
            (
                first.object_channels[0].id,
                first.object_channels[0].channel
            ),
            (0, 0)
        );
        assert_eq!(first.name_updates[0].name.as_str(), "IAMF 300");

        let events = events(&frames);
        let at = |sample: u64| {
            events
                .iter()
                .rev()
                .find(|e| e.sample_pos == sample)
                .unwrap_or_else(|| panic!("no event at {sample}"))
                .pos
        };
        let h = std::f64::consts::FRAC_1_SQRT_2;
        assert!(close(at(0), [0.0, 1.0, 0.0]), "front {:?}", at(0));
        // Halfway to the left: azimuth +45 (IAMF, positive left) is x < 0.
        assert!(close(at(1024 + 512), [-h, h, 0.0]), "{:?}", at(1536));
        assert!(close(at(2048), [-1.0, 0.0, 0.0]), "left {:?}", at(2048));
        assert!(close(at(3072), [0.0, -1.0, 0.0]), "rear {:?}", at(3072));
        // A move ramps over the position interval; the first sighting jumps.
        assert_eq!(events[0].ramp_duration, 0);
        assert!(events.iter().any(|e| e.ramp_duration == POSITION_INTERVAL));

        // The object's PCM is the source's first channel through the
        // element's -3 dB mix gain.
        let source = vector("dialog_clip_stereo.wav").expect("the vectors' source");
        let (channels, expected) = read_wav_s16(&source);
        let gain = iamf_dec::params::q78_db_to_linear(-768);
        let expected: Vec<i32> = expected
            .chunks(usize::from(channels))
            .map(|c| float_to_pcm_i32((c[0] >> 8) as f32 / 32768.0 * gain))
            .collect();
        let ours = interleaved(&frames);
        let n = ours.len().min(expected.len());
        assert!(n > 100_000);
        assert_eq!(ours[..n], expected[..n]);
    }

    #[test]
    fn a_mixed_mix_carries_the_bed_and_the_objects() {
        // IAMF v2.0 advanced-1: a 5.1 element and four static polar objects.
        let Some(stream) = vector("test_000903.iamf") else {
            eprintln!("skipping: set HARLETTY_IAMF_VECTORS to the libiamf test vectors");
            return;
        };
        let mut bridge = AtmosBridge::new(false);
        let stream = std::fs::read(stream).unwrap();
        let frames = decode_raw(&mut bridge, &stream);
        assert!(bridge.has_objects());
        assert_eq!(bridge.source_label().as_str(), "IAMF (PCM) 5.1 + 4 objects");
        let labels = frames[0].channel_labels.as_slice();
        assert_eq!(labels[..12], OUTPUT_LABELS);
        assert_eq!(labels[12..], [RChannelLabel::Object; 4]);
        let declared = &frames[0].metadata[0].object_channels;
        assert_eq!(
            declared
                .iter()
                .map(|d| (d.id, d.channel))
                .collect::<Vec<_>>(),
            [(0, 12), (1, 13), (2, 14), (3, 15)]
        );
        // Static objects: announced at the start, then only on the 2 Hz
        // heartbeat, all four every time.
        let events = events(&frames);
        let seconds = frames
            .iter()
            .map(|f| f64::from(f.sample_count))
            .sum::<f64>()
            / 48_000.0;
        let batches = events.len() / 4;
        assert_eq!(events.len() % 4, 0);
        assert!(
            batches as f64 <= seconds * HEARTBEAT_HZ as f64 + 2.0,
            "{batches} batches over {seconds:.1} s"
        );
        // Azimuth 1, elevation 2, distance 3/127: just left of the front,
        // close to the listener.
        let d = 3.0 / 127.0;
        let (az, el) = (1f64.to_radians(), 2f64.to_radians());
        let expected = [
            d * el.cos() * (-az).sin(),
            d * el.cos() * az.cos(),
            d * el.sin(),
        ];
        assert!(events.iter().all(|e| close(e.pos, expected)));

        // A reset declares the objects again.
        bridge.reset();
        let after = decode_raw(&mut bridge, &stream);
        assert_eq!(after[0].metadata[0].object_channels.len(), 4);
    }

    #[test]
    fn a_dialogue_element_follows_the_bed_tagged() {
        // Base-advanced: two stereo LPCM elements, the first adjustable
        // (element gain offset of type RANGE) and neither annotated — the
        // adjustable one is the dialogue.
        let Some(path) = vector("test_000854.iamf") else {
            eprintln!("skipping: set HARLETTY_IAMF_VECTORS to the libiamf test vectors");
            return;
        };
        let stream = std::fs::read(path).unwrap();
        let mut bridge = AtmosBridge::new(false);
        let frames = decode_raw(&mut bridge, &stream);
        assert!(!frames.is_empty());
        assert!(!bridge.has_objects());
        let labels = frames[0].channel_labels.as_slice();
        assert_eq!(labels[..12], OUTPUT_LABELS);
        assert_eq!(labels[12..], [RChannelLabel::L, RChannelLabel::R]);
        let tags = bridge.channel_tags();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].kind.as_str(), "dialogue");
        assert_eq!(tags[0].label.as_str(), "test_sub_mix_0_audio_element_0");
        assert_eq!(tags[0].channels.as_slice(), [12, 13]);

        // Rendered to the bed and added back, the dialogue gives the mix.
        let mut settings = StreamSettings::default();
        settings.layout = OUTPUT_LAYOUT;
        // The v2.0 profiles, as the bridge asks for them.
        settings.object_passthrough = true;
        let mut whole =
            StreamDecoder::new_from_descriptors(&stream, settings, &DefaultFactory).unwrap();
        let mut mix = Vec::new();
        let mut unit = Vec::new();
        whole.decode(&stream).unwrap();
        while whole.get_output_temporal_unit_f32(&mut unit).unwrap() {
            mix.extend_from_slice(&unit);
        }
        let scale = 1.0 / 8_388_608.0;
        let mut ours = Vec::with_capacity(mix.len());
        let mut peak = 0f32;
        for frame in &frames {
            let n = frame.sample_count as usize;
            let planes: Vec<Vec<f32>> = (12..14)
                .map(|c| {
                    (0..n)
                        .map(|s| frame.pcm[s * 14 + c] as f32 * scale)
                        .collect()
                })
                .collect();
            let info = iamf_dec::layout::loudspeaker_info(1).unwrap();
            let rendered = iamf_dec::render::render(
                &iamf_dec::reconstruct::Reconstructed::Channels {
                    matrix: info.matrix,
                    planar: planes,
                },
                OUTPUT_LAYOUT.matrix_layout(),
            )
            .unwrap();
            for s in 0..n {
                for c in 0..12 {
                    let dialogue = rendered[c][s];
                    peak = peak.max(dialogue.abs());
                    ours.push(frame.pcm[s * 14 + c] as f32 * scale + dialogue);
                }
            }
        }
        assert_eq!(ours.len(), mix.len());
        assert!(peak > 0.01, "the dialogue is silent");
        let worst = ours
            .iter()
            .zip(&mix)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(worst < 4.0 * scale, "bed + dialogue differs by {worst}");
    }

    #[test]
    fn a_mix_of_unmarked_elements_tags_nothing() {
        // Two-layer 5.1 + stereo, neither annotated nor adjustable: the bed
        // as before.
        let Some(path) = vector("test_000087.iamf") else {
            eprintln!("skipping: set HARLETTY_IAMF_VECTORS to the libiamf test vectors");
            return;
        };
        let stream = std::fs::read(path).unwrap();
        let mut bridge = AtmosBridge::new(false);
        let frames = decode_raw(&mut bridge, &stream);
        assert_eq!(frames[0].channel_labels.as_slice(), OUTPUT_LABELS);
        assert!(bridge.channel_tags().is_empty());
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

    /// The f32 formulation is the decoder's 32-bit quantization shifted
    /// down, for every input: the carries around each 24-bit step, the
    /// clamp edges, non-finite values and a strided sweep of all f32s.
    #[test]
    fn bed_samples_are_the_decoder_s32_shifted_down() {
        let check = |sample: f32| {
            assert_eq!(
                bed_to_pcm_i32(sample),
                iamf_dec::post::quantize_s32(sample) >> 8,
                "sample {sample:e} ({:#010x})",
                sample.to_bits()
            );
        };
        // Around every 2^-9 of a 24-bit step near zero, where the 32-bit
        // rounding carries, and their float neighbours.
        for n in -300_000i32..=300_000 {
            let sample = n as f32 / (8_388_608.0 * 1024.0);
            check(sample);
            check(f32::from_bits(sample.to_bits() + 1));
            check(f32::from_bits(sample.to_bits().wrapping_sub(1)));
        }
        for n in -70_000i32..=70_000 {
            // Halves of a 32-bit step: the ties.
            check(n as f32 / 4_294_967_296.0);
            // Around 2^15 steps, where f32 stops resolving 2^-9 of a step.
            for base in [32_767.0f32, 32_768.0, 16_384.0, 8_388_607.0] {
                check((base + n as f32 / 1024.0) / 8_388_608.0);
                check(-(base + n as f32 / 1024.0) / 8_388_608.0);
            }
        }
        for sample in [
            0.0,
            -0.0,
            1.0,
            -1.0,
            0.999_999_94,
            -0.999_999_94,
            1.000_000_1,
            2.0,
            -2.0,
            1.0e30,
            -1.0e30,
            f32::MIN_POSITIVE,
            -f32::MIN_POSITIVE,
            f32::MAX,
            f32::MIN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
        ] {
            check(sample);
        }
        for bits in (0..=u32::MAX).step_by(4_093) {
            check(f32::from_bits(bits));
        }
    }
}
