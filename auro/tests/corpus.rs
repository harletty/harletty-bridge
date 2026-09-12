// SPDX-License-Identifier: Apache-2.0
//
// Against a real carrier, when one is at hand. `HARLETTY_AURO_CORPUS` names
// an interleaved little-endian 32-bit PCM file (24-bit samples left-aligned,
// as `ffmpeg -f s32le` writes them) and `HARLETTY_AURO_CORPUS_CHANNELS` its
// channel count; `HARLETTY_AURO_CORPUS_LAYOUT` the layout name expected.

use auro::{Detector, Layout};

#[test]
fn a_real_carrier_latches_its_layout() {
    let (Ok(path), Ok(channels)) = (
        std::env::var("HARLETTY_AURO_CORPUS"),
        std::env::var("HARLETTY_AURO_CORPUS_CHANNELS"),
    ) else {
        eprintln!("skipping: HARLETTY_AURO_CORPUS is not set");
        return;
    };
    let channels: usize = channels.parse().unwrap();
    let expected =
        std::env::var("HARLETTY_AURO_CORPUS_LAYOUT").unwrap_or_else(|_| "7.1_5H_1T".into());
    let bytes = std::fs::read(&path).unwrap();
    let frames = bytes.len() / (4 * channels);
    let mut det = Detector::new(channels);
    let mut latched = None;
    let chunk = 512;
    let mut buf = vec![0i32; chunk];
    for start in (0..frames).step_by(chunk) {
        let n = chunk.min(frames - start);
        for ch in 0..channels {
            for (i, slot) in buf[..n].iter_mut().enumerate() {
                let o = ((start + i) * channels + ch) * 4;
                *slot =
                    i32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]) >> 8;
            }
            if let Some(d) = det.push(ch, &buf[..n]) {
                latched = Some(d);
            }
        }
    }
    let d = latched.expect("the corpus should latch");
    assert_eq!(d.original.name(), Some(expected.as_str()));
    assert_eq!(d.carrier, Layout(if channels == 8 { 447 } else { 63 }));
    for ch in 0..channels {
        let s = det.stats(ch);
        eprintln!("channel {ch}: {s:?}");
        assert!(s.valid_blocks > 10 * s.rejected, "channel {ch}: {s:?}");
    }
}
