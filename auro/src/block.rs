// SPDX-License-Identifier: Apache-2.0
//
// One Auro-Codec block: the sync signature, the CRC that guards it, and the
// bit-multiplexed side channel that carries the ADOL configuration.
//
// Everything here works on 24-bit samples held in `i32` (sign-extended or
// not: only the low 24 bits are ever read). A carrier channel is a plain PCM
// channel whose `m` least-significant bits have been replaced, block by
// block, with a serial bitstream. The first sixteen samples of a block are
// its header:
//
// ```text
//   bit 0 of samples 0..16   sync: all ones
//   bit 1 of samples 0..16   CRC-16/CCITT of the block, MSB first
//   bit 2 of samples 0..8    block-size code, MSB first
//   bit 2 of samples 8..12   four flag bits
//   bit 2 of samples 12..16  14 - m, the count of LSBs this block borrows
// ```
//
// The remaining bits of the header samples (bits 3..m) and the low `m` bits
// of every later sample, MSB first, form the payload. One bit every `16 * m`
// payload positions is reserved and must read zero; for `m = 3` that is
// bit 0 of every sixteenth sample, which is what stops a sync run at exactly
// sixteen ones.
//
// The bit layout follows the public description of the format
// (MediaInfoLib PR #2531 and almirus/Orua-D3, both derived from the
// commercial decoder). Nothing in this file interprets audio.

use crate::layout::ChannelConfig;

/// Samples in a block header, all of which carry the sync signature.
pub const SYNC_SAMPLES: usize = 16;
/// Largest block the size code can express (`0x7F << 5 | 0x10`, plus 16).
pub const MAX_BLOCK: usize = 4096;
/// Smallest `m` the header can announce.
pub const MIN_LSB_BITS: u8 = 3;
/// Largest `m` the header can announce (`14 - 0`).
pub const MAX_LSB_BITS: u8 = 14;

/// The block-size code that means 1000 samples: the only size that is not a
/// multiple of sixteen, so it gets a code of its own.
const SIZE_CODE_1000: u32 = 0x3D0;

/// What the sixteen header samples announce about their block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncHeader {
    /// Samples in the block, header included. At most [`MAX_BLOCK`].
    pub block_size: u16,
    /// Least-significant bits borrowed from every sample of this block.
    pub lsb_bits: u8,
}

#[inline]
fn low24(sample: i32) -> u32 {
    (sample as u32) & 0x00FF_FFFF
}

#[inline]
fn bit(sample: i32, index: u32) -> u32 {
    (low24(sample) >> index) & 1
}

/// Read the sync signature and the size and width codes off the first
/// sixteen samples. `None` when the samples are not a block header.
///
/// This is the cheap test that runs on every sixteen-ones run; the CRC and
/// the reserved-bit check need the whole block and come later.
pub fn parse_sync(samples: &[i32]) -> Option<SyncHeader> {
    if samples.len() < SYNC_SAMPLES {
        return None;
    }
    if samples[..SYNC_SAMPLES].iter().any(|&s| bit(s, 0) == 0) {
        return None;
    }
    let mut size_code = 0u32;
    for &s in &samples[..8] {
        size_code = (size_code << 1) | bit(s, 2);
    }
    // Seven bits of block count in units of 32, one bit in units of 16.
    let raw = size_code << 4;
    let block_size = if raw == SIZE_CODE_1000 {
        1000
    } else {
        raw + SYNC_SAMPLES as u32
    };
    let mut width_code = 0u32;
    for &s in &samples[12..16] {
        width_code = (width_code << 1) | bit(s, 2);
    }
    let lsb_bits = 14u32.checked_sub(width_code)?;
    if lsb_bits < u32::from(MIN_LSB_BITS) {
        return None;
    }
    Some(SyncHeader {
        block_size: block_size as u16,
        lsb_bits: lsb_bits as u8,
    })
}

/// Where payload position `pos` lives for a block borrowing `m` bits:
/// `(sample index, bit index)`.
///
/// `skip_reserved` selects the payload view, in which the reserved bit every
/// `16 * m` positions is stepped over, from the raw view used to locate
/// those reserved bits themselves.
#[inline]
pub fn bit_location(pos: usize, m: usize, skip_reserved: bool) -> (usize, u32) {
    if pos < 48 {
        // The three header bit planes: sync, CRC, codes.
        return (pos & 15, (pos >> 4) as u32);
    }
    let header_bits = SYNC_SAMPLES * m;
    if pos < header_bits {
        // Bits 3..m of the header samples, MSB first.
        let per_sample = m - 3;
        let rel = pos - 48;
        let idx = rel / per_sample;
        let rem = rel % per_sample;
        return (idx, (m - 1 - rem) as u32);
    }
    let pos = if skip_reserved {
        pos + (pos - m) / (header_bits - 1)
    } else {
        pos
    };
    (pos / m, (m - 1 - pos % m) as u32)
}

/// The reserved-bit check: one raw position every `16 * m`, starting at
/// `17 * m - 1`, must read zero over the whole block.
pub fn reserved_bits_clear(samples: &[i32], header: SyncHeader) -> bool {
    let m = usize::from(header.lsb_bits);
    let block = usize::from(header.block_size);
    if samples.len() < block {
        return false;
    }
    let total_bits = m * block;
    let mut pos = 17 * m - 1;
    while pos < total_bits {
        let (idx, b) = bit_location(pos, m, false);
        if idx >= block || bit(samples[idx], b) != 0 {
            return false;
        }
        pos += SYNC_SAMPLES * m;
    }
    true
}

#[inline]
fn crc16_ccitt_update(crc: u16, byte: u8) -> u16 {
    let mut c = crc ^ (u16::from(byte) << 8);
    for _ in 0..8 {
        c = if c & 0x8000 != 0 {
            (c << 1) ^ 0x1021
        } else {
            c << 1
        };
    }
    c
}

/// CRC-16/CCITT over the three bytes of every sample of the block, low byte
/// first, with the CRC's own bit plane (bit 1 of the header samples) masked
/// out, inverted at the end.
pub fn block_crc(samples: &[i32]) -> u16 {
    let mut crc = 0u16;
    for (i, &s) in samples.iter().enumerate() {
        let w = low24(s);
        let mut b0 = (w & 0xFF) as u8;
        if i < SYNC_SAMPLES {
            b0 &= 0xFD;
        }
        crc = crc16_ccitt_update(crc, b0);
        crc = crc16_ccitt_update(crc, ((w >> 8) & 0xFF) as u8);
        crc = crc16_ccitt_update(crc, ((w >> 16) & 0xFF) as u8);
    }
    !crc
}

/// The CRC the header carries: bit 1 of samples 0..16, MSB first.
pub fn stored_crc(samples: &[i32]) -> u16 {
    samples[..SYNC_SAMPLES]
        .iter()
        .fold(0u16, |acc, &s| (acc << 1) | bit(s, 1) as u16)
}

/// Why a block that looked like one could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// Fewer samples than the header announces.
    Truncated,
    /// A reserved bit was set.
    ReservedBit,
    /// The CRC does not match.
    Crc,
    /// The payload ended inside a field.
    Underrun,
    /// The stream-id count has no defined table size.
    StreamCount,
    /// An ADOL block did not start with tag 1.
    AdolTag,
    /// An ADOL opcode outside the known set.
    AdolOpcode(u8),
}

/// Sequential reader over the payload view of one block.
struct BitReader<'a> {
    samples: &'a [i32],
    m: usize,
    pos: usize,
}

impl BitReader<'_> {
    fn read(&mut self, bits: u32) -> Result<u32, BlockError> {
        let mut v = 0u32;
        for _ in 0..bits {
            let (idx, b) = bit_location(self.pos, self.m, true);
            if idx >= self.samples.len() {
                return Err(BlockError::Underrun);
            }
            v = (v << 1) | bit(self.samples[idx], b);
            self.pos += 1;
        }
        Ok(v)
    }
}

/// One ADOL instruction: an opcode and up to two operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdolInstruction {
    pub opcode: u8,
    pub operand: u32,
    /// Second operand of the two-operand opcode `0x40`; zero otherwise.
    pub operand2: u32,
}

/// Most ADOL instructions one block is allowed to carry before parsing
/// stops. A layout announcement is a handful; this bounds a hostile stream.
pub const MAX_ADOL_INSTRUCTIONS: usize = 64;

/// What one validated block says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockInfo {
    pub header: SyncHeader,
    /// The four stream-id slots of this carrier; `0xFF` marks an empty slot.
    pub stream_ids: [u8; 4],
    /// The channel-input configuration announced by ADOL `0x1E`, when the
    /// block carries one.
    pub config: Option<ChannelConfig>,
    /// Instructions parsed, including the terminators.
    pub adol_instructions: usize,
}

/// Validate a whole block (reserved bits, CRC) and read its configuration.
///
/// `samples` must hold at least `header.block_size` samples starting at the
/// sync; anything beyond is ignored.
pub fn parse_block(samples: &[i32], header: SyncHeader) -> Result<BlockInfo, BlockError> {
    let block = usize::from(header.block_size);
    if samples.len() < block {
        return Err(BlockError::Truncated);
    }
    let samples = &samples[..block];
    if !reserved_bits_clear(samples, header) {
        return Err(BlockError::ReservedBit);
    }
    if block_crc(samples) != stored_crc(samples) {
        return Err(BlockError::Crc);
    }

    let mut r = BitReader {
        samples,
        m: usize::from(header.lsb_bits),
        // Positions 32..48 are the size, flag and width codes parse_sync read.
        pos: 48,
    };
    // Fixed fields ahead of the ADOL bytecode. Their meaning is not public;
    // they are stepped over at the widths the reference parser uses.
    r.read(8)?;
    r.read(8)?;
    r.read(1)?;
    r.read(1)?;
    r.read(2)?;
    r.read(4)?;
    r.read(8)?;
    let adol_blocks = r.read(8)?;
    r.read(8)?;
    let mut stream_ids = [0xFFu8; 4];
    let mut streams = 0usize;
    for slot in &mut stream_ids {
        *slot = r.read(8)? as u8;
        if *slot != 0xFF {
            streams += 1;
        }
    }
    for _ in 0..4 {
        r.read(8)?;
    }
    let table_words = match streams {
        0 | 1 => 0,
        2 => 2,
        3 => 5,
        _ => return Err(BlockError::StreamCount),
    };
    for _ in 0..table_words {
        r.read(32)?;
    }

    let mut config = None;
    let mut adol_instructions = 0usize;
    for _ in 0..adol_blocks {
        if r.read(8)? != 1 {
            return Err(BlockError::AdolTag);
        }
        loop {
            if adol_instructions >= MAX_ADOL_INSTRUCTIONS {
                return Err(BlockError::Underrun);
            }
            let ins = read_instruction(&mut r)?;
            adol_instructions += 1;
            if ins.opcode == 0x1E {
                config = Some(ChannelConfig(ins.operand as u8));
            }
            if ins.opcode == 0 {
                break;
            }
        }
    }

    Ok(BlockInfo {
        header,
        stream_ids,
        config,
        adol_instructions,
    })
}

fn read_instruction(r: &mut BitReader<'_>) -> Result<AdolInstruction, BlockError> {
    let opcode = r.read(8)? as u8;
    let mut ins = AdolInstruction {
        opcode,
        operand: 0,
        operand2: 0,
    };
    match opcode {
        0x00 => {}
        0x01 | 0x03 | 0x5A..=0x62 => ins.operand = r.read(16)?,
        0x02 | 0x04 | 0x1E | 0x1F | 0x41 | 0x47 | 0x50..=0x58 => ins.operand = r.read(8)?,
        0x0E | 0x46 | 0x6E..=0x76 | 0x80..=0x85 | 0x8C..=0x90 => ins.operand = r.read(32)?,
        0x40 => {
            ins.operand = r.read(8)?;
            ins.operand2 = r.read(8)?;
        }
        0x64..=0x6C => ins.operand = r.read(24)?,
        other => return Err(BlockError::AdolOpcode(other)),
    }
    Ok(ins)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_bit_planes_come_first() {
        for m in [3usize, 5, 10] {
            for pos in 0..48 {
                assert_eq!(bit_location(pos, m, true), (pos % 16, (pos / 16) as u32));
            }
        }
    }

    #[test]
    fn header_samples_lend_their_remaining_bits_before_the_body() {
        // m = 5: header samples carry bits 3 and 4 as payload positions 48..80.
        assert_eq!(bit_location(48, 5, true), (0, 4));
        assert_eq!(bit_location(49, 5, true), (0, 3));
        assert_eq!(bit_location(50, 5, true), (1, 4));
        assert_eq!(bit_location(79, 5, true), (15, 3));
        assert_eq!(bit_location(80, 5, true), (16, 4));
        // m = 3: nothing left in the header, the body starts at sample 16.
        assert_eq!(bit_location(48, 3, true), (16, 2));
        assert_eq!(bit_location(50, 3, false), (16, 0));
    }

    #[test]
    fn the_payload_view_steps_over_the_reserved_bit() {
        // m = 3: raw position 50 (sample 16, bit 0) is reserved; payload
        // position 50 is the next raw one.
        assert_eq!(bit_location(50, 3, true), (17, 2));
        // and again every 48 positions: raw 98 is sample 32, bit 0.
        assert_eq!(bit_location(98, 3, false), (32, 0));
        assert_eq!(bit_location(96, 3, true), (32, 1));
        assert_eq!(bit_location(97, 3, true), (33, 2));
    }

    #[test]
    fn a_sync_needs_sixteen_ones() {
        let mut s = [1i32; 16];
        assert!(parse_sync(&s).is_some());
        s[7] = 0;
        assert!(parse_sync(&s).is_none());
        assert!(parse_sync(&s[..15]).is_none());
    }

    #[test]
    fn crc_reads_bit_one_of_the_header() {
        let mut s = [1i32; 16];
        // 0xA5C3, MSB first.
        for (i, sample) in s.iter_mut().enumerate() {
            let b = (0xA5C3u16 >> (15 - i)) & 1;
            *sample |= i32::from(b) << 1;
        }
        assert_eq!(stored_crc(&s), 0xA5C3);
    }
}
