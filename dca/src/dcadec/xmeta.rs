// SPDX-License-Identifier: Apache-2.0
//! Private DTS:X extension metadata: what the wrapper around the XLL-X channel
//! sets says about the extension waveforms.
//!
//! Two facts matter for playback and both come from the same bytes:
//!
//! - **How each waveform was mixed into the compatible bed.** The encoder
//!   folded every height feed and every object into the 7.1 bed it also
//!   transmits, at gains it writes down here. Rendering a feed at its own
//!   position without first removing that contribution plays it twice. The
//!   standard profile states the fold in its type-2 matrix; the alternate
//!   profiles state the height fold in their type-3 rows and each object's
//!   fold in the sparse reference rows attached to its position record.
//! - **Where the objects sit.** Alternate-profile records carry a spherical
//!   position per object.
//!
//! Which waveform each row describes was established against the corpus
//! audio, not from the bytes alone: the last four extension waveforms of an
//! alternate profile are the fixed heights, in type-3 row order, and object
//! record `index` is the extension waveform of the same index. See
//! `docs/private-metadata-probe.md` in the repository for the evidence.
//!
//! This module is realtime code: it reads a bounded prefix with a checked bit
//! reader, allocates nothing, and reports every unfamiliar form as an error
//! instead of guessing. A frame whose metadata does not parse simply has no
//! usable fold, which the consumer must treat as "keep the bed as authored".

use crate::dcadec::tables::DMIXTABLE;
use crate::dcadec::xll::{
    DCA_SYNCWORD_XLL_X, DCA_SYNCWORD_XLL_X_ALT_D0, DCA_SYNCWORD_XLL_X_ALT_D1,
    DCA_SYNCWORD_XLL_X_ALT_D3, DCA_SYNCWORD_XLL_X_ALT_D4, alternate_protected_prefix, crc16_ccitt,
};
use crate::spatial::SpatialChannel;

/// Most extension waveforms any known profile carries (five objects plus the
/// four fixed heights).
pub const MAX_SOURCES: usize = 9;
/// Channels of the compatible reference layout the folds are expressed over.
pub const REFERENCE_CHANNELS: usize = 8;
/// DCA speaker index behind each column of the 7.1 reference layout
/// (`0x084b`: C, L/R, LFE, Lsr/Rsr and Lss/Rss in bit order).
pub const REFERENCE_SPEAKERS: [usize; REFERENCE_CHANNELS] = [0, 1, 2, 5, 7, 8, 3, 4];
/// The 7.1 reference mask every seven-channel profile uses.
pub const REFERENCE_MASK_7_1: u32 = 0x084b;
/// The 5.1 reference mask of the object-only variant (C, L/R, Ls/Rs, LFE).
pub const REFERENCE_MASK_5_1: u32 = 0x000f;

/// DCA speaker index behind each column of a reference mask, in mask bit
/// order, and the column count. A mask carrying an unknown speaker bit, or
/// both surround pairs that map to the same DCA indices, has no layout.
fn reference_speakers(mask: u32) -> Option<([u8; REFERENCE_CHANNELS], usize)> {
    if mask & (1 << 2) != 0 && mask & (1 << 11) != 0 {
        return None;
    }
    let mut speakers = [0u8; REFERENCE_CHANNELS];
    let mut count = 0;
    for bit in 0..16 {
        if mask & (1 << bit) == 0 {
            continue;
        }
        let members: &[u8] = match bit {
            0 => &[0],
            1 => &[1, 2],
            2 => &[3, 4],
            3 => &[5],
            4 => &[6],
            6 => &[7, 8],
            11 => &[3, 4],
            _ => return None,
        };
        for &member in members {
            if count == REFERENCE_CHANNELS {
                return None;
            }
            speakers[count] = member;
            count += 1;
        }
    }
    (count > 0).then_some((speakers, count))
}

/// Columns of a full output layout (reference mask plus the height mask):
/// for each column, the reference column it stands for, or the height feed
/// it names. Height columns come from bits 5 (Lh/Rh) and 15 (Lhr/Rhr).
fn full_columns(
    output_mask: u32,
) -> Option<([Result<usize, SpatialChannel>; MAX_FULL_COLUMNS], usize)> {
    let mut columns = [Ok(0usize); MAX_FULL_COLUMNS];
    let mut count = 0;
    let mut reference = 0;
    for bit in 0..16 {
        if output_mask & (1 << bit) == 0 {
            continue;
        }
        let entries: [Result<usize, SpatialChannel>; 2] = match bit {
            5 => [
                Err(SpatialChannel::TopFrontLeft),
                Err(SpatialChannel::TopFrontRight),
            ],
            15 => [
                Err(SpatialChannel::TopBackLeft),
                Err(SpatialChannel::TopBackRight),
            ],
            0 | 3 | 4 => {
                reference += 1;
                [Ok(reference - 1), Ok(usize::MAX)]
            }
            1 | 2 | 6 | 11 => {
                reference += 2;
                [Ok(reference - 2), Ok(reference - 1)]
            }
            _ => return None,
        };
        for entry in entries {
            if entry == Ok(usize::MAX) {
                continue;
            }
            if count == MAX_FULL_COLUMNS {
                return None;
            }
            columns[count] = entry;
            count += 1;
        }
    }
    Some((columns, count))
}
/// DCA speaker indices a [`FoldPlan`] can address.
pub const FOLD_SPEAKERS: usize = 16;

const HEIGHT_MASK: u32 = 0x8020;
/// Columns of the widest full layout: eight reference channels plus the
/// four heights.
const MAX_FULL_COLUMNS: usize = REFERENCE_CHANNELS + 4;
/// The standard profile's height mask `0x8020` lists Lh/Rh then Lhr/Rhr.
const STANDARD_HEIGHTS: [SpatialChannel; 4] = [
    SpatialChannel::TopFrontLeft,
    SpatialChannel::TopFrontRight,
    SpatialChannel::TopBackLeft,
    SpatialChannel::TopBackRight,
];
const FIXED_HEIGHT_COUNT: usize = 4;
/// Unread type-3 control word; every corpus stream carries this value.
const TYPE3_CONTROL: u32 = 0x3fa;
const UNITY_CODE: u32 = 61;
const CENTRE_HEIGHT_MASK: u32 = 0x80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XMetadataError {
    /// The payload ends before the metadata does.
    Truncated,
    /// A form this reader has no verified interpretation for.
    Unsupported(&'static str),
    /// A CRC, boundary or padding check failed.
    Invalid(&'static str),
}

type R<T> = Result<T, XMetadataError>;

struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn read(&mut self, width: usize) -> R<u32> {
        let end = self
            .pos
            .checked_add(width)
            .ok_or(XMetadataError::Truncated)?;
        if width > 32 || end > self.bytes.len().saturating_mul(8) {
            return Err(XMetadataError::Truncated);
        }
        let mut value = 0u32;
        for bit in self.pos..end {
            value = (value << 1) | u32::from((self.bytes[bit / 8] >> (7 - bit % 8)) & 1);
        }
        self.pos = end;
        Ok(value)
    }

    fn expect(&mut self, width: usize, expected: u32, field: &'static str) -> R<()> {
        if self.read(width)? != expected {
            return Err(XMetadataError::Unsupported(field));
        }
        Ok(())
    }

    fn align(&mut self, field: &'static str) -> R<()> {
        let padding = (8 - self.pos % 8) % 8;
        if self.read(padding)? != 0 {
            return Err(XMetadataError::Invalid(field));
        }
        Ok(())
    }
}

/// Linear gain of a six-bit gain code.
///
/// Every code verified against the corpus audio is the decoder's downmix
/// table entry at index `4 * code - 3`, i.e. half a decibel per code with 61
/// as unity (61 → 32768, 55 → 23170, 58 → 27571, 46 → 13818). Code 0 and the
/// escape codes 62/63 have no verified meaning and are rejected.
pub fn gain_code_linear(code: u32) -> Option<f32> {
    match code {
        1..=UNITY_CODE => Some(f32::from(DMIXTABLE[4 * code as usize - 3]) / 32768.0),
        _ => None,
    }
}

/// A transmitted object position in the stream's own units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SphericalPosition {
    /// 0 = front, negative = left, in half-degrees.
    pub azimuth_half_degrees: i16,
    /// Positive = up, in half-degrees.
    pub elevation_half_degrees: i16,
    /// Distance in 1/64 units; 64 is the reference distance.
    pub distance_64ths: u8,
}

impl SphericalPosition {
    fn from_codes(azimuth: u32, elevation: u32, distance: u32) -> Self {
        let azimuth_half_degrees = match azimuth {
            47 => -220,
            193 => 220,
            value => (3 * value as i16 - 360).min(357),
        };
        let elevation_half_degrees = (3 * (elevation as i16 - 60)).min(180);
        let distance_64ths = if distance == 0 { 0 } else { distance as u8 + 1 };
        Self {
            azimuth_half_degrees,
            elevation_half_degrees,
            distance_64ths,
        }
    }

    pub fn azimuth_degrees(self) -> f64 {
        f64::from(self.azimuth_half_degrees) / 2.0
    }

    pub fn elevation_degrees(self) -> f64 {
        f64::from(self.elevation_half_degrees) / 2.0
    }

    pub fn distance(self) -> f64 {
        f64::from(self.distance_64ths) / 64.0
    }

    /// ADM Cartesian `[x, y, z]`: x left-to-right, y back-to-front, z
    /// floor-to-ceiling, the same spherical conversion the renderer applies
    /// to polar events.
    pub fn to_adm_cartesian(self) -> [f64; 3] {
        let azimuth = self.azimuth_degrees().to_radians();
        let elevation = self.elevation_degrees().to_radians();
        let distance = self.distance();
        let horizontal = distance * elevation.cos();
        [
            horizontal * azimuth.sin(),
            horizontal * azimuth.cos(),
            distance * elevation.sin(),
        ]
    }
}

/// How one extension waveform was mixed into the compatible bed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BedFold {
    /// Linear gain of the waveform in each reference column; 0.0 is absent.
    Known([f32; REFERENCE_CHANNELS]),
    /// The waveform is in the bed, but the stream does not say at which
    /// gains (an object record without reference rows). Nothing can be
    /// subtracted, so the waveform must not be rendered a second time.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SourceRole {
    /// A fixed height feed.
    Height(SpatialChannel),
    /// An object at a transmitted position.
    Object {
        position: SphericalPosition,
        /// The record also declares the centre-height speaker as a
        /// fixed-channel alternative for this object.
        centre_height_alternative: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceMetadata {
    pub role: SourceRole,
    pub fold: BedFold,
}

/// One frame's metadata for every extension waveform, in waveform order,
/// with the reference layout its folds are expressed over.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct XMetadata {
    sources: [Option<SourceMetadata>; MAX_SOURCES],
    count: usize,
    reference_speakers: [u8; REFERENCE_CHANNELS],
    reference_count: usize,
}

impl XMetadata {
    /// Parse the metadata of an extension payload that decoded into
    /// `source_count` waveforms. `payload` is `HdFrame::x_payload`.
    pub fn parse(payload: &[u8], source_count: usize) -> R<Self> {
        let syncword = payload
            .get(..4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_be_bytes)
            .ok_or(XMetadataError::Truncated)?;
        match syncword {
            DCA_SYNCWORD_XLL_X => parse_standard(payload, source_count),
            DCA_SYNCWORD_XLL_X_ALT_D0
            | DCA_SYNCWORD_XLL_X_ALT_D1
            | DCA_SYNCWORD_XLL_X_ALT_D3
            | DCA_SYNCWORD_XLL_X_ALT_D4 => parse_alternate(payload, source_count),
            _ => Err(XMetadataError::Unsupported("extension profile")),
        }
    }

    /// Assemble metadata from already-decoded sources over the 7.1 reference
    /// layout (consumers' tests and fallbacks). `None` when there are more
    /// than [`MAX_SOURCES`].
    pub fn from_sources(sources: &[SourceMetadata]) -> Option<Self> {
        Self::from_sources_on(sources, REFERENCE_MASK_7_1)
    }

    /// [`Self::from_sources`] over the reference layout `reference_mask`.
    pub fn from_sources_on(sources: &[SourceMetadata], reference_mask: u32) -> Option<Self> {
        if sources.len() > MAX_SOURCES {
            return None;
        }
        let (reference_speakers, reference_count) = reference_speakers(reference_mask)?;
        let mut stored = [None; MAX_SOURCES];
        for (slot, source) in stored.iter_mut().zip(sources) {
            *slot = Some(*source);
        }
        Some(Self {
            sources: stored,
            count: sources.len(),
            reference_speakers,
            reference_count,
        })
    }

    pub fn source_count(&self) -> usize {
        self.count
    }

    /// DCA speaker index behind each fold column.
    pub fn reference_speakers(&self) -> &[u8] {
        &self.reference_speakers[..self.reference_count]
    }

    pub fn source(&self, index: usize) -> Option<&SourceMetadata> {
        self.sources.get(index).and_then(Option::as_ref)
    }

    pub fn sources(&self) -> impl Iterator<Item = &SourceMetadata> {
        self.sources[..self.count].iter().flatten()
    }
}

/// Per-frame subtraction table: for each DCA speaker, the gain of every
/// extension waveform already present in that speaker's bed channel. Built
/// once per frame and applied per sample without branching on profile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FoldPlan {
    gains: [[f32; MAX_SOURCES]; FOLD_SPEAKERS],
    /// Per-sample change of each gain across the frame, so a gain that was
    /// estimated rather than stated can ramp from the previous frame's value
    /// to this frame's without a step. Zero for every stated fold.
    slope: [[f32; MAX_SOURCES]; FOLD_SPEAKERS],
    /// Bit `k` set when waveform `k` contributes to that speaker. Sixteen bits
    /// because the widest profile carries nine waveforms, one more than a byte.
    used: [u16; FOLD_SPEAKERS],
    /// Bit `k` set when waveform `k` has no known fold.
    unknown: u16,
    count: usize,
}

impl FoldPlan {
    /// No metadata: nothing can be subtracted and no waveform may be
    /// rendered, since each is still in the bed at an unknown gain.
    pub fn all_unknown(source_count: usize) -> Self {
        let count = source_count.min(MAX_SOURCES);
        Self {
            gains: [[0.0; MAX_SOURCES]; FOLD_SPEAKERS],
            slope: [[0.0; MAX_SOURCES]; FOLD_SPEAKERS],
            used: [0; FOLD_SPEAKERS],
            unknown: (1u16 << count) - 1,
            count,
        }
    }

    /// The standard profile's fixed-height fold at one gain for all four
    /// feeds (TFL→L, TFR→R, TBL→Lsr, TBR→Rsr), for a frame whose matrix could
    /// not be read.
    pub fn standard_heights(gain: f32) -> Self {
        let mut plan = Self::all_unknown(0);
        plan.count = FIXED_HEIGHT_COUNT;
        for (feed, &speaker) in [1usize, 2, 7, 8].iter().enumerate() {
            plan.gains[speaker][feed] = gain;
            plan.used[speaker] |= 1 << feed;
        }
        plan
    }

    pub fn from_metadata(metadata: &XMetadata) -> Self {
        let mut plan = Self::all_unknown(0);
        plan.count = metadata.source_count();
        for (feed, source) in metadata.sources().enumerate() {
            match source.fold {
                BedFold::Known(columns) => {
                    for (&speaker, &gain) in metadata.reference_speakers().iter().zip(&columns) {
                        if gain != 0.0 {
                            let speaker = usize::from(speaker);
                            plan.gains[speaker][feed] = gain;
                            plan.used[speaker] |= 1 << feed;
                        }
                    }
                }
                BedFold::Unknown => plan.unknown |= 1 << feed,
            }
        }
        plan
    }

    pub fn source_count(&self) -> usize {
        self.count
    }

    /// Whether waveform `feed` may be rendered on its own channel.
    pub fn source_is_known(&self, feed: usize) -> bool {
        feed < self.count && self.unknown & (1 << feed) == 0
    }

    /// Whether any waveform of this frame has no stated fold.
    pub fn has_unknown(&self) -> bool {
        self.unknown != 0
    }

    /// The gain of waveform `feed` in `speaker` at the start of the frame
    /// (zero when it contributes nothing there).
    pub fn gain(&self, speaker: usize, feed: usize) -> f32 {
        if speaker < FOLD_SPEAKERS && feed < self.count && self.used[speaker] & (1 << feed) != 0 {
            self.gains[speaker][feed]
        } else {
            0.0
        }
    }

    /// Whether any subtraction applies to `speaker`.
    pub fn touches(&self, speaker: usize) -> bool {
        self.used.get(speaker).is_some_and(|&used| used != 0)
    }

    /// The bed sample of `speaker` with every known contribution removed.
    /// `sources` are the extension waveforms; a missing sample contributes
    /// nothing rather than panicking.
    #[inline]
    pub fn clean(&self, speaker: usize, bed: f32, sample: usize, sources: &[Vec<f32>]) -> f32 {
        let Some(&used) = self.used.get(speaker) else {
            return bed;
        };
        if used == 0 {
            return bed;
        }
        let gains = &self.gains[speaker];
        let slope = &self.slope[speaker];
        let position = sample as f32;
        let mut value = bed;
        for (feed, source) in sources.iter().enumerate().take(self.count) {
            if used & (1 << feed) != 0 {
                value -= (gains[feed] + slope[feed] * position)
                    * source.get(sample).copied().unwrap_or(0.0);
            }
        }
        value
    }
}

/// Estimates, from the audio itself, the fold of every waveform whose fold
/// the stream does not state.
///
/// Every extension waveform was mixed into the compatible bed by the encoder;
/// a record without reference rows only withholds the gains. Measured on such
/// streams, the waveform is present in the bed sample-aligned, at gains that
/// are constant (a wide channel) or follow the transmitted position (an object
/// panned into the bed by the encoder's own renderer, whose law differs from
/// one encoder generation to the next). Rather than guess the law, this solves
/// for the gains directly: per frame and per bed speaker, the least-squares
/// fit of the bed on the unstated waveforms, jointly so that two waveforms
/// sharing content are not both credited with it. The fit never adds energy
/// to a speaker within the frame; what it removes is what the waveforms
/// explain. A waveform silent in the frame keeps its last gains. Gains ramp
/// across the frame from the previous frame's estimate, so a moving object
/// leaves no steps in the bed.
///
/// Fixed-size state, no allocation per frame: a K×K normal system with K the
/// number of unstated waveforms (at most [`MAX_SOURCES`]), solved by Cholesky.
#[derive(Clone, Copy, Debug)]
pub struct FoldEstimator {
    gains: [[f32; MAX_SOURCES]; FOLD_SPEAKERS],
    /// Bit `k` set once waveform `k` has an estimate to ramp from.
    primed: u16,
}

impl Default for FoldEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl FoldEstimator {
    /// Squared-sample floor under which a waveform counts as silent in the
    /// frame (about -100 dBFS RMS for full-scale 1.0): its gains cannot be
    /// measured and its last ones stand.
    const SILENCE_ENERGY_PER_SAMPLE: f64 = 1e-10;
    /// Ridge added to the normal matrix, relative to its largest diagonal:
    /// keeps two nearly identical waveforms from cancelling each other with
    /// huge opposite gains.
    const RIDGE: f64 = 1e-4;
    /// Gains beyond this are not a fold; the estimate is discarded and the
    /// previous one stands.
    const MAX_GAIN: f32 = 4.0;
    /// Share of each frame's measurement taken into the running gain. One
    /// frame's fit carries noise from whatever else the speaker plays (about
    /// 0.04 for content at the waveform's own level); averaging two frames
    /// halves its power at the cost of one frame of lag on a moving object.
    const SMOOTHING: f32 = 0.5;

    pub const fn new() -> Self {
        Self {
            gains: [[0.0; MAX_SOURCES]; FOLD_SPEAKERS],
            primed: 0,
        }
    }

    /// Forget every estimate (a new stream, or a presentation change).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Fill in the unstated folds of `plan` from this frame's audio: `bed` is
    /// indexed by DCA speaker like `HdFrame::samples`, `sources` are the
    /// extension waveforms. Afterwards every waveform is known to the plan,
    /// so it is rendered on its own channel and removed from the bed.
    pub fn refine(&mut self, plan: &mut FoldPlan, bed: &[Option<Vec<f32>>], sources: &[Vec<f32>]) {
        if plan.unknown == 0 {
            return;
        }
        let count = plan.count.min(sources.len());
        let length = sources.iter().take(count).map(Vec::len).min().unwrap_or(0);
        if length == 0 {
            return;
        }
        // The unstated waveforms with something to measure this frame.
        let mut feeds = [0usize; MAX_SOURCES];
        let mut k = 0usize;
        for feed in 0..count {
            if plan.unknown & (1 << feed) == 0 {
                continue;
            }
            let energy: f64 = sources[feed][..length]
                .iter()
                .map(|&v| f64::from(v) * f64::from(v))
                .sum();
            if energy >= Self::SILENCE_ENERGY_PER_SAMPLE * length as f64 {
                feeds[k] = feed;
                k += 1;
            }
        }

        // Normal matrix over the active waveforms, once per frame.
        let mut gram = [[0.0f64; MAX_SOURCES]; MAX_SOURCES];
        let mut max_diagonal = 0.0f64;
        for i in 0..k {
            for j in 0..=i {
                let dot: f64 = sources[feeds[i]][..length]
                    .iter()
                    .zip(&sources[feeds[j]][..length])
                    .map(|(&a, &b)| f64::from(a) * f64::from(b))
                    .sum();
                gram[i][j] = dot;
                gram[j][i] = dot;
            }
            max_diagonal = max_diagonal.max(gram[i][i]);
        }
        let ridge = Self::RIDGE * max_diagonal;
        for (i, row) in gram.iter_mut().enumerate().take(k) {
            row[i] += ridge;
        }
        let cholesky = cholesky(&gram, k);

        for (speaker, channel) in bed.iter().enumerate().take(FOLD_SPEAKERS) {
            let Some(channel) = channel.as_deref() else {
                continue;
            };
            if channel.len() < length {
                continue;
            }
            let mut estimate = [0.0f64; MAX_SOURCES];
            if let Some(factor) = cholesky.as_ref() {
                let mut rhs = [0.0f64; MAX_SOURCES];
                for (i, &feed) in feeds.iter().enumerate().take(k) {
                    rhs[i] = sources[feed][..length]
                        .iter()
                        .zip(&channel[..length])
                        .map(|(&a, &b)| f64::from(a) * f64::from(b))
                        .sum();
                }
                solve(factor, k, &mut rhs);
                estimate[..k].copy_from_slice(&rhs[..k]);
            }
            for feed in 0..count {
                if plan.unknown & (1 << feed) == 0 {
                    continue;
                }
                let bit = 1u16 << feed;
                let previous = self.gains[speaker][feed];
                let measured = feeds[..k]
                    .iter()
                    .position(|&f| f == feed)
                    .map(|i| estimate[i] as f32)
                    .filter(|g| g.is_finite() && g.abs() <= Self::MAX_GAIN);
                let target = match measured {
                    Some(measured) if self.primed & bit != 0 => {
                        previous + Self::SMOOTHING * (measured - previous)
                    }
                    Some(measured) => measured,
                    None => previous,
                };
                let start = if self.primed & bit != 0 {
                    previous
                } else {
                    target
                };
                plan.gains[speaker][feed] = start;
                plan.slope[speaker][feed] = (target - start) / length as f32;
                plan.used[speaker] |= bit;
                self.gains[speaker][feed] = target;
            }
        }
        // Every measured waveform is now accounted for and may be rendered.
        let unknown = plan.unknown & ((1u16 << count) - 1);
        self.primed |= unknown;
        plan.unknown &= !unknown;
    }
}

/// Lower-triangular Cholesky factor of the leading `k`×`k` block, or `None`
/// when the matrix is not positive definite (all waveforms silent).
fn cholesky(
    matrix: &[[f64; MAX_SOURCES]; MAX_SOURCES],
    k: usize,
) -> Option<[[f64; MAX_SOURCES]; MAX_SOURCES]> {
    if k == 0 {
        return None;
    }
    let mut l = [[0.0f64; MAX_SOURCES]; MAX_SOURCES];
    for i in 0..k {
        for j in 0..=i {
            let mut sum = matrix[i][j];
            for p in 0..j {
                sum -= l[i][p] * l[j][p];
            }
            if i == j {
                if sum <= 0.0 {
                    return None;
                }
                l[i][i] = sum.sqrt();
            } else {
                l[i][j] = sum / l[j][j];
            }
        }
    }
    Some(l)
}

/// Solve L Lᵀ x = b in place, for the leading `k` unknowns.
fn solve(l: &[[f64; MAX_SOURCES]; MAX_SOURCES], k: usize, b: &mut [f64; MAX_SOURCES]) {
    for i in 0..k {
        let mut sum = b[i];
        for p in 0..i {
            sum -= l[i][p] * b[p];
        }
        b[i] = sum / l[i][i];
    }
    for i in (0..k).rev() {
        let mut sum = b[i];
        for p in i + 1..k {
            sum -= l[p][i] * b[p];
        }
        b[i] = sum / l[i][i];
    }
}

fn parse_standard(payload: &[u8], source_count: usize) -> R<XMetadata> {
    if source_count != FIXED_HEIGHT_COUNT {
        return Err(XMetadataError::Unsupported("standard waveform count"));
    }
    let mut b = Bits {
        bytes: payload,
        pos: 0,
    };
    let (reference_mask, output_mask) = layout_header(&mut b, 2, REFERENCE_MASK_7_1)?;
    if reference_mask != REFERENCE_MASK_7_1 || output_mask & !reference_mask != HEIGHT_MASK {
        return Err(XMetadataError::Unsupported("standard matrix layout"));
    }
    b.expect(5, 2, "matrix header field")?;
    b.expect(6, 0, "matrix header value")?;
    b.expect(1, 1, "matrix mode")?;
    b.expect(1, 0, "optional matrix parameters")?;
    b.expect(1, 1, "inline matrix flag")?;
    b.expect(6, UNITY_CODE, "matrix scale")?;
    b.expect(1, 0, "additional scales")?;
    let mut sources = [None; MAX_SOURCES];
    for (feed, &height) in STANDARD_HEIGHTS.iter().enumerate() {
        let mask = b.read(REFERENCE_CHANNELS)?;
        if mask == 0 {
            return Err(XMetadataError::Unsupported("empty matrix row"));
        }
        let columns = sparse_row(&mut b, mask, REFERENCE_CHANNELS)?;
        sources[feed] = Some(SourceMetadata {
            role: SourceRole::Height(height),
            fold: BedFold::Known(columns),
        });
    }
    b.align("matrix padding")?;
    let protected = b.pos / 8 + 2;
    let bytes = payload.get(..protected).ok_or(XMetadataError::Truncated)?;
    if crc16_ccitt(bytes) != 0 {
        return Err(XMetadataError::Invalid("matrix CRC"));
    }
    Ok(XMetadata {
        sources,
        count: FIXED_HEIGHT_COUNT,
        reference_speakers: REFERENCE_SPEAKERS.map(|speaker| speaker as u8),
        reference_count: REFERENCE_CHANNELS,
    })
}

/// Read the element header shared by the type-2 and type-3 layout elements.
/// Every flag without a verified meaning is pinned to its observed value.
fn layout_header(b: &mut Bits<'_>, kind: u32, inherited: u32) -> R<(u32, u32)> {
    b.expect(8, kind, "element type")?;
    b.expect(8, 0, "element header byte")?;
    b.expect(1, 0, "header flag")?;
    b.expect(4, 1, "header field A")?;
    b.expect(4, u32::from(kind == 3), "header field B")?;
    let reference_mask = if b.read(1)? != 0 {
        b.expect(1, 0, "reference flag")?;
        b.expect(1, 1, "explicit reference mask")?;
        b.expect(2, 0, "reference mode")?;
        b.expect(3, 0, "reference field")?;
        let width = 4 * (b.read(3)? as usize + 1);
        b.read(width)?
    } else {
        b.expect(1, 0, "implicit reference mask")?;
        b.expect(4, 0, "implicit reference field")?;
        inherited
    };
    b.expect(1, 1, "level field present")?;
    b.expect(6, UNITY_CODE, "level code")?;
    b.expect(1, 0, "additional level")?;
    let width = 4 * (b.read(3)? as usize + 1);
    let output_mask = b.read(width)?;
    if output_mask & reference_mask != reference_mask {
        return Err(XMetadataError::Unsupported("non-superset layout"));
    }
    Ok((reference_mask, output_mask))
}

/// Read the gain codes of the set bits of `mask` over `columns` reference
/// columns into a dense gain row.
fn sparse_row(b: &mut Bits<'_>, mask: u32, columns: usize) -> R<[f32; REFERENCE_CHANNELS]> {
    let mut row = [0.0; REFERENCE_CHANNELS];
    for (column, slot) in row.iter_mut().enumerate().take(columns) {
        if mask & (1 << column) != 0 {
            *slot = gain_code_linear(b.read(6)?).ok_or(XMetadataError::Unsupported("gain code"))?;
        }
    }
    Ok(row)
}

/// Length of the CRC-protected alternate prefix and the mask of channel
/// sets that follow it, by the same rule the audio decoder applies.
fn alternate_prefix_len(payload: &[u8]) -> R<(usize, u8)> {
    alternate_protected_prefix(payload).map_err(|_| XMetadataError::Invalid("alternate prefix CRC"))
}

fn parse_alternate(payload: &[u8], source_count: usize) -> R<XMetadata> {
    let (prefix_len, _set_mask) = alternate_prefix_len(payload)?;
    let prefix = &payload[..prefix_len];
    let mut sources = [None; MAX_SOURCES];
    // The type-241 element is byte-aligned and self-delimiting. Either the
    // envelope CRC follows it directly (object-only variant), or a type-3
    // element describing the height quartet does and must end exactly at
    // the CRC.
    let (type3_start, object_count, reference_mask) = parse_type241(prefix, &mut sources)?;
    let (reference_speakers, reference_count) = reference_speakers(reference_mask)
        .ok_or(XMetadataError::Unsupported("reference layout"))?;
    let heights_present = type3_start + 2 != prefix.len();
    let expected = if heights_present {
        object_count + FIXED_HEIGHT_COUNT
    } else {
        object_count
    };
    if source_count != expected || source_count > MAX_SOURCES {
        return Err(XMetadataError::Unsupported("alternate waveform count"));
    }
    if heights_present {
        let heights = parse_type3(&prefix[type3_start..], reference_mask)?;
        for (slot, height) in sources[object_count..source_count].iter_mut().zip(heights) {
            *slot = Some(height);
        }
    }
    Ok(XMetadata {
        sources,
        count: source_count,
        reference_speakers,
        reference_count,
    })
}

/// The type-3 element: four rows over the full layout (reference channels
/// plus the height pairs), one per fixed height in the order the second
/// channel set decodes them. A row's height column names the feed; its
/// reference columns are the feed's fold.
fn parse_type3(bytes: &[u8], reference_mask: u32) -> R<[SourceMetadata; FIXED_HEIGHT_COUNT]> {
    let mut b = Bits { bytes, pos: 0 };
    let (declared_reference, output_mask) = layout_header(&mut b, 3, reference_mask)?;
    if declared_reference != reference_mask || output_mask != reference_mask | HEIGHT_MASK {
        return Err(XMetadataError::Unsupported("type-3 layout"));
    }
    let (columns, column_count) =
        full_columns(output_mask).ok_or(XMetadataError::Unsupported("type-3 layout"))?;
    b.expect(5, 2, "type-3 field")?;
    b.expect(6, 0, "type-3 value")?;
    b.expect(1, 1, "type-3 flag")?;
    b.expect(1, 0, "type-3 option")?;
    b.expect(12, TYPE3_CONTROL, "type-3 control")?;
    let mut heights = [SourceMetadata {
        role: SourceRole::Height(SpatialChannel::TopFrontLeft),
        fold: BedFold::Unknown,
    }; FIXED_HEIGHT_COUNT];
    for height in &mut heights {
        let mask = b.read(column_count)?;
        let mut identity = None;
        let mut fold = [0.0; REFERENCE_CHANNELS];
        for (column, entry) in columns.iter().enumerate().take(column_count) {
            if mask & (1 << column) == 0 {
                continue;
            }
            let code = b.read(6)?;
            match *entry {
                Ok(reference) => {
                    fold[reference] = gain_code_linear(code)
                        .ok_or(XMetadataError::Unsupported("type-3 gain code"))?;
                }
                Err(speaker) => {
                    if identity.replace(speaker).is_some() || code != UNITY_CODE {
                        return Err(XMetadataError::Unsupported("type-3 height column"));
                    }
                }
            }
        }
        let speaker = identity.ok_or(XMetadataError::Unsupported("type-3 row without height"))?;
        *height = SourceMetadata {
            role: SourceRole::Height(speaker),
            fold: BedFold::Known(fold),
        };
    }
    b.align("type-3 padding")?;
    if b.pos / 8 + 2 != bytes.len() {
        return Err(XMetadataError::Invalid("type-3 end"));
    }
    Ok(heights)
}

/// The type-241 element: object declarations, then one position record per
/// object with optional reference rows, then optional auxiliary
/// fixed-channel alternatives. Fills `objects` by declared index and returns
/// the byte offset at which the next element starts, the declared object
/// count and the reference mask the rows are expressed over.
fn parse_type241(bytes: &[u8], objects: &mut [Option<SourceMetadata>]) -> R<(usize, usize, u32)> {
    let mut b = Bits { bytes, pos: 0 };
    for (width, value) in [
        (8, 241),
        (8, 0x40),
        (4, 0),
        (1, 0),
        (4, 1),
        (1, 1),
        (1, 0),
        (1, 1),
    ] {
        b.expect(width, value, "type-241 header")?;
    }
    let count = b.read(4)? as usize + 1;
    if count > objects.len() {
        return Err(XMetadataError::Unsupported("type-241 declaration count"));
    }
    for (width, value) in [(1, 0), (1, 0), (1, 1), (1, 1), (2, 0), (3, 0)] {
        b.expect(width, value, "type-241 reference form")?;
    }
    let width = 4 * (b.read(3)? as usize + 1);
    let reference_mask = b.read(width)?;
    let (_, columns) = reference_speakers(reference_mask)
        .ok_or(XMetadataError::Unsupported("type-241 reference layout"))?;
    // No optional levels, additional layouts or timing parameters; precision
    // zero gives three-bit indices and two-bit modes.
    b.expect(8, 0, "type-241 optional fields")?;
    let mut indices = [0usize; MAX_SOURCES];
    let mut modes = [0u32; MAX_SOURCES];
    for record in 0..count {
        b.expect(1, 1, "inactive type-241 declaration")?;
        // A two-bit field with two observed values: 3 on the 7.1 profiles,
        // 1 on the object-only 5.1 variant. Its meaning is not established.
        if !matches!(b.read(2)?, 1 | 3) {
            return Err(XMetadataError::Unsupported("type-241 declaration field"));
        }
        let index = b.read(3)? as usize;
        if index >= count || indices[..record].contains(&index) {
            return Err(XMetadataError::Unsupported("type-241 declaration index"));
        }
        let mode = b.read(2)?;
        if mode > 1 {
            return Err(XMetadataError::Unsupported("type-241 declaration mode"));
        }
        // One component, component code zero, no optional parameter,
        // parameter mode zero, one subcomponent.
        b.expect(10, 0, "type-241 component form")?;
        indices[record] = index;
        modes[record] = mode;
    }
    for record in 0..count {
        // Mode 0 carries a four-bit option field and no reference rows; the
        // field's meaning is not established, only its exact consumption.
        if modes[record] == 0 {
            b.expect(4, 1, "mode-0 record options")?;
        } else {
            b.expect(3, 0, "type-241 record options")?;
        }
        b.expect(7, 0x20, "type-241 position options")?;
        b.expect(1, 1, "type-241 gain present")?;
        b.expect(1, 1, "type-241 position flag")?;
        b.expect(2, 0, "type-241 position mode")?;
        b.expect(6, UNITY_CODE, "type-241 position gain")?;
        let distance = b.read(6)?;
        let azimuth = b.read(8)?;
        let elevation = b.read(7)?;
        b.expect(1, 0, "type-241 extent")?;
        let fold = if modes[record] == 1 {
            let first = b.read(1)? != 0;
            let second = b.read(1)? != 0;
            let fold = if first {
                let mask = b.read(columns)?;
                sparse_row(&mut b, mask, columns)?
            } else {
                [0.0; REFERENCE_CHANNELS]
            };
            if second {
                return Err(XMetadataError::Unsupported("type-241 second reference row"));
            }
            BedFold::Known(fold)
        } else {
            BedFold::Unknown
        };
        objects[indices[record]] = Some(SourceMetadata {
            role: SourceRole::Object {
                position: SphericalPosition::from_codes(azimuth, elevation, distance),
                centre_height_alternative: false,
            },
            fold,
        });
    }
    if b.read(1)? != 0 {
        for record in 0..count {
            if b.read(1)? == 0 {
                continue;
            }
            let entries = b.read(2)? + 1;
            let width = b.read(5)? as usize;
            for _ in 0..entries {
                // Only the observed single-channel form is known: bit 7 of the
                // speaker mask, the centre-height position, at unity.
                if b.read(width)? != CENTRE_HEIGHT_MASK {
                    return Err(XMetadataError::Unsupported("type-241 auxiliary layout"));
                }
                b.expect(1, 1, "type-241 auxiliary flag")?;
                b.expect(1, 1, "type-241 auxiliary route")?;
                b.expect(6, UNITY_CODE, "type-241 auxiliary gain")?;
                if let Some(SourceMetadata {
                    role:
                        SourceRole::Object {
                            centre_height_alternative,
                            ..
                        },
                    ..
                }) = objects[indices[record]].as_mut()
                {
                    *centre_height_alternative = true;
                }
            }
        }
    }
    b.align("type-241 padding")?;
    Ok((b.pos / 8, count, reference_mask))
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! Synthetic payloads built field by field, so tests exercise the reader
    //! on values the corpus never shows.

    use super::*;
    use crate::dcadec::xll::XLL_X_ALT_OUTER_SUFFIX;

    /// The 7.1 full layout: reference channels plus the height pairs.
    const FULL_MASK_7_1: u32 = REFERENCE_MASK_7_1 | HEIGHT_MASK;

    pub(crate) struct BitWriter {
        bits: Vec<u8>,
    }

    impl BitWriter {
        pub(crate) fn new() -> Self {
            Self { bits: Vec::new() }
        }

        pub(crate) fn push(&mut self, value: u32, width: usize) -> &mut Self {
            for bit in (0..width).rev() {
                self.bits.push(((value >> bit) & 1) as u8);
            }
            self
        }

        pub(crate) fn align(&mut self) -> &mut Self {
            while self.bits.len() % 8 != 0 {
                self.bits.push(0);
            }
            self
        }

        pub(crate) fn bytes(&self) -> Vec<u8> {
            let mut bytes = vec![0u8; self.bits.len().div_ceil(8)];
            for (i, bit) in self.bits.iter().enumerate() {
                bytes[i / 8] |= bit << (7 - i % 8);
            }
            bytes
        }
    }

    fn crc_appended(mut bytes: Vec<u8>) -> Vec<u8> {
        let crc = crc16_ccitt(&bytes);
        bytes.extend_from_slice(&crc.to_be_bytes());
        bytes
    }

    fn layout_header_bits(w: &mut BitWriter, kind: u32, output_mask: u32, explicit_mask: bool) {
        w.push(kind, 8)
            .push(0, 8)
            .push(0, 1)
            .push(1, 4)
            .push(u32::from(kind == 3), 4);
        if explicit_mask {
            w.push(1, 1)
                .push(0, 1)
                .push(1, 1)
                .push(0, 2)
                .push(0, 3)
                .push(2, 3)
                .push(REFERENCE_MASK_7_1, 12);
        } else {
            w.push(0, 1).push(0, 1).push(0, 4);
        }
        w.push(1, 1)
            .push(UNITY_CODE, 6)
            .push(0, 1)
            .push(3, 3)
            .push(output_mask, 16);
    }

    /// A standard type-2 matrix: `rows[i]` is `(target mask, gain code)` for
    /// height `i`, followed by the bare XLL bytes the decoder would find.
    pub(crate) fn standard_payload(rows: [(u32, u32); 4]) -> Vec<u8> {
        let mut w = BitWriter::new();
        layout_header_bits(&mut w, 2, FULL_MASK_7_1, true);
        w.push(2, 5)
            .push(0, 6)
            .push(1, 1)
            .push(0, 1)
            .push(1, 1)
            .push(UNITY_CODE, 6)
            .push(0, 1);
        for (mask, code) in rows {
            w.push(mask, 8);
            for column in 0..8 {
                if mask & (1 << column) != 0 {
                    w.push(code, 6);
                }
            }
        }
        w.align();
        let mut payload = crc_appended(w.bytes());
        // Something after the protected prefix, as in a real frame.
        payload.extend_from_slice(&[0x41, 0xa0, 0x00, 0x00]);
        payload
    }

    pub(crate) struct ObjectRecord {
        pub mode: u32,
        pub index: u32,
        pub distance: u32,
        pub azimuth: u32,
        pub elevation: u32,
        /// `(reference mask, gain code)`; `None` when the row is absent.
        pub row: Option<(u32, u32)>,
        pub centre_height: bool,
    }

    /// An alternate prefix (type 241 + type 3, CRC-delimited) followed by
    /// the outer control suffix and a few bytes, as `x_payload` carries it.
    /// `height_rows[i]` is `(full-layout mask, bed gain code)` for height
    /// row `i`; its height column is set at unity by `height_column`.
    pub(crate) fn alternate_payload(
        records: &[ObjectRecord],
        height_rows: [(u32, u32); 4],
    ) -> Vec<u8> {
        alternate_payload_with(records, height_rows, false)
    }

    /// `explicit_type3_mask` writes the type-3 reference mask explicitly
    /// instead of inheriting it, a form the corpus never uses.
    pub(crate) fn alternate_payload_with(
        records: &[ObjectRecord],
        height_rows: [(u32, u32); 4],
        explicit_type3_mask: bool,
    ) -> Vec<u8> {
        let mut w = BitWriter::new();
        // The profile byte (0xd0/0xd1/0xd3) is the header fields below with
        // the declaration count in its low nibble, not a separate field.
        w.push(0xf1, 8)
            .push(0x40, 8)
            .push(0, 4)
            .push(0, 1)
            .push(1, 4)
            .push(1, 1)
            .push(0, 1)
            .push(1, 1)
            .push(records.len() as u32 - 1, 4);
        w.push(0, 1)
            .push(0, 1)
            .push(1, 1)
            .push(1, 1)
            .push(0, 2)
            .push(0, 3);
        w.push(2, 3).push(REFERENCE_MASK_7_1, 12).push(0, 8);
        for record in records {
            w.push(1, 1)
                .push(3, 2)
                .push(record.index, 3)
                .push(record.mode, 2)
                .push(0, 10);
        }
        for record in records {
            if record.mode == 0 {
                w.push(1, 4);
            } else {
                w.push(0, 3);
            }
            w.push(0x20, 7)
                .push(1, 1)
                .push(1, 1)
                .push(0, 2)
                .push(UNITY_CODE, 6);
            w.push(record.distance, 6)
                .push(record.azimuth, 8)
                .push(record.elevation, 7)
                .push(0, 1);
            if record.mode == 1 {
                match record.row {
                    Some((mask, code)) => {
                        w.push(1, 1).push(0, 1).push(mask, 8);
                        for column in 0..8 {
                            if mask & (1 << column) != 0 {
                                w.push(code, 6);
                            }
                        }
                    }
                    None => {
                        w.push(0, 1).push(0, 1);
                    }
                }
            }
        }
        let any_aux = records.iter().any(|record| record.centre_height);
        w.push(u32::from(any_aux), 1);
        if any_aux {
            for record in records {
                w.push(u32::from(record.centre_height), 1);
                if record.centre_height {
                    w.push(0, 2)
                        .push(8, 5)
                        .push(CENTRE_HEIGHT_MASK, 8)
                        .push(1, 1)
                        .push(1, 1)
                        .push(UNITY_CODE, 6);
                }
            }
        }
        w.align();
        let type241 = w.bytes();

        let mut w = BitWriter::new();
        layout_header_bits(&mut w, 3, FULL_MASK_7_1, explicit_type3_mask);
        w.push(2, 5)
            .push(0, 6)
            .push(1, 1)
            .push(0, 1)
            .push(TYPE3_CONTROL, 12);
        for (mask, code) in height_rows {
            w.push(mask, 12);
            let (columns, column_count) = full_columns(FULL_MASK_7_1).unwrap();
            for (column, entry) in columns.iter().enumerate().take(column_count) {
                if mask & (1 << column) != 0 {
                    w.push(if entry.is_err() { UNITY_CODE } else { code }, 6);
                }
            }
        }
        w.align();
        let type3 = w.bytes();

        let mut prefix = type241;
        prefix.extend_from_slice(&type3);
        let mut payload = crc_appended(prefix);
        payload.extend_from_slice(&XLL_X_ALT_OUTER_SUFFIX);
        payload.extend_from_slice(&[0xb2, 0, 0, 0, 0, 0, 0, 0]);
        payload
    }

    /// The object-only variant on a 5.1 bed: one mode-1 record over the
    /// reference mask `0x00f` (four-bit width field, six fold columns,
    /// declaration field 1), no type-3 element, and an outer marker saying
    /// that only the first channel set follows.
    pub(crate) fn object_only_payload(record: &ObjectRecord) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.push(0xf1, 8)
            .push(0x40, 8)
            .push(0, 4)
            .push(0, 1)
            .push(1, 4)
            .push(1, 1)
            .push(0, 1)
            .push(1, 1)
            .push(0, 4);
        w.push(0, 1)
            .push(0, 1)
            .push(1, 1)
            .push(1, 1)
            .push(0, 2)
            .push(0, 3);
        w.push(0, 3).push(REFERENCE_MASK_5_1, 4).push(0, 8);
        w.push(1, 1)
            .push(1, 2)
            .push(record.index, 3)
            .push(record.mode, 2)
            .push(0, 10);
        if record.mode == 0 {
            w.push(1, 4);
        } else {
            w.push(0, 3);
        }
        w.push(0x20, 7)
            .push(1, 1)
            .push(1, 1)
            .push(0, 2)
            .push(UNITY_CODE, 6);
        w.push(record.distance, 6)
            .push(record.azimuth, 8)
            .push(record.elevation, 7)
            .push(0, 1);
        if record.mode == 1 {
            match record.row {
                Some((mask, code)) => {
                    w.push(1, 1).push(0, 1).push(mask, 6);
                    for column in 0..6 {
                        if mask & (1 << column) != 0 {
                            w.push(code, 6);
                        }
                    }
                }
                None => {
                    w.push(0, 1).push(0, 1);
                }
            }
        }
        w.push(0, 1); // no auxiliary section
        w.align();
        let mut payload = crc_appended(w.bytes());
        payload.push(0x01);
        payload.extend_from_slice(&crate::dcadec::xll::XLL_X_ALT_MARKER_TAIL);
        payload.extend_from_slice(&[0xb2, 0, 0, 0, 0, 0, 0, 0]);
        payload
    }

    /// The corpus D3 form: heights folded at code 55 into L, R, Lsr, Rsr.
    pub(crate) const HEIGHT_ROWS_55: [(u32, u32); 4] = [
        (1 << 1 | 1 << 4, 55),
        (1 << 2 | 1 << 5, 55),
        (1 << 6 | 1 << 10, 55),
        (1 << 7 | 1 << 11, 55),
    ];
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    const Q55: f32 = 23170.0 / 32768.0;

    #[test]
    fn gain_codes_follow_the_downmix_table_at_the_verified_points() {
        assert_eq!(gain_code_linear(61), Some(1.0));
        assert_eq!(gain_code_linear(55), Some(Q55));
        assert_eq!(gain_code_linear(58), Some(27571.0 / 32768.0));
        assert_eq!(gain_code_linear(46), Some(13818.0 / 32768.0));
        for code in [0, 62, 63, 64, 200] {
            assert_eq!(gain_code_linear(code), None, "code {code}");
        }
    }

    #[test]
    fn spherical_calibration_and_cartesian_conversion() {
        assert_eq!(
            SphericalPosition::from_codes(193, 60, 0),
            SphericalPosition {
                azimuth_half_degrees: 220,
                elevation_half_degrees: 0,
                distance_64ths: 0
            }
        );
        assert_eq!(
            SphericalPosition::from_codes(120, 77, 63),
            SphericalPosition {
                azimuth_half_degrees: 0,
                elevation_half_degrees: 51,
                distance_64ths: 64
            }
        );
        assert_eq!(
            SphericalPosition::from_codes(23, 79, 63).azimuth_half_degrees,
            -291
        );
        assert_eq!(
            SphericalPosition::from_codes(47, 127, 1).azimuth_half_degrees,
            -220
        );
        assert_eq!(
            SphericalPosition::from_codes(255, 127, 1).elevation_half_degrees,
            180
        );

        let left = SphericalPosition {
            azimuth_half_degrees: -180,
            elevation_half_degrees: 0,
            distance_64ths: 64,
        }
        .to_adm_cartesian();
        assert!((left[0] + 1.0).abs() < 1e-9 && left[1].abs() < 1e-9 && left[2].abs() < 1e-9);
        let up = SphericalPosition {
            azimuth_half_degrees: 0,
            elevation_half_degrees: 180,
            distance_64ths: 32,
        }
        .to_adm_cartesian();
        assert!(up[0].abs() < 1e-9 && up[1].abs() < 1e-9 && (up[2] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn standard_matrix_becomes_height_folds() {
        let payload = standard_payload([(1 << 1, 55), (1 << 2, 55), (1 << 4, 55), (1 << 5, 55)]);
        let metadata = XMetadata::parse(&payload, 4).expect("standard matrix");
        assert_eq!(metadata.source_count(), 4);
        let expected = [
            (SpatialChannel::TopFrontLeft, 1),
            (SpatialChannel::TopFrontRight, 2),
            (SpatialChannel::TopBackLeft, 4),
            (SpatialChannel::TopBackRight, 5),
        ];
        for (feed, (speaker, column)) in expected.into_iter().enumerate() {
            let source = metadata.source(feed).unwrap();
            assert_eq!(source.role, SourceRole::Height(speaker));
            let BedFold::Known(columns) = source.fold else {
                panic!("standard fold must be known");
            };
            for (index, gain) in columns.iter().enumerate() {
                assert_eq!(*gain, if index == column { Q55 } else { 0.0 });
            }
        }
        let plan = FoldPlan::from_metadata(&metadata);
        // Reference column 1 is L (speaker 1), column 4 is Lsr (speaker 7).
        let sources = vec![vec![0.5f32], vec![0.25], vec![0.125], vec![0.0625]];
        assert_eq!(plan.clean(1, 1.0, 0, &sources), 1.0 - Q55 * 0.5);
        assert_eq!(plan.clean(7, 1.0, 0, &sources), 1.0 - Q55 * 0.125);
        assert_eq!(plan.clean(0, 1.0, 0, &sources), 1.0);
        assert!((0..4).all(|feed| plan.source_is_known(feed)));

        let changed = standard_payload([(1 << 0, 54), (1 << 2, 55), (1 << 4, 55), (1 << 5, 55)]);
        let metadata = XMetadata::parse(&changed, 4).unwrap();
        let BedFold::Known(columns) = metadata.source(0).unwrap().fold else {
            panic!()
        };
        assert_eq!(columns[0], gain_code_linear(54).unwrap());
        assert_eq!(columns[1], 0.0);
    }

    #[test]
    fn standard_matrix_rejects_corruption_and_wrong_counts() {
        let payload = standard_payload([(1 << 1, 55), (1 << 2, 55), (1 << 4, 55), (1 << 5, 55)]);
        assert!(XMetadata::parse(&payload, 5).is_err());
        for end in 0..21 {
            assert!(XMetadata::parse(&payload[..end], 4).is_err(), "end {end}");
        }
        for bit in 0..21 * 8 {
            let mut damaged = payload.clone();
            damaged[bit / 8] ^= 1 << (bit % 8);
            assert!(XMetadata::parse(&damaged, 4).is_err(), "bit {bit}");
        }
        assert!(XMetadata::parse(&[0xf1, 0x40, 0x00, 0xd9, 0, 0, 0, 0], 5).is_err());
        assert!(XMetadata::parse(&[], 4).is_err());
    }

    fn d3_records() -> Vec<ObjectRecord> {
        vec![
            ObjectRecord {
                mode: 1,
                index: 0,
                distance: 63,
                azimuth: 23,
                elevation: 79,
                row: Some((1 << 4 | 1 << 5, 21)),
                centre_height: false,
            },
            ObjectRecord {
                mode: 1,
                index: 1,
                distance: 63,
                azimuth: 217,
                elevation: 78,
                row: Some((1 << 5, 61)),
                centre_height: false,
            },
            ObjectRecord {
                mode: 1,
                index: 2,
                distance: 63,
                azimuth: 20,
                elevation: 60,
                row: None,
                centre_height: false,
            },
            ObjectRecord {
                mode: 1,
                index: 3,
                distance: 63,
                azimuth: 220,
                elevation: 60,
                row: Some((1 << 5, 61)),
                centre_height: false,
            },
        ]
    }

    #[test]
    fn alternate_objects_and_heights_are_read_from_fields() {
        let payload = alternate_payload(&d3_records(), HEIGHT_ROWS_55);
        let metadata = XMetadata::parse(&payload, 8).expect("alternate metadata");
        assert_eq!(metadata.source_count(), 8);

        let object = metadata.source(0).unwrap();
        let SourceRole::Object {
            position,
            centre_height_alternative,
        } = object.role
        else {
            panic!("source 0 is an object");
        };
        assert_eq!(position.azimuth_half_degrees, -291);
        assert_eq!(position.elevation_half_degrees, 57);
        assert_eq!(position.distance_64ths, 64);
        assert!(!centre_height_alternative);
        let BedFold::Known(columns) = object.fold else {
            panic!()
        };
        let code21 = gain_code_linear(21).unwrap();
        assert_eq!(columns, [0.0, 0.0, 0.0, 0.0, code21, code21, 0.0, 0.0]);

        // An object without a reference row is not in the bed at all.
        assert_eq!(metadata.source(2).unwrap().fold, BedFold::Known([0.0; 8]));

        let heights = [
            SpatialChannel::TopFrontLeft,
            SpatialChannel::TopFrontRight,
            SpatialChannel::TopBackLeft,
            SpatialChannel::TopBackRight,
        ];
        for (feed, speaker) in (4..8).zip(heights) {
            let source = metadata.source(feed).unwrap();
            assert_eq!(source.role, SourceRole::Height(speaker));
            let BedFold::Known(columns) = source.fold else {
                panic!()
            };
            assert_eq!(columns.iter().filter(|&&g| g != 0.0).count(), 1);
            assert_eq!(columns.iter().sum::<f32>(), Q55);
        }

        let plan = FoldPlan::from_metadata(&metadata);
        let sources: Vec<Vec<f32>> = (0..8).map(|k| vec![0.01 * (k + 1) as f32]).collect();
        // Lsr (speaker 7) receives object 0 at code 21 and height TBL (feed 6) at code 55.
        let expected = 1.0 - code21 * 0.01 - Q55 * 0.07;
        assert!((plan.clean(7, 1.0, 0, &sources) - expected).abs() < 1e-6);
        // L (speaker 1) receives only height TFL (feed 4).
        assert!((plan.clean(1, 1.0, 0, &sources) - (1.0 - Q55 * 0.05)).abs() < 1e-6);
        assert!((0..8).all(|feed| plan.source_is_known(feed)));
        assert!(plan.touches(7) && plan.touches(1) && !plan.touches(0));
    }

    #[test]
    fn declaration_order_and_indices_come_from_fields() {
        let mut records = d3_records();
        records.swap(0, 3);
        records.swap(1, 2);
        let payload = alternate_payload(&records, HEIGHT_ROWS_55);
        let metadata = XMetadata::parse(&payload, 8).unwrap();
        let SourceRole::Object { position, .. } = metadata.source(0).unwrap().role else {
            panic!()
        };
        assert_eq!(
            position.azimuth_half_degrees, -291,
            "index 0 keeps its record"
        );
        let SourceRole::Object { position, .. } = metadata.source(3).unwrap().role else {
            panic!()
        };
        assert_eq!(position.azimuth_half_degrees, 300);

        let mut duplicate = d3_records();
        duplicate[1].index = 0;
        assert!(XMetadata::parse(&alternate_payload(&duplicate, HEIGHT_ROWS_55), 8).is_err());
        assert!(
            XMetadata::parse(&payload, 7).is_err(),
            "count must match the declarations"
        );
        assert!(XMetadata::parse(&payload, 4).is_err());
    }

    #[test]
    fn mode0_objects_have_positions_but_no_usable_fold() {
        let records = vec![
            ObjectRecord {
                mode: 0,
                index: 0,
                distance: 63,
                azimuth: 97,
                elevation: 68,
                row: None,
                centre_height: false,
            },
            ObjectRecord {
                mode: 0,
                index: 1,
                distance: 63,
                azimuth: 143,
                elevation: 68,
                row: None,
                centre_height: false,
            },
        ];
        let payload = alternate_payload(&records, HEIGHT_ROWS_55);
        let metadata = XMetadata::parse(&payload, 6).unwrap();
        for feed in 0..2 {
            let source = metadata.source(feed).unwrap();
            assert_eq!(source.fold, BedFold::Unknown);
            let SourceRole::Object { position, .. } = source.role else {
                panic!()
            };
            assert_eq!(
                position.azimuth_half_degrees,
                if feed == 0 { -69 } else { 69 }
            );
            assert_eq!(position.elevation_half_degrees, 24);
        }
        let plan = FoldPlan::from_metadata(&metadata);
        assert!(!plan.source_is_known(0) && !plan.source_is_known(1));
        assert!((2..6).all(|feed| plan.source_is_known(feed)));
        assert!(!plan.source_is_known(6));
        // Heights still unfold; the objects stay in the bed untouched.
        let sources: Vec<Vec<f32>> = (0..6).map(|_| vec![0.1f32]).collect();
        assert!((plan.clean(1, 1.0, 0, &sources) - (1.0 - Q55 * 0.1)).abs() < 1e-6);
    }

    #[test]
    fn d0_object_carries_its_centre_height_alternative_and_unity_heights() {
        let records = vec![ObjectRecord {
            mode: 1,
            index: 0,
            distance: 63,
            azimuth: 120,
            elevation: 77,
            row: Some((1 << 0 | 1 << 1 | 1 << 2, 46)),
            centre_height: true,
        }];
        let rows = [
            (1 << 1 | 1 << 4, 61),
            (1 << 2 | 1 << 5, 61),
            (1 << 6 | 1 << 10, 61),
            (1 << 7 | 1 << 11, 61),
        ];
        let payload = alternate_payload(&records, rows);
        let metadata = XMetadata::parse(&payload, 5).unwrap();
        let object = metadata.source(0).unwrap();
        assert!(matches!(
            object.role,
            SourceRole::Object {
                centre_height_alternative: true,
                ..
            }
        ));
        let plan = FoldPlan::from_metadata(&metadata);
        let sources: Vec<Vec<f32>> = (0..5).map(|k| vec![0.1 * (k + 1) as f32]).collect();
        let code46 = gain_code_linear(46).unwrap();
        // C receives the object only; L receives the object and TFL at unity.
        assert!((plan.clean(0, 1.0, 0, &sources) - (1.0 - code46 * 0.1)).abs() < 1e-6);
        assert!((plan.clean(1, 1.0, 0, &sources) - (1.0 - code46 * 0.1 - 0.2)).abs() < 1e-6);
    }

    #[test]
    fn every_truncation_and_extension_of_an_alternate_prefix_is_rejected() {
        let payload = alternate_payload(&d3_records(), HEIGHT_ROWS_55);
        let (prefix_len, _) = alternate_prefix_len(&payload).unwrap();
        for end in 0..prefix_len {
            assert!(XMetadata::parse(&payload[..end], 8).is_err(), "end {end}");
        }
        for bit in 0..prefix_len * 8 {
            let mut damaged = payload.clone();
            damaged[bit / 8] ^= 1 << (bit % 8);
            assert!(XMetadata::parse(&damaged, 8).is_err(), "bit {bit}");
        }
    }

    #[test]
    fn unfamiliar_forms_are_errors_not_guesses() {
        let mut rows = HEIGHT_ROWS_55;
        rows[0] = (1 << 1, 55); // a height row without its own speaker
        assert!(XMetadata::parse(&alternate_payload(&d3_records(), rows), 8).is_err());
        let mut rows = HEIGHT_ROWS_55;
        rows[1] = (1 << 2 | 1 << 4 | 1 << 5, 55); // two height columns
        assert!(XMetadata::parse(&alternate_payload(&d3_records(), rows), 8).is_err());
        let mut records = d3_records();
        records[0].row = Some((1 << 4, 62)); // escape code
        assert!(XMetadata::parse(&alternate_payload(&records, HEIGHT_ROWS_55), 8).is_err());
    }

    #[test]
    fn type3_accepts_an_explicit_reference_mask() {
        let payload = alternate_payload_with(&d3_records(), HEIGHT_ROWS_55, true);
        let metadata = XMetadata::parse(&payload, 8).expect("explicit type-3 mask");
        assert_eq!(
            metadata.source(4).unwrap().role,
            SourceRole::Height(SpatialChannel::TopFrontLeft)
        );
    }

    #[test]
    fn object_only_variant_on_a_5_1_bed_has_one_object_and_no_heights() {
        let record = ObjectRecord {
            mode: 1,
            index: 0,
            distance: 63,
            azimuth: 120,
            elevation: 77,
            row: Some((1 << 0 | 1 << 1 | 1 << 2, 46)),
            centre_height: false,
        };
        let payload = object_only_payload(&record);
        let metadata = XMetadata::parse(&payload, 1).expect("object-only metadata");
        assert_eq!(metadata.source_count(), 1);
        assert_eq!(metadata.reference_speakers(), &[0, 1, 2, 3, 4, 5]);
        let object = metadata.source(0).unwrap();
        let SourceRole::Object { position, .. } = object.role else {
            panic!("the single feed is an object");
        };
        assert_eq!(position.elevation_half_degrees, 51);
        let code46 = gain_code_linear(46).unwrap();
        assert_eq!(
            object.fold,
            BedFold::Known([code46, code46, code46, 0.0, 0.0, 0.0, 0.0, 0.0])
        );
        // Only the six 5.1 speakers can receive a fold; the front three do.
        let plan = FoldPlan::from_metadata(&metadata);
        let sources = vec![vec![0.5f32]];
        assert!((plan.clean(0, 1.0, 0, &sources) - (1.0 - code46 * 0.5)).abs() < 1e-6);
        assert!((plan.clean(2, 1.0, 0, &sources) - (1.0 - code46 * 0.5)).abs() < 1e-6);
        assert_eq!(plan.clean(3, 1.0, 0, &sources), 1.0);
        assert_eq!(plan.clean(7, 1.0, 0, &sources), 1.0);
        assert!(plan.source_is_known(0));

        // The waveform count must match: no height quartet here.
        assert!(XMetadata::parse(&payload, 5).is_err());
        for end in 0..payload.len() - 8 {
            assert!(XMetadata::parse(&payload[..end], 1).is_err(), "end {end}");
        }
    }

    /// Deterministic pseudo-noise in [-1, 1].
    fn noise(seed: u64, length: usize) -> Vec<f32> {
        // splitmix64: streams from neighbouring seeds are uncorrelated,
        // which a linear congruential generator would not give.
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (0..length)
            .map(|_| {
                state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = state;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^= z >> 31;
                ((z >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
            })
            .collect()
    }

    fn quiet(signal: &[f32]) -> Vec<f32> {
        signal.iter().map(|v| v * 0.3).collect()
    }

    fn mixed(dry: &[f32], parts: &[(&[f32], f32)]) -> Vec<f32> {
        dry.iter()
            .enumerate()
            .map(|(s, &v)| v + parts.iter().map(|(x, g)| x[s] * g).sum::<f32>())
            .collect()
    }

    fn unknown_objects(count: usize) -> XMetadata {
        let sources: Vec<SourceMetadata> = (0..count)
            .map(|i| SourceMetadata {
                role: SourceRole::Object {
                    position: SphericalPosition {
                        azimuth_half_degrees: (i as i16) * 60,
                        elevation_half_degrees: 0,
                        distance_64ths: 64,
                    },
                    centre_height_alternative: false,
                },
                fold: BedFold::Unknown,
            })
            .collect();
        XMetadata::from_sources(&sources).expect("metadata")
    }

    #[test]
    fn estimator_recovers_an_unstated_fold_from_the_bed() {
        let n = 2048;
        let x = [noise(1, n), noise(2, n)];
        let dry = [quiet(&noise(3, n)), quiet(&noise(4, n))];
        let mut bed: Vec<Option<Vec<f32>>> = vec![None; 9];
        // L carries feed 0 at 0.92 and feed 1 at 0.10; Rs carries feed 1 at 0.34.
        bed[1] = Some(mixed(&dry[0], &[(&x[0], 0.92), (&x[1], 0.10)]));
        bed[4] = Some(mixed(&dry[1], &[(&x[1], 0.34)]));
        bed[0] = Some(dry[0].clone());

        let mut plan = FoldPlan::from_metadata(&unknown_objects(2));
        assert!(plan.has_unknown());
        let mut estimator = FoldEstimator::new();
        estimator.refine(&mut plan, &bed, &x);

        assert!(!plan.has_unknown());
        assert!(plan.source_is_known(0) && plan.source_is_known(1));
        assert!(
            (plan.gain(1, 0) - 0.92).abs() < 0.03,
            "L/feed0 {}",
            plan.gain(1, 0)
        );
        assert!(
            (plan.gain(1, 1) - 0.10).abs() < 0.03,
            "L/feed1 {}",
            plan.gain(1, 1)
        );
        assert!(
            (plan.gain(4, 1) - 0.34).abs() < 0.03,
            "Rs/feed1 {}",
            plan.gain(4, 1)
        );
        assert!(plan.gain(4, 0).abs() < 0.03, "Rs/feed0 {}", plan.gain(4, 0));
        assert!(plan.gain(0, 0).abs() < 0.03, "C carries nothing");
        // The cleaned L is the dry signal, within the fit's noise.
        let residual: f32 = (0..n)
            .map(|s| (plan.clean(1, bed[1].as_ref().unwrap()[s], s, &x) - dry[0][s]).powi(2))
            .sum::<f32>()
            / n as f32;
        assert!(residual < 1e-4, "residual {residual}");
        // Untouched speakers pass through.
        assert_eq!(plan.clean(7, 0.25, 3, &x), 0.25);
    }

    #[test]
    fn estimator_separates_waveforms_that_share_content() {
        let n = 2048;
        let a = noise(11, n);
        let b: Vec<f32> = noise(12, n)
            .iter()
            .zip(&a)
            .map(|(&y, &x)| 0.6 * x + 0.5 * y)
            .collect();
        let x = [a.clone(), b];
        let mut bed: Vec<Option<Vec<f32>>> = vec![None; 9];
        // Only feed 0 is in L; a one-at-a-time fit would credit feed 1 too.
        bed[1] = Some(mixed(&quiet(&noise(13, n)), &[(&x[0], 1.0)]));

        let mut plan = FoldPlan::from_metadata(&unknown_objects(2));
        FoldEstimator::new().refine(&mut plan, &bed, &x);
        assert!((plan.gain(1, 0) - 1.0).abs() < 0.05, "{}", plan.gain(1, 0));
        assert!(plan.gain(1, 1).abs() < 0.05, "{}", plan.gain(1, 1));
    }

    #[test]
    fn estimator_ramps_from_the_previous_frame_and_holds_through_silence() {
        let n = 2048;
        let x = [noise(21, n)];
        let mut bed: Vec<Option<Vec<f32>>> = vec![None; 9];
        bed[2] = Some(mixed(&quiet(&noise(22, n)), &[(&x[0], 0.5)]));
        let mut estimator = FoldEstimator::new();

        // First frame: no history, the estimate applies flat.
        let mut plan = FoldPlan::from_metadata(&unknown_objects(1));
        estimator.refine(&mut plan, &bed, &x);
        assert!((plan.gain(2, 0) - 0.5).abs() < 0.05);
        assert_eq!(plan.slope[2][0], 0.0);

        // Second frame at a new gain: starts at the old one and ramps halfway
        // to the new measurement.
        bed[2] = Some(mixed(&quiet(&noise(23, n)), &[(&x[0], 0.9)]));
        let mut plan = FoldPlan::from_metadata(&unknown_objects(1));
        estimator.refine(&mut plan, &bed, &x);
        let start = plan.gain(2, 0);
        let end = start + plan.slope[2][0] * n as f32;
        assert!((start - 0.5).abs() < 0.05, "start {start}");
        assert!((end - 0.7).abs() < 0.05, "end {end}");
        let first = plan.clean(2, bed[2].as_ref().unwrap()[0], 0, &x);
        assert!((first - (bed[2].as_ref().unwrap()[0] - start * x[0][0])).abs() < 1e-6);

        // Third frame, the waveform silent: the last gains stand, flat, and the
        // waveform is still rendered (as silence).
        let silent = [vec![0.0f32; n]];
        let mut plan = FoldPlan::from_metadata(&unknown_objects(1));
        estimator.refine(&mut plan, &bed, &silent);
        assert!(plan.source_is_known(0));
        assert!((plan.gain(2, 0) - end).abs() < 1e-6);
        assert_eq!(plan.slope[2][0], 0.0);
    }

    #[test]
    fn estimator_leaves_stated_folds_alone() {
        let n = 2048;
        let x = [noise(31, n), noise(32, n)];
        let mut bed: Vec<Option<Vec<f32>>> = vec![None; 9];
        bed[1] = Some(mixed(&quiet(&noise(33, n)), &[(&x[0], 0.7), (&x[1], 0.7)]));
        let mut known = [0.0f32; REFERENCE_CHANNELS];
        known[1] = 0.25; // states feed 0 in L at 0.25, whatever the audio says
        let sources = [
            SourceMetadata {
                role: SourceRole::Height(SpatialChannel::TopFrontLeft),
                fold: BedFold::Known(known),
            },
            unknown_objects(1).source(0).copied().unwrap(),
        ];
        let mut plan = FoldPlan::from_metadata(&XMetadata::from_sources(&sources).unwrap());
        FoldEstimator::new().refine(&mut plan, &bed, &x);
        assert_eq!(plan.gain(1, 0), 0.25, "a stated gain is not re-estimated");
        assert!(plan.source_is_known(1));
        // Feed 1's estimate absorbs what feed 0's stated gain under-removes only
        // insofar as the two are correlated; independent noise leaves it at 0.7.
        assert!((plan.gain(1, 1) - 0.7).abs() < 0.1, "{}", plan.gain(1, 1));
    }

    #[test]
    fn fallback_plans_behave_as_documented() {
        let unknown = FoldPlan::all_unknown(6);
        assert!((0..6).all(|feed| !unknown.source_is_known(feed)));
        let sources = vec![vec![0.5f32]; 6];
        assert_eq!(unknown.clean(1, 0.3, 0, &sources), 0.3);

        let standard = FoldPlan::standard_heights(Q55);
        let sources = vec![vec![0.1f32], vec![0.2], vec![0.3], vec![0.4]];
        assert!((standard.clean(1, 1.0, 0, &sources) - (1.0 - Q55 * 0.1)).abs() < 1e-6);
        assert!((standard.clean(8, 1.0, 0, &sources) - (1.0 - Q55 * 0.4)).abs() < 1e-6);
        assert_eq!(standard.clean(3, 1.0, 0, &sources), 1.0);
        assert!((0..4).all(|feed| standard.source_is_known(feed)));

        let short: Vec<Vec<f32>> = vec![Vec::new(); 4];
        assert_eq!(
            standard.clean(1, 1.0, 0, &short),
            1.0,
            "missing samples contribute nothing"
        );
        assert_eq!(standard.clean(FOLD_SPEAKERS + 3, 1.0, 0, &sources), 1.0);
    }
}
