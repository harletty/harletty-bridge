//! Spatial-extension presentations: what the DTS:X extension waveforms in an
//! [`HdFrame`] are.
//!
//! The lossless decoder recovers the extension channel set as a bag of
//! waveforms with no one-to-one speaker mask. Deciding that "waveform 5 of an
//! eight-feed alternate profile is top-front-right" is *codec* knowledge, so it
//! lives here rather than in a consumer — the realtime bridge and the offline
//! ADM/DAMF exporter both need it and must not disagree about it.
//!
//! A presentation splits the feeds into **objects**, whose positions and bed
//! folds come from the frame's private metadata ([`crate::XMetadata`]), and
//! **fixed channels** at the positions listed here. What deliberately does NOT
//! live here: the per-sample bed recombination. That is [`crate::FoldPlan`],
//! built from the metadata and applied by each host.
//!
//! Everything below is expressed as plain data. This module has no notion of
//! any consumer's channel-label ABI.
//!
//! # Status
//!
//! [`XPresentation::Height`] is established. The alternate profiles' feed
//! identities were established against a finite corpus by comparing the
//! decoded audio with the private metadata (see the repository's
//! `docs/private-metadata-probe.md`): the last four feeds are the fixed
//! heights named by the type-3 rows, the first feeds are the declared objects,
//! and D0's single object also declares the centre-height speaker as its
//! fixed alternative. The object-only variant on a 5.1 bed carries one
//! declared object and no height quartet at all. They are exposed as presentations rather than as
//! research defaults, but no listening sign-off exists yet.

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
    /// Five-feed alternate profile: one object whose record declares the
    /// centre-height speaker as its fixed alternative, presented as that fixed
    /// channel, then the four fixed heights.
    FixedD0,
    /// Six-feed alternate profile: two objects, then the four fixed heights.
    ObjectsD1,
    /// Eight-feed alternate profile: four objects, then the four fixed
    /// heights.
    ObjectsD3,
    /// Single-feed alternate profile on a 5.1 bed: one object, no fixed
    /// heights. Its envelope carries the first channel set only.
    ObjectOnly,
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
        [
            Self::FixedD0,
            Self::ObjectsD1,
            Self::ObjectsD3,
            Self::ObjectOnly,
        ]
        .into_iter()
        .find(|presentation| feeds_are(presentation.feed_count()))
    }

    /// Number of extension waveforms this presentation carries.
    pub fn feed_count(self) -> usize {
        match self {
            Self::Height => 4,
            Self::FixedD0 => 5,
            Self::ObjectsD1 => 6,
            Self::ObjectsD3 => 8,
            Self::ObjectOnly => 1,
        }
    }

    /// Feeds presented as objects, positioned by the frame's metadata.
    pub fn object_feeds(self) -> std::ops::Range<usize> {
        0..self.feed_count() - self.fixed_channels().len()
    }

    /// Feeds presented as fixed channels, parallel to [`Self::fixed_channels`].
    pub fn fixed_feeds(self) -> std::ops::Range<usize> {
        self.object_feeds().end..self.feed_count()
    }

    /// Speaker position of each fixed feed, in feed order.
    pub fn fixed_channels(self) -> &'static [SpatialChannel] {
        match self {
            Self::Height | Self::ObjectsD1 | Self::ObjectsD3 => &HEIGHT_CHANNELS,
            Self::FixedD0 => &D0_CHANNELS,
            Self::ObjectOnly => &[],
        }
    }

    /// Whether this presentation's feed identities rest on corpus evidence
    /// rather than on the standard profile's established layout. Consumers
    /// should say so when they surface it to a user.
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
        assert_eq!(XPresentation::Height.object_feeds(), 0..0);
        assert_eq!(XPresentation::Height.fixed_feeds(), 0..4);
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
            (6, XPresentation::ObjectsD1),
            (8, XPresentation::ObjectsD3),
            (1, XPresentation::ObjectOnly),
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
        for n in [0usize, 2, 3, 7, 9] {
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

    /// Objects come first, the fixed heights last, and together they cover
    /// every feed exactly once.
    #[test]
    fn feeds_split_into_objects_then_fixed_channels() {
        for (presentation, objects, fixed) in [
            (XPresentation::Height, 0..0, 0..4),
            (XPresentation::FixedD0, 0..0, 0..5),
            (XPresentation::ObjectsD1, 0..2, 2..6),
            (XPresentation::ObjectsD3, 0..4, 4..8),
            (XPresentation::ObjectOnly, 0..1, 1..1),
        ] {
            assert_eq!(presentation.object_feeds(), objects, "{presentation:?}");
            assert_eq!(presentation.fixed_feeds(), fixed, "{presentation:?}");
            assert_eq!(
                presentation.fixed_channels().len(),
                presentation.fixed_feeds().len()
            );
            assert_eq!(
                presentation.object_feeds().len() + presentation.fixed_feeds().len(),
                presentation.feed_count()
            );
        }
        assert_eq!(
            XPresentation::ObjectsD3.fixed_channels(),
            &HEIGHT_CHANNELS,
            "alternate heights are the standard quartet"
        );
        assert_eq!(
            XPresentation::FixedD0.fixed_channels()[0],
            SpatialChannel::TopFrontCenter
        );
    }
}
