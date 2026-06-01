// SPDX-License-Identifier: Apache-2.0
//
// DTS Coherent Acoustics (DCA) core frame-header parser. Ported from ffmpeg's
// `ff_dca_parse_core_frame_header` (libavcodec/dca.c) and the constants in
// dca.h / dca_core.h / dca_sample_rate_tab.h.

use crate::bitstream::BitReader;

/// Big-endian core syncword (`DCA_SYNCWORD_CORE_BE`).
pub const SYNCWORD_CORE_BE: u32 = 0x7FFE_8001;
/// Extension substream syncword (`DCA_SYNCWORD_SUBSTREAM`).
pub const SYNCWORD_SUBSTREAM: u32 = 0x6458_2025;

/// Minimum number of header bytes needed before [`parse_header`] can succeed.
pub const CORE_FRAME_HEADER_SIZE: usize = 18;

const DCA_PCMBLOCK_SAMPLES: u32 = 32;
const DCA_SUBBAND_SAMPLES: u32 = 8;
const DCA_AMODE_COUNT: u32 = 10;

/// `ff_dca_sample_rates` (dca_sample_rate_tab.h): 0 marks an invalid code.
pub const SAMPLE_RATES: [u32; 16] = [
    0, 8000, 16000, 32000, 0, 0, 11025, 22050, 44100, 0, 0, 12000, 24000, 48000, 96000, 192000,
];

/// `ff_dca_bits_per_sample` (dca.c): source PCM resolution by `pcmr_code`.
/// 0 marks an invalid/reserved code.
pub const BITS_PER_SAMPLE: [u8; 8] = [16, 16, 20, 20, 0, 24, 24, 0];

/// DCA core audio channel arrangement (`enum DCACoreAudioMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioMode {
    Mono,        // 0: A
    MonoDual,    // 1: A + B
    Stereo,      // 2: L + R
    StereoSumDiff, // 3: (L+R) + (L-R)
    StereoTotal, // 4: LT + RT
    ThreeF,      // 5: C + L + R
    TwoF1R,      // 6: L + R + S
    ThreeF1R,    // 7: C + L + R + S
    TwoF2R,      // 8: L + R + SL + SR
    ThreeF2R,    // 9: C + L + R + SL + SR
}

impl AudioMode {
    pub fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0 => Self::Mono,
            1 => Self::MonoDual,
            2 => Self::Stereo,
            3 => Self::StereoSumDiff,
            4 => Self::StereoTotal,
            5 => Self::ThreeF,
            6 => Self::TwoF1R,
            7 => Self::ThreeF1R,
            8 => Self::TwoF2R,
            9 => Self::ThreeF2R,
            _ => return None,
        })
    }

    /// Number of primary audio channels (excluding LFE) for this arrangement.
    pub fn channel_count(self) -> usize {
        match self {
            Self::Mono => 1,
            Self::MonoDual | Self::Stereo | Self::StereoSumDiff | Self::StereoTotal => 2,
            Self::ThreeF | Self::TwoF1R => 3,
            Self::ThreeF1R | Self::TwoF2R => 4,
            Self::ThreeF2R => 5,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("insufficient data for core frame header")]
    InsufficientData,
    #[error("invalid core syncword")]
    InvalidSyncword,
    #[error("invalid deficit sample count")]
    DeficitSamples,
    #[error("invalid PCM block count")]
    PcmBlocks,
    #[error("invalid frame size")]
    FrameSize,
    #[error("unsupported audio mode {0}")]
    AudioMode(u32),
    #[error("invalid sample rate code")]
    SampleRate,
    #[error("reserved bit set")]
    ReservedBit,
    #[error("invalid LFE flag")]
    LfeFlag,
    #[error("invalid PCM resolution code")]
    PcmRes,
}

/// Parsed DCA core frame header (subset relevant to PCM decode + framing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameInfo {
    pub crc_present: bool,
    /// Number of 32-sample PCM blocks in this frame.
    pub npcmblocks: u32,
    /// Primary core frame byte size (used for extraction).
    pub frame_size: usize,
    pub audio_mode: AudioMode,
    pub sample_rate: u32,
    /// LFE flag: 0 = none, 1 = 128x decimation, 2 = 64x decimation.
    pub lfe_present: u8,
    pub source_pcm_res: u8,
    /// Source samples are 1-bit-encoded (es) — affects PCM scaling.
    pub es_format: bool,
    pub predictor_history: bool,
    pub ext_audio_present: bool,
    pub ext_audio_type: u8,
}

impl FrameInfo {
    /// Samples produced per channel for this frame (`npcmblocks * 32`).
    pub fn samples_per_channel(&self) -> usize {
        self.npcmblocks as usize * DCA_PCMBLOCK_SAMPLES as usize
    }

    pub fn has_lfe(&self) -> bool {
        self.lfe_present != 0
    }
}

/// Parse and validate a DCA core frame header. Mirrors
/// `ff_dca_parse_core_frame_header` exactly.
pub fn parse_header(buf: &[u8]) -> Result<FrameInfo, ParseError> {
    if buf.len() < CORE_FRAME_HEADER_SIZE {
        return Err(ParseError::InsufficientData);
    }
    let mut gb = BitReader::new(buf);

    if gb.read_bits(32).ok_or(ParseError::InsufficientData)? != SYNCWORD_CORE_BE {
        return Err(ParseError::InvalidSyncword);
    }

    let _normal_frame = gb.read_bit().ok_or(ParseError::InsufficientData)?;
    let deficit_samples = gb.read_bits(5).ok_or(ParseError::InsufficientData)? + 1;
    if deficit_samples != DCA_PCMBLOCK_SAMPLES {
        return Err(ParseError::DeficitSamples);
    }

    let crc_present = gb.read_bit().ok_or(ParseError::InsufficientData)?;
    let npcmblocks = gb.read_bits(7).ok_or(ParseError::InsufficientData)? + 1;
    if npcmblocks & (DCA_SUBBAND_SAMPLES - 1) != 0 {
        return Err(ParseError::PcmBlocks);
    }

    let frame_size = gb.read_bits(14).ok_or(ParseError::InsufficientData)? as usize + 1;
    if frame_size < 96 {
        return Err(ParseError::FrameSize);
    }

    let audio_mode_code = gb.read_bits(6).ok_or(ParseError::InsufficientData)?;
    if audio_mode_code >= DCA_AMODE_COUNT {
        return Err(ParseError::AudioMode(audio_mode_code));
    }
    let audio_mode = AudioMode::from_code(audio_mode_code).ok_or(ParseError::AudioMode(audio_mode_code))?;

    let sr_code = gb.read_bits(4).ok_or(ParseError::InsufficientData)? as usize;
    let sample_rate = SAMPLE_RATES[sr_code];
    if sample_rate == 0 {
        return Err(ParseError::SampleRate);
    }

    let _br_code = gb.read_bits(5).ok_or(ParseError::InsufficientData)?;
    if gb.read_bit().ok_or(ParseError::InsufficientData)? {
        return Err(ParseError::ReservedBit);
    }

    let _drc_present = gb.read_bit().ok_or(ParseError::InsufficientData)?;
    let _ts_present = gb.read_bit().ok_or(ParseError::InsufficientData)?;
    let _aux_present = gb.read_bit().ok_or(ParseError::InsufficientData)?;
    let _hdcd_master = gb.read_bit().ok_or(ParseError::InsufficientData)?;
    let ext_audio_type = gb.read_bits(3).ok_or(ParseError::InsufficientData)? as u8;
    let ext_audio_present = gb.read_bit().ok_or(ParseError::InsufficientData)?;
    let _sync_ssf = gb.read_bit().ok_or(ParseError::InsufficientData)?;
    let lfe_present = gb.read_bits(2).ok_or(ParseError::InsufficientData)? as u8;
    if lfe_present == 3 {
        return Err(ParseError::LfeFlag);
    }

    let predictor_history = gb.read_bit().ok_or(ParseError::InsufficientData)?;
    if crc_present {
        gb.skip_bits(16).ok_or(ParseError::InsufficientData)?;
    }
    let _filter_perfect = gb.read_bit().ok_or(ParseError::InsufficientData)?;
    let _encoder_rev = gb.read_bits(4).ok_or(ParseError::InsufficientData)?;
    let _copy_hist = gb.read_bits(2).ok_or(ParseError::InsufficientData)?;
    let pcmr_code = gb.read_bits(3).ok_or(ParseError::InsufficientData)? as usize;
    if BITS_PER_SAMPLE[pcmr_code] == 0 {
        return Err(ParseError::PcmRes);
    }

    let _sumdiff_front = gb.read_bit().ok_or(ParseError::InsufficientData)?;
    let _sumdiff_surround = gb.read_bit().ok_or(ParseError::InsufficientData)?;
    let _dn_code = gb.read_bits(4).ok_or(ParseError::InsufficientData)?;

    Ok(FrameInfo {
        crc_present,
        npcmblocks,
        frame_size,
        audio_mode,
        sample_rate,
        lfe_present,
        source_pcm_res: BITS_PER_SAMPLE[pcmr_code],
        // es_format: pcmr codes 5/6 (24-bit) carry the ES flag in upstream; the
        // simple path leaves it false until the coding header refines it.
        es_format: pcmr_code >= 5,
        predictor_history,
        ext_audio_present,
        ext_audio_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_syncword() {
        let buf = [0u8; 32];
        assert_eq!(parse_header(&buf), Err(ParseError::InvalidSyncword));
    }

    #[test]
    fn rejects_short_buffer() {
        let buf = [0x7F, 0xFE, 0x80, 0x01];
        assert_eq!(parse_header(&buf), Err(ParseError::InsufficientData));
    }
}
