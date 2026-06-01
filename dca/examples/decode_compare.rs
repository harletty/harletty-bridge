// SPDX-License-Identifier: Apache-2.0
//
// Decode a raw DTS core .dts file and compare to an ffmpeg f32le reference.
// Usage: cargo run -p dca --example decode_compare -- <in.dts> <ref.f32> [channels]

use std::io::Read;

use dca::{BedChannel, Extractor, PcmDecoder};

fn wav_index(b: BedChannel) -> usize {
    // 5.1 WAV order: FL FR FC LFE BL BR
    match b {
        BedChannel::FrontLeft => 0,
        BedChannel::FrontRight => 1,
        BedChannel::Center => 2,
        BedChannel::SurroundLeft | BedChannel::RearLeft => 4,
        BedChannel::SurroundRight | BedChannel::RearRight => 5,
        BedChannel::RearCenter => 4,
        _ => 0,
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let in_path = args.next().expect("usage: decode_compare <in.dts> <ref.f32> [channels]");
    let ref_path = args.next().expect("need reference .f32");
    let channels: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(6);

    let mut bytes = Vec::new();
    std::fs::File::open(&in_path).unwrap().read_to_end(&mut bytes).unwrap();

    let mut ex = Extractor::default();
    ex.push_bytes(&bytes);
    let mut dec = PcmDecoder::new();

    // Interleaved WAV-order output.
    let mut out: Vec<f32> = Vec::new();
    let mut frames = 0;
    while let Some(frame) = ex.next_frame().expect("extract") {
        let r = match dec.push_access_unit(frame.as_bytes()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("decode error at frame {frames}: {e}");
                break;
            }
        };
        let pcm = &r.pcm;
        let n = pcm.samples_per_channel();
        // Assemble one interleaved block of `channels` per sample in WAV order.
        let base = out.len();
        out.resize(base + n * channels, 0.0);
        for (ci, &bed) in pcm.fullband_channel_order.iter().enumerate() {
            let wi = wav_index(bed);
            let src = &pcm.fullband_channels[ci];
            for s in 0..n {
                out[base + s * channels + wi] = src[s];
            }
        }
        if let Some(lfe) = &pcm.lfe_channel {
            for s in 0..n {
                out[base + s * channels + 3] = lfe[s];
            }
        }
        frames += 1;
    }
    println!("decoded {frames} frames, {} samples/ch", out.len() / channels);

    // Load reference.
    let mut rbytes = Vec::new();
    std::fs::File::open(&ref_path).unwrap().read_to_end(&mut rbytes).unwrap();
    let reference: Vec<f32> = rbytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    println!("reference {} samples/ch", reference.len() / channels);

    let nframes_cmp = out.len().min(reference.len());
    let nsamp = nframes_cmp / channels;
    let mut sq = vec![0f64; channels];
    let mut maxd = vec![0f32; channels];
    let mut first_div: Option<usize> = None;
    for s in 0..nsamp {
        for c in 0..channels {
            let a = out[s * channels + c];
            let b = reference[s * channels + c];
            let d = (a - b).abs();
            sq[c] += (d as f64) * (d as f64);
            if d > maxd[c] {
                maxd[c] = d;
            }
            if first_div.is_none() && d > 1e-2 {
                first_div = Some(s);
            }
        }
    }
    println!("compared {nsamp} samples/ch");
    let names = ["FL", "FR", "FC", "LFE", "BL", "BR"];
    let mut worst = 0f64;
    for c in 0..channels {
        let rmse = (sq[c] / nsamp as f64).sqrt();
        worst = worst.max(rmse);
        println!(
            "  ch{c} {:>3}: rmse={:.6e} maxabs={:.6e}",
            names.get(c).unwrap_or(&"?"),
            rmse,
            maxd[c]
        );
    }
    println!("worst rmse = {worst:.6e}  (gate 5.0e-3)");
    if let Some(s) = first_div {
        println!("first divergence (>1e-2) at sample {s}");
    }
}
