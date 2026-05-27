pub const SYNCWORD: u16 = 0x0B77;

const MIN_HEADER_BYTES: usize = 7;
const SAMPLE_RATES: [u32; 3] = [48_000, 44_100, 32_000];
const HALF_SAMPLE_RATES: [u32; 3] = [24_000, 22_050, 16_000];
const BLOCKS_PER_SYNCFRAME: [u8; 4] = [1, 2, 3, 6];

/// Legacy AC-3 (bsid ≤ 10) frame size in 16-bit words, indexed by
/// `frmsizecod >> 1` then `fscod`. Per ATSC A/52 §5.4.1.4 Table 5.18.
const LEGACY_AC3_FRAME_SIZE_WORDS: [[usize; 3]; 19] = [
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    InsufficientData,
    InvalidSyncword,
    ReservedSampleRateCode,
    UnsupportedBitstreamId(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamType {
    Independent,
    Dependent,
    Ac3Converted,
    Reserved,
}

impl StreamType {
    #[inline]
    fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::Independent,
            1 => Self::Dependent,
            2 => Self::Ac3Converted,
            _ => Self::Reserved,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleRateCode {
    Full(u8),
    Half(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelMode {
    DualMono,
    Mono,
    Stereo,
    ThreeFront,
    TwoFrontOneRear,
    ThreeFrontOneRear,
    TwoFrontTwoRear,
    ThreeFrontTwoRear,
}

impl ChannelMode {
    #[inline]
    fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::DualMono,
            1 => Self::Mono,
            2 => Self::Stereo,
            3 => Self::ThreeFront,
            4 => Self::TwoFrontOneRear,
            5 => Self::ThreeFrontOneRear,
            6 => Self::TwoFrontTwoRear,
            _ => Self::ThreeFrontTwoRear,
        }
    }

    #[inline]
    pub fn base_channels(self) -> u8 {
        match self {
            Self::DualMono | Self::Stereo => 2,
            Self::Mono => 1,
            Self::ThreeFront | Self::TwoFrontOneRear => 3,
            Self::ThreeFrontOneRear | Self::TwoFrontTwoRear => 4,
            Self::ThreeFrontTwoRear => 5,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameInfo {
    pub stream_type: StreamType,
    pub substream_id: u8,
    pub frame_size: usize,
    pub sample_rate_code: SampleRateCode,
    pub sample_rate: u32,
    pub num_blocks: u8,
    pub samples: u16,
    pub channel_mode: ChannelMode,
    pub lfe: bool,
    pub bitstream_id: u8,
}

impl FrameInfo {
    #[inline]
    pub fn channels(self) -> u8 {
        self.channel_mode.base_channels() + u8::from(self.lfe)
    }

    #[inline]
    pub fn bitrate(self) -> u32 {
        ((self.frame_size as u64 * 8 * self.sample_rate as u64) / self.samples as u64) as u32
    }
}

/// Parse a legacy AC-3 syncframe header (bsid ≤ 10).
///
/// AC-3 and E-AC-3 share the 0x0B77 syncword and place `bsid` at the same
/// bit position (40-44), but disagree on the fields before bit 40 — AC-3
/// carries `crc1` + `fscod` + `frmsizecod` where E-AC-3 carries
/// `strmtyp` + `substreamid` + `frmsiz`. This entry point exists so the
/// raw transport extractor (which sees a mixed AC-3 / E-AC-3 stream from
/// dual-stream MKV tracks) can frame both. Channel-shape fields are filled
/// with placeholders; downstream consumers re-parse the full AC-3 header
/// (see `is_legacy_ac3_frame` in the bridge) so this only has to be
/// correct for the framing layer.
pub fn parse_legacy_ac3_header(data: &[u8]) -> Result<FrameInfo, ParseError> {
    if data.len() < MIN_HEADER_BYTES {
        return Err(ParseError::InsufficientData);
    }
    if data[0] != ((SYNCWORD >> 8) as u8) || data[1] != (SYNCWORD as u8) {
        return Err(ParseError::InvalidSyncword);
    }
    let bitstream_id = (data[5] >> 3) & 0x1F;
    if bitstream_id > 10 {
        return Err(ParseError::UnsupportedBitstreamId(bitstream_id));
    }
    let fscod = data[4] >> 6;
    let frmsizecod = data[4] & 0x3F;
    let bitrate_index = usize::from(frmsizecod >> 1);
    let sample_rate = SAMPLE_RATES
        .get(usize::from(fscod))
        .copied()
        .ok_or(ParseError::ReservedSampleRateCode)?;
    let frame_size = LEGACY_AC3_FRAME_SIZE_WORDS
        .get(bitrate_index)
        .and_then(|row| row.get(usize::from(fscod)).copied())
        .ok_or(ParseError::ReservedSampleRateCode)?
        * 2;
    Ok(FrameInfo {
        stream_type: StreamType::Independent,
        substream_id: 0,
        frame_size,
        sample_rate_code: SampleRateCode::Full(fscod),
        sample_rate,
        num_blocks: 6,
        samples: 1536,
        // Placeholders — downstream uses is_legacy_ac3_frame to re-parse.
        channel_mode: ChannelMode::Mono,
        lfe: false,
        bitstream_id,
    })
}

pub fn parse_header(data: &[u8]) -> Result<FrameInfo, ParseError> {
    if data.len() < MIN_HEADER_BYTES {
        return Err(ParseError::InsufficientData);
    }

    let mut reader = BitReader::new(data);
    let syncword = reader.read_u16(16).ok_or(ParseError::InsufficientData)?;
    if syncword != SYNCWORD {
        return Err(ParseError::InvalidSyncword);
    }

    let stream_type = StreamType::from_bits(reader.read_u8(2).ok_or(ParseError::InsufficientData)?);
    let substream_id = reader.read_u8(3).ok_or(ParseError::InsufficientData)?;
    let frame_size =
        (usize::from(reader.read_u16(11).ok_or(ParseError::InsufficientData)?) + 1) * 2;
    let fscod = reader.read_u8(2).ok_or(ParseError::InsufficientData)?;

    let (sample_rate_code, sample_rate, num_blocks) = if fscod == 3 {
        let fscod2 = reader.read_u8(2).ok_or(ParseError::InsufficientData)?;
        let sample_rate = HALF_SAMPLE_RATES
            .get(usize::from(fscod2))
            .copied()
            .ok_or(ParseError::ReservedSampleRateCode)?;
        (SampleRateCode::Half(fscod2), sample_rate, 6)
    } else {
        let numblkscod = reader.read_u8(2).ok_or(ParseError::InsufficientData)?;
        (
            SampleRateCode::Full(fscod),
            SAMPLE_RATES[usize::from(fscod)],
            BLOCKS_PER_SYNCFRAME[usize::from(numblkscod)],
        )
    };

    let channel_mode =
        ChannelMode::from_bits(reader.read_u8(3).ok_or(ParseError::InsufficientData)?);
    let lfe = reader.read_bool().ok_or(ParseError::InsufficientData)?;
    let bitstream_id = reader.read_u8(5).ok_or(ParseError::InsufficientData)?;
    if !(11..=16).contains(&bitstream_id) {
        return Err(ParseError::UnsupportedBitstreamId(bitstream_id));
    }

    Ok(FrameInfo {
        stream_type,
        substream_id,
        frame_size,
        sample_rate_code,
        sample_rate,
        num_blocks,
        samples: u16::from(num_blocks) * 256,
        channel_mode,
        lfe,
        bitstream_id,
    })
}

struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    #[inline]
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    #[inline]
    fn read_bool(&mut self) -> Option<bool> {
        Some(self.read_u8(1)? != 0)
    }

    #[inline]
    fn read_u8(&mut self, bits: u8) -> Option<u8> {
        debug_assert!(bits <= 8);
        self.read_bits(bits).map(|value| value as u8)
    }

    #[inline]
    fn read_u16(&mut self, bits: u8) -> Option<u16> {
        debug_assert!(bits <= 16);
        self.read_bits(bits).map(|value| value as u16)
    }

    fn read_bits(&mut self, bits: u8) -> Option<u32> {
        if bits == 0 {
            return Some(0);
        }

        let next_bit_pos = self.bit_pos.checked_add(usize::from(bits))?;
        if next_bit_pos > self.data.len() * 8 {
            return None;
        }

        let mut value = 0u32;
        for _ in 0..bits {
            let byte = self.data[self.bit_pos / 8];
            let shift = 7 - (self.bit_pos % 8);
            value = (value << 1) | u32::from((byte >> shift) & 1);
            self.bit_pos += 1;
        }
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: [u8; 7] = [0x0B, 0x77, 0x00, 0x7F, 0x05, 0x80, 0x00];

    #[test]
    fn parses_syncframe_header() {
        let info = parse_header(&HEADER).unwrap();

        assert_eq!(info.stream_type, StreamType::Independent);
        assert_eq!(info.substream_id, 0);
        assert_eq!(info.frame_size, 256);
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.num_blocks, 1);
        assert_eq!(info.samples, 256);
        assert_eq!(info.channel_mode, ChannelMode::Stereo);
        assert!(info.lfe);
        assert_eq!(info.channels(), 3);
        assert_eq!(info.bitstream_id, 16);
        assert_eq!(info.bitrate(), 384_000);
    }

    #[test]
    fn rejects_non_eac3_bsid() {
        let mut header = HEADER;
        header[5] = 0x40;

        assert_eq!(
            parse_header(&header),
            Err(ParseError::UnsupportedBitstreamId(8))
        );
    }

    #[test]
    fn parses_half_sample_rate_header() {
        let header = [0x0B, 0x77, 0x00, 0x7F, 0xE5, 0x80, 0x00];
        let info = parse_header(&header).unwrap();

        assert_eq!(info.sample_rate_code, SampleRateCode::Half(2));
        assert_eq!(info.sample_rate, 16_000);
        assert_eq!(info.num_blocks, 6);
        assert_eq!(info.samples, 1536);
    }
}
