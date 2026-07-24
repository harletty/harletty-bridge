// SPDX-License-Identifier: Apache-2.0
//
// Write one interleaved f32le RMS row per decoded XLL frame: the eight
// compatible-bed channels followed by every XLL-X extension source. This is a
// compact research artifact for aligning alternate encodes of the same
// programme; it is not used by the realtime decoder.
//
// Usage:
//   cargo run -p dca --release --example xll_rms_envelope -- \
//     <track.dts> <output.f32le>

use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};

use dca::{parse_header, HdDecoder, HdError, SYNCWORD_SUBSTREAM};

const HEADER_BYTES: usize = 18;
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

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let square_sum = samples
        .iter()
        .map(|&sample| {
            let sample = sample as f64;
            sample * sample
        })
        .sum::<f64>();
    (square_sum / samples.len() as f64).sqrt() as f32
}

fn main() {
    let mut args = std::env::args().skip(1);
    let input_path = args.next().expect("input.dts");
    let output_path = args.next().expect("output.f32le");

    let input = std::fs::File::open(&input_path).expect("open input");
    let mut reader = BufReader::with_capacity(1024 * 1024, input);
    let output = std::fs::File::create(&output_path).expect("create output");
    let mut output = BufWriter::with_capacity(1024 * 1024, output);
    let mut decoder = HdDecoder::new();
    let mut core = Vec::new();
    let mut exss = Vec::new();
    let mut header = [0u8; HEADER_BYTES];
    let mut extensions = None::<usize>;
    let mut frame_samples = None::<usize>;
    let mut sample_rate = None::<u32>;
    let mut frames = 0u64;

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

        exss.resize(16, 0);
        reader.read_exact(&mut exss).expect("read EXSS prefix");
        let size = exss_size(&exss).expect("parse EXSS size");
        exss.resize(size, 0);
        reader
            .read_exact(&mut exss[16..])
            .expect("read EXSS payload");

        let frame = match decoder.decode(&core, &exss) {
            Ok(frame) => frame,
            Err(HdError::Pending) => continue,
            Err(error) => panic!("decode frame {frames}: {error:?}"),
        };
        if frame.x_samples.is_empty() {
            continue;
        }
        let count = frame.x_samples[0].len();
        if frame.x_samples.iter().any(|channel| channel.len() != count) {
            panic!("inconsistent extension channel lengths at frame {frames}");
        }
        if let Some(expected) = extensions {
            assert_eq!(
                frame.x_samples.len(),
                expected,
                "extension count changed at frame {frames}"
            );
        } else {
            extensions = Some(frame.x_samples.len());
        }
        if let Some(expected) = frame_samples {
            assert_eq!(count, expected, "frame size changed at frame {frames}");
        } else {
            frame_samples = Some(count);
        }
        if let Some(expected) = sample_rate {
            assert_eq!(
                frame.sample_rate, expected,
                "sample rate changed at frame {frames}"
            );
        } else {
            sample_rate = Some(frame.sample_rate);
        }

        for (speaker, _) in BED {
            let samples = frame
                .samples
                .get(speaker)
                .and_then(Option::as_deref)
                .expect("missing bed speaker");
            assert_eq!(
                samples.len(),
                count,
                "bed frame size changed at frame {frames}"
            );
            output
                .write_all(&rms(samples).to_le_bytes())
                .expect("write bed RMS");
        }
        for channel in &frame.x_samples {
            output
                .write_all(&rms(channel).to_le_bytes())
                .expect("write extension RMS");
        }
        frames += 1;
    }
    output.flush().expect("flush output");

    println!("file={input_path}");
    println!("output={output_path}");
    println!(
        "sample_rate={} frame_samples={} bed={} extensions={} channels={} frames={frames}",
        sample_rate.unwrap_or(0),
        frame_samples.unwrap_or(0),
        BED.len(),
        extensions.unwrap_or(0),
        BED.len() + extensions.unwrap_or(0),
    );
    println!(
        "channel_order={}{}",
        BED.map(|(_, name)| name).join(","),
        (0..extensions.unwrap_or(0))
            .map(|source| format!(",X{source}"))
            .collect::<String>()
    );
}
