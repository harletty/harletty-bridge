// SPDX-License-Identifier: Apache-2.0
//
// Residuals: a Golomb-Rice coded index per sample selects an entry of the
// block's codebook. Two streams folded into one carrier share one index
// that selects from two codebooks laid end to end.

use crate::stream::{Cursor, MAX_CODEBOOK, Payload, StreamBlock};

/// Why residuals could not be decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RiceError {
    /// An index reached past the codebook.
    Index,
    /// The unary prefix ran past any sane length: the stream is not a Rice
    /// stream.
    Prefix,
}

/// Unary prefixes longer than this are noise, not data.
const MAX_PREFIX: u32 = 64;

/// The decoded codebook of one block: `count` entries per stream, sign and
/// magnitude packed in `width` bits.
pub struct Codebook {
    pub entries: [i32; MAX_CODEBOOK],
    pub count: usize,
    pub streams: usize,
}

impl Codebook {
    pub const fn new() -> Self {
        Self {
            entries: [0; MAX_CODEBOOK],
            count: 0,
            streams: 0,
        }
    }

    pub fn load(&mut self, payload: &Payload, block: &StreamBlock) {
        let width = u32::from(block.width);
        self.count = usize::from(block.count);
        self.streams = if block.mode == 3 { 2 } else { 1 };
        let mut c = Cursor {
            payload,
            pos: block.codebook_pos,
        };
        let top = if width == 0 || width >= 32 {
            0
        } else {
            1u32 << (width - 1)
        };
        for e in self.entries.iter_mut().take(self.count * self.streams) {
            let v = c.read(width);
            *e = if width < 32 {
                let mag = (v & top.wrapping_sub(1)) as i32;
                if v & top != 0 { -mag } else { mag }
            } else {
                v as i32
            };
        }
    }
}

impl Default for Codebook {
    fn default() -> Self {
        Self::new()
    }
}

/// Decode `n` residual entries into `out` (one value per entry, or two
/// interleaved for a three-stream fold). Returns the payload position
/// reached.
pub fn decode_residuals(
    payload: &Payload,
    block: &StreamBlock,
    codebook: &Codebook,
    n: usize,
    out: &mut [i32],
) -> Result<usize, RiceError> {
    let mut c = Cursor {
        payload,
        pos: block.rice_pos,
    };
    let mut k = u32::from(block.k);
    let mut counter = 0u32;
    let two = codebook.streams == 2;
    for i in 0..n {
        if block.adaptive && counter == 0 {
            k = c.read(3);
        }
        counter = if counter + 1 < 32 { counter + 1 } else { 0 };
        let mut q = 0u32;
        while c.bit() == 1 {
            q += 1;
            if q > MAX_PREFIX {
                return Err(RiceError::Prefix);
            }
        }
        // The suffix is sent least-significant bit first.
        let mut val = 0u32;
        for b in 0..k {
            if c.bit() == 1 {
                val |= 1 << b;
            }
        }
        let index = ((q << k) | val) as usize;
        if index >= codebook.count {
            return Err(RiceError::Index);
        }
        if two {
            out[2 * i] = codebook.entries[index];
            out[2 * i + 1] = codebook.entries[codebook.count + index];
        } else {
            out[i] = codebook.entries[index];
        }
    }
    Ok(c.pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload_from_bits(bits: &[u32]) -> Payload {
        let mut p = Payload::new();
        for (i, &b) in bits.iter().enumerate() {
            p.words[i / 32] |= b << (31 - i % 32);
        }
        p.len_bits = bits.len();
        p
    }

    #[test]
    fn rice_index_is_unary_prefix_then_lsb_first_suffix() {
        // k = 2, codebook of 8 entries (width 4, sign-magnitude): entry i = i,
        // entry 5 = -1 (0b1001).
        let mut bits = Vec::new();
        for e in [0u32, 1, 2, 3, 4, 0b1001, 6, 7] {
            for b in (0..4).rev() {
                bits.push((e >> b) & 1);
            }
        }
        let codebook_pos = 0;
        let rice_pos = bits.len();
        // index 5 = q 1, val 1: prefix "10", suffix lsb-first "10"
        bits.extend([1, 0, 1, 0]);
        // index 2 = q 0, val 2: "0", suffix "01"
        bits.extend([0, 0, 1]);
        // index 7 = q 1, val 3: "10", "11"
        bits.extend([1, 0, 1, 1]);
        let p = payload_from_bits(&bits);
        let block = StreamBlock {
            ids: [0, 9, 0xff],
            mode: 2,
            seeds: [0; 5],
            gain_codes: [0; 3],
            gain_offset: 0,
            config: None,
            k: 2,
            adaptive: false,
            count: 8,
            width: 4,
            codebook_pos,
            rice_pos,
        };
        let mut cb = Codebook::new();
        cb.load(&p, &block);
        assert_eq!(&cb.entries[..8], &[0, 1, 2, 3, 4, -1, 6, 7]);
        let mut out = [0i32; 3];
        let end = decode_residuals(&p, &block, &cb, 3, &mut out).unwrap();
        assert_eq!(out, [-1, 2, 7]);
        assert_eq!(end, bits.len());
    }

    #[test]
    fn an_index_past_the_codebook_is_an_error() {
        let bits = [1u32, 1, 1, 0]; // q = 3, k = 0 -> index 3 of a 2-entry book
        let p = payload_from_bits(&bits);
        let block = StreamBlock {
            ids: [0, 9, 0xff],
            mode: 2,
            seeds: [0; 5],
            gain_codes: [0; 3],
            gain_offset: 0,
            config: None,
            k: 0,
            adaptive: false,
            count: 2,
            width: 4,
            codebook_pos: 0,
            rice_pos: 0,
        };
        let mut cb = Codebook::new();
        cb.count = 2;
        cb.streams = 1;
        let mut out = [0i32; 1];
        assert_eq!(
            decode_residuals(&p, &block, &cb, 1, &mut out),
            Err(RiceError::Index)
        );
    }
}
