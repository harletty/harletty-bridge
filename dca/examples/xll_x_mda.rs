// SPDX-License-Identifier: Apache-2.0
//
// Test whether the DTS:X XLL-X extension directly embeds the open MDA
// bitstream syntax from ETSI TS 103 223.  The MDA frame-header local label is
// encoded as the distinctive 24-bit sequence 0x81, 0x5a, 0xa5.  Search at
// every bit alignment, and also look for the namespace/asset URI strings used
// by the normative MDA packet grammar.
//
// Usage:
//   cargo run -p dca --release --example xll_x_mda -- <in.dts> [max_mb]

// This is a falsification probe: no hits rules out byte-for-byte MDA framing,
// but does not rule out a compact/private serialization of the MDA model.

use std::io::Read;

use dca::parser::parse_header;
use dca::{exss_substream_size, HdDecoder};

const MDA_FRAME_HEADER: [u8; 3] = [0x81, 0x5a, 0xa5];
const URI_MARKERS: [&[u8]; 3] = [b"mdaif.org", b"urn:x-mda", b"bitstream:afid:"];

fn bit_at(data: &[u8], bit: usize) -> u8 {
    (data[bit / 8] >> (7 - bit % 8)) & 1
}

fn find_bits(data: &[u8], pattern: &[u8]) -> Vec<usize> {
    let pattern_bits = pattern.len() * 8;
    if data.len() * 8 < pattern_bits {
        return Vec::new();
    }

    let mut hits = Vec::new();
    for start in 0..=data.len() * 8 - pattern_bits {
        let matches = (0..pattern_bits).all(|i| bit_at(data, start + i) == bit_at(pattern, i));
        if matches {
            hits.push(start);
        }
    }
    hits
}

fn contains_bytes(data: &[u8], needle: &[u8]) -> bool {
    data.windows(needle.len()).any(|window| window == needle)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: xll_x_mda <in.dts> [max_mb]");
    let max_mb: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(64);

    let mut input = std::fs::File::open(&path).expect("open input");
    let mut bytes = vec![0u8; max_mb * 1024 * 1024];
    let read = input.read(&mut bytes).expect("read input");
    bytes.truncate(read);

    let mut decoder = HdDecoder::new();
    let mut offset = 0usize;
    let mut frames = 0usize;
    let mut payloads = 0usize;
    let mut payload_bytes = 0usize;
    let mut signature_hits = 0usize;
    let mut hit_frames = 0usize;
    let mut alignment_hits = [0usize; 8];
    let mut first_hits = Vec::new();
    let mut uri_hits = [0usize; URI_MARKERS.len()];

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
            if frame.x_present || frame.x_imax {
                let payload = &frame.x_payload;
                payloads += 1;
                payload_bytes += payload.len();

                let hits = find_bits(payload, &MDA_FRAME_HEADER);
                if !hits.is_empty() {
                    hit_frames += 1;
                }
                for bit_offset in hits {
                    signature_hits += 1;
                    alignment_hits[bit_offset % 8] += 1;
                    if first_hits.len() < 16 {
                        first_hits.push((frames, bit_offset));
                    }
                }

                for (i, marker) in URI_MARKERS.iter().enumerate() {
                    if contains_bytes(payload, marker) {
                        uri_hits[i] += 1;
                    }
                }
            }
            frames += 1;
        }

        offset += core.frame_size + exss_len;
    }

    // For uniformly random data, the expected number of accidental 24-bit
    // matches is approximately the number of tested starts divided by 2^24.
    let expected_random = (payload_bytes.saturating_mul(8)) as f64 / (1u64 << 24) as f64;
    println!("read {read} bytes from {path}");
    println!("decoded frames: {frames}; XLL-X payloads: {payloads}");
    println!("payload bytes scanned: {payload_bytes}");
    println!(
        "MDA frame-header signature 81 5a a5: {signature_hits} hits in {hit_frames} frames \
         (random expectation {expected_random:.3})"
    );
    println!("hits by starting bit alignment: {alignment_hits:?}");
    if !first_hits.is_empty() {
        println!("first hits as (frame, bit offset): {first_hits:?}");
    }
    for (marker, hits) in URI_MARKERS.iter().zip(uri_hits) {
        println!(
            "ASCII marker {:?}: {hits} payloads",
            String::from_utf8_lossy(marker)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::find_bits;

    #[test]
    fn finds_byte_aligned_pattern() {
        assert_eq!(
            find_bits(&[0, 0x81, 0x5a, 0xa5, 0], &[0x81, 0x5a, 0xa5]),
            vec![8]
        );
    }

    #[test]
    fn finds_unaligned_pattern() {
        // 0x81_5a_a5 shifted right by three bits, with zero padding.
        assert_eq!(
            find_bits(&[0x00, 0x10, 0x2b, 0x54, 0xa0], &[0x81, 0x5a, 0xa5]),
            vec![11]
        );
    }
}
