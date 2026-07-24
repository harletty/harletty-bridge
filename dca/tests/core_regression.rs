// SPDX-License-Identifier: Apache-2.0
//
// Regression: decode a raw DTS core stream and compare to an ffmpeg f32le
// reference, gating per-channel RMSE. The corpus lives outside the repo (it is
// derived from copyrighted content), so the test SKIPS when the files are
// absent. Regenerate locally with:
//
//   SRC=<input.mkv>
//   ffmpeg -v error -y -i "$SRC" -map 0:4 -t 8 -c:a copy -f dts \
//       dumps/dts51_core.dts
//   ffmpeg -v error -y -i "$SRC" -map 0:4 -t 8 -f f32le -ac 6 \
//       dumps/dts51_ref.f32
//   export HARLETTY_DTS_CORE_CORPUS="$PWD/dumps/dts51_core.dts"
//   export HARLETTY_DTS_CORE_REFERENCE="$PWD/dumps/dts51_ref.f32"
//
// (select a plain DTS 5.1 track that ffmpeg decodes core-only,
// so it validates the core decoder without the XLL extension.)

use dca::{BedChannel, Extractor, PcmDecoder};

const CHANNELS: usize = 6;
const RMSE_GATE: f64 = 5.0e-3;

fn wav_index(b: BedChannel) -> usize {
    match b {
        BedChannel::FrontLeft => 0,
        BedChannel::FrontRight => 1,
        BedChannel::Center => 2,
        BedChannel::SurroundLeft | BedChannel::RearLeft | BedChannel::RearCenter => 4,
        BedChannel::SurroundRight | BedChannel::RearRight => 5,
        _ => 0,
    }
}

#[test]
fn dts_core_5_1_matches_ffmpeg() {
    let Ok(dts) = std::env::var("HARLETTY_DTS_CORE_CORPUS") else {
        eprintln!("skipping: HARLETTY_DTS_CORE_CORPUS is not set");
        return;
    };
    let Ok(reference) = std::env::var("HARLETTY_DTS_CORE_REFERENCE") else {
        eprintln!("skipping: HARLETTY_DTS_CORE_REFERENCE is not set");
        return;
    };
    if !std::path::Path::new(&dts).is_file() || !std::path::Path::new(&reference).is_file() {
        eprintln!("skipping: configured DTS core corpus is not readable");
        return;
    }

    let bytes = std::fs::read(dts).unwrap();
    let mut ex = Extractor::default();
    ex.push_bytes(&bytes);
    let mut dec = PcmDecoder::new();

    let mut out: Vec<f32> = Vec::new();
    while let Some(frame) = ex.next_frame().expect("extract") {
        let r = dec.push_access_unit(frame.as_bytes()).expect("decode");
        let pcm = &r.pcm;
        assert!(pcm.decoded);
        let n = pcm.samples_per_channel();
        let base = out.len();
        out.resize(base + n * CHANNELS, 0.0);
        for (ci, &bed) in pcm.fullband_channel_order.iter().enumerate() {
            let wi = wav_index(bed);
            let src = &pcm.fullband_channels[ci];
            for s in 0..n {
                out[base + s * CHANNELS + wi] = src[s];
            }
        }
        if let Some(lfe) = &pcm.lfe_channel {
            for s in 0..n {
                out[base + s * CHANNELS + 3] = lfe[s];
            }
        }
    }

    let rbytes = std::fs::read(reference).unwrap();
    let reference: Vec<f32> = rbytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();

    let nsamp = out.len().min(reference.len()) / CHANNELS;
    assert!(nsamp > 48_000, "decoded too little ({nsamp} samples)");

    for c in 0..CHANNELS {
        let mut sq = 0f64;
        for s in 0..nsamp {
            let d = (out[s * CHANNELS + c] - reference[s * CHANNELS + c]) as f64;
            sq += d * d;
        }
        let rmse = (sq / nsamp as f64).sqrt();
        assert!(rmse < RMSE_GATE, "channel {c} rmse {rmse:.3e} exceeds gate");
    }
}
