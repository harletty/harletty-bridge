// SPDX-License-Identifier: Apache-2.0
//
// The published original/encoded pair, when it is at hand: an Auro-2D
// carrier (three channels) that folds a 5.1 mix, and that 5.1 mix.
// `HARLETTY_AURO_PAIR_CARRIER` and `HARLETTY_AURO_PAIR_ORIGINAL` name the two
// as interleaved little-endian 32-bit PCM (24-bit left-aligned), with
// `HARLETTY_AURO_PAIR_CHANNELS` = "3,6".

use auro::{ChannelDecoder, StreamId};

fn read_s32(path: &str, channels: usize) -> Vec<Vec<i32>> {
    let bytes = std::fs::read(path).unwrap();
    let frames = bytes.len() / (4 * channels);
    let mut out = vec![vec![0i32; frames]; channels];
    for f in 0..frames {
        for (c, ch) in out.iter_mut().enumerate() {
            let o = (f * channels + c) * 4;
            ch[f] = i32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]) >> 8;
        }
    }
    out
}

#[test]
fn the_folded_five_one_comes_back_out_of_the_stereo_carrier() {
    let (Ok(carrier_path), Ok(original_path)) = (
        std::env::var("HARLETTY_AURO_PAIR_CARRIER"),
        std::env::var("HARLETTY_AURO_PAIR_ORIGINAL"),
    ) else {
        eprintln!("skipping: HARLETTY_AURO_PAIR_CARRIER is not set");
        return;
    };
    let chans = std::env::var("HARLETTY_AURO_PAIR_CHANNELS").unwrap_or_else(|_| "3,6".into());
    let mut it = chans.split(',').map(|s| s.parse::<usize>().unwrap());
    let (nc, no) = (it.next().unwrap(), it.next().unwrap());
    let carrier = read_s32(&carrier_path, nc);
    let original = read_s32(&original_path, no);
    let frames = carrier[0].len();

    // Restored streams by id, and how many carriers contributed to each
    // sample (a stream folded into two carriers comes back twice).
    let mut restored: Vec<Vec<i64>> = vec![vec![0; frames]; 16];
    let mut hits: Vec<Vec<u8>> = vec![vec![0; frames]; 16];
    let mut blocks = 0u64;
    let mut errors = 0u64;
    for ch in &carrier {
        let mut dec = ChannelDecoder::new();
        for chunk in ch.chunks(1024) {
            dec.push(chunk, |d| {
                blocks += 1;
                let n = usize::from(d.header.block_size);
                let start = d.start as usize;
                for (id, out) in d.outputs.iter().take(usize::from(d.stream.mode)) {
                    let StreamId(i) = *id;
                    for (k, &v) in out[..n].iter().enumerate() {
                        restored[usize::from(i)][start + k] += i64::from(v);
                        hits[usize::from(i)][start + k] += 1;
                    }
                }
            });
        }
        errors += dec.decode_errors;
    }
    eprintln!("blocks {blocks}, decode errors {errors}");
    assert!(blocks > 0);
    assert_eq!(errors, 0);

    // Stream ids 0..6 are the 5.1 in the original's order.
    for id in 0..6usize {
        let orig = &original[id];
        let mut sxy = 0f64;
        let mut sxx = 0f64;
        let mut syy = 0f64;
        let mut covered = 0usize;
        for f in 0..frames {
            if hits[id][f] == 0 {
                continue;
            }
            let x = restored[id][f] as f64 / f64::from(hits[id][f]);
            let y = f64::from(orig[f]);
            sxy += x * y;
            sxx += x * x;
            syy += y * y;
            covered += 1;
        }
        if syy == 0.0 {
            eprintln!(
                "id {id} ({}): original is silent, skipped",
                StreamId(id as u8).name()
            );
            continue;
        }
        let corr = sxy / (sxx.sqrt() * syy.sqrt());
        let gain_db = 10.0 * (sxx / syy).log10();
        eprintln!(
            "id {id} ({}): covered {covered}/{frames}, corr {corr:.6}, gain {gain_db:+.3} dB",
            StreamId(id as u8).name()
        );
        assert!(covered > frames / 2, "id {id} barely covered");
        assert!(corr > 0.9999, "id {id}: corr {corr}");
        assert!(gain_db.abs() < 0.1, "id {id}: gain {gain_db} dB");
    }
}
