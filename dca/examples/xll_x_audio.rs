// SPDX-License-Identifier: Apache-2.0
//
// Decode the four lossless, speaker-unmapped waveforms carried by the XLL-X
// extension and write them as a 32-bit float, four-channel WAV file.
//
// Usage:
//   cargo run -p dca --release --example xll_x_audio -- \
//     <in.dts> <out.wav> [max_mb]

use std::io::{Read, Seek, SeekFrom, Write};

use dca::parser::parse_header;
use dca::{exss_substream_size, HdDecoder};

fn write_wav_header(file: &mut std::fs::File, sample_rate: u32, data_bytes: u32) {
    let channels = 4u16;
    let bits_per_sample = 32u16;
    let block_align = channels * bits_per_sample / 8;
    let byte_rate = sample_rate * block_align as u32;

    file.seek(SeekFrom::Start(0)).expect("seek WAV header");
    file.write_all(b"RIFF").expect("write WAV header");
    file.write_all(&(36 + data_bytes).to_le_bytes())
        .expect("write WAV size");
    file.write_all(b"WAVEfmt ").expect("write WAV header");
    file.write_all(&16u32.to_le_bytes())
        .expect("write fmt size");
    file.write_all(&3u16.to_le_bytes())
        .expect("write IEEE-float format");
    file.write_all(&channels.to_le_bytes())
        .expect("write channel count");
    file.write_all(&sample_rate.to_le_bytes())
        .expect("write sample rate");
    file.write_all(&byte_rate.to_le_bytes())
        .expect("write byte rate");
    file.write_all(&block_align.to_le_bytes())
        .expect("write block alignment");
    file.write_all(&bits_per_sample.to_le_bytes())
        .expect("write sample format");
    file.write_all(b"data").expect("write data tag");
    file.write_all(&data_bytes.to_le_bytes())
        .expect("write data size");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let input_path = args
        .next()
        .expect("usage: xll_x_audio <in.dts> <out.wav> [max_mb]");
    let output_path = args
        .next()
        .expect("usage: xll_x_audio <in.dts> <out.wav> [max_mb]");
    let max_mb: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(256);

    let mut input = std::fs::File::open(&input_path).expect("open input");
    let mut bytes = vec![0u8; max_mb * 1024 * 1024];
    let read = input.read(&mut bytes).expect("read input");
    bytes.truncate(read);

    let mut output = std::fs::File::create(&output_path).expect("create output");
    write_wav_header(&mut output, 48_000, 0);

    let mut decoder = HdDecoder::new();
    let mut offset = 0usize;
    let mut decoded_frames = 0usize;
    let mut failed_frames = 0usize;
    let mut data_bytes = 0u64;
    let mut sample_rate = None;

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
            if frame.x_samples.len() == 4 {
                sample_rate.get_or_insert(frame.sample_rate);
                let samples = frame.x_samples[0].len();
                if frame
                    .x_samples
                    .iter()
                    .any(|channel| channel.len() != samples)
                {
                    panic!("inconsistent XLL-X channel lengths");
                }
                let frame_bytes = samples as u64 * 4 * 4;
                if data_bytes + frame_bytes > u32::MAX as u64 - 36 {
                    panic!("output exceeds classic RIFF/WAV 4 GiB limit");
                }
                for i in 0..samples {
                    for channel in &frame.x_samples {
                        output
                            .write_all(&channel[i].to_le_bytes())
                            .expect("write PCM");
                    }
                }
                data_bytes += frame_bytes;
                decoded_frames += 1;
            } else if frame.x_present {
                failed_frames += 1;
                eprintln!(
                    "XLL-X decode failed at frame {}: {:?}",
                    decoded_frames + failed_frames,
                    frame.x_decode_error
                );
            }
        }
        offset += core.frame_size + exss_len;
    }

    let sample_rate = sample_rate.expect("no four-channel XLL-X audio found");
    write_wav_header(&mut output, sample_rate, data_bytes as u32);
    output.flush().expect("flush output");
    println!(
        "decoded {decoded_frames} frames ({:.3} s) to {output_path}; failures={failed_frames}",
        data_bytes as f64 / (sample_rate as f64 * 4.0 * 4.0)
    );
}
