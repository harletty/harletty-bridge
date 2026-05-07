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

    pub(crate) fn set_limit_bits(&mut self, bit_size: usize) {
        self.bit_size = bit_size.min(self.data.len() * 8);
        if self.bit_pos > self.bit_size {
            self.bit_pos = self.bit_size;
        }
    }

    pub(crate) fn bits_left(&self, bits: usize) -> bool {
        self.bit_pos + bits <= self.bit_size
    }

    pub(crate) fn read_bits(&mut self, bits: usize) -> Option<u32> {
        if !self.bits_left(bits) {
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
