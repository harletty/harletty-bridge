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
                    trailing_bits.push(payload.len() * 8 - frame.x_bits_consumed);
                    for (channel, samples) in frame.x_samples.iter().enumerate() {
                        let energy = samples
                            .iter()
                            .map(|&sample| (sample as f64).powi(2))
                            .sum::<f64>();
                        channel_energy[channel] += energy;
                        channel_samples[channel] += samples.len();
                        frame_rms[channel].push((energy / samples.len().max(1) as f64).sqrt());
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
