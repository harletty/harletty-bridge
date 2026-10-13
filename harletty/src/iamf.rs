//! IAMF (AOMedia Immersive Audio Model and Formats) input: a standalone OBU
//! stream — an IA sequence header, its descriptors, then temporal units — read
//! in arbitrary chunks and decoded through iamf-rs.
//!
//! The sibling of `bridge/src/iamf_pipeline.rs`, which does the same job for
//! the realtime path, and the reference for what is done here: the same
//! framing, the same decoder settings (objects handed out rather than
//! rendered, every other element rendered to BS.2051 System J), the same
//! coordinate conventions. The framer is duplicated rather than shared: it is
//! a few lines, and the CLI never depends on the bridge crate.
//!
//! Temporal delimiters are optional in IAMF and nothing here relies on them:
//! a stream read back from Matroska carries none, and the decoder finds the
//! unit boundaries from the audio frames themselves.

use anyhow::{Result, anyhow, bail};
use iamf_codecs::DefaultFactory;
use iamf_dec::MatrixLayout;
use iamf_dec::layout::{SoundSystem, expanded_info, loudspeaker_info};
use iamf_dec::position::ObjectPosition;
use iamf_dec::presentation::Descriptors;
use iamf_dec::reconstruct::Reconstructed;
use iamf_dec::stream::{DecodedObject, OutputSampleType, StreamDecoder, StreamSettings};
use iamf_obu::descriptors::{AudioElementConfig, CodecId};

use crate::codec_probe::iamf_profile_name as profile_name;

/// The layout every non-object element is rendered to.
const OUTPUT_LAYOUT: SoundSystem = SoundSystem::J;

/// System J's channels in the decoder's IAMF order: L, R, C, LFE, Lss, Rss,
/// Lrs, Rrs, Ltf, Rtf, Ltb, Rtb.
pub const BED_CHANNELS: usize = 12;

/// Index of the LFE among [`BED_CHANNELS`].
const BED_LFE: usize = 3;

/// Samples between two object positions: the decoder evaluates each object's
/// animated position this often (5.3 ms at 48 kHz).
pub const POSITION_INTERVAL: u32 = 256;

/// Bytes the framer may hold without completing an OBU. An OBU's size is a
/// leb128 the stream states up front, so a value past this is garbage, not a
/// large OBU still arriving.
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

/// What the reader does with an OBU, by type.
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
fn frame_obu(data: &[u8]) -> Result<Option<ObuFrame>> {
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
            bail!("iamf: obu_size leb128 longer than 8 bytes");
        }
    }
    if size as usize > MAX_OBU_BYTES {
        bail!("iamf: obu_size {size} past the framing bound");
    }
    let len = 1 + size_len + size as usize;
    if data.len() < len {
        return Ok(None);
    }
    Ok(Some(ObuFrame {
        obu_type: header >> 3,
        redundant: header & 0x04 != 0,
        len,
    }))
}

/// One IA sequence as the decoder will hand it out.
#[derive(Clone, Debug, PartialEq)]
pub struct Sequence {
    /// `primary_profile` and `additional_profile` of the sequence header.
    pub primary_profile: u8,
    pub additional_profile: u8,
    /// The codec of the first codec config: `FLAC`, `Opus`, `AAC`, `PCM`.
    pub codec: &'static str,
    /// The mix presentation decoded.
    pub mix_id: u32,
    /// Audio elements of that mix.
    pub elements: usize,
    /// Objects it hands out per temporal unit (IAMF v2.0), in mix order.
    pub objects: usize,
    /// Indices in [`BED_CHANNELS`] of the System J channels the mix's other
    /// elements render into, ascending: what the master set's bed holds.
    /// Empty when the mix has nothing but objects.
    pub bed: Vec<usize>,
    /// From the codec config; 0 when it only shows once a frame decodes
    /// (AAC).
    pub sample_rate: u32,
    /// `IAMF (FLAC) LFE + 11 objects`, for the log.
    pub description: String,
}

/// One decoded temporal unit, as the decoder left it.
pub struct Unit<'a> {
    /// The rendered bed, interleaved in System J's [`BED_CHANNELS`]; `None`
    /// when the mix has nothing but objects.
    pub bed: Option<&'a [f32]>,
    /// The objects, in mix order: mono samples, positions at sample offsets
    /// into them every [`POSITION_INTERVAL`], and the position subblocks
    /// that start in the unit.
    pub objects: &'a [DecodedObject],
    /// Samples per channel.
    pub samples: usize,
    pub sample_rate: u32,
}

/// What a decode hands its consumer.
pub trait Sink {
    /// A sequence opens: before its first unit.
    fn sequence(&mut self, sequence: &Sequence) -> Result<()>;
    /// A temporal unit decoded.
    fn unit(&mut self, unit: Unit<'_>) -> Result<()>;
    /// The decoder dropped buffered units after a decode error and resumes
    /// at the next temporal unit.
    fn discontinuity(&mut self) {}
}

/// Frames an IAMF OBU stream pushed in arbitrary chunks and decodes it.
pub struct IamfReader {
    /// Bytes not yet framed into complete OBUs.
    buf: Vec<u8>,
    /// Descriptor OBUs of the IA sequence being opened, from its sequence
    /// header up to its first temporal unit.
    descriptors: Vec<u8>,
    decoder: Option<StreamDecoder>,
    /// A decode error ends the decode instead of dropping the unit.
    strict: bool,
    /// The temporal unit being pulled, interleaved: kept from one unit to
    /// the next so the decoder renders into the same buffer.
    unit: Vec<f32>,
    /// Sequences opened so far.
    pub sequences: u64,
    /// Temporal units decoded so far.
    pub units: u64,
    /// Bytes skipped before the first sequence header.
    pub skipped: u64,
}

impl IamfReader {
    pub fn new(strict: bool) -> Self {
        Self {
            buf: Vec::new(),
            descriptors: Vec::new(),
            decoder: None,
            strict,
            unit: Vec::new(),
            sequences: 0,
            units: 0,
            skipped: 0,
        }
    }

    /// Frame `data` with whatever was buffered before it, and decode every
    /// temporal unit that completes.
    pub fn push(&mut self, data: &[u8], sink: &mut dyn Sink) -> Result<()> {
        self.buf.extend_from_slice(data);
        let buf = std::mem::take(&mut self.buf);
        let mut pos = 0;
        let result = self.drain(&buf, &mut pos, sink);
        self.buf = buf;
        self.buf.drain(..pos);
        result
    }

    /// The input has ended: flush what the decoder still holds. Bytes that
    /// never made a whole OBU are a cut stream, reported and dropped.
    pub fn finish(&mut self, sink: &mut dyn Sink) -> Result<()> {
        if !self.buf.is_empty() {
            let msg = format!(
                "iamf: {} trailing bytes do not make a whole OBU",
                self.buf.len()
            );
            if self.strict {
                bail!(msg);
            }
            log::warn!("{msg}");
            self.buf.clear();
        }
        // A sequence that never reached its audio still has to be opened for
        // its consumer to know what it was.
        if self.decoder.is_none() && !self.descriptors.is_empty() {
            self.open_sequence(sink)?;
        }
        self.end_sequence(sink)
    }

    /// Frame and handle every complete OBU of `buf` from `pos`, leaving
    /// `pos` at the first byte not consumed.
    fn drain(&mut self, buf: &[u8], pos: &mut usize, sink: &mut dyn Sink) -> Result<()> {
        // Start of the contiguous data OBUs not yet handed to the decoder:
        // forwarded in one call rather than one per OBU.
        let mut run: Option<usize> = None;
        while let Some(frame) = frame_obu(&buf[*pos..])? {
            let obu = &buf[*pos..*pos + frame.len];
            let kind = obu_kind(frame.obu_type);
            let forwards = kind == ObuKind::Data && self.decoder.is_some();
            if !forwards {
                if let Some(start) = run.take() {
                    self.decode(&buf[start..*pos], sink)?;
                }
            }
            match kind {
                ObuKind::SequenceHeader if frame.redundant && self.decoder.is_some() => {}
                ObuKind::SequenceHeader => {
                    self.end_sequence(sink)?;
                    self.descriptors.clear();
                    self.descriptors.extend_from_slice(obu);
                }
                // Mid-sequence descriptors are redundant copies; before the
                // first sequence header there is no sequence to add them to.
                ObuKind::Descriptor if self.decoder.is_none() && !self.descriptors.is_empty() => {
                    if self.descriptors.len() + obu.len() > MAX_DESCRIPTOR_BYTES {
                        bail!("iamf: descriptors past their bound");
                    }
                    self.descriptors.extend_from_slice(obu);
                }
                ObuKind::Descriptor | ObuKind::Reserved => {}
                ObuKind::Data if forwards => {
                    run.get_or_insert(*pos);
                }
                ObuKind::Data if self.descriptors.is_empty() => {
                    // Before any sequence header: nothing to decode it with.
                    self.skipped += frame.len as u64;
                }
                ObuKind::Data => {
                    // The first temporal unit closes the descriptors.
                    self.open_sequence(sink)?;
                    run = Some(*pos);
                }
            }
            *pos += frame.len;
        }
        match run {
            Some(start) => self.decode(&buf[start..*pos], sink),
            None => Ok(()),
        }
    }

    /// Build the decoder from the collected descriptors.
    fn open_sequence(&mut self, sink: &mut dyn Sink) -> Result<()> {
        let mut settings = StreamSettings::default();
        settings.layout = OUTPUT_LAYOUT;
        settings.sample_type = Some(OutputSampleType::Int32LittleEndian);
        // Objects go to the master set as objects, not rendered.
        settings.object_passthrough = true;
        settings.object_position_interval = POSITION_INTERVAL;
        let decoder =
            StreamDecoder::new_from_descriptors(&self.descriptors, settings, &DefaultFactory)
                .map_err(|err| anyhow!("iamf: cannot decode this sequence: {err}"))?;
        let sequence = describe(&self.descriptors, &decoder)?;
        log::info!(
            "IAMF sequence: {}, mix {}, profile {}/{}, {} Hz",
            sequence.description,
            sequence.mix_id,
            profile_name(sequence.primary_profile),
            profile_name(sequence.additional_profile),
            sequence.sample_rate
        );
        self.sequences += 1;
        self.decoder = Some(decoder);
        sink.sequence(&sequence)
    }

    /// Flush the units still in the decoder and close the sequence.
    fn end_sequence(&mut self, sink: &mut dyn Sink) -> Result<()> {
        if let Some(decoder) = &mut self.decoder {
            decoder.signal_end_of_decoding();
            self.pull(sink)?;
        }
        self.decoder = None;
        Ok(())
    }

    /// Hand data OBUs to the decoder and pull what it completes. A corrupt
    /// packet is only fatal in strict mode; otherwise the decoder drops its
    /// buffered units and picks up at the next temporal unit.
    fn decode(&mut self, obus: &[u8], sink: &mut dyn Sink) -> Result<()> {
        let Some(decoder) = self.decoder.as_mut() else {
            return Ok(());
        };
        if let Err(err) = decoder.decode(obus) {
            if self.strict {
                bail!("iamf: decode error: {err}");
            }
            log::warn!("iamf: decode error, the units it held are dropped: {err}");
            decoder.reset();
            sink.discontinuity();
            return Ok(());
        }
        self.pull(sink)
    }

    fn pull(&mut self, sink: &mut dyn Sink) -> Result<()> {
        let Some(decoder) = self.decoder.as_mut() else {
            return Ok(());
        };
        let channels = decoder.num_output_channels();
        if channels != BED_CHANNELS {
            bail!("iamf: decoder renders {channels} channels, the bed has {BED_CHANNELS}");
        }
        let bed = decoder.has_rendered_elements();
        loop {
            match decoder.get_output_temporal_unit_f32(&mut self.unit) {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(err) if self.strict => bail!("iamf: render error: {err}"),
                Err(err) => {
                    log::warn!("iamf: render error, the units the decoder held are dropped: {err}");
                    decoder.reset();
                    sink.discontinuity();
                    return Ok(());
                }
            }
            // Left in the decoder, which reuses their buffers.
            let objects = decoder.objects();
            let samples = if bed {
                self.unit.len() / channels
            } else {
                objects.first().map_or(0, |o| o.samples.len())
            };
            self.units += 1;
            sink.unit(Unit {
                bed: bed.then_some(&self.unit[..samples * channels]),
                objects,
                samples,
                sample_rate: decoder.sample_rate(),
            })?;
        }
    }
}

/// What a sequence's selected mix holds.
fn describe(descriptors: &[u8], decoder: &StreamDecoder) -> Result<Sequence> {
    let parsed = Descriptors::collect(descriptors)
        .map_err(|err| anyhow!("iamf: unreadable descriptors: {err}"))?;
    let (mix_id, _) = decoder.selected_mix();
    let elements = parsed
        .mix_presentations
        .iter()
        .find(|mix| mix.mix_presentation_id == mix_id)
        .and_then(|mix| mix.sub_mixes.first())
        .map(|sub_mix| {
            sub_mix
                .elements
                .iter()
                .filter_map(|sub| {
                    parsed
                        .audio_elements
                        .iter()
                        .find(|e| e.audio_element_id == sub.audio_element_id)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut reached = [false; BED_CHANNELS];
    let mut names: Vec<String> = Vec::new();
    for element in &elements {
        match &element.config {
            AudioElementConfig::ChannelBased { layers } => {
                let Some(top) = layers.last() else { continue };
                names.push(
                    layer_name(top.loudspeaker_layout, top.expanded_loudspeaker_layout).to_owned(),
                );
                // The decoder renders the highest layer to System J (7.1.4
                // matches no lower one); the channels it reaches are those
                // its rendering matrix has a gain for. An expanded layout is
                // the only layer, rendered whole or, as a subset, through
                // the rows of its reference layout's matrix.
                let layout = if top.loudspeaker_layout == 15 {
                    top.expanded_loudspeaker_layout
                        .and_then(expanded_info)
                        .map(|info| (info.matrix, info.rows, info.channels))
                } else {
                    loudspeaker_info(top.loudspeaker_layout)
                        .map(|info| (info.matrix, None, info.channels))
                };
                match layout {
                    Some((matrix, rows, channels)) => {
                        mark_rendered(matrix, rows, channels, &mut reached);
                    }
                    None => reached = [true; BED_CHANNELS],
                }
            }
            AudioElementConfig::AmbisonicsMono {
                output_channel_count,
                ..
            }
            | AudioElementConfig::AmbisonicsProjection {
                output_channel_count,
                ..
            } => {
                names.push(ambisonics_name(*output_channel_count));
                // Rendered to every speaker; the LFE stays silent.
                for (index, slot) in reached.iter_mut().enumerate() {
                    *slot |= index != BED_LFE;
                }
            }
            AudioElementConfig::ObjectBased { .. } => {}
        }
    }
    let objects = decoder.num_objects();
    match objects {
        0 => {}
        1 => names.push("1 object".to_owned()),
        n => names.push(format!("{n} objects")),
    }
    let codec = match parsed.codec_configs.first().map(|c| c.codec_id) {
        Some(CodecId::Opus) => "Opus",
        Some(CodecId::AacLc) => "AAC",
        Some(CodecId::Flac) => "FLAC",
        Some(CodecId::Lpcm) => "PCM",
        Some(CodecId::Unknown(_)) | None => "unknown",
    };
    let header = parsed.sequence_header.as_ref();
    let mut description = format!("IAMF ({codec})");
    if !names.is_empty() {
        description.push(' ');
        description.push_str(&names.join(" + "));
    }
    Ok(Sequence {
        primary_profile: header.map_or(0, |h| h.primary_profile),
        additional_profile: header.map_or(0, |h| h.additional_profile),
        codec,
        mix_id,
        elements: elements.len(),
        objects,
        // Whether the decoder renders anything at all is its call; what it
        // reaches is read off the elements.
        bed: if decoder.has_rendered_elements() {
            (0..BED_CHANNELS).filter(|&i| reached[i]).collect()
        } else {
            Vec::new()
        },
        sample_rate: decoder.sample_rate(),
        description,
    })
}

/// Mark the System J channels a `channels`-channel layout of `matrix`
/// renders into, one input channel at a time so that no two gains can
/// cancel. `rows` are the matrix rows of a subset layout's channels.
fn mark_rendered(
    matrix: MatrixLayout,
    rows: Option<&'static [usize]>,
    channels: usize,
    reached: &mut [bool; BED_CHANNELS],
) {
    for input in 0..channels {
        let planar = (0..channels)
            .map(|i| vec![if i == input { 1.0 } else { 0.0 }])
            .collect();
        let probe = Reconstructed::Channels {
            matrix,
            rows,
            planar,
        };
        match iamf_dec::render::render(&probe, OUTPUT_LAYOUT.matrix_layout()) {
            Ok(rendered) => {
                for (slot, plane) in reached.iter_mut().zip(&rendered) {
                    *slot |= plane.iter().any(|&s| s != 0.0);
                }
            }
            // No matrix to System J: the decoder will refuse the element
            // anyway, so this only keeps the bed whole until it does.
            Err(_) => *reached = [true; BED_CHANNELS],
        }
    }
}

/// IAMF `loudspeaker_layout` (§3.6.2), and `expanded_loudspeaker_layout`
/// when it is 15.
fn layer_name(layout: u8, expanded: Option<u8>) -> &'static str {
    match layout {
        15 => match expanded {
            Some(0) => "LFE",
            Some(1) => "stereo-S",
            Some(2) => "stereo-SS",
            Some(3) => "stereo-RS",
            Some(4) => "stereo-TF",
            Some(5) => "stereo-TB",
            Some(6) => "top 4ch",
            Some(7) => "3.0",
            Some(8) => "9.1.6",
            Some(9) => "stereo-F",
            Some(10) => "stereo-Si",
            Some(11) => "stereo-TpSi",
            Some(12) => "top 6ch",
            Some(13) => "10.2.9.3",
            Some(14) => "LFE pair",
            Some(15) => "bottom 3ch",
            Some(16) => "7.1.5.4",
            Some(17) => "bottom 4ch",
            Some(18) => "top 1ch",
            Some(19) => "top 5ch",
            _ => "expanded layout",
        },
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

/// Steps per unit that position coordinates are rounded to: a fraction of a
/// step of the finest coding IAMF has (16-bit cartesian, 1/32767), and short
/// enough that a master set does not spell out the float noise of the
/// decoder's single-precision positions.
const POSITION_SCALE: f64 = 1e7;

/// An IAMF object position in DAMF (ADM) cartesian coordinates: x right,
/// y front, z up, each -1..=1. IAMF's cartesian positions already are; its
/// polar azimuth counts positive to the left, ADM's x to the right. The same
/// mapping as the bridge's.
pub fn adm_position(position: ObjectPosition) -> [f64; 3] {
    let raw = match position {
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
    };
    // Divided back rather than multiplied, so the value is the double
    // nearest the decimal and prints as it; `+ 0.0` turns a rounded -0 into
    // 0, which is how it is written.
    raw.map(|v| (v * POSITION_SCALE).round() / POSITION_SCALE + 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_a_complete_obu() {
        // Temporal delimiter (type 4), empty payload.
        assert_eq!(
            frame_obu(&[4 << 3, 0x00, 0xAA]).unwrap(),
            Some(ObuFrame {
                obu_type: 4,
                redundant: false,
                len: 2
            })
        );
        // Redundant mix presentation, 130-byte payload (two-byte leb128).
        let mut obu = vec![(2 << 3) | 0x04, 0x82, 0x01];
        obu.resize(3 + 130, 0);
        assert_eq!(
            frame_obu(&obu).unwrap(),
            Some(ObuFrame {
                obu_type: 2,
                redundant: true,
                len: 133
            })
        );
    }

    #[test]
    fn waits_for_an_incomplete_obu() {
        assert_eq!(frame_obu(&[]).unwrap(), None);
        // Size field cut in the middle of its leb128.
        assert_eq!(frame_obu(&[5 << 3, 0x82]).unwrap(), None);
        // Payload one byte short.
        assert_eq!(frame_obu(&[5 << 3, 0x03, 1, 2]).unwrap(), None);
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
        assert_eq!(obu_kind(24), ObuKind::Data);
        assert_eq!(obu_kind(25), ObuKind::Reserved);
        assert_eq!(obu_kind(30), ObuKind::Reserved);
    }

    #[test]
    fn polar_azimuth_is_positive_to_the_left() {
        let left = adm_position(ObjectPosition::Polar {
            azimuth: 90.0,
            elevation: 0.0,
            distance: 1.0,
        });
        assert_eq!(left, [-1.0, 0.0, 0.0]);
        let up = adm_position(ObjectPosition::Polar {
            azimuth: 0.0,
            elevation: 90.0,
            distance: 1.0,
        });
        assert_eq!(up, [0.0, 0.0, 1.0]);
        let cart = adm_position(ObjectPosition::Cartesian {
            x: 0.25,
            y: -0.5,
            z: 1.0,
        });
        assert_eq!(cart, [0.25, -0.5, 1.0]);
    }

    /// The bed holds the System J channels an element reaches, read off its
    /// rendering matrix: all of them for 7.1.4; for 5.1, whose surrounds sit
    /// at ±110°, the rear surrounds and no side or height.
    #[test]
    fn a_layout_reaches_the_channels_its_matrix_renders_into() {
        let mut reached = [false; BED_CHANNELS];
        let info = loudspeaker_info(7).unwrap();
        mark_rendered(info.matrix, None, info.channels, &mut reached);
        assert_eq!(reached, [true; BED_CHANNELS]);

        let mut reached = [false; BED_CHANNELS];
        let info = loudspeaker_info(2).unwrap();
        mark_rendered(info.matrix, None, info.channels, &mut reached);
        // L, R, C, LFE, Lss, Rss, Lrs, Rrs, then the four heights.
        assert_eq!(
            reached,
            [
                true, true, true, true, false, false, true, true, false, false, false, false
            ]
        );

        // An expanded subset reaches what its reference layout's rows do:
        // the LFE of 7.1.4 alone; the top front pair of 7.1.4.
        let mut reached = [false; BED_CHANNELS];
        let info = expanded_info(0).unwrap();
        mark_rendered(info.matrix, info.rows, info.channels, &mut reached);
        let mut lfe = [false; BED_CHANNELS];
        lfe[BED_LFE] = true;
        assert_eq!(reached, lfe);

        let mut reached = [false; BED_CHANNELS];
        let info = expanded_info(4).unwrap();
        mark_rendered(info.matrix, info.rows, info.channels, &mut reached);
        assert_eq!(
            reached,
            [
                false, false, false, false, false, false, false, false, true, true, false, false
            ]
        );
    }

    #[test]
    fn names_ambisonics_orders() {
        assert_eq!(ambisonics_name(4), "ambisonics 1st order");
        assert_eq!(ambisonics_name(16), "ambisonics 3rd order");
    }
}
