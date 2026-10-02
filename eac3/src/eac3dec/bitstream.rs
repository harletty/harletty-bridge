// SPDX-License-Identifier: Apache-2.0

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

    /// Move to `bit_pos`, for a caller that read [`Self::data`] itself and
    /// has checked where it stopped against [`Self::limit_bits`].
    pub(crate) fn set_position(&mut self, bit_pos: usize) {
        self.bit_pos = bit_pos.min(self.bit_size);
    }

    pub(crate) fn limit_bits(&self) -> usize {
        self.bit_size
    }

    pub(crate) fn data(&self) -> &'a [u8] {
        self.data
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
    pub(crate) fn read_bits(&mut self, bits: usize) -> Option<u32> {
        if !self.bits_left(bits) {
            return None;
        }
        if bits == 0 {
            return Some(0);
        }
        if bits > 32 {
            // Only the low 32 bits fit the result; the rest are skipped, as
            // shifting them through a `u32` one at a time always did.
            self.bit_pos += bits - 32;
            return self.read_bits(32);
        }
        let value = (self.load_be64() << (self.bit_pos & 7)) >> (64 - bits);
        self.bit_pos += bits;
        Some(value as u32)
    }

    /// The eight bytes from the current byte on, big-endian, zero past the
    /// end of the data. `bits_left` has already checked the bits read from it.
    #[inline]
    fn load_be64(&self) -> u64 {
        let byte_pos = self.bit_pos >> 3;
        match self.data.get(byte_pos..byte_pos + 8) {
            Some(word) => u64::from_be_bytes(word.try_into().unwrap()),
            None => {
                let mut word = [0u8; 8];
                let tail = &self.data[byte_pos.min(self.data.len())..];
                word[..tail.len()].copy_from_slice(tail);
                u64::from_be_bytes(word)
            }
        }
    }

    pub(crate) fn show_bits(&self, bits: usize) -> Option<u32> {
        let mut copy = *self;
        copy.read_bits(bits)
    }

    #[inline]
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

    #[inline]
    pub(crate) fn skip_bits(&mut self, bits: usize) -> Option<()> {
        if self.bits_left(bits) {
            self.bit_pos += bits;
            Some(())
        } else {
            None
        }
    }

    pub(crate) fn read_bytes(&mut self, count: usize) -> Option<Vec<u8>> {
        let mut bytes = Vec::with_capacity(count);
        for _ in 0..count {
            bytes.push(self.read_bits(8)? as u8);
        }
        Some(bytes)
    }

    pub(crate) fn read_variable_bits(&mut self, width: usize) -> Option<u32> {
        let mut total = 0u32;
        loop {
            let value = self.read_bits(width)?;
            let read_more = self.read_bit()?;
            total += value;
            if !read_more {
                break;
            }
            total = (total + 1) << width;
        }
        Some(total)
    }

    pub(crate) fn skip_variable_bits(&mut self, width: usize) -> Option<()> {
        self.read_variable_bits(width).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::BitReader;

    /// The bit-at-a-time reader the word reader replaced, kept as its yardstick.
    fn read_bits_one_by_one(data: &[u8], bit_pos: usize, bits: usize) -> u32 {
        let mut value = 0u32;
        for pos in bit_pos..bit_pos + bits {
            value = (value << 1) | ((data[pos >> 3] >> (7 - (pos & 7))) & 1) as u32;
        }
        value
    }

    #[test]
    fn word_reads_match_bit_reads_at_every_width_and_offset() {
        let data: Vec<u8> = (0..23u32)
            .map(|i| (i.wrapping_mul(0x9e37_79b9) >> 13) as u8)
            .collect();
        let total = data.len() * 8;
        for bits in 0..=40 {
            for bit_pos in 0..=total {
                let mut reader = BitReader::with_offset(&data, bit_pos);
                let read = reader.read_bits(bits);
                if bit_pos + bits > total {
                    assert_eq!(read, None, "bits={bits} pos={bit_pos}");
                    assert_eq!(reader.position(), bit_pos);
                    continue;
                }
                // Past 32 bits only the low 32 survive.
                let skipped = bits.saturating_sub(32);
                let expected = read_bits_one_by_one(&data, bit_pos + skipped, bits - skipped);
                assert_eq!(read, Some(expected), "bits={bits} pos={bit_pos}");
                assert_eq!(reader.position(), bit_pos + bits);
            }
        }
    }

    #[test]
    fn a_limit_stops_reads_inside_the_data() {
        let data = [0xffu8; 16];
        let mut reader = BitReader::new(&data);
        reader.set_limit_bits(20);
        assert_eq!(reader.read_bits(16), Some(0xffff));
        assert_eq!(reader.read_bits(5), None);
        assert_eq!(reader.read_bits(4), Some(0xf));
        assert_eq!(reader.read_bits(1), None);
    }
}
