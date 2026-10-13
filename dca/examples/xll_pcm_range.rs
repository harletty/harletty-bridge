// SPDX-License-Identifier: Apache-2.0
//
// Extract a time range as interleaved f32le: active bed speakers in ascending
// DCA index order, followed by the decoded XLL-X extension sources. The tool
// prints the exact channel order needed by the Python system-identification
// scripts. Research tool only; it is not used by the realtime decoder.
//
// Usage:
//   cargo run -p dca --release --example xll_pcm_range -- \
//     <input.dts> <output.f32le> <start-seconds> <end-seconds>

use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};

use dca::{HdDecoder, HdError, SYNCWORD_SUBSTREAM, parse_header};

const HEADER_BYTES: usize = 18;
const EXSS_PREFIX_BYTES: usize = 16;

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

fn exss_size(data: &[u8]) -> Option<usize> {
    let mut bit = 0usize;
    if read_bits(data, &mut bit, 32)? != SYNCWORD_SUBSTREAM {
        return None;
    }
    read_bits(data, &mut bit, 8)?;
    read_bits(data, &mut bit, 2)?;
    let wide = read_bits(data, &mut bit, 1)? as usize;
    read_bits(data, &mut bit, 8 + 4 * wide)?;
    Some(read_bits(data, &mut bit, 16 + 4 * wide)? as usize + 1)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let input_path = args.next().expect("input.dts");
    let output_path = args.next().expect("output.f32le");
    let start_seconds: f64 = args
        .next()
        .expect("start seconds")
        .parse()
        .expect("start number");
    let end_seconds: f64 = args
        .next()
        .expect("end seconds")
        .parse()
        .expect("end number");
    assert!(end_seconds > start_seconds, "end must be after start");

    let input = std::fs::File::open(input_path).expect("open input");
    let mut reader = BufReader::with_capacity(1024 * 1024, input);
    let mut output = BufWriter::with_capacity(
        1024 * 1024,
        std::fs::File::create(output_path).expect("create output"),
    );
    let mut decoder = HdDecoder::new();
    let mut core = Vec::new();
    let mut exss = Vec::new();
    let mut header = [0u8; HEADER_BYTES];
    let mut sample_position = 0u64;
    let mut written = 0u64;
    let mut speakers = None::<Vec<usize>>;
    let mut extensions = None::<usize>;
    let mut sample_rate = None::<u32>;

    loop {
        match reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => panic!("read core header: {error}"),
        }
        let info = parse_header(&header).expect("parse core");
        core.resize(info.frame_size, 0);
        core[..HEADER_BYTES].copy_from_slice(&header);
        reader
            .read_exact(&mut core[HEADER_BYTES..])
            .expect("read core");
        exss.resize(EXSS_PREFIX_BYTES, 0);
        reader.read_exact(&mut exss).expect("read EXSS prefix");
        let size = exss_size(&exss).expect("parse EXSS size");
        exss.resize(size, 0);
        reader
            .read_exact(&mut exss[EXSS_PREFIX_BYTES..])
            .expect("read EXSS");

        match decoder.decode(&core, &exss) {
            Ok(frame) => {
                let count = frame
                    .samples
                    .iter()
                    .find_map(|channel| channel.as_ref().map(Vec::len))
                    .unwrap_or(0);
                let rate = *sample_rate.get_or_insert(frame.sample_rate);
                let start_sample = (start_seconds * rate as f64) as u64;
                let end_sample = (end_seconds * rate as f64) as u64;
                if sample_position + count as u64 > start_sample && sample_position < end_sample {
                    if !(frame.x_present || frame.x_imax) || frame.x_samples.is_empty() {
                        panic!("requested range has no decoded XLL-X sources");
                    }
                    let active: Vec<_> = frame
                        .samples
                        .iter()
                        .enumerate()
                        .filter_map(|(index, channel)| channel.as_ref().map(|_| index))
                        .collect();
                    if let Some(expected) = &speakers {
                        assert_eq!(&active, expected, "bed speaker order changed");
                    } else {
                        speakers = Some(active.clone());
                    }
                    if let Some(expected) = extensions {
                        assert_eq!(frame.x_samples.len(), expected, "source count changed");
                    } else {
                        extensions = Some(frame.x_samples.len());
                    }
                    for sample in 0..count {
                        let absolute = sample_position + sample as u64;
                        if absolute < start_sample {
                            continue;
                        }
                        if absolute >= end_sample {
                            break;
                        }
                        for &speaker in &active {
                            output
                                .write_all(
                                    &frame.samples[speaker].as_ref().unwrap()[sample].to_le_bytes(),
                                )
                                .expect("write bed");
                        }
                        for channel in &frame.x_samples {
                            output
                                .write_all(&channel[sample].to_le_bytes())
                                .expect("write extension");
                        }
                        written += 1;
                    }
                }
                sample_position += count as u64;
                if sample_position >= end_sample {
                    break;
                }
            }
            Err(HdError::Pending) => {}
            Err(error) => panic!("decode: {error:?}"),
        }
    }
    output.flush().expect("flush");
    println!(
        "rate={} speakers={:?} extensions={} written={} seconds={:.3}",
        sample_rate.unwrap_or(0),
        speakers.unwrap_or_default(),
        extensions.unwrap_or(0),
        written,
        written as f64 / sample_rate.unwrap_or(48_000) as f64
    );
}
