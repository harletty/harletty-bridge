// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::needless_range_loop, clippy::too_many_arguments)]

use super::syncframe::{ExpStrategy, ParseError};

pub(crate) const LFE_END_MANTISSA: usize = 7;
const MAX_ALLOCATION_SIZE: usize = 256;
const MASK_BANDS: usize = 50;
const BAP_BITS: [usize; 16] = [0, 5, 7, 3, 7, 4, 5, 6, 7, 8, 9, 10, 11, 12, 14, 16];
/// The widest field one mantissa can read.
const MAX_MANTISSA_BITS: usize = 16;
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
    /// Equality, without handing empty `Vec`s to `memcmp`: glibc's AVX-512
    /// `bcmp` issues a masked load even for zero bytes, and from an empty
    /// `Vec`'s dangling pointer that takes a microcode assist costing more
    /// than the whole bit allocation it was meant to skip.
    fn same_as(&self, other: &Self) -> bool {
        self.mode == other.mode
            && self.offsets.len() == other.offsets.len()
            && (self.offsets.is_empty()
                || (self.offsets == other.offsets
                    && self.lengths == other.lengths
                    && self.bit_allocation == other.bit_allocation))
    }

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

#[derive(Debug, Clone, Copy, PartialEq)]
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
    /// The exponents as the shift a mantissa takes: clamped to an `i32`'s.
    shifts: Vec<u8>,
    psd: Vec<i32>,
    integrated_psd: Vec<i32>,
    bap: Vec<u8>,
    excite: Vec<i32>,
    mask: Vec<i32>,
    grouped_scratch: Vec<i32>,
    /// The arguments `bap` was last computed from, while the exponents it was
    /// computed from are still the current ones. `allocate` is a pure function
    /// of the exponents and these, so a block that reuses its exponents and
    /// repeats them has its `bap` already (FFmpeg's `bit_alloc_stages`).
    allocated_with: Option<AllocationArgs>,
    /// What `count_mantissa_bits` last found in `bap`, while `bap` is unchanged.
    mantissa_counts: Option<MantissaCounts>,
    /// The mantissas `bap` asks for, sorted by class, while `bap` is
    /// unchanged. Made by the first channel that decodes some: a walk that
    /// only counts them, as the inspection's does, never needs it.
    plan: Option<Box<MantissaPlan>>,
    /// Whether `bap` is the all-zero one `clear_bap` leaves, which asks for
    /// no mantissa at all.
    bap_is_clear: bool,
}

/// The bins of a `bap` range, sorted into what `count_mantissa_bits` needs:
/// the bits of the ungrouped mantissas, and how many of each grouped kind.
#[derive(Debug, Clone, Copy)]
struct MantissaCounts {
    start: usize,
    end: usize,
    bits: usize,
    bap1: usize,
    bap2: usize,
    bap4: usize,
}

#[derive(Debug, Clone)]
struct AllocationArgs {
    start: usize,
    end: usize,
    fgain_code: u8,
    snr_offset: i32,
    params: BitAllocationParams,
    sample_rate_index: usize,
    delta: DeltaBitAllocationState,
    fast_leak: i32,
    slow_leak: i32,
    aht: bool,
}

impl AllocationArgs {
    #[allow(clippy::too_many_arguments)]
    fn matches(
        &self,
        start: usize,
        end: usize,
        fgain_code: u8,
        snr_offset: i32,
        params: BitAllocationParams,
        sample_rate_index: usize,
        delta: &DeltaBitAllocationState,
        fast_leak: i32,
        slow_leak: i32,
        aht: bool,
    ) -> bool {
        self.start == start
            && self.end == end
            && self.fgain_code == fgain_code
            && self.snr_offset == snr_offset
            && self.params == params
            && self.sample_rate_index == sample_rate_index
            && self.fast_leak == fast_leak
            && self.slow_leak == slow_leak
            && self.aht == aht
            && self.delta.same_as(delta)
    }
}

impl AllocationState {
    pub(crate) fn new() -> Self {
        Self {
            exponents: vec![0; MAX_ALLOCATION_SIZE],
            shifts: vec![0; MAX_ALLOCATION_SIZE],
            psd: vec![0; MAX_ALLOCATION_SIZE],
            integrated_psd: vec![0; MASK_BANDS],
            bap: vec![0; MAX_ALLOCATION_SIZE],
            excite: vec![0; MASK_BANDS],
            mask: vec![0; MASK_BANDS],
            grouped_scratch: Vec::new(),
            allocated_with: None,
            mantissa_counts: None,
            plan: None,
            bap_is_clear: true,
        }
    }

    pub(crate) fn clear_bap(&mut self) {
        self.bap.fill(0);
        self.allocated_with = None;
        self.mantissa_counts = None;
        self.forget_plan();
        self.bap_is_clear = true;
    }

    /// `bap` is about to change: what was sorted out of it no longer holds.
    fn forget_plan(&mut self) {
        if let Some(plan) = &mut self.plan {
            plan.valid = false;
        }
        self.bap_is_clear = false;
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
        if self.allocated_with.as_ref().is_some_and(|args| {
            args.matches(
                start,
                end,
                fgain_code,
                snr_offset,
                params,
                sample_rate_index,
                delta,
                fast_leak,
                slow_leak,
                aht,
            )
        }) {
            return Ok(());
        }
        self.mantissa_counts = None;
        self.forget_plan();
        let args = AllocationArgs {
            start,
            end,
            fgain_code,
            snr_offset,
            params,
            sample_rate_index,
            delta: delta.clone(),
            fast_leak,
            slow_leak,
            aht,
        };

        // The tables as arrays of their own: through `self` every store
        // would have the compiler fetch each `Vec`'s pointer again.
        let (Ok(psd), Ok(integrated_psd), Ok(excite), Ok(mask), Ok(bap)) = (
            <&[i32; MAX_ALLOCATION_SIZE]>::try_from(&self.psd[..]),
            <&[i32; MASK_BANDS]>::try_from(&self.integrated_psd[..]),
            <&mut [i32; MASK_BANDS]>::try_from(&mut self.excite[..]),
            <&mut [i32; MASK_BANDS]>::try_from(&mut self.mask[..]),
            <&mut [u8; MAX_ALLOCATION_SIZE]>::try_from(&mut self.bap[..]),
        ) else {
            return Ok(());
        };

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
            let mut lowcomp = calc_lowcomp(0, integrated_psd[0], integrated_psd[1], 0);
            excite[0] = integrated_psd[0] - fgain - lowcomp;
            lowcomp = calc_lowcomp(lowcomp, integrated_psd[1], integrated_psd[2], 1);
            excite[1] = integrated_psd[1] - fgain - lowcomp;
            begin = 7;

            for band in 2..7 {
                if bnd_end != 7 || band != 6 {
                    lowcomp = calc_lowcomp(
                        lowcomp,
                        integrated_psd[band],
                        integrated_psd[band + 1],
                        band,
                    );
                }
                fast_leak = integrated_psd[band] - fgain;
                slow_leak = integrated_psd[band] - slow_gain;
                excite[band] = fast_leak - lowcomp;
                if (bnd_end != 7 || band != 6) && integrated_psd[band] <= integrated_psd[band + 1] {
                    begin = band + 1;
                    break;
                }
            }

            for band in begin..bnd_end.min(22) {
                if bnd_end != 7 || band != 6 {
                    lowcomp = calc_lowcomp(
                        lowcomp,
                        integrated_psd[band],
                        integrated_psd[band + 1],
                        band,
                    );
                }
                fast_leak = (fast_leak - fast_decay).max(integrated_psd[band] - fgain);
                slow_leak = (slow_leak - slow_decay).max(integrated_psd[band] - slow_gain);
                excite[band] = (fast_leak - lowcomp).max(slow_leak);
            }
            begin = 22;
        }

        for band in begin..bnd_end {
            fast_leak = (fast_leak - fast_decay).max(integrated_psd[band] - fgain);
            slow_leak = (slow_leak - slow_decay).max(integrated_psd[band] - slow_gain);
            excite[band] = fast_leak.max(slow_leak);
        }

        for band in bnd_start..bnd_end {
            if integrated_psd[band] < dbknee {
                excite[band] += (dbknee - integrated_psd[band]) >> 2;
            }
            mask[band] = excite[band].max(HTH[sample_rate_index][band]);
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
                    let Some(mask) = mask.get_mut(band) else {
                        break;
                    };
                    *mask += delta_mask;
                    band += 1;
                }
            }
        } else if delta.mode == DeltaBitAllocationMode::MuteOutput {
            // TODO: Model reserved `MuteOutput` delta allocation the same way as a real decoder.
            bap.fill(0);
            self.allocated_with = None;
            self.mantissa_counts = None;
            return Ok(());
        }

        let bap_tab: &[u8; 64] = if aht { &HEBAPTAB } else { &BAPTAB };
        let mut bin = start;
        let mut band = bnd_start;
        loop {
            let last_bin = BNDTAB[band].min(end);
            let mut masked = mask[band] - snr_offset - floor;
            if masked < 0 {
                masked = 0;
            }
            masked = (masked & 0x1fe0) + floor;
            while bin < last_bin {
                let address = ((psd[bin] - masked) >> 5).clamp(0, 63) as usize;
                bap[bin] = bap_tab[address];
                bin += 1;
            }
            band += 1;
            if end <= last_bin {
                break;
            }
        }
        bap[bin..].fill(0);
        self.allocated_with = Some(args);
        Ok(())
    }

    pub(crate) fn count_mantissa_bits(
        &mut self,
        start: usize,
        end: usize,
        group_state: &mut MantissaGroupState,
    ) -> usize {
        // A block that kept its bit allocation has the same counts; only the
        // grouping carried in from the channels before it differs.
        let counts = match self.mantissa_counts {
            Some(counts) if counts.start == start && counts.end == end => counts,
            _ => {
                let mut counts = MantissaCounts {
                    start,
                    end,
                    bits: 0,
                    bap1: 0,
                    bap2: 0,
                    bap4: 0,
                };
                for bin in start..end {
                    match self.bap[bin] {
                        1 => counts.bap1 += 1,
                        2 => counts.bap2 += 1,
                        4 => counts.bap4 += 1,
                        value => counts.bits += BAP_BITS[value as usize],
                    }
                }
                self.mantissa_counts = Some(counts);
                counts
            }
        };
        let MantissaCounts {
            mut bits,
            bap1,
            bap2,
            bap4,
            ..
        } = counts;

        bits += ((group_state.bap1_pos + bap1) / 3) * BAP_BITS[1];
        bits += ((group_state.bap2_pos + bap2) / 3) * BAP_BITS[2];
        bits += ((group_state.bap4_pos + bap4) / 2) * BAP_BITS[4];

        group_state.bap1_pos = (group_state.bap1_pos + bap1) % 3;
        group_state.bap2_pos = (group_state.bap2_pos + bap2) % 3;
        group_state.bap4_pos = (group_state.bap4_pos + bap4) % 2;

        bits
    }

    /// Read this channel's mantissas for `start..end` and scale them by their
    /// exponents into `target`, zero elsewhere.
    ///
    /// Which field a mantissa is follows from `bap`, and reading them in
    /// bitstream order means a jump on `bap` per bin that the processor
    /// cannot predict - most of what that loop costs, along with the bins
    /// that have no mantissa at all, walked for nothing. So `bap` is sorted
    /// first, into a [`MantissaPlan`]: the mantissas of each class with the
    /// bit their field starts at, in a pass that branches on nothing in
    /// `bap`. The fields are then read by one tight loop per class. Most
    /// blocks keep the `bap` of the block before, and its plan with it.
    pub(crate) fn decode_transform_coeffs(
        &mut self,
        reader: &mut super::bitstream::BitReader<'_>,
        target: &mut [f32; MAX_ALLOCATION_SIZE],
        start: usize,
        end: usize,
        state: &mut MantissaDecodeState,
    ) -> Result<(), ParseError> {
        let (Some(baps), Ok(shifts)) = (
            self.bap.get(start..end),
            <&[u8; MAX_ALLOCATION_SIZE]>::try_from(&self.shifts[..]),
        ) else {
            target.fill(0.0);
            return Ok(());
        };
        if self.bap_is_clear {
            target.fill(0.0);
            return Ok(());
        }
        let phases = state.phases();
        let plan = self
            .plan
            .get_or_insert_with(|| Box::new(MantissaPlan::new()));
        if !plan.valid || plan.start != start || plan.end != end || plan.phases != phases {
            plan.build(baps, start, end, phases);
        }
        if !plan.fits {
            return self.decode_mantissas_in_order(reader, target, start, end, state);
        }
        let Some(plan) = self.plan.as_deref() else {
            return Ok(());
        };

        let position = reader.position();
        let after = position + plan.bits;
        if after > reader.limit_bits() {
            return Err(ParseError::ShortPacket);
        }
        // The fields are read as whole words, so a channel that reaches the
        // last bytes of the data reads a copy of them with zeros after.
        let data = reader.data();
        let tail;
        let (data, base) = if after.div_ceil(8) + 8 <= data.len() {
            (data, position)
        } else {
            let from = (position / 8).min(data.len());
            let mut copy = [0u8; MAX_ALLOCATION_SIZE * MAX_MANTISSA_BITS / 8 + 16];
            let kept = (data.len() - from).min(copy.len() - 8);
            copy[..kept].copy_from_slice(&data[from..from + kept]);
            tail = copy;
            (&tail[..], position - from * 8)
        };

        target.fill(0.0);
        plan.decode(data, base, shifts, target, state);
        reader.set_position(after);
        Ok(())
    }

    /// The mantissas one after the other, each field deciding where the next
    /// starts: for a range no plan lists.
    fn decode_mantissas_in_order(
        &self,
        reader: &mut super::bitstream::BitReader<'_>,
        target: &mut [f32; MAX_ALLOCATION_SIZE],
        start: usize,
        end: usize,
        state: &mut MantissaDecodeState,
    ) -> Result<(), ParseError> {
        let (Some(baps), Some(shifts)) = (self.bap.get(start..end), self.shifts.get(start..end))
        else {
            target.fill(0.0);
            return Ok(());
        };
        target[..start].fill(0.0);
        target[end..].fill(0.0);
        let target = &mut target[start..end];

        // The position and the groups in flight stay in registers over the
        // channel: nothing in the loop calls out, and nothing in it can fail.
        // The reads are whole words, so a channel that could reach the last
        // bytes of the data reads a copy of them with zeros after; the
        // position is checked against the limit once, at the end.
        let data = reader.data();
        let position = reader.position();
        let reach = (position + MAX_MANTISSA_BITS * baps.len()) / 8 + 9;
        let tail;
        let (data, base) = if reach <= data.len() {
            (data, 0)
        } else {
            let from = (position / 8).min(data.len());
            let mut copy = [0u8; MAX_ALLOCATION_SIZE * MAX_MANTISSA_BITS / 8 + 16];
            let kept = (data.len() - from).min(copy.len() - 8);
            copy[..kept].copy_from_slice(&data[from..from + kept]);
            tail = copy;
            (&tail[..], from * 8)
        };
        let mut position = position - base;
        let MantissaDecodeState {
            mut group1,
            mut group2,
            mut group4,
        } = *state;
        let mut overrun = false;
        macro_rules! read {
            ($bits:expr) => {{
                let bits: usize = $bits;
                let Some(word) = data.get(position / 8..position / 8 + 8) else {
                    overrun = true;
                    break;
                };
                let word = u64::from_be_bytes(word.try_into().unwrap()) << (position & 7);
                position += bits;
                (word >> (64 - bits)) as usize
            }};
        }

        // A group is its code and how far into it the channel is, as the
        // index of the value in the flat table: the code times the row
        // length, plus the position. Codes index tables sized to their field
        // width, so the masks change nothing; they let the compiler drop the
        // bounds checks.
        for ((slot, &bap), &shift) in target.iter_mut().zip(baps).zip(shifts) {
            let symmetric = match bap {
                0 => 0,
                1 => {
                    group1 += 1;
                    if group1 & 3 == 3 {
                        group1 = read!(BAP_BITS[1]) << 2;
                    }
                    BAP1_VALUES[group1 & (BAP1_VALUES.len() - 1)]
                }
                2 => {
                    group2 += 1;
                    if group2 & 3 == 3 {
                        group2 = read!(BAP_BITS[2]) << 2;
                    }
                    BAP2_VALUES[group2 & (BAP2_VALUES.len() - 1)]
                }
                3 => BAP3_TABLE[read!(BAP_BITS[3]) & (BAP3_TABLE.len() - 1)],
                4 => {
                    group4 += 1;
                    if group4 & 1 == 0 {
                        group4 = read!(BAP_BITS[4]) << 1;
                    }
                    BAP4_VALUES[group4 & (BAP4_VALUES.len() - 1)]
                }
                5 => BAP5_TABLE[read!(BAP_BITS[5]) & (BAP5_TABLE.len() - 1)],
                bap => {
                    let bits = BAP_BITS[bap as usize & (BAP_BITS.len() - 1)];
                    let raw = read!(bits) as i32;
                    *slot = scale_int32((raw << (32 - bits)) >> shift);
                    continue;
                }
            };
            *slot = (symmetric >> shift) as f32 * FROM_INT24;
        }

        let position = base + position;
        if overrun || position > reader.limit_bits() {
            return Err(ParseError::ShortPacket);
        }
        reader.set_position(position);
        *state = MantissaDecodeState {
            group1,
            group2,
            group4,
        };
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
        self.allocated_with = None;

        let mut current_exponent = absolute_exponent;
        self.exponents[start_mantissa] = current_exponent;
        // Each group sets three runs of `group_size` exponents. A group list
        // that would run past the last bin is rejected before any of it is
        // written; the state it would have half-filled is dropped with the
        // error either way.
        let written = grouped.len() * 3 * group_size;
        let Some(exponents) = self
            .exponents
            .get_mut(exponent_offset..exponent_offset + written)
        else {
            return Err(ParseError::InvalidHeader("expmant"));
        };
        // One loop per group size, so each run is plain stores and not a
        // fill of a length only known when it runs.
        macro_rules! expand {
            ($size:literal) => {
                for (&group, runs) in grouped.iter().zip(exponents.chunks_exact_mut(3 * $size)) {
                    let runs: &mut [i32; 3 * $size] = runs.try_into().unwrap();
                    let deltas = EXPONENT_GROUP_DELTAS[group as usize & 127];
                    for (delta, run) in deltas.into_iter().zip(runs.chunks_exact_mut($size)) {
                        current_exponent += delta;
                        let run: &mut [i32; $size] = run.try_into().unwrap();
                        *run = [current_exponent; $size];
                    }
                }
            };
        }
        match group_size {
            1 => expand!(1),
            2 => expand!(2),
            _ => expand!(4),
        }

        for (shift, &exponent) in self.shifts.iter_mut().zip(&self.exponents) {
            *shift = exponent.clamp(0, 31) as u8;
        }

        // An end below the start (a corrupt coupling range) writes nothing.
        let psd_end = end_mantissa.max(start_mantissa);
        for (psd, &exponent) in self.psd[start_mantissa..psd_end]
            .iter_mut()
            .zip(&self.exponents[start_mantissa..psd_end])
        {
            *psd = 3072 - (exponent << 7);
        }

        integrate_psd(
            &self.psd,
            &mut self.integrated_psd,
            start_mantissa,
            end_mantissa,
        );

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

/// Where the mantissa groups stand between two channels of a block: for each
/// grouped `bap`, the index in its flat table of the value last taken.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MantissaDecodeState {
    group1: usize,
    group2: usize,
    group4: usize,
}

impl MantissaDecodeState {
    /// Every group used up, so the first mantissa of each kind reads a code.
    pub(crate) fn new_block() -> Self {
        Self {
            group1: 2,
            group2: 2,
            group4: 1,
        }
    }
}

impl MantissaDecodeState {
    /// For each kind of group, the place in it the next mantissa of that
    /// kind takes: 0 when it starts a new one.
    fn phases(&self) -> [u8; 4] {
        [
            0,
            ((self.group1 & 3) as u8 + 1) % 3,
            ((self.group2 & 3) as u8 + 1) % 3,
            ((self.group4 & 1) as u8 + 1) % 2,
        ]
    }
}

/// What a `bap` reads its mantissa as: nothing, its own asymmetric field, one
/// of the two ungrouped symmetric fields, or a share of a group of three
/// (`bap` 1 and 2) or two (`bap` 4).
const MANTISSA_CLASS: [usize; 16] = [0, 4, 5, 2, 6, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1];
const ASYMMETRIC: usize = 1;
const SYMMETRIC3: usize = 2;
const SYMMETRIC5: usize = 3;
/// The grouped classes, in the order of [`MantissaDecodeState`]'s groups.
const GROUPED: [usize; 3] = [4, 5, 6];
const CLASSES: usize = 8;
/// Mantissas per group of a class.
const GROUP_LENGTH: [usize; CLASSES] = [1, 1, 1, 1, 3, 3, 2, 1];
/// A list has room for every bin a channel can have.
const LIST: usize = MAX_ALLOCATION_SIZE;
/// A bin no channel has, where the members a group does not have in this
/// channel are written, and wiped after.
const SPARE_BIN: usize = 255;
/// `8 * class` of a `bap`: where its class's count sits in the counts word,
/// and, times 32, where its list starts.
const CLASS_SHIFT: [u8; 16] = {
    let mut result = [0; 16];
    let mut bap = 0;
    while bap < 16 {
        result[bap] = 8 * MANTISSA_CLASS[bap] as u8;
        bap += 1;
    }
    result
};
/// The bits the mantissa at `place` in its class's list reads, by `bap << 8
/// | place`: its field for one that is not grouped, the group's code for the
/// first of a group, nothing for the others.
const FIELD_BITS: [u8; 16 << 8] = {
    let mut result = [0; 16 << 8];
    let mut bap = 0;
    while bap < 16 {
        let mut place = 0;
        while place < 256 {
            if place % GROUP_LENGTH[MANTISSA_CLASS[bap]] == 0 {
                result[bap << 8 | place] = BAP_BITS[bap] as u8;
            }
            place += 1;
        }
        bap += 1;
    }
    result
};
/// The top bits of a word an asymmetric mantissa of each `bap` takes.
const ASYMMETRIC_FIELD: [u32; 16] = {
    let mut result = [0; 16];
    let mut bap = 6;
    while bap < 16 {
        result[bap] = !0 << (32 - BAP_BITS[bap]);
        bap += 1;
    }
    result
};

/// The mantissas a `bap` range asks for, sorted by class: each as `bin | bap
/// << 8 | bit << 16`, `bit` being where the field it reads starts, counted
/// from the channel's first. A grouped mantissa that does not start its
/// group has the bit the channel had reached, which nothing reads.
///
/// A grouped class's list is laid out in groups: it starts at the place the
/// class had reached in its group when the channel began, so the group begun
/// in an earlier channel fills the first slots and every other group starts
/// on a multiple of its length. The slots a group does not fill hold the
/// spare bin. The plan is therefore for those `phases` as well as for `bap`.
#[derive(Debug, Clone)]
struct MantissaPlan {
    valid: bool,
    start: usize,
    end: usize,
    phases: [u8; 4],
    /// Whether the range is one the lists can hold.
    fits: bool,
    /// The bits the channel's mantissas take.
    bits: usize,
    /// Where each class's list ends.
    ends: [usize; CLASSES],
    lists: [u32; CLASSES * LIST],
}

impl MantissaPlan {
    fn new() -> Self {
        Self {
            valid: false,
            start: 0,
            end: 0,
            phases: [0; 4],
            fits: false,
            bits: 0,
            ends: [0; CLASSES],
            lists: [0; CLASSES * LIST],
        }
    }

    /// Sort the mantissas of `baps` into their lists. One pass with nothing
    /// in it that depends on `bap` but the index of a table: where each list
    /// has got to is a byte of `ends`, and a bin with no mantissa is written
    /// to a list nothing reads.
    fn build(&mut self, baps: &[u8], start: usize, end: usize, phases: [u8; 4]) {
        self.valid = true;
        self.start = start;
        self.end = end;
        self.phases = phases;
        // Every bin, and every place in a list, below the spare bin.
        self.fits = end <= SPARE_BIN - 2;
        if !self.fits {
            return;
        }
        let mut ends = 0u64;
        for (class, &phase) in GROUPED.iter().zip(&phases[1..]) {
            ends |= (phase as u64) << (8 * class);
        }
        let mut bit = 0usize;
        for (bin, &bap) in (start..end).zip(baps) {
            let bap = bap as usize & 15;
            let shift = CLASS_SHIFT[bap] as usize;
            let place = (ends >> shift) as usize & 0xff;
            self.lists[(shift << 5) + place] = (bin | bap << 8 | bit << 16) as u32;
            ends += 1 << shift;
            bit += FIELD_BITS[bap << 8 | place] as usize;
        }
        self.bits = bit;
        for (class, end) in self.ends.iter_mut().enumerate() {
            *end = (ends >> (8 * class)) as usize & 0xff;
        }
        for (class, &phase) in GROUPED.iter().zip(&phases[1..]) {
            let list = &mut self.lists[class * LIST..(class + 1) * LIST];
            let end = self.ends[*class];
            list[..phase as usize].fill(SPARE_BIN as u32);
            list[end..end.next_multiple_of(GROUP_LENGTH[*class])].fill(SPARE_BIN as u32);
        }
    }

    /// Decode what the plan lists from `data`, the channel's first mantissa
    /// bit being bit `base` of it. Every field must be readable as a whole
    /// word: `data` reaches eight bytes past the channel's last bit.
    fn decode(
        &self,
        data: &[u8],
        base: usize,
        shifts: &[u8; MAX_ALLOCATION_SIZE],
        target: &mut [f32; MAX_ALLOCATION_SIZE],
        state: &mut MantissaDecodeState,
    ) {
        // The bits of an entry's field, at the top of a word.
        let field = |entry: u32| -> u64 {
            let offset = base + (entry >> 16) as usize;
            let word = match data.get(offset / 8..offset / 8 + 8) {
                Some(word) => u64::from_be_bytes(word.try_into().unwrap()),
                None => 0,
            };
            word << (offset & 7)
        };
        let list = |class: usize| &self.lists[class * LIST..class * LIST + self.ends[class]];

        for &entry in list(ASYMMETRIC) {
            let (bin, bap) = (entry as usize & 0xff, (entry as usize >> 8) & 15);
            let top = (field(entry) >> 32) as u32;
            let mantissa = (top & ASYMMETRIC_FIELD[bap]) as i32;
            target[bin] = scale_int32(mantissa >> shifts[bin]);
        }
        for &entry in list(SYMMETRIC3) {
            let bin = entry as usize & 0xff;
            let code = (field(entry) >> (64 - BAP_BITS[3])) as usize;
            target[bin] = (BAP3_TABLE[code] >> shifts[bin]) as f32 * FROM_INT24;
        }
        for &entry in list(SYMMETRIC5) {
            let bin = entry as usize & 0xff;
            let code = (field(entry) >> (64 - BAP_BITS[5])) as usize;
            target[bin] = (BAP5_TABLE[code] >> shifts[bin]) as f32 * FROM_INT24;
        }

        let code = state.group1 >> 2;
        let (code, place) =
            self.decode_groups::<3, 5>(0, code, &BAP1_TABLE, &field, shifts, target);
        state.group1 = code << 2 | place.unwrap_or(state.group1 & 3);
        let code = state.group2 >> 2;
        let (code, place) =
            self.decode_groups::<3, 7>(1, code, &BAP2_TABLE, &field, shifts, target);
        state.group2 = code << 2 | place.unwrap_or(state.group2 & 3);
        let code = state.group4 >> 1;
        let (code, place) =
            self.decode_groups::<2, 7>(2, code, &BAP4_TABLE, &field, shifts, target);
        state.group4 = code << 1 | place.unwrap_or(state.group4 & 1);
        target[SPARE_BIN] = 0.0;
    }

    /// The mantissas of one grouped class, `LENGTH` to a code of `BITS` bits:
    /// the group the class was in when the channel began, whose code is
    /// `carried`, then the channel's own. Returns the code of the group the
    /// class is left in and the place last taken in it, if the channel has
    /// any mantissa of the class.
    #[inline(always)]
    fn decode_groups<const LENGTH: usize, const BITS: usize>(
        &self,
        group: usize,
        carried: usize,
        table: &[[i32; LENGTH]],
        field: &impl Fn(u32) -> u64,
        shifts: &[u8; MAX_ALLOCATION_SIZE],
        target: &mut [f32; MAX_ALLOCATION_SIZE],
    ) -> (usize, Option<usize>) {
        let class = GROUPED[group];
        let phase = self.phases[group + 1] as usize;
        let end = self.ends[class];
        // Whole groups: the build filled the last one up with the spare bin.
        let list = &self.lists[class * LIST..class * LIST + end.next_multiple_of(LENGTH)];
        let mut code = carried & (table.len() - 1);
        for (index, members) in list.chunks_exact(LENGTH).enumerate() {
            if index != 0 || phase == 0 {
                code = (field(members[0]) >> (64 - BITS)) as usize & (table.len() - 1);
            }
            for (&member, &value) in members.iter().zip(&table[code]) {
                let bin = member as usize & 0xff;
                target[bin] = (value >> shifts[bin]) as f32 * FROM_INT24;
            }
        }
        (code, (end > phase).then(|| (end - 1) % LENGTH))
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

/// The bands of more than one bin, as runs of bands of one width:
/// `(first band, first bin, bins per band, bands)`.
const BAND_RUNS: [(usize, usize, usize, usize); 4] = [
    (28, 28, 3, 7),
    (35, 49, 6, 6),
    (41, 85, 12, 4),
    (45, 133, 24, 5),
];

/// Sum the power of `start..end` into its bands (A/52 7.2.2.2). A band's sum
/// is a chain of `log_add`s, one bin after the other, and each step waits for
/// the one before; but no band waits for another, so bands of one width are
/// summed side by side.
fn integrate_psd(psd: &[i32], integrated: &mut [i32], start: usize, end: usize) {
    let (Ok(psd), Ok(integrated)) = (
        <&[i32; MAX_ALLOCATION_SIZE]>::try_from(psd),
        <&mut [i32; MASK_BANDS]>::try_from(integrated),
    ) else {
        return;
    };
    if end <= start || end > 253 {
        // A range no stream should carry: the plain walk, band by band.
        let mut bin = start;
        let mut band = MASKTAB[start];
        while let (Some(&band_end), Some(&power)) = (BNDTAB.get(band), psd.get(bin)) {
            let last_bin = band_end.min(end);
            let mut sum = power;
            bin += 1;
            while bin < last_bin {
                sum = log_add(sum, psd[bin]);
                bin += 1;
            }
            integrated[band] = sum;
            band += 1;
            if end <= last_bin {
                break;
            }
        }
        return;
    }

    // One bin per band below 28.
    let single_end = end.min(28);
    if start < single_end {
        integrated[start..single_end].copy_from_slice(&psd[start..single_end]);
    }
    for (first_band, first_bin, width, bands) in BAND_RUNS {
        let run_end = first_bin + width * bands;
        if end <= first_bin || start >= run_end {
            continue;
        }
        // Whole bands of the run inside the range, then the cut ones.
        let first_whole = if start <= first_bin {
            0
        } else {
            (start - first_bin).div_ceil(width)
        };
        let whole_end = (end.min(run_end) - first_bin) / width;
        let mut next = first_whole;
        while next < whole_end {
            let base = first_bin + next * width;
            let target = &mut integrated[first_band + next..];
            next += match whole_end - next {
                1 => break,
                2 => sum_bands::<2>(psd, base, width, target),
                3 => sum_bands::<3>(psd, base, width, target),
                4 => sum_bands::<4>(psd, base, width, target),
                5 => sum_bands::<5>(psd, base, width, target),
                6 => sum_bands::<6>(psd, base, width, target),
                _ => sum_bands::<7>(psd, base, width, target),
            };
        }
        // What is left: the bands the range cuts, and a whole one on its own.
        for band in (0..first_whole).chain(next..bands) {
            let low = (first_bin + band * width).max(start);
            let high = (first_bin + (band + 1) * width).min(end);
            if low >= high {
                continue;
            }
            let mut sum = psd[low];
            for &power in &psd[low + 1..high] {
                sum = log_add(sum, power);
            }
            integrated[first_band + band] = sum;
        }
    }
}

/// Sum `LANES` bands of `width` bins from `base` on, side by side.
#[inline(always)]
fn sum_bands<const LANES: usize>(
    psd: &[i32; MAX_ALLOCATION_SIZE],
    base: usize,
    width: usize,
    integrated: &mut [i32],
) -> usize {
    let (Some(psd), Some(integrated)) = (
        psd.get(base..base + LANES * width),
        integrated.get_mut(..LANES),
    ) else {
        return LANES;
    };
    let mut sums: [i32; LANES] = std::array::from_fn(|lane| psd[lane * width]);
    for offset in 1..width {
        for (lane, sum) in sums.iter_mut().enumerate() {
            *sum = log_add(*sum, psd[lane * width + offset]);
        }
    }
    integrated.copy_from_slice(&sums);
    LANES
}

#[inline(always)]
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

/// The `levels` symmetric quantizer values of A/52 table 7.19-7.23, in 24-bit
/// fixed point, followed by zeros: `(2^23 - 1) * (2i + 1 - levels) / levels`.
const fn quantization<const LEN: usize>(levels: i32) -> [i32; LEN] {
    let mut result = [0; LEN];
    let mut index = 0;
    while index < levels as usize && index < LEN {
        result[index] = (((1 << 23) - 1) * (2 * index as i32 + 1 - levels)) / levels;
        index += 1;
    }
    result
}

/// A grouped mantissa code split into its `GROUPS` quantizer values, most
/// significant first, for every code the field width can carry (codes past
/// `levels^GROUPS` included: they decode the way the arithmetic says).
const fn grouped_quantization<const GROUPS: usize, const LEN: usize>(
    levels: i32,
) -> [[i32; GROUPS]; LEN] {
    let source = quantization::<16>(levels);
    let mut result = [[0; GROUPS]; LEN];
    let mut code = 0;
    while code < LEN {
        let mut grouped = code;
        let mut slot = GROUPS;
        while slot > 0 {
            slot -= 1;
            result[code][slot] = source[grouped % levels as usize];
            grouped /= levels as usize;
        }
        code += 1;
    }
    result
}

const BAP1_TABLE: [[i32; 3]; 1 << BAP_BITS[1]] = grouped_quantization(3);
const BAP2_TABLE: [[i32; 3]; 1 << BAP_BITS[2]] = grouped_quantization(5);
const BAP3_TABLE: [i32; 1 << BAP_BITS[3]] = quantization(7);
/// The grouped tables flat, a group of three in a row of four: the value at
/// `position` of the group `code` is at `code * row + position`.
const BAP1_VALUES: [i32; 4 << BAP_BITS[1]] = flat_rows(BAP1_TABLE);
const BAP2_VALUES: [i32; 4 << BAP_BITS[2]] = flat_rows(BAP2_TABLE);
const BAP4_VALUES: [i32; 2 << BAP_BITS[4]] = flat_rows(BAP4_TABLE);

const fn flat_rows<const GROUPS: usize, const LEN: usize, const FLAT: usize>(
    table: [[i32; GROUPS]; LEN],
) -> [i32; FLAT] {
    let row = FLAT / LEN;
    let mut result = [0; FLAT];
    let mut code = 0;
    while code < LEN {
        let mut position = 0;
        while position < GROUPS {
            result[code * row + position] = table[code][position];
            position += 1;
        }
        code += 1;
    }
    result
}
const BAP4_TABLE: [[i32; 2]; 1 << BAP_BITS[4]] = grouped_quantization(11);
const BAP5_TABLE: [i32; 1 << BAP_BITS[5]] = quantization(15);

/// Exponent deltas of a 7-bit group, `[g / 25 - 2, (g % 25) / 5 - 2, g % 5 - 2]`
/// (A/52 7.1.3), for every value the field can carry.
const EXPONENT_GROUP_DELTAS: [[i32; 3]; 128] = {
    let mut result = [[0; 3]; 128];
    let mut group = 0;
    while group < 128 {
        result[group] = [
            group as i32 / 25 - 2,
            (group as i32 % 25) / 5 - 2,
            group as i32 % 5 - 2,
        ];
        group += 1;
    }
    result
};

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The const tables against the runtime generators they replaced.
    #[test]
    fn const_quantization_tables_match_the_generated_ones() {
        assert_eq!(
            BAP1_TABLE.to_vec(),
            generate_grouped_quantization::<3>(3, BAP_BITS[1])
        );
        assert_eq!(
            BAP2_TABLE.to_vec(),
            generate_grouped_quantization::<3>(5, BAP_BITS[2])
        );
        assert_eq!(BAP3_TABLE.to_vec(), generate_quantization(7));
        assert_eq!(
            BAP4_TABLE.to_vec(),
            generate_grouped_quantization::<2>(11, BAP_BITS[4])
        );
        assert_eq!(BAP5_TABLE.to_vec(), generate_quantization(15));
    }

    /// xorshift64, for inputs that only have to be varied.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: u64) -> usize {
            ((self.next() >> 20) % bound) as usize
        }
    }

    /// Every range a channel or the coupling channel can have, against the
    /// walk the standard describes: one band after the other.
    #[test]
    fn band_sums_match_the_plain_walk() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..4 {
            let psd: Vec<i32> = (0..MAX_ALLOCATION_SIZE)
                .map(|_| 3072 - ((rng.below(25) as i32) << 7))
                .collect();
            for start in 0..253 {
                for end in start + 1..=253 {
                    let mut plain = [i32::MIN; MASK_BANDS];
                    let mut bin = start;
                    let mut band = MASKTAB[start];
                    loop {
                        let last_bin = BNDTAB[band].min(end);
                        plain[band] = psd[bin];
                        bin += 1;
                        while bin < last_bin {
                            plain[band] = log_add(plain[band], psd[bin]);
                            bin += 1;
                        }
                        band += 1;
                        if end <= last_bin {
                            break;
                        }
                    }
                    let mut summed = [i32::MIN; MASK_BANDS];
                    integrate_psd(&psd, &mut summed, start, end);
                    assert_eq!(summed, plain, "start={start} end={end}");
                }
            }
        }
    }

    /// The mantissa loop as it was first written: one checked read per field,
    /// the groups unpacked into their values.
    struct PlainMantissas {
        bap1_pos: usize,
        bap2_pos: usize,
        bap4_pos: usize,
        bap1_next: [i32; 3],
        bap2_next: [i32; 3],
        bap4_next: [i32; 2],
    }

    impl PlainMantissas {
        fn new_block() -> Self {
            Self {
                bap1_pos: 2,
                bap2_pos: 2,
                bap4_pos: 1,
                bap1_next: [0; 3],
                bap2_next: [0; 3],
                bap4_next: [0; 2],
            }
        }

        fn decode(
            &mut self,
            reader: &mut BitReader<'_>,
            baps: &[u8],
            exponents: &[i32],
        ) -> Option<Vec<f32>> {
            let mut result = Vec::with_capacity(baps.len());
            for (&bap, &exponent) in baps.iter().zip(exponents) {
                result.push(match bap {
                    0 => 0.0,
                    1 => {
                        self.bap1_pos += 1;
                        if self.bap1_pos == 3 {
                            self.bap1_next = BAP1_TABLE[reader.read_bits(5)? as usize];
                            self.bap1_pos = 0;
                        }
                        scale_int24(self.bap1_next[self.bap1_pos], exponent)
                    }
                    2 => {
                        self.bap2_pos += 1;
                        if self.bap2_pos == 3 {
                            self.bap2_next = BAP2_TABLE[reader.read_bits(7)? as usize];
                            self.bap2_pos = 0;
                        }
                        scale_int24(self.bap2_next[self.bap2_pos], exponent)
                    }
                    3 => scale_int24(BAP3_TABLE[reader.read_bits(3)? as usize], exponent),
                    4 => {
                        self.bap4_pos += 1;
                        if self.bap4_pos == 2 {
                            self.bap4_next = BAP4_TABLE[reader.read_bits(7)? as usize];
                            self.bap4_pos = 0;
                        }
                        scale_int24(self.bap4_next[self.bap4_pos], exponent)
                    }
                    5 => scale_int24(BAP5_TABLE[reader.read_bits(4)? as usize], exponent),
                    bap => {
                        let bits = BAP_BITS[bap as usize];
                        let raw = reader.read_bits(bits)? as i32;
                        scale_int32(shift_right_signed(raw << (32 - bits), exponent))
                    }
                });
            }
            Some(result)
        }
    }

    use super::super::bitstream::BitReader;

    /// Channels of a block one after the other, each inheriting the groups
    /// the one before left open - the same `bap` met at every place in a
    /// group, and met again at a place it was already planned for - over data
    /// that ends anywhere from well past the mantissas (whole-word reads) to
    /// inside them (a short packet), and from every bit offset.
    #[test]
    fn mantissas_match_the_plain_reads() {
        const CHANNELS: usize = 5;
        let mut rng = Rng(0x0123_4567_89ab_cdef);
        for round in 0..400 {
            let mut allocation = AllocationState::new();
            let (start, end) = match round % 4 {
                0 => (0, 1 + rng.below(253)),
                1 => (37 + 12 * rng.below(8), 133 + rng.below(120)),
                2 => (0, 7),
                // The longest range a channel has, and one past what a plan
                // lists, which is decoded in order.
                _ => (0, [253, 256][rng.below(2)]),
            };
            // Runs of one `bap`, as a spectrum has them, with every value in.
            let mut bap = rng.below(16) as u8;
            for bin in 0..MAX_ALLOCATION_SIZE {
                if rng.below(3) == 0 {
                    bap = match rng.below(4) {
                        0 => 0,
                        1 => 1 + rng.below(5) as u8,
                        _ => rng.below(16) as u8,
                    };
                }
                allocation.bap[bin] = bap;
                allocation.exponents[bin] = rng.below(25) as i32;
                allocation.shifts[bin] = allocation.exponents[bin] as u8;
            }
            allocation.forget_plan();

            let needed = CHANNELS * MAX_MANTISSA_BITS * (end - start) / 8 + 2;
            let length = match round % 5 {
                0 => needed + 600,
                1 => needed,
                _ => 1 + rng.below(needed as u64),
            };
            let data: Vec<u8> = (0..length).map(|_| rng.next() as u8).collect();
            let offset = rng.below(8).min(length * 8);

            let mut plain_reader = BitReader::with_offset(&data, offset);
            let mut plain = PlainMantissas::new_block();
            let mut reader = BitReader::with_offset(&data, offset);
            let mut state = MantissaDecodeState::new_block();
            for channel in 0..CHANNELS {
                let expected = plain.decode(
                    &mut plain_reader,
                    &allocation.bap[start..end],
                    &allocation.exponents[start..end],
                );
                let mut target = [f32::NAN; MAX_ALLOCATION_SIZE];
                let decoded = allocation.decode_transform_coeffs(
                    &mut reader,
                    &mut target,
                    start,
                    end,
                    &mut state,
                );
                let Some(expected) = expected else {
                    assert!(decoded.is_err(), "round {round} channel {channel}");
                    break;
                };
                assert!(decoded.is_ok(), "round {round} channel {channel}");
                assert_eq!(reader.position(), plain_reader.position(), "round {round}");
                let bits = |values: &[f32]| values.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
                assert_eq!(bits(&target[start..end]), bits(&expected), "round {round}");
                assert!(
                    target[..start]
                        .iter()
                        .chain(&target[end..])
                        .all(|v| v.to_bits() == 0)
                );
            }
        }
    }

    /// A channel with no bits allocated reads nothing and leaves the groups
    /// of the block as they are.
    #[test]
    fn a_cleared_allocation_reads_no_mantissa() {
        let data = [0xa5u8; 64];
        let mut allocation = AllocationState::new();
        allocation.bap.fill(7);
        allocation.forget_plan();
        let mut reader = BitReader::with_offset(&data, 3);
        let mut state = MantissaDecodeState::new_block();
        let mut target = [f32::NAN; MAX_ALLOCATION_SIZE];
        allocation
            .decode_transform_coeffs(&mut reader, &mut target, 0, 20, &mut state)
            .unwrap();
        assert_eq!(reader.position(), 3 + 20 * BAP_BITS[7]);

        allocation.clear_bap();
        let before = reader.position();
        let mut target = [f32::NAN; MAX_ALLOCATION_SIZE];
        allocation
            .decode_transform_coeffs(&mut reader, &mut target, 0, 20, &mut state)
            .unwrap();
        assert_eq!(reader.position(), before);
        assert!(target.iter().all(|value| value.to_bits() == 0));
    }

    /// Each strategy's runs against the running sum they stand for.
    #[test]
    fn exponent_groups_expand_to_their_runs() {
        let mut rng = Rng(0xfeed_face_cafe_beef);
        for (strategy, size) in [
            (ExpStrategy::D15, 1),
            (ExpStrategy::D25, 2),
            (ExpStrategy::D45, 4),
        ] {
            for _ in 0..50 {
                let groups: Vec<i32> = (0..1 + rng.below(20))
                    .map(|_| rng.below(125) as i32)
                    .collect();
                let end = 1 + groups.len() * 3 * size;
                let mut allocation = AllocationState::new();
                allocation
                    .decode_grouped_exponents(strategy, 0, 1, end, 12, &groups)
                    .unwrap();
                let mut expected = vec![12];
                let mut current = 12;
                for group in &groups {
                    for delta in [group / 25 - 2, (group % 25) / 5 - 2, group % 5 - 2] {
                        current += delta;
                        expected.extend(std::iter::repeat_n(current, size));
                    }
                }
                assert_eq!(&allocation.exponents[..end], &expected[..]);
                for (shift, exponent) in allocation.shifts[..end].iter().zip(&expected) {
                    assert_eq!(*shift as i32, (*exponent).clamp(0, 31));
                }
            }
        }
    }

    #[test]
    fn exponent_group_deltas_match_the_arithmetic() {
        for group in 0..128i32 {
            assert_eq!(
                EXPONENT_GROUP_DELTAS[group as usize],
                [group / 25 - 2, (group % 25) / 5 - 2, group % 5 - 2]
            );
        }
    }
}
