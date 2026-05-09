// SPDX-License-Identifier: Apache-2.0

#![allow(
    clippy::collapsible_if,
    clippy::needless_range_loop,
    clippy::too_many_arguments
)]

use std::fmt;

use super::allocation::{
    AllocationState, BitAllocationParams, DeltaBitAllocationMode, DeltaBitAllocationState,
    LFE_END_MANTISSA, MantissaDecodeState, MantissaGroupState, grouped_exponent_count,
    sample_rate_index,
};
use super::bitstream::BitReader;
use super::imdct::ImdctState;
use super::metadata::{
    MetadataParseState, ParsedEmdfPayloadData, parse_emdf_payload_body_with_state,
};
use super::pcm::CorePcmFrame;
use crate::BedChannel;
use std::sync::atomic::{AtomicU8, Ordering};
use thiserror::Error;

const EAC3_BLOCKS: [u8; 4] = [1, 2, 3, 6];
const AC3_SAMPLE_RATES: [u32; 3] = [48_000, 44_100, 32_000];
const AC3_CHANNELS: [u8; 8] = [2, 1, 2, 3, 3, 4, 4, 5];
const AC3_FRAME_SIZE_WORDS: [[usize; 3]; 19] = [
    [64, 69, 96],
    [80, 87, 120],
    [96, 104, 144],
    [112, 121, 168],
    [128, 139, 192],
    [160, 174, 240],
    [192, 208, 288],
    [224, 243, 336],
    [256, 278, 384],
    [320, 348, 480],
    [384, 417, 576],
    [448, 487, 672],
    [512, 557, 768],
    [640, 696, 960],
    [768, 835, 1152],
    [896, 975, 1344],
    [1024, 1114, 1536],
    [1152, 1253, 1728],
    [1280, 1393, 1920],
];
const DEF_CPL_BNDSTRC: [bool; 18] = [
    false, false, false, false, false, false, false, false, true, false, true, true, false, true,
    true, true, true, true,
];
const ECPL_SUBBAND_TAB: [usize; 23] = [
    13, 19, 25, 31, 37, 49, 61, 73, 85, 97, 109, 121, 133, 145, 157, 169, 181, 193, 205, 217, 229,
    241, 253,
];
const FRM_EXP_STRATEGIES: [[u8; 6]; 32] = [
    [1, 0, 0, 0, 0, 0],
    [1, 0, 0, 0, 0, 3],
    [1, 0, 0, 0, 2, 0],
    [1, 0, 0, 0, 3, 3],
    [2, 0, 0, 2, 0, 0],
    [2, 0, 0, 2, 0, 3],
    [2, 0, 0, 3, 2, 0],
    [2, 0, 0, 3, 3, 3],
    [2, 0, 1, 0, 0, 0],
    [2, 0, 2, 0, 0, 3],
    [2, 0, 2, 0, 2, 0],
    [2, 0, 2, 0, 3, 3],
    [2, 0, 3, 2, 0, 0],
    [2, 0, 3, 2, 0, 3],
    [2, 0, 3, 3, 2, 0],
    [2, 0, 3, 3, 3, 3],
    [3, 1, 0, 0, 0, 0],
    [3, 1, 0, 0, 0, 3],
    [3, 2, 0, 0, 2, 0],
    [3, 2, 0, 0, 3, 3],
    [3, 2, 0, 2, 0, 0],
    [3, 2, 0, 2, 0, 3],
    [3, 2, 0, 3, 2, 0],
    [3, 2, 0, 3, 3, 3],
    [3, 3, 1, 0, 0, 0],
    [3, 3, 2, 0, 0, 3],
    [3, 3, 2, 0, 2, 0],
    [3, 3, 2, 0, 3, 3],
    [3, 3, 3, 2, 0, 0],
    [3, 3, 3, 2, 0, 3],
    [3, 3, 3, 3, 2, 0],
    [3, 3, 3, 3, 3, 3],
];

const LOG_LEVEL_ERROR: u8 = 1;
const LOG_LEVEL_WARN: u8 = 2;
const LOG_LEVEL_INFO: u8 = 3;
const LOG_LEVEL_DEBUG: u8 = 4;
const LOG_LEVEL_TRACE: u8 = 5;

static AUX_LOG_LEVEL: AtomicU8 = AtomicU8::new(LOG_LEVEL_DEBUG);

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

pub(crate) fn set_aux_log_level(level: log::Level) {
    AUX_LOG_LEVEL.store(encode_log_level(level), Ordering::Relaxed);
}

fn aux_log_level() -> log::Level {
    decode_log_level(AUX_LOG_LEVEL.load(Ordering::Relaxed))
}

fn emit_aux_debug(args: fmt::Arguments<'_>) {
    log::log!(target: "starmine_ad::eac3dec::aux", aux_log_level(), "{args}");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// E-AC-3 frame coding mode.
pub enum FrameType {
    LegacyAc3,
    Independent,
    Dependent,
    Ac3Convert,
}

impl fmt::Display for FrameType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameType::LegacyAc3 => f.write_str("legacy-ac3"),
            FrameType::Independent => f.write_str("independent"),
            FrameType::Dependent => f.write_str("dependent"),
            FrameType::Ac3Convert => f.write_str("ac3-convert"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Where the EMDF payloads were recovered from for this access unit.
pub enum EmdfSource {
    None,
    AuxData,
    FrameScanFallback,
}

impl fmt::Display for EmdfSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EmdfSource::None => f.write_str("none"),
            EmdfSource::AuxData => f.write_str("aux-data"),
            EmdfSource::FrameScanFallback => f.write_str("frame-scan"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Outcome of the auxiliary-data extraction path.
pub enum AuxParseStatus {
    Disabled,
    Extracted,
    SyncAnchoredRecovery,
    NoBlockStartInfo,
    UnsupportedSyntax,
    SyntaxMismatch,
}

impl fmt::Display for AuxParseStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuxParseStatus::Disabled => f.write_str("disabled"),
            AuxParseStatus::Extracted => f.write_str("extracted"),
            AuxParseStatus::SyncAnchoredRecovery => f.write_str("sync-anchored"),
            AuxParseStatus::NoBlockStartInfo => f.write_str("no-blkstart"),
            AuxParseStatus::UnsupportedSyntax => f.write_str("unsupported"),
            AuxParseStatus::SyntaxMismatch => f.write_str("syntax-mismatch"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpStrategy {
    Reuse,
    D15,
    D25,
    D45,
}

impl ExpStrategy {
    fn from_bits(bits: u32) -> Result<Self, ParseError> {
        match bits {
            0 => Ok(Self::Reuse),
            1 => Ok(Self::D15),
            2 => Ok(Self::D25),
            3 => Ok(Self::D45),
            _ => Err(ParseError::InvalidHeader("expstr")),
        }
    }

    fn from_frame_code(code: u8, block: usize) -> Result<Self, ParseError> {
        FRM_EXP_STRATEGIES
            .get(code as usize)
            .and_then(|row| row.get(block))
            .copied()
            .ok_or(ParseError::InvalidHeader("frm-expstr"))
            .and_then(|value| Self::from_bits(value as u32))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
/// Parse errors returned by the low-level access-unit inspection and decode helpers.
pub enum ParseError {
    #[error("short-packet")]
    ShortPacket,
    #[error("bad-syncword")]
    BadSyncword,
    #[error("not-eac3")]
    NotEac3,
    #[error("invalid-header:{0}")]
    InvalidHeader(&'static str),
    #[error("unsupported-feature:{0}")]
    UnsupportedFeature(&'static str),
    #[error("truncated-frame expected={expected} available={available}")]
    TruncatedFrame { expected: usize, available: usize },
    #[error("trailing-data expected={expected} provided={provided}")]
    TrailingData { expected: usize, provided: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Lightweight description of one EMDF payload inside an access unit.
pub struct PayloadInfo {
    pub emdf_block_index: usize,
    pub payload_id: u8,
    pub payload_size_bytes: usize,
    pub sample_offset: Option<u16>,
}

impl PayloadInfo {
    /// Human-readable payload kind derived from `payload_id`.
    pub fn payload_name(&self) -> &'static str {
        payload_name(self.payload_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// One recovered skip-field payload.
pub struct SkipFieldInfo {
    pub block_index: Option<usize>,
    pub bit_offset: usize,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
/// Parsed EMDF payload plus its raw bytes.
pub struct EmdfPayloadInfo {
    pub info: PayloadInfo,
    pub bytes: Vec<u8>,
    pub parsed: ParsedEmdfPayloadData,
    pub parse_error: Option<ParseError>,
}

impl EmdfPayloadInfo {
    /// Human-readable payload kind derived from [`PayloadInfo::payload_id`].
    pub fn payload_name(&self) -> &'static str {
        self.info.payload_name()
    }

    /// Short one-line summary suitable for logs and debugging output.
    pub fn short_summary(&self) -> Option<String> {
        self.parsed.short_summary()
    }
}

#[derive(Debug, Clone, PartialEq)]
/// One EMDF block recovered from the frame or auxiliary data.
pub struct EmdfBlockInfo {
    pub sync_offset: usize,
    pub payloads: Vec<EmdfPayloadInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransientProcessorInfo {
    pub location: u16,
    pub length: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrameInfo {
    pub exponent_strategies_embedded: bool,
    pub adaptive_hybrid_transform_enabled: bool,
    pub snr_offset_strategy: u8,
    pub transient_processing_enabled: bool,
    pub block_switching_enabled: bool,
    pub dithering_enabled: bool,
    pub bit_allocation_mode_enabled: bool,
    pub frame_gain_syntax_enabled: bool,
    pub delta_bit_allocation_enabled: bool,
    pub skip_field_syntax_enabled: bool,
    pub spectral_extension_attenuation_enabled: bool,
    pub coupling_strategy_updates: Vec<bool>,
    pub coupling_in_use: Vec<bool>,
    pub coupling_exponent_strategy: Vec<Option<ExpStrategy>>,
    pub channel_exponent_strategy: Vec<Vec<ExpStrategy>>,
    pub lfe_exponent_strategy: Vec<bool>,
    pub converter_exponent_strategy_present: bool,
    pub converter_exponent_strategy: Vec<u8>,
    pub frame_csnr_offset: Option<u8>,
    pub frame_fsnr_offset: Option<u8>,
    pub transient_processors: Vec<Option<TransientProcessorInfo>>,
    pub spectral_extension_attenuation: Vec<Option<u8>>,
    pub block_start_info_present: bool,
    pub block_start_info_bit_len: usize,
    pub block_payload_start_bit_offset: usize,
}

#[derive(Debug, Clone, PartialEq)]
/// Parsed summary for one complete E-AC-3 access unit.
///
/// The struct keeps both header-level information and any recovered EMDF payloads so callers can
/// choose how deep they want to inspect the frame before moving on to PCM decoding.
pub struct AccessUnitInfo {
    pub frame_size: usize,
    pub bitstream_id: u8,
    pub frame_type: FrameType,
    pub substreamid: u8,
    pub sample_rate: u32,
    pub num_blocks: u8,
    pub channel_mode: u8,
    pub channels: u8,
    pub fullband_channels: u8,
    pub lfe_on: bool,
    pub addbsi_present: bool,
    pub extension_type_a: bool,
    pub complexity_index_type_a: u8,
    pub mixing_metadata_present: bool,
    pub informational_metadata_present: bool,
    pub addbsi_bytes: Vec<u8>,
    pub body_start_bit_offset: usize,
    pub audio_frame: AudioFrameInfo,
    pub skip_fields: Vec<SkipFieldInfo>,
    pub trailing_aux_data: Vec<u8>,
    pub aux_data: Vec<u8>,
    pub aux_parse_status: AuxParseStatus,
    pub emdf_source: EmdfSource,
    pub emdf_blocks: Vec<EmdfBlockInfo>,
    pub emdf_block_count: usize,
    pub first_emdf_sync_offset: Option<usize>,
}

impl AccessUnitInfo {
    /// Iterate over all EMDF payloads in block order.
    pub fn payloads(&self) -> impl Iterator<Item = &EmdfPayloadInfo> {
        self.emdf_blocks
            .iter()
            .flat_map(|block| block.payloads.iter())
    }

    /// Number of EMDF payloads that failed to parse after their bytes were recovered.
    pub fn payload_parse_error_count(&self) -> usize {
        self.payloads()
            .filter(|payload| payload.parse_error.is_some())
            .count()
    }

    /// Number of JOC payloads present in this access unit.
    pub fn joc_payload_count(&self) -> usize {
        self.payloads()
            .filter(|payload| payload.info.payload_id == 14)
            .count()
    }

    /// Number of OAMD payloads present in this access unit.
    pub fn oamd_payload_count(&self) -> usize {
        self.payloads()
            .filter(|payload| payload.info.payload_id == 11)
            .count()
    }

    /// Total recovered skip-field bytes across all blocks.
    pub fn skip_field_bytes_len(&self) -> usize {
        self.skip_fields.iter().map(|field| field.bytes.len()).sum()
    }

    /// Compact text summary intended for logs and offline comparison tools.
    pub fn summary(&self) -> String {
        let coupling_blocks = self
            .audio_frame
            .coupling_in_use
            .iter()
            .filter(|in_use| **in_use)
            .count();

        let mut out = format!(
            "frame={} bsid={} type={} ssid={} sr={} blocks={} acmod={} ch={} lfe={} addbsi={} extA={} complexity={} body={}bit block0={}bit expmode={} skipfield={} cpl={}/{} aux={}B skip={}B/{}blk auxparse={} emdfsrc={} emdf={} payloads={} joc={} oamd={}",
            self.frame_size,
            self.bitstream_id,
            self.frame_type,
            self.substreamid,
            self.sample_rate,
            self.num_blocks,
            self.channel_mode,
            self.channels,
            if self.lfe_on { 1 } else { 0 },
            if self.addbsi_present { 1 } else { 0 },
            if self.extension_type_a { 1 } else { 0 },
            self.complexity_index_type_a,
            self.body_start_bit_offset,
            self.audio_frame.block_payload_start_bit_offset,
            if self.audio_frame.exponent_strategies_embedded {
                "per-block"
            } else {
                "frame-code"
            },
            if self.audio_frame.skip_field_syntax_enabled {
                1
            } else {
                0
            },
            coupling_blocks,
            self.num_blocks,
            self.aux_data.len(),
            self.skip_field_bytes_len(),
            self.skip_fields.len(),
            self.aux_parse_status,
            self.emdf_source,
            self.emdf_block_count,
            self.payloads().count(),
            self.joc_payload_count(),
            self.oamd_payload_count(),
        );

        let parse_error_count = self.payload_parse_error_count();
        if parse_error_count != 0 {
            out.push_str(&format!(" emdferr={parse_error_count}"));
        }

        if let Some(first_sync) = self.first_emdf_sync_offset {
            out.push_str(&format!(" first_sync={first_sync}"));
        }
        let mut payloads = self.payloads().peekable();
        if payloads.peek().is_some() {
            out.push_str(" payloads=[");
            for (index, payload) in payloads.enumerate() {
                if index != 0 {
                    out.push(',');
                }
                out.push_str(&format!(
                    "{}/{}:{}B",
                    payload.info.payload_id,
                    payload.payload_name(),
                    payload.info.payload_size_bytes
                ));
                if let Some(sample_offset) = payload.info.sample_offset {
                    out.push_str(&format!("@{sample_offset}"));
                }
                if let Some(summary) = payload.short_summary() {
                    out.push('{');
                    out.push_str(&summary);
                    out.push('}');
                }
                if let Some(err) = &payload.parse_error {
                    out.push('!');
                    out.push_str(&err.to_string());
                }
            }
            out.push(']');
        }
        out
    }
}

struct ParsedAudioFrame {
    info: AudioFrameInfo,
    block_start_bit_offsets: Option<Vec<usize>>,
}

struct BlockAllocationInfo {
    channel_end_mantissas: Vec<usize>,
}

#[derive(Debug, Default)]
struct TrailingAuxDataInfo {
    start_bit_offset: usize,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
struct BlockSyntaxState {
    bit_allocation_params: BitAllocationParams,
    coupling_allocation: AllocationState,
    coupling_coordinates: Vec<[f32; DEF_CPL_BNDSTRC.len()]>,
    coupling_delta_bit_allocation: DeltaBitAllocationState,
    channel_allocations: Vec<AllocationState>,
    channel_delta_bit_allocation: Vec<DeltaBitAllocationState>,
    cpl_fast_leak: i32,
    cpl_fgain_code: u8,
    cpl_fsnr_offset: i32,
    cpl_slow_leak: i32,
    channel_fgain_codes: Vec<u8>,
    channel_fsnr_offsets: Vec<i32>,
    chbwcod: Vec<u8>,
    chincpl: Vec<bool>,
    chinspx: Vec<bool>,
    csnr_offset: i32,
    cplbegf: usize,
    cpl_band_struct: [bool; DEF_CPL_BNDSTRC.len()],
    cplendf: usize,
    ecplinu: bool,
    first_cpl_coords: Vec<bool>,
    first_cpl_leak: bool,
    first_spx_coords: Vec<bool>,
    lfe_allocation: Option<AllocationState>,
    lfe_fgain_code: u8,
    lfe_fsnr_offset: i32,
    ncplbnd: usize,
    ncplsubnd: usize,
    phsflginu: bool,
    sample_rate_index: usize,
    spxbegf: usize,
    spx_begin_subbnd: usize,
    spx_end_subbnd: usize,
    spx_in_use: bool,
    nspxbnds: usize,
    spx_master: Vec<u8>,
    spx_gains: Vec<Vec<f32>>,
    spx_noise_blend: Vec<bool>,
}

impl BlockSyntaxState {
    fn new(fullband_channels: usize, lfe_on: bool, sample_rate_index: usize) -> Self {
        Self {
            bit_allocation_params: BitAllocationParams::default(),
            coupling_allocation: AllocationState::new(),
            coupling_coordinates: vec![[0.0; DEF_CPL_BNDSTRC.len()]; fullband_channels],
            coupling_delta_bit_allocation: DeltaBitAllocationState::default(),
            channel_allocations: (0..fullband_channels)
                .map(|_| AllocationState::new())
                .collect(),
            channel_delta_bit_allocation: (0..fullband_channels)
                .map(|_| DeltaBitAllocationState::default())
                .collect(),
            cpl_fast_leak: 0,
            cpl_fgain_code: 4,
            cpl_fsnr_offset: 0,
            cpl_slow_leak: 0,
            channel_fgain_codes: vec![4; fullband_channels],
            channel_fsnr_offsets: vec![0; fullband_channels],
            chbwcod: vec![0; fullband_channels],
            chincpl: vec![false; fullband_channels],
            chinspx: vec![false; fullband_channels],
            csnr_offset: 0,
            cplbegf: 0,
            cpl_band_struct: DEF_CPL_BNDSTRC,
            cplendf: 0,
            ecplinu: false,
            first_cpl_coords: vec![true; fullband_channels],
            first_cpl_leak: true,
            first_spx_coords: vec![true; fullband_channels],
            lfe_allocation: lfe_on.then(AllocationState::new),
            lfe_fgain_code: 4,
            lfe_fsnr_offset: 0,
            ncplbnd: 0,
            ncplsubnd: 0,
            phsflginu: false,
            sample_rate_index,
            spxbegf: 0,
            spx_begin_subbnd: 0,
            spx_end_subbnd: 0,
            spx_in_use: false,
            nspxbnds: 0,
            spx_master: Vec::new(),
            spx_gains: Vec::new(),
            spx_noise_blend: Vec::new(),
        }
    }

    fn clear_spx(&mut self) {
        self.spx_in_use = false;
        self.nspxbnds = 0;
        self.spx_begin_subbnd = 0;
        self.spx_end_subbnd = 0;
        self.spxbegf = 0;
        self.spx_master.clear();
        self.spx_gains.clear();
        self.spx_noise_blend.clear();
        for in_use in &mut self.chinspx {
            *in_use = false;
        }
        for first in &mut self.first_spx_coords {
            *first = true;
        }
    }

    fn clear_coupling(&mut self) {
        self.ecplinu = false;
        self.cplbegf = 0;
        self.cplendf = 0;
        self.cpl_fast_leak = 0;
        self.cpl_fgain_code = 4;
        self.cpl_fsnr_offset = 0;
        self.cpl_slow_leak = 0;
        self.phsflginu = false;
        self.ncplbnd = 0;
        self.ncplsubnd = 0;
        self.coupling_delta_bit_allocation = DeltaBitAllocationState::default();
        self.coupling_allocation.clear_bap();
        for in_use in &mut self.chincpl {
            *in_use = false;
        }
        for first in &mut self.first_cpl_coords {
            *first = true;
        }
        self.first_cpl_leak = true;
    }
}

#[derive(Debug, Default)]
pub(crate) struct AuxDataDecodeState {
    fullband_channels: usize,
    lfe_on: bool,
    sample_rate_index: Option<usize>,
    block_syntax: Option<BlockSyntaxState>,
}

impl AuxDataDecodeState {
    pub(crate) fn reset(&mut self) {
        self.fullband_channels = 0;
        self.lfe_on = false;
        self.sample_rate_index = None;
        self.block_syntax = None;
    }

    fn prepare_block_syntax(
        &mut self,
        fullband_channels: usize,
        lfe_on: bool,
        sample_rate_index: usize,
    ) -> BlockSyntaxState {
        let needs_reset = self.sample_rate_index != Some(sample_rate_index)
            || self.fullband_channels != fullband_channels
            || self.lfe_on != lfe_on
            || self.block_syntax.is_none();
        if needs_reset {
            self.fullband_channels = fullband_channels;
            self.lfe_on = lfe_on;
            self.sample_rate_index = Some(sample_rate_index);
            self.block_syntax = Some(BlockSyntaxState::new(
                fullband_channels,
                lfe_on,
                sample_rate_index,
            ));
        }
        self.block_syntax
            .clone()
            .unwrap_or_else(|| BlockSyntaxState::new(fullband_channels, lfe_on, sample_rate_index))
    }

    fn commit_block_syntax(&mut self, state: BlockSyntaxState) {
        self.block_syntax = Some(state);
    }
}

#[derive(Debug, Default)]
pub(crate) struct CoreDecodeState {
    fullband_channels: usize,
    lfe_on: bool,
    sample_rate_index: Option<usize>,
    block_syntax: Option<BlockSyntaxState>,
    imdct: Vec<ImdctState>,
    lfe_imdct: Option<ImdctState>,
}

impl CoreDecodeState {
    pub(crate) fn reset(&mut self) {
        self.fullband_channels = 0;
        self.lfe_on = false;
        self.sample_rate_index = None;
        self.block_syntax = None;
        self.imdct.clear();
        self.lfe_imdct = None;
    }

    fn reconfigure(&mut self, fullband_channels: usize, lfe_on: bool, sample_rate_index: usize) {
        let needs_reset = self.sample_rate_index != Some(sample_rate_index)
            || self.fullband_channels != fullband_channels
            || self.lfe_on != lfe_on
            || self.block_syntax.is_none();
        if needs_reset {
            self.fullband_channels = fullband_channels;
            self.lfe_on = lfe_on;
            self.sample_rate_index = Some(sample_rate_index);
            self.block_syntax = Some(BlockSyntaxState::new(
                fullband_channels,
                lfe_on,
                sample_rate_index,
            ));
            self.imdct = (0..fullband_channels).map(|_| ImdctState::new()).collect();
            self.lfe_imdct = lfe_on.then(ImdctState::new);
        }
    }
}

/// Parse one complete access unit without keeping any cross-frame state.
///
/// Use this helper for one-off inspection, tests, or tools that already manage stream boundaries
/// externally. Stateful callers should prefer [`crate::Decoder`].
pub fn inspect_access_unit(data: &[u8]) -> Result<AccessUnitInfo, ParseError> {
    let mut metadata_state = MetadataParseState::default();
    inspect_access_unit_with_metadata_state(data, &mut metadata_state, None)
}

/// Parse one complete legacy AC-3 access unit.
///
/// This path is intentionally narrow: it exposes the core PCM syntax needed by
/// E-AC-3 dependent/JOC streams that carry their 5.1 core as a legacy AC-3
/// syncframe.  Metadata/aux payload recovery remains E-AC-3 only.
pub fn inspect_legacy_ac3_access_unit(data: &[u8]) -> Result<AccessUnitInfo, ParseError> {
    if data.len() < 7 {
        return Err(ParseError::ShortPacket);
    }

    let mut reader = BitReader::new(data);
    let sync = reader.read_bits(16).ok_or(ParseError::ShortPacket)?;
    if sync != 0x0B77 {
        return Err(ParseError::BadSyncword);
    }

    reader.skip_bits(16).ok_or(ParseError::ShortPacket)?; // crc1
    let fscod = reader.read_bits(2).ok_or(ParseError::ShortPacket)? as u8;
    let frmsizecod = reader.read_bits(6).ok_or(ParseError::ShortPacket)? as u8;
    let sample_rate = *AC3_SAMPLE_RATES
        .get(usize::from(fscod))
        .ok_or(ParseError::InvalidHeader("sample-rate"))?;
    let frame_size = legacy_ac3_frame_size(fscod, frmsizecod)?;
    if data.len() < frame_size {
        return Err(ParseError::TruncatedFrame {
            expected: frame_size,
            available: data.len(),
        });
    }

    let frame = &data[..frame_size];
    let mut reader = BitReader::with_offset(frame, 40);
    let bitstream_id = reader.read_bits(5).ok_or(ParseError::ShortPacket)? as u8;
    if bitstream_id > 10 {
        return Err(ParseError::InvalidHeader("legacy-bsid"));
    }
    let substreamid = reader.read_bits(3).ok_or(ParseError::ShortPacket)? as u8; // bsmod
    let channel_mode = reader.read_bits(3).ok_or(ParseError::ShortPacket)? as u8;
    if (channel_mode & 0x01) != 0 && channel_mode != 1 {
        reader.skip_bits(2).ok_or(ParseError::ShortPacket)?; // cmixlev
    }
    if (channel_mode & 0x04) != 0 {
        reader.skip_bits(2).ok_or(ParseError::ShortPacket)?; // surmixlev
    }
    if channel_mode == 2 {
        reader.skip_bits(2).ok_or(ParseError::ShortPacket)?; // dsurmod
    }
    let lfe_on = reader.read_bit().ok_or(ParseError::ShortPacket)?;
    let fullband_channels = AC3_CHANNELS[channel_mode as usize];
    let channels = fullband_channels + u8::from(lfe_on);

    skip_legacy_ac3_bsi_tail(&mut reader, channel_mode)?;
    let body_start_bit_offset = reader.position();
    let audio_frame =
        legacy_ac3_audio_frame_info(body_start_bit_offset, fullband_channels as usize, lfe_on);

    Ok(AccessUnitInfo {
        frame_size,
        bitstream_id,
        frame_type: FrameType::LegacyAc3,
        substreamid,
        sample_rate,
        num_blocks: 6,
        channel_mode,
        channels,
        fullband_channels,
        lfe_on,
        addbsi_present: false,
        extension_type_a: false,
        complexity_index_type_a: 0,
        mixing_metadata_present: false,
        informational_metadata_present: false,
        addbsi_bytes: Vec::new(),
        body_start_bit_offset,
        audio_frame,
        skip_fields: Vec::new(),
        trailing_aux_data: Vec::new(),
        aux_data: Vec::new(),
        aux_parse_status: AuxParseStatus::Disabled,
        emdf_source: EmdfSource::None,
        emdf_blocks: Vec::new(),
        emdf_block_count: 0,
        first_emdf_sync_offset: None,
    })
}

pub(crate) fn inspect_access_unit_with_metadata_state(
    data: &[u8],
    metadata_state: &mut MetadataParseState,
    mut aux_state: Option<&mut AuxDataDecodeState>,
) -> Result<AccessUnitInfo, ParseError> {
    if data.len() < 7 {
        return Err(ParseError::ShortPacket);
    }

    let mut reader = BitReader::new(data);
    let sync = reader.read_bits(16).ok_or(ParseError::ShortPacket)?;
    if sync != 0x0B77 {
        return Err(ParseError::BadSyncword);
    }

    let bitstream_id = (reader.show_bits(29).ok_or(ParseError::ShortPacket)? & 0x1F) as u8;
    if bitstream_id <= 10 {
        return Err(ParseError::NotEac3);
    }
    if bitstream_id > 16 {
        return Err(ParseError::InvalidHeader("bsid"));
    }

    reader.skip_bits(2).ok_or(ParseError::ShortPacket)?;
    reader.skip_bits(3).ok_or(ParseError::ShortPacket)?;
    let frame_size = ((reader.read_bits(11).ok_or(ParseError::ShortPacket)? as usize) + 1) << 1;
    if frame_size < 2 {
        return Err(ParseError::InvalidHeader("frame-size"));
    }

    let frame = &data[..frame_size.min(data.len())];
    let mut reader = BitReader::new(frame);
    let sync = reader.read_bits(16).ok_or(ParseError::ShortPacket)?;
    if sync != 0x0B77 {
        return Err(ParseError::BadSyncword);
    }

    let frame_type = match reader.read_bits(2).ok_or(ParseError::ShortPacket)? {
        0 => FrameType::Independent,
        1 => FrameType::Dependent,
        2 => FrameType::Ac3Convert,
        _ => return Err(ParseError::InvalidHeader("frame-type")),
    };

    let substreamid = reader.read_bits(3).ok_or(ParseError::ShortPacket)? as u8;
    let frame_size_again =
        ((reader.read_bits(11).ok_or(ParseError::ShortPacket)? as usize) + 1) << 1;
    debug_assert_eq!(frame_size_again, frame_size);

    let sr_code = reader.read_bits(2).ok_or(ParseError::ShortPacket)?;
    let (sample_rate, num_blocks) = if sr_code == 3 {
        let sr_code2 = reader.read_bits(2).ok_or(ParseError::ShortPacket)?;
        if sr_code2 == 3 {
            return Err(ParseError::InvalidHeader("sample-rate"));
        }
        (AC3_SAMPLE_RATES[sr_code2 as usize] / 2, 6)
    } else {
        let num_blocks_code = reader.read_bits(2).ok_or(ParseError::ShortPacket)?;
        (
            AC3_SAMPLE_RATES[sr_code as usize],
            EAC3_BLOCKS[num_blocks_code as usize],
        )
    };
    let sample_rate_index =
        sample_rate_index(sample_rate).ok_or(ParseError::InvalidHeader("sample-rate"))?;

    let channel_mode = reader.read_bits(3).ok_or(ParseError::ShortPacket)? as u8;
    let lfe_on = reader.read_bit().ok_or(ParseError::ShortPacket)?;
    let fullband_channels = AC3_CHANNELS[channel_mode as usize];
    let channels = fullband_channels + if lfe_on { 1 } else { 0 };

    reader.skip_bits(5).ok_or(ParseError::ShortPacket)?;

    let volume_programs = if channel_mode == 0 { 2 } else { 1 };
    for _ in 0..volume_programs {
        reader.skip_bits(5).ok_or(ParseError::ShortPacket)?;
        if reader.read_bit().ok_or(ParseError::ShortPacket)? {
            reader.skip_bits(8).ok_or(ParseError::ShortPacket)?;
        }
    }

    if matches!(frame_type, FrameType::Dependent)
        && reader.read_bit().ok_or(ParseError::ShortPacket)?
    {
        reader.skip_bits(16).ok_or(ParseError::ShortPacket)?;
    }

    let mixing_metadata_present = read_mixing_metadata(
        &mut reader,
        frame_type,
        channel_mode,
        lfe_on,
        num_blocks as usize,
        volume_programs,
    )?;

    let informational_metadata_present =
        read_informational_metadata(&mut reader, channel_mode, sample_rate, volume_programs)?;

    if matches!(frame_type, FrameType::Independent) && num_blocks != 6 {
        reader.skip_bits(1).ok_or(ParseError::ShortPacket)?;
    }

    if matches!(frame_type, FrameType::Ac3Convert) {
        let has_original_size = if num_blocks == 6 {
            true
        } else {
            reader.read_bit().ok_or(ParseError::ShortPacket)?
        };
        if has_original_size {
            reader.skip_bits(6).ok_or(ParseError::ShortPacket)?;
        }
    }

    let mut addbsi_bytes = Vec::new();
    let mut addbsi_present = false;
    if reader.read_bit().ok_or(ParseError::ShortPacket)? {
        addbsi_present = true;
        let addbsi_len = reader.read_bits(6).ok_or(ParseError::ShortPacket)? as usize + 1;
        addbsi_bytes = reader
            .read_bytes(addbsi_len)
            .ok_or(ParseError::ShortPacket)?;
    }

    let extension_type_a = addbsi_bytes.first().is_some_and(|byte| (byte & 0x01) != 0);
    let complexity_index_type_a = if extension_type_a {
        addbsi_bytes.get(1).copied().unwrap_or_default()
    } else {
        0
    };

    let body_start_bit_offset = reader.position();
    let mut body_reader = reader;
    body_reader.set_limit_bits(frame.len() * 8);
    let audio_frame = parse_audio_frame(
        &mut body_reader,
        frame_type,
        frame_size / 2,
        num_blocks as usize,
        channel_mode,
        fullband_channels as usize,
        lfe_on,
    )?;

    let trailing_aux_data = extract_trailing_aux_data(frame);
    let mut skip_fields = Vec::new();
    let mut aux_parse_status = AuxParseStatus::Disabled;
    if audio_frame.info.skip_field_syntax_enabled {
        if num_blocks == 1 || audio_frame.block_start_bit_offsets.is_some() {
            match collect_skip_fields(
                frame,
                frame_type,
                num_blocks as usize,
                channel_mode,
                fullband_channels as usize,
                lfe_on,
                &audio_frame.info,
                audio_frame.block_start_bit_offsets.as_deref(),
                trailing_aux_data.start_bit_offset,
                sample_rate_index,
            ) {
                Ok(fields) => {
                    skip_fields = fields;
                    aux_parse_status = AuxParseStatus::Extracted;
                }
                // TODO: Implement the remaining block syntaxes so real aux extraction works
                // without falling back to frame scanning on these streams.
                Err(ParseError::UnsupportedFeature(_)) => {
                    aux_parse_status = AuxParseStatus::UnsupportedSyntax
                }
                // TODO: Once the block walker covers more syntax, promote unexpected
                // syntax mismatches to hard parse failures instead of silent fallback.
                Err(ParseError::ShortPacket)
                | Err(ParseError::InvalidHeader("block-start-info")) => {
                    aux_parse_status = AuxParseStatus::SyntaxMismatch;
                }
                Err(err) => return Err(err),
            }
        } else {
            let mut walk_state = if let Some(state) = aux_state.as_deref_mut() {
                state.prepare_block_syntax(fullband_channels as usize, lfe_on, sample_rate_index)
            } else {
                BlockSyntaxState::new(fullband_channels as usize, lfe_on, sample_rate_index)
            };
            match collect_skip_fields_without_block_start(
                frame,
                frame_type,
                num_blocks as usize,
                channel_mode,
                fullband_channels as usize,
                lfe_on,
                &audio_frame.info,
                trailing_aux_data.start_bit_offset,
                &mut walk_state,
            ) {
                Ok(fields) => {
                    skip_fields = fields;
                    aux_parse_status = AuxParseStatus::Extracted;
                    if let Some(state) = aux_state.as_deref_mut() {
                        state.commit_block_syntax(walk_state);
                    }
                }
                Err(err @ ParseError::UnsupportedFeature(_)) => {
                    emit_aux_debug(format_args!(
                        "no-blkstart frame sequential parse error: {err}"
                    ));
                    // TODO: Delete this EMDF-anchored fallback once no-blkstrtinfo walking
                    // covers coupling/SPX and other remaining unsupported syntaxes.
                    if let Some(recovered_fields) = recover_skip_fields_from_emdf_markers(frame) {
                        skip_fields = recovered_fields;
                        aux_parse_status = AuxParseStatus::SyncAnchoredRecovery;
                    } else {
                        aux_parse_status = AuxParseStatus::UnsupportedSyntax;
                    }
                }
                Err(
                    err @ (ParseError::ShortPacket
                    | ParseError::InvalidHeader("block-end")
                    | ParseError::InvalidHeader("mantissa-range")),
                ) => {
                    emit_aux_debug(format_args!(
                        "no-blkstart frame sequential parse error: {err}"
                    ));
                    if let Some(recovered_fields) = recover_skip_fields_from_emdf_markers(frame) {
                        skip_fields = recovered_fields;
                        aux_parse_status = AuxParseStatus::SyncAnchoredRecovery;
                    } else {
                        aux_parse_status = AuxParseStatus::NoBlockStartInfo;
                    }
                }
                Err(err) => return Err(err),
            }
        }
    }

    let mut aux_data = Vec::new();
    for field in &skip_fields {
        aux_data.extend_from_slice(&field.bytes);
    }
    aux_data.extend_from_slice(&trailing_aux_data.bytes);

    let (emdf_source, emdf_blocks) = if aux_data.is_empty() {
        scan_frame_for_emdf(frame, metadata_state)
    } else {
        let emdf_blocks = scan_emdf_blocks_with_metadata_state(&aux_data, metadata_state);
        if emdf_blocks.is_empty() {
            scan_frame_for_emdf(frame, metadata_state)
        } else {
            (EmdfSource::AuxData, emdf_blocks)
        }
    };
    let first_emdf_sync_offset = emdf_blocks.first().map(|block| block.sync_offset);

    Ok(AccessUnitInfo {
        frame_size,
        bitstream_id,
        frame_type,
        substreamid,
        sample_rate,
        num_blocks,
        channel_mode,
        channels,
        fullband_channels,
        lfe_on,
        addbsi_present,
        extension_type_a,
        complexity_index_type_a,
        mixing_metadata_present,
        informational_metadata_present,
        addbsi_bytes,
        body_start_bit_offset,
        audio_frame: audio_frame.info,
        skip_fields,
        trailing_aux_data: trailing_aux_data.bytes,
        aux_data,
        aux_parse_status,
        emdf_source,
        emdf_block_count: emdf_blocks.len(),
        first_emdf_sync_offset,
        emdf_blocks,
    })
}

fn legacy_ac3_frame_size(fscod: u8, frmsizecod: u8) -> Result<usize, ParseError> {
    let bitrate_index = usize::from(frmsizecod >> 1);
    if fscod > 2 || bitrate_index >= AC3_FRAME_SIZE_WORDS.len() {
        return Err(ParseError::InvalidHeader("frame-size"));
    }
    Ok(AC3_FRAME_SIZE_WORDS[bitrate_index][usize::from(fscod)] * 2)
}

fn skip_legacy_ac3_bsi_tail(
    reader: &mut BitReader<'_>,
    channel_mode: u8,
) -> Result<(), ParseError> {
    let dual_mono = channel_mode == 0;
    reader.skip_bits(5).ok_or(ParseError::ShortPacket)?; // dialnorm
    skip_conditional_bits(reader, 8)?; // compre/compr
    if dual_mono {
        reader.skip_bits(5).ok_or(ParseError::ShortPacket)?; // dialnorm2
        skip_conditional_bits(reader, 8)?; // compr2e/compr2
    }
    skip_conditional_bits(reader, 8)?; // langcode/langcod
    if dual_mono {
        skip_conditional_bits(reader, 8)?; // langcod2e/langcod2
    }
    if reader.read_bit().ok_or(ParseError::ShortPacket)? {
        reader.skip_bits(7).ok_or(ParseError::ShortPacket)?; // mixlevel + roomtyp
    }
    if dual_mono && reader.read_bit().ok_or(ParseError::ShortPacket)? {
        reader.skip_bits(7).ok_or(ParseError::ShortPacket)?; // mixlevel2 + roomtyp2
    }
    reader.skip_bits(2).ok_or(ParseError::ShortPacket)?; // copyrightb + origbs
    skip_conditional_bits(reader, 14)?; // timecod1e/timecod1
    skip_conditional_bits(reader, 14)?; // timecod2e/timecod2
    if reader.read_bit().ok_or(ParseError::ShortPacket)? {
        let addbsil = reader.read_bits(6).ok_or(ParseError::ShortPacket)? as usize + 1;
        reader
            .skip_bits(addbsil * 8)
            .ok_or(ParseError::ShortPacket)?;
    }
    Ok(())
}

fn legacy_ac3_audio_frame_info(
    block_payload_start_bit_offset: usize,
    fullband_channels: usize,
    lfe_on: bool,
) -> AudioFrameInfo {
    let num_blocks = 6usize;
    let lfe_exponent_strategy = if lfe_on {
        vec![false; num_blocks]
    } else {
        Vec::new()
    };
    AudioFrameInfo {
        exponent_strategies_embedded: true,
        adaptive_hybrid_transform_enabled: false,
        snr_offset_strategy: 2,
        transient_processing_enabled: false,
        block_switching_enabled: true,
        dithering_enabled: true,
        bit_allocation_mode_enabled: true,
        frame_gain_syntax_enabled: false,
        delta_bit_allocation_enabled: true,
        skip_field_syntax_enabled: true,
        spectral_extension_attenuation_enabled: false,
        coupling_strategy_updates: vec![false; num_blocks],
        coupling_in_use: vec![false; num_blocks],
        coupling_exponent_strategy: vec![None; num_blocks],
        channel_exponent_strategy: vec![vec![ExpStrategy::Reuse; fullband_channels]; num_blocks],
        lfe_exponent_strategy,
        converter_exponent_strategy_present: false,
        converter_exponent_strategy: Vec::new(),
        frame_csnr_offset: None,
        frame_fsnr_offset: None,
        transient_processors: vec![None; fullband_channels],
        spectral_extension_attenuation: vec![None; fullband_channels],
        block_start_info_present: false,
        block_start_info_bit_len: 0,
        block_payload_start_bit_offset,
    }
}

fn read_mixing_metadata(
    reader: &mut BitReader<'_>,
    frame_type: FrameType,
    channel_mode: u8,
    lfe_on: bool,
    num_blocks: usize,
    volume_programs: usize,
) -> Result<bool, ParseError> {
    let enabled = reader.read_bit().ok_or(ParseError::ShortPacket)?;
    if !enabled {
        return Ok(false);
    }

    if channel_mode > 2 {
        reader.skip_bits(2).ok_or(ParseError::ShortPacket)?;
    }
    if (channel_mode & 1) != 0 && channel_mode > 2 {
        reader.skip_bits(6).ok_or(ParseError::ShortPacket)?;
    }
    if (channel_mode & 0x4) != 0 {
        reader.skip_bits(6).ok_or(ParseError::ShortPacket)?;
    }
    if lfe_on && reader.read_bit().ok_or(ParseError::ShortPacket)? {
        reader.skip_bits(5).ok_or(ParseError::ShortPacket)?;
    }

    if matches!(frame_type, FrameType::Independent) {
        for _ in 0..volume_programs {
            if reader.read_bit().ok_or(ParseError::ShortPacket)? {
                reader.skip_bits(6).ok_or(ParseError::ShortPacket)?;
            }
        }
        if reader.read_bit().ok_or(ParseError::ShortPacket)? {
            reader.skip_bits(6).ok_or(ParseError::ShortPacket)?;
        }

        match reader.read_bits(2).ok_or(ParseError::ShortPacket)? {
            1 => reader.skip_bits(5).ok_or(ParseError::ShortPacket)?,
            2 => reader.skip_bits(12).ok_or(ParseError::ShortPacket)?,
            3 => {
                let mixdata_len = reader.read_bits(5).ok_or(ParseError::ShortPacket)? as usize + 2;
                reader
                    .skip_bits(mixdata_len * 8)
                    .ok_or(ParseError::ShortPacket)?;
            }
            _ => {}
        }

        if channel_mode < 2 {
            if reader.read_bit().ok_or(ParseError::ShortPacket)? {
                reader.skip_bits(14).ok_or(ParseError::ShortPacket)?;
            }
            if channel_mode == 0 && reader.read_bit().ok_or(ParseError::ShortPacket)? {
                reader.skip_bits(14).ok_or(ParseError::ShortPacket)?;
            }
        }

        if reader.read_bit().ok_or(ParseError::ShortPacket)? {
            if num_blocks == 1 {
                reader.skip_bits(5).ok_or(ParseError::ShortPacket)?;
            } else {
                for _ in 0..num_blocks {
                    if reader.read_bit().ok_or(ParseError::ShortPacket)? {
                        reader.skip_bits(5).ok_or(ParseError::ShortPacket)?;
                    }
                }
            }
        }
    }

    Ok(true)
}

fn read_informational_metadata(
    reader: &mut BitReader<'_>,
    channel_mode: u8,
    sample_rate: u32,
    volume_programs: usize,
) -> Result<bool, ParseError> {
    let enabled = reader.read_bit().ok_or(ParseError::ShortPacket)?;
    if !enabled {
        return Ok(false);
    }

    reader.skip_bits(3).ok_or(ParseError::ShortPacket)?;
    reader.skip_bits(2).ok_or(ParseError::ShortPacket)?;
    if channel_mode == 2 {
        reader.skip_bits(4).ok_or(ParseError::ShortPacket)?;
    } else if channel_mode >= 6 {
        reader.skip_bits(2).ok_or(ParseError::ShortPacket)?;
    }

    for _ in 0..volume_programs {
        if reader.read_bit().ok_or(ParseError::ShortPacket)? {
            reader.skip_bits(8).ok_or(ParseError::ShortPacket)?;
        }
    }

    if (32_000..=48_000).contains(&sample_rate) {
        reader.skip_bits(1).ok_or(ParseError::ShortPacket)?;
    }

    Ok(true)
}

fn parse_audio_frame(
    reader: &mut BitReader<'_>,
    frame_type: FrameType,
    words_per_syncframe: usize,
    num_blocks: usize,
    channel_mode: u8,
    fullband_channels: usize,
    lfe_on: bool,
) -> Result<ParsedAudioFrame, ParseError> {
    let exponent_strategies_embedded = if num_blocks != 6 {
        true
    } else {
        reader.read_bit().ok_or(ParseError::ShortPacket)?
    };
    let adaptive_hybrid_transform_enabled = if num_blocks == 6 {
        reader.read_bit().ok_or(ParseError::ShortPacket)?
    } else {
        false
    };
    if adaptive_hybrid_transform_enabled {
        return Err(ParseError::UnsupportedFeature("aht"));
    }

    let snr_offset_strategy = reader.read_bits(2).ok_or(ParseError::ShortPacket)? as u8;
    let transient_processing_enabled = reader.read_bit().ok_or(ParseError::ShortPacket)?;
    let block_switching_enabled = reader.read_bit().ok_or(ParseError::ShortPacket)?;
    let dithering_enabled = reader.read_bit().ok_or(ParseError::ShortPacket)?;
    let bit_allocation_mode_enabled = reader.read_bit().ok_or(ParseError::ShortPacket)?;
    let frame_gain_syntax_enabled = reader.read_bit().ok_or(ParseError::ShortPacket)?;
    let delta_bit_allocation_enabled = reader.read_bit().ok_or(ParseError::ShortPacket)?;
    let skip_field_syntax_enabled = reader.read_bit().ok_or(ParseError::ShortPacket)?;
    let spectral_extension_attenuation_enabled =
        reader.read_bit().ok_or(ParseError::ShortPacket)?;

    let mut coupling_strategy_updates = vec![false; num_blocks];
    let mut coupling_in_use = vec![false; num_blocks];
    if channel_mode > 1 {
        coupling_strategy_updates[0] = true;
        coupling_in_use[0] = reader.read_bit().ok_or(ParseError::ShortPacket)?;
        for block in 1..num_blocks {
            coupling_strategy_updates[block] = reader.read_bit().ok_or(ParseError::ShortPacket)?;
            if coupling_strategy_updates[block] {
                coupling_in_use[block] = reader.read_bit().ok_or(ParseError::ShortPacket)?;
            } else {
                coupling_in_use[block] = coupling_in_use[block - 1];
            }
        }
    }

    let mut coupling_exponent_strategy = vec![None; num_blocks];
    let mut channel_exponent_strategy =
        vec![vec![ExpStrategy::Reuse; fullband_channels]; num_blocks];

    if exponent_strategies_embedded {
        for block in 0..num_blocks {
            if coupling_in_use[block] {
                coupling_exponent_strategy[block] = Some(ExpStrategy::from_bits(
                    reader.read_bits(2).ok_or(ParseError::ShortPacket)?,
                )?);
            }
            for channel in 0..fullband_channels {
                channel_exponent_strategy[block][channel] =
                    ExpStrategy::from_bits(reader.read_bits(2).ok_or(ParseError::ShortPacket)?)?;
            }
        }
    } else {
        let frame_coupling_code =
            if channel_mode > 1 && coupling_in_use.iter().any(|in_use| *in_use) {
                Some(reader.read_bits(5).ok_or(ParseError::ShortPacket)? as u8)
            } else {
                None
            };
        let mut frame_channel_codes = vec![0u8; fullband_channels];
        for code in &mut frame_channel_codes {
            *code = reader.read_bits(5).ok_or(ParseError::ShortPacket)? as u8;
        }

        for block in 0..num_blocks {
            if coupling_in_use[block] {
                if let Some(code) = frame_coupling_code {
                    coupling_exponent_strategy[block] =
                        Some(ExpStrategy::from_frame_code(code, block)?);
                }
            }
            for channel in 0..fullband_channels {
                channel_exponent_strategy[block][channel] =
                    ExpStrategy::from_frame_code(frame_channel_codes[channel], block)?;
            }
        }
    }

    let mut lfe_exponent_strategy = Vec::new();
    if lfe_on {
        lfe_exponent_strategy.reserve(num_blocks);
        for _ in 0..num_blocks {
            lfe_exponent_strategy.push(reader.read_bit().ok_or(ParseError::ShortPacket)?);
        }
    }

    let converter_exponent_strategy_present = matches!(frame_type, FrameType::Independent)
        && if num_blocks == 6 {
            true
        } else {
            reader.read_bit().ok_or(ParseError::ShortPacket)?
        };
    let mut converter_exponent_strategy = Vec::new();
    if converter_exponent_strategy_present {
        converter_exponent_strategy.reserve(fullband_channels);
        for _ in 0..fullband_channels {
            converter_exponent_strategy
                .push(reader.read_bits(5).ok_or(ParseError::ShortPacket)? as u8);
        }
    }

    let (frame_csnr_offset, frame_fsnr_offset) = if snr_offset_strategy == 0 {
        (
            Some(reader.read_bits(6).ok_or(ParseError::ShortPacket)? as u8),
            Some(reader.read_bits(4).ok_or(ParseError::ShortPacket)? as u8),
        )
    } else {
        (None, None)
    };

    let mut transient_processors = vec![None; fullband_channels];
    if transient_processing_enabled {
        for processor in &mut transient_processors {
            if reader.read_bit().ok_or(ParseError::ShortPacket)? {
                *processor = Some(TransientProcessorInfo {
                    location: reader.read_bits(10).ok_or(ParseError::ShortPacket)? as u16,
                    length: reader.read_bits(8).ok_or(ParseError::ShortPacket)? as u8,
                });
            }
        }
    }

    let mut spectral_extension_attenuation = vec![None; fullband_channels];
    if spectral_extension_attenuation_enabled {
        for attenuation in &mut spectral_extension_attenuation {
            if reader.read_bit().ok_or(ParseError::ShortPacket)? {
                *attenuation = Some(reader.read_bits(5).ok_or(ParseError::ShortPacket)? as u8);
            }
        }
    }

    let mut block_start_info_present = false;
    let mut block_start_info_bit_len = 0usize;
    let mut block_start_bit_offsets = None;
    if num_blocks != 1 && reader.read_bit().ok_or(ParseError::ShortPacket)? {
        block_start_info_present = true;
        let bits_per_block_start = 4 + log2_ceil(words_per_syncframe);
        block_start_info_bit_len = (num_blocks - 1) * bits_per_block_start;
        let mut offsets = Vec::with_capacity(num_blocks);
        for _ in 1..num_blocks {
            offsets.push(
                reader
                    .read_bits(bits_per_block_start)
                    .ok_or(ParseError::ShortPacket)? as usize,
            );
        }

        let mut resolved_offsets = Vec::with_capacity(num_blocks);
        resolved_offsets.push(reader.position());
        resolved_offsets.extend(offsets);
        if block_start_offsets_are_valid(&resolved_offsets, words_per_syncframe * 16) {
            block_start_bit_offsets = Some(resolved_offsets);
        }
    }

    Ok(ParsedAudioFrame {
        info: AudioFrameInfo {
            exponent_strategies_embedded,
            adaptive_hybrid_transform_enabled,
            snr_offset_strategy,
            transient_processing_enabled,
            block_switching_enabled,
            dithering_enabled,
            bit_allocation_mode_enabled,
            frame_gain_syntax_enabled,
            delta_bit_allocation_enabled,
            skip_field_syntax_enabled,
            spectral_extension_attenuation_enabled,
            coupling_strategy_updates,
            coupling_in_use,
            coupling_exponent_strategy,
            channel_exponent_strategy,
            lfe_exponent_strategy,
            converter_exponent_strategy_present,
            converter_exponent_strategy,
            frame_csnr_offset,
            frame_fsnr_offset,
            transient_processors,
            spectral_extension_attenuation,
            block_start_info_present,
            block_start_info_bit_len,
            block_payload_start_bit_offset: reader.position(),
        },
        block_start_bit_offsets,
    })
}

fn block_start_offsets_are_valid(offsets: &[usize], frame_bits: usize) -> bool {
    if offsets.is_empty() || offsets[0] >= frame_bits {
        return false;
    }

    let mut previous = offsets[0];
    for &offset in &offsets[1..] {
        if offset <= previous || offset >= frame_bits {
            return false;
        }
        previous = offset;
    }
    true
}

fn extract_trailing_aux_data(frame: &[u8]) -> TrailingAuxDataInfo {
    let frame_bits = frame.len() * 8;
    let trailer_start = frame_bits.saturating_sub(32);
    if frame_bits < 32 {
        return TrailingAuxDataInfo {
            start_bit_offset: trailer_start,
            bytes: Vec::new(),
        };
    }

    let mut footer = BitReader::with_offset(frame, trailer_start);
    let aux_length = match footer.read_bits(14) {
        Some(length) => length as usize,
        None => {
            return TrailingAuxDataInfo {
                start_bit_offset: frame_bits,
                bytes: Vec::new(),
            };
        }
    };
    let aux_present = footer.read_bit().unwrap_or(false);
    if !aux_present {
        return TrailingAuxDataInfo {
            start_bit_offset: frame_bits,
            bytes: Vec::new(),
        };
    }

    // TODO: Reconfirm the trailer aux length unit against a second decoder reference.
    // Existing decoder implementations disagree around this field.
    let Some(aux_start_bit_offset) = trailer_start.checked_sub(aux_length * 8) else {
        return TrailingAuxDataInfo {
            start_bit_offset: frame_bits,
            bytes: Vec::new(),
        };
    };

    let mut reader = BitReader::with_offset(frame, aux_start_bit_offset);
    let bytes = reader.read_bytes(aux_length).unwrap_or_default();
    if bytes.len() != aux_length {
        return TrailingAuxDataInfo {
            start_bit_offset: frame_bits,
            bytes: Vec::new(),
        };
    }

    TrailingAuxDataInfo {
        start_bit_offset: aux_start_bit_offset,
        bytes,
    }
}

fn collect_skip_fields(
    frame: &[u8],
    frame_type: FrameType,
    num_blocks: usize,
    channel_mode: u8,
    fullband_channels: usize,
    lfe_on: bool,
    audio_frame: &AudioFrameInfo,
    block_start_bit_offsets: Option<&[usize]>,
    audio_payload_end_bit: usize,
    sample_rate_index: usize,
) -> Result<Vec<SkipFieldInfo>, ParseError> {
    let block_starts = match (num_blocks, block_start_bit_offsets) {
        (1, _) => vec![audio_frame.block_payload_start_bit_offset],
        (_, Some(offsets)) if offsets.len() == num_blocks => offsets.to_vec(),
        _ => return Err(ParseError::InvalidHeader("block-start-info")),
    };

    if audio_payload_end_bit > frame.len() * 8 {
        return Err(ParseError::InvalidHeader("block-start-info"));
    }

    let mut state = BlockSyntaxState::new(fullband_channels, lfe_on, sample_rate_index);
    let mut audio_frame = audio_frame.clone();
    let mut skip_fields = Vec::new();
    for block in 0..num_blocks {
        let block_start = block_starts[block];
        let block_end = if block + 1 < block_starts.len() {
            block_starts[block + 1]
        } else {
            audio_payload_end_bit
        };
        if block_end < block_start {
            return Err(ParseError::InvalidHeader("block-start-info"));
        }

        let mut reader = BitReader::with_offset(frame, block_start);
        reader.set_limit_bits(block_end);
        if let Some(skip_field) = parse_block(
            &mut reader,
            block,
            frame_type,
            channel_mode,
            fullband_channels,
            lfe_on,
            &mut audio_frame,
            &mut state,
            false,
        )? {
            skip_fields.push(skip_field);
        }
    }

    Ok(skip_fields)
}

fn collect_skip_fields_without_block_start(
    frame: &[u8],
    frame_type: FrameType,
    num_blocks: usize,
    channel_mode: u8,
    fullband_channels: usize,
    lfe_on: bool,
    audio_frame: &AudioFrameInfo,
    audio_payload_end_bit: usize,
    state: &mut BlockSyntaxState,
) -> Result<Vec<SkipFieldInfo>, ParseError> {
    let mut audio_frame = audio_frame.clone();
    let mut reader = BitReader::with_offset(frame, audio_frame.block_payload_start_bit_offset);
    reader.set_limit_bits(audio_payload_end_bit);

    let mut skip_fields = Vec::new();
    for block in 0..num_blocks {
        if let Some(skip_field) = parse_block(
            &mut reader,
            block,
            frame_type,
            channel_mode,
            fullband_channels,
            lfe_on,
            &mut audio_frame,
            state,
            true,
        )? {
            skip_fields.push(skip_field);
        }
        let skip_len = skip_fields
            .last()
            .filter(|field| field.block_index == Some(block))
            .map(|field| field.bytes.len())
            .unwrap_or(0);
        emit_aux_debug(format_args!(
            "no-blkstart block={block} pos={} skip={}B",
            reader.position(),
            skip_len,
        ));
    }

    if reader.position() > audio_payload_end_bit {
        emit_aux_debug(format_args!(
            "no-blkstart end mismatch pos={} expected={audio_payload_end_bit}",
            reader.position(),
        ));
        return Err(ParseError::InvalidHeader("block-end"));
    }

    if reader.position() < audio_payload_end_bit {
        emit_aux_debug(format_args!(
            "no-blkstart trailing-tail pos={} expected={audio_payload_end_bit}",
            reader.position(),
        ));
    }
    // Existing decoder implementations decode the declared blocks and then read the syncframe
    // tail from the back without asserting an exact forward reader end position. Keep that tail
    // tolerance until the footer / trailing aux layout is modeled precisely enough to tighten the
    // validation again.
    // TODO: Replace this tail tolerance with a verified syncframe footer parser.

    Ok(skip_fields)
}

fn recover_skip_fields_from_emdf_markers(frame: &[u8]) -> Option<Vec<SkipFieldInfo>> {
    // TODO: Replace this EMDF-anchored recovery path with a full no-blkstrtinfo block walk.
    let mut recovered: Vec<SkipFieldInfo> = Vec::new();
    for emdf_block in scan_emdf_blocks(frame) {
        if let Some(skip_field) = recover_skip_field_before_offset(frame, emdf_block.sync_offset) {
            if !recovered
                .iter()
                .any(|existing| existing.bit_offset == skip_field.bit_offset)
            {
                recovered.push(skip_field);
            }
        }
    }

    if recovered.is_empty() {
        None
    } else {
        recovered.sort_by_key(|field| field.bit_offset);
        Some(recovered)
    }
}

fn recover_skip_field_before_offset(frame: &[u8], sync_offset: usize) -> Option<SkipFieldInfo> {
    let sync_bit_offset = sync_offset.checked_mul(8)?;
    let header_bit_offset = sync_bit_offset.checked_sub(10)?;
    let mut reader = BitReader::with_offset(frame, header_bit_offset);
    if !reader.read_bit()? {
        return None;
    }

    let skip_length = reader.read_bits(9)? as usize;
    let bytes = reader.read_bytes(skip_length)?;
    if bytes.len() != skip_length
        || bytes.first().copied() != Some(0x58)
        || bytes.get(1).copied() != Some(0x38)
    {
        return None;
    }
    if scan_emdf_blocks(&bytes).is_empty() {
        return None;
    }

    Some(SkipFieldInfo {
        block_index: None,
        bit_offset: sync_bit_offset,
        bytes,
    })
}

fn parse_block(
    reader: &mut BitReader<'_>,
    block: usize,
    frame_type: FrameType,
    channel_mode: u8,
    fullband_channels: usize,
    lfe_on: bool,
    audio_frame: &mut AudioFrameInfo,
    state: &mut BlockSyntaxState,
    consume_mantissas: bool,
) -> Result<Option<SkipFieldInfo>, ParseError> {
    let block_start = reader.position();
    if audio_frame.block_switching_enabled {
        reader
            .skip_bits(fullband_channels)
            .ok_or(ParseError::ShortPacket)?;
    }
    if audio_frame.dithering_enabled {
        reader
            .skip_bits(fullband_channels)
            .ok_or(ParseError::ShortPacket)?;
    }

    skip_conditional_bits(reader, 8)?;
    if channel_mode == 0 {
        skip_conditional_bits(reader, 8)?;
    }

    if matches!(frame_type, FrameType::LegacyAc3) {
        read_legacy_ac3_coupling_strategy(
            reader,
            block,
            channel_mode,
            fullband_channels,
            audio_frame,
            state,
        )?;
        read_legacy_ac3_coupling_coordinates(
            reader,
            block,
            channel_mode,
            fullband_channels,
            audio_frame,
            state,
        )?;
        read_legacy_ac3_exponent_strategies(reader, block, fullband_channels, lfe_on, audio_frame)?;
    } else {
        read_spx(reader, block, channel_mode, fullband_channels, state)?;
        read_coupling_strategy(
            reader,
            block,
            channel_mode,
            fullband_channels,
            audio_frame,
            state,
        )?;
        read_coupling_coordinates(
            reader,
            block,
            channel_mode,
            fullband_channels,
            audio_frame,
            state,
        )?;
    }
    let allocation = read_exponents(reader, block, fullband_channels, lfe_on, audio_frame, state)?;

    read_bit_allocation_params(reader, block, audio_frame, state)?;
    read_snr_offsets(
        reader,
        block,
        frame_type,
        fullband_channels,
        lfe_on,
        audio_frame,
        state,
    )?;
    if !matches!(frame_type, FrameType::LegacyAc3) {
        read_frame_gain_codes(reader, block, fullband_channels, lfe_on, audio_frame, state)?;
    }

    if matches!(frame_type, FrameType::Independent)
        && reader.read_bit().ok_or(ParseError::ShortPacket)?
    {
        reader.skip_bits(10).ok_or(ParseError::ShortPacket)?;
    }

    if matches!(frame_type, FrameType::LegacyAc3) {
        read_legacy_ac3_coupling_leak_info(reader, audio_frame.coupling_in_use[block], state)?;
    } else {
        read_coupling_leak_info(reader, audio_frame.coupling_in_use[block], state)?;
    }

    read_delta_bit_allocation(reader, block, fullband_channels, audio_frame, state)?;

    let mut skip_field = None;
    if audio_frame.skip_field_syntax_enabled && reader.read_bit().ok_or(ParseError::ShortPacket)? {
        let skip_length = reader.read_bits(9).ok_or(ParseError::ShortPacket)? as usize;
        let bit_offset = reader.position();
        let bytes = reader
            .read_bytes(skip_length)
            .ok_or(ParseError::ShortPacket)?;
        skip_field = Some(SkipFieldInfo {
            block_index: Some(block),
            bit_offset,
            bytes,
        });
    }

    let syntax_end = reader.position();
    if consume_mantissas {
        consume_block_mantissas(reader, block, lfe_on, audio_frame, state, &allocation)?;
    }
    let skip_bits = skip_field
        .as_ref()
        .map(|field| 10 + field.bytes.len() * 8)
        .unwrap_or(0);
    emit_aux_debug(format_args!(
        "block={block} parse start={block_start} syntax_end={syntax_end} end={} syntax_bits={} skip_bits={skip_bits} spx={} spxbegf={} spx_begin_sbnd={} chbwcod={:?} chexp={:?} lfeexp={}",
        reader.position(),
        syntax_end - block_start,
        state.spx_in_use as u8,
        state.spxbegf,
        state.spx_begin_subbnd,
        state.chbwcod,
        audio_frame.channel_exponent_strategy[block],
        audio_frame
            .lfe_exponent_strategy
            .get(block)
            .copied()
            .unwrap_or(false) as u8,
    ));

    Ok(skip_field)
}

fn skip_conditional_bits(reader: &mut BitReader<'_>, bits: usize) -> Result<(), ParseError> {
    if reader.read_bit().ok_or(ParseError::ShortPacket)? {
        reader.skip_bits(bits).ok_or(ParseError::ShortPacket)?;
    }
    Ok(())
}

fn read_legacy_ac3_coupling_strategy(
    reader: &mut BitReader<'_>,
    block: usize,
    channel_mode: u8,
    fullband_channels: usize,
    audio_frame: &mut AudioFrameInfo,
    state: &mut BlockSyntaxState,
) -> Result<(), ParseError> {
    if channel_mode <= 1 {
        audio_frame.coupling_strategy_updates[block] = false;
        audio_frame.coupling_in_use[block] = false;
        state.clear_coupling();
        return Ok(());
    }

    let update = reader.read_bit().ok_or(ParseError::ShortPacket)?;
    if block == 0 && !update {
        return Err(ParseError::InvalidHeader("cplstrat"));
    }
    audio_frame.coupling_strategy_updates[block] = update;
    if update {
        let in_use = reader.read_bit().ok_or(ParseError::ShortPacket)?;
        audio_frame.coupling_in_use[block] = in_use;
        state.ecplinu = false;
        if in_use {
            if channel_mode == 2 {
                for in_use in &mut state.chincpl {
                    *in_use = true;
                }
            } else {
                for channel in 0..fullband_channels {
                    state.chincpl[channel] = reader.read_bit().ok_or(ParseError::ShortPacket)?;
                }
            }

            state.phsflginu = if channel_mode == 2 {
                reader.read_bit().ok_or(ParseError::ShortPacket)?
            } else {
                false
            };
            state.cplbegf = reader.read_bits(4).ok_or(ParseError::ShortPacket)? as usize;
            state.cplendf = reader.read_bits(4).ok_or(ParseError::ShortPacket)? as usize;
            if state.cplendf < state.cplbegf {
                state.clear_coupling();
                audio_frame.coupling_in_use[block] = false;
                return Ok(());
            }
            state.ncplsubnd = 3 + state.cplendf - state.cplbegf;
            state.ncplbnd = state.ncplsubnd;
            for band in 1..state.ncplsubnd {
                let index = state.cplbegf + band;
                let band_reuse = reader.read_bit().ok_or(ParseError::ShortPacket)?;
                let Some(slot) = state.cpl_band_struct.get_mut(index) else {
                    return Err(ParseError::InvalidHeader("cplbndstrc"));
                };
                *slot = band_reuse;
                if band_reuse {
                    state.ncplbnd -= 1;
                }
            }
        } else {
            state.clear_coupling();
        }
    } else {
        audio_frame.coupling_in_use[block] = audio_frame.coupling_in_use[block - 1];
    }

    Ok(())
}

fn read_legacy_ac3_exponent_strategies(
    reader: &mut BitReader<'_>,
    block: usize,
    fullband_channels: usize,
    lfe_on: bool,
    audio_frame: &mut AudioFrameInfo,
) -> Result<(), ParseError> {
    if audio_frame.coupling_in_use[block] {
        audio_frame.coupling_exponent_strategy[block] = Some(ExpStrategy::from_bits(
            reader.read_bits(2).ok_or(ParseError::ShortPacket)?,
        )?);
    } else {
        audio_frame.coupling_exponent_strategy[block] = None;
    }

    for channel in 0..fullband_channels {
        audio_frame.channel_exponent_strategy[block][channel] =
            ExpStrategy::from_bits(reader.read_bits(2).ok_or(ParseError::ShortPacket)?)?;
    }
    if lfe_on {
        audio_frame.lfe_exponent_strategy[block] =
            reader.read_bit().ok_or(ParseError::ShortPacket)?;
    }

    Ok(())
}

fn decode_coupling_coordinate(exponent: i32, mantissa: i32, master_coord: i32) -> f32 {
    let shift = if exponent != 15 {
        15 - exponent - master_coord
    } else {
        15 - master_coord
    };
    let base = if exponent != 15 {
        mantissa + 16
    } else {
        mantissa
    };
    let scaled = if shift >= 0 {
        base.checked_shl(shift as u32).unwrap_or(0)
    } else {
        base >> (-shift) as u32
    };
    scaled as f32 / 131_072.0
}

fn coupling_start_mantissa(state: &BlockSyntaxState) -> usize {
    37 + 12 * state.cplbegf
}

fn read_coupling_leak_info(
    reader: &mut BitReader<'_>,
    coupling_in_use: bool,
    state: &mut BlockSyntaxState,
) -> Result<(), ParseError> {
    if !coupling_in_use {
        return Ok(());
    }

    let coupling_leak_present = if state.first_cpl_leak {
        state.first_cpl_leak = false;
        true
    } else {
        reader.read_bit().ok_or(ParseError::ShortPacket)?
    };
    if coupling_leak_present {
        state.cpl_fast_leak =
            ((reader.read_bits(3).ok_or(ParseError::ShortPacket)? as i32) << 8) + 768;
        state.cpl_slow_leak =
            ((reader.read_bits(3).ok_or(ParseError::ShortPacket)? as i32) << 8) + 768;
    }

    Ok(())
}

fn read_legacy_ac3_coupling_leak_info(
    reader: &mut BitReader<'_>,
    coupling_in_use: bool,
    state: &mut BlockSyntaxState,
) -> Result<(), ParseError> {
    if !coupling_in_use {
        return Ok(());
    }

    if reader.read_bit().ok_or(ParseError::ShortPacket)? {
        state.cpl_fast_leak =
            ((reader.read_bits(3).ok_or(ParseError::ShortPacket)? as i32) << 8) + 768;
        state.cpl_slow_leak =
            ((reader.read_bits(3).ok_or(ParseError::ShortPacket)? as i32) << 8) + 768;
    }

    Ok(())
}

fn coupling_end_mantissa(state: &BlockSyntaxState) -> Result<usize, ParseError> {
    if state.ecplinu {
        let begin = *ECPL_SUBBAND_TAB
            .get(state.cplbegf)
            .ok_or(ParseError::InvalidHeader("ecplbegf"))?;
        let end = *ECPL_SUBBAND_TAB
            .get(state.cplendf)
            .ok_or(ParseError::InvalidHeader("ecplendf"))?;
        if end < begin {
            return Err(ParseError::InvalidHeader("ecplendf"));
        }
        Ok(end)
    } else {
        Ok(37 + 12 * (state.cplendf + 3))
    }
}

fn allocate_coupling_channel(state: &mut BlockSyntaxState) -> Result<(), ParseError> {
    let cplendmant = coupling_end_mantissa(state)?;
    let coupling_snr_offset = (((state.csnr_offset - 15) << 4) + state.cpl_fsnr_offset) << 2;
    if state.csnr_offset == 0 && state.cpl_fsnr_offset == 0 {
        state.coupling_allocation.clear_bap();
        return Ok(());
    }

    state.coupling_allocation.allocate(
        coupling_start_mantissa(state),
        cplendmant,
        state.cpl_fgain_code,
        coupling_snr_offset,
        state.bit_allocation_params,
        state.sample_rate_index,
        &state.coupling_delta_bit_allocation,
        state.cpl_fast_leak,
        state.cpl_slow_leak,
    )
}

fn apply_standard_coupling(
    state: &BlockSyntaxState,
    channel: usize,
    coeffs: &mut [f32; 256],
    coupling_coeffs: &[f32; 256],
) {
    let mut used_band = 0usize;
    for subband in 0..state.ncplsubnd {
        if subband != 0
            && !state
                .cpl_band_struct
                .get(state.cplbegf + subband)
                .copied()
                .unwrap_or(false)
        {
            used_band += 1;
        }
        let gain = state.coupling_coordinates[channel]
            .get(used_band)
            .copied()
            .unwrap_or_default();
        let offset = (state.cplbegf + subband) * 12 + 37;
        for bin in 0..12 {
            coeffs[bin + offset] = coupling_coeffs[bin + offset] * gain;
        }
    }
}

fn read_spx(
    reader: &mut BitReader<'_>,
    block: usize,
    channel_mode: u8,
    fullband_channels: usize,
    state: &mut BlockSyntaxState,
) -> Result<(), ParseError> {
    let spx_strategy_updates = if block == 0 {
        true
    } else {
        reader.read_bit().ok_or(ParseError::ShortPacket)?
    };
    if spx_strategy_updates {
        if reader.read_bit().ok_or(ParseError::ShortPacket)? {
            state.spx_in_use = true;
            if channel_mode == 1 {
                for in_use in &mut state.chinspx {
                    *in_use = false;
                }
                if !state.chinspx.is_empty() {
                    state.chinspx[0] = true;
                }
            } else {
                for channel in 0..fullband_channels {
                    state.chinspx[channel] = reader.read_bit().ok_or(ParseError::ShortPacket)?;
                }
            }
            reader.skip_bits(2).ok_or(ParseError::ShortPacket)?;
            state.spxbegf = reader.read_bits(3).ok_or(ParseError::ShortPacket)? as usize;
            let spxendf = reader.read_bits(3).ok_or(ParseError::ShortPacket)? as usize;
            state.spx_begin_subbnd = if state.spxbegf < 6 {
                state.spxbegf + 2
            } else {
                state.spxbegf * 2 - 3
            };
            state.spx_end_subbnd = if spxendf < 3 {
                spxendf + 5
            } else {
                spxendf * 2 + 3
            };

            let mut band_struct = vec![false; state.spx_end_subbnd];
            if reader.read_bit().ok_or(ParseError::ShortPacket)? {
                for band in (state.spx_begin_subbnd + 1)..state.spx_end_subbnd {
                    band_struct[band] = reader.read_bit().ok_or(ParseError::ShortPacket)?;
                }
            }
            state.nspxbnds =
                count_spx_bands(&band_struct, state.spx_begin_subbnd, state.spx_end_subbnd);
        } else {
            state.clear_spx();
        }
    }

    if state.spx_in_use {
        for channel in 0..fullband_channels {
            if state.chinspx[channel] {
                let coordinates_present = if state.first_spx_coords[channel] {
                    state.first_spx_coords[channel] = false;
                    true
                } else {
                    reader.read_bit().ok_or(ParseError::ShortPacket)?
                };
                if coordinates_present {
                    let master = reader.read_bits(2).ok_or(ParseError::ShortPacket)? as u8;
                    let noise_blend = reader.read_bit().ok_or(ParseError::ShortPacket)?;
                    if noise_blend {
                        reader.skip_bits(4).ok_or(ParseError::ShortPacket)?;
                    }
                    let mut gains = Vec::with_capacity(state.nspxbnds);
                    for _ in 0..state.nspxbnds {
                        let exponent = reader.read_bits(4).ok_or(ParseError::ShortPacket)? as i32;
                        let mantissa = reader.read_bits(2).ok_or(ParseError::ShortPacket)? as i32;
                        gains.push(decode_coupling_coordinate(
                            exponent,
                            mantissa,
                            master as i32 * 3,
                        ));
                    }
                    while state.spx_master.len() <= channel {
                        state.spx_master.push(0);
                    }
                    while state.spx_gains.len() <= channel {
                        state.spx_gains.push(Vec::new());
                    }
                    while state.spx_noise_blend.len() <= channel {
                        state.spx_noise_blend.push(false);
                    }
                    state.spx_master[channel] = master;
                    state.spx_gains[channel] = gains;
                    state.spx_noise_blend[channel] = noise_blend;
                }
            } else {
                state.first_spx_coords[channel] = true;
            }
        }
    }

    Ok(())
}

fn count_spx_bands(band_struct: &[bool], begin_subbnd: usize, end_subbnd: usize) -> usize {
    let mut bands = 1usize;
    for band in (begin_subbnd + 1)..end_subbnd {
        if !band_struct.get(band).copied().unwrap_or(false) {
            bands += 1;
        }
    }
    bands
}

fn read_coupling_strategy(
    reader: &mut BitReader<'_>,
    block: usize,
    channel_mode: u8,
    fullband_channels: usize,
    audio_frame: &AudioFrameInfo,
    state: &mut BlockSyntaxState,
) -> Result<(), ParseError> {
    if channel_mode <= 1 {
        state.clear_coupling();
        return Ok(());
    }

    if audio_frame.coupling_strategy_updates[block] {
        if audio_frame.coupling_in_use[block] {
            state.ecplinu = reader.read_bit().ok_or(ParseError::ShortPacket)?;
            if channel_mode == 2 {
                for in_use in &mut state.chincpl {
                    *in_use = true;
                }
            } else {
                for channel in 0..fullband_channels {
                    state.chincpl[channel] = reader.read_bit().ok_or(ParseError::ShortPacket)?;
                }
            }

            if state.ecplinu {
                // TODO: Walk enhanced coupling (`ecplinu`) instead of falling back to frame-scan.
                state.clear_coupling();
                return Ok(());
            }

            state.phsflginu = if channel_mode == 2 {
                reader.read_bit().ok_or(ParseError::ShortPacket)?
            } else {
                false
            };
            state.cplbegf = reader.read_bits(4).ok_or(ParseError::ShortPacket)? as usize;
            state.cplendf = if !state.spx_in_use {
                reader.read_bits(4).ok_or(ParseError::ShortPacket)? as usize
            } else if state.spxbegf < 6 {
                state.spxbegf.saturating_sub(2)
            } else {
                state.spxbegf * 2 - 7
            };

            if state.cplendf < state.cplbegf {
                state.clear_coupling();
                return Ok(());
            }

            state.ncplsubnd = 3 + state.cplendf - state.cplbegf;
            if state.ncplsubnd == 0 {
                return Err(ParseError::InvalidHeader("ncplsubnd"));
            }
            state.ncplbnd = state.ncplsubnd;

            if reader.read_bit().ok_or(ParseError::ShortPacket)? {
                for band in 1..state.ncplsubnd {
                    let index = state.cplbegf + band;
                    let band_reuse = reader.read_bit().ok_or(ParseError::ShortPacket)?;
                    let Some(slot) = state.cpl_band_struct.get_mut(index) else {
                        return Err(ParseError::InvalidHeader("cplbndstrc"));
                    };
                    *slot = band_reuse;
                    if band_reuse {
                        state.ncplbnd -= 1;
                    }
                }
            } else {
                for band in 1..state.ncplsubnd {
                    let index = state.cplbegf + band;
                    if state.cpl_band_struct.get(index).copied().unwrap_or(false) {
                        state.ncplbnd -= 1;
                    }
                }
            }
        } else {
            state.clear_coupling();
        }
    }

    Ok(())
}

fn read_coupling_coordinates(
    reader: &mut BitReader<'_>,
    block: usize,
    channel_mode: u8,
    fullband_channels: usize,
    audio_frame: &AudioFrameInfo,
    state: &mut BlockSyntaxState,
) -> Result<(), ParseError> {
    if !audio_frame.coupling_in_use[block] {
        return Ok(());
    }
    if state.ecplinu {
        // TODO: Walk enhanced coupling coordinates instead of falling back to frame-scan.
        state.clear_coupling();
        return Ok(());
    }

    let mut stereo_phase_flags_required = false;
    for channel in 0..fullband_channels {
        if state.chincpl[channel] {
            let coordinates_present = if state.first_cpl_coords[channel] {
                state.first_cpl_coords[channel] = false;
                true
            } else {
                reader.read_bit().ok_or(ParseError::ShortPacket)?
            };
            if coordinates_present {
                let master_coord = reader.read_bits(2).ok_or(ParseError::ShortPacket)? as i32 * 3;
                for band in 0..state.ncplbnd {
                    let exponent = reader.read_bits(4).ok_or(ParseError::ShortPacket)? as i32;
                    let mantissa = reader.read_bits(4).ok_or(ParseError::ShortPacket)? as i32;
                    state.coupling_coordinates[channel][band] =
                        decode_coupling_coordinate(exponent, mantissa, master_coord);
                }
                stereo_phase_flags_required |= channel_mode == 2;
            }
        } else {
            state.first_cpl_coords[channel] = true;
        }
    }

    if channel_mode == 2 && state.phsflginu && stereo_phase_flags_required {
        // TODO: Walk stereo coupling phase flags instead of falling back to frame-scan.
        return Err(ParseError::UnsupportedFeature("stereo-coupling-phase"));
    }

    Ok(())
}

fn read_legacy_ac3_coupling_coordinates(
    reader: &mut BitReader<'_>,
    block: usize,
    channel_mode: u8,
    fullband_channels: usize,
    audio_frame: &AudioFrameInfo,
    state: &mut BlockSyntaxState,
) -> Result<(), ParseError> {
    if !audio_frame.coupling_in_use[block] {
        return Ok(());
    }

    let mut stereo_phase_flags_required = false;
    for channel in 0..fullband_channels {
        if state.chincpl[channel] {
            let coordinates_present = reader.read_bit().ok_or(ParseError::ShortPacket)?;
            if coordinates_present {
                let master_coord = reader.read_bits(2).ok_or(ParseError::ShortPacket)? as i32 * 3;
                for band in 0..state.ncplbnd {
                    let exponent = reader.read_bits(4).ok_or(ParseError::ShortPacket)? as i32;
                    let mantissa = reader.read_bits(4).ok_or(ParseError::ShortPacket)? as i32;
                    state.coupling_coordinates[channel][band] =
                        decode_coupling_coordinate(exponent, mantissa, master_coord);
                }
                stereo_phase_flags_required |= channel_mode == 2;
            }
        }
    }

    if channel_mode == 2 && state.phsflginu && stereo_phase_flags_required {
        // TODO: Walk stereo coupling phase flags instead of falling back.
        return Err(ParseError::UnsupportedFeature("stereo-coupling-phase"));
    }

    Ok(())
}

fn read_exponents(
    reader: &mut BitReader<'_>,
    block: usize,
    fullband_channels: usize,
    lfe_on: bool,
    audio_frame: &AudioFrameInfo,
    state: &mut BlockSyntaxState,
) -> Result<BlockAllocationInfo, ParseError> {
    for channel in 0..fullband_channels {
        if audio_frame.channel_exponent_strategy[block][channel] != ExpStrategy::Reuse
            && !state.chincpl[channel]
            && !state.chinspx[channel]
        {
            state.chbwcod[channel] = reader.read_bits(6).ok_or(ParseError::ShortPacket)? as u8;
        }
    }

    let cplstrtmant = 37 + 12 * state.cplbegf;
    let cplendmant = 37 + 12 * (state.cplendf + 3);
    if audio_frame.coupling_in_use[block] {
        if state.ecplinu {
            // TODO: Derive exponent group sizes for enhanced coupling instead of falling back.
            state.clear_coupling();
            return Ok(BlockAllocationInfo {
                channel_end_mantissas: vec![0; fullband_channels],
            });
        }
        if let Some(strategy) = audio_frame.coupling_exponent_strategy[block] {
            // Reuse means no exponent payload at all — neither the absolute
            // exponent nor any grouped data. Per FFmpeg ac3dec.c:1109, the
            // absexp + decode_exponents call is gated on `strategy != REUSE`.
            if strategy != ExpStrategy::Reuse {
                // Coupling uses (end - start) / (3 << (strategy - 1)) per FFmpeg
                // ac3dec.c:1086, which differs from the fullband formula on D15
                // (no `+ group_size - 4` adjust).
                let cpl_range = cplendmant.saturating_sub(cplstrtmant);
                let group_size = match strategy {
                    ExpStrategy::Reuse => 0,
                    ExpStrategy::D15 => 3,
                    ExpStrategy::D25 => 6,
                    ExpStrategy::D45 => 12,
                };
                let ncplgrps = if group_size == 0 { 0 } else { cpl_range / group_size };
                state.coupling_allocation.read_coupling_exponents(
                    reader,
                    strategy,
                    cplstrtmant,
                    cplendmant,
                    ncplgrps,
                )?;
            }
        }
    }

    let mut channel_end_mantissas = vec![0usize; fullband_channels];
    for channel in 0..fullband_channels {
        let endmant = if state.ecplinu {
            // TODO: Derive `endmant` for enhanced coupling instead of falling back.
            state.spx_begin_subbnd * 12 + 25
        } else if state.spx_in_use && !audio_frame.coupling_in_use[block] {
            state.spx_begin_subbnd * 12 + 25
        } else if state.chincpl[channel] {
            cplstrtmant
        } else {
            if state.chbwcod[channel] > 60 {
                return Err(ParseError::InvalidHeader("chbwcod"));
            }
            (state.chbwcod[channel] as usize + 12) * 3 + 37
        };

        channel_end_mantissas[channel] = endmant;
        let group_count = grouped_exponent_count(
            endmant,
            audio_frame.channel_exponent_strategy[block][channel],
        )?;
        if audio_frame.channel_exponent_strategy[block][channel] != ExpStrategy::Reuse {
            state.channel_allocations[channel].read_channel_exponents(
                reader,
                audio_frame.channel_exponent_strategy[block][channel],
                group_count,
                endmant,
            )?;
        }
    }

    if lfe_on
        && audio_frame
            .lfe_exponent_strategy
            .get(block)
            .copied()
            .unwrap_or(false)
    {
        if let Some(lfe_allocation) = state.lfe_allocation.as_mut() {
            lfe_allocation.read_lfe_exponents(reader)?;
        }
    }

    Ok(BlockAllocationInfo {
        channel_end_mantissas,
    })
}

fn read_bit_allocation_params(
    reader: &mut BitReader<'_>,
    block: usize,
    audio_frame: &AudioFrameInfo,
    state: &mut BlockSyntaxState,
) -> Result<(), ParseError> {
    if audio_frame.bit_allocation_mode_enabled {
        if reader.read_bit().ok_or(ParseError::ShortPacket)? {
            state.bit_allocation_params = BitAllocationParams {
                slow_decay_code: reader.read_bits(2).ok_or(ParseError::ShortPacket)? as usize,
                fast_decay_code: reader.read_bits(2).ok_or(ParseError::ShortPacket)? as usize,
                slow_gain_code: reader.read_bits(2).ok_or(ParseError::ShortPacket)? as usize,
                db_per_bit_code: reader.read_bits(2).ok_or(ParseError::ShortPacket)? as usize,
                floor_code: reader.read_bits(3).ok_or(ParseError::ShortPacket)? as usize,
            };
        }
    } else if block == 0 {
        // Per FFmpeg eac3dec.c:387-394 — when bit_allocation_syntax is disabled at
        // frame level, set default bit-allocation parameters ONCE per frame using
        // specific table indices (NOT the zero default).
        state.bit_allocation_params = BitAllocationParams {
            slow_decay_code: 2,
            fast_decay_code: 1,
            slow_gain_code: 1,
            db_per_bit_code: 2,
            floor_code: 7,
        };
    }
    Ok(())
}

fn read_snr_offsets(
    reader: &mut BitReader<'_>,
    block: usize,
    frame_type: FrameType,
    fullband_channels: usize,
    lfe_on: bool,
    audio_frame: &AudioFrameInfo,
    state: &mut BlockSyntaxState,
) -> Result<(), ParseError> {
    if matches!(frame_type, FrameType::LegacyAc3) {
        if !reader.read_bit().ok_or(ParseError::ShortPacket)? {
            return Ok(());
        }
        state.csnr_offset = reader.read_bits(6).ok_or(ParseError::ShortPacket)? as i32;
        if audio_frame.coupling_in_use[block] {
            state.cpl_fsnr_offset = reader.read_bits(4).ok_or(ParseError::ShortPacket)? as i32;
            state.cpl_fgain_code = reader.read_bits(3).ok_or(ParseError::ShortPacket)? as u8;
        }
        for channel in 0..fullband_channels {
            state.channel_fsnr_offsets[channel] =
                reader.read_bits(4).ok_or(ParseError::ShortPacket)? as i32;
            state.channel_fgain_codes[channel] =
                reader.read_bits(3).ok_or(ParseError::ShortPacket)? as u8;
        }
        if lfe_on {
            state.lfe_fsnr_offset = reader.read_bits(4).ok_or(ParseError::ShortPacket)? as i32;
            state.lfe_fgain_code = reader.read_bits(3).ok_or(ParseError::ShortPacket)? as u8;
        }
        return Ok(());
    }

    if audio_frame.snr_offset_strategy == 0 {
        state.csnr_offset = audio_frame.frame_csnr_offset.unwrap_or_default() as i32;
        let fsnr = audio_frame.frame_fsnr_offset.unwrap_or_default() as i32;
        state.cpl_fsnr_offset = fsnr;
        for offset in &mut state.channel_fsnr_offsets {
            *offset = fsnr;
        }
        if lfe_on {
            state.lfe_fsnr_offset = fsnr;
        }
        return Ok(());
    }

    let snr_offsets_present = if block == 0 {
        true
    } else {
        reader.read_bit().ok_or(ParseError::ShortPacket)?
    };
    if !snr_offsets_present {
        return Ok(());
    }

    state.csnr_offset = reader.read_bits(6).ok_or(ParseError::ShortPacket)? as i32;
    match audio_frame.snr_offset_strategy {
        1 => {
            let block_fsnr = reader.read_bits(4).ok_or(ParseError::ShortPacket)? as i32;
            state.cpl_fsnr_offset = block_fsnr;
            for offset in &mut state.channel_fsnr_offsets {
                *offset = block_fsnr;
            }
            if lfe_on {
                state.lfe_fsnr_offset = block_fsnr;
            }
        }
        2 => {
            if audio_frame.coupling_in_use[block] {
                state.cpl_fsnr_offset = reader.read_bits(4).ok_or(ParseError::ShortPacket)? as i32;
            }
            for channel in 0..fullband_channels {
                state.channel_fsnr_offsets[channel] =
                    reader.read_bits(4).ok_or(ParseError::ShortPacket)? as i32;
            }
            if lfe_on {
                state.lfe_fsnr_offset = reader.read_bits(4).ok_or(ParseError::ShortPacket)? as i32;
            }
        }
        _ => {}
    }
    Ok(())
}

fn read_frame_gain_codes(
    reader: &mut BitReader<'_>,
    block: usize,
    fullband_channels: usize,
    lfe_on: bool,
    audio_frame: &AudioFrameInfo,
    state: &mut BlockSyntaxState,
) -> Result<(), ParseError> {
    let frame_gain_present = audio_frame.frame_gain_syntax_enabled
        && reader.read_bit().ok_or(ParseError::ShortPacket)?;
    if frame_gain_present {
        if audio_frame.coupling_in_use[block] {
            state.cpl_fgain_code = reader.read_bits(3).ok_or(ParseError::ShortPacket)? as u8;
        }
        for channel in 0..fullband_channels {
            state.channel_fgain_codes[channel] =
                reader.read_bits(3).ok_or(ParseError::ShortPacket)? as u8;
        }
        if lfe_on {
            state.lfe_fgain_code = reader.read_bits(3).ok_or(ParseError::ShortPacket)? as u8;
        }
    } else if block == 0 {
        // Per FFmpeg ac3dec.c:1162 — for E-AC-3 with no per-block fast-gain flag,
        // reset to default ONLY on block 0. Subsequent blocks keep the previous
        // fgain values to allow them to persist across the frame.
        state.cpl_fgain_code = 4;
        for fgain in &mut state.channel_fgain_codes {
            *fgain = 4;
        }
        if lfe_on {
            state.lfe_fgain_code = 4;
        }
    }
    Ok(())
}

fn read_delta_bit_allocation(
    reader: &mut BitReader<'_>,
    block: usize,
    fullband_channels: usize,
    audio_frame: &AudioFrameInfo,
    state: &mut BlockSyntaxState,
) -> Result<(), ParseError> {
    if !audio_frame.delta_bit_allocation_enabled
        || !reader.read_bit().ok_or(ParseError::ShortPacket)?
    {
        return Ok(());
    }

    let coupling_mode = if audio_frame.coupling_in_use[block] {
        let mode = DeltaBitAllocationMode::from_bits(
            reader.read_bits(2).ok_or(ParseError::ShortPacket)? as u8,
        )?;
        state.coupling_delta_bit_allocation.mode = mode;
        Some(mode)
    } else {
        None
    };

    for channel in 0..fullband_channels {
        state.channel_delta_bit_allocation[channel].mode = DeltaBitAllocationMode::from_bits(
            reader.read_bits(2).ok_or(ParseError::ShortPacket)? as u8,
        )?;
    }

    if coupling_mode == Some(DeltaBitAllocationMode::NewInfoFollows) {
        state.coupling_delta_bit_allocation.read_segments(reader)?;
    }
    for channel in 0..fullband_channels {
        if state.channel_delta_bit_allocation[channel].mode
            == DeltaBitAllocationMode::NewInfoFollows
        {
            state.channel_delta_bit_allocation[channel].read_segments(reader)?;
        }
    }
    Ok(())
}

fn consume_block_mantissas(
    reader: &mut BitReader<'_>,
    block: usize,
    lfe_on: bool,
    audio_frame: &AudioFrameInfo,
    state: &mut BlockSyntaxState,
    allocation: &BlockAllocationInfo,
) -> Result<(), ParseError> {
    if state.spx_in_use {
        // SPX channels only carry mantissas for the baseband portion (up to
        // spx_begin_subbnd * 12 + 25). count_mantissa_bits already uses the
        // truncated end_mantissa from read_exponents, so the bit accounting is
        // correct. The extension bands are filled by the SPX synthesis pass
        // during actual decode and don't consume mantissa bits here.
    }

    let mut mantissa_groups = MantissaGroupState::new_block();
    let mut total_mantissa_bits = 0usize;
    let first_coupled_channel = if audio_frame.coupling_in_use[block] {
        allocate_coupling_channel(state)?;
        state.chincpl.iter().position(|in_use| *in_use)
    } else {
        None
    };
    for channel in 0..allocation.channel_end_mantissas.len() {
        let end_mantissa = allocation.channel_end_mantissas[channel];
        if end_mantissa > 256 {
            return Err(ParseError::InvalidHeader("mantissa-range"));
        }

        let channel_snr_offset =
            (((state.csnr_offset - 15) << 4) + state.channel_fsnr_offsets[channel]) << 2;
        if state.csnr_offset == 0 && state.channel_fsnr_offsets[channel] == 0 {
            state.channel_allocations[channel].clear_bap();
        } else {
            state.channel_allocations[channel].allocate(
                0,
                end_mantissa,
                state.channel_fgain_codes[channel],
                channel_snr_offset,
                state.bit_allocation_params,
                state.sample_rate_index,
                &state.channel_delta_bit_allocation[channel],
                0,
                0,
            )?;
        }

        let mantissa_bits = state.channel_allocations[channel].count_mantissa_bits(
            0,
            end_mantissa,
            &mut mantissa_groups,
        );
        total_mantissa_bits += mantissa_bits;
        emit_aux_debug(format_args!(
            "block={block} ch={channel} endmant={end_mantissa} csnr={} fsnr={} fgain={} mantissa_bits={mantissa_bits}",
            state.csnr_offset,
            state.channel_fsnr_offsets[channel],
            state.channel_fgain_codes[channel],
        ));
        reader
            .skip_bits(mantissa_bits)
            .ok_or(ParseError::ShortPacket)?;

        if first_coupled_channel == Some(channel) {
            let coupling_bits = state.coupling_allocation.count_mantissa_bits(
                coupling_start_mantissa(state),
                coupling_end_mantissa(state)?,
                &mut mantissa_groups,
            );
            total_mantissa_bits += coupling_bits;
            emit_aux_debug(format_args!(
                "block={block} cpl startmant={} endmant={} csnr={} fsnr={} fgain={} mantissa_bits={coupling_bits}",
                coupling_start_mantissa(state),
                coupling_end_mantissa(state)?,
                state.csnr_offset,
                state.cpl_fsnr_offset,
                state.cpl_fgain_code,
            ));
            reader
                .skip_bits(coupling_bits)
                .ok_or(ParseError::ShortPacket)?;
        }
    }

    if lfe_on {
        if let Some(lfe_allocation) = state.lfe_allocation.as_mut() {
            let lfe_snr_offset = (((state.csnr_offset - 15) << 4) + state.lfe_fsnr_offset) << 2;
            if state.csnr_offset == 0 && state.lfe_fsnr_offset == 0 {
                lfe_allocation.clear_bap();
            } else {
                lfe_allocation.allocate(
                    0,
                    LFE_END_MANTISSA,
                    state.lfe_fgain_code,
                    lfe_snr_offset,
                    state.bit_allocation_params,
                    state.sample_rate_index,
                    &DeltaBitAllocationState::default(),
                    0,
                    0,
                )?;
            }
            let mantissa_bits =
                lfe_allocation.count_mantissa_bits(0, LFE_END_MANTISSA, &mut mantissa_groups);
            total_mantissa_bits += mantissa_bits;
            emit_aux_debug(format_args!(
                "block={block} lfe endmant={} csnr={} fsnr={} fgain={} mantissa_bits={mantissa_bits}",
                LFE_END_MANTISSA, state.csnr_offset, state.lfe_fsnr_offset, state.lfe_fgain_code,
            ));
            reader
                .skip_bits(mantissa_bits)
                .ok_or(ParseError::ShortPacket)?;
        }
    }

    emit_aux_debug(format_args!(
        "block={block} total_mantissa_bits={total_mantissa_bits}"
    ));

    Ok(())
}

pub(crate) fn decode_core_pcm_frame_with_state(
    frame: &[u8],
    info: &AccessUnitInfo,
    state: &mut CoreDecodeState,
) -> Result<CorePcmFrame, ParseError> {
    let mut pcm = CorePcmFrame {
        sample_rate: 0,
        fullband_channel_order: Vec::new(),
        fullband_channels: Vec::new(),
        lfe_channel: None,
    };
    decode_core_pcm_frame_with_state_into(frame, info, state, &mut pcm)?;
    Ok(pcm)
}

pub(crate) fn decode_core_pcm_frame_with_state_into(
    frame: &[u8],
    info: &AccessUnitInfo,
    state: &mut CoreDecodeState,
    pcm: &mut CorePcmFrame,
) -> Result<(), ParseError> {
    if info.frame_type == FrameType::Dependent {
        // TODO: Merge dependent substreams with their associated independent substream before
        // exposing a general PCM path.
        return Err(ParseError::UnsupportedFeature("non-independent-core-pcm"));
    }

    let sample_rate_index =
        sample_rate_index(info.sample_rate).ok_or(ParseError::InvalidHeader("sample-rate"))?;
    let trailing_aux = extract_trailing_aux_data(frame);
    let mut reader = BitReader::with_offset(frame, info.audio_frame.block_payload_start_bit_offset);
    reader.set_limit_bits(trailing_aux.start_bit_offset);

    let fullband_order = fullband_channel_order(info.channel_mode)?;
    if fullband_order.len() != info.fullband_channels as usize {
        return Err(ParseError::InvalidHeader("channel-mode"));
    }

    let frame_samples = info.num_blocks as usize * 256;
    pcm.sample_rate = info.sample_rate;
    pcm.fullband_channel_order.clear();
    pcm.fullband_channel_order.extend_from_slice(fullband_order);
    prepare_core_channel_storage(
        &mut pcm.fullband_channels,
        info.fullband_channels as usize,
        frame_samples,
    );
    prepare_lfe_channel_storage(&mut pcm.lfe_channel, info.lfe_on, frame_samples);
    state.reconfigure(
        info.fullband_channels as usize,
        info.lfe_on,
        sample_rate_index,
    );
    let CoreDecodeState {
        block_syntax,
        imdct,
        lfe_imdct,
        ..
    } = state;
    let block_syntax = block_syntax
        .as_mut()
        .ok_or(ParseError::InvalidHeader("core-decode-state"))?;

    let mut audio_frame = info.audio_frame.clone();
    for block in 0..info.num_blocks as usize {
        decode_block_core_pcm(
            &mut reader,
            block,
            info,
            &mut audio_frame,
            block_syntax,
            imdct,
            lfe_imdct.as_mut(),
            &mut pcm.fullband_channels,
            pcm.lfe_channel.as_mut(),
        )?;
    }

    Ok(())
}

fn prepare_core_channel_storage(channels: &mut Vec<Vec<f32>>, count: usize, samples: usize) {
    channels.resize_with(count, Vec::new);
    if channels.len() > count {
        channels.truncate(count);
    }
    for channel in channels.iter_mut() {
        if channel.len() != samples {
            channel.resize(samples, 0.0);
        }
    }
}

fn prepare_lfe_channel_storage(channel: &mut Option<Vec<f32>>, enabled: bool, samples: usize) {
    if enabled {
        let storage = channel.get_or_insert_with(Vec::new);
        if storage.len() != samples {
            storage.resize(samples, 0.0);
        }
    } else {
        *channel = None;
    }
}

fn fullband_channel_order(channel_mode: u8) -> Result<&'static [BedChannel], ParseError> {
    match channel_mode {
        0 => Ok(&[BedChannel::Center, BedChannel::Center]),
        1 => Ok(&[BedChannel::Center]),
        2 => Ok(&[BedChannel::FrontLeft, BedChannel::FrontRight]),
        3 => Ok(&[
            BedChannel::FrontLeft,
            BedChannel::Center,
            BedChannel::FrontRight,
        ]),
        4 => Ok(&[
            BedChannel::FrontLeft,
            BedChannel::FrontRight,
            BedChannel::RearCenter,
        ]),
        5 => Ok(&[
            BedChannel::FrontLeft,
            BedChannel::Center,
            BedChannel::FrontRight,
            BedChannel::RearCenter,
        ]),
        6 => Ok(&[
            BedChannel::FrontLeft,
            BedChannel::FrontRight,
            BedChannel::SurroundLeft,
            BedChannel::SurroundRight,
        ]),
        7 => Ok(&[
            BedChannel::FrontLeft,
            BedChannel::Center,
            BedChannel::FrontRight,
            BedChannel::SurroundLeft,
            BedChannel::SurroundRight,
        ]),
        _ => Err(ParseError::InvalidHeader("channel-mode")),
    }
}

fn bittrace_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("HARLETTY_EAC3_BITTRACE").is_some())
}

#[inline]
fn bittrace(tag: &str, block: usize, pos: usize) {
    if bittrace_enabled() {
        eprintln!("BITTRACE\tblk={block}\ttag={tag}\tbit_pos={pos}");
    }
}

#[inline]
pub(crate) fn bittrace_ch(tag: &str, block: usize, ch: i32, pos: usize) {
    if bittrace_enabled() {
        eprintln!("BITTRACE\tblk={block}\tch={ch}\ttag={tag}\tbit_pos={pos}");
    }
}

fn decode_block_core_pcm(
    reader: &mut BitReader<'_>,
    block: usize,
    info: &AccessUnitInfo,
    audio_frame: &mut AudioFrameInfo,
    state: &mut BlockSyntaxState,
    imdct: &mut [ImdctState],
    lfe_imdct: Option<&mut ImdctState>,
    fullband_channels: &mut [Vec<f32>],
    lfe_channel: Option<&mut Vec<f32>>,
) -> Result<(), ParseError> {
    let fullband_count = info.fullband_channels as usize;
    let mut block_switch = vec![false; fullband_count];

    bittrace("audblk_start", block, reader.position());

    if audio_frame.block_switching_enabled {
        for flag in &mut block_switch {
            *flag = reader.read_bit().ok_or(ParseError::ShortPacket)?;
        }
    }
    if audio_frame.dithering_enabled {
        for _ in 0..fullband_count {
            // TODO: Inject AC-3 dithering for zero-BAP bins if it matters in practice.
            reader.read_bit().ok_or(ParseError::ShortPacket)?;
        }
    }

    skip_conditional_bits(reader, 8)?;
    if info.channel_mode == 0 {
        skip_conditional_bits(reader, 8)?;
    }
    bittrace("after_dynrng", block, reader.position());

    if matches!(info.frame_type, FrameType::LegacyAc3) {
        read_legacy_ac3_coupling_strategy(
            reader,
            block,
            info.channel_mode,
            fullband_count,
            audio_frame,
            state,
        )?;
        read_legacy_ac3_coupling_coordinates(
            reader,
            block,
            info.channel_mode,
            fullband_count,
            audio_frame,
            state,
        )?;
        read_legacy_ac3_exponent_strategies(
            reader,
            block,
            fullband_count,
            info.lfe_on,
            audio_frame,
        )?;
    } else {
        read_spx(reader, block, info.channel_mode, fullband_count, state)?;
        bittrace("after_spx", block, reader.position());
        read_coupling_strategy(
            reader,
            block,
            info.channel_mode,
            fullband_count,
            audio_frame,
            state,
        )?;
        bittrace("after_cpl_strategy", block, reader.position());
        read_coupling_coordinates(
            reader,
            block,
            info.channel_mode,
            fullband_count,
            audio_frame,
            state,
        )?;
        bittrace("after_cpl_coords", block, reader.position());
    }

    let allocation = read_exponents(
        reader,
        block,
        fullband_count,
        info.lfe_on,
        audio_frame,
        state,
    )?;
    bittrace("after_exponents", block, reader.position());
    read_bit_allocation_params(reader, block, audio_frame, state)?;
    bittrace("after_bit_alloc", block, reader.position());
    read_snr_offsets(
        reader,
        block,
        info.frame_type,
        fullband_count,
        info.lfe_on,
        audio_frame,
        state,
    )?;
    bittrace("after_snr", block, reader.position());
    if !matches!(info.frame_type, FrameType::LegacyAc3) {
        read_frame_gain_codes(
            reader,
            block,
            fullband_count,
            info.lfe_on,
            audio_frame,
            state,
        )?;
        bittrace("after_fast_gain", block, reader.position());
    }

    if info.frame_type == FrameType::Independent
        && reader.read_bit().ok_or(ParseError::ShortPacket)?
    {
        reader.skip_bits(10).ok_or(ParseError::ShortPacket)?;
    }
    bittrace("after_convsnr", block, reader.position());

    if matches!(info.frame_type, FrameType::LegacyAc3) {
        read_legacy_ac3_coupling_leak_info(reader, audio_frame.coupling_in_use[block], state)?;
    } else {
        read_coupling_leak_info(reader, audio_frame.coupling_in_use[block], state)?;
    }
    bittrace("after_cpl_leak", block, reader.position());

    read_delta_bit_allocation(reader, block, fullband_count, audio_frame, state)?;
    bittrace("after_dba", block, reader.position());

    if audio_frame.skip_field_syntax_enabled && reader.read_bit().ok_or(ParseError::ShortPacket)? {
        let skip_length = reader.read_bits(9).ok_or(ParseError::ShortPacket)? as usize;
        reader
            .skip_bits(skip_length * 8)
            .ok_or(ParseError::ShortPacket)?;
    }
    bittrace("after_skip", block, reader.position());

    let block_offset = block * 256;
    decode_block_pcm_mantissas(
        reader,
        block,
        block_offset,
        info.lfe_on,
        state,
        &allocation,
        &block_switch,
        imdct,
        lfe_imdct,
        fullband_channels,
        lfe_channel,
    )?;
    bittrace("audblk_end", block, reader.position());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_block_pcm_mantissas(
    reader: &mut BitReader<'_>,
    block: usize,
    block_offset: usize,
    lfe_on: bool,
    state: &mut BlockSyntaxState,
    allocation: &BlockAllocationInfo,
    block_switch: &[bool],
    imdct: &mut [ImdctState],
    lfe_imdct: Option<&mut ImdctState>,
    fullband_channels: &mut [Vec<f32>],
    lfe_channel: Option<&mut Vec<f32>>,
) -> Result<(), ParseError> {
    let mut mantissa_state = MantissaDecodeState::new_block();
    let mut coupling_coeffs = [0.0f32; 256];
    let first_coupled_channel = if state.chincpl.iter().any(|in_use| *in_use) {
        allocate_coupling_channel(state)?;
        state.chincpl.iter().position(|in_use| *in_use)
    } else {
        None
    };

    for channel in 0..allocation.channel_end_mantissas.len() {
        let end_mantissa = allocation.channel_end_mantissas[channel];
        if end_mantissa > 256 {
            return Err(ParseError::InvalidHeader("mantissa-range"));
        }

        let channel_snr_offset =
            (((state.csnr_offset - 15) << 4) + state.channel_fsnr_offsets[channel]) << 2;
        if state.csnr_offset == 0 && state.channel_fsnr_offsets[channel] == 0 {
            state.channel_allocations[channel].clear_bap();
        } else {
            state.channel_allocations[channel].allocate(
                0,
                end_mantissa,
                state.channel_fgain_codes[channel],
                channel_snr_offset,
                state.bit_allocation_params,
                state.sample_rate_index,
                &state.channel_delta_bit_allocation[channel],
                0,
                0,
            )?;
        }

        bittrace_ch("mantissas_ch_start", block, channel as i32, reader.position());
        let mut coeffs = [0.0f32; 256];
        state.channel_allocations[channel].decode_transform_coeffs(
            reader,
            &mut coeffs,
            0,
            end_mantissa,
            &mut mantissa_state,
        )?;
        if first_coupled_channel == Some(channel) {
            bittrace_ch("mantissas_cpl_start", block, -1, reader.position());
            state.coupling_allocation.decode_transform_coeffs(
                reader,
                &mut coupling_coeffs,
                coupling_start_mantissa(state),
                coupling_end_mantissa(state)?,
                &mut mantissa_state,
            )?;
        }
        if state.chincpl[channel] {
            apply_standard_coupling(state, channel, &mut coeffs, &coupling_coeffs);
        }
        if state.spx_in_use && state.chinspx[channel] {
            apply_spx_extension(&mut coeffs, end_mantissa, state, channel);
        }
        imdct[channel].apply(
            &coeffs,
            block_switch.get(channel).copied().unwrap_or(false),
            &mut fullband_channels[channel][block_offset..block_offset + 256],
        );
    }

    if lfe_on {
        if let (Some(lfe_allocation), Some(lfe_imdct), Some(lfe_channel)) =
            (state.lfe_allocation.as_mut(), lfe_imdct, lfe_channel)
        {
            let lfe_snr_offset = (((state.csnr_offset - 15) << 4) + state.lfe_fsnr_offset) << 2;
            if state.csnr_offset == 0 && state.lfe_fsnr_offset == 0 {
                lfe_allocation.clear_bap();
            } else {
                lfe_allocation.allocate(
                    0,
                    LFE_END_MANTISSA,
                    state.lfe_fgain_code,
                    lfe_snr_offset,
                    state.bit_allocation_params,
                    state.sample_rate_index,
                    &DeltaBitAllocationState::default(),
                    0,
                    0,
                )?;
            }

            bittrace_ch("mantissas_lfe_start", block, -2, reader.position());
            let mut coeffs = [0.0f32; 256];
            lfe_allocation.decode_transform_coeffs(
                reader,
                &mut coeffs,
                0,
                LFE_END_MANTISSA,
                &mut mantissa_state,
            )?;
            lfe_imdct.apply(
                &coeffs,
                false,
                &mut lfe_channel[block_offset..block_offset + 256],
            );
        }
    }

    Ok(())
}

/// Apply spectral extension to fill the high-frequency bins of `coeffs`.
///
/// After mantissa decode, SPX channels only have transform coefficients in the
/// baseband region (bins 0..end_mantissa). This function fills the extension
/// region by copying the baseband's spectral envelope, scaled by per-band SPX
/// gains. The result is a full 256-point spectrum ready for the IMDCT.
fn apply_spx_extension(
    coeffs: &mut [f32; 256],
    baseband_end: usize,
    state: &BlockSyntaxState,
    channel: usize,
) {
    let Some(gains) = state.spx_gains.get(channel) else {
        emit_aux_debug(format_args!(
            "spx_ext: no gains for channel {channel} (spx_in_use={} gains_len={})",
            state.spx_in_use,
            state.spx_gains.len(),
        ));
        return;
    };
    let nbands = gains.len();
    if nbands == 0 {
        return;
    }

    let extension_start = state.spx_begin_subbnd * 12 + 25;
    let extension_end = (state.spx_end_subbnd * 12 + 37).min(256);
    if extension_start >= extension_end || extension_start >= 256 {
        return;
    }

    let extension_len = extension_end - extension_start;

    for band in 0..nbands {
        let band_start = extension_start + (band * extension_len) / nbands;
        let band_end = extension_start + ((band + 1) * extension_len) / nbands;
        let src_start = (band * baseband_end) / nbands;

        for bin in band_start..band_end {
            let src_bin = src_start + (bin - band_start) % (baseband_end - src_start).max(1);
            coeffs[bin] = coeffs[src_bin] * gains[band];
        }
    }
}

fn scan_frame_for_emdf(
    frame: &[u8],
    metadata_state: &mut MetadataParseState,
) -> (EmdfSource, Vec<EmdfBlockInfo>) {
    let blocks = scan_emdf_blocks_with_metadata_state(frame, metadata_state);
    let source = if blocks.is_empty() {
        EmdfSource::None
    } else {
        EmdfSource::FrameScanFallback
    };
    (source, blocks)
}

fn scan_emdf_blocks(data: &[u8]) -> Vec<EmdfBlockInfo> {
    let mut metadata_state = MetadataParseState::default();
    scan_emdf_blocks_with_metadata_state(data, &mut metadata_state)
}

fn scan_emdf_blocks_with_metadata_state(
    data: &[u8],
    metadata_state: &mut MetadataParseState,
) -> Vec<EmdfBlockInfo> {
    let mut blocks = Vec::new();
    let mut offset = 0usize;

    while offset + 4 <= data.len() {
        if data[offset] == 0x58 && data[offset + 1] == 0x38 {
            if let Some((block, next_offset)) =
                parse_emdf_block(data, offset, blocks.len(), metadata_state)
            {
                blocks.push(block);
                offset = next_offset;
                continue;
            }
        }
        offset += 1;
    }
    blocks
}

fn parse_emdf_block(
    frame: &[u8],
    sync_offset: usize,
    block_index: usize,
    metadata_state: &mut MetadataParseState,
) -> Option<(EmdfBlockInfo, usize)> {
    let mut reader = BitReader::with_offset(frame, sync_offset * 8);
    let sync = reader.read_bits(16)?;
    if sync != 0x5838 {
        return None;
    }
    let length = reader.read_bits(16)? as usize;
    let block_end_bit = reader.position() + length * 8;
    if block_end_bit > frame.len() * 8 {
        return None;
    }

    let version = match reader.read_bits(2)? {
        3 => 3 + reader.read_variable_bits(2)?,
        other => other,
    };
    if version != 0 {
        return None;
    }

    let key = match reader.read_bits(3)? {
        7 => 7 + reader.read_variable_bits(3)?,
        other => other,
    };
    if key != 0 {
        return None;
    }

    let mut payloads = Vec::new();
    while reader.position() < block_end_bit {
        let mut payload_id = reader.read_bits(5)?;
        if payload_id == 0 {
            break;
        }
        if payload_id == 0x1F {
            payload_id += reader.read_variable_bits(5)?;
        }

        let has_sample_offset = reader.read_bit()?;
        let sample_offset = if has_sample_offset {
            Some((reader.read_bits(12)? >> 1) as u16)
        } else {
            None
        };

        if reader.read_bit()? {
            reader.skip_variable_bits(11)?;
        }
        if reader.read_bit()? {
            reader.skip_variable_bits(2)?;
        }
        if reader.read_bit()? {
            reader.skip_bits(8)?;
        }

        let frame_not_aligned = reader.read_bit()?;
        if !frame_not_aligned {
            let mut frame_aligned = false;
            if !has_sample_offset {
                frame_aligned = reader.read_bit()?;
                if frame_aligned {
                    reader.skip_bits(2)?;
                }
            }
            if has_sample_offset || frame_aligned {
                reader.skip_bits(7)?;
            }
        }

        let payload_size_bytes = reader.read_variable_bits(8)? as usize;
        if reader.position() + payload_size_bytes * 8 > block_end_bit {
            return None;
        }

        let info = PayloadInfo {
            emdf_block_index: block_index,
            payload_id: payload_id as u8,
            payload_size_bytes,
            sample_offset,
        };
        let bytes = reader.read_bytes(payload_size_bytes)?;
        let (parsed, parse_error) =
            match parse_emdf_payload_body_with_state(info.payload_id, &bytes, metadata_state) {
                Ok(parsed) => (parsed, None),
                Err(err) => (ParsedEmdfPayloadData::Unknown, Some(err)),
            };

        payloads.push(EmdfPayloadInfo {
            info,
            bytes,
            parsed,
            parse_error,
        });
    }

    if payloads.is_empty() {
        return None;
    }

    Some((
        EmdfBlockInfo {
            sync_offset,
            payloads,
        },
        block_end_bit.div_ceil(8),
    ))
}

fn payload_name(payload_id: u8) -> &'static str {
    match payload_id {
        11 => "oamd",
        14 => "joc",
        _ => "unknown",
    }
}

fn log2_ceil(value: usize) -> usize {
    if value <= 1 {
        0
    } else {
        usize::BITS as usize - (value - 1).leading_zeros() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AccessUnitInfo, AudioFrameInfo, BlockSyntaxState, CoreDecodeState, EmdfSource, ExpStrategy,
        FrameType, ParseError, apply_spx_extension, decode_block_core_pcm, inspect_access_unit,
    };
    use crate::BedChannel;

    fn push_bits(bits: &mut Vec<bool>, value: u32, width: usize) {
        for bit in (0..width).rev() {
            bits.push(((value >> bit) & 1) != 0);
        }
    }

    fn push_variable_bits(bits: &mut Vec<bool>, value: u32, width: usize) {
        push_bits(bits, value, width);
        bits.push(false);
    }

    fn bits_to_bytes(bits: &[bool], frame_size: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; frame_size];
        for (index, bit) in bits.iter().copied().enumerate() {
            if bit {
                bytes[index >> 3] |= 1 << (7 - (index & 7));
            }
        }
        bytes
    }

    fn build_emdf_block(payload_id: u8, payload_bytes: &[u8]) -> Vec<u8> {
        let mut payload_bits = Vec::new();
        push_bits(&mut payload_bits, 0, 2);
        push_bits(&mut payload_bits, 0, 3);
        push_bits(&mut payload_bits, payload_id as u32, 5);
        push_bits(&mut payload_bits, 0, 1);
        push_bits(&mut payload_bits, 0, 1);
        push_bits(&mut payload_bits, 0, 1);
        push_bits(&mut payload_bits, 0, 1);
        push_bits(&mut payload_bits, 1, 1);
        push_variable_bits(&mut payload_bits, payload_bytes.len() as u32, 8);
        for &byte in payload_bytes {
            push_bits(&mut payload_bits, byte as u32, 8);
        }
        push_bits(&mut payload_bits, 0, 5);
        while payload_bits.len() % 8 != 0 {
            payload_bits.push(false);
        }

        let mut bits = Vec::new();
        push_bits(&mut bits, 0x5838, 16);
        push_bits(&mut bits, (payload_bits.len() / 8) as u32, 16);
        bits.extend(payload_bits);
        bits_to_bytes(&bits, bits.len().div_ceil(8))
    }

    pub(crate) fn build_minimal_eac3_frame(frame_size: usize) -> Vec<u8> {
        let mut bits = Vec::new();
        let frmsiz = ((frame_size / 2) - 1) as u32;

        push_bits(&mut bits, 0x0B77, 16);
        push_bits(&mut bits, 0, 2);
        push_bits(&mut bits, 0, 3);
        push_bits(&mut bits, frmsiz, 11);
        push_bits(&mut bits, 0, 2);
        push_bits(&mut bits, 3, 2);
        push_bits(&mut bits, 7, 3);
        push_bits(&mut bits, 1, 1);
        push_bits(&mut bits, 16, 5);
        push_bits(&mut bits, 0, 5);
        push_bits(&mut bits, 0, 1);
        push_bits(&mut bits, 0, 1);
        push_bits(&mut bits, 0, 1);
        push_bits(&mut bits, 0, 1);

        let mut bytes = vec![0u8; frame_size];
        for (index, bit) in bits.iter().copied().enumerate() {
            if bit {
                bytes[index >> 3] |= 1 << (7 - (index & 7));
            }
        }
        bytes
    }

    fn push_zeroed_channel_exponents(
        bits: &mut Vec<bool>,
        strategy: ExpStrategy,
        end_mantissa: usize,
    ) {
        let groups = super::grouped_exponent_count(end_mantissa, strategy)
            .expect("channel exponent group count");
        push_bits(bits, 0, 4);
        for _ in 0..groups {
            push_bits(bits, 0, 7);
        }
        push_bits(bits, 0, 2);
    }

    fn push_zeroed_coupling_exponents(
        bits: &mut Vec<bool>,
        strategy: ExpStrategy,
        start_mantissa: usize,
        end_mantissa: usize,
    ) {
        let groups =
            super::grouped_exponent_count(end_mantissa.saturating_sub(start_mantissa), strategy)
                .expect("coupling exponent group count");
        push_bits(bits, 0, 4);
        for _ in 0..groups {
            push_bits(bits, 0, 7);
        }
    }

    fn build_single_block_payload(frame_type: FrameType, skip_bytes: &[u8]) -> Vec<u8> {
        let mut bits = Vec::new();
        push_bits(&mut bits, 0, 1);
        push_bits(&mut bits, 0, 1);
        push_bits(&mut bits, 0, 6);
        push_zeroed_channel_exponents(&mut bits, ExpStrategy::D45, 73);
        if frame_type == FrameType::Independent {
            push_bits(&mut bits, 0, 1);
        }
        push_bits(&mut bits, 1, 1);
        push_bits(&mut bits, skip_bytes.len() as u32, 9);
        for &byte in skip_bytes {
            push_bits(&mut bits, byte as u32, 8);
        }
        bits_to_bytes(&bits, bits.len().div_ceil(8))
    }

    fn build_single_block_coupling_payload(frame_type: FrameType, skip_bytes: &[u8]) -> Vec<u8> {
        let mut bits = Vec::new();
        push_bits(&mut bits, 0, 1);
        push_bits(&mut bits, 0, 1);
        push_bits(&mut bits, 0, 1);
        push_bits(&mut bits, 1, 1);
        push_bits(&mut bits, 1, 1);
        push_bits(&mut bits, 1, 1);
        push_bits(&mut bits, 0, 4);
        push_bits(&mut bits, 0, 4);
        push_bits(&mut bits, 0, 1);
        for _ in 0..3 {
            push_bits(&mut bits, 0, 2);
            for _ in 0..3 {
                push_bits(&mut bits, 0, 4);
                push_bits(&mut bits, 0, 4);
            }
        }
        push_zeroed_coupling_exponents(&mut bits, ExpStrategy::D45, 37, 73);
        for _ in 0..3 {
            push_zeroed_channel_exponents(&mut bits, ExpStrategy::D45, 37);
        }
        if frame_type == FrameType::Independent {
            push_bits(&mut bits, 0, 1);
        }
        push_bits(&mut bits, 0, 3);
        push_bits(&mut bits, 0, 3);
        push_bits(&mut bits, 1, 1);
        push_bits(&mut bits, skip_bytes.len() as u32, 9);
        for &byte in skip_bytes {
            push_bits(&mut bits, byte as u32, 8);
        }
        bits_to_bytes(&bits, bits.len().div_ceil(8))
    }

    fn single_block_audio_frame(fullband_channels: usize, coupling_in_use: bool) -> AudioFrameInfo {
        AudioFrameInfo {
            exponent_strategies_embedded: true,
            adaptive_hybrid_transform_enabled: false,
            snr_offset_strategy: 0,
            transient_processing_enabled: false,
            block_switching_enabled: false,
            dithering_enabled: false,
            bit_allocation_mode_enabled: false,
            frame_gain_syntax_enabled: false,
            delta_bit_allocation_enabled: false,
            skip_field_syntax_enabled: true,
            spectral_extension_attenuation_enabled: false,
            coupling_strategy_updates: vec![coupling_in_use],
            coupling_in_use: vec![coupling_in_use],
            coupling_exponent_strategy: vec![coupling_in_use.then_some(ExpStrategy::D45)],
            channel_exponent_strategy: vec![vec![ExpStrategy::D45; fullband_channels]],
            lfe_exponent_strategy: Vec::new(),
            converter_exponent_strategy_present: false,
            converter_exponent_strategy: Vec::new(),
            frame_csnr_offset: Some(0),
            frame_fsnr_offset: Some(0),
            transient_processors: vec![None],
            spectral_extension_attenuation: vec![None],
            block_start_info_present: false,
            block_start_info_bit_len: 0,
            block_payload_start_bit_offset: 0,
        }
    }

    #[test]
    fn inspects_minimal_access_unit() {
        let frame = build_minimal_eac3_frame(32);
        let info = inspect_access_unit(&frame).expect("access unit should parse");
        assert_eq!(info.frame_size, 32);
        assert_eq!(info.bitstream_id, 16);
        assert_eq!(info.frame_type, FrameType::Independent);
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.num_blocks, 6);
        assert!(info.lfe_on);
        assert_eq!(info.channels, 6);
        assert_eq!(info.fullband_channels, 5);
        assert_eq!(info.audio_frame.channel_exponent_strategy.len(), 6);
        assert_eq!(
            info.audio_frame.channel_exponent_strategy[0][0],
            ExpStrategy::D15
        );
        assert!(info.audio_frame.converter_exponent_strategy_present);
        assert_eq!(info.emdf_source, EmdfSource::None);
    }

    #[test]
    fn rejects_short_input() {
        let err = inspect_access_unit(&[0x0B, 0x77, 0x00]).expect_err("short input");
        assert_eq!(err, ParseError::ShortPacket);
    }

    #[test]
    fn extracts_aux_data_from_single_block_skip_field() {
        let emdf = build_emdf_block(14, &[0xAA]);
        assert_eq!(super::scan_emdf_blocks(&emdf).len(), 1);
        let block_payload = build_single_block_payload(FrameType::Independent, &emdf);
        let audio_frame = super::AudioFrameInfo {
            exponent_strategies_embedded: true,
            adaptive_hybrid_transform_enabled: false,
            snr_offset_strategy: 0,
            transient_processing_enabled: false,
            block_switching_enabled: false,
            dithering_enabled: false,
            bit_allocation_mode_enabled: false,
            frame_gain_syntax_enabled: false,
            delta_bit_allocation_enabled: false,
            skip_field_syntax_enabled: true,
            spectral_extension_attenuation_enabled: false,
            coupling_strategy_updates: vec![false],
            coupling_in_use: vec![false],
            coupling_exponent_strategy: vec![None],
            channel_exponent_strategy: vec![vec![ExpStrategy::D45]],
            lfe_exponent_strategy: Vec::new(),
            converter_exponent_strategy_present: false,
            converter_exponent_strategy: Vec::new(),
            frame_csnr_offset: Some(0),
            frame_fsnr_offset: Some(0),
            transient_processors: vec![None],
            spectral_extension_attenuation: vec![None],
            block_start_info_present: false,
            block_start_info_bit_len: 0,
            block_payload_start_bit_offset: 0,
        };

        let skip_fields = super::collect_skip_fields(
            &block_payload,
            FrameType::Independent,
            1,
            1,
            1,
            false,
            &audio_frame,
            None,
            block_payload.len() * 8,
            0,
        )
        .expect("single-block payload should parse");

        assert_eq!(skip_fields.len(), 1);
        assert_eq!(skip_fields[0].block_index, Some(0));
        assert_eq!(skip_fields[0].bytes, emdf);
        assert_eq!(super::scan_emdf_blocks(&skip_fields[0].bytes).len(), 1);
    }

    #[test]
    fn accepts_zero_padded_tail_without_block_start_info() {
        let emdf = build_emdf_block(14, &[0xAA]);
        let mut block_payload = build_single_block_payload(FrameType::Independent, &emdf);
        block_payload.extend_from_slice(&[0u8; 16]);

        let audio_frame = single_block_audio_frame(1, false);
        let mut state = BlockSyntaxState::new(1, false, 0);

        let skip_fields = super::collect_skip_fields_without_block_start(
            &block_payload,
            FrameType::Independent,
            1,
            1,
            1,
            false,
            &audio_frame,
            block_payload.len() * 8,
            &mut state,
        )
        .expect("single-block payload with zero padding should parse");

        assert_eq!(skip_fields.len(), 1);
        assert_eq!(skip_fields[0].bytes, emdf);
    }

    #[test]
    fn accepts_zero_padded_tail_with_footer_prefix_without_block_start_info() {
        let emdf = build_emdf_block(14, &[0xAA]);
        let mut block_payload = build_single_block_payload(FrameType::Independent, &emdf);
        block_payload.extend_from_slice(&[0u8; 16]);
        block_payload.push(0x02);

        let audio_frame = single_block_audio_frame(1, false);
        let mut state = BlockSyntaxState::new(1, false, 0);

        let skip_fields = super::collect_skip_fields_without_block_start(
            &block_payload,
            FrameType::Independent,
            1,
            1,
            1,
            false,
            &audio_frame,
            block_payload.len() * 8,
            &mut state,
        )
        .expect("single-block payload with footer prefix should parse");

        assert_eq!(skip_fields.len(), 1);
        assert_eq!(skip_fields[0].bytes, emdf);
    }

    #[test]
    fn extracts_aux_data_from_single_block_coupling_without_block_start_info() {
        let emdf = build_emdf_block(14, &[0xAA]);
        let block_payload = build_single_block_coupling_payload(FrameType::Independent, &emdf);
        let audio_frame = single_block_audio_frame(3, true);
        let mut state = BlockSyntaxState::new(3, false, 0);

        let skip_fields = super::collect_skip_fields_without_block_start(
            &block_payload,
            FrameType::Independent,
            1,
            3,
            3,
            false,
            &audio_frame,
            block_payload.len() * 8,
            &mut state,
        )
        .expect("single-block payload with coupling should parse");

        assert_eq!(skip_fields.len(), 1);
        assert_eq!(skip_fields[0].bytes, emdf);
    }

    #[test]
    fn decodes_single_block_coupling_pcm_to_silence() {
        let payload = build_single_block_coupling_payload(FrameType::Independent, &[]);
        let mut audio_frame = single_block_audio_frame(3, true);
        let info = AccessUnitInfo {
            frame_size: payload.len(),
            bitstream_id: 16,
            frame_type: FrameType::Independent,
            substreamid: 0,
            sample_rate: 48_000,
            num_blocks: 1,
            channel_mode: 3,
            channels: 3,
            fullband_channels: 3,
            lfe_on: false,
            addbsi_present: false,
            extension_type_a: false,
            complexity_index_type_a: 0,
            mixing_metadata_present: false,
            informational_metadata_present: false,
            addbsi_bytes: Vec::new(),
            body_start_bit_offset: 0,
            audio_frame: audio_frame.clone(),
            skip_fields: Vec::new(),
            trailing_aux_data: Vec::new(),
            aux_data: Vec::new(),
            aux_parse_status: super::AuxParseStatus::Disabled,
            emdf_source: EmdfSource::None,
            emdf_blocks: Vec::new(),
            emdf_block_count: 0,
            first_emdf_sync_offset: None,
        };
        let mut state = BlockSyntaxState::new(3, false, 0);
        let mut reader = super::BitReader::new(&payload);
        let mut imdct = vec![
            super::ImdctState::new(),
            super::ImdctState::new(),
            super::ImdctState::new(),
        ];
        let mut channels = vec![vec![1.0f32; 256], vec![1.0f32; 256], vec![1.0f32; 256]];

        decode_block_core_pcm(
            &mut reader,
            0,
            &info,
            &mut audio_frame,
            &mut state,
            &mut imdct,
            None,
            &mut channels,
            None,
        )
        .expect("coupling PCM block should decode");

        assert!(
            channels
                .iter()
                .flat_map(|channel| channel.iter())
                .all(|sample| sample.abs() < 1e-6)
        );
    }

    #[test]
    fn pcm_channel_order_supports_dual_mono_and_rear_center_layouts() {
        assert_eq!(
            super::fullband_channel_order(0).expect("dual mono should map"),
            &[BedChannel::Center, BedChannel::Center]
        );
        assert_eq!(
            super::fullband_channel_order(4).expect("rear-center stereo should map"),
            &[
                BedChannel::FrontLeft,
                BedChannel::FrontRight,
                BedChannel::RearCenter,
            ]
        );
        assert_eq!(
            super::fullband_channel_order(5).expect("rear-center 4.0 should map"),
            &[
                BedChannel::FrontLeft,
                BedChannel::Center,
                BedChannel::FrontRight,
                BedChannel::RearCenter,
            ]
        );
    }

    #[test]
    fn ac3_convert_frame_type_uses_general_pcm_path() {
        let mut frame = build_single_block_payload(FrameType::Ac3Convert, &[]);
        frame.resize(32, 0);
        let info = AccessUnitInfo {
            frame_size: frame.len(),
            bitstream_id: 16,
            frame_type: FrameType::Ac3Convert,
            substreamid: 0,
            sample_rate: 48_000,
            num_blocks: 1,
            channel_mode: 1,
            channels: 1,
            fullband_channels: 1,
            lfe_on: false,
            addbsi_present: false,
            extension_type_a: false,
            complexity_index_type_a: 0,
            mixing_metadata_present: false,
            informational_metadata_present: false,
            addbsi_bytes: Vec::new(),
            body_start_bit_offset: 0,
            audio_frame: single_block_audio_frame(1, false),
            skip_fields: Vec::new(),
            trailing_aux_data: Vec::new(),
            aux_data: Vec::new(),
            aux_parse_status: super::AuxParseStatus::Disabled,
            emdf_source: EmdfSource::None,
            emdf_blocks: Vec::new(),
            emdf_block_count: 0,
            first_emdf_sync_offset: None,
        };

        let mut state = CoreDecodeState::default();
        let pcm = super::decode_core_pcm_frame_with_state(&frame, &info, &mut state)
            .expect("converted frame should decode through PCM path");
        assert_eq!(pcm.sample_rate, 48_000);
        assert_eq!(pcm.fullband_channel_order, vec![BedChannel::Center]);
    }

    #[test]
    fn spx_extension_copies_baseband_with_gains() {
        let mut state = BlockSyntaxState::new(1, false, 0);
        state.spx_in_use = true;
        state.spx_begin_subbnd = 8;
        state.spx_end_subbnd = 14;
        state.spx_gains = vec![vec![0.5, 2.0, 1.0]];
        state.chinspx = vec![true];

        // Fill baseband with a known ramp (bins 0..baseband_end).
        // baseband_end = spx_begin_subbnd * 12 + 25 = 8*12+25 = 121
        let baseband_end = 121;
        let mut coeffs = [0.0f32; 256];
        for bin in 0..baseband_end {
            coeffs[bin] = bin as f32;
        }

        apply_spx_extension(&mut coeffs, baseband_end, &state, 0);

        // extension_start = 8*12+25 = 121, extension_end = min(14*12+37, 256) = 205
        // extension_len = 205-121 = 84, 3 bands.
        // Band 0: bins [121..149), src_start=0 -> copies coeffs[0..28] * 0.5
        // Band 1: bins [149..177), src_start=40 -> copies coeffs[40..68] * 2.0
        // Band 2: bins [177..205), src_start=80 -> copies coeffs[80..108] * 1.0
        for bin in 121..149 {
            let expected = (bin - 121) as f32 * 0.5;
            assert!(
                (coeffs[bin] - expected).abs() < 1e-6,
                "band 0 bin {bin}: expected {expected}, got {}",
                coeffs[bin]
            );
        }
        for bin in 149..177 {
            let expected = (40 + (bin - 149)) as f32 * 2.0;
            assert!(
                (coeffs[bin] - expected).abs() < 1e-6,
                "band 1 bin {bin}: expected {expected}, got {}",
                coeffs[bin]
            );
        }
        for bin in 177..205 {
            let expected = (80 + (bin - 177)) as f32 * 1.0;
            assert!(
                (coeffs[bin] - expected).abs() < 1e-6,
                "band 2 bin {bin}: expected {expected}, got {}",
                coeffs[bin]
            );
        }

        // Baseband should be unchanged.
        for bin in 0..baseband_end {
            assert!((coeffs[bin] - bin as f32).abs() < 1e-6);
        }
        // Bins above extension_end should remain zero.
        for bin in 205..256 {
            assert!(coeffs[bin].abs() < 1e-6);
        }
    }
}
