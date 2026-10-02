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

    #[inline]
    pub(crate) fn bits_left(&self, bits: usize) -> bool {
        self.bit_pos + bits <= self.bit_size
    }

    #[inline]
    pub(crate) fn remaining(&self) -> usize {
        self.bit_size.saturating_sub(self.bit_pos)
    }

    /// The next `bits` (1..=32) bits without consuming them, MSB-first. Bits
    /// past the end of `data` read as zero; callers check `bits_left` (or the
    /// decoded length) themselves.
    #[inline]
    fn peek_padded(&self, bits: usize) -> u32 {
        debug_assert!((1..=32).contains(&bits));
        let byte = self.bit_pos >> 3;
        let word = match self.data.get(byte..byte + 8) {
            Some(chunk) => u64::from_be_bytes(chunk.try_into().unwrap()),
            None => {
                let mut buf = [0u8; 8];
                let tail = self.data.get(byte..).unwrap_or(&[]);
                buf[..tail.len()].copy_from_slice(tail);
                u64::from_be_bytes(buf)
            }
        };
        // At most 7 + 32 bits are needed, so one 64-bit load always suffices.
        ((word << (self.bit_pos & 7)) >> (64 - bits)) as u32
    }

    #[inline]
    pub(crate) fn read_bits(&mut self, bits: usize) -> Option<u32> {
        if bits == 0 {
            return Some(0);
        }
        if bits > 32 || !self.bits_left(bits) {
            return None;
        }
        let value = self.peek_padded(bits);
        self.bit_pos += bits;
        Some(value)
    }

    /// Up to 32 bits for a table lookup, zero-padded past the end of the data.
    /// The caller consumes what it decoded with [`Self::consume`].
    #[inline]
    pub(crate) fn peek_lookahead(&self, bits: usize) -> u32 {
        self.peek_padded(bits)
    }

    /// Consume `bits` already examined with [`Self::peek_lookahead`]; fails,
    /// consuming nothing, if fewer remain.
    #[inline]
    pub(crate) fn consume(&mut self, bits: usize) -> Option<()> {
        self.skip_bits(bits)
    }

    pub(crate) fn show_bits(&self, bits: usize) -> Option<u32> {
        let mut copy = *self;
        copy.read_bits(bits)
    }

    #[inline]
    pub(crate) fn read_bit(&mut self) -> Option<bool> {
        self.read_bits(1).map(|bit| bit != 0)
    }

    #[inline]
    pub(crate) fn read_signed_bits(&mut self, bits: usize) -> Option<i32> {
        if bits == 0 || bits > 31 {
            return None;
        }
        let value = self.read_bits(bits)? as i32;
        let shift = 32 - bits;
        Some((value << shift) >> shift)
    }

    #[inline]
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

    /// Count leading 0 bits up to the first 1 (`get_unary(gb, 1, len)`), bounded
    /// by `len`. Consumes the terminating 1 bit (or stops at `len`).
    pub(crate) fn get_unary(&mut self, len: usize) -> usize {
        let mut count = 0usize;
        loop {
            let avail = (len - count).min(self.remaining());
            if avail == 0 {
                return count;
            }
            let chunk = avail.min(32);
            let zeros = (self.peek_padded(chunk) << (32 - chunk)).leading_zeros() as usize;
            if zeros < chunk {
                self.bit_pos += zeros + 1;
                return count + zeros;
            }
            self.bit_pos += chunk;
            count += chunk;
        }
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

#[cfg(test)]
mod tests {
    use super::BitReader;

    /// The bit-at-a-time reads the word loads replaced, kept as the reference.
    fn bit(data: &[u8], pos: usize) -> u32 {
        ((data[pos >> 3] >> (7 - (pos & 7))) & 1) as u32
    }

    fn read_reference(data: &[u8], limit: usize, pos: &mut usize, bits: usize) -> Option<u32> {
        if bits == 0 {
            return Some(0);
        }
        if bits > 32 || *pos + bits > limit {
            return None;
        }
        let mut value = 0u32;
        for _ in 0..bits {
            value = (value << 1) | bit(data, *pos);
            *pos += 1;
        }
        Some(value)
    }

    fn unary_reference(data: &[u8], limit: usize, pos: &mut usize, len: usize) -> usize {
        for i in 0..len {
            if *pos >= limit {
                return i;
            }
            let b = bit(data, *pos);
            *pos += 1;
            if b == 1 {
                return i;
            }
        }
        len
    }

    /// Reads of every width and unary runs at every offset, up to and past
    /// the end of the data and of a limit set below it.
    #[test]
    fn word_reads_match_bit_reads() {
        let mut state = 0x9e37_79b9u32;
        let mut next = || {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            state >> 8
        };
        for round in 0..400 {
            let len = (next() % 24) as usize;
            // Sparse ones so unary runs get long.
            let data: Vec<u8> = (0..len)
                .map(|_| {
                    if round % 2 == 0 {
                        next() as u8
                    } else {
                        (next() as u8) & (next() as u8) & (next() as u8)
                    }
                })
                .collect();
            let limit = (data.len() * 8).saturating_sub((next() % 12) as usize);
            for start in 0..(data.len() * 8).min(40) {
                let mut fast = BitReader::with_offset(&data, start);
                fast.set_limit_bits(limit);
                let mut pos = start.min(limit);
                loop {
                    let op = next() % 3;
                    if op == 2 {
                        let n = (next() % 70) as usize;
                        let a = fast.get_unary(n);
                        let b = unary_reference(&data, limit, &mut pos, n);
                        assert_eq!(a, b);
                    } else {
                        let n = (next() % 34) as usize;
                        let a = fast.read_bits(n);
                        let b = read_reference(&data, limit, &mut pos, n);
                        assert_eq!(a, b);
                        if a.is_none() && n <= 32 {
                            assert_eq!(fast.position(), pos);
                            break;
                        }
                    }
                    assert_eq!(fast.position(), pos);
                }
            }
        }
    }
}
