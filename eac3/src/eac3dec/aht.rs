// SPDX-License-Identifier: Apache-2.0

//! Adaptive Hybrid Transform (AHT) mantissa decoding for E-AC-3
//! (ETSI TS 102 366 Annex E, clause E.2.4).
//!
//! An AHT channel carries all six blocks' mantissas in block 0: per bin,
//! either one vector-quantised index (hebap 1 - 7, clause E.2.4.4.1) or six
//! scalar / gain-adaptive codewords (hebap 8 - 19, clause E.2.4.4.2). The
//! six values are the bin's DCT-II spectrum across the blocks; a 6-point
//! inverse DCT (clause E.2.4.5) turns them back into one MDCT mantissa per
//! block.

use super::aht_tables::{
    GAQ_REMAP, HEBAP_MANTISSA_BITS, VQ_HEBAP1, VQ_HEBAP2, VQ_HEBAP3, VQ_HEBAP4, VQ_HEBAP5,
    VQ_HEBAP6, VQ_HEBAP7,
};
use super::bitstream::BitReader;
use super::syncframe::ParseError;

/// Most bins one AHT channel can carry (the transform length).
const MAX_BINS: usize = 256;

/// Largest valid hebap (Table E.2.1).
const MAX_HEBAP: u8 = 19;

/// First hebap coded with scalar / gain-adaptive quantisation (Table E.2.2).
const FIRST_SCALAR_HEBAP: u8 = 8;

/// Bits `BitReader::word_at` guarantees at the top of its word.
const WORD_AT_BITS: usize = 57;

/// Fractional bits of the output mantissas (and of every intermediate
/// mantissa value below): `x` is held as `x * 2^23`.
const FRACTION_BITS: u32 = 23;

/// The VQ codebooks of clause E.3 (Tables E.3.1 - E.3.7), indexed by hebap,
/// each row already through the inverse DCT of clause E.2.4.5: a VQ bin's
/// six MDCT mantissas are a fixed function of its index, so the transform
/// is done once, at compile time. Each codebook has exactly
/// `2^HEBAP_MANTISSA_BITS[hebap]` rows, so every index the syntax can carry
/// addresses a row.
static VQ_CODEBOOKS: [&[[i32; 6]]; 8] = [
    &[],
    &transformed_codebook(&VQ_HEBAP1),
    &transformed_codebook(&VQ_HEBAP2),
    &transformed_codebook(&VQ_HEBAP3),
    &transformed_codebook(&VQ_HEBAP4),
    &transformed_codebook(&VQ_HEBAP5),
    &transformed_codebook(&VQ_HEBAP6),
    &transformed_codebook(&VQ_HEBAP7),
];

/// A codebook's rows (16-bit two's complement fractions, the AHT spectrum
/// X(k, j) of one bin) as MDCT mantissas times `2^23`.
const fn transformed_codebook<const ROWS: usize>(rows: &[[u16; 6]; ROWS]) -> [[i32; 6]; ROWS] {
    let mut table = [[0i32; 6]; ROWS];
    let mut row = 0;
    while row < ROWS {
        let mut spectrum = [0i32; 6];
        let mut j = 0;
        while j < 6 {
            spectrum[j] = (rows[row][j] as i16 as i32) << (FRACTION_BITS - 15);
            j += 1;
        }
        table[row] = inverse_dct(&spectrum);
        row += 1;
    }
    table
}

/// One scalar mantissa quantiser of clause E.2.4.4.2 (Table E.2.5) for one
/// hebap and one gain, in the form the decode loop uses.
///
/// A mantissa starts with a `small_bits` codeword. If that codeword equals
/// `tag` (the most negative two's complement pattern, clause E.2.4.4.2) a
/// `large_bits` codeword follows and gives the value; otherwise the first
/// codeword is the value. A codeword `c` of width `w` is the fraction
/// `x = c / 2^(w - 1)`, and the reconstructed value is `y = x * (1 + a) + b`
/// (Table E.2.6, `a` and `b` in 16-bit fractions) for the Gk = 1 quantiser
/// and the large codewords, `y = x / Gk` for small codewords. Each `scale`
/// below folds `(1 + a)` (or `1 / Gk`) and the codeword's `2^-(w - 1)` into
/// one multiplier, so `y * 2^23 = (c * scale + 2^14) >> 15 + b`.
#[derive(Clone, Copy)]
struct ScalarQuantizer {
    small_bits: u32,
    /// Never matches for a quantiser without large mantissas (Gk = 1).
    tag: u64,
    small_scale: i64,
    large_bits: u32,
    large_scale: i64,
    /// `b * 2^23` for a non-negative and for a negative large codeword.
    large_offset_positive: i64,
    large_offset_negative: i64,
}

/// `tag` of a quantiser that has no large mantissas: no codeword of at most
/// 16 bits equals it.
const NO_TAG: u64 = u64::MAX;

impl ScalarQuantizer {
    /// Table E.2.6 constant as a Q15 fraction (16-bit two's complement).
    const fn q15(value: u16) -> i64 {
        value as i16 as i64
    }

    /// Multiplier turning a `bits`-wide codeword into `value * 2^23` after the
    /// final `>> 15`, for a Q15 gain `gain_q15` applied to the codeword's
    /// fraction `c / 2^(bits - 1)`.
    const fn scale(gain_q15: i64, bits: u32) -> i64 {
        gain_q15 << (FRACTION_BITS + 1 - bits)
    }

    /// The quantiser for `hebap` (8 - 19) and mapped gain `gain` (0, 1, 2 for
    /// Gk = 1, 2, 4; Table E.2.4), from Tables E.2.2, E.2.5 and E.2.6.
    /// Gains 1 and 2 only exist for hebap 8 - 16 (Table E.2.3); above, the
    /// Gk = 1 quantiser is returned for them.
    const fn new(hebap: usize, gain: usize) -> Self {
        let m = HEBAP_MANTISSA_BITS[hebap] as u32;
        let remap = GAQ_REMAP[hebap - FIRST_SCALAR_HEBAP as usize];
        let one = 1 << 15;
        if gain == 0 || hebap > 16 {
            // Gk = 1: one m-bit codeword, 2^m - 1 levels, symmetric through
            // the remap of Table E.2.6 (b = 0).
            let [a, _, _] = remap[0];
            return Self {
                small_bits: m,
                tag: NO_TAG,
                small_scale: Self::scale(one + Self::q15(a), m),
                large_bits: 1,
                large_scale: 0,
                large_offset_positive: 0,
                large_offset_negative: 0,
            };
        }
        let [a, b_positive, b_negative] = remap[gain];
        // Table E.2.5. Gk = 2: small and large codewords of m - 1 bits;
        // Gk = 4: small codeword of m - 2 bits, large codeword of m bits.
        let (small_bits, large_bits) = if gain == 1 {
            (m - 1, m - 1)
        } else {
            (m - 2, m)
        };
        Self {
            small_bits,
            tag: 1 << (small_bits - 1),
            // Small mantissas are attenuated by 1 / Gk (Figure E.2.1).
            small_scale: Self::scale(one >> gain, small_bits),
            large_bits,
            large_scale: Self::scale(one + Self::q15(a), large_bits),
            large_offset_positive: Self::q15(b_positive) << (FRACTION_BITS - 15),
            large_offset_negative: Self::q15(b_negative) << (FRACTION_BITS - 15),
        }
    }
}

/// Every scalar quantiser, indexed `[hebap - 8][mapped gain]`.
static SCALAR_QUANTIZERS: [[ScalarQuantizer; 3]; 12] = {
    let mut table = [[ScalarQuantizer::new(8, 0); 3]; 12];
    let mut row = 0;
    while row < 12 {
        let mut gain = 0;
        while gain < 3 {
            table[row][gain] = ScalarQuantizer::new(row + FIRST_SCALAR_HEBAP as usize, gain);
            gain += 1;
        }
        row += 1;
    }
    table
};

/// Decode one channel's AHT payload (block 0 of the frame) into per-block
/// transform coefficient mantissas for bins `start..end`.
pub(super) fn decode_pre_mantissas(
    reader: &mut BitReader<'_>,
    hebap: &[u8],
    start: usize,
    end: usize,
    pre_mantissas: &mut [[i32; 6]],
) -> Result<(), ParseError> {
    let (Some(hebap), Some(output)) = (hebap.get(start..end), pre_mantissas.get_mut(start..end))
    else {
        return Err(ParseError::InvalidHeader("aht-range"));
    };
    if hebap.len() > MAX_BINS {
        return Err(ParseError::InvalidHeader("aht-range"));
    }

    // gaqmod (clauses E.1.3.3.27 / 30 / 33), then the helper variables of
    // clause E.2.4.2: bins with 8 <= hebap < endbap carry a gain word (the
    // active GAQ bins); endbap is 12 for gaqmod 0 and 1, 17 for 2 and 3.
    let gaqmod = reader.read_bits(2).ok_or(ParseError::ShortPacket)?;
    let endbap = if gaqmod < 2 { 12 } else { 17 };
    let mut active_bins = 0usize;
    for &bin_hebap in hebap {
        if bin_hebap > MAX_HEBAP {
            return Err(ParseError::InvalidHeader("aht-hebap"));
        }
        active_bins += usize::from((FIRST_SCALAR_HEBAP..endbap).contains(&bin_hebap));
    }

    // The gain words (gaqsections of them, clause E.2.4.2), unpacked to one
    // mapped gain per active bin, in bin order: 0, 1, 2 for Gk = 1, 2, 4
    // (Table E.2.4). gaqmod 0 sends none (all Gk = 1); gaqmod 1 and 2 send
    // one bit per active bin, 1 meaning Gk = 2 or Gk = 4 respectively;
    // gaqmod 3 sends 5-bit words packing three gains (clause E.2.4.4.2).
    let mut gains = [0u8; MAX_BINS];
    let gains = &mut gains[..active_bins];
    match gaqmod {
        1 | 2 => {
            let raised = gaqmod as u8;
            for gain in gains.iter_mut() {
                if reader.read_bit().ok_or(ParseError::ShortPacket)? {
                    *gain = raised;
                }
            }
        }
        3 => {
            for triplet in gains.chunks_mut(3) {
                let grpgain = reader.read_bits(5).ok_or(ParseError::ShortPacket)? as u8;
                // Three 3-state gains code at most 26.
                if grpgain >= 27 {
                    return Err(ParseError::InvalidHeader("aht-gaq-gain"));
                }
                let mapped = [grpgain / 9, (grpgain % 9) / 3, grpgain % 3];
                for (gain, value) in triplet.iter_mut().zip(mapped) {
                    *gain = value;
                }
            }
        }
        _ => {}
    }

    // The pre-mantissas (clause E.1.2.4), bin by bin. The loop keeps its own
    // bit position and checks it against the limit once at the end: every
    // field inside it is at most 20 bits (a Gk = 2 or 4 tag plus its large
    // codeword), well within one `word_at` word, which reads zeros past the
    // data.
    let data = reader.data();
    let mut position = reader.position();
    let mut next_gain = 0usize;
    for (&bin_hebap, mantissas) in hebap.iter().zip(output.iter_mut()) {
        *mantissas = if bin_hebap >= FIRST_SCALAR_HEBAP {
            // Six scalar / GAQ mantissas, one per AHT transform index j,
            // then the inverse DCT.
            let gain = if bin_hebap < endbap {
                let gain = gains.get(next_gain).copied().unwrap_or(0);
                next_gain += 1;
                gain
            } else {
                0
            };
            let quantizer =
                &SCALAR_QUANTIZERS[usize::from(bin_hebap - FIRST_SCALAR_HEBAP)][usize::from(gain)];
            // One loaded word serves several mantissas: it is refilled only
            // when fewer bits are left in it than the longest mantissa.
            let longest = (quantizer.small_bits + quantizer.large_bits) as usize;
            let mut word = BitReader::word_at(data, position);
            let mut available = WORD_AT_BITS;
            let mut spectrum = [0i32; 6];
            for value in &mut spectrum {
                if available < longest {
                    word = BitReader::word_at(data, position);
                    available = WORD_AT_BITS;
                }
                let (decoded, bits) = decode_scalar(quantizer, word);
                *value = decoded;
                position += bits;
                word <<= bits;
                available -= bits;
            }
            inverse_dct(&spectrum)
        } else if bin_hebap > 0 {
            // One VQ index for the six values (clause E.2.4.4.1), selecting a
            // row of the hebap's (already transformed) codebook.
            let bits = u32::from(HEBAP_MANTISSA_BITS[usize::from(bin_hebap)]);
            let index = (BitReader::word_at(data, position) >> (64 - bits)) as usize;
            position += bits as usize;
            VQ_CODEBOOKS[usize::from(bin_hebap)]
                .get(index)
                .copied()
                .unwrap_or([0; 6])
        } else {
            // hebap 0: no bits, and exact zeros in every block (no dither).
            [0; 6]
        };
    }
    if position > reader.limit_bits() {
        return Err(ParseError::ShortPacket);
    }
    reader.set_position(position);
    Ok(())
}

/// Decode one scalar / GAQ mantissa from the bits at the top of `word`:
/// its value times `2^23`, and the bits it took.
#[inline(always)]
fn decode_scalar(quantizer: &ScalarQuantizer, word: u64) -> (i32, usize) {
    const ROUND: i64 = 1 << 14;
    let small_bits = quantizer.small_bits;
    if word >> (64 - small_bits) == quantizer.tag {
        // Large mantissa: the codeword after the tag, remapped with the sign
        // of x deciding b (Table E.2.6).
        let large_bits = quantizer.large_bits;
        let code = ((word << small_bits) as i64) >> (64 - large_bits);
        let offset = if code < 0 {
            quantizer.large_offset_negative
        } else {
            quantizer.large_offset_positive
        };
        let value = ((code * quantizer.large_scale + ROUND) >> 15) + offset;
        (value as i32, (small_bits + large_bits) as usize)
    } else {
        let code = (word as i64) >> (64 - small_bits);
        let value = (code * quantizer.small_scale + ROUND) >> 15;
        (value as i32, small_bits as usize)
    }
}

/// `sqrt(2) / 2`, `sqrt(6) / 2` and `sqrt(3) / 2` with 30 fractional bits.
const HALF_SQRT2_Q30: i64 = 759_250_125;
const HALF_SQRT6_Q30: i64 = 1_315_059_792;
const HALF_SQRT3_Q30: i64 = 929_887_697;

/// The inverse DCT of clause E.2.4.5 for one bin, on values times `2^23`:
///
/// `C(m) = sqrt(2) * sum_j R_j X(j) cos(j (2m + 1) pi / 12)`, m = 0..5,
/// with `R_0 = 1 / sqrt(2)` and `R_j = 1` otherwise.
///
/// Evaluated with the symmetry `cos(j (2(5 - m) + 1) pi / 12) =
/// (-1)^j cos(j (2m + 1) pi / 12)`: with `E(m)` the even-j part and `O(m)`
/// the odd-j part, `C(m) = E(m) + O(m)` and `C(5 - m) = E(m) - O(m)` for
/// m = 0, 1, 2. Writing out the cosines (multiples of pi / 12):
///
/// - `E(0) = X0 + (sqrt(6)/2) X2 + (sqrt(2)/2) X4`, `E(1) = X0 - sqrt(2) X4`,
///   `E(2) = X0 - (sqrt(6)/2) X2 + (sqrt(2)/2) X4`;
/// - `O(0) = (sqrt(3)/2)(X1 + X5) + (X1 - X5)/2 + X3`, `O(1) = X1 - X3 - X5`,
///   `O(2) = (sqrt(3)/2)(X1 + X5) - (X1 - X5)/2 - X3`;
///
/// three constant multiplies per bin. Intermediate sums carry 30 extra
/// fractional bits and are rounded once.
#[inline(always)]
const fn inverse_dct(x: &[i32; 6]) -> [i32; 6] {
    const SHIFT: u32 = 30;
    let [x0, x1, x2, x3, x4, x5] = [
        x[0] as i64,
        x[1] as i64,
        x[2] as i64,
        x[3] as i64,
        x[4] as i64,
        x[5] as i64,
    ];

    // Every output contains X0 once, so its term also carries the rounding
    // bias of the final shift.
    let dc = (x0 << SHIFT) + (1 << (SHIFT - 1));
    let x4_term = x4 * HALF_SQRT2_Q30;
    let x2_term = x2 * HALF_SQRT6_Q30;
    let even_outer = dc + x4_term;
    let even = [even_outer + x2_term, dc - 2 * x4_term, even_outer - x2_term];

    let difference = x1 - x5;
    let sum_term = (x1 + x5) * HALF_SQRT3_Q30;
    let odd_outer = (difference << (SHIFT - 1)) + (x3 << SHIFT);
    let odd = [
        sum_term + odd_outer,
        (difference - x3) << SHIFT,
        sum_term - odd_outer,
    ];

    [
        ((even[0] + odd[0]) >> SHIFT) as i32,
        ((even[1] + odd[1]) >> SHIFT) as i32,
        ((even[2] + odd[2]) >> SHIFT) as i32,
        ((even[2] - odd[2]) >> SHIFT) as i32,
        ((even[1] - odd[1]) >> SHIFT) as i32,
        ((even[0] - odd[0]) >> SHIFT) as i32,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: f64 = (1u32 << FRACTION_BITS) as f64;

    /// The clause E.2.4.5 formula, evaluated directly.
    fn direct_inverse_dct(x: &[f64; 6]) -> [f64; 6] {
        std::array::from_fn(|m| {
            let sum: f64 = (0..6)
                .map(|j| {
                    let r = if j == 0 {
                        std::f64::consts::FRAC_1_SQRT_2
                    } else {
                        1.0
                    };
                    let angle = (j * (2 * m + 1)) as f64 * std::f64::consts::PI / 12.0;
                    r * x[j] * angle.cos()
                })
                .sum();
            std::f64::consts::SQRT_2 * sum
        })
    }

    #[test]
    fn fast_inverse_dct_matches_the_formula() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..10_000 {
            let x: [i32; 6] = std::array::from_fn(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                ((seed >> 40) as i32) - (1 << 23)
            });
            let got = inverse_dct(&x);
            let want = direct_inverse_dct(&x.map(f64::from));
            for (g, w) in got.iter().zip(want) {
                assert!(
                    (f64::from(*g) - w).abs() <= 1.0,
                    "{x:?}: {got:?} vs {want:?}"
                );
            }
        }
    }

    #[test]
    fn every_codebook_has_a_row_per_index() {
        for hebap in 1..8 {
            let rows = VQ_CODEBOOKS[hebap].len();
            assert_eq!(rows, 1 << HEBAP_MANTISSA_BITS[hebap], "hebap {hebap}");
        }
    }

    /// Value of `code` (a `bits`-wide codeword, sign-extended) through the
    /// small or the large path of `quantizer`, as a fraction.
    fn value(quantizer: &ScalarQuantizer, large: bool, code: i64) -> f64 {
        let word = if large {
            (quantizer.tag << (64 - quantizer.small_bits))
                | (((code as u64) << (64 - quantizer.large_bits)) >> quantizer.small_bits)
        } else {
            (code as u64) << (64 - quantizer.small_bits)
        };
        let (decoded, bits) = decode_scalar(quantizer, word);
        let expected_bits = quantizer.small_bits + if large { quantizer.large_bits } else { 0 };
        assert_eq!(bits as u32, expected_bits);
        f64::from(decoded) / ONE
    }

    /// The Table E.2.6 constants must give the reconstruction levels and step
    /// sizes of Table E.2.5: this cross-checks their transcription.
    #[test]
    fn remapped_quantizers_have_the_step_sizes_of_table_e_2_5() {
        // 16-bit constants: allow a couple of Q15 steps of error.
        let tolerance = 3.0 / 32768.0;
        for hebap in 8..=19usize {
            let m = u32::from(HEBAP_MANTISSA_BITS[hebap]);
            // Gk = 1: levels 2c / (2^m - 1) for c in -(2^(m-1) - 1) ..= 2^(m-1) - 1.
            let q = ScalarQuantizer::new(hebap, 0);
            let half = 1i64 << (m - 1);
            for code in -(half - 1)..half {
                let want = 2.0 * code as f64 / ((1u64 << m) - 1) as f64;
                let got = value(&q, false, code);
                assert!(
                    (got - want).abs() <= tolerance,
                    "hebap {hebap} Gk 1 code {code}: {got} vs {want}"
                );
            }
            if hebap > 16 {
                continue;
            }
            for (gain, divisor) in [(1usize, 2.0f64), (2, 4.0)] {
                let q = ScalarQuantizer::new(hebap, gain);
                // Small: step 1 / 2^(m-1), no tag pattern.
                let small_half = 1i64 << (q.small_bits - 1);
                for code in -(small_half - 1)..small_half {
                    let want = code as f64 / small_half as f64 / divisor;
                    assert_eq!(
                        value(&q, false, code),
                        want,
                        "hebap {hebap} gain {gain} small {code}"
                    );
                }
                // Large: 2^large_bits levels, symmetric about zero, evenly
                // spaced with the step of Table E.2.5, outside the small range.
                let step = if gain == 1 {
                    1.0 / ((1u64 << (m - 1)) - 1) as f64
                } else {
                    3.0 / ((1u64 << (m + 1)) - 2) as f64
                };
                let large_half = 1i64 << (q.large_bits - 1);
                for code in -large_half..large_half {
                    let got = value(&q, true, code);
                    let mirror = value(&q, true, -1 - code);
                    assert!(
                        (got + mirror).abs() <= tolerance,
                        "hebap {hebap} gain {gain} large {code}"
                    );
                    if code + 1 < large_half && code != -1 {
                        let next = value(&q, true, code + 1);
                        assert!(
                            ((next - got) - step).abs() <= tolerance,
                            "hebap {hebap} gain {gain} step at {code}"
                        );
                    }
                }
                assert!(value(&q, true, 0) > (small_half - 1) as f64 / small_half as f64 / divisor);
            }
        }
    }

    /// Bits written most significant first.
    struct Bits(Vec<u8>, usize);

    impl Bits {
        fn new() -> Self {
            Self(Vec::new(), 0)
        }
        fn put(&mut self, value: u32, bits: usize) {
            for i in (0..bits).rev() {
                if self.1 % 8 == 0 {
                    self.0.push(0);
                }
                if (value >> i) & 1 != 0 {
                    *self.0.last_mut().unwrap() |= 0x80 >> (self.1 % 8);
                }
                self.1 += 1;
            }
        }
    }

    #[test]
    fn decodes_vq_and_gaq_bins_and_consumes_exactly_the_payload() {
        // gaqmod 3; bins: hebap 0, 4 (VQ), 9, 9, 13, 18.
        let hebap = [0u8, 4, 9, 9, 13, 18];
        let mut bits = Bits::new();
        bits.put(3, 2);
        // Active bins: the two hebap 9 and the hebap 13 one: one triplet,
        // gains Gk = 2, 4, 1 -> mapped 1, 2, 0 -> 1 * 9 + 2 * 3 + 0 = 15.
        bits.put(15, 5);
        // hebap 4: VQ index 31 (5 bits).
        bits.put(31, 5);
        // hebap 9 (m = 4), Gk = 2: small codewords of 3 bits, tag 0b100,
        // large codewords of 3 bits. Small 1, tag + large 3, then four zeros.
        bits.put(0b001, 3);
        bits.put(0b100, 3);
        bits.put(0b011, 3);
        for _ in 0..4 {
            bits.put(0, 3);
        }
        // hebap 9, Gk = 4: small of 2 bits (tag 0b10), large of 4 bits.
        bits.put(0b10, 2);
        bits.put(0b1000, 4);
        for _ in 0..5 {
            bits.put(0b11, 2);
        }
        // hebap 13 (m = 8), Gk = 1: six 8-bit codewords.
        for code in [0x7fu32, 0x81, 0, 0, 0, 0] {
            bits.put(code, 8);
        }
        // hebap 18 (m = 14), no gain: six 14-bit codewords.
        for _ in 0..6 {
            bits.put(0x1000, 14);
        }
        let payload_bits = bits.1;
        bits.put(0b1011, 4); // trailing data, not ours
        let mut reader = BitReader::new(&bits.0);
        let mut out = vec![[7i32; 6]; hebap.len()];
        decode_pre_mantissas(&mut reader, &hebap, 0, hebap.len(), &mut out).unwrap();
        assert_eq!(reader.position(), payload_bits);
        assert_eq!(out[0], [0; 6]);

        let check = |bin: usize, spectrum: [f64; 6]| {
            let want = direct_inverse_dct(&spectrum.map(|v| v * ONE));
            for (g, w) in out[bin].iter().zip(want) {
                assert!(
                    (f64::from(*g) - w).abs() <= 64.0,
                    "bin {bin}: {:?} vs {want:?}",
                    out[bin]
                );
            }
        };
        check(1, VQ_HEBAP4[31].map(|v| f64::from(v as i16) / 32768.0));
        let gk2_large_three = 0.75 * (1.0 + (0xc925u16 as i16) as f64 / 32768.0) + 0.5;
        check(2, [0.25 / 2.0, gk2_large_three, 0.0, 0.0, 0.0, 0.0]);
        let gk4_large_min = -1.0 * (1.0 + (0xe666u16 as i16) as f64 / 32768.0)
            + (0xeccdu16 as i16) as f64 / 32768.0;
        check(3, [gk4_large_min, -0.125, -0.125, -0.125, -0.125, -0.125]);
        let gk1 = |code: f64| code / 128.0 * (1.0 + 0x0081 as f64 / 32768.0);
        check(4, [gk1(127.0), gk1(-127.0), 0.0, 0.0, 0.0, 0.0]);
        let h18 = 0.5 * (1.0 + 2.0 / 32768.0);
        check(5, [h18; 6]);
    }

    #[test]
    fn truncated_payload_is_a_short_packet() {
        let hebap = [19u8; 10];
        let data = [0u8; 8];
        let mut reader = BitReader::new(&data);
        let mut out = vec![[0i32; 6]; hebap.len()];
        assert!(matches!(
            decode_pre_mantissas(&mut reader, &hebap, 0, hebap.len(), &mut out),
            Err(ParseError::ShortPacket)
        ));
    }

    #[test]
    fn invalid_inputs_are_errors_not_panics() {
        let data = [0xffu8; 64];
        let mut out = vec![[0i32; 6]; 4];
        let mut reader = BitReader::new(&data);
        // hebap out of range.
        assert!(decode_pre_mantissas(&mut reader, &[20, 1, 1, 1], 0, 4, &mut out).is_err());
        // Ranges past the slices.
        let mut reader = BitReader::new(&data);
        assert!(decode_pre_mantissas(&mut reader, &[1, 1], 0, 4, &mut out).is_err());
        let mut reader = BitReader::new(&data);
        assert!(decode_pre_mantissas(&mut reader, &[1; 8], 0, 8, &mut out).is_err());
        // gaqmod 3 with an impossible gain triplet (all ones = 31).
        let mut reader = BitReader::new(&data);
        assert!(decode_pre_mantissas(&mut reader, &[8; 4], 0, 4, &mut out).is_err());
    }
}
