// SPDX-License-Identifier: Apache-2.0
//
// The carrier channels of one stream, unfolded together.
//
// Each carrier is decoded on its own; this joins them. Blocks are only
// decodable once their last sample has arrived, so output lags input by one
// block. Samples no block claims — before the first block, after the last,
// or in a block that failed — play the carrier as authored for the bed and
// silence for the heights, which is what the disc sounds like without a
// decoder. A stream folded into two carriers is averaged.

use crate::block::MAX_BLOCK;
use crate::decode::{ChannelDecoder, StreamId};
use crate::detect::ChannelStats;

/// Output ring per stream. Holds the latency plus the largest push plus one
/// block. Power of two.
const RING: usize = 16384;
const RING_MASK: u64 = (RING - 1) as u64;
/// Largest single push the ring is sized for.
pub const MAX_PUSH: usize = 4096;
/// Output lags input by this many samples.
pub const LATENCY: usize = MAX_BLOCK;

/// Stream ids run 0..16.
const STREAMS: usize = 16;

struct Ring {
    sum: Box<[i32]>,
    hits: Box<[u8]>,
}

impl Ring {
    fn new() -> Self {
        Self {
            sum: vec![0; RING].into_boxed_slice(),
            hits: vec![0; RING].into_boxed_slice(),
        }
    }
}

struct Carrier {
    decoder: ChannelDecoder,
    /// The bed stream this carrier plays as without a decoder.
    id: StreamId,
    /// The carrier as authored, for the samples no block claims.
    plain: Box<[i32]>,
    /// Which samples a decoded block claimed.
    claimed: Box<[u8]>,
}

pub struct Unfolder {
    carriers: Vec<Carrier>,
    rings: Vec<Option<Ring>>,
    released: u64,
    latency: usize,
}

impl Unfolder {
    /// `carrier_ids` says which bed stream each carrier channel plays as;
    /// `outputs` which streams will be asked for.
    pub fn new(carrier_ids: &[StreamId], outputs: &[StreamId]) -> Self {
        let mut rings: Vec<Option<Ring>> = (0..STREAMS).map(|_| None).collect();
        for id in outputs {
            if let Some(slot) = rings.get_mut(usize::from(id.0)) {
                *slot = Some(Ring::new());
            }
        }
        Self {
            carriers: carrier_ids
                .iter()
                .map(|&id| Carrier {
                    decoder: ChannelDecoder::new(),
                    id,
                    plain: vec![0; RING].into_boxed_slice(),
                    claimed: vec![0; RING].into_boxed_slice(),
                })
                .collect(),
            rings,
            released: 0,
            latency: LATENCY,
        }
    }

    pub fn carrier_count(&self) -> usize {
        self.carriers.len()
    }

    pub fn stats(&self, carrier: usize) -> ChannelStats {
        self.carriers[carrier].decoder.stats()
    }

    pub fn decode_errors(&self) -> u64 {
        self.carriers.iter().map(|c| c.decoder.decode_errors).sum()
    }

    /// Feed the next samples of one carrier. At most [`MAX_PUSH`] at a
    /// time; every carrier must be fed the same amount before `take`.
    pub fn push(&mut self, carrier: usize, samples: &[i32]) {
        debug_assert!(samples.len() <= MAX_PUSH);
        let Self {
            carriers, rings, ..
        } = self;
        let c = &mut carriers[carrier];
        let base = c.decoder.position();
        for (i, &s) in samples.iter().enumerate() {
            let at = ((base + i as u64) & RING_MASK) as usize;
            c.plain[at] = s;
            c.claimed[at] = 0;
        }
        let claimed = &mut c.claimed;
        c.decoder.push(samples, |d| {
            let n = usize::from(d.header.block_size);
            for i in 0..n {
                claimed[((d.start + i as u64) & RING_MASK) as usize] = 1;
            }
            for (id, out) in d.outputs.iter().take(usize::from(d.stream.mode)) {
                let Some(Some(ring)) = rings.get_mut(usize::from(id.0)) else {
                    continue;
                };
                for (i, &v) in out[..n].iter().enumerate() {
                    let at = ((d.start + i as u64) & RING_MASK) as usize;
                    if ring.hits[at] == 0 {
                        ring.sum[at] = v;
                    } else {
                        ring.sum[at] = ring.sum[at].saturating_add(v);
                    }
                    ring.hits[at] = ring.hits[at].saturating_add(1);
                }
            }
        });
    }

    /// Samples every carrier has passed, minus the latency.
    fn frontier(&self) -> u64 {
        let min = self
            .carriers
            .iter()
            .map(|c| c.decoder.position())
            .min()
            .unwrap_or(0);
        min.saturating_sub(self.latency as u64)
    }

    /// Samples that can be taken now.
    pub fn ready(&self) -> usize {
        (self.frontier() - self.released) as usize
    }

    /// No more input is coming: everything pushed becomes takeable.
    pub fn finish(&mut self) {
        self.latency = 0;
    }

    /// Take up to `out.len() / ids.len()` frames, interleaved in the order
    /// of `ids`. Returns the frames written.
    pub fn take(&mut self, ids: &[StreamId], out: &mut [i32]) -> usize {
        if ids.is_empty() {
            return 0;
        }
        let frames = self.ready().min(out.len() / ids.len());
        for f in 0..frames {
            let pos = self.released + f as u64;
            let at = (pos & RING_MASK) as usize;
            for (j, id) in ids.iter().enumerate() {
                out[f * ids.len() + j] = self.sample(*id, at);
            }
        }
        // Clear what was taken so the ring is clean when it wraps back.
        for f in 0..frames {
            let at = ((self.released + f as u64) & RING_MASK) as usize;
            for ring in self.rings.iter_mut().flatten() {
                ring.hits[at] = 0;
                ring.sum[at] = 0;
            }
            for c in &mut self.carriers {
                c.claimed[at] = 0;
            }
        }
        self.released += frames as u64;
        frames
    }

    fn sample(&self, id: StreamId, at: usize) -> i32 {
        if let Some(Some(ring)) = self.rings.get(usize::from(id.0)) {
            let hits = ring.hits[at];
            if hits == 1 {
                return ring.sum[at];
            }
            if hits > 1 {
                return ring.sum[at] / i32::from(hits);
            }
        }
        // Nothing decoded here: the carrier as authored, when it is this
        // stream and no block claimed the sample.
        for c in &self.carriers {
            if c.id == id && c.claimed[at] == 0 {
                return c.plain[at];
            }
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_pcm_passes_through_after_the_latency() {
        let ids = [StreamId(0), StreamId(1)];
        let mut u = Unfolder::new(&ids, &[StreamId(0), StreamId(1), StreamId(9)]);
        let left: Vec<i32> = (0..6000).map(|i| i * 8).collect();
        let right: Vec<i32> = (0..6000).map(|i| -i * 8).collect();
        for chunk in 0..6 {
            u.push(0, &left[chunk * 1000..(chunk + 1) * 1000]);
            u.push(1, &right[chunk * 1000..(chunk + 1) * 1000]);
        }
        assert_eq!(u.ready(), 6000 - LATENCY);
        let mut out = vec![0i32; 3 * 6000];
        let n = u.take(&[StreamId(0), StreamId(1), StreamId(9)], &mut out);
        assert_eq!(n, 6000 - LATENCY);
        assert_eq!(&out[..6], &[0, 0, 0, 8, -8, 0]);
        u.finish();
        let n2 = u.take(&[StreamId(0), StreamId(1), StreamId(9)], &mut out);
        assert_eq!(n2, LATENCY);
        assert_eq!(out[0], (6000 - LATENCY as i32) * 8);
    }
}
