// SPDX-License-Identifier: Apache-2.0
//
// DCA core Huffman (VLC) tables, built from the generated `{symbol, length}`
// source pairs the same way ffmpeg's `ff_dca_init_vlcs` does:
// `ff_vlc_init_from_lengths` assigns canonical codes by walking the entries in
// order, MSB-aligned, with `code += 1 << (BITS - len)` after each. The stored
// symbol is offset by the codebook's `entry_offset`.

use std::sync::OnceLock;

use super::tables::{BITALLOC_OFFSETS, BITALLOC_SIZES, QUANT_INDEX_GROUP_SIZE, VLC_SRC_TABLES};
use crate::bitstream::BitReader;

const DCA_CODE_BOOKS: usize = 10;

/// Bits resolved by a codebook's first lookup; longer codes take a second one.
const ROOT_BITS: u32 = 9;
/// Entry flag: the entry points at a second-level table.
const SUBTABLE: u32 = 1 << 31;

/// A canonical prefix-code table, decoded by table lookup: the first
/// `root_bits` of the stream index `table`, whose entry is either a code
/// (`len << 16 | symbol`), a pointer to a second-level table for the longer
/// codes sharing that prefix (`SUBTABLE | sub_bits << 24 | offset`), or 0 for
/// a bit pattern no code starts with.
#[derive(Debug, Default)]
pub(crate) struct Vlc {
    table: Vec<u32>,
    root_bits: u32,
    max_len: u32,
}

impl Vlc {
    /// Build from a slice of `{symbol, length}` pairs plus the symbol offset.
    fn from_lengths(pairs: &[[u8; 2]], offset: i32) -> Self {
        let mut codes = Vec::with_capacity(pairs.len());
        // 64-bit MSB-aligned accumulator, matching ff_vlc_init_from_lengths.
        let mut acc: u64 = 0;
        for &[symbol, len] in pairs {
            if len == 0 {
                continue; // unused entry
            }
            let code = (acc >> (64 - len as u32)) as u32;
            acc = acc.wrapping_add(1u64 << (64 - len as u32));
            codes.push((code, len as u32, symbol as i32 + offset));
        }
        let max_len = codes.iter().map(|&(_, len, _)| len).max().unwrap_or(0);
        let root_bits = max_len.min(ROOT_BITS);
        let entry = |len: u32, symbol: i32| {
            debug_assert!(i16::try_from(symbol).is_ok());
            len << 16 | (symbol as i16 as u16 as u32)
        };

        let mut table = vec![0u32; 1 << root_bits];
        // Second-level width per root prefix: the longest code's excess bits.
        let mut sub_bits = vec![0u32; 1 << root_bits];
        for &(code, len, symbol) in &codes {
            if len <= root_bits {
                let first = (code << (root_bits - len)) as usize;
                table[first..first + (1 << (root_bits - len))].fill(entry(len, symbol));
            } else {
                let prefix = (code >> (len - root_bits)) as usize;
                sub_bits[prefix] = sub_bits[prefix].max(len - root_bits);
            }
        }
        for (prefix, &bits) in sub_bits.iter().enumerate() {
            if bits != 0 {
                debug_assert!(table.len() < 1 << 16);
                table[prefix] = SUBTABLE | bits << 24 | table.len() as u32;
                table.resize(table.len() + (1 << bits), 0);
            }
        }
        for &(code, len, symbol) in &codes {
            if len > root_bits {
                let prefix = (code >> (len - root_bits)) as usize;
                let bits = sub_bits[prefix];
                let extra = len - root_bits;
                let base = (table[prefix] & 0xffff) as usize;
                let first = base + (((code & ((1 << extra) - 1)) << (bits - extra)) as usize);
                table[first..first + (1 << (bits - extra))].fill(entry(len, symbol));
            }
        }
        Self {
            table,
            root_bits,
            max_len,
        }
    }

    /// Decode one symbol, reading bits MSB-first. Returns `None` on bitstream
    /// underrun or an invalid (non-prefix) code.
    #[inline]
    pub(crate) fn get(&self, gb: &mut BitReader) -> Option<i32> {
        if self.max_len == 0 {
            return None;
        }
        let peek = gb.peek_lookahead(self.max_len as usize);
        let rest = self.max_len - self.root_bits;
        let mut e = self.table[(peek >> rest) as usize];
        if e & SUBTABLE != 0 {
            let bits = (e >> 24) & 0x1f;
            let index = (peek >> (rest - bits)) & ((1 << bits) - 1);
            e = self.table[(e & 0xffff) as usize + index as usize];
        }
        let len = e >> 16;
        if len == 0 {
            return None;
        }
        gb.consume(len as usize)?;
        Some(e as u16 as i16 as i32)
    }
}

/// The core VLC sets, sliced from `VLC_SRC_TABLES` in `ff_dca_init_vlcs` order.
#[derive(Debug)]
pub(crate) struct CoreVlcs {
    /// `[codebook][group]` — quantization index codebooks.
    pub(crate) quant_index: Vec<Vec<Vlc>>,
    pub(crate) bit_allocation: Vec<Vlc>,  // 5
    pub(crate) scale_factor: Vec<Vlc>,    // 5
    pub(crate) transition_mode: Vec<Vlc>, // 4
}

impl CoreVlcs {
    fn build() -> Self {
        let src = VLC_SRC_TABLES;
        let mut pos = 0usize;
        let mut take = |n: usize, offset: i32| -> Vlc {
            let v = Vlc::from_lengths(&src[pos..pos + n], offset);
            pos += n;
            v
        };

        // 1) quant_index[i][j], i in 0..10, j in 0..group_size[i].
        let mut quant_index = Vec::with_capacity(DCA_CODE_BOOKS);
        for i in 0..DCA_CODE_BOOKS {
            let groups = QUANT_INDEX_GROUP_SIZE[i] as usize;
            let size = BITALLOC_SIZES[i] as usize;
            let offset = BITALLOC_OFFSETS[i] as i32;
            let mut row = Vec::with_capacity(groups);
            for _ in 0..groups {
                row.push(take(size, offset));
            }
            quant_index.push(row);
        }

        // 2) bit_allocation[5], 12 codes, offset 1.
        let bit_allocation = (0..5).map(|_| take(12, 1)).collect();
        // 3) scale_factor[5], 129 codes, offset -64.
        let scale_factor = (0..5).map(|_| take(129, -64)).collect();
        // 4) transition_mode[4], 4 codes, offset 0.
        let transition_mode = (0..4).map(|_| take(4, 0)).collect();

        Self {
            quant_index,
            bit_allocation,
            scale_factor,
            transition_mode,
        }
    }
}

/// Lazily-built, shared core VLC tables.
pub(crate) fn core_vlcs() -> &'static CoreVlcs {
    static VLCS: OnceLock<CoreVlcs> = OnceLock::new();
    VLCS.get_or_init(CoreVlcs::build)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitstream_writer::BitWriter;

    #[test]
    fn builds_all_core_vlcs() {
        let v = core_vlcs();
        assert_eq!(v.bit_allocation.len(), 5);
        assert_eq!(v.scale_factor.len(), 5);
        assert_eq!(v.transition_mode.len(), 4);
        assert_eq!(v.quant_index.len(), 10);
        for (i, row) in v.quant_index.iter().enumerate() {
            assert_eq!(row.len(), QUANT_INDEX_GROUP_SIZE[i] as usize);
        }
    }

    /// The bit-by-bit walk the tables replaced, kept as the reference.
    fn get_reference(pairs: &[[u8; 2]], offset: i32, gb: &mut BitReader) -> Option<i32> {
        let mut map = std::collections::HashMap::new();
        let mut max_len = 0u8;
        let mut acc: u64 = 0;
        for &[symbol, len] in pairs {
            if len == 0 {
                continue;
            }
            let code = (acc >> (64 - len as u32)) as u32;
            acc = acc.wrapping_add(1u64 << (64 - len as u32));
            map.insert((1u32 << len) | code, symbol as i32 + offset);
            max_len = max_len.max(len);
        }
        let mut key = 1u32;
        for _ in 0..max_len {
            key = (key << 1) | gb.read_bit()? as u32;
            if let Some(&sym) = map.get(&key) {
                return Some(sym);
            }
        }
        None
    }

    /// Every codebook, on pseudo-random streams of every length up to a few
    /// codes: same symbols, same positions, same failures as the bit walk.
    #[test]
    fn tables_match_the_bit_by_bit_walk() {
        let mut books: Vec<(&[[u8; 2]], i32)> = Vec::new();
        let src = VLC_SRC_TABLES;
        let mut pos = 0usize;
        let mut take = |n: usize, offset: i32| {
            books.push((&src[pos..pos + n], offset));
            pos += n;
        };
        for i in 0..DCA_CODE_BOOKS {
            for _ in 0..QUANT_INDEX_GROUP_SIZE[i] {
                take(BITALLOC_SIZES[i] as usize, BITALLOC_OFFSETS[i] as i32);
            }
        }
        (0..5).for_each(|_| take(12, 1));
        (0..5).for_each(|_| take(129, -64));
        (0..4).for_each(|_| take(4, 0));

        let mut state = 0x1234_5678u32;
        for (pairs, offset) in books {
            let vlc = Vlc::from_lengths(pairs, offset);
            for _ in 0..300 {
                let len = (state >> 24) as usize % 9;
                let bytes: Vec<u8> = (0..len)
                    .map(|_| {
                        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                        (state >> 16) as u8
                    })
                    .collect();
                for limit in [bytes.len() * 8, (bytes.len() * 8).saturating_sub(3)] {
                    let mut fast = BitReader::new(&bytes);
                    let mut slow = BitReader::new(&bytes);
                    fast.set_limit_bits(limit);
                    slow.set_limit_bits(limit);
                    loop {
                        let a = vlc.get(&mut fast);
                        let b = get_reference(pairs, offset, &mut slow);
                        assert_eq!(a, b);
                        if a.is_none() {
                            break;
                        }
                        assert_eq!(fast.position(), slow.position());
                    }
                }
            }
        }
    }

    #[test]
    fn bitalloc_3_roundtrip() {
        // First codebook group 0 is bitalloc_3: pairs {1,1},{2,2},{0,2} with
        // offset BITALLOC_OFFSETS[0] = -1. Canonical codes: sym1="0",
        // sym2="10", sym0="11"; output symbol = stored + (-1).
        let vlc = &core_vlcs().quant_index[0][0];
        let cases = [(0b0u32, 1usize, 1 - 1), (0b10, 2, 2 - 1), (0b11, 2, 0 - 1)];
        for (code, len, expected) in cases {
            let mut w = BitWriter::new();
            w.write(len, code);
            let bytes = w.finish();
            let mut gb = BitReader::new(&bytes);
            assert_eq!(vlc.get(&mut gb), Some(expected), "code {code:0len$b}");
        }
    }
}
