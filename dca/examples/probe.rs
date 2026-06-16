// SPDX-License-Identifier: Apache-2.0
//
// Probe a raw .dts elementary stream: run the core extractor and print the
// first frames' parsed headers. Usage: cargo run -p dca --example probe -- <file> [max]

use std::io::Read;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: probe <file.dts> [max_frames]");
    let max: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);

    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .expect("open")
        .read_to_end(&mut bytes)
        .expect("read");
    println!("read {} bytes from {path}", bytes.len());

    // Scan for both syncwords so we can see what the stream actually contains.
    let core = dca::SYNCWORD_CORE_BE.to_be_bytes();
    let exss = dca::SYNCWORD_SUBSTREAM.to_be_bytes();
    let count = |needle: &[u8]| bytes.windows(4).filter(|w| *w == needle).count();
    println!(
        "core syncwords: {}   substream syncwords: {}",
        count(&core),
        count(&exss)
    );

    let mut ex = dca::Extractor::default();
    ex.push_bytes(&bytes);
    let mut n = 0;
    while let Some(frame) = ex.next_frame().expect("extract") {
        let info = frame.info();
        println!(
            "frame {n}: size={} mode={:?} ch={} sr={} lfe={} blocks={} samples={} pcmres={}",
            info.frame_size,
            info.audio_mode,
            info.audio_mode.channel_count(),
            info.sample_rate,
            info.lfe_present,
            info.npcmblocks,
            info.samples_per_channel(),
            info.source_pcm_res,
        );
        n += 1;
        if n >= max {
            break;
        }
    }
    println!("extracted {n} core frame(s)");
}
