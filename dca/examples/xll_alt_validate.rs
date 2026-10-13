// SPDX-License-Identifier: Apache-2.0
//
// Stream an alternate extension profile through the high-level decoder and
// report bounded-memory PCM integrity statistics. This diagnostic deliberately
// does not infer speaker or object semantics.
//
// Usage:
//   cargo run -p dca --release --example xll_alt_validate -- <in.dts>

use std::collections::BTreeMap;
use std::io::{BufReader, ErrorKind, Read};

use dca::{HdDecoder, HdError, SYNCWORD_SUBSTREAM, parse_header};

const HEADER_BYTES: usize = 18;
const EXSS_PREFIX_BYTES: usize = 16;
const READER_CAPACITY: usize = 1024 * 1024;

fn read_bits(data: &[u8], position: &mut usize, width: usize) -> Option<u32> {
    if width > 32 || position.checked_add(width)? > data.len() * 8 {
        return None;
    }
    let mut value = 0u32;
    for _ in 0..width {
        value = (value << 1) | ((data[*position / 8] >> (7 - *position % 8)) & 1) as u32;
        *position += 1;
    }
    Some(value)
}

fn exss_size_from_prefix(data: &[u8]) -> Option<usize> {
    let mut position = 0usize;
    if read_bits(data, &mut position, 32)? != SYNCWORD_SUBSTREAM {
        return None;
    }
    read_bits(data, &mut position, 8)?;
    read_bits(data, &mut position, 2)?;
    let wide_header = read_bits(data, &mut position, 1)? as usize;
    read_bits(data, &mut position, 8 + 4 * wide_header)?;
    Some(read_bits(data, &mut position, 16 + 4 * wide_header)? as usize + 1)
}

#[derive(Clone, Copy, Default)]
struct ChannelStats {
    samples: u64,
    frames: u64,
    silent_frames: u64,
    clipped_samples: u64,
    non_finite_samples: u64,
    sum: f64,
    square_sum: f64,
    diff_square_sum: f64,
    differences: u64,
    boundary_square_sum: f64,
    boundaries: u64,
    peak: f32,
    max_difference: f32,
    max_boundary: f32,
    previous: Option<f32>,
}

struct ErrorProfile {
    frames: u64,
    min_payload: usize,
    max_payload: usize,
    first_frame: u64,
    first_prefix: Vec<u8>,
    layout_probe: String,
}

impl ErrorProfile {
    fn new(frame: u64, payload: &[u8]) -> Self {
        Self {
            frames: 0,
            min_payload: usize::MAX,
            max_payload: 0,
            first_frame: frame,
            first_prefix: payload.iter().take(80).copied().collect(),
            layout_probe: layout_probe(payload),
        }
    }

    fn add(&mut self, payload_size: usize) {
        self.frames += 1;
        self.min_payload = self.min_payload.min(payload_size);
        self.max_payload = self.max_payload.max(payload_size);
    }
}

fn geometry_candidates(control: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut candidates = Vec::new();
    for offset in 0..=control.len().saturating_mul(8).saturating_sub(13) {
        let mut position = offset;
        let Some(segment_log2) = read_bits(control, &mut position, 4) else {
            continue;
        };
        let Some(segment_samples_log2) = read_bits(control, &mut position, 4) else {
            continue;
        };
        let Some(size_bits) = read_bits(control, &mut position, 5) else {
            continue;
        };
        let Some(segments) = 1usize.checked_shl(segment_log2) else {
            continue;
        };
        let Some(segment_samples) = 1usize.checked_shl(segment_samples_log2) else {
            continue;
        };
        let size_bits = size_bits as usize + 1;
        if segments <= 8
            && segments.checked_mul(segment_samples) == Some(512)
            && (4..=20).contains(&size_bits)
        {
            candidates.push((offset, segments, size_bits));
        }
    }
    candidates
}

fn layout_probe(payload: &[u8]) -> String {
    const INNER_SUFFIX: [u8; 6] = [0x02, 0x34, 0x38, 0x8c, 0x4f, 0x00];
    let (control_start, offset_bias) = match payload.get(..4) {
        Some([0xf1, 0x40, 0x00, 0xd0]) => (54, 66),
        Some([0xf1, 0x40, 0x00, 0xd1]) => (55, 67),
        _ => return "unknown profile".to_owned(),
    };
    let Some(&tag) = payload.get(control_start) else {
        return "short payload".to_owned();
    };
    let control_size = if tag == 0xb2 { 7 } else { 8 };
    let Some(outer) = payload.get(control_start..control_start + control_size) else {
        return "short outer control".to_owned();
    };
    let outer_candidates = geometry_candidates(outer);
    let mut inner = Vec::new();
    for &(common_bit, _, _) in &outer_candidates {
        let Some(width) = common_bit.checked_sub(14) else {
            continue;
        };
        let mut position = 9;
        let Some(span) = read_bits(outer, &mut position, width) else {
            continue;
        };
        let Some(nominal) = (span as usize)
            .checked_mul(2)
            .and_then(|span| span.checked_add(offset_bias))
        else {
            continue;
        };
        for second_offset in [Some(nominal), nominal.checked_sub(1)]
            .into_iter()
            .flatten()
        {
            let start = second_offset.saturating_sub(24);
            let Some(window) = payload.get(start..second_offset) else {
                continue;
            };
            let Some(suffix) = window
                .windows(INNER_SUFFIX.len())
                .rposition(|bytes| bytes == INNER_SUFFIX)
            else {
                continue;
            };
            let control = &window[suffix + INNER_SUFFIX.len()..];
            inner.push((
                second_offset,
                control.to_vec(),
                geometry_candidates(control),
            ));
        }
    }
    format!("outer={outer:02x?} candidates={outer_candidates:?} inner={inner:02x?}")
}

impl ChannelStats {
    fn add_frame(&mut self, samples: &[f32]) {
        self.frames += 1;
        let mut frame_peak = 0.0f32;
        for &sample in samples {
            if !sample.is_finite() {
                self.non_finite_samples += 1;
                continue;
            }
            let magnitude = sample.abs();
            frame_peak = frame_peak.max(magnitude);
            self.peak = self.peak.max(magnitude);
            self.clipped_samples += u64::from(magnitude >= 1.0);
            self.sum += sample as f64;
            self.square_sum += (sample as f64).powi(2);
            if let Some(previous) = self.previous {
                let difference = (sample - previous).abs();
                self.max_difference = self.max_difference.max(difference);
                self.diff_square_sum += (difference as f64).powi(2);
                self.differences += 1;
            }
            self.previous = Some(sample);
            self.samples += 1;
        }
        self.silent_frames += u64::from(frame_peak == 0.0);
    }

    fn add_boundary(&mut self, first: f32) {
        if let Some(previous) = self.previous {
            let difference = (first - previous).abs();
            self.max_boundary = self.max_boundary.max(difference);
            self.boundary_square_sum += (difference as f64).powi(2);
            self.boundaries += 1;
        }
    }

    fn print(&self, channel: usize) {
        let samples = self.samples.max(1) as f64;
        let mean = self.sum / samples;
        let rms = (self.square_sum / samples).sqrt();
        let diff_rms = (self.diff_square_sum / self.differences.max(1) as f64).sqrt();
        let boundary_rms = (self.boundary_square_sum / self.boundaries.max(1) as f64).sqrt();
        println!(
            "channel {channel}: frames={} samples={} silent_frames={} mean={mean:+.9e} rms={rms:.9e} peak={:.9e} clipped={} non_finite={} diff_rms={diff_rms:.9e} max_diff={:.9e} boundary_rms={boundary_rms:.9e} max_boundary={:.9e}",
            self.frames,
            self.samples,
            self.silent_frames,
            self.peak,
            self.clipped_samples,
            self.non_finite_samples,
            self.max_difference,
            self.max_boundary,
        );
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: xll_alt_validate <in.dts>");
    let input = std::fs::File::open(&path).expect("open input");
    let file_bytes = input.metadata().expect("input metadata").len();
    let mut reader = BufReader::with_capacity(READER_CAPACITY, input);
    let mut decoder = HdDecoder::new();
    let mut core = Vec::new();
    let mut exss = Vec::new();
    let mut header = [0u8; HEADER_BYTES];
    let mut frames = 0u64;
    let mut decoded_frames = 0u64;
    let mut pending_frames = 0u64;
    let mut input_bytes = 0u64;
    let mut channel_counts = BTreeMap::<usize, u64>::new();
    let mut pcm_resolutions = BTreeMap::<usize, u64>::new();
    let mut extension_errors = BTreeMap::<&'static str, u64>::new();
    let mut error_profiles = BTreeMap::<(&'static str, u8), ErrorProfile>::new();
    let mut channel_stats = Vec::<ChannelStats>::new();

    loop {
        match reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => panic!("read core header at frame {frames}: {error}"),
        }
        let info = parse_header(&header).expect("parse core header");
        core.resize(info.frame_size, 0);
        core[..HEADER_BYTES].copy_from_slice(&header);
        reader
            .read_exact(&mut core[HEADER_BYTES..])
            .expect("read core frame");

        exss.resize(EXSS_PREFIX_BYTES, 0);
        reader.read_exact(&mut exss).expect("read EXSS size prefix");
        let exss_size = exss_size_from_prefix(&exss).unwrap_or_else(|| {
            panic!(
                "parse EXSS size at frame {frames}: head={:02x?}",
                &exss[..EXSS_PREFIX_BYTES]
            )
        });
        exss.resize(exss_size, 0);
        reader
            .read_exact(&mut exss[EXSS_PREFIX_BYTES..])
            .expect("read EXSS frame");
        input_bytes += (core.len() + exss.len()) as u64;

        match decoder.decode(&core, &exss) {
            Ok(frame) => {
                if frame.x_imax {
                    *channel_counts.entry(frame.x_samples.len()).or_default() += 1;
                    *pcm_resolutions.entry(frame.x_pcm_bit_res).or_default() += 1;
                    if let Some(error) = frame.x_decode_error {
                        *extension_errors.entry(error).or_default() += 1;
                        let tag_offset = match frame.x_payload.get(..4) {
                            Some([0xf1, 0x40, 0x00, 0xd0]) => 54,
                            Some([0xf1, 0x40, 0x00, 0xd1]) => 55,
                            _ => usize::MAX,
                        };
                        let tag = frame.x_payload.get(tag_offset).copied().unwrap_or_default();
                        error_profiles
                            .entry((error, tag))
                            .or_insert_with(|| ErrorProfile::new(frames, &frame.x_payload))
                            .add(frame.x_payload.len());
                    }
                    if !frame.x_samples.is_empty() {
                        decoded_frames += 1;
                        if channel_stats.len() < frame.x_samples.len() {
                            channel_stats.resize(frame.x_samples.len(), ChannelStats::default());
                        }
                        for (stats, samples) in channel_stats.iter_mut().zip(&frame.x_samples) {
                            if let Some(&first) = samples.first() {
                                stats.add_boundary(first);
                            }
                            stats.add_frame(samples);
                        }
                    }
                }
            }
            Err(HdError::Pending) => pending_frames += 1,
            Err(error) => panic!("decode frame {frames}: {error:?}"),
        }
        frames += 1;
        if frames.is_multiple_of(100_000) {
            eprintln!(
                "{frames} frames, {:.1}% of input",
                input_bytes as f64 * 100.0 / file_bytes.max(1) as f64
            );
        }
    }

    println!("file: {path}");
    println!("bytes: {input_bytes}/{file_bytes}");
    println!("frames: {frames}; pending: {pending_frames}");
    println!("decoded alternate frames: {decoded_frames}");
    println!("source counts: {channel_counts:?}");
    println!("PCM resolutions: {pcm_resolutions:?}");
    println!("extension errors: {extension_errors:?}");
    println!("error profiles (kind, control tag):");
    for ((error, tag), profile) in error_profiles {
        println!(
            "  {error}, {tag:#04x}: frames={} payload={}..={} first_frame={} prefix={:02x?} probe={}",
            profile.frames,
            profile.min_payload,
            profile.max_payload,
            profile.first_frame,
            profile.first_prefix,
            profile.layout_probe,
        );
    }
    for (channel, stats) in channel_stats.iter().enumerate() {
        stats.print(channel);
    }
}
