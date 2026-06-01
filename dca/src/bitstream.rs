// SPDX-License-Identifier: Apache-2.0
//
// MSB-first bit reader, mirroring `eac3/src/eac3dec/bitstream.rs`. DCA is a
// big-endian bitstream (`get_bits` in ffmpeg's get_bits.h reads MSB-first), so
// the same reader semantics apply.
//
// Some helpers are only exercised by the subband DSP decode (ported
// incrementally); allow dead_code so the utility surface stays complete.
#![allow(dead_code)]

#[derive(Clone, Copy)]
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    bit_size: usize,
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            bit_size: data.len() * 8,
            bit_pos: 0,
        }
    }

    pub(crate) fn with_offset(data: &'a [u8], bit_pos: usize) -> Self {
        Self {
            data,
            bit_size: data.len() * 8,
            bit_pos,
        }
    }

    pub(crate) fn position(&self) -> usize {
        self.bit_pos
    }

    pub(crate) fn set_limit_bits(&mut self, bit_size: usize) {
        self.bit_size = bit_size.min(self.data.len() * 8);
        if self.bit_pos > self.bit_size {
            self.bit_pos = self.bit_size;
        }
    }

    pub(crate) fn bits_left(&self, bits: usize) -> bool {
        self.bit_pos + bits <= self.bit_size
    }

    pub(crate) fn remaining(&self) -> usize {
        self.bit_size.saturating_sub(self.bit_pos)
    }

    pub(crate) fn read_bits(&mut self, bits: usize) -> Option<u32> {
        if bits == 0 {
            return Some(0);
        }
        if bits > 32 || !self.bits_left(bits) {
            return None;
        }

        let mut value = 0u32;
        for _ in 0..bits {
            let byte_pos = self.bit_pos >> 3;
            let bit_off = 7 - (self.bit_pos & 7);
            value = (value << 1) | ((self.data[byte_pos] >> bit_off) & 1) as u32;
            self.bit_pos += 1;
        }
        Some(value)
    }

    pub(crate) fn show_bits(&self, bits: usize) -> Option<u32> {
        let mut copy = *self;
        copy.read_bits(bits)
    }

    pub(crate) fn read_bit(&mut self) -> Option<bool> {
        self.read_bits(1).map(|bit| bit != 0)
    }

    pub(crate) fn read_signed_bits(&mut self, bits: usize) -> Option<i32> {
        if bits == 0 || bits > 31 {
            return None;
        }
        let value = self.read_bits(bits)? as i32;
        let shift = 32 - bits;
        Some((value << shift) >> shift)
    }

    pub(crate) fn skip_bits(&mut self, bits: usize) -> Option<()> {
        if self.bits_left(bits) {
            self.bit_pos += bits;
            Some(())
        } else {
            None
        }
    }

    /// Align the read cursor up to the next 32-bit boundary, matching ffmpeg's
    /// frame-level word alignment used between DCA substream blocks.
    pub(crate) fn align_bits(&mut self, n: usize) {
        let rem = self.bit_pos % n;
        if rem != 0 {
            self.bit_pos += n - rem;
        }
    }

    /// Seek to an absolute bit position (`ff_dca_seek_bits`). Returns false if
    /// the position is past the available data (caller treats as error).
    pub(crate) fn seek(&mut self, pos: usize) -> bool {
        if pos > self.bit_size {
            return false;
        }
        self.bit_pos = pos;
        true
    }

    /// Skip an arbitrary number of bits (may exceed 32; `skip_bits_long`).
    pub(crate) fn skip_bits_long(&mut self, bits: usize) -> Option<()> {
        if self.bits_left(bits) {
            self.bit_pos += bits;
            Some(())
        } else {
            None
        }
    }
}
