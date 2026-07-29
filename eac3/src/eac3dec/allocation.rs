// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::needless_range_loop, clippy::too_many_arguments)]

use std::sync::OnceLock;

use super::syncframe::{ExpStrategy, ParseError};

pub(crate) const LFE_END_MANTISSA: usize = 7;
const MAX_ALLOCATION_SIZE: usize = 256;
const MASK_BANDS: usize = 50;
const BAP_BITS: [usize; 16] = [0, 5, 7, 3, 7, 4, 5, 6, 7, 8, 9, 10, 11, 12, 14, 16];
const GROUP_ADD: [isize; 3] = [-1, 2, 8];
const GROUP_DIV: [usize; 3] = [3, 6, 12];
const SLOWDEC: [i32; 4] = [0x0f, 0x11, 0x13, 0x15];
const FASTDEC: [i32; 4] = [0x3f, 0x53, 0x67, 0x7b];
const SLOWGAIN: [i32; 4] = [0x540, 0x4d8, 0x478, 0x410];
const DBPBTAB: [i32; 4] = [0x000, 0x700, 0x900, 0xb00];
const FLOORTAB: [i32; 8] = [0x2f0, 0x2b0, 0x270, 0x230, 0x1f0, 0x170, 0x0f0, -2048];
const HTH: [[i32; MASK_BANDS]; 3] = [
    [
        0x04d0, 0x04d0, 0x0440, 0x0400, 0x03e0, 0x03c0, 0x03b0, 0x03b0, 0x03a0, 0x03a0, 0x03a0,
        0x03a0, 0x03a0, 0x0390, 0x0390, 0x0390, 0x0380, 0x0380, 0x0370, 0x0370, 0x0360, 0x0360,
        0x0350, 0x0350, 0x0340, 0x0340, 0x0330, 0x0320, 0x0310, 0x0300, 0x02f0, 0x02f0, 0x02f0,
        0x02f0, 0x0300, 0x0310, 0x0340, 0x0390, 0x03e0, 0x0420, 0x0460, 0x0490, 0x04a0, 0x0460,
        0x0440, 0x0440, 0x0520, 0x0800, 0x0840, 0x0840,
    ],
    [
        0x04f0, 0x04f0, 0x0460, 0x0410, 0x03e0, 0x03d0, 0x03c0, 0x03b0, 0x03b0, 0x03a0, 0x03a0,
        0x03a0, 0x03a0, 0x03a0, 0x0390, 0x0390, 0x0390, 0x0380, 0x0380, 0x0380, 0x0370, 0x0370,
        0x0360, 0x0360, 0x0350, 0x0350, 0x0340, 0x0340, 0x0320, 0x0310, 0x0300, 0x02f0, 0x02f0,
        0x02f0, 0x02f0, 0x0300, 0x0320, 0x0350, 0x0390, 0x03e0, 0x0420, 0x0450, 0x04a0, 0x0490,
        0x0460, 0x0440, 0x0480, 0x0630, 0x0840, 0x0840,
    ],
    [
        0x0580, 0x0580, 0x04b0, 0x0450, 0x0420, 0x03f0, 0x03e0, 0x03d0, 0x03c0, 0x03b0, 0x03b0,
        0x03b0, 0x03a0, 0x03a0, 0x03a0, 0x03a0, 0x03a0, 0x03a0, 0x03a0, 0x03a0, 0x0390, 0x0390,
        0x0390, 0x0390, 0x0380, 0x0380, 0x0380, 0x0370, 0x0360, 0x0350, 0x0340, 0x0330, 0x0320,
        0x0310, 0x0300, 0x02f0, 0x02f0, 0x02f0, 0x0300, 0x0310, 0x0330, 0x0350, 0x03c0, 0x0410,
        0x0470, 0x04a0, 0x0460, 0x0440, 0x0450, 0x04e0,
    ],
];
const BNDTAB: [usize; MASK_BANDS] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 31, 34, 37, 40, 43, 46, 49, 55, 61, 67, 73, 79, 85, 97, 109, 121, 133, 157, 181, 205,
    229, 253,
];
const MASKTAB: [usize; MAX_ALLOCATION_SIZE] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 28, 28, 29, 29, 29, 30, 30, 30, 31, 31, 31, 32, 32, 32, 33, 33, 33, 34, 34, 34, 35,
    35, 35, 35, 35, 35, 36, 36, 36, 36, 36, 36, 37, 37, 37, 37, 37, 37, 38, 38, 38, 38, 38, 38, 39,
    39, 39, 39, 39, 39, 40, 40, 40, 40, 40, 40, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 42,
    42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 43, 43, 43, 43, 43, 43, 43, 43, 43, 43, 43, 43, 44,
    44, 44, 44, 44, 44, 44, 44, 44, 44, 44, 44, 45, 45, 45, 45, 45, 45, 45, 45, 45, 45, 45, 45, 45,
    45, 45, 45, 45, 45, 45, 45, 45, 45, 45, 45, 46, 46, 46, 46, 46, 46, 46, 46, 46, 46, 46, 46, 46,
    46, 46, 46, 46, 46, 46, 46, 46, 46, 46, 46, 47, 47, 47, 47, 47, 47, 47, 47, 47, 47, 47, 47, 47,
    47, 47, 47, 47, 47, 47, 47, 47, 47, 47, 47, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
    48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 49, 49, 49, 49, 49, 49, 49, 49, 49, 49, 49, 49, 49,
    49, 49, 49, 49, 49, 49, 49, 49, 49, 49, 49, 0, 0, 0,
];
const LATAB: [i32; 246] = [
    0x40, 0x3f, 0x3e, 0x3d, 0x3c, 0x3b, 0x3a, 0x39, 0x38, 0x37, 0x36, 0x35, 0x34, 0x34, 0x33, 0x32,
    0x31, 0x30, 0x2f, 0x2f, 0x2e, 0x2d, 0x2c, 0x2c, 0x2b, 0x2a, 0x29, 0x29, 0x28, 0x27, 0x26, 0x26,
    0x25, 0x24, 0x24, 0x23, 0x23, 0x22, 0x21, 0x21, 0x20, 0x20, 0x1f, 0x1e, 0x1e, 0x1d, 0x1d, 0x1c,
    0x1c, 0x1b, 0x1b, 0x1a, 0x1a, 0x19, 0x19, 0x18, 0x18, 0x17, 0x17, 0x16, 0x16, 0x15, 0x15, 0x15,
    0x14, 0x14, 0x13, 0x13, 0x13, 0x12, 0x12, 0x12, 0x11, 0x11, 0x11, 0x10, 0x10, 0x10, 0x0f, 0x0f,
    0x0f, 0x0e, 0x0e, 0x0e, 0x0d, 0x0d, 0x0d, 0x0d, 0x0c, 0x0c, 0x0c, 0x0c, 0x0b, 0x0b, 0x0b, 0x0b,
    0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x09, 0x09, 0x09, 0x09, 0x09, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08,
    0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x05, 0x05,
    0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04,
    0x04, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x02,
    0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02,
    0x02, 0x02, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
const BAPTAB: [u8; 64] = [
    0, 1, 1, 1, 1, 1, 2, 2, 3, 3, 3, 4, 4, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7, 8, 8, 8, 8, 9, 9, 9, 9,
    10, 10, 10, 10, 11, 11, 11, 11, 12, 12, 12, 12, 13, 13, 13, 13, 14, 14, 14, 14, 14, 14, 14, 14,
    15, 15, 15, 15, 15, 15, 15, 15, 15,
];
/// High-efficiency bap table used for AHT channels (A/52 Annex E; FFmpeg
/// `ff_eac3_hebap_tab`). Values are `hebap` codes 0..=19.
const HEBAPTAB: [u8; 64] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 8, 8, 8, 9, 9, 9, 10, 10, 10, 10, 11, 11, 11, 11, 12, 12, 12, 12,
    13, 13, 13, 13, 14, 14, 14, 14, 15, 15, 15, 15, 16, 16, 16, 16, 17, 17, 17, 17, 18, 18, 18, 18,
    18, 18, 18, 18, 19, 19, 19, 19, 19, 19, 19, 19, 19,
];
const INT24_MAX: f32 = ((1 << 23) - 1) as f32;
const FROM_INT24: f32 = 1.0 / INT24_MAX;
const FROM_INT32: f32 = 1.0 / i32::MAX as f32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeltaBitAllocationMode {
    Reuse = 0,
    NewInfoFollows = 1,
    NoAllocation = 2,
    MuteOutput = 3,
}

impl DeltaBitAllocationMode {
    pub(crate) fn from_bits(bits: u8) -> Result<Self, ParseError> {
        match bits {
            0 => Ok(Self::Reuse),
            1 => Ok(Self::NewInfoFollows),
            2 => Ok(Self::NoAllocation),
            3 => Ok(Self::MuteOutput),
            _ => Err(ParseError::InvalidHeader("deltba")),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DeltaBitAllocationState {
    pub(crate) mode: DeltaBitAllocationMode,
    offsets: Vec<usize>,
    lengths: Vec<usize>,
    bit_allocation: Vec<u8>,
}

impl Default for DeltaBitAllocationState {
    fn default() -> Self {
        Self {
            mode: DeltaBitAllocationMode::NoAllocation,
            offsets: Vec::new(),
            lengths: Vec::new(),
            bit_allocation: Vec::new(),
        }
    }
}

impl DeltaBitAllocationState {
    pub(crate) fn read_segments(
        &mut self,
        reader: &mut super::bitstream::BitReader<'_>,
    ) -> Result<(), ParseError> {
        let segments = reader.read_bits(3).ok_or(ParseError::ShortPacket)? as usize + 1;
        self.offsets.clear();
        self.lengths.clear();
        self.bit_allocation.clear();
        self.offsets.reserve(segments);
        self.lengths.reserve(segments);
        self.bit_allocation.reserve(segments);
        for _ in 0..segments {
            self.offsets
                .push(reader.read_bits(5).ok_or(ParseError::ShortPacket)? as usize);
            self.lengths
                .push(reader.read_bits(4).ok_or(ParseError::ShortPacket)? as usize);
            self.bit_allocation
                .push(reader.read_bits(3).ok_or(ParseError::ShortPacket)? as u8);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BitAllocationParams {
    pub(crate) slow_decay_code: usize,
    pub(crate) fast_decay_code: usize,
    pub(crate) slow_gain_code: usize,
    pub(crate) db_per_bit_code: usize,
    pub(crate) floor_code: usize,
}

impl Default for BitAllocationParams {
    fn default() -> Self {
        Self {
            slow_decay_code: 2,
            fast_decay_code: 1,
            slow_gain_code: 1,
            db_per_bit_code: 2,
            floor_code: 7,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AllocationState {
    exponents: Vec<i32>,
    psd: Vec<i32>,
    integrated_psd: Vec<i32>,
    bap: Vec<u8>,
    excite: Vec<i32>,
    mask: Vec<i32>,
    grouped_scratch: Vec<i32>,
}

impl AllocationState {
    pub(crate) fn new() -> Self {
        Self {
            exponents: vec![0; MAX_ALLOCATION_SIZE],
            psd: vec![0; MAX_ALLOCATION_SIZE],
            integrated_psd: vec![0; MASK_BANDS],
            bap: vec![0; MAX_ALLOCATION_SIZE],
            excite: vec![0; MASK_BANDS],
            mask: vec![0; MASK_BANDS],
            grouped_scratch: Vec::new(),
        }
    }

    pub(crate) fn clear_bap(&mut self) {
        self.bap.fill(0);
    }

    pub(crate) fn read_channel_exponents(
        &mut self,
        reader: &mut super::bitstream::BitReader<'_>,
        strategy: ExpStrategy,
        groups: usize,
        end_mantissa: usize,
    ) -> Result<(), ParseError> {
        let absolute_exponent = reader.read_bits(4).ok_or(ParseError::ShortPacket)? as i32;
        self.grouped_scratch.clear();
        self.grouped_scratch.reserve(groups);
        for _ in 0..groups {
            self.grouped_scratch
                .push(reader.read_bits(7).ok_or(ParseError::ShortPacket)? as i32);
        }
        reader.skip_bits(2).ok_or(ParseError::ShortPacket)?;
        let grouped = std::mem::take(&mut self.grouped_scratch);
        let result = self.decode_grouped_exponents(
            strategy,
            0,
            1,
            end_mantissa,
            absolute_exponent,
            &grouped,
        );
        self.grouped_scratch = grouped;
        result
    }

    pub(crate) fn read_lfe_exponents(
        &mut self,
        reader: &mut super::bitstream::BitReader<'_>,
    ) -> Result<(), ParseError> {
        let absolute_exponent = reader.read_bits(4).ok_or(ParseError::ShortPacket)? as i32;
        self.grouped_scratch.clear();
        self.grouped_scratch.reserve(2);
        self.grouped_scratch
            .push(reader.read_bits(7).ok_or(ParseError::ShortPacket)? as i32);
        self.grouped_scratch
            .push(reader.read_bits(7).ok_or(ParseError::ShortPacket)? as i32);
        let grouped = std::mem::take(&mut self.grouped_scratch);
        let result = self.decode_grouped_exponents(
            ExpStrategy::D15,
            0,
            1,
            LFE_END_MANTISSA,
            absolute_exponent,
            &grouped,
        );
        self.grouped_scratch = grouped;
        result
    }

    pub(crate) fn read_coupling_exponents(
        &mut self,
        reader: &mut super::bitstream::BitReader<'_>,
        strategy: ExpStrategy,
        start_mantissa: usize,
        end_mantissa: usize,
        groups: usize,
    ) -> Result<(), ParseError> {
        // ATSC A/52B §E.1.3.1.1: cplabsexp is a 4-bit field with an implicit LSB of 0,
        // i.e. the actual absolute exponent is the encoded value scaled by 2.
        let absolute_exponent = (reader.read_bits(4).ok_or(ParseError::ShortPacket)? as i32) << 1;
        self.grouped_scratch.clear();
        self.grouped_scratch.reserve(groups);
        for _ in 0..groups {
            self.grouped_scratch
                .push(reader.read_bits(7).ok_or(ParseError::ShortPacket)? as i32);
        }
        let grouped = std::mem::take(&mut self.grouped_scratch);
        // For coupling, cplabsexp is a base only and is NOT itself a usable exponent;
        // the first decoded exponent at `start_mantissa` is `cplabsexp + delta0`.
        // Pass `exponent_offset = start_mantissa` so the loop overwrites the placeholder.
        let result = self.decode_grouped_exponents(
            strategy,
            start_mantissa,
            start_mantissa,
            end_mantissa,
            absolute_exponent,
            &grouped,
        );
        self.grouped_scratch = grouped;
        result
    }

    pub(crate) fn allocate(
        &mut self,
        start: usize,
        end: usize,
        fgain_code: u8,
        snr_offset: i32,
        params: BitAllocationParams,
        sample_rate_index: usize,
        delta: &DeltaBitAllocationState,
        mut fast_leak: i32,
        mut slow_leak: i32,
        aht: bool,
    ) -> Result<(), ParseError> {
        if end == 0 || end > MAX_ALLOCATION_SIZE || start >= end {
            self.clear_bap();
            return Ok(());
        }

        let slow_decay = SLOWDEC[params.slow_decay_code];
        let fast_decay = FASTDEC[params.fast_decay_code];
        let slow_gain = SLOWGAIN[params.slow_gain_code];
        let dbknee = DBPBTAB[params.db_per_bit_code];
        let floor = FLOORTAB[params.floor_code];

        let bnd_start = MASKTAB[start];
        let bnd_end = MASKTAB[end - 1] + 1;
        let fgain = FASTGAIN[fgain_code as usize];
        let mut begin = bnd_start;

        if bnd_start == 0 {
            let mut lowcomp = calc_lowcomp(0, self.integrated_psd[0], self.integrated_psd[1], 0);
            self.excite[0] = self.integrated_psd[0] - fgain - lowcomp;
            lowcomp = calc_lowcomp(lowcomp, self.integrated_psd[1], self.integrated_psd[2], 1);
            self.excite[1] = self.integrated_psd[1] - fgain - lowcomp;
            begin = 7;

            for band in 2..7 {
                if bnd_end != 7 || band != 6 {
                    lowcomp = calc_lowcomp(
                        lowcomp,
                        self.integrated_psd[band],
                        self.integrated_psd[band + 1],
                        band,
                    );
                }
                fast_leak = self.integrated_psd[band] - fgain;
                slow_leak = self.integrated_psd[band] - slow_gain;
                self.excite[band] = fast_leak - lowcomp;
                if (bnd_end != 7 || band != 6)
                    && self.integrated_psd[band] <= self.integrated_psd[band + 1]
                {
                    begin = band + 1;
                    break;
                }
            }

            for band in begin..bnd_end.min(22) {
                if bnd_end != 7 || band != 6 {
                    lowcomp = calc_lowcomp(
                        lowcomp,
                        self.integrated_psd[band],
                        self.integrated_psd[band + 1],
                        band,
                    );
                }
                fast_leak = (fast_leak - fast_decay).max(self.integrated_psd[band] - fgain);
                slow_leak = (slow_leak - slow_decay).max(self.integrated_psd[band] - slow_gain);
                self.excite[band] = (fast_leak - lowcomp).max(slow_leak);
            }
            begin = 22;
        }

        for band in begin..bnd_end {
            fast_leak = (fast_leak - fast_decay).max(self.integrated_psd[band] - fgain);
            slow_leak = (slow_leak - slow_decay).max(self.integrated_psd[band] - slow_gain);
            self.excite[band] = fast_leak.max(slow_leak);
        }

        for band in bnd_start..bnd_end {
            if self.integrated_psd[band] < dbknee {
                self.excite[band] += (dbknee - self.integrated_psd[band]) >> 2;
            }
            self.mask[band] = self.excite[band].max(HTH[sample_rate_index][band]);
        }

        if matches!(
            delta.mode,
            DeltaBitAllocationMode::Reuse | DeltaBitAllocationMode::NewInfoFollows
        ) {
            let mut band = bnd_start;
            // Iterate the zipped segments so a partially-read state (e.g. a
            // ShortPacket mid read_segments that a later Reuse block picks up)
            // can never index out of bounds — audio paths must not panic.
            for ((&offset, &length), &bits) in delta
                .offsets
                .iter()
                .zip(&delta.lengths)
                .zip(&delta.bit_allocation)
            {
                band += offset;
                let delta_mask = if bits >= 4 {
                    ((bits as i32) - 3) << 7
                } else {
                    ((bits as i32) - 4) << 7
                };
                for _ in 0..length {
                    let Some(mask) = self.mask.get_mut(band) else {
                        break;
                    };
                    *mask += delta_mask;
                    band += 1;
                }
            }
        } else if delta.mode == DeltaBitAllocationMode::MuteOutput {
            // TODO: Model reserved `MuteOutput` delta allocation the same way as a real decoder.
            self.clear_bap();
            return Ok(());
        }

        let bap_tab: &[u8; 64] = if aht { &HEBAPTAB } else { &BAPTAB };
        let mut bin = start;
        let mut band = bnd_start;
        loop {
            let last_bin = BNDTAB[band].min(end);
            let mut masked = self.mask[band] - snr_offset - floor;
            if masked < 0 {
                masked = 0;
            }
            masked = (masked & 0x1fe0) + floor;
            while bin < last_bin {
                let address = ((self.psd[bin] - masked) >> 5).clamp(0, 63) as usize;
                self.bap[bin] = bap_tab[address];
                bin += 1;
            }
            band += 1;
            if end <= last_bin {
                break;
            }
        }
        for bap in &mut self.bap[bin..] {
            *bap = 0;
        }
        Ok(())
    }

    pub(crate) fn count_mantissa_bits(
        &self,
        start: usize,
        end: usize,
        group_state: &mut MantissaGroupState,
    ) -> usize {
        let mut bits = 0usize;
        let mut bap1 = 0usize;
        let mut bap2 = 0usize;
        let mut bap4 = 0usize;

        for bin in start..end {
            match self.bap[bin] {
                1 => bap1 += 1,
                2 => bap2 += 1,
                4 => bap4 += 1,
                value => bits += BAP_BITS[value as usize],
            }
        }

        bits += ((group_state.bap1_pos + bap1) / 3) * BAP_BITS[1];
        bits += ((group_state.bap2_pos + bap2) / 3) * BAP_BITS[2];
        bits += ((group_state.bap4_pos + bap4) / 2) * BAP_BITS[4];

        group_state.bap1_pos = (group_state.bap1_pos + bap1) % 3;
        group_state.bap2_pos = (group_state.bap2_pos + bap2) % 3;
        group_state.bap4_pos = (group_state.bap4_pos + bap4) % 2;

        bits
    }

    pub(crate) fn decode_transform_coeffs(
        &self,
        reader: &mut super::bitstream::BitReader<'_>,
        target: &mut [f32; MAX_ALLOCATION_SIZE],
        start: usize,
        end: usize,
        state: &mut MantissaDecodeState,
    ) -> Result<(), ParseError> {
        target.fill(0.0);

        for bin in start..end {
            target[bin] = match self.bap[bin] {
                0 => 0.0,
                1 => {
                    state.bap1_pos += 1;
                    if state.bap1_pos == 3 {
                        let code = reader
                            .read_bits(BAP_BITS[1])
                            .ok_or(ParseError::ShortPacket)?
                            as usize;
                        state.bap1_next.copy_from_slice(
                            bap1_table()
                                .get(code)
                                .ok_or(ParseError::InvalidHeader("bap1"))?,
                        );
                        state.bap1_pos = 0;
                    }
                    scale_int24(state.bap1_next[state.bap1_pos], self.exponents[bin])
                }
                2 => {
                    state.bap2_pos += 1;
                    if state.bap2_pos == 3 {
                        let code = reader
                            .read_bits(BAP_BITS[2])
                            .ok_or(ParseError::ShortPacket)?
                            as usize;
                        state.bap2_next.copy_from_slice(
                            bap2_table()
                                .get(code)
                                .ok_or(ParseError::InvalidHeader("bap2"))?,
                        );
                        state.bap2_pos = 0;
                    }
                    scale_int24(state.bap2_next[state.bap2_pos], self.exponents[bin])
                }
                3 => {
                    let code = reader
                        .read_bits(BAP_BITS[3])
                        .ok_or(ParseError::ShortPacket)? as usize;
                    scale_int24(
                        *bap3_table()
                            .get(code)
                            .ok_or(ParseError::InvalidHeader("bap3"))?,
                        self.exponents[bin],
                    )
                }
                4 => {
                    state.bap4_pos += 1;
                    if state.bap4_pos == 2 {
                        let code = reader
                            .read_bits(BAP_BITS[4])
                            .ok_or(ParseError::ShortPacket)?
                            as usize;
                        state.bap4_next.copy_from_slice(
                            bap4_table()
                                .get(code)
                                .ok_or(ParseError::InvalidHeader("bap4"))?,
                        );
                        state.bap4_pos = 0;
                    }
                    scale_int24(state.bap4_next[state.bap4_pos], self.exponents[bin])
                }
                5 => {
                    let code = reader
                        .read_bits(BAP_BITS[5])
                        .ok_or(ParseError::ShortPacket)? as usize;
                    scale_int24(
                        *bap5_table()
                            .get(code)
                            .ok_or(ParseError::InvalidHeader("bap5"))?,
                        self.exponents[bin],
                    )
                }
                bap => {
                    let bits = BAP_BITS[bap as usize];
                    let raw = reader.read_bits(bits).ok_or(ParseError::ShortPacket)? as i32;
                    let signed = raw << (32 - bits);
                    scale_int32(shift_right_signed(signed, self.exponents[bin]))
                }
            };
        }

        Ok(())
    }

    /// Decode this channel's AHT payload from block 0 of the frame: GAQ gain
    /// codes plus VQ / GAQ pre-mantissas for `start..end`, followed by the
    /// per-bin 6-point IDCT. `allocate()` must have run with `aht = true` so
    /// `bap` holds hebap values. Consumes the exact bit count of the payload,
    /// so it is also used by the syntax walker to stay bit-aligned.
    pub(crate) fn decode_aht_mantissas(
        &self,
        reader: &mut super::bitstream::BitReader<'_>,
        start: usize,
        end: usize,
        pre_mantissas: &mut Vec<[i32; 6]>,
    ) -> Result<(), ParseError> {
        if end > MAX_ALLOCATION_SIZE || start > end {
            return Err(ParseError::InvalidHeader("aht-range"));
        }
        pre_mantissas.clear();
        pre_mantissas.resize(end, [0; 6]);
        super::aht::decode_pre_mantissas(reader, &self.bap, start, end, pre_mantissas)
    }

    /// Produce one block's transform coefficients from decoded AHT
    /// pre-mantissas, applying the per-bin exponent shift (the AHT equivalent
    /// of `decode_transform_coeffs`).
    pub(crate) fn extract_aht_coeffs(
        &self,
        pre_mantissas: &[[i32; 6]],
        block: usize,
        target: &mut [f32; MAX_ALLOCATION_SIZE],
        start: usize,
        end: usize,
    ) {
        target.fill(0.0);
        let end = end.min(pre_mantissas.len()).min(MAX_ALLOCATION_SIZE);
        for bin in start..end {
            let pre = pre_mantissas[bin].get(block).copied().unwrap_or(0);
            target[bin] = scale_int24(pre, self.exponents[bin]);
        }
    }

    fn decode_grouped_exponents(
        &mut self,
        strategy: ExpStrategy,
        start_mantissa: usize,
        exponent_offset: usize,
        end_mantissa: usize,
        absolute_exponent: i32,
        grouped: &[i32],
    ) -> Result<(), ParseError> {
        if end_mantissa > MAX_ALLOCATION_SIZE {
            return Err(ParseError::InvalidHeader("endmant"));
        }

        let group_size = match strategy {
            ExpStrategy::Reuse => return Ok(()),
            ExpStrategy::D15 => 1,
            ExpStrategy::D25 => 2,
            ExpStrategy::D45 => 4,
        };

        let mut current_exponent = absolute_exponent;
        self.exponents[start_mantissa] = current_exponent;
        let mut mantissa = exponent_offset;
        for &group in grouped {
            current_exponent += group / 25 - 2;
            for _ in 0..group_size {
                if mantissa >= MAX_ALLOCATION_SIZE {
                    return Err(ParseError::InvalidHeader("expmant"));
                }
                self.exponents[mantissa] = current_exponent;
                mantissa += 1;
            }

            current_exponent += (group % 25) / 5 - 2;
            for _ in 0..group_size {
                if mantissa >= MAX_ALLOCATION_SIZE {
                    return Err(ParseError::InvalidHeader("expmant"));
                }
                self.exponents[mantissa] = current_exponent;
                mantissa += 1;
            }

            current_exponent += group % 5 - 2;
            for _ in 0..group_size {
                if mantissa >= MAX_ALLOCATION_SIZE {
                    return Err(ParseError::InvalidHeader("expmant"));
                }
                self.exponents[mantissa] = current_exponent;
                mantissa += 1;
            }
        }

        for bin in start_mantissa..end_mantissa {
            self.psd[bin] = 3072 - (self.exponents[bin] << 7);
        }

        let mut bin = start_mantissa;
        let mut band = MASKTAB[start_mantissa];
        loop {
            let last_bin = BNDTAB[band].min(end_mantissa);
            self.integrated_psd[band] = self.psd[bin];
            bin += 1;
            while bin < last_bin {
                self.integrated_psd[band] = log_add(self.integrated_psd[band], self.psd[bin]);
                bin += 1;
            }
            band += 1;
            if end_mantissa <= last_bin {
                break;
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MantissaGroupState {
    bap1_pos: usize,
    bap2_pos: usize,
    bap4_pos: usize,
}

impl MantissaGroupState {
    pub(crate) fn new_block() -> Self {
        Self {
            bap1_pos: 2,
            bap2_pos: 2,
            bap4_pos: 1,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MantissaDecodeState {
    bap1_pos: usize,
    bap2_pos: usize,
    bap4_pos: usize,
    bap1_next: [i32; 3],
    bap2_next: [i32; 3],
    bap4_next: [i32; 2],
}

impl MantissaDecodeState {
    pub(crate) fn new_block() -> Self {
        Self {
            bap1_pos: 2,
            bap2_pos: 2,
            bap4_pos: 1,
            bap1_next: [0; 3],
            bap2_next: [0; 3],
            bap4_next: [0; 2],
        }
    }
}

pub(crate) fn grouped_exponent_count(
    end_mantissa: usize,
    strategy: ExpStrategy,
) -> Result<usize, ParseError> {
    let Some(index) = exp_strategy_index(strategy) else {
        return Ok(0);
    };
    let adjusted = end_mantissa as isize + GROUP_ADD[index];
    if adjusted < 0 {
        return Err(ParseError::InvalidHeader("endmant"));
    }
    Ok(adjusted as usize / GROUP_DIV[index])
}

pub(crate) fn sample_rate_index(sample_rate: u32) -> Option<usize> {
    match sample_rate {
        48_000 | 24_000 | 12_000 => Some(0),
        44_100 | 22_050 | 11_025 => Some(1),
        32_000 | 16_000 | 8_000 => Some(2),
        _ => None,
    }
}

fn exp_strategy_index(strategy: ExpStrategy) -> Option<usize> {
    match strategy {
        ExpStrategy::Reuse => None,
        ExpStrategy::D15 => Some(0),
        ExpStrategy::D25 => Some(1),
        ExpStrategy::D45 => Some(2),
    }
}

fn log_add(a: i32, b: i32) -> i32 {
    let delta = a - b;
    let address = (delta.abs() >> 1).min((LATAB.len() - 1) as i32) as usize;
    if delta >= 0 {
        a + LATAB[address]
    } else {
        b + LATAB[address]
    }
}

fn calc_lowcomp(previous: i32, current: i32, next: i32, band: usize) -> i32 {
    if band < 7 {
        if current + 256 == next {
            return 384;
        }
        if current > next {
            return (previous - 64).max(0);
        }
    } else if band < 20 {
        if current + 256 == next {
            return 320;
        }
        if current > next {
            return (previous - 64).max(0);
        }
    } else {
        return (previous - 128).max(0);
    }
    previous
}

const FASTGAIN: [i32; 8] = [0x080, 0x100, 0x180, 0x200, 0x280, 0x300, 0x380, 0x400];

fn scale_int24(value: i32, exponent: i32) -> f32 {
    shift_right_signed(value, exponent) as f32 * FROM_INT24
}

fn scale_int32(value: i32) -> f32 {
    value as f32 * FROM_INT32
}

fn shift_right_signed(value: i32, bits: i32) -> i32 {
    if bits <= 0 {
        value
    } else if bits >= 31 {
        if value < 0 { -1 } else { 0 }
    } else {
        value >> bits
    }
}

fn generate_quantization(levels: i32) -> Vec<i32> {
    let mut result = vec![0; levels as usize + 1];
    let mut numerator = -1 - levels;
    for value in result.iter_mut().take(levels as usize) {
        numerator += 2;
        *value = (((1 << 23) - 1) * numerator) / levels;
    }
    result
}

fn generate_grouped_quantization<const GROUPS: usize>(
    levels: i32,
    group_bits: usize,
) -> Vec<[i32; GROUPS]> {
    let source = generate_quantization(levels);
    let mut result = Vec::with_capacity(1 << group_bits);
    for code in 0..(1 << group_bits) {
        let mut entry = [0; GROUPS];
        let mut grouped = code;
        for slot in entry.iter_mut().rev() {
            *slot = source[grouped % levels as usize];
            grouped /= levels as usize;
        }
        result.push(entry);
    }
    result
}

fn bap1_table() -> &'static Vec<[i32; 3]> {
    static TABLE: OnceLock<Vec<[i32; 3]>> = OnceLock::new();
    TABLE.get_or_init(|| generate_grouped_quantization::<3>(3, BAP_BITS[1]))
}

fn bap2_table() -> &'static Vec<[i32; 3]> {
    static TABLE: OnceLock<Vec<[i32; 3]>> = OnceLock::new();
    TABLE.get_or_init(|| generate_grouped_quantization::<3>(5, BAP_BITS[2]))
}

fn bap3_table() -> &'static Vec<i32> {
    static TABLE: OnceLock<Vec<i32>> = OnceLock::new();
    TABLE.get_or_init(|| generate_quantization(7))
}

fn bap4_table() -> &'static Vec<[i32; 2]> {
    static TABLE: OnceLock<Vec<[i32; 2]>> = OnceLock::new();
    TABLE.get_or_init(|| generate_grouped_quantization::<2>(11, BAP_BITS[4]))
}

fn bap5_table() -> &'static Vec<i32> {
    static TABLE: OnceLock<Vec<i32>> = OnceLock::new();
    TABLE.get_or_init(|| generate_quantization(15))
}
