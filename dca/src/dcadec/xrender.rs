// SPDX-License-Identifier: Apache-2.0
//! The bed fold of a DTS:X object whose record states no gains.
//!
//! An object record in mode 0 carries a position and no reference rows, yet
//! its waveform is in the compatible bed: the encoder panned it there with its
//! own object renderer, from the same position. Re-running that renderer over
//! the bed layout gives the gains to subtract. The law below reproduces the
//! measured fold of the static unstated objects of the corpus to four digits
//! (at (-34.5, 12) degrees on 7.1: L 0.9152, C and R 0.1305, Lss 0.3430,
//! Lsr 0.1037), which neither the ETSI TS 103 584 predefined virtual speakers
//! nor any blind fit matched. See `docs/private-metadata-probe.md`.
//!
//! The renderer is a VBAP over the convex hull of the bed speakers:
//!
//! - every full-range bed speaker sits on the horizontal ring (the side pair
//!   at +-90 degrees, whatever the bed calls it); a bed with no speaker above
//!   25 degrees gets a virtual speaker at +45 degrees over each real one, and
//!   one at -45 degrees under it when none is below -25;
//! - every facet of the hull is kept, including both triangulations of a
//!   coplanar quad; a position inside several facets takes their mean, a
//!   position on an edge counts each of its two facets half;
//! - the object's gains are the square roots of its VBAP gains;
//! - each virtual speaker is folded into the real ones through a row made of
//!   the plain VBAP gains at its azimuth -45, 0 and +45 degrees on the ring,
//!   weighted 0.707, 1, 0.707, summed in power and normalised; a record flag
//!   instead folds it whole into the speaker it stands over;
//! - the folds add in power, and the result is normalised to unit power.
//!
//! The hull is built once per bed layout; a position's gains are computed
//! when it changes, never per sample.

use crate::dcadec::xmeta::{FoldPlan, MAX_SOURCES, REFERENCE_CHANNELS, SourceRole, XMetadata};

/// Full-range speakers the hull can hold (a reference layout minus its LFE).
const MAX_REAL: usize = REFERENCE_CHANNELS;
/// Real speakers plus one virtual ring above and one below.
const MAX_POINTS: usize = 3 * MAX_REAL;
/// Facets of the widest hull: a coplanar octagon on each ring keeps all 56 of
/// its triangles, plus four per side band.
const MAX_TRIANGLES: usize = 256;
/// Tolerance of the hull and containment tests.
const EPSILON: f64 = 1e-6;
/// How far outside a facet's plane another point may sit and still leave the
/// facet on the hull.
const HULL_SLACK: f64 = 1.000_001;

/// Horizontal azimuth (degrees, negative to the left) of a full-range DCA
/// reference speaker; `None` for the LFE and speakers no reference layout
/// carries.
fn speaker_azimuth(speaker: u8) -> Option<f64> {
    Some(match speaker {
        0 => 0.0,
        1 => -30.0,
        2 => 30.0,
        // Ls/Rs on a 5.1 bed and Lss/Rss on 7.1 share these indices; the
        // renderer places both at the sides.
        3 => -90.0,
        4 => 90.0,
        6 => 180.0,
        7 => -150.0,
        8 => 150.0,
        _ => return None,
    })
}

fn direction(azimuth: f64, elevation: f64) -> [f64; 3] {
    let (a, e) = (azimuth.to_radians(), elevation.to_radians());
    [a.sin() * e.cos(), e.sin(), -a.cos() * e.cos()]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

#[derive(Clone, Copy, Debug)]
struct Triangle {
    vertices: [usize; 3],
    /// Row `k` dotted with a direction gives vertex `k`'s VBAP gain.
    inverse: [[f64; 3]; 3],
}

impl Triangle {
    fn new(points: &[[f64; 3]], vertices: [usize; 3]) -> Option<Self> {
        let [a, b, c] = vertices.map(|v| points[v]);
        let det = dot(a, cross(b, c));
        if det.abs() <= EPSILON {
            return None;
        }
        let inverse = [cross(b, c), cross(c, a), cross(a, b)].map(|row| row.map(|v| v / det));
        Some(Self { vertices, inverse })
    }

    fn gains(&self, p: [f64; 3]) -> [f64; 3] {
        self.inverse.map(|row| dot(row, p))
    }
}

/// The bed layout's hull and virtual-speaker folds.
#[derive(Clone, Debug)]
struct Hull {
    /// Reference column of each real speaker.
    columns: [usize; MAX_REAL],
    real: usize,
    point_count: usize,
    /// Real speaker each virtual point `real + k` stands over.
    under: [usize; MAX_POINTS],
    /// Fold row of each virtual point `real + k` over the real speakers.
    fold: [[f64; MAX_REAL]; MAX_POINTS],
    triangles: [Triangle; MAX_TRIANGLES],
    triangle_count: usize,
}

impl Hull {
    /// `None` for a layout with fewer than three full-range speakers, or
    /// whose hull leaves a direction uncovered: a 5.1 bed has nothing behind
    /// its side pair, so the rear virtual folds cannot be built (the
    /// reference decoder refuses such a layout too). Its rendered objects
    /// stay unknown to the plan.
    fn new(reference_speakers: &[u8]) -> Option<Self> {
        let mut columns = [0usize; MAX_REAL];
        let mut azimuths = [0.0f64; MAX_REAL];
        let mut real = 0;
        for (column, &speaker) in reference_speakers.iter().enumerate() {
            if let Some(azimuth) = speaker_azimuth(speaker) {
                if real == MAX_REAL {
                    return None;
                }
                columns[real] = column;
                azimuths[real] = azimuth;
                real += 1;
            }
        }
        if real < 3 {
            return None;
        }
        let mut points = [[0.0; 3]; MAX_POINTS];
        let mut under = [0usize; MAX_POINTS];
        for (r, &azimuth) in azimuths.iter().enumerate().take(real) {
            points[r] = direction(azimuth, 0.0);
            points[real + r] = direction(azimuth, 45.0);
            points[2 * real + r] = direction(azimuth, -45.0);
            under[real + r] = r;
            under[2 * real + r] = r;
        }
        let point_count = 3 * real;
        let points_used = &points[..point_count];

        let placeholder = Triangle {
            vertices: [0; 3],
            inverse: [[0.0; 3]; 3],
        };
        let mut triangles = [placeholder; MAX_TRIANGLES];
        let mut triangle_count = 0;
        for i in 0..point_count {
            for j in i + 1..point_count {
                for k in j + 1..point_count {
                    let heights = [points[i][1], points[j][1], points[k][1]];
                    let upper = heights.iter().all(|&y| y >= -EPSILON);
                    let lower = heights.iter().all(|&y| y <= EPSILON);
                    if !upper && !lower {
                        continue;
                    }
                    let Some(triangle) = Triangle::new(points_used, [i, j, k]) else {
                        continue;
                    };
                    // A facet of the upper (lower) hull has no point of the
                    // upper (lower) half beyond its plane.
                    let on_hull = points_used.iter().all(|&p| {
                        (upper && !lower && p[1] < -EPSILON)
                            || (lower && !upper && p[1] > EPSILON)
                            || triangle.gains(p).iter().sum::<f64>() <= HULL_SLACK
                    });
                    if on_hull {
                        if triangle_count == MAX_TRIANGLES {
                            return None;
                        }
                        triangles[triangle_count] = triangle;
                        triangle_count += 1;
                    }
                }
            }
        }

        let mut hull = Self {
            columns,
            real,
            point_count,
            under,
            fold: [[0.0; MAX_REAL]; MAX_POINTS],
            triangles,
            triangle_count,
        };
        for (r, &azimuth) in azimuths.iter().enumerate().take(real) {
            let row = hull.fold_row(azimuth)?;
            hull.fold[real + r] = row;
            hull.fold[2 * real + r] = row;
        }
        Some(hull)
    }

    /// VBAP gains of direction `p` over every hull point: the mean over the
    /// facets containing it, an edge counting each facet half.
    fn vbap(&self, p: [f64; 3]) -> Option<[f64; MAX_POINTS]> {
        let mut out = [0.0; MAX_POINTS];
        let mut total = 0.0;
        for triangle in &self.triangles[..self.triangle_count] {
            let gains = triangle.gains(p);
            if gains.iter().any(|&g| g < -EPSILON) {
                continue;
            }
            let positive = gains.iter().filter(|&&g| g > EPSILON).count();
            let weight = if positive == 2 { 0.5 } else { 1.0 };
            total += weight;
            for (&vertex, &gain) in triangle.vertices.iter().zip(&gains) {
                out[vertex] += weight * gain;
            }
        }
        if total <= 0.0 {
            return None;
        }
        if total > 1.0 {
            out.iter_mut().for_each(|g| *g /= total);
        }
        Some(out)
    }

    /// Fold of the virtual speakers at `azimuth` into the real ones.
    fn fold_row(&self, azimuth: f64) -> Option<[f64; MAX_REAL]> {
        let mut row = [0.0f64; MAX_REAL];
        for (offset, weight) in [
            (-45.0, std::f64::consts::FRAC_1_SQRT_2),
            (0.0, 1.0),
            (45.0, std::f64::consts::FRAC_1_SQRT_2),
        ] {
            let gains = self.vbap(direction(azimuth + offset, 0.0))?;
            for (acc, &g) in row.iter_mut().zip(&gains[..self.real]) {
                *acc = acc.hypot(g * weight);
            }
        }
        let norm = row.iter().map(|g| g * g).sum::<f64>().sqrt();
        if norm <= 0.0 {
            return None;
        }
        row.iter_mut().for_each(|g| *g /= norm);
        Some(row)
    }

    /// Unit-power gains of an object at (`azimuth`, `elevation`) over the real
    /// speakers. `direct` folds each virtual speaker whole into the speaker
    /// under it instead of through its fold row.
    fn pan(&self, azimuth: f64, elevation: f64, direct: bool) -> Option<[f64; MAX_REAL]> {
        let mut all = self.vbap(direction(azimuth, elevation))?;
        all.iter_mut().for_each(|g| *g = g.max(0.0).sqrt());
        let (real, virtual_points) = all.split_at_mut(self.real);
        for (k, &v) in virtual_points[..self.point_count - self.real]
            .iter()
            .enumerate()
        {
            if v == 0.0 {
                continue;
            }
            let point = self.real + k;
            if direct {
                let target = &mut real[self.under[point]];
                *target = target.hypot(v);
            } else {
                for (g, &f) in real.iter_mut().zip(&self.fold[point][..self.real]) {
                    *g = g.hypot(v * f);
                }
            }
        }
        let mut gains = [0.0; MAX_REAL];
        gains[..self.real].copy_from_slice(real);
        let norm = gains.iter().map(|g| g * g).sum::<f64>().sqrt();
        if norm <= 0.0 {
            return None;
        }
        gains
            .iter_mut()
            .for_each(|g| *g = (*g / norm).clamp(0.0, 1.0));
        Some(gains)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rendered {
    azimuth: f64,
    elevation: f64,
    direct: bool,
    /// Gain per reference column.
    gains: [f32; REFERENCE_CHANNELS],
}

/// Fills in the bed fold of every object the stream says the encoder
/// rendered into the bed from its position (mode-0 records), frame by frame.
///
/// A position's gains are computed when it changes; across a frame the gains
/// ramp from the previous frame's, so a moving object leaves no steps in the
/// bed. The hull is rebuilt only when the reference layout changes.
#[derive(Clone, Debug, Default)]
pub struct FoldRenderer {
    hull: Option<Box<Hull>>,
    /// Reference speakers the hull was built for; empty before the first
    /// frame.
    layout: Vec<u8>,
    rendered: [Option<Rendered>; MAX_SOURCES],
    /// Bit `k` set once waveform `k` has gains to ramp from.
    primed: u16,
}

impl FoldRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget every object's gains (a new stream, or a presentation change).
    pub fn reset(&mut self) {
        self.rendered = [None; MAX_SOURCES];
        self.primed = 0;
    }

    /// Give `plan` the fold of every waveform it does not know and `metadata`
    /// marks as rendered from its position. `frame_length` is the frame's
    /// sample count, over which the gains ramp. Afterwards such a waveform is
    /// known to the plan, so it plays at its position and leaves the bed.
    pub fn apply(&mut self, plan: &mut FoldPlan, metadata: &XMetadata, frame_length: usize) {
        if !plan.has_unknown() || frame_length == 0 {
            return;
        }
        let speakers = metadata.reference_speakers();
        if self.layout != speakers {
            self.layout = speakers.to_vec();
            self.hull = Hull::new(speakers).map(Box::new);
            self.reset();
        }
        let Some(hull) = self.hull.as_deref() else {
            return;
        };
        for feed in 0..plan.source_count().min(MAX_SOURCES) {
            if plan.source_is_known(feed) || !metadata.fold_is_rendered(feed) {
                continue;
            }
            let Some(SourceRole::Object { position, .. }) = metadata.source(feed).map(|s| s.role)
            else {
                continue;
            };
            let (azimuth, elevation) = (position.azimuth_degrees(), position.elevation_degrees());
            let direct = metadata.fold_is_direct(feed);
            let previous = self.rendered[feed];
            let target = match previous {
                Some(r)
                    if r.azimuth == azimuth && r.elevation == elevation && r.direct == direct =>
                {
                    r.gains
                }
                _ => {
                    let Some(pan) = hull.pan(azimuth, elevation, direct) else {
                        continue;
                    };
                    let mut gains = [0.0f32; REFERENCE_CHANNELS];
                    for (&column, &g) in hull.columns.iter().zip(&pan).take(hull.real) {
                        gains[column] = g as f32;
                    }
                    gains
                }
            };
            let bit = 1u16 << feed;
            let start = match previous {
                Some(r) if self.primed & bit != 0 => r.gains,
                _ => target,
            };
            for (column, &speaker) in speakers.iter().enumerate() {
                plan.set_ramp(
                    usize::from(speaker),
                    feed,
                    start[column],
                    target[column],
                    frame_length,
                );
            }
            plan.mark_known(feed);
            self.rendered[feed] = Some(Rendered {
                azimuth,
                elevation,
                direct,
                gains: target,
            });
            self.primed |= bit;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dcadec::xmeta::{BedFold, REFERENCE_MASK_7_1, SourceMetadata, SphericalPosition};

    /// C, L, R, LFE, Lsr, Rsr, Lss, Rss: the 7.1 reference columns.
    const SPEAKERS_7_1: [u8; 8] = [0, 1, 2, 5, 7, 8, 3, 4];

    fn pan_7_1(azimuth: f64, elevation: f64, direct: bool) -> [f64; 8] {
        let hull = Hull::new(&SPEAKERS_7_1).expect("7.1 hull");
        let gains = hull.pan(azimuth, elevation, direct).expect("pan");
        let mut columns = [0.0; 8];
        for (&column, &g) in hull.columns.iter().zip(&gains).take(hull.real) {
            columns[column] = g;
        }
        columns
    }

    #[test]
    fn reproduces_the_measured_unstated_fold() {
        // Two static mode-0 objects of the corpus, measured in the bed to four
        // digits over nine extracts.
        let g = pan_7_1(-34.5, 12.0, false);
        let expected = [0.1305, 0.9152, 0.1305, 0.0, 0.1037, 0.0, 0.3430, 0.0];
        for (column, (&got, &want)) in g.iter().zip(&expected).enumerate() {
            assert!(
                (got - want).abs() < 5e-5,
                "column {column}: {got} vs {want}"
            );
        }
        let mirrored = pan_7_1(34.5, 12.0, false);
        for (l, r) in [(1, 2), (4, 5), (6, 7)] {
            assert!((g[l] - mirrored[r]).abs() < 1e-12);
        }
    }

    #[test]
    fn a_speaker_position_lands_on_that_speaker() {
        for (azimuth, column) in [(0.0, 0), (-30.0, 1), (30.0, 2), (-90.0, 6), (150.0, 5)] {
            for direct in [false, true] {
                let g = pan_7_1(azimuth, 0.0, direct);
                assert!((g[column] - 1.0).abs() < 1e-9, "{azimuth}: {g:?}");
                assert!(g.iter().map(|v| v * v).sum::<f64>() - 1.0 < 1e-9);
            }
        }
    }

    #[test]
    fn every_position_has_unit_power() {
        for azimuth in (-180..180).step_by(15) {
            for elevation in (-90..=90).step_by(15) {
                let g = pan_7_1(f64::from(azimuth), f64::from(elevation), false);
                let power: f64 = g.iter().map(|v| v * v).sum();
                assert!(
                    (power - 1.0).abs() < 1e-9,
                    "({azimuth}, {elevation}): {power}"
                );
                assert_eq!(g[3], 0.0, "LFE never receives an object");
            }
        }
    }

    #[test]
    fn the_direct_flag_keeps_a_virtual_speaker_over_its_own() {
        // Straight up over the left speaker: the virtual speaker above L takes
        // everything; folded directly it lands in L alone.
        let g = pan_7_1(-30.0, 45.0, true);
        assert!((g[1] - 1.0).abs() < 1e-9, "{g:?}");
        let spread = pan_7_1(-30.0, 45.0, false);
        assert!(spread[0] > 0.2 && spread[6] > 0.2, "{spread:?}");
    }

    #[test]
    fn a_bed_open_at_the_back_has_no_hull() {
        assert!(Hull::new(&[0, 1, 2, 3, 4, 5]).is_none(), "5.1");
        assert!(Hull::new(&[1, 2, 5]).is_none(), "stereo");
        let rear_centre = Hull::new(&[0, 1, 2, 3, 4, 5, 6]).expect("6.1");
        assert_eq!(rear_centre.real, 6);
    }

    fn object(azimuth_half_degrees: i16, elevation_half_degrees: i16) -> SourceMetadata {
        SourceMetadata {
            role: SourceRole::Object {
                position: SphericalPosition {
                    azimuth_half_degrees,
                    elevation_half_degrees,
                    distance_64ths: 64,
                },
                centre_height_alternative: false,
            },
            fold: BedFold::Unknown,
        }
    }

    #[test]
    fn apply_fills_rendered_waveforms_and_ramps_a_move() {
        let sources = [object(-69, 24), object(69, 24)];
        let mut metadata = XMetadata::from_sources_on(&sources, REFERENCE_MASK_7_1).unwrap();
        metadata.mark_rendered(0, false);
        let mut plan = FoldPlan::from_metadata(&metadata);
        let mut renderer = FoldRenderer::new();
        renderer.apply(&mut plan, &metadata, 512);
        assert!(plan.source_is_known(0));
        assert!(
            !plan.source_is_known(1),
            "an unmarked unknown waveform is left alone"
        );
        assert!((plan.gain(1, 0) - 0.9152).abs() < 5e-5);
        assert!((plan.gain(3, 0) - 0.3430).abs() < 5e-5);
        assert!(plan.touches(7) && !plan.touches(8));

        // The object moves: the next frame starts on the old gains and ramps.
        let moved = [object(-120, 24), object(69, 24)];
        let mut metadata = XMetadata::from_sources_on(&moved, REFERENCE_MASK_7_1).unwrap();
        metadata.mark_rendered(0, false);
        let mut plan = FoldPlan::from_metadata(&metadata);
        renderer.apply(&mut plan, &metadata, 512);
        assert!(
            (plan.gain(1, 0) - 0.9152).abs() < 5e-5,
            "starts where it was"
        );
        let bed = vec![0.0f32; 512];
        let x = vec![vec![1.0f32; 512], vec![0.0f32; 512]];
        let mut out = vec![0.0f32; 512];
        plan.clean_channel(3, &bed, &x, &mut out);
        let target = pan_7_1(-60.0, 12.0, false)[6] as f32;
        assert!(
            (-out[511] - target).abs() < 0.01,
            "{} vs {target}",
            -out[511]
        );
    }
}
