// SPDX-License-Identifier: Apache-2.0
//
// Blocks built here by a writer that mirrors the reader's bit map, so the
// parser and the detector are exercised without any real carrier.

use auro::block::{MAX_BLOCK, SYNC_SAMPLES, bit_location, block_crc};
use auro::{BlockError, ChannelConfig, Detector, Layout, parse_block, parse_sync};

/// Deterministic 24-bit "audio": a linear congruential generator, so the
/// low bits are as random as a real mix's before the encoder borrows them.
struct Lcg(u32);

impl Lcg {
    fn next(&mut self) -> i32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((self.0 >> 8) as i32) << 8 >> 8
    }
}

fn set_bit(samples: &mut [i32], idx: usize, bit: u32, value: u32) {
    let mask = 1i32 << bit;
    if value & 1 != 0 {
        samples[idx] |= mask;
    } else {
        samples[idx] &= !mask;
    }
}

struct Writer<'a> {
    samples: &'a mut [i32],
    m: usize,
    pos: usize,
}

impl Writer<'_> {
    fn put(&mut self, bits: u32, value: u32) {
        for i in (0..bits).rev() {
            let (idx, b) = bit_location(self.pos, self.m, true);
            set_bit(self.samples, idx, b, (value >> i) & 1);
            self.pos += 1;
        }
    }
}

/// Build one block: `block_size` samples borrowing `m` bits, announcing
/// `config` on stream slot `slot`. `adol` is appended after the `0x1E`
/// announcement, before the terminator.
fn build_block(
    seed: u32,
    block_size: usize,
    m: usize,
    config: u8,
    slot: u8,
    adol: &[(u8, u32, u32)],
) -> Vec<i32> {
    assert!(block_size <= MAX_BLOCK);
    let mut lcg = Lcg(seed);
    let mut s: Vec<i32> = (0..block_size).map(|_| lcg.next()).collect();
    // Header planes.
    for (i, v) in s.iter_mut().enumerate().take(SYNC_SAMPLES) {
        *v &= !0b111;
        *v |= 1;
        let _ = i;
    }
    let size_code = if block_size == 1000 {
        0x3Du32
    } else {
        ((block_size - SYNC_SAMPLES) >> 4) as u32
    };
    for i in 0..8 {
        set_bit(&mut s, i, 2, (size_code >> (7 - i)) & 1);
    }
    for i in 8..12 {
        set_bit(&mut s, i, 2, 0);
    }
    let width_code = 14 - m as u32;
    for i in 12..16 {
        set_bit(&mut s, i, 2, (width_code >> (15 - i)) & 1);
    }
    // Payload.
    let mut w = Writer {
        samples: &mut s,
        m,
        pos: 48,
    };
    w.put(8, 1);
    w.put(8, 10);
    w.put(1, 0);
    w.put(1, 0);
    w.put(2, 0);
    w.put(4, 0);
    w.put(8, 3);
    w.put(8, 1); // one ADOL block
    w.put(8, 5);
    w.put(8, u32::from(slot));
    w.put(8, 0xFF);
    w.put(8, 0xFF);
    w.put(8, 0xFF);
    for _ in 0..4 {
        w.put(8, 0);
    }
    w.put(8, 1); // tag
    w.put(8, 0x1E);
    w.put(8, u32::from(config));
    for &(op, a, b) in adol {
        w.put(8, u32::from(op));
        match op {
            0x40 => {
                w.put(8, a);
                w.put(8, b);
            }
            0x01 | 0x03 | 0x5A..=0x62 => w.put(16, a),
            0x64..=0x6C => w.put(24, a),
            0x0E | 0x46 | 0x6E..=0x76 | 0x80..=0x85 | 0x8C..=0x90 => w.put(32, a),
            _ => w.put(8, a),
        }
    }
    w.put(8, 0);
    // Reserved bits.
    let mut pos = 17 * m - 1;
    while pos < m * block_size {
        let (idx, b) = bit_location(pos, m, false);
        set_bit(&mut s, idx, b, 0);
        pos += SYNC_SAMPLES * m;
    }
    // CRC last: it covers everything else.
    let crc = block_crc(&s);
    for i in 0..16 {
        set_bit(&mut s, i, 1, u32::from((crc >> (15 - i)) & 1));
    }
    s
}

#[test]
fn a_synthetic_block_parses_back() {
    let block = build_block(7, 1000, 3, 62, 0, &[(0x64, 0x010203, 0), (0x40, 2, 200)]);
    let header = parse_sync(&block).expect("sync");
    assert_eq!(header.block_size, 1000);
    assert_eq!(header.lsb_bits, 3);
    let info = parse_block(&block, header).expect("block");
    assert_eq!(info.config, Some(ChannelConfig(62)));
    assert_eq!(info.stream_ids, [0, 0xFF, 0xFF, 0xFF]);
    assert_eq!(info.adol_instructions, 4);
}

#[test]
fn wider_borrow_and_a_power_of_two_block() {
    for m in [4usize, 5, 8, 10] {
        let block = build_block(11 + m as u32, 1024, m, 50, 4, &[]);
        let header = parse_sync(&block).expect("sync");
        assert_eq!((header.block_size, header.lsb_bits), (1024, m as u8));
        let info = parse_block(&block, header).expect("block");
        assert_eq!(info.config, Some(ChannelConfig(50)));
    }
}

#[test]
fn one_flipped_audio_bit_fails_the_crc() {
    let mut block = build_block(3, 1000, 3, 62, 0, &[]);
    let header = parse_sync(&block).unwrap();
    block[500] ^= 1 << 12;
    assert_eq!(parse_block(&block, header), Err(BlockError::Crc));
}

#[test]
fn a_set_reserved_bit_is_rejected_before_the_crc() {
    let mut block = build_block(3, 1000, 3, 62, 0, &[]);
    let header = parse_sync(&block).unwrap();
    // m = 3: bit 0 of sample 16 is the first reserved bit.
    block[16] |= 1;
    assert_eq!(parse_block(&block, header), Err(BlockError::ReservedBit));
}

#[test]
fn the_detector_latches_after_three_agreeing_blocks() {
    let mut stream = Vec::new();
    for seed in 0..4u32 {
        stream.extend(build_block(100 + seed, 1000, 3, 62, 0, &[]));
    }
    let mut det = Detector::new(2);
    let mut latched = None;
    let mut announcements = 0;
    // Odd chunking, and a second channel that carries nothing.
    for (i, chunk) in stream.chunks(333).enumerate() {
        if let Some(d) = det.push(0, chunk) {
            announcements += 1;
            latched = Some((d, i));
        }
        det.push(1, &vec![0x1234 << 3; chunk.len()]);
    }
    let (d, when) = latched.expect("latched");
    assert_eq!(announcements, 1);
    assert_eq!(d.config, ChannelConfig(62));
    assert_eq!(d.original, Layout(32703));
    assert_eq!(d.carrier, Layout(447));
    assert_eq!(d.block_size, 1000);
    // Three blocks are 3000 samples: latched on the chunk that completes them.
    assert_eq!(when, 3000 / 333);
    assert_eq!(det.stats(0).valid_blocks, 4);
    assert_eq!(det.stats(1).valid_blocks, 0);
}

#[test]
fn noise_never_latches() {
    let mut lcg = Lcg(99);
    let noise: Vec<i32> = (0..200_000).map(|_| lcg.next()).collect();
    let mut det = Detector::new(1);
    assert!(det.push(0, &noise).is_none());
    assert!(det.detection().is_none());
    assert_eq!(det.stats(0).valid_blocks, 0);
}

#[test]
fn a_disagreeing_block_restarts_the_count() {
    let mut stream = Vec::new();
    stream.extend(build_block(1, 1000, 3, 62, 0, &[]));
    stream.extend(build_block(2, 1000, 3, 62, 0, &[]));
    stream.extend(build_block(3, 1000, 3, 50, 0, &[]));
    stream.extend(build_block(4, 1000, 3, 62, 0, &[]));
    stream.extend(build_block(5, 1000, 3, 62, 0, &[]));
    let mut det = Detector::new(1);
    assert!(det.push(0, &stream).is_none());
    stream.clear();
    stream.extend(build_block(6, 1000, 3, 62, 0, &[]));
    assert_eq!(
        det.push(0, &stream).map(|d| d.config),
        Some(ChannelConfig(62))
    );
}
