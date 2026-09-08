// SPDX-License-Identifier: Apache-2.0
//! Corpus-gated checks of the private-metadata reader against real streams.
//!
//! Inputs come from the `HARLETTY_DTSX_STANDARD_CORPUS`, `HARLETTY_D0_CORPUS`,
//! `HARLETTY_D1_CORPUS` and `HARLETTY_D3_CORPUS` environment variables; each
//! test self-skips, loudly, when its input is absent. A green run without the
//! corpus therefore proves nothing about the reader; run these locally.

use std::io::Read;

use dca::{
    BedFold, FoldPlan, HdDecoder, HdError, SourceRole, SpatialChannel, XMetadata, XPresentation,
    exss_substream_size, parse_header,
};

const PREFIX_BYTES: usize = 4 * 1024 * 1024;
const Q55: f32 = 23170.0 / 32768.0;

fn corpus(var: &str) -> Option<Vec<u8>> {
    let path = std::env::var(var).ok()?;
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("skipping: {var}={path} is not readable ({error})");
            return None;
        }
    };
    let mut bytes = vec![0u8; PREFIX_BYTES];
    let mut read = 0;
    while read < bytes.len() {
        match file.read(&mut bytes[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(error) => {
                eprintln!("skipping: {var} could not be read ({error})");
                return None;
            }
        }
    }
    bytes.truncate(read);
    Some(bytes)
}

struct Survey {
    frames: usize,
    presentation: XPresentation,
    metadata: XMetadata,
}

/// Decode every complete DTS-HD frame of `bytes` and parse its metadata,
/// requiring one presentation throughout and a successful parse on every
/// frame that carries extension waveforms. Returns the last metadata read.
fn survey(bytes: &[u8]) -> Survey {
    let mut decoder = HdDecoder::new();
    let mut offset = 0;
    let mut frames = 0;
    let mut presentation = None;
    let mut metadata = None;
    while offset + 18 <= bytes.len() {
        let info = parse_header(&bytes[offset..]).expect("core header");
        let core_end = offset + info.frame_size;
        if core_end + 16 > bytes.len() {
            break;
        }
        let Some(exss_size) = exss_substream_size(&bytes[core_end..]) else {
            // The prefix ends inside this substream; anything else is a
            // stream the decoder would not play either.
            assert!(
                bytes.len() - core_end < 65_536,
                "EXSS header at byte {core_end} (frame {frames})"
            );
            break;
        };
        let exss_end = core_end + exss_size;
        if exss_end > bytes.len() {
            break;
        }
        match decoder.decode(&bytes[offset..core_end], &bytes[core_end..exss_end]) {
            Ok(frame) => {
                frames += 1;
                let detected = XPresentation::detect(&frame).expect("extension presentation");
                if let Some(previous) = presentation.replace(detected) {
                    assert_eq!(previous, detected, "presentation changed at frame {frames}");
                }
                let parsed = XMetadata::parse(&frame.x_payload, frame.x_samples.len())
                    .unwrap_or_else(|error| panic!("frame {frames}: {error:?}"));
                assert_eq!(parsed.source_count(), detected.feed_count());
                // The last four feeds of every profile are the fixed heights
                // (D0's first fixed feed is its centre-height object).
                for feed in detected.feed_count() - 4..detected.feed_count() {
                    assert!(
                        matches!(parsed.source(feed).unwrap().role, SourceRole::Height(_)),
                        "frame {frames}: feed {feed} is a fixed height"
                    );
                }
                metadata = Some(parsed);
            }
            Err(HdError::Pending) => {}
            Err(error) => panic!("decode error at byte {offset}: {error:?}"),
        }
        offset = exss_end;
    }
    assert!(frames > 100, "corpus was not exercised ({frames} frames)");
    Survey {
        frames,
        presentation: presentation.unwrap(),
        metadata: metadata.unwrap(),
    }
}

/// The reference column that each fixed height (the last four feeds) folds
/// into, in row order.
fn height_fold_columns(metadata: &XMetadata, presentation: XPresentation) -> Vec<(usize, f32)> {
    (presentation.feed_count() - 4..presentation.feed_count())
        .map(|feed| match metadata.source(feed).unwrap().fold {
            BedFold::Known(columns) => {
                let set: Vec<_> = columns
                    .iter()
                    .enumerate()
                    .filter(|(_, gain)| **gain != 0.0)
                    .map(|(column, gain)| (column, *gain))
                    .collect();
                assert_eq!(set.len(), 1, "one bed column per height");
                set[0]
            }
            BedFold::Unknown => panic!("height fold must be known"),
        })
        .collect()
}

#[test]
fn standard_matrix_parses_on_every_frame() {
    let Some(bytes) = corpus("HARLETTY_DTSX_STANDARD_CORPUS") else {
        eprintln!("skipping: HARLETTY_DTSX_STANDARD_CORPUS is not set");
        return;
    };
    let survey = survey(&bytes);
    assert_eq!(survey.presentation, XPresentation::Height);
    assert_eq!(
        height_fold_columns(&survey.metadata, survey.presentation),
        vec![(1, Q55), (2, Q55), (4, Q55), (5, Q55)]
    );
    eprintln!(
        "standard: {} frames, fold code 55 on every height",
        survey.frames
    );
}

#[test]
fn d0_folds_heights_at_unity_and_declares_a_centre_height_object() {
    let Some(bytes) = corpus("HARLETTY_D0_CORPUS") else {
        eprintln!("skipping: HARLETTY_D0_CORPUS is not set");
        return;
    };
    let survey = survey(&bytes);
    assert_eq!(survey.presentation, XPresentation::FixedD0);
    assert_eq!(
        height_fold_columns(&survey.metadata, survey.presentation),
        vec![(1, 1.0), (2, 1.0), (4, 1.0), (5, 1.0)]
    );
    let object = survey.metadata.source(0).unwrap();
    assert!(matches!(
        object.role,
        SourceRole::Object {
            centre_height_alternative: true,
            ..
        }
    ));
    assert!(matches!(object.fold, BedFold::Known(_)));
    let plan = FoldPlan::from_metadata(&survey.metadata);
    assert!((0..5).all(|feed| plan.source_is_known(feed)));
    eprintln!("D0: {} frames", survey.frames);
}

#[test]
fn d1_folds_heights_at_minus_3_db_and_reads_two_object_positions() {
    let Some(bytes) = corpus("HARLETTY_D1_CORPUS") else {
        eprintln!("skipping: HARLETTY_D1_CORPUS is not set");
        return;
    };
    let survey = survey(&bytes);
    assert_eq!(survey.presentation, XPresentation::ObjectsD1);
    assert_eq!(
        height_fold_columns(&survey.metadata, survey.presentation),
        vec![(1, Q55), (2, Q55), (4, Q55), (5, Q55)]
    );
    for feed in 0..2 {
        let SourceRole::Object { position, .. } = survey.metadata.source(feed).unwrap().role else {
            panic!("feed {feed} is an object");
        };
        assert_eq!(position.distance_64ths, 64);
        assert_eq!(
            position.azimuth_half_degrees.signum(),
            if feed == 0 { -1 } else { 1 },
            "objects are a left/right pair"
        );
    }
    eprintln!(
        "D1: {} frames, object folds {:?}",
        survey.frames,
        (0..2)
            .map(|feed| matches!(
                survey.metadata.source(feed).unwrap().fold,
                BedFold::Known(_)
            ))
            .collect::<Vec<_>>()
    );
}

#[test]
fn d3_folds_heights_at_minus_3_db_and_reads_four_object_folds() {
    let Some(bytes) = corpus("HARLETTY_D3_CORPUS") else {
        eprintln!("skipping: HARLETTY_D3_CORPUS is not set");
        return;
    };
    let survey = survey(&bytes);
    assert_eq!(survey.presentation, XPresentation::ObjectsD3);
    let folds = height_fold_columns(&survey.metadata, survey.presentation);
    assert_eq!(folds, vec![(1, Q55), (2, Q55), (4, Q55), (5, Q55)]);
    assert_eq!(
        survey.metadata.source(4).unwrap().role,
        SourceRole::Height(SpatialChannel::TopFrontLeft)
    );
    for feed in 0..4 {
        let source = survey.metadata.source(feed).unwrap();
        assert!(matches!(source.role, SourceRole::Object { .. }));
        assert!(
            matches!(source.fold, BedFold::Known(_)),
            "D3 objects carry rows"
        );
    }
    let plan = FoldPlan::from_metadata(&survey.metadata);
    assert!((0..8).all(|feed| plan.source_is_known(feed)));
    assert!(
        plan.touches(7) && plan.touches(8),
        "rear pair receives objects and heights"
    );
    eprintln!("D3: {} frames", survey.frames);
}
