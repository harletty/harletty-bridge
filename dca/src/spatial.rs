//! Spatial-extension presentations: what the DTS:X extension waveforms in an
//! [`HdFrame`] mean.
//!
//! The lossless decoder recovers the extension channel set as a bag of
//! waveforms with no one-to-one speaker mask. Deciding that "waveform 2 of a
//! five-feed alternate profile is top-front-right" is *codec* knowledge, so it
//! lives here rather than in a consumer — the realtime bridge and the offline
//! ADM/DAMF exporter both need it and must not disagree about it.
//!
//! What deliberately does NOT live here: fold/unfold gains and the per-sample
//! recombination. Those are presentation choices a renderer makes, and the
//! bridge owns them.
//!
//! Everything below is expressed as plain data. This module has no notion of
//! any consumer's channel-label ABI.
//!
//! # Status
//!
//! Only [`XPresentation::Height`] is established. The alternate profiles are
//! research results from a finite corpus; each carries its provenance in the
//! docs below. They are exposed so consumers can render *something* coherent,
//! not because the mapping is confirmed normative.

use crate::hd::HdFrame;

/// A speaker position an extension waveform belongs at.
///
/// Intentionally narrow: only positions the extension channel sets actually
/// use. The main bed uses [`crate::BedChannel`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpatialChannel {
    TopFrontLeft,
    TopFrontRight,
    TopFrontCenter,
    TopSideLeft,
    TopSideRight,
    TopBackLeft,
    TopBackRight,
    WideLeft,
    WideRight,
}

/// The spatial-extension presentation carried by a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XPresentation {
    /// Standard DTS:X: four full-coded height waveforms folded into the 7.1
    /// bed. The stable stereo pairs are front-height L/R then rear-height L/R.
    Height,
    /// Experimental five-feed alternate profile. Inferred from two complete
    /// programmes: the first feed has a stable front-centre signature and the
    /// remaining four form the established top quartet.
    FixedD0,
    /// Experimental six-feed alternate profile. Inferred from a single complete
    /// programme. The wide fold is strongly measurable, but all six channel
    /// identities remain experimental pending an independent source.
    FixedD1,
    /// Experimental eight-feed alternate profile, presented as named objects at
    /// static inferred positions. See [`XPresentation::object_positions`].
    ObjectsD3,
}

const HEIGHT_CHANNELS: [SpatialChannel; 4] = [
    SpatialChannel::TopFrontLeft,
    SpatialChannel::TopFrontRight,
    SpatialChannel::TopBackLeft,
    SpatialChannel::TopBackRight,
];

const D0_CHANNELS: [SpatialChannel; 5] = [
    SpatialChannel::TopFrontCenter,
    SpatialChannel::TopFrontLeft,
    SpatialChannel::TopFrontRight,
    SpatialChannel::TopBackLeft,
    SpatialChannel::TopBackRight,
];

const D1_CHANNELS: [SpatialChannel; 6] = [
    SpatialChannel::TopFrontLeft,
    SpatialChannel::TopFrontRight,
    SpatialChannel::WideLeft,
    SpatialChannel::WideRight,
    SpatialChannel::TopBackLeft,
    SpatialChannel::TopBackRight,
];

/// Research-only D3 default positions, as `[x, y, z]` with x left-to-right,
/// y back-to-front and z floor-to-ceiling, each in `-1.0..=1.0`.
///
/// The corpus supports stable left/right pairing and a recurring rear
/// association for feeds 6/7. Feeds 2/3 also match the front-elevation group in
/// the paired fixed-layout control, while 4/5 remain the broad side pair.
/// Feeds 0/1 are the least stable pair and are placed at the front-wide
/// positions by elimination. These are presentation defaults, not decoded
/// coordinates and not a claimed normative speaker map.
const D3_OBJECT_POSITIONS: [[f64; 3]; 8] = [
    [-1.0, 0.5, 0.0],  // 0: wide left
    [1.0, 0.5, 0.0],   // 1: wide right
    [-1.0, 1.0, 1.0],  // 2: top front left
    [1.0, 1.0, 1.0],   // 3: top front right
    [-1.0, 0.0, 1.0],  // 4: top side left
    [1.0, 0.0, 1.0],   // 5: top side right
    [-1.0, -1.0, 1.0], // 6: top back left
    [1.0, -1.0, 1.0],  // 7: top back right
];

/// The position each D3 feed is nominally associated with, parallel to
/// [`D3_OBJECT_POSITIONS`]. Advisory only — a consumer that renders D3 as
/// objects should use the positions, not these labels.
const D3_CHANNEL_HINTS: [SpatialChannel; 8] = [
    SpatialChannel::WideLeft,
    SpatialChannel::WideRight,
    SpatialChannel::TopFrontLeft,
    SpatialChannel::TopFrontRight,
    SpatialChannel::TopSideLeft,
    SpatialChannel::TopSideRight,
    SpatialChannel::TopBackLeft,
    SpatialChannel::TopBackRight,
];

impl XPresentation {
    /// Classifies the extension channel set of `frame`, or `None` when there is
    /// no usable one.
    ///
    /// A presentation is only reported when every extension waveform is present
    /// at the frame's bed length, so a consumer can index them without
    /// re-validating. The four-feed standard layout is tested first and wins
    /// outright: a frame that somehow flags an alternate profile while carrying
    /// a plain height quartet is treated as standard DTS:X.
    pub fn detect(frame: &HdFrame) -> Option<Self> {
        let sample_count = frame.bed_sample_count();
        if sample_count == 0 {
            return None;
        }
        let feeds_are = |n: usize| {
            frame.x_samples.len() == n
                && frame
                    .x_samples
                    .iter()
                    .all(|channel| channel.len() == sample_count)
        };

        if feeds_are(HEIGHT_CHANNELS.len()) {
            return Some(Self::Height);
        }
        if !frame.x_imax {
            return None;
        }
        match frame.x_samples.len() {
            n if n == D0_CHANNELS.len() && feeds_are(n) => Some(Self::FixedD0),
            n if n == D1_CHANNELS.len() && feeds_are(n) => Some(Self::FixedD1),
            n if n == D3_OBJECT_POSITIONS.len() && feeds_are(n) => Some(Self::ObjectsD3),
            _ => None,
        }
    }

    /// Number of extension waveforms this presentation carries.
    pub fn feed_count(self) -> usize {
        self.channels().len()
    }

    /// Speaker position of each extension waveform, in feed order.
    ///
    /// For [`XPresentation::ObjectsD3`] these are advisory hints; that
    /// presentation is meant to be rendered as objects via
    /// [`XPresentation::object_positions`].
    pub fn channels(self) -> &'static [SpatialChannel] {
        match self {
            Self::Height => &HEIGHT_CHANNELS,
            Self::FixedD0 => &D0_CHANNELS,
            Self::FixedD1 => &D1_CHANNELS,
            Self::ObjectsD3 => &D3_CHANNEL_HINTS,
        }
    }

    /// Static positions for a presentation whose feeds are objects rather than
    /// fixed channels, or `None` for the fixed presentations.
    pub fn object_positions(self) -> Option<&'static [[f64; 3]]> {
        match self {
            Self::ObjectsD3 => Some(&D3_OBJECT_POSITIONS),
            _ => None,
        }
    }

    /// Whether this presentation's channel identities are inferred research
    /// results rather than an established mapping. Consumers should say so
    /// when they surface it to a user.
    pub fn is_experimental(self) -> bool {
        !matches!(self, Self::Height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_with(x_samples: Vec<Vec<f32>>, x_imax: bool, bed_len: usize) -> HdFrame {
        HdFrame {
            samples: vec![Some(vec![0.0; bed_len])],
            x_samples,
            x_imax,
            ..HdFrame::default()
        }
    }

    #[test]
    fn detects_standard_height_quartet() {
        let f = frame_with(vec![vec![0.0; 512]; 4], false, 512);
        assert_eq!(XPresentation::detect(&f), Some(XPresentation::Height));
        assert!(!XPresentation::Height.is_experimental());
    }

    #[test]
    fn alternate_profiles_need_the_imax_flag() {
        for n in [5usize, 6, 8] {
            let f = frame_with(vec![vec![0.0; 512]; n], false, 512);
            assert_eq!(XPresentation::detect(&f), None, "{n} feeds without x_imax");
        }
    }

    #[test]
    fn detects_each_alternate_profile() {
        for (n, expected) in [
            (5usize, XPresentation::FixedD0),
            (6, XPresentation::FixedD1),
            (8, XPresentation::ObjectsD3),
        ] {
            let f = frame_with(vec![vec![0.0; 512]; n], true, 512);
            assert_eq!(XPresentation::detect(&f), Some(expected));
            assert!(expected.is_experimental());
            assert_eq!(expected.feed_count(), n);
        }
    }

    /// The quartet wins even when the alternate-profile flag is set, matching
    /// the order the realtime path has always used.
    #[test]
    fn height_quartet_outranks_the_imax_flag() {
        let f = frame_with(vec![vec![0.0; 512]; 4], true, 512);
        assert_eq!(XPresentation::detect(&f), Some(XPresentation::Height));
    }

    #[test]
    fn rejects_ragged_or_short_feeds() {
        // One feed of the wrong length invalidates the whole set.
        let mut x = vec![vec![0.0f32; 512]; 4];
        x[2].truncate(511);
        assert_eq!(XPresentation::detect(&frame_with(x, false, 512)), None);

        // Feed counts that match no presentation.
        for n in [0usize, 1, 2, 3, 7, 9] {
            let f = frame_with(vec![vec![0.0; 512]; n], true, 512);
            assert_eq!(XPresentation::detect(&f), None, "{n} feeds");
        }
    }

    #[test]
    fn a_bedless_frame_detects_nothing() {
        let f = HdFrame {
            samples: vec![None],
            x_samples: vec![vec![0.0; 512]; 4],
            ..HdFrame::default()
        };
        assert_eq!(XPresentation::detect(&f), None);
    }

    #[test]
    fn only_d3_carries_object_positions() {
        assert_eq!(XPresentation::Height.object_positions(), None);
        assert_eq!(XPresentation::FixedD0.object_positions(), None);
        assert_eq!(XPresentation::FixedD1.object_positions(), None);
        let d3 = XPresentation::ObjectsD3.object_positions().unwrap();
        assert_eq!(d3.len(), XPresentation::ObjectsD3.feed_count());
        // Left feeds sit left of centre, right feeds right of it.
        for (i, pos) in d3.iter().enumerate() {
            let expected_sign = if i % 2 == 0 { -1.0 } else { 1.0 };
            assert_eq!(pos[0].signum(), expected_sign, "feed {i} laterality");
            assert!(pos.iter().all(|c| (-1.0..=1.0).contains(c)), "feed {i} range");
        }
    }

    #[test]
    fn channel_tables_match_their_feed_counts() {
        for p in [
            XPresentation::Height,
            XPresentation::FixedD0,
            XPresentation::FixedD1,
            XPresentation::ObjectsD3,
        ] {
            assert_eq!(p.channels().len(), p.feed_count());
        }
    }
}
