// SPDX-License-Identifier: Apache-2.0
//
// DCA core Huffman (VLC) tables, built from the generated `{symbol, length}`
// source pairs the same way ffmpeg's `ff_dca_init_vlcs` does:
// `ff_vlc_init_from_lengths` assigns canonical codes by walking the entries in
// order, MSB-aligned, with `code += 1 << (BITS - len)` after each. The stored
// symbol is offset by the codebook's `entry_offset`.

use std::collections::HashMap;
use std::sync::OnceLock;

use super::tables::{BITALLOC_OFFSETS, BITALLOC_SIZES, QUANT_INDEX_GROUP_SIZE, VLC_SRC_TABLES};
use crate::bitstream::BitReader;

const DCA_CODE_BOOKS: usize = 10;

/// A canonical prefix-code table. Keyed by a sentinel-prefixed code value
/// (`(1 << len) | code`) so `(len, code)` pairs never collide across lengths.
#[derive(Debug, Default)]
pub(crate) struct Vlc {
    map: HashMap<u32, i32>,
    max_len: u8,
}

impl Vlc {
    /// Build from a slice of `{symbol, length}` pairs plus the symbol offset.
    fn from_lengths(pairs: &[[u8; 2]], offset: i32) -> Self {
        let mut map = HashMap::with_capacity(pairs.len());
        let mut max_len = 0u8;
        // 64-bit MSB-aligned accumulator, matching ff_vlc_init_from_lengths.
        let mut acc: u64 = 0;
        for &[symbol, len] in pairs {
            if len == 0 {
                continue; // unused entry
            }
            let code = (acc >> (64 - len as u32)) as u32;
            acc = acc.wrapping_add(1u64 << (64 - len as u32));
            let key = (1u32 << len) | code;
            map.insert(key, symbol as i32 + offset);
            max_len = max_len.max(len);
        }
        Self { map, max_len }
    }

    /// Decode one symbol, reading bits MSB-first. Returns `None` on bitstream
    /// underrun or an invalid (non-prefix) code.
    pub(crate) fn get(&self, gb: &mut BitReader) -> Option<i32> {
        let mut key = 1u32;
        for _ in 0..self.max_len {
            let bit = gb.read_bit()? as u32;
            key = (key << 1) | bit;
            if let Some(&sym) = self.map.get(&key) {
                return Some(sym);
            }
        }
        None
    }
}

/// The core VLC sets, sliced from `VLC_SRC_TABLES` in `ff_dca_init_vlcs` order.
#[derive(Debug)]
pub(crate) struct CoreVlcs {
    /// `[codebook][group]` — quantization index codebooks.
    pub(crate) quant_index: Vec<Vec<Vlc>>,
    pub(crate) bit_allocation: Vec<Vlc>, // 5
    pub(crate) scale_factor: Vec<Vlc>,   // 5
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
