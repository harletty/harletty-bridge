// SPDX-License-Identifier: Apache-2.0
//
// The stream layer of one block: the payload bits gathered into words, and
// the per-block description of what the carrier holds — which streams, how
// they were folded, and where their residual data starts.
//
// Every carrier channel describes itself. After the 48 header bits parsed by
// [`crate::block`], the payload opens with a 112-bit fixed header, the
// predictor seeds, the ADOL bytecode (also used for the layout), a small
// codebook of residual values, and finally the Golomb-Rice coded indices
// that pick from that codebook, one per sample (two in the three-stream
// fold).

use crate::block::{MAX_BLOCK, SYNC_SAMPLES, SyncHeader};

/// Payload words for the largest block at the widest borrow: `14 * 4096`
/// bits.
pub const PAYLOAD_WORDS: usize = (14 * MAX_BLOCK).div_ceil(32);

/// Largest codebook the count code can express (`8 * 0x53 - 64`), twice
/// for the three-stream fold.
pub const MAX_CODEBOOK: usize = 2 * (8 * 0x53 - 64);

/// Most streams one carrier folds.
pub const MAX_STREAMS: usize = 3;

/// The side-channel bits of one block, MSB first, packed in `u32` words.
pub struct Payload {
    pub words: [u32; PAYLOAD_WORDS],
    pub len_bits: usize,
}

impl Payload {
    pub const fn new() -> Self {
        Self {
            words: [0; PAYLOAD_WORDS],
            len_bits: 0,
        }
    }

    /// Gather the payload of `block` (all `header.block_size` samples). The
    /// header samples lend bits `3..m`; every later sample lends its low
    /// `m` bits except bit 0 of every sixteenth sample, which is reserved.
    pub fn gather(&mut self, block: &[i32], header: SyncHeader) {
        let m = u32::from(header.lsb_bits);
        let mut n = 0usize;
        let mut word = 0u32;
        let mut fill = 0u32;
        let mut push = |bit: u32| {
            word = (word << 1) | bit;
            fill += 1;
            if fill == 32 {
                self.words[n / 32] = word;
                word = 0;
                fill = 0;
            }
            n += 1;
        };
        for (i, &s) in block.iter().enumerate() {
            let low = if i < SYNC_SAMPLES {
                3
            } else if i % 16 == 0 {
                1
            } else {
                0
            };
            let v = s as u32;
            let mut b = m;
            while b > low {
                b -= 1;
                push((v >> b) & 1);
            }
        }
        if fill != 0 {
            self.words[n / 32] = word << (32 - fill);
        }
        self.len_bits = n;
    }

    /// Bit at `pos`, or 0 past the end.
    #[inline]
    pub fn bit(&self, pos: usize) -> u32 {
        if pos >= self.len_bits {
            return 0;
        }
        (self.words[pos / 32] >> (31 - (pos % 32))) & 1
    }
}

impl Default for Payload {
    fn default() -> Self {
        Self::new()
    }
}

/// Sequential MSB-first reader over a [`Payload`].
pub struct Cursor<'a> {
    pub payload: &'a Payload,
    pub pos: usize,
}

impl Cursor<'_> {
    #[inline]
    pub fn bit(&mut self) -> u32 {
        let b = self.payload.bit(self.pos);
        self.pos += 1;
        b
    }

    pub fn read(&mut self, bits: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..bits {
            v = (v << 1) | self.bit();
        }
        v
    }
}

/// Why a block's stream layer could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamError {
    /// The fixed header's first field is outside the accepted range.
    Header,
    /// The codebook count code has no defined size.
    CountCode,
    /// An ADOL block did not start with tag 1 or 2.
    AdolTag,
    /// An ADOL opcode outside the known set.
    AdolOpcode(u8),
    /// The bytecode did not terminate within the instruction budget.
    AdolRunaway,
    /// The codebook does not fit in the payload.
    CodebookOverrun,
    /// A gain index (code + offset) is outside the gain table.
    Gain,
}

/// What one carrier holds in one block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamBlock {
    /// Streams folded into this carrier, in fold order; `mode` of them are
    /// valid.
    pub ids: [u8; MAX_STREAMS],
    /// 1, 2 or 3 streams (0 = nothing announced).
    pub mode: u8,
    /// Predictor seeds: two for the two-stream fold, five for three.
    pub seeds: [i32; 5],
    /// Gain codes per stream (ADOL `0x40`), in tenths of a dB.
    pub gain_codes: [u8; MAX_STREAMS],
    /// Gain offset added to every code (ADOL `0x41`).
    pub gain_offset: u8,
    /// Channel-input configuration announced by ADOL `0x1E`, if any.
    pub config: Option<u8>,
    /// Rice parameter, or the initial one when `adaptive`.
    pub k: u8,
    /// Re-read the Rice parameter from the stream every 32 samples.
    pub adaptive: bool,
    /// Codebook entries per stream and bits per entry.
    pub count: u16,
    pub width: u8,
    /// Payload bit position of the codebook and of the Golomb-Rice stream.
    pub codebook_pos: usize,
    pub rice_pos: usize,
}

/// Codebook entries from the count code: three ranges of linear steps.
pub fn codebook_count(code: u32) -> Option<u16> {
    Some(match code {
        0..=4 => 2 * code + 8,
        5..=15 => 4 * code,
        16..=0x53 => 8 * code - 64,
        _ => return None,
    } as u16)
}

/// Most ADOL instructions the stream layer follows before giving up.
const MAX_INSTRUCTIONS: usize = 256;

/// Parse the stream layer from a gathered payload.
pub fn parse_stream(payload: &Payload) -> Result<StreamBlock, StreamError> {
    let mut c = Cursor { payload, pos: 0 };
    let hdr = c.read(16);
    if !(hdr < 0x200 && (hdr < 0x100 || (hdr & 0xff) < 0xb)) {
        return Err(StreamError::Header);
    }
    let w = c.read(32);
    let ids_word = c.read(32);
    let _ids2 = c.read(32);
    let mut ids = [0xffu8; MAX_STREAMS];
    let mut mode = 0u8;
    for (j, slot) in ids.iter_mut().enumerate() {
        *slot = ((ids_word >> (24 - 8 * j)) & 0xff) as u8;
        if *slot != 0xff {
            mode += 1;
        }
    }
    // A missing slot ahead of a present one would leave a hole in the fold.
    let mode = if ids[..usize::from(mode)].iter().any(|&v| v == 0xff) {
        0
    } else {
        mode
    };
    let nseeds = match mode {
        3 => 5,
        2 => 2,
        _ => 0,
    };
    let mut seeds = [0i32; 5];
    for seed in seeds.iter_mut().take(nseeds) {
        *seed = c.read(32) as i32;
    }
    let adol_blocks = (w >> 8) & 0xf;
    let width = (w & 0xff) as u8;
    let count = codebook_count((w >> 16) & 0xff).ok_or(StreamError::CountCode)?;
    let k = ((w >> 24) & 0xf) as u8;
    let adaptive = w & 0x4000_0000 != 0;

    let mut gain_codes = [0u8; MAX_STREAMS];
    let mut gain_offset = 0u8;
    let mut config = None;
    let mut budget = MAX_INSTRUCTIONS;
    for _ in 0..adol_blocks {
        match c.read(8) {
            1 => loop {
                if budget == 0 {
                    return Err(StreamError::AdolRunaway);
                }
                budget -= 1;
                let op = c.read(8) as u8;
                match op {
                    0x00 => break,
                    0x01 | 0x03 | 0x5A..=0x62 => {
                        c.read(16);
                    }
                    0x40 => {
                        let ch = c.read(8) as u8;
                        let scaler = c.read(8) as u8;
                        for j in 0..usize::from(mode) {
                            if ids[j] == ch {
                                gain_codes[j] = scaler;
                            }
                        }
                    }
                    0x41 => gain_offset = c.read(8) as u8,
                    0x1E => config = Some(c.read(8) as u8),
                    0x02 | 0x04 | 0x1F | 0x47 | 0x50..=0x58 => {
                        c.read(8);
                    }
                    0x0E | 0x46 | 0x6E..=0x76 | 0x80..=0x85 | 0x8C..=0x90 => {
                        c.read(32);
                    }
                    0x64..=0x6C => {
                        c.read(24);
                    }
                    other => return Err(StreamError::AdolOpcode(other)),
                }
            },
            2 => {
                let n = (c.read(24) >> 16) & 0xff;
                for _ in 0..n {
                    c.read(32);
                }
            }
            _ => return Err(StreamError::AdolTag),
        }
    }
    let codebook_pos = c.pos;
    let streams = if mode == 3 { 2 } else { 1 };
    let codebook_bits = usize::from(count) * usize::from(width) * streams;
    if codebook_pos + codebook_bits > payload.len_bits {
        return Err(StreamError::CodebookOverrun);
    }
    Ok(StreamBlock {
        ids,
        mode,
        seeds,
        gain_codes,
        gain_offset,
        config,
        k,
        adaptive,
        count,
        width,
        codebook_pos,
        rice_pos: codebook_pos + codebook_bits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codebook_sizes_follow_the_three_ranges() {
        assert_eq!(codebook_count(0), Some(8));
        assert_eq!(codebook_count(4), Some(16));
        assert_eq!(codebook_count(5), Some(20));
        assert_eq!(codebook_count(15), Some(60));
        assert_eq!(codebook_count(16), Some(64));
        assert_eq!(codebook_count(0x53), Some(600));
        assert_eq!(codebook_count(0x54), None);
    }

    #[test]
    fn gather_takes_bits_three_up_in_the_header_then_m_per_sample() {
        // m = 4, block of 20 samples: 16 header samples give bit 3 each,
        // sample 16 gives bits 3..1 (bit 0 reserved), 17..20 give 4 bits.
        let mut block = [0i32; 20];
        block[0] = 0b1000; // header: bit 3 set -> first payload bit 1
        block[16] = 0b0111; // bits 3,2,1 -> 0,1,1 (bit 0 ignored)
        block[17] = 0b1001;
        let header = SyncHeader {
            block_size: 20,
            lsb_bits: 4,
        };
        let mut p = Payload::new();
        p.gather(&block, header);
        assert_eq!(p.len_bits, 16 + 3 + 3 * 4);
        let bits: Vec<u32> = (0..p.len_bits).map(|i| p.bit(i)).collect();
        assert_eq!(
            &bits[..16],
            &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(&bits[16..19], &[0, 1, 1]);
        assert_eq!(&bits[19..23], &[1, 0, 0, 1]);
        assert_eq!(p.bit(1000), 0);
    }
}
