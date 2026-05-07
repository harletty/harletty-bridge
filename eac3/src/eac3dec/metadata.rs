// SPDX-License-Identifier: Apache-2.0

use super::bitstream::BitReader;
use super::syncframe::ParseError;
use crate::{BedChannel, ObjectAnchor, Vec3};
use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};

const ISF_OBJECT_COUNT: [usize; 6] = [4, 8, 10, 14, 15, 30];
const SAMPLE_OFFSET_INDEX: [u8; 4] = [8, 16, 18, 24];
const RAMP_DURATIONS: [i16; 3] = [0, 512, 1536];
const RAMP_DURATION_INDEX: [i16; 16] = [
    32, 64, 128, 256, 320, 480, 1000, 1001, 1024, 1600, 1601, 1602, 1920, 2000, 2002, 2048,
];
const DISTANCE_FACTORS: [f32; 16] = [
    1.1, 1.3, 1.6, 2.0, 2.5, 3.2, 4.0, 5.0, 6.3, 7.9, 10.0, 12.6, 15.8, 20.0, 25.1, 50.1,
];
const DEPTH_FACTORS: [f32; 4] = [0.25, 0.5, 1.0, 2.0];
const XY_SCALE: f32 = 1.0 / 62.0;
const Z_SCALE: f32 = 1.0 / 15.0;
const SIZE_SCALE: f32 = 1.0 / 31.0;
const JOC_NUM_BANDS: [usize; 8] = [1, 3, 5, 7, 9, 12, 15, 23];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedEmdfPayloadKind {
    Unknown,
    Oamd,
    Joc,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParsedEmdfPayloadData {
    Unknown,
    Oamd(OamdPayload),
    Joc(JocPayload),
}

impl ParsedEmdfPayloadData {
    pub fn kind(&self) -> ParsedEmdfPayloadKind {
        match self {
            Self::Unknown => ParsedEmdfPayloadKind::Unknown,
            Self::Oamd(_) => ParsedEmdfPayloadKind::Oamd,
            Self::Joc(_) => ParsedEmdfPayloadKind::Joc,
        }
    }

    pub fn short_summary(&self) -> Option<String> {
        match self {
            Self::Unknown => None,
            Self::Oamd(payload) => Some(payload.short_summary()),
            Self::Joc(payload) => Some(payload.short_summary()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OamdPayload {
    pub version: u8,
    pub object_count: usize,
    pub alternate_object_present: bool,
    pub element_count: usize,
    pub beds: usize,
    pub bed_instances: usize,
    pub bed_or_isf_objects: usize,
    pub dynamic_objects: usize,
    pub isf_in_use: bool,
    pub isf_index: Option<u8>,
    pub bed_assignment: Vec<Vec<BedChannel>>,
    pub elements: Vec<OamdElement>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OamdElement {
    pub element_index: u8,
    pub byte_length: usize,
    pub kind: OamdElementKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OamdElementKind {
    Unknown,
    Object(OamdObjectElement),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OamdObjectElement {
    pub sample_offset: u8,
    pub block_updates: Vec<OamdBlockUpdate>,
    pub object_blocks: Vec<Vec<OamdObjectBlock>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OamdBlockUpdate {
    pub offset: i16,
    pub ramp_duration: i16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OamdObjectBlock {
    pub inactive: bool,
    pub basic_info_status: u8,
    pub basic_info_blocks: Option<u8>,
    pub render_info_status: u8,
    pub render_info_blocks: Option<u8>,
    pub anchor: ObjectAnchor,
    pub gain: Option<f32>,
    pub priority: Option<u8>,
    pub valid_position: bool,
    pub differential_position: bool,
    pub position: Option<Vec3>,
    pub distance: Option<f32>,
    pub size: Option<f32>,
    pub screen_factor: Option<f32>,
    pub depth_factor: Option<f32>,
    pub additional_data_bytes: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JocPayload {
    pub downmix_config: u8,
    pub channel_count: usize,
    pub object_count: usize,
    pub gain: f32,
    pub sequence_counter: u16,
    pub objects: Vec<JocObject>,
}

impl OamdPayload {
    pub fn short_summary(&self) -> String {
        format!(
            "oamd[obj={} bed={} dyn={} bedinst={} isf={} elem={}]",
            self.object_count,
            self.beds,
            self.dynamic_objects,
            self.bed_instances,
            self.isf_index
                .map(|index| index.to_string())
                .unwrap_or_else(|| "-".to_string()),
            self.element_count,
        )
    }
}

impl JocPayload {
    pub fn active_object_count(&self) -> usize {
        self.objects.iter().filter(|object| object.active).count()
    }

    pub fn short_summary(&self) -> String {
        format!(
            "joc[ch={} obj={} active={} gain={:.3}]",
            self.channel_count,
            self.object_count,
            self.active_object_count(),
            self.gain,
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct JocObject {
    pub active: bool,
    pub bands_index: Option<u8>,
    pub bands: usize,
    pub sparse_coded: bool,
    pub quantization_table: Option<u8>,
    pub steep_slope: bool,
    pub data_points: usize,
    pub timeslot_offsets: Vec<u8>,
    pub data: Option<JocObjectData>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JocObjectData {
    Sparse {
        channel_indices: Vec<Vec<u8>>,
        vectors: Vec<Vec<u16>>,
    },
    Dense {
        matrices: Vec<Vec<Vec<u16>>>,
    },
}

#[derive(Clone, Copy)]
enum HuffmanType {
    Matrix,
    Vector,
    Index,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MetadataParseState {
    oamd: OamdPayloadParseState,
}

impl MetadataParseState {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Clone, Default)]
struct OamdPayloadParseState {
    elements: Vec<OamdElementParseState>,
}

#[derive(Debug, Clone, Default)]
struct OamdElementParseState {
    object: Option<OamdObjectElementParseState>,
}

#[derive(Debug, Clone, Default)]
struct OamdObjectElementParseState {
    object_blocks: Vec<Vec<OamdObjectBlockParseState>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct OamdObjectBlockParseState {
    anchor: ObjectAnchor,
    gain: Option<f32>,
    priority: Option<u8>,
    valid_position: bool,
    differential_position: bool,
    position: Option<Vec3>,
    distance: Option<f32>,
    size: Option<f32>,
    screen_factor: Option<f32>,
    depth_factor: Option<f32>,
}

impl Default for OamdObjectBlockParseState {
    fn default() -> Self {
        Self {
            anchor: ObjectAnchor::Room,
            gain: None,
            priority: None,
            valid_position: false,
            differential_position: false,
            position: None,
            distance: None,
            size: None,
            screen_factor: None,
            depth_factor: None,
        }
    }
}

pub(crate) fn parse_emdf_payload_body_with_state(
    payload_id: u8,
    bytes: &[u8],
    state: &mut MetadataParseState,
) -> Result<ParsedEmdfPayloadData, ParseError> {
    let mut reader = BitReader::new(bytes);
    reader.set_limit_bits(bytes.len() * 8);
    match payload_id {
        11 => {
            let mut payload = parse_oamd_payload(&mut reader)?;
            state.oamd.resolve_payload(&mut payload);
            Ok(ParsedEmdfPayloadData::Oamd(payload))
        }
        14 => Ok(ParsedEmdfPayloadData::Joc(parse_joc_payload(&mut reader)?)),
        _ => Ok(ParsedEmdfPayloadData::Unknown),
    }
}

impl OamdPayloadParseState {
    fn resolve_payload(&mut self, payload: &mut OamdPayload) {
        if self.elements.len() != payload.element_count {
            self.elements = vec![OamdElementParseState::default(); payload.element_count];
        }

        for (slot, element) in payload.elements.iter_mut().enumerate() {
            self.elements[slot].resolve_element(
                element,
                payload.object_count,
                payload.bed_or_isf_objects,
            );
        }
    }
}

impl OamdElementParseState {
    fn resolve_element(
        &mut self,
        element: &mut OamdElement,
        object_count: usize,
        bed_or_isf_objects: usize,
    ) {
        let OamdElementKind::Object(object_element) = &mut element.kind else {
            return;
        };

        let block_count = object_element.block_updates.len();
        let needs_reset = self.object.as_ref().is_none_or(|state| {
            state.object_blocks.len() != object_count
                || state.object_blocks.first().map(Vec::len).unwrap_or(0) != block_count
        });
        if needs_reset {
            self.object = Some(OamdObjectElementParseState {
                object_blocks: vec![
                    vec![OamdObjectBlockParseState::default(); block_count];
                    object_count
                ],
            });
        }

        if let Some(state) = self.object.as_mut() {
            state.resolve_object_element(object_element, bed_or_isf_objects);
        }
    }
}

impl OamdObjectElementParseState {
    fn resolve_object_element(
        &mut self,
        element: &mut OamdObjectElement,
        bed_or_isf_objects: usize,
    ) {
        for (object_index, blocks) in element.object_blocks.iter_mut().enumerate() {
            for (block_index, block) in blocks.iter_mut().enumerate() {
                self.object_blocks[object_index][block_index]
                    .resolve_block(block, object_index < bed_or_isf_objects);
            }
        }
    }
}

impl OamdObjectBlockParseState {
    fn resolve_block(&mut self, parsed: &mut OamdObjectBlock, bed_or_isf_object: bool) {
        let inactive = parsed.inactive;
        let basic_info_status = if inactive {
            0
        } else {
            parsed.basic_info_status
        };
        if (basic_info_status & 1) != 0 {
            let blocks = parsed
                .basic_info_blocks
                .unwrap_or(if basic_info_status == 1 { 3 } else { 0 });

            if (blocks & 2) != 0
                && let Some(gain) = parsed.gain
            {
                self.gain = Some(gain);
            }

            if (blocks & 1) != 0 && parsed.priority.is_some() {
                self.priority = parsed.priority;
            }
        }

        let render_info_status = if inactive || bed_or_isf_object {
            0
        } else {
            parsed.render_info_status
        };
        if (render_info_status & 1) != 0 {
            let blocks = parsed
                .render_info_blocks
                .unwrap_or(if render_info_status == 1 { 15 } else { 0 });

            self.valid_position = (blocks & 1) != 0;
            if self.valid_position {
                self.differential_position = parsed.differential_position;
                if let Some(position) = parsed.position {
                    self.position = Some(position);
                }
                self.distance = parsed.distance;
            }

            if (blocks & 4) != 0
                && let Some(size) = parsed.size
            {
                self.size = Some(size);
            }

            if (blocks & 8) != 0 && parsed.anchor == ObjectAnchor::Screen {
                self.anchor = ObjectAnchor::Screen;
                self.screen_factor = parsed.screen_factor;
                self.depth_factor = parsed.depth_factor;
            }
        }

        if bed_or_isf_object {
            self.anchor = ObjectAnchor::Speaker;
        }

        parsed.anchor = self.anchor;
        parsed.gain = self.gain;
        parsed.priority = self.priority;
        parsed.valid_position = self.valid_position;
        parsed.differential_position = self.differential_position;
        parsed.position = self.position;
        parsed.distance = self.distance;
        parsed.size = self.size;
        parsed.screen_factor = self.screen_factor;
        parsed.depth_factor = self.depth_factor;
    }
}

const LOG_LEVEL_ERROR: u8 = 1;
const LOG_LEVEL_WARN: u8 = 2;
const LOG_LEVEL_INFO: u8 = 3;
const LOG_LEVEL_DEBUG: u8 = 4;
const LOG_LEVEL_TRACE: u8 = 5;

static METADATA_LOG_LEVEL: AtomicU8 = AtomicU8::new(LOG_LEVEL_DEBUG);

const fn encode_log_level(level: log::Level) -> u8 {
    match level {
        log::Level::Error => LOG_LEVEL_ERROR,
        log::Level::Warn => LOG_LEVEL_WARN,
        log::Level::Info => LOG_LEVEL_INFO,
        log::Level::Debug => LOG_LEVEL_DEBUG,
        log::Level::Trace => LOG_LEVEL_TRACE,
    }
}

const fn decode_log_level(level: u8) -> log::Level {
    match level {
        LOG_LEVEL_ERROR => log::Level::Error,
        LOG_LEVEL_WARN => log::Level::Warn,
        LOG_LEVEL_INFO => log::Level::Info,
        LOG_LEVEL_TRACE => log::Level::Trace,
        _ => log::Level::Debug,
    }
}

pub(crate) fn set_metadata_log_level(level: log::Level) {
    METADATA_LOG_LEVEL.store(encode_log_level(level), Ordering::Relaxed);
}

fn metadata_log_level() -> log::Level {
    decode_log_level(METADATA_LOG_LEVEL.load(Ordering::Relaxed))
}

fn emit_metadata_debug(args: fmt::Arguments<'_>) {
    log::log!(
        target: "starmine_ad::eac3dec::metadata",
        metadata_log_level(),
        "{args}"
    );
}

fn parse_oamd_payload(reader: &mut BitReader<'_>) -> Result<OamdPayload, ParseError> {
    let mut version = read_bits(reader, 2, "OAver")? as u8;
    if version == 3 {
        version = version
            .checked_add(read_bits(reader, 3, "OAver")? as u8)
            .ok_or(ParseError::InvalidHeader("OAver"))?;
    }
    if version != 0 {
        return Err(ParseError::UnsupportedFeature("OAver"));
    }

    let mut object_count = read_bits(reader, 5, "object_count")? as usize + 1;
    if object_count == 32 {
        object_count += read_bits(reader, 7, "object_count-ext")? as usize;
    }

    let mut isf_in_use = false;
    let mut isf_index = None;
    let mut bed_assignment = parse_program_assignment(reader, &mut isf_in_use, &mut isf_index)?;
    let beds = bed_assignment.iter().map(Vec::len).sum::<usize>();
    let alternate_object_present = read_bit(reader, "alternate_object_present")?;

    let mut element_count = read_bits(reader, 4, "oa_element_count")? as usize;
    if element_count == 15 {
        element_count += read_bits(reader, 5, "oa_element_count-ext")? as usize;
    }

    let mut bed_or_isf_objects = beds;
    if let Some(index) = isf_index {
        bed_or_isf_objects += ISF_OBJECT_COUNT
            .get(index as usize)
            .copied()
            .ok_or(ParseError::UnsupportedFeature("ISF"))?;
    }

    emit_metadata_debug(format_args!(
        "oamd version={version} obj={object_count} beds={beds} bedinst={} bed_or_isf={bed_or_isf_objects} alt={} elem={element_count}",
        bed_assignment.len(),
        alternate_object_present as u8,
    ));

    let mut elements = Vec::with_capacity(element_count);
    for _ in 0..element_count {
        elements.push(parse_oamd_element(
            reader,
            alternate_object_present,
            object_count,
            bed_or_isf_objects,
        )?);
    }

    if bed_assignment.is_empty() && object_count > 0 {
        bed_assignment = Vec::new();
    }

    Ok(OamdPayload {
        version,
        object_count,
        alternate_object_present,
        element_count,
        beds,
        bed_instances: bed_assignment.len(),
        bed_or_isf_objects,
        dynamic_objects: object_count.saturating_sub(bed_or_isf_objects),
        isf_in_use,
        isf_index,
        bed_assignment,
        elements,
    })
}

fn parse_program_assignment(
    reader: &mut BitReader<'_>,
    isf_in_use: &mut bool,
    isf_index: &mut Option<u8>,
) -> Result<Vec<Vec<BedChannel>>, ParseError> {
    if read_bit(reader, "b_dyn_object_only_program")? {
        if read_bit(reader, "b_lfe_present")? {
            return Ok(vec![vec![BedChannel::LowFrequencyEffects]]);
        }
        return Ok(Vec::new());
    }

    let content_description = read_bits(reader, 4, "content_description")? as u8;
    let mut bed_assignment = Vec::new();
    if (content_description & 1) != 0 {
        read_bit(reader, "b_bed_distributable")?;
        let bed_instances = if read_bit(reader, "b_multi_bed")? {
            read_bits(reader, 3, "num_bed_instances")? as usize + 2
        } else {
            1
        };
        bed_assignment.reserve(bed_instances);
        for _ in 0..bed_instances {
            if read_bit(reader, "b_lfe_only_bed")? {
                bed_assignment.push(vec![BedChannel::LowFrequencyEffects]);
                continue;
            }
            if read_bit(reader, "b_standard_bed")? {
                let standard_assignment = read_bit_vec(reader, 10)?;
                let mut channels = Vec::new();
                for (index, enabled) in standard_assignment.into_iter().enumerate() {
                    if enabled {
                        channels.extend(STANDARD_BED_CHANNELS[index].iter().copied());
                    }
                }
                bed_assignment.push(channels);
            } else {
                let assignment = read_bit_vec(reader, BED_CHANNELS.len())?;
                let mut channels = Vec::new();
                for (index, enabled) in assignment.into_iter().enumerate() {
                    if enabled {
                        channels.push(BED_CHANNELS[index]);
                    }
                }
                bed_assignment.push(channels);
            }
        }
    }

    if (content_description & 2) != 0 {
        *isf_in_use = true;
        let index = read_bits(reader, 3, "isf_index")? as u8;
        if index as usize >= ISF_OBJECT_COUNT.len() {
            return Err(ParseError::UnsupportedFeature("ISF"));
        }
        *isf_index = Some(index);
    }

    if (content_description & 4) != 0 {
        let count = read_bits(reader, 5, "room_or_screen_anchored_objects")?;
        if count == 31 {
            skip_bits(reader, 7, "room_or_screen_anchored_objects-ext")?;
        }
    }

    if (content_description & 8) != 0 {
        let bytes = read_bits(reader, 4, "reserved_content_description")? as usize + 1;
        skip_bits(reader, bytes * 8, "reserved_content_description-bytes")?;
    }

    Ok(bed_assignment)
}

fn parse_oamd_element(
    reader: &mut BitReader<'_>,
    alternate_object_present: bool,
    object_count: usize,
    bed_or_isf_objects: usize,
) -> Result<OamdElement, ParseError> {
    let start_pos = reader.position();
    let element_index = read_bits(reader, 4, "oa_element_id_idx")? as u8;
    // Some real-world streams only line up if the encoded value is treated as a byte count
    // instead of a bit count.
    let byte_length = read_variable_bits_limited(reader, 4, 4, "oa_element_length")? as usize + 1;
    let payload_start = reader.position();
    let end_pos = reader
        .position()
        .checked_add(byte_length * 8)
        .ok_or(ParseError::InvalidHeader("oa_element_length"))?;
    emit_metadata_debug(format_args!(
        "oamd-element start={start_pos} idx={element_index} len={}B payload_start={payload_start} end={end_pos} alt={}",
        byte_length, alternate_object_present as u8,
    ));
    let skip = if alternate_object_present { 5 } else { 1 };
    skip_bits(reader, skip, "oa_alternate_object_info")?;

    let kind = if element_index == 1 {
        OamdElementKind::Object(parse_oamd_object_element(
            reader,
            object_count,
            bed_or_isf_objects,
        )?)
    } else {
        OamdElementKind::Unknown
    };

    if reader.position() > end_pos {
        emit_metadata_debug(format_args!(
            "oamd-element overrun idx={element_index} pos={} end={end_pos}",
            reader.position(),
        ));
        return Err(ParseError::InvalidHeader("oa_element_length"));
    }
    if reader.position() < end_pos {
        skip_bits(reader, end_pos - reader.position(), "oa_element_padding")?;
    }

    Ok(OamdElement {
        element_index,
        byte_length,
        kind,
    })
}

fn parse_oamd_object_element(
    reader: &mut BitReader<'_>,
    object_count: usize,
    bed_or_isf_objects: usize,
) -> Result<OamdObjectElement, ParseError> {
    let (sample_offset, block_updates) = parse_md_update_info(reader)?;
    emit_metadata_debug(format_args!(
        "oamd-object-element sample_offset={sample_offset} blocks={} objects={object_count} bed_or_isf={bed_or_isf_objects}",
        block_updates.len(),
    ));
    if !read_bit(reader, "oa_reserved_flag")? {
        skip_bits(reader, 5, "oa_reserved_bits")?;
    }

    let mut object_blocks =
        vec![vec![OamdObjectBlock::default(); block_updates.len()]; object_count];
    for (object_index, blocks) in object_blocks.iter_mut().enumerate() {
        for (block_index, block) in blocks.iter_mut().enumerate() {
            *block = parse_oamd_object_block(
                reader,
                object_index,
                block_index,
                object_index < bed_or_isf_objects,
            )?;
        }
    }

    Ok(OamdObjectElement {
        sample_offset,
        block_updates,
        object_blocks,
    })
}

fn parse_md_update_info(
    reader: &mut BitReader<'_>,
) -> Result<(u8, Vec<OamdBlockUpdate>), ParseError> {
    let sample_offset = match read_bits(reader, 2, "mdOffset")? {
        0 => 0,
        1 => SAMPLE_OFFSET_INDEX[read_bits(reader, 2, "mdOffset-index")? as usize],
        2 => read_bits(reader, 5, "mdOffset-explicit")? as u8,
        _ => return Err(ParseError::UnsupportedFeature("mdOffset")),
    };

    let blocks = read_bits(reader, 3, "num_obj_info_blocks")? as usize + 1;
    let mut updates = Vec::with_capacity(blocks);
    for _ in 0..blocks {
        let offset = read_bits(reader, 6, "block_offset_factor")? as i16 + sample_offset as i16;
        let ramp_duration = match read_bits(reader, 2, "ramp_duration_code")? {
            code @ 0..=2 => RAMP_DURATIONS[code as usize],
            _ => {
                if read_bit(reader, "ramp_duration_indexed")? {
                    RAMP_DURATION_INDEX[read_bits(reader, 4, "ramp_duration_index")? as usize]
                } else {
                    read_bits(reader, 11, "ramp_duration_explicit")? as i16
                }
            }
        };
        updates.push(OamdBlockUpdate {
            offset,
            ramp_duration,
        });
    }
    emit_metadata_debug(format_args!(
        "oamd-update-info sample_offset={} blocks={:?}",
        sample_offset, updates
    ));
    Ok((sample_offset, updates))
}

fn parse_oamd_object_block(
    reader: &mut BitReader<'_>,
    object_index: usize,
    block_index: usize,
    bed_or_isf_object: bool,
) -> Result<OamdObjectBlock, ParseError> {
    let start_pos = reader.position();
    let inactive = read_bit(reader, "oa_object_inactive")?;
    let basic_info_status = if inactive {
        0
    } else if block_index == 0 {
        1
    } else {
        read_bits(reader, 2, "oa_basic_info_status")? as u8
    };

    let mut basic_info_blocks = None;
    let mut gain = None;
    let mut priority = None;
    if (basic_info_status & 1) != 0 {
        let blocks = if basic_info_status == 1 {
            3
        } else {
            read_bits(reader, 2, "oa_basic_info_blocks")? as u8
        };
        basic_info_blocks = Some(blocks);

        if (blocks & 2) != 0 {
            let gain_code = read_bits(reader, 2, "oa_gain_helper")?;
            gain = match gain_code {
                0 => Some(0.707),
                1 => Some(0.0),
                2 => {
                    let gain_step = read_bits(reader, 6, "oa_gain_step")? as i32;
                    let gain_db = if gain_step < 15 {
                        15 - gain_step
                    } else {
                        14 - gain_step
                    };
                    Some(db_to_gain(gain_db as f32) * 0.707)
                }
                _ => None,
            };
        }

        if (blocks & 1) != 0 && !read_bit(reader, "oa_default_priority")? {
            priority = Some(read_bits(reader, 5, "oa_priority")? as u8);
        }
    }

    let mut render_info_status = 0u8;
    let mut render_info_blocks = None;
    if !inactive && !bed_or_isf_object {
        render_info_status = if block_index == 0 {
            1
        } else {
            read_bits(reader, 2, "oa_render_info_status")? as u8
        };
    }

    let mut anchor = ObjectAnchor::Room;
    let mut valid_position = false;
    let mut differential_position = false;
    let mut position = None;
    let mut distance = None;
    let mut size = None;
    let mut screen_factor = None;
    let mut depth_factor = None;

    if (render_info_status & 1) != 0 {
        let blocks = if render_info_status == 1 {
            15
        } else {
            read_bits(reader, 4, "oa_render_info_blocks")? as u8
        };
        render_info_blocks = Some(blocks);

        if (blocks & 1) != 0 {
            valid_position = true;
            differential_position =
                block_index != 0 && read_bit(reader, "oa_differential_position")?;
            position = Some(if differential_position {
                Vec3 {
                    x: read_signed_bits(reader, 3, "oa_delta_x")? as f32 * XY_SCALE,
                    y: read_signed_bits(reader, 3, "oa_delta_y")? as f32 * XY_SCALE,
                    z: read_signed_bits(reader, 3, "oa_delta_z")? as f32 * Z_SCALE,
                }
            } else {
                let pos_x = read_bits(reader, 6, "oa_pos_x")? as f32;
                let pos_y = read_bits(reader, 6, "oa_pos_y")? as f32;
                let pos_z_sign = if read_bit(reader, "oa_pos_z_sign")? {
                    1.0
                } else {
                    -1.0
                };
                let pos_z_mag = read_bits(reader, 4, "oa_pos_z_mag")? as f32;
                Vec3 {
                    x: (pos_x * XY_SCALE).min(1.0),
                    y: (pos_y * XY_SCALE).min(1.0),
                    z: (pos_z_sign * pos_z_mag * Z_SCALE).min(1.0),
                }
            });

            if read_bit(reader, "oa_distance_present")? {
                distance = Some(if read_bit(reader, "oa_infinite_distance")? {
                    f32::INFINITY
                } else {
                    DISTANCE_FACTORS[read_bits(reader, 4, "oa_distance_index")? as usize]
                });
            }
        }

        if (blocks & 2) != 0 {
            skip_bits(reader, 4, "oa_zone_constraints")?;
        }

        if (blocks & 4) != 0 {
            size = match read_bits(reader, 2, "oa_size_mode")? {
                0 => Some(0.0),
                1 => Some(read_bits(reader, 5, "oa_size_scalar")? as f32 * SIZE_SCALE),
                2 => {
                    let x = read_bits(reader, 5, "oa_size_x")? as f32 * SIZE_SCALE;
                    let y = read_bits(reader, 5, "oa_size_y")? as f32 * SIZE_SCALE;
                    let z = read_bits(reader, 5, "oa_size_z")? as f32 * SIZE_SCALE;
                    Some((x * x + y * y + z * z).sqrt())
                }
                _ => None,
            };
        }

        if (blocks & 8) != 0 && read_bit(reader, "oa_screen_anchor")? {
            anchor = ObjectAnchor::Screen;
            screen_factor = Some((read_bits(reader, 3, "oa_screen_factor")? as f32 + 1.0) * 0.125);
            depth_factor = Some(DEPTH_FACTORS[read_bits(reader, 2, "oa_depth_factor")? as usize]);
        }

        skip_bits(reader, 1, "oa_snap_to_channel")?;
    }

    let additional_data_bytes = if read_bit(reader, "oa_additional_table_data")? {
        let bytes = read_bits(reader, 4, "oa_additional_table_bytes")? as usize + 1;
        skip_bits(reader, bytes * 8, "oa_additional_table_payload")?;
        bytes
    } else {
        0
    };

    if bed_or_isf_object {
        anchor = ObjectAnchor::Speaker;
    }

    emit_metadata_debug(format_args!(
        "oamd-object-block obj={object_index} blk={block_index} start={start_pos} end={} inactive={} basic_status={} basic_blocks={basic_info_blocks:?} gain={gain:?} render_status={} render_blocks={render_info_blocks:?} anchor={anchor:?} valid_pos={} diff={} pos={position:?} dist={distance:?} size={size:?} screen={screen_factor:?}/{depth_factor:?} addtl={additional_data_bytes}",
        reader.position(),
        inactive as u8,
        basic_info_status,
        render_info_status,
        valid_position as u8,
        differential_position as u8,
    ));

    Ok(OamdObjectBlock {
        inactive,
        basic_info_status,
        basic_info_blocks,
        render_info_status,
        render_info_blocks,
        anchor,
        gain,
        priority,
        valid_position,
        differential_position,
        position,
        distance,
        size,
        screen_factor,
        depth_factor,
        additional_data_bytes,
    })
}

fn parse_joc_payload(reader: &mut BitReader<'_>) -> Result<JocPayload, ParseError> {
    let downmix_config = read_bits(reader, 3, "joc_dmx_config_idx")? as u8;
    if downmix_config > 4 {
        return Err(ParseError::UnsupportedFeature("joc_dmx_config_idx"));
    }
    let channel_count = if downmix_config == 0 || downmix_config == 3 {
        5
    } else {
        7
    };

    let object_count = read_bits(reader, 6, "joc_num_objects")? as usize + 1;
    if read_bits(reader, 3, "joc_ext_config_idx")? != 0 {
        return Err(ParseError::UnsupportedFeature("joc_ext_config_idx"));
    }

    let gain_power = read_bits(reader, 3, "joc_gain_power")? as i32;
    let gain_fraction = read_bits(reader, 5, "joc_gain_fraction")? as f32 / 32.0;
    let gain = 1.0 + gain_fraction * 2f32.powi(gain_power - 4);
    let sequence_counter = read_bits(reader, 10, "joc_sequence_counter")? as u16;

    emit_metadata_debug(format_args!(
        "joc-header dmx={} channels={} objects={} gain={:.3} seq={} bits={}",
        downmix_config,
        channel_count,
        object_count,
        gain,
        sequence_counter,
        reader.position(),
    ));

    let mut objects = Vec::with_capacity(object_count);
    for object_index in 0..object_count {
        if !reader.bits_left(1) {
            // Some streams omit trailing inactive objects at the exact end of the payload.
            // Keep hard errors for partial object headers/data, but tolerate this tail case.
            emit_metadata_debug(format_args!(
                "joc-object idx={object_index} truncated-tail bits={} remaining={} -> implicit-inactive",
                reader.position(),
                object_count - object_index,
            ));
            for _ in object_index..object_count {
                objects.push(JocObject {
                    active: false,
                    bands_index: None,
                    bands: 0,
                    sparse_coded: false,
                    quantization_table: None,
                    steep_slope: false,
                    data_points: 0,
                    timeslot_offsets: Vec::new(),
                    data: None,
                });
            }
            break;
        }
        emit_metadata_debug(format_args!(
            "joc-object idx={object_index} start_bits={}",
            reader.position()
        ));
        let active = read_bit(reader, "b_joc_obj_present")?;
        if !active {
            emit_metadata_debug(format_args!(
                "joc-object idx={object_index} inactive end_bits={}",
                reader.position()
            ));
            objects.push(JocObject {
                active,
                bands_index: None,
                bands: 0,
                sparse_coded: false,
                quantization_table: None,
                steep_slope: false,
                data_points: 0,
                timeslot_offsets: Vec::new(),
                data: None,
            });
            continue;
        }

        let bands_index = read_bits(reader, 3, "joc_num_bands_idx")? as u8;
        let bands = *JOC_NUM_BANDS
            .get(bands_index as usize)
            .ok_or(ParseError::InvalidHeader("joc_num_bands_idx"))?;
        let sparse_coded = read_bit(reader, "b_joc_sparse")?;
        let quantization_table = read_bit(reader, "joc_num_quant_idx")? as u8;
        let steep_slope = read_bit(reader, "joc_slope_idx")?;
        let data_points = read_bits(reader, 1, "joc_num_dpoints")? as usize + 1;

        let mut timeslot_offsets = Vec::new();
        if steep_slope {
            timeslot_offsets.reserve(data_points);
            for _ in 0..data_points {
                timeslot_offsets.push(read_bits(reader, 5, "joc_timeslot_offset")? as u8 + 1);
            }
        }

        emit_metadata_debug(format_args!(
            "joc-object idx={object_index} active bands_idx={} bands={} sparse={} quant={} steep={} points={} offsets={:?} header_end_bits={}",
            bands_index,
            bands,
            sparse_coded as u8,
            quantization_table,
            steep_slope as u8,
            data_points,
            timeslot_offsets,
            reader.position(),
        ));

        objects.push(JocObject {
            active,
            bands_index: Some(bands_index),
            bands,
            sparse_coded,
            quantization_table: Some(quantization_table),
            steep_slope,
            data_points,
            timeslot_offsets,
            data: None,
        });
    }

    for (object_index, object) in objects.iter_mut().enumerate() {
        if !object.active {
            continue;
        }

        let quantization_table = object
            .quantization_table
            .ok_or(ParseError::InvalidHeader("joc_num_quant_idx"))?;
        let bands = object.bands;
        let data_points = object.data_points;

        object.data = if object.sparse_coded {
            let channel_table = huffman_table(channel_count, HuffmanType::Index);
            let vector_table = huffman_table(quantization_table as usize, HuffmanType::Vector);
            let mut channel_indices = Vec::with_capacity(data_points);
            let mut vectors = Vec::with_capacity(data_points);
            for _ in 0..data_points {
                let mut channels = Vec::with_capacity(bands);
                channels.push(read_bits(reader, 3, "joc_sparse_channel0")? as u8);
                for _ in 1..bands {
                    channels.push(huffman_decode(channel_table, reader)? as u8);
                }

                let mut data_vector = Vec::with_capacity(bands);
                for _ in 0..bands {
                    data_vector.push(huffman_decode(vector_table, reader)?);
                }

                channel_indices.push(channels);
                vectors.push(data_vector);
            }
            emit_metadata_debug(format_args!(
                "joc-object idx={object_index} sparse-data preview_channels={:?} preview_vectors={:?} end_bits={}",
                channel_indices
                    .first()
                    .map(|channels| &channels[..channels.len().min(12)])
                    .unwrap_or(&[]),
                vectors
                    .first()
                    .map(|vector| &vector[..vector.len().min(12)])
                    .unwrap_or(&[]),
                reader.position(),
            ));
            Some(JocObjectData::Sparse {
                channel_indices,
                vectors,
            })
        } else {
            let matrix_table = huffman_table(quantization_table as usize, HuffmanType::Matrix);
            let mut matrices = Vec::with_capacity(data_points);
            for _ in 0..data_points {
                let mut data_point = Vec::with_capacity(channel_count);
                for _ in 0..channel_count {
                    let mut channel = Vec::with_capacity(bands);
                    for _ in 0..bands {
                        channel.push(huffman_decode(matrix_table, reader)?);
                    }
                    data_point.push(channel);
                }
                matrices.push(data_point);
            }
            emit_metadata_debug(format_args!(
                "joc-object idx={object_index} dense-data preview={:?} end_bits={}",
                matrices
                    .first()
                    .and_then(|data_point| data_point.first())
                    .map(|channel| &channel[..channel.len().min(12)])
                    .unwrap_or(&[]),
                reader.position(),
            ));
            Some(JocObjectData::Dense { matrices })
        };
    }

    Ok(JocPayload {
        downmix_config,
        channel_count,
        object_count,
        gain,
        sequence_counter,
        objects,
    })
}

fn huffman_decode(
    table: &'static [[i16; 2]],
    reader: &mut BitReader<'_>,
) -> Result<u16, ParseError> {
    let mut node = 0i16;
    loop {
        let bit = if read_bit(reader, "joc_huffman_bit")? {
            1
        } else {
            0
        };
        node = table
            .get(node as usize)
            .and_then(|children| children.get(bit))
            .copied()
            .ok_or(ParseError::InvalidHeader("joc_huffman_node"))?;
        if node <= 0 {
            return Ok((!node) as u16);
        }
    }
}

fn huffman_table(mode: usize, kind: HuffmanType) -> &'static [[i16; 2]] {
    match kind {
        HuffmanType::Matrix => {
            if mode == 1 {
                JOC_HUFF_CODE_FINE_GENERIC
            } else {
                JOC_HUFF_CODE_COARSE_GENERIC
            }
        }
        HuffmanType::Vector => {
            if mode == 1 {
                JOC_HUFF_CODE_FINE_COEFF_SPARSE
            } else {
                JOC_HUFF_CODE_COARSE_COEFF_SPARSE
            }
        }
        HuffmanType::Index => {
            if mode == 7 {
                JOC_HUFF_CODE_7CH_POS_INDEX_SPARSE
            } else {
                JOC_HUFF_CODE_5CH_POS_INDEX_SPARSE
            }
        }
    }
}

fn read_bits(
    reader: &mut BitReader<'_>,
    bits: usize,
    field: &'static str,
) -> Result<u32, ParseError> {
    reader
        .read_bits(bits)
        .ok_or(ParseError::InvalidHeader(field))
}

fn read_signed_bits(
    reader: &mut BitReader<'_>,
    bits: usize,
    field: &'static str,
) -> Result<i32, ParseError> {
    reader
        .read_signed_bits(bits)
        .ok_or(ParseError::InvalidHeader(field))
}

fn read_bit(reader: &mut BitReader<'_>, field: &'static str) -> Result<bool, ParseError> {
    reader.read_bit().ok_or(ParseError::InvalidHeader(field))
}

fn skip_bits(
    reader: &mut BitReader<'_>,
    bits: usize,
    field: &'static str,
) -> Result<(), ParseError> {
    reader
        .skip_bits(bits)
        .ok_or(ParseError::InvalidHeader(field))
}

fn read_variable_bits_limited(
    reader: &mut BitReader<'_>,
    width: usize,
    mut limit: usize,
    field: &'static str,
) -> Result<u32, ParseError> {
    let mut value = 0u32;
    let mut groups = Vec::new();
    loop {
        let part = read_bits(reader, width, field)?;
        groups.push(part);
        value = value
            .checked_add(part)
            .ok_or(ParseError::InvalidHeader(field))?;
        let read_more = read_bit(reader, field)?;
        if field == "oa_element_length" {
            emit_metadata_debug(format_args!(
                "varbits field={field} group={} part={part} more={}",
                groups.len() - 1,
                read_more as u8,
            ));
        }
        if !read_more {
            if field == "oa_element_length" {
                emit_metadata_debug(format_args!(
                    "varbits field={field} value={value} groups={groups:?}"
                ));
            }
            return Ok(value);
        }
        value = value
            .checked_add(1)
            .and_then(|next| next.checked_shl(width as u32))
            .ok_or(ParseError::InvalidHeader(field))?;
        if limit == 0 {
            if field == "oa_element_length" {
                emit_metadata_debug(format_args!(
                    "varbits field={field} value={value} groups={groups:?} limit-hit"
                ));
            }
            return Ok(value);
        }
        limit -= 1;
    }
}

fn read_bit_vec(reader: &mut BitReader<'_>, bits: usize) -> Result<Vec<bool>, ParseError> {
    let mut result = vec![false; bits];
    for index in (0..bits).rev() {
        result[index] = read_bit(reader, "bit-array")?;
    }
    Ok(result)
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

const BED_CHANNELS: [BedChannel; 17] = [
    BedChannel::FrontLeft,
    BedChannel::FrontRight,
    BedChannel::Center,
    BedChannel::LowFrequencyEffects,
    BedChannel::SurroundLeft,
    BedChannel::SurroundRight,
    BedChannel::RearLeft,
    BedChannel::RearRight,
    BedChannel::TopFrontLeft,
    BedChannel::TopFrontRight,
    BedChannel::TopSurroundLeft,
    BedChannel::TopSurroundRight,
    BedChannel::TopRearLeft,
    BedChannel::TopRearRight,
    BedChannel::WideLeft,
    BedChannel::WideRight,
    BedChannel::LowFrequencyEffects2,
];

const STANDARD_BED_CHANNELS: [&[BedChannel]; 10] = [
    &[BedChannel::FrontLeft, BedChannel::FrontRight],
    &[BedChannel::Center],
    &[BedChannel::LowFrequencyEffects],
    &[BedChannel::SurroundLeft, BedChannel::SurroundRight],
    &[BedChannel::RearLeft, BedChannel::RearRight],
    &[BedChannel::TopFrontLeft, BedChannel::TopFrontRight],
    &[BedChannel::TopSurroundLeft, BedChannel::TopSurroundRight],
    &[BedChannel::TopRearLeft, BedChannel::TopRearRight],
    &[BedChannel::WideLeft, BedChannel::WideRight],
    &[BedChannel::LowFrequencyEffects2],
];

impl Default for OamdObjectBlock {
    fn default() -> Self {
        Self {
            inactive: false,
            basic_info_status: 0,
            basic_info_blocks: None,
            render_info_status: 0,
            render_info_blocks: None,
            anchor: ObjectAnchor::Room,
            gain: None,
            priority: None,
            valid_position: false,
            differential_position: false,
            position: None,
            distance: None,
            size: None,
            screen_factor: None,
            depth_factor: None,
            additional_data_bytes: 0,
        }
    }
}

const JOC_HUFF_CODE_COARSE_GENERIC: &[[i16; 2]] = &[
    [-1, 1],
    [2, -2],
    [-96, 3],
    [4, -3],
    [-95, 5],
    [6, 7],
    [-4, -94],
    [8, 9],
    [-5, -93],
    [10, 11],
    [-6, -92],
    [12, 13],
    [-7, -91],
    [14, 15],
    [16, -90],
    [-8, 17],
    [18, -89],
    [-9, 19],
    [20, 21],
    [-88, -10],
    [22, 23],
    [-11, -87],
    [24, 25],
    [26, -86],
    [-12, 27],
    [28, -85],
    [-13, 29],
    [30, 31],
    [32, -84],
    [-14, 33],
    [34, -15],
    [-83, 35],
    [36, 37],
    [-16, 38],
    [-17, -82],
    [39, 40],
    [41, -81],
    [42, 43],
    [44, 45],
    [46, 47],
    [48, 49],
    [50, 51],
    [52, -18],
    [-78, 53],
    [-19, 54],
    [55, 56],
    [57, 58],
    [-22, 59],
    [60, 61],
    [62, 63],
    [64, 65],
    [66, 67],
    [68, -20],
    [-21, -79],
    [-80, -25],
    [69, 70],
    [-26, 71],
    [72, 73],
    [74, 75],
    [76, 77],
    [78, 79],
    [80, 81],
    [82, 83],
    [84, 85],
    [86, 87],
    [88, 89],
    [90, 91],
    [92, 93],
    [94, -23],
    [-74, -75],
    [-72, -73],
    [-76, -77],
    [-34, -35],
    [-32, -33],
    [-38, -39],
    [-36, -37],
    [-30, -31],
    [-28, -29],
    [-50, -51],
    [-48, -49],
    [-54, -55],
    [-52, -53],
    [-42, -43],
    [-40, -41],
    [-46, -47],
    [-44, -45],
    [-66, -67],
    [-64, -65],
    [-70, -71],
    [-68, -69],
    [-58, -59],
    [-56, -57],
    [-62, -63],
    [-60, -61],
    [-24, -27],
];

const JOC_HUFF_CODE_FINE_GENERIC: &[[i16; 2]] = &[
    [-1, 1],
    [2, 3],
    [-2, -192],
    [4, 5],
    [6, -3],
    [-191, 7],
    [8, 9],
    [-4, -190],
    [10, 11],
    [-5, -189],
    [12, 13],
    [-6, 14],
    [-188, 15],
    [16, -7],
    [-187, 17],
    [18, -8],
    [-186, 19],
    [20, -9],
    [-185, 21],
    [22, -10],
    [-184, 23],
    [24, -11],
    [25, -183],
    [26, 27],
    [-12, -182],
    [28, 29],
    [-13, -181],
    [30, 31],
    [-180, -14],
    [32, 33],
    [34, -179],
    [-15, 35],
    [36, -178],
    [-16, 37],
    [38, -177],
    [39, -17],
    [40, 41],
    [-176, 42],
    [-18, 43],
    [-19, 44],
    [-175, 45],
    [46, -174],
    [-20, 47],
    [-173, 48],
    [49, -21],
    [50, 51],
    [52, -22],
    [53, 54],
    [-172, 55],
    [-171, -23],
    [56, 57],
    [58, -170],
    [59, -24],
    [-25, 60],
    [-169, 61],
    [62, 63],
    [64, 65],
    [66, 67],
    [-168, 68],
    [-26, 69],
    [-167, -27],
    [70, -166],
    [-165, 71],
    [-29, 72],
    [73, 74],
    [-30, 75],
    [76, 77],
    [78, 79],
    [80, -28],
    [81, 82],
    [83, -163],
    [-31, -33],
    [-164, -161],
    [84, 85],
    [86, 87],
    [88, 89],
    [90, 91],
    [92, 93],
    [94, 95],
    [96, 97],
    [98, 99],
    [-32, -162],
    [100, 101],
    [102, 103],
    [104, 105],
    [106, 107],
    [108, 109],
    [110, 111],
    [-160, 112],
    [-36, -38],
    [113, 114],
    [115, 116],
    [117, 118],
    [119, 120],
    [121, 122],
    [123, 124],
    [125, 126],
    [127, 128],
    [129, 130],
    [131, 132],
    [133, -35],
    [-158, 134],
    [-155, -156],
    [-37, -42],
    [135, 136],
    [137, 138],
    [139, 140],
    [141, 142],
    [143, 144],
    [145, 146],
    [147, 148],
    [149, 150],
    [151, 152],
    [153, 154],
    [155, 156],
    [157, 158],
    [159, 160],
    [161, 162],
    [163, 164],
    [165, 166],
    [167, 168],
    [169, 170],
    [171, 172],
    [173, 174],
    [175, 176],
    [177, 178],
    [179, 180],
    [181, 182],
    [183, 184],
    [-157, 185],
    [-45, -48],
    [186, 187],
    [188, 189],
    [-34, -41],
    [190, -39],
    [-60, -61],
    [-58, -59],
    [-64, -65],
    [-62, -63],
    [-52, -53],
    [-50, -51],
    [-56, -57],
    [-54, -55],
    [-76, -77],
    [-74, -75],
    [-80, -81],
    [-78, -79],
    [-68, -69],
    [-66, -67],
    [-72, -73],
    [-70, -71],
    [-47, -49],
    [-44, -46],
    [-124, -125],
    [-122, -123],
    [-128, -129],
    [-126, -127],
    [-116, -117],
    [-114, -115],
    [-120, -121],
    [-118, -119],
    [-140, -141],
    [-138, -139],
    [-144, -145],
    [-142, -143],
    [-132, -133],
    [-130, -131],
    [-136, -137],
    [-134, -135],
    [-92, -93],
    [-90, -91],
    [-96, -97],
    [-94, -95],
    [-84, -85],
    [-82, -83],
    [-88, -89],
    [-86, -87],
    [-108, -109],
    [-106, -107],
    [-112, -113],
    [-110, -111],
    [-100, -101],
    [-98, -99],
    [-104, -105],
    [-102, -103],
    [-154, -159],
    [-148, -149],
    [-146, -147],
    [-152, -153],
    [-150, -151],
    [-40, -43],
];

const JOC_HUFF_CODE_COARSE_COEFF_SPARSE: &[[i16; 2]] = &[
    [-1, 1],
    [2, 3],
    [-2, -96],
    [4, 5],
    [6, -95],
    [-3, 7],
    [8, 9],
    [-4, 10],
    [-94, 11],
    [12, -5],
    [-93, 13],
    [14, 15],
    [-6, -92],
    [16, 17],
    [18, -7],
    [-91, 19],
    [20, -8],
    [-90, 21],
    [22, 23],
    [-9, -89],
    [24, 25],
    [26, -10],
    [-88, 27],
    [28, 29],
    [30, -11],
    [-87, 31],
    [32, 33],
    [34, 35],
    [-12, -86],
    [36, 37],
    [38, -13],
    [39, -85],
    [40, 41],
    [42, 43],
    [-14, -84],
    [44, 45],
    [46, 47],
    [-83, -15],
    [48, 49],
    [50, -16],
    [51, 52],
    [-82, 53],
    [54, -81],
    [55, 56],
    [-17, 57],
    [58, -80],
    [59, 60],
    [-18, 61],
    [62, 63],
    [-79, 64],
    [-19, -78],
    [65, 66],
    [67, 68],
    [69, -20],
    [-77, -21],
    [70, 71],
    [72, 73],
    [74, -76],
    [75, -22],
    [76, 77],
    [-75, 78],
    [79, 80],
    [-54, -74],
    [-73, 81],
    [-23, 82],
    [-50, -24],
    [-55, -25],
    [83, -47],
    [-49, -44],
    [-71, 84],
    [-48, -51],
    [85, -72],
    [-26, -53],
    [-70, -27],
    [86, -45],
    [87, 88],
    [-68, 89],
    [-29, -43],
    [90, -30],
    [-46, -69],
    [91, -28],
    [-52, -31],
    [92, -32],
    [93, -64],
    [-67, 94],
    [-36, -33],
    [-63, -37],
    [-65, -61],
    [-66, -59],
    [-34, -38],
    [-41, -42],
    [-35, -60],
    [-39, -57],
    [-56, -40],
    [-62, -58],
];

const JOC_HUFF_CODE_FINE_COEFF_SPARSE: &[[i16; 2]] = &[
    [1, -1],
    [2, 3],
    [4, -2],
    [-192, 5],
    [6, 7],
    [8, -3],
    [-191, 9],
    [10, 11],
    [12, -190],
    [-4, 13],
    [14, 15],
    [-189, -5],
    [16, 17],
    [18, -6],
    [-188, 19],
    [20, 21],
    [-7, -187],
    [22, 23],
    [-8, 24],
    [-186, 25],
    [-9, 26],
    [27, -185],
    [28, -10],
    [29, 30],
    [-184, 31],
    [-11, 32],
    [33, -183],
    [34, -12],
    [35, -182],
    [36, 37],
    [38, -13],
    [-181, 39],
    [40, -14],
    [41, -180],
    [42, 43],
    [-179, -15],
    [44, -16],
    [45, -178],
    [46, 47],
    [48, 49],
    [50, -177],
    [-17, 51],
    [-18, 52],
    [-176, 53],
    [54, 55],
    [-175, -19],
    [56, 57],
    [58, -20],
    [59, -174],
    [60, 61],
    [-21, 62],
    [63, -173],
    [64, 65],
    [66, -172],
    [67, 68],
    [-22, 69],
    [70, 71],
    [-23, 72],
    [-171, 73],
    [74, 75],
    [76, -24],
    [77, -170],
    [-25, 78],
    [79, 80],
    [81, -169],
    [82, 83],
    [84, -26],
    [85, -168],
    [86, 87],
    [88, 89],
    [-167, 90],
    [-27, 91],
    [92, -28],
    [93, -166],
    [94, -29],
    [95, 96],
    [97, 98],
    [-165, 99],
    [100, -30],
    [-164, 101],
    [102, 103],
    [104, 105],
    [-163, 106],
    [-31, 107],
    [-32, 108],
    [109, 110],
    [-161, 111],
    [-160, -162],
    [112, -34],
    [-33, 113],
    [114, 115],
    [116, 117],
    [118, 119],
    [120, -159],
    [121, 122],
    [123, -158],
    [124, 125],
    [-36, -155],
    [126, 127],
    [-35, 128],
    [129, 130],
    [-157, 131],
    [-156, 132],
    [-37, 133],
    [134, 135],
    [-154, -38],
    [136, 137],
    [-39, -41],
    [138, -153],
    [139, -40],
    [-149, 140],
    [141, 142],
    [143, 144],
    [-151, 145],
    [146, 147],
    [148, -42],
    [-43, 149],
    [150, 151],
    [-152, 152],
    [-46, -98],
    [153, 154],
    [155, -147],
    [156, 157],
    [158, -107],
    [159, 160],
    [-145, -150],
    [-96, 161],
    [162, -45],
    [-146, 163],
    [164, -97],
    [-108, -105],
    [-148, -106],
    [-44, 165],
    [-94, -141],
    [-99, 166],
    [-89, 167],
    [-50, -95],
    [-100, -48],
    [-144, 168],
    [169, 170],
    [-51, -142],
    [-90, -91],
    [-47, -49],
    [171, -53],
    [-93, -143],
    [-137, -138],
    [-55, -101],
    [172, 173],
    [-54, -86],
    [-88, -87],
    [-103, 174],
    [175, -61],
    [-109, 176],
    [177, 178],
    [-52, -139],
    [-57, -140],
    [179, 180],
    [-56, -136],
    [-58, -102],
    [181, 182],
    [-60, -135],
    [183, -104],
    [-128, -134],
    [-92, 184],
    [-59, -62],
    [185, 186],
    [-71, -133],
    [187, -127],
    [-126, 188],
    [-63, -64],
    [-85, -132],
    [189, -66],
    [-121, -125],
    [190, -68],
    [-74, -75],
    [-70, -73],
    [-81, -65],
    [-118, -131],
    [-72, -110],
    [-119, -120],
    [-76, -84],
    [-122, -130],
    [-83, -117],
    [-69, -78],
    [-80, -82],
    [-123, -124],
    [-67, -116],
    [-129, -77],
    [-113, -114],
    [-112, -115],
    [-79, -111],
];

const JOC_HUFF_CODE_5CH_POS_INDEX_SPARSE: &[[i16; 2]] = &[[-1, 1], [2, 3], [-4, -3], [-2, -5]];
const JOC_HUFF_CODE_7CH_POS_INDEX_SPARSE: &[[i16; 2]] =
    &[[-1, 1], [2, 3], [4, 5], [-4, -3], [-2, -5], [-6, -7]];

#[cfg(test)]
mod tests {
    use super::*;

    fn block_updates(count: usize) -> Vec<OamdBlockUpdate> {
        (0..count)
            .map(|index| OamdBlockUpdate {
                offset: index as i16 * 16,
                ramp_duration: 0,
            })
            .collect()
    }

    fn payload_with_object_blocks(object_blocks: Vec<Vec<OamdObjectBlock>>) -> OamdPayload {
        let block_count = object_blocks.first().map(Vec::len).unwrap_or(0);
        OamdPayload {
            version: 0,
            object_count: object_blocks.len(),
            alternate_object_present: false,
            element_count: 1,
            beds: 0,
            bed_instances: 0,
            bed_or_isf_objects: 0,
            dynamic_objects: object_blocks.len(),
            isf_in_use: false,
            isf_index: None,
            bed_assignment: Vec::new(),
            elements: vec![OamdElement {
                element_index: 1,
                byte_length: 0,
                kind: OamdElementKind::Object(OamdObjectElement {
                    sample_offset: 0,
                    block_updates: block_updates(block_count),
                    object_blocks,
                }),
            }],
        }
    }

    fn second_block(payload: &OamdPayload) -> &OamdObjectBlock {
        let OamdElementKind::Object(object) = &payload.elements[0].kind else {
            panic!("expected object element");
        };
        &object.object_blocks[0][1]
    }

    #[test]
    fn oamd_state_reuses_omitted_fields_across_frames() {
        let position = Vec3 {
            x: 0.25,
            y: 0.5,
            z: -0.25,
        };
        let mut initial = payload_with_object_blocks(vec![vec![
            OamdObjectBlock::default(),
            OamdObjectBlock {
                basic_info_status: 3,
                basic_info_blocks: Some(3),
                render_info_status: 3,
                render_info_blocks: Some(13),
                anchor: ObjectAnchor::Screen,
                gain: Some(0.5),
                priority: Some(7),
                valid_position: true,
                differential_position: false,
                position: Some(position),
                distance: Some(4.0),
                size: Some(0.25),
                screen_factor: Some(0.5),
                depth_factor: Some(2.0),
                ..OamdObjectBlock::default()
            },
        ]]);
        let mut omitted = payload_with_object_blocks(vec![vec![
            OamdObjectBlock::default(),
            OamdObjectBlock {
                basic_info_status: 0,
                render_info_status: 0,
                ..OamdObjectBlock::default()
            },
        ]]);

        let mut state = MetadataParseState::default();
        state.oamd.resolve_payload(&mut initial);
        state.oamd.resolve_payload(&mut omitted);

        let resolved = second_block(&omitted);
        assert_eq!(resolved.anchor, ObjectAnchor::Screen);
        assert_eq!(resolved.gain, Some(0.5));
        assert_eq!(resolved.priority, Some(7));
        assert!(resolved.valid_position);
        assert!(!resolved.differential_position);
        assert_eq!(resolved.position, Some(position));
        assert_eq!(resolved.distance, Some(4.0));
        assert_eq!(resolved.size, Some(0.25));
        assert_eq!(resolved.screen_factor, Some(0.5));
        assert_eq!(resolved.depth_factor, Some(2.0));
    }

    #[test]
    fn oamd_state_clears_valid_position_when_render_info_excludes_position() {
        let position = Vec3 {
            x: 0.75,
            y: 0.125,
            z: 0.5,
        };
        let mut first = payload_with_object_blocks(vec![vec![
            OamdObjectBlock::default(),
            OamdObjectBlock {
                basic_info_status: 3,
                basic_info_blocks: Some(3),
                render_info_status: 3,
                render_info_blocks: Some(13),
                anchor: ObjectAnchor::Screen,
                gain: Some(0.8),
                priority: Some(3),
                valid_position: true,
                differential_position: true,
                position: Some(position),
                distance: Some(3.2),
                size: Some(0.4),
                screen_factor: Some(0.625),
                depth_factor: Some(0.5),
                ..OamdObjectBlock::default()
            },
        ]]);
        let mut second = payload_with_object_blocks(vec![vec![
            OamdObjectBlock::default(),
            OamdObjectBlock {
                basic_info_status: 0,
                render_info_status: 3,
                render_info_blocks: Some(4),
                size: Some(0.75),
                ..OamdObjectBlock::default()
            },
        ]]);

        let mut state = MetadataParseState::default();
        state.oamd.resolve_payload(&mut first);
        state.oamd.resolve_payload(&mut second);

        let resolved = second_block(&second);
        assert_eq!(resolved.anchor, ObjectAnchor::Screen);
        assert_eq!(resolved.gain, Some(0.8));
        assert_eq!(resolved.priority, Some(3));
        assert!(!resolved.valid_position);
        assert!(resolved.differential_position);
        assert_eq!(resolved.position, Some(position));
        assert_eq!(resolved.distance, Some(3.2));
        assert_eq!(resolved.size, Some(0.75));
        assert_eq!(resolved.screen_factor, Some(0.625));
        assert_eq!(resolved.depth_factor, Some(0.5));
    }
}
