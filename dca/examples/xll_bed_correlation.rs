// SPDX-License-Identifier: Apache-2.0
//
// Measure sample and frame-RMS correlations between decoded alternate-profile
// extension sources and the lossless bed. Research tool only; it is not used by
// the realtime decoder.
//
// Usage:
//   cargo run -p dca --release --example xll_bed_correlation -- <track.dts>

use std::io::Read;

use dca::{HdDecoder, HdError, exss_substream_size, parse_header};

const BED: [(usize, &str); 8] = [
    (0, "C"),
    (1, "L"),
    (2, "R"),
    (3, "Ls"),
    (4, "Rs"),
    (5, "LFE"),
    (7, "Lb"),
    (8, "Rb"),
];

#[derive(Default)]
struct Correlations {
    observations: u64,
    x_sum: Vec<f64>,
    x_square: Vec<f64>,
    bed_sum: [f64; BED.len()],
    bed_square: [f64; BED.len()],
    cross: Vec<[f64; BED.len()]>,
}

impl Correlations {
    fn with_sources(sources: usize) -> Self {
        Self {
            x_sum: vec![0.0; sources],
            x_square: vec![0.0; sources],
            cross: vec![[0.0; BED.len()]; sources],
            ..Self::default()
        }
    }

    fn add(&mut self, extension: &[Vec<f32>], bed: &[&[f32]]) {
        let samples = extension[0].len();
        self.observations += samples as u64;
        for (source, channel) in extension.iter().enumerate() {
            for &sample in channel {
                let sample = sample as f64;
                self.x_sum[source] += sample;
                self.x_square[source] += sample * sample;
            }
        }
        for (speaker, channel) in bed.iter().enumerate() {
            for &sample in *channel {
                let sample = sample as f64;
                self.bed_sum[speaker] += sample;
                self.bed_square[speaker] += sample * sample;
            }
        }
        for (source, extension_channel) in extension.iter().enumerate() {
            for (speaker, bed_channel) in bed.iter().enumerate() {
                self.cross[source][speaker] += extension_channel
                    .iter()
                    .zip(*bed_channel)
                    .map(|(&x, &y)| x as f64 * y as f64)
                    .sum::<f64>();
            }
        }
    }

    fn add_rms(&mut self, extension: &[Vec<f32>], bed: &[&[f32]]) {
        self.observations += 1;
        let x_rms = extension
            .iter()
            .map(|channel| {
                (channel
                    .iter()
                    .map(|&sample| (sample as f64).powi(2))
                    .sum::<f64>()
                    / channel.len() as f64)
                    .sqrt()
            })
            .collect::<Vec<_>>();
        let bed_rms = bed
            .iter()
            .map(|channel| {
                (channel
                    .iter()
                    .map(|&sample| (sample as f64).powi(2))
                    .sum::<f64>()
                    / channel.len() as f64)
                    .sqrt()
            })
            .collect::<Vec<_>>();
        for (source, &value) in x_rms.iter().enumerate() {
            self.x_sum[source] += value;
            self.x_square[source] += value * value;
        }
        for (speaker, &value) in bed_rms.iter().enumerate() {
            self.bed_sum[speaker] += value;
            self.bed_square[speaker] += value * value;
        }
        for (source, &x) in x_rms.iter().enumerate() {
            for (speaker, &y) in bed_rms.iter().enumerate() {
                self.cross[source][speaker] += x * y;
            }
        }
    }

    fn correlation(&self, source: usize, speaker: usize) -> f64 {
        let n = self.observations as f64;
        let covariance =
            self.cross[source][speaker] - self.x_sum[source] * self.bed_sum[speaker] / n;
        let x_variance = self.x_square[source] - self.x_sum[source].powi(2) / n;
        let bed_variance = self.bed_square[speaker] - self.bed_sum[speaker].powi(2) / n;
        if x_variance <= 0.0 || bed_variance <= 0.0 {
            f64::NAN
        } else {
            covariance / (x_variance * bed_variance).sqrt()
        }
    }

    fn print(&self, title: &str) {
        println!("{title} (n={}):", self.observations);
        print!("       ");
        for (_, name) in BED {
            print!(" {name:>7}");
        }
        println!();
        for source in 0..self.x_sum.len() {
            print!("  X{source:<2} ");
            for speaker in 0..BED.len() {
                print!(" {:>+7.3}", self.correlation(source, speaker));
            }
            println!();
        }
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: xll_bed_correlation <track.dts>");
    let mut input = Vec::new();
    std::fs::File::open(&path)
        .expect("open input")
        .read_to_end(&mut input)
        .expect("read input");

    let mut decoder = HdDecoder::new();
    let mut sample_correlations = None;
    let mut rms_correlations = None;
    let mut offset = 0usize;
    let mut frames = 0u64;
    let mut output_mask = None;
    while offset + 18 <= input.len() {
        let header = parse_header(&input[offset..]).expect("parse core");
        let exss_offset = offset.checked_add(header.frame_size).expect("core offset");
        let exss_size = exss_substream_size(&input[exss_offset..]).expect("parse EXSS");
        let frame_end = exss_offset.checked_add(exss_size).expect("frame end");
        let frame =
            match decoder.decode(&input[offset..exss_offset], &input[exss_offset..frame_end]) {
                Ok(frame) => frame,
                Err(HdError::Pending) => {
                    offset = frame_end;
                    continue;
                }
                Err(error) => panic!("decode frame {frames}: {error:?}"),
            };
        if frame.x_samples.is_empty() {
            offset = frame_end;
            continue;
        }
        output_mask.get_or_insert(frame.output_mask);
        let samples = frame.x_samples[0].len();
        if frame
            .x_samples
            .iter()
            .any(|channel| channel.len() != samples)
        {
            panic!("inconsistent extension channel lengths at frame {frames}");
        }
        let bed = BED.map(|(speaker, _)| {
            frame
                .samples
                .get(speaker)
                .and_then(Option::as_deref)
                .expect("missing bed speaker")
        });
        if bed.iter().any(|channel| channel.len() != samples) {
            panic!("inconsistent bed channel lengths at frame {frames}");
        }

        let sample_stats = sample_correlations
            .get_or_insert_with(|| Correlations::with_sources(frame.x_samples.len()));
        let rms_stats = rms_correlations
            .get_or_insert_with(|| Correlations::with_sources(frame.x_samples.len()));
        sample_stats.add(&frame.x_samples, &bed);
        rms_stats.add_rms(&frame.x_samples, &bed);
        frames += 1;
        offset = frame_end;
    }

    println!("file: {path}");
    println!("decoded frames: {frames}");
    println!("bed output mask: {:#010x}", output_mask.expect("no bed"));
    sample_correlations
        .expect("no extension sources")
        .print("sample correlation");
    rms_correlations
        .expect("no extension sources")
        .print("frame-RMS correlation");
}
