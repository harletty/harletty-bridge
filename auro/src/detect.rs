// SPDX-License-Identifier: Apache-2.0
//
// Streaming detection over the lossless channels of a decoder.
//
// Each carrier channel gets a fixed ring of recent samples and a small
// candidate list. A run of sixteen sync ones proposes a block; when the
// block's last sample arrives it is copied out contiguously and validated.
// Storage is allocated once, at construction, and nothing is allocated per
// sample: this runs inside the realtime decode path.

use crate::block::{BlockError, BlockInfo, MAX_BLOCK, SYNC_SAMPLES, parse_block, parse_sync};
use crate::layout::{ChannelConfig, Layout};

/// Ring capacity per channel. A power of two so indexing is a mask; larger
/// than [`MAX_BLOCK`] plus the longest run a candidate can be proposed at.
const RING: usize = 8192;
const RING_MASK: u64 = (RING - 1) as u64;
/// Sync candidates kept per channel. A block can hold a few accidental runs
/// of sixteen ones in its audio bits; each is checked and dropped.
const MAX_PENDING: usize = 4;
/// Validated blocks that must agree before a configuration is announced.
const CONFIRMATIONS: u8 = 3;

/// A latched Auro-Codec carrier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Detection {
    pub config: ChannelConfig,
    /// What a full decode would restore.
    pub original: Layout,
    /// What the PCM plays as without a decoder.
    pub carrier: Layout,
    /// Block length in samples.
    pub block_size: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChannelStats {
    /// Blocks that passed the reserved-bit and CRC checks.
    pub valid_blocks: u64,
    /// Sync runs that did not turn into a valid block.
    pub rejected: u64,
    /// Smallest and largest LSB width seen on valid blocks (0 when none).
    pub min_lsb_bits: u8,
    pub max_lsb_bits: u8,
}

struct Candidate {
    start: u64,
    block_size: u16,
}

struct ChannelDetector {
    ring: Box<[i32]>,
    /// Samples pushed so far; the next sample lands at `count & RING_MASK`.
    count: u64,
    run_ones: u32,
    pending: [Candidate; MAX_PENDING],
    pending_len: usize,
    stats: ChannelStats,
}

impl ChannelDetector {
    fn new() -> Self {
        Self {
            ring: vec![0; RING].into_boxed_slice(),
            count: 0,
            run_ones: 0,
            pending: std::array::from_fn(|_| Candidate {
                start: 0,
                block_size: 0,
            }),
            pending_len: 0,
            stats: ChannelStats::default(),
        }
    }

    #[inline]
    fn at(&self, index: u64) -> i32 {
        self.ring[(index & RING_MASK) as usize]
    }

    fn copy_out(&self, start: u64, len: usize, into: &mut [i32]) {
        for (i, slot) in into[..len].iter_mut().enumerate() {
            *slot = self.at(start + i as u64);
        }
    }

    /// Push samples; every validated block is handed to `on_block`.
    fn push(&mut self, samples: &[i32], scratch: &mut [i32], mut on_block: impl FnMut(BlockInfo)) {
        for &s in samples {
            self.ring[(self.count & RING_MASK) as usize] = s;
            self.count += 1;
            if s & 1 != 0 {
                self.run_ones += 1;
            } else {
                // A header is sixteen ones followed by the block's first
                // reserved bit, which is always clear. So a candidate is
                // proposed by the zero that ends a run, for the sixteen
                // samples before that zero: a stray one ahead of a header
                // (the previous block's last payload bit) lengthens the run
                // without moving the header.
                if self.run_ones >= SYNC_SAMPLES as u32 {
                    let start = self.count - 1 - SYNC_SAMPLES as u64;
                    self.copy_out(start, SYNC_SAMPLES, scratch);
                    if let Some(header) = parse_sync(&scratch[..SYNC_SAMPLES]) {
                        if self.pending_len < MAX_PENDING {
                            self.pending[self.pending_len] = Candidate {
                                start,
                                block_size: header.block_size,
                            };
                            self.pending_len += 1;
                        }
                    }
                }
                self.run_ones = 0;
            }
            self.settle(scratch, &mut on_block);
        }
    }

    /// Validate every candidate whose block is now complete.
    fn settle(&mut self, scratch: &mut [i32], on_block: &mut impl FnMut(BlockInfo)) {
        let mut i = 0;
        while i < self.pending_len {
            let Candidate { start, block_size } = self.pending[i];
            let len = usize::from(block_size);
            if start + len as u64 > self.count {
                i += 1;
                continue;
            }
            // Remove by swapping the last candidate in; order is irrelevant.
            self.pending_len -= 1;
            self.pending.swap(i, self.pending_len);

            self.copy_out(start, len, scratch);
            let block = &scratch[..len];
            let Some(header) = parse_sync(block) else {
                self.stats.rejected += 1;
                continue;
            };
            match parse_block(block, header) {
                Ok(info) => {
                    self.stats.valid_blocks += 1;
                    let m = header.lsb_bits;
                    if self.stats.min_lsb_bits == 0 || m < self.stats.min_lsb_bits {
                        self.stats.min_lsb_bits = m;
                    }
                    if m > self.stats.max_lsb_bits {
                        self.stats.max_lsb_bits = m;
                    }
                    on_block(info);
                }
                Err(BlockError::Truncated) => unreachable!("block copied whole"),
                Err(_) => self.stats.rejected += 1,
            }
        }
    }
}

/// Detector over the lossless channels of one stream.
pub struct Detector {
    channels: Vec<ChannelDetector>,
    scratch: Box<[i32]>,
    latched: Option<Detection>,
    /// The configuration seen on the last valid block, and how many blocks in
    /// a row agreed with it.
    candidate: Option<(ChannelConfig, u16, u8)>,
}

impl Detector {
    /// A detector for `channels` carrier channels. All storage is allocated
    /// here.
    pub fn new(channels: usize) -> Self {
        Self {
            channels: (0..channels).map(|_| ChannelDetector::new()).collect(),
            scratch: vec![0; MAX_BLOCK].into_boxed_slice(),
            latched: None,
            candidate: None,
        }
    }

    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Feed the next samples of one channel. Returns the detection the first
    /// time it latches, `None` on every other call.
    pub fn push(&mut self, channel: usize, samples: &[i32]) -> Option<Detection> {
        let already = self.latched.is_some();
        let Some(det) = self.channels.get_mut(channel) else {
            return None;
        };
        let scratch = &mut self.scratch;
        let latched = &mut self.latched;
        let candidate = &mut self.candidate;
        det.push(samples, scratch, |info| {
            if latched.is_some() {
                return;
            }
            let Some(config) = info.config else {
                return;
            };
            let (Some(original), Some(carrier)) = (config.original(), config.carrier()) else {
                return;
            };
            let block_size = info.header.block_size;
            let agreed = match *candidate {
                Some((c, b, n)) if c == config && b == block_size => n + 1,
                _ => 1,
            };
            *candidate = Some((config, block_size, agreed));
            if agreed >= CONFIRMATIONS {
                *latched = Some(Detection {
                    config,
                    original,
                    carrier,
                    block_size,
                });
            }
        });
        if already { None } else { self.latched }
    }

    pub fn detection(&self) -> Option<Detection> {
        self.latched
    }

    pub fn stats(&self, channel: usize) -> ChannelStats {
        self.channels
            .get(channel)
            .map(|c| c.stats)
            .unwrap_or_default()
    }

    /// Forget everything; storage is kept.
    pub fn reset(&mut self) {
        for c in &mut self.channels {
            c.count = 0;
            c.run_ones = 0;
            c.pending_len = 0;
            c.stats = ChannelStats::default();
        }
        self.latched = None;
        self.candidate = None;
    }
}
