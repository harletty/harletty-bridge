// SPDX-License-Identifier: Apache-2.0
//
// Search the variable part of an XLL-X payload for a plausible bare XLL
// channel-set header.  DTS-UHD (ETSI TS 103 491) explicitly permits XLL audio
// chunks containing channel-set header/data without a nested XLL frame sync,
// so absence of 0x41a29547 alone does not falsify reuse of this grammar.
//
// Usage:
//   cargo run -p dca --release --example xll_x_chs -- <in.dts> [max_mb]

use std::collections::HashMap;
use std::io::Read;

use dca::parser::parse_header;
use dca::{exss_substream_size, HdDecoder};

const SEARCH_START: usize = 22 * 8;
const SEARCH_END_BYTE: usize = 160;
const CHANNEL_PAIRS: [(usize, usize); 6] = [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)];

fn read_bits(data: &[u8], position: &mut usize, width: usize) -> Option<u32> {
    if width > 32 || *position + width > data.len() * 8 {
        return None;
    }
    let mut value = 0u32;
    for _ in 0..width {
        value = (value << 1) | ((data[*position / 8] >> (7 - *position % 8)) & 1) as u32;
        *position += 1;
    }
    Some(value)
}

#[derive(Debug)]
struct Candidate {
    bit_offset: usize,
    header_size: usize,
    channels: usize,
    pcm_resolution: usize,
    storage_resolution: usize,
    residual_mask: u32,
}

fn candidate_at(data: &[u8], bit_offset: usize) -> Option<Candidate> {
    let mut position = bit_offset;
    let header_size = read_bits(data, &mut position, 10)? as usize + 1;
    let channels = read_bits(data, &mut position, 4)? as usize + 1;
    let residual_mask = read_bits(data, &mut position, channels)?;
    let pcm_resolution = read_bits(data, &mut position, 5)? as usize + 1;
    let storage_resolution = read_bits(data, &mut position, 5)? as usize + 1;
    let frequency_index = read_bits(data, &mut position, 4)?;
    let frequency_modifier = read_bits(data, &mut position, 2)?;
    let replacement_set = read_bits(data, &mut position, 2)?;

    let minimum_header_bits = position - bit_offset;
    let header_fits =
        header_size * 8 >= minimum_header_bits && bit_offset + header_size * 8 <= data.len() * 8;
    if !header_fits
        || !matches!(storage_resolution, 16 | 20 | 24)
        || pcm_resolution > storage_resolution
        || frequency_index != 12 // 48 kHz in the DCA EXSS table
        || frequency_modifier != 0
        || replacement_set != 0
    {
        return None;
    }

    Some(Candidate {
        bit_offset,
        header_size,
        channels,
        pcm_resolution,
        storage_resolution,
        residual_mask,
    })
}

fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc = 0xffffu16;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: xll_x_chs <in.dts> [max_mb]");
    let max_mb: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(64);

    let mut input = std::fs::File::open(&path).expect("open input");
    let mut bytes = vec![0u8; max_mb * 1024 * 1024];
    let read = input.read(&mut bytes).expect("read input");
    bytes.truncate(read);

    let mut decoder = HdDecoder::new();
    let mut offset = 0usize;
    let mut frames = 0usize;
    let mut payloads = 0usize;
    let mut candidate_frames = 0usize;
    let mut count_matching_frames = 0usize;
    let mut all_offsets: HashMap<usize, usize> = HashMap::new();
    let mut count_matching_offsets: HashMap<usize, usize> = HashMap::new();
    let mut header_sizes: HashMap<usize, usize> = HashMap::new();
    let mut exact_offset_frames = 0usize;
    let mut exact_offset_crc_valid = 0usize;
    let mut exact_basic_fields: HashMap<(usize, usize, usize, u32), usize> = HashMap::new();
    let mut first_candidates = Vec::new();
    let mut decoded_audio_frames = 0usize;
    let mut descriptor_navigation_frames = 0usize;
    let mut first_descriptor_fallbacks = Vec::new();
    let mut decode_errors: HashMap<String, usize> = HashMap::new();
    let mut navigation_deltas: HashMap<isize, usize> = HashMap::new();
    let mut trailer_crc_valid = [0usize; 3];
    let mut trailer_words: HashMap<u16, usize> = HashMap::new();
    let mut descriptor_tails: HashMap<(usize, Vec<u8>), usize> = HashMap::new();
    let mut channel_header_tails: HashMap<usize, usize> = HashMap::new();
    let mut trailing_bits = Vec::new();
    let mut channel_energy = [0f64; 4];
    let mut channel_samples = [0usize; 4];
    let mut frame_rms: [Vec<f64>; 4] = std::array::from_fn(|_| Vec::new());
    let mut sample_sum = [0f64; 4];
    let mut sample_cross = [[0f64; 4]; 4];
    let mut joint_samples = 0usize;
    let mut frame_pair_correlation: [Vec<f64>; 6] = std::array::from_fn(|_| Vec::new());
    let mut frame_pair_balance: [Vec<f64>; 6] = std::array::from_fn(|_| Vec::new());
    let mut constant_frame_values: [Vec<Option<f32>>; 4] = std::array::from_fn(|_| Vec::new());
    let mut frame_geometry = None;
    let mut first_layout_bytes = None;

    while offset + 18 < bytes.len() {
        let core = match parse_header(&bytes[offset..]) {
            Ok(header) => header,
            Err(_) => break,
        };
        let exss_offset = offset + core.frame_size;
        if exss_offset + 4 > bytes.len() {
            break;
        }
        let exss_len = match exss_substream_size(&bytes[exss_offset..]) {
            Some(len) if exss_offset + len <= bytes.len() => len,
            _ => break,
        };

        if let Ok(frame) = decoder.decode(
            &bytes[offset..exss_offset],
            &bytes[exss_offset..exss_offset + exss_len],
        ) {
            *descriptor_tails
                .entry((
                    frame.exss_descriptor_tail_bits,
                    frame.exss_descriptor_tail.clone(),
                ))
                .or_default() += 1;
            if frame.x_present || frame.x_imax {
                payloads += 1;
                frame_geometry.get_or_insert((
                    frame.xll_frame_segments,
                    frame.xll_segment_samples,
                    frame.xll_segment_size_bits,
                    frame.xll_band_crc_present,
                    frame.xll_scalable_lsbs,
                ));
                let payload = &frame.x_payload;
                if payload.len() > 24 {
                    let header_size =
                        (((payload[22] as usize) << 2) | ((payload[23] as usize) >> 6)) + 1;
                    let navi_start = 22 + header_size;
                    let navi_bits = frame.xll_frame_segments * frame.xll_segment_size_bits;
                    let navi_size = navi_bits.div_ceil(8) + 2;
                    if payload.len() >= navi_start + navi_size {
                        let mut bit = navi_start * 8;
                        let mut audio_bytes = 0usize;
                        for _ in 0..frame.xll_frame_segments {
                            if let Some(size) =
                                read_bits(payload, &mut bit, frame.xll_segment_size_bits)
                            {
                                audio_bytes += size as usize + 1;
                            }
                        }
                        let predicted_end = navi_start + navi_size + audio_bytes;
                        let delta = payload.len() as isize - predicted_end as isize;
                        *navigation_deltas.entry(delta).or_default() += 1;
                        if delta >= 2 {
                            let crc_end = predicted_end + 2;
                            let trailer = u16::from_be_bytes([
                                payload[predicted_end],
                                payload[predicted_end + 1],
                            ]);
                            *trailer_words.entry(trailer).or_default() += 1;
                            for (slot, start) in trailer_crc_valid.iter_mut().zip([0usize, 4, 22]) {
                                if crc16_ccitt(&payload[start..crc_end]) == 0 {
                                    *slot += 1;
                                }
                            }
                        }
                    }
                }
                if first_layout_bytes.is_none() && payload.len() > 24 {
                    let header_size =
                        (((payload[22] as usize) << 2) | ((payload[23] as usize) >> 6)) + 1;
                    let data_start = 22 + header_size;
                    first_layout_bytes = Some((
                        payload.len(),
                        header_size,
                        data_start,
                        payload[data_start..payload.len().min(data_start + 24)].to_vec(),
                        payload[payload.len().saturating_sub(24)..].to_vec(),
                    ));
                }
                if frame.x_samples.len() == 4 {
                    *channel_header_tails
                        .entry(frame.x_header_tail_bits)
                        .or_default() += 1;
                    decoded_audio_frames += 1;
                    descriptor_navigation_frames += usize::from(frame.x_descriptor_navigation_used);
                    if !frame.x_descriptor_navigation_used && first_descriptor_fallbacks.len() < 8 {
                        first_descriptor_fallbacks.push((
                            frames,
                            frame.x_descriptor_offset,
                            frame.x_payload_offset,
                            frame.x_descriptor_size,
                            frame.x_payload.len(),
                        ));
                    }
                    trailing_bits.push(payload.len() * 8 - frame.x_bits_consumed);
                    for (channel, samples) in frame.x_samples.iter().enumerate() {
                        let energy = samples
                            .iter()
                            .map(|&sample| (sample as f64).powi(2))
                            .sum::<f64>();
                        channel_energy[channel] += energy;
                        channel_samples[channel] += samples.len();
                        frame_rms[channel].push((energy / samples.len().max(1) as f64).sqrt());
                        constant_frame_values[channel].push(samples.first().copied().filter(
                            |first| {
                                samples
                                    .iter()
                                    .all(|sample| sample.to_bits() == first.to_bits())
                            },
                        ));
                    }
                    let samples = frame.x_samples[0].len();
                    if frame
                        .x_samples
                        .iter()
                        .all(|channel| channel.len() == samples)
                    {
                        for sample in 0..samples {
                            let values: [f64; 4] = std::array::from_fn(|channel| {
                                frame.x_samples[channel][sample] as f64
                            });
                            for a in 0..4 {
                                sample_sum[a] += values[a];
                                for b in 0..4 {
                                    sample_cross[a][b] += values[a] * values[b];
                                }
                            }
                        }
                        joint_samples += samples;
                        for (pair, &(a, b)) in CHANNEL_PAIRS.iter().enumerate() {
                            let correlation =
                                pearson_samples(&frame.x_samples[a], &frame.x_samples[b]);
                            let rms_a = (frame.x_samples[a]
                                .iter()
                                .map(|&value| (value as f64).powi(2))
                                .sum::<f64>()
                                / samples.max(1) as f64)
                                .sqrt();
                            let rms_b = (frame.x_samples[b]
                                .iter()
                                .map(|&value| (value as f64).powi(2))
                                .sum::<f64>()
                                / samples.max(1) as f64)
                                .sqrt();
                            frame_pair_correlation[pair].push(correlation);
                            frame_pair_balance[pair].push(rms_b / (rms_a + rms_b));
                        }
                    }
                } else if let Some(error) = &frame.x_decode_error {
                    *decode_errors.entry(error.clone()).or_default() += 1;
                }
                let expected_channels =
                    payload.get(22).copied().unwrap_or(3).saturating_sub(3) as usize;
                let end = (payload.len().min(SEARCH_END_BYTE) * 8).saturating_sub(1);
                let mut candidates = Vec::new();
                for bit_offset in SEARCH_START..=end {
                    if let Some(candidate) = candidate_at(payload, bit_offset) {
                        candidates.push(candidate);
                    }
                }
                if !candidates.is_empty() {
                    candidate_frames += 1;
                }
                let mut count_match = false;
                for candidate in candidates {
                    *all_offsets.entry(candidate.bit_offset).or_default() += 1;
                    if candidate.bit_offset == SEARCH_START {
                        exact_offset_frames += 1;
                        *exact_basic_fields
                            .entry((
                                candidate.header_size,
                                candidate.channels,
                                candidate.pcm_resolution,
                                candidate.residual_mask,
                            ))
                            .or_default() += 1;
                        let start = candidate.bit_offset / 8;
                        let end = start + candidate.header_size;
                        if candidate.storage_resolution == 24
                            && end <= payload.len()
                            && crc16_ccitt(&payload[start..end]) == 0
                        {
                            exact_offset_crc_valid += 1;
                        }
                    }
                    if candidate.channels == expected_channels {
                        count_match = true;
                        *count_matching_offsets
                            .entry(candidate.bit_offset)
                            .or_default() += 1;
                        *header_sizes.entry(candidate.header_size).or_default() += 1;
                        if first_candidates.len() < 16 {
                            first_candidates.push((
                                frames,
                                candidate.bit_offset,
                                candidate.header_size,
                                candidate.channels,
                            ));
                        }
                    }
                }
                if count_match {
                    count_matching_frames += 1;
                }
            }
            frames += 1;
        }
        offset += core.frame_size + exss_len;
    }

    println!("read {read} bytes from {path}");
    println!("decoded frames: {frames}; XLL-X payloads: {payloads}");
    println!(
        "inherited XLL geometry (segments, samples/segment, size bits, band CRC, scalable LSB): \
         {frame_geometry:?}"
    );
    println!(
        "first payload (bytes, header bytes, data start, data head, payload tail): \
         {first_layout_bytes:02x?}"
    );
    println!("frames with any basic XLL chset candidate: {candidate_frames}");
    println!(
        "candidate at byte 22: {exact_offset_frames}; valid CRC16-CCITT: \
         {exact_offset_crc_valid}/{exact_offset_frames}"
    );
    println!("decoded four-channel audio frames: {decoded_audio_frames}/{payloads}");
    println!(
        "frames located through EXSS descriptor navigation: {descriptor_navigation_frames}/{payloads}"
    );
    println!(
        "first descriptor fallbacks (frame, hinted/actual offset, hinted/actual size): \
         {first_descriptor_fallbacks:?}"
    );
    println!("extension decode errors: {decode_errors:?}");
    let mut navigation_deltas = navigation_deltas.into_iter().collect::<Vec<_>>();
    navigation_deltas.sort_unstable_by_key(|&(delta, _)| delta);
    println!("payload bytes after predicted audio end: {navigation_deltas:?}");
    println!("two-byte trailer CRC valid from offsets 0/4/22: {trailer_crc_valid:?}");
    let mut trailer_words = trailer_words.into_iter().collect::<Vec<_>>();
    trailer_words.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    println!(
        "most common two-byte trailers: {:?}",
        trailer_words.into_iter().take(12).collect::<Vec<_>>()
    );
    let mut descriptor_tails = descriptor_tails.into_iter().collect::<Vec<_>>();
    descriptor_tails.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    println!(
        "EXSS descriptor tails (bits, bytes, frames), {} distinct: {:?}",
        descriptor_tails.len(),
        descriptor_tails
            .into_iter()
            .take(8)
            .map(|((bits, bytes), frames)| (bits, bytes, frames))
            .collect::<Vec<_>>()
    );
    println!("unparsed channel-header tail bits (includes CRC16): {channel_header_tails:?}");
    if !trailing_bits.is_empty() {
        println!(
            "trailing bits after sample decode: min={} max={}",
            trailing_bits.iter().min().unwrap(),
            trailing_bits.iter().max().unwrap()
        );
        let rms: Vec<_> = channel_energy
            .iter()
            .zip(channel_samples)
            .map(|(&energy, samples)| (energy / samples.max(1) as f64).sqrt())
            .collect();
        println!("decoded channel RMS: {rms:?}");
        println!("frame-RMS correlation matrix:");
        for a in 0..4 {
            println!(
                "  {}: {:?}",
                a,
                (0..4)
                    .map(|b| pearson(&frame_rms[a], &frame_rms[b]))
                    .collect::<Vec<_>>()
            );
        }
        println!("sample correlation matrix:");
        for a in 0..4 {
            println!(
                "  {}: {:?}",
                a,
                (0..4)
                    .map(|b| aggregate_pearson(
                        sample_sum[a],
                        sample_sum[b],
                        sample_cross[a][a],
                        sample_cross[b][b],
                        sample_cross[a][b],
                        joint_samples,
                    ))
                    .collect::<Vec<_>>()
            );
        }
        println!("least-squares pair fits (A->B: gain, residual/B RMS):");
        for a in 0..4 {
            for b in a + 1..4 {
                let gain = sample_cross[a][b] / sample_cross[a][a];
                let residual_energy = (sample_cross[b][b] - gain * sample_cross[a][b]).max(0.0);
                let residual_ratio = (residual_energy / sample_cross[b][b]).sqrt();
                println!("  {a}->{b}: {gain:.9}, {residual_ratio:.9}");
            }
        }
        println!("per-frame coherent pairs (|correlation| >= 0.98):");
        for (pair, &(a, b)) in CHANNEL_PAIRS.iter().enumerate() {
            let coherent = frame_pair_correlation[pair]
                .iter()
                .zip(&frame_pair_balance[pair])
                .filter_map(|(&correlation, &balance)| {
                    (correlation.abs() >= 0.98 && balance.is_finite()).then_some(balance)
                })
                .collect::<Vec<_>>();
            let steps = frame_pair_correlation[pair]
                .windows(2)
                .zip(frame_pair_balance[pair].windows(2))
                .filter_map(|(correlations, balances)| {
                    (correlations[0].abs() >= 0.98
                        && correlations[1].abs() >= 0.98
                        && balances[0].is_finite()
                        && balances[1].is_finite())
                    .then_some((balances[1] - balances[0]).abs())
                })
                .collect::<Vec<_>>();
            if !coherent.is_empty() {
                println!(
                    "  {a}/{b}: {}/{} frames, balance B p10/p50/p90={:.4}/{:.4}/{:.4}, \
                     step p50/p90={:.5}/{:.5}",
                    coherent.len(),
                    frame_pair_correlation[pair].len(),
                    percentile(&coherent, 0.1),
                    percentile(&coherent, 0.5),
                    percentile(&coherent, 0.9),
                    percentile(&steps, 0.5),
                    percentile(&steps, 0.9),
                );
            }
        }
        println!("constant-PCM frames by channel:");
        for (channel, values) in constant_frame_values.iter().enumerate() {
            let constant = values.iter().flatten().copied().collect::<Vec<_>>();
            let distinct = constant
                .iter()
                .map(|value| value.to_bits())
                .collect::<std::collections::HashSet<_>>();
            let first_transitions = values
                .iter()
                .enumerate()
                .filter_map(|(frame, value)| value.map(|value| (frame, value)))
                .scan(None, |previous: &mut Option<u32>, (frame, value)| {
                    let bits = value.to_bits();
                    let changed = previous.is_none_or(|previous| previous != bits);
                    *previous = Some(bits);
                    Some(changed.then_some((frame, value * 8_388_608.0)))
                })
                .flatten()
                .take(16)
                .collect::<Vec<_>>();
            println!(
                "  {channel}: {}/{} frames, {} distinct; first transitions as 24-bit PCM: {:?}",
                constant.len(),
                values.len(),
                distinct.len(),
                first_transitions,
            );
        }
    }
    println!(
        "frames with candidate channels == byte22-3: {count_matching_frames} ({:.3}%)",
        100.0 * count_matching_frames as f64 / payloads.max(1) as f64
    );
    let mut offsets: Vec<_> = count_matching_offsets.into_iter().collect();
    offsets.sort_by(|a, b| b.1.cmp(&a.1));
    println!("top matching bit offsets as offset:frames:");
    for (bit_offset, occurrences) in offsets.into_iter().take(16) {
        println!(
            "  {bit_offset:>4} (byte {:>3} + bit {}): {occurrences}",
            bit_offset / 8,
            bit_offset % 8
        );
    }
    let mut sizes: Vec<_> = header_sizes.into_iter().collect();
    sizes.sort_by(|a, b| b.1.cmp(&a.1));
    println!(
        "top matching header sizes: {:?}",
        &sizes[..sizes.len().min(12)]
    );
    let mut basic_fields: Vec<_> = exact_basic_fields.into_iter().collect();
    basic_fields.sort_by(|a, b| b.1.cmp(&a.1));
    println!(
        "byte-22 fields as (header bytes, channels, PCM bits, residual mask): {:?}",
        &basic_fields[..basic_fields.len().min(20)]
    );
    println!("first matching candidates (frame, bit, header bytes, channels):");
    for candidate in first_candidates {
        println!("  {candidate:?}");
    }

    let mut any_offsets: Vec<_> = all_offsets.into_iter().collect();
    any_offsets.sort_by(|a, b| b.1.cmp(&a.1));
    println!(
        "top offsets regardless of channel count: {:?}",
        &any_offsets[..any_offsets.len().min(12)]
    );
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    let am = a[..n].iter().sum::<f64>() / n.max(1) as f64;
    let bm = b[..n].iter().sum::<f64>() / n.max(1) as f64;
    let mut covariance = 0.0;
    let mut av = 0.0;
    let mut bv = 0.0;
    for (&x, &y) in a[..n].iter().zip(&b[..n]) {
        covariance += (x - am) * (y - bm);
        av += (x - am).powi(2);
        bv += (y - bm).powi(2);
    }
    covariance / (av * bv).sqrt()
}

fn aggregate_pearson(
    sum_a: f64,
    sum_b: f64,
    square_a: f64,
    square_b: f64,
    cross: f64,
    samples: usize,
) -> f64 {
    let n = samples.max(1) as f64;
    let covariance = cross - sum_a * sum_b / n;
    let variance_a = square_a - sum_a * sum_a / n;
    let variance_b = square_b - sum_b * sum_b / n;
    covariance / (variance_a * variance_b).sqrt()
}

fn pearson_samples(a: &[f32], b: &[f32]) -> f64 {
    let samples = a.len().min(b.len());
    let mut sum_a = 0.0;
    let mut sum_b = 0.0;
    let mut square_a = 0.0;
    let mut square_b = 0.0;
    let mut cross = 0.0;
    for (&a, &b) in a[..samples].iter().zip(&b[..samples]) {
        let a = a as f64;
        let b = b as f64;
        sum_a += a;
        sum_b += b;
        square_a += a * a;
        square_b += b * b;
        cross += a * b;
    }
    aggregate_pearson(sum_a, sum_b, square_a, square_b, cross, samples)
}

fn percentile(values: &[f64], quantile: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable_by(f64::total_cmp);
    sorted[((sorted.len() - 1) as f64 * quantile).round() as usize]
}
