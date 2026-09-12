// SPDX-License-Identifier: Apache-2.0
//
// Auro-3D over the DTS-HD MA path.
//
// A lossless DTS-HD track may be an Auro-Codec carrier: the low bits of its
// PCM then fold a larger layout (a 7.1 carrier holding 13.1). Whether it is
// one is only known a few blocks in, and unfolding lags input by a block,
// so the frames decoded so far are held back until the question is settled.
// If the carrier is confirmed, the held frames are replayed into the
// unfolder and every frame from then on is the unfolded layout, with the
// bed and the height layer as labelled fixed channels. If it is not, the
// held frames go out as they are and this stage steps aside for the rest
// of the stream.
//
// Costs: nothing per sample beyond the detector's ring writes; per block,
// one payload gather, one residual decode and one unmix on fixed storage.
// The unfolder and the decoders are allocated once, when the carrier is
// confirmed.

use abi_stable::std_types::RVec;
use auro::{Detector, StreamId, Unfolder};
use bridge_api::{RChannelLabel, RDecodedFrame};

use crate::labels::auro_stream_to_r;

/// Samples to hold back before giving up on a stream that shows no valid
/// block at all: two of the largest blocks, so a block of any size has had
/// time to complete once.
const NO_BLOCK_LIMIT: usize = 2 * auro::block::MAX_BLOCK;
/// Samples to hold back while blocks validate but the layout has not
/// latched yet.
const HOLD_LIMIT: usize = 4 * auro::block::MAX_BLOCK;

/// The bed stream a DCA speaker index plays as.
fn speaker_stream(speaker: usize) -> StreamId {
    StreamId(match speaker {
        0 => 2,
        1 => 0,
        2 => 1,
        3 => 4,
        4 => 5,
        5 => 3,
        6 => 6,
        7 => 7,
        8 => 8,
        _ => 0xff,
    })
}

struct Held {
    frame: RDecodedFrame,
    /// DCA speaker index of each of the frame's first channels.
    speakers: Vec<usize>,
}

enum Phase {
    Undecided {
        detector: Detector,
        held: Vec<Held>,
        held_samples: usize,
    },
    Unfolding {
        unfolder: Unfolder,
        /// DCA speaker index of each carrier channel, in unfolder order.
        speakers: Vec<usize>,
        outputs: Vec<StreamId>,
        labels: RVec<RChannelLabel>,
        sample_rate: u32,
        scratch: Vec<i32>,
    },
    Plain,
}

pub(crate) struct DtsAuroState {
    phase: Phase,
}

impl Default for DtsAuroState {
    fn default() -> Self {
        Self {
            phase: Phase::Undecided {
                detector: Detector::new(0),
                held: Vec::new(),
                held_samples: 0,
            },
        }
    }
}

impl DtsAuroState {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_unfolding(&self) -> bool {
        matches!(self.phase, Phase::Unfolding { .. })
    }

    /// One built DTS-HD frame, whose first `speakers.len()` channels are the
    /// lossless bed in that speaker order, with the decoder's integer
    /// output for it. Appends to `out` whatever can go out now.
    pub(crate) fn route<'a>(
        &mut self,
        frame: RDecodedFrame,
        speakers: &[usize],
        lossless: impl Iterator<Item = (usize, &'a [i32])>,
        out: &mut RVec<RDecodedFrame>,
    ) {
        match &mut self.phase {
            Phase::Plain => out.push(frame),
            Phase::Unfolding {
                unfolder,
                speakers: carriers,
                outputs,
                labels,
                sample_rate,
                scratch,
            } => {
                for (speaker, samples) in lossless {
                    if let Some(index) = carriers.iter().position(|&s| s == speaker) {
                        for chunk in samples.chunks(auro::unfold::MAX_PUSH) {
                            unfolder.push(index, chunk);
                        }
                    }
                }
                Self::drain(unfolder, outputs, labels, *sample_rate, scratch, out);
            }
            Phase::Undecided {
                detector,
                held,
                held_samples,
            } => {
                // A frame with more channels than its bed carries a DTS:X
                // presentation, which no Auro carrier does.
                if frame.channel_count as usize != speakers.len() {
                    self.give_up(out);
                    out.push(frame);
                    return;
                }
                if detector.channel_count() == 0 {
                    let count = speakers.iter().copied().max().map_or(0, |m| m + 1);
                    *detector = Detector::new(count);
                }
                let n = frame.sample_count as usize;
                let mut latched = None;
                for (speaker, samples) in lossless {
                    if let Some(d) = detector.push(speaker, samples) {
                        latched = Some(d);
                    }
                }
                held.push(Held {
                    frame,
                    speakers: speakers.to_vec(),
                });
                *held_samples += n;
                if let Some(detection) = latched {
                    self.start_unfolding(detection, out);
                    return;
                }
                let any_block = speakers.iter().any(|&s| detector.stats(s).valid_blocks > 0);
                let limit = if any_block {
                    HOLD_LIMIT
                } else {
                    NO_BLOCK_LIMIT
                };
                if *held_samples > limit {
                    self.give_up(out);
                }
            }
        }
    }

    /// A frame without lossless output (core only) cannot be a carrier.
    pub(crate) fn not_a_carrier(&mut self, out: &mut RVec<RDecodedFrame>) {
        if matches!(self.phase, Phase::Undecided { .. }) {
            self.give_up(out);
        }
    }

    fn give_up(&mut self, out: &mut RVec<RDecodedFrame>) {
        let phase = std::mem::replace(&mut self.phase, Phase::Plain);
        if let Phase::Undecided { held, .. } = phase {
            for h in held {
                out.push(h.frame);
            }
        }
    }

    fn start_unfolding(&mut self, detection: auro::Detection, out: &mut RVec<RDecodedFrame>) {
        let phase = std::mem::replace(&mut self.phase, Phase::Plain);
        let Phase::Undecided { held, .. } = phase else {
            return;
        };
        let Some(first) = held.first() else {
            return;
        };
        let outputs: Vec<StreamId> = match detection.original.streams() {
            Some(streams) => streams.as_slice().iter().map(|&id| StreamId(id)).collect(),
            None => {
                log::warn!(
                    "dts: Auro-3D layout {:?} has no known stream list; keeping the carrier as is",
                    detection.original
                );
                for h in held {
                    out.push(h.frame);
                }
                return;
            }
        };
        let name = |layout: auro::Layout| layout.name().unwrap_or("?");
        log::info!(
            "dts: Auro-3D carrier {} folded into {} ({}-sample blocks); unfolding to {} channels",
            name(detection.original),
            name(detection.carrier),
            detection.block_size,
            outputs.len()
        );
        let speakers = first.speakers.clone();
        let carrier_ids: Vec<StreamId> = speakers.iter().map(|&s| speaker_stream(s)).collect();
        let labels: RVec<RChannelLabel> = outputs.iter().map(|&id| auro_stream_to_r(id)).collect();
        let sample_rate = first.frame.sampling_frequency;
        let mut unfolder = Unfolder::new(&carrier_ids, &outputs);
        unfolder.set_latency(usize::from(detection.block_size));
        let mut scratch = Vec::new();
        // Replay what was held: the frames' PCM is the lossless bed exactly.
        let mut column = Vec::new();
        for h in &held {
            let channels = h.frame.channel_count as usize;
            let n = h.frame.sample_count as usize;
            for index in 0..speakers.len() {
                column.clear();
                column.extend((0..n).map(|s| h.frame.pcm[s * channels + index]));
                for chunk in column.chunks(auro::unfold::MAX_PUSH) {
                    unfolder.push(index, chunk);
                }
            }
            Self::drain(
                &mut unfolder,
                &outputs,
                &labels,
                sample_rate,
                &mut scratch,
                out,
            );
        }
        self.phase = Phase::Unfolding {
            unfolder,
            speakers,
            outputs,
            labels,
            sample_rate,
            scratch,
        };
    }

    fn drain(
        unfolder: &mut Unfolder,
        outputs: &[StreamId],
        labels: &RVec<RChannelLabel>,
        sample_rate: u32,
        scratch: &mut Vec<i32>,
        out: &mut RVec<RDecodedFrame>,
    ) {
        let ready = unfolder.ready();
        if ready == 0 {
            return;
        }
        scratch.clear();
        scratch.resize(ready * outputs.len(), 0);
        let frames = unfolder.take(outputs, scratch);
        scratch.truncate(frames * outputs.len());
        let mut pcm: RVec<i32> = RVec::with_capacity(scratch.len());
        pcm.extend(scratch.iter().copied());
        out.push(RDecodedFrame {
            sampling_frequency: sample_rate,
            sample_count: frames as u32,
            channel_count: outputs.len() as u32,
            pcm,
            channel_labels: labels.clone(),
            metadata: RVec::new(),
            drc_gain: 1.0,
            drc_ramp_duration: 0,
            dialogue_level: None.into(),
            is_new_segment: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use auro::block::{SYNC_SAMPLES, bit_location, block_crc};

    const BLOCK: usize = 1024;
    const M: usize = 3;

    fn set_bit(samples: &mut [i32], idx: usize, bit: u32, value: u32) {
        let mask = 1i32 << bit;
        if value & 1 != 0 {
            samples[idx] |= mask;
        } else {
            samples[idx] &= !mask;
        }
    }

    /// One block whose payload announces a single stream `id` on channel
    /// configuration `config`, over `audio` (its low bits are replaced).
    fn block(audio: &[i32], config: u8, id: u8) -> Vec<i32> {
        let mut s: Vec<i32> = audio.iter().map(|&v| v & !0b111).collect();
        for v in s.iter_mut().take(SYNC_SAMPLES) {
            *v |= 1;
        }
        let size_code = ((BLOCK - SYNC_SAMPLES) >> 4) as u32;
        for i in 0..8 {
            set_bit(&mut s, i, 2, (size_code >> (7 - i)) & 1);
        }
        let width_code = 14 - M as u32;
        for i in 12..16 {
            set_bit(&mut s, i, 2, (width_code >> (15 - i)) & 1);
        }
        let mut pos = 48usize;
        let mut put = |s: &mut [i32], bits: u32, value: u32| {
            for i in (0..bits).rev() {
                let (idx, b) = bit_location(pos, M, true);
                set_bit(s, idx, b, (value >> i) & 1);
                pos += 1;
            }
        };
        put(&mut s, 8, 1);
        put(&mut s, 8, 10);
        put(&mut s, 8, 0); // fixed Rice parameter 0
        put(&mut s, 8, 0); // codebook count code
        put(&mut s, 8, 1); // one ADOL block
        put(&mut s, 8, 0); // codebook width
        put(&mut s, 8, u32::from(id));
        for _ in 0..3 {
            put(&mut s, 8, 0xff);
        }
        for _ in 0..4 {
            put(&mut s, 8, 0);
        }
        put(&mut s, 8, 1);
        put(&mut s, 8, 0x1E);
        put(&mut s, 8, u32::from(config));
        put(&mut s, 8, 0);
        // Everything after the announcement is zero; clear the reserved bits.
        let mut rp = 17 * M - 1;
        while rp < M * BLOCK {
            let (idx, b) = bit_location(rp, M, false);
            set_bit(&mut s, idx, b, 0);
            rp += SYNC_SAMPLES * M;
        }
        let crc = block_crc(&s);
        for i in 0..16 {
            set_bit(&mut s, i, 1, u32::from((crc >> (15 - i)) & 1));
        }
        s
    }

    fn frame(channels: &[&[i32]], speakers: &[usize]) -> RDecodedFrame {
        let n = channels[0].len();
        let mut pcm = RVec::with_capacity(n * channels.len());
        for s in 0..n {
            for c in channels {
                pcm.push(c[s]);
            }
        }
        RDecodedFrame {
            sampling_frequency: 48_000,
            sample_count: n as u32,
            channel_count: channels.len() as u32,
            pcm,
            channel_labels: speakers
                .iter()
                .map(|&s| crate::dts_pipeline::speaker_to_label(s))
                .collect(),
            metadata: RVec::new(),
            drc_gain: 1.0,
            drc_ramp_duration: 0,
            dialogue_level: None.into(),
            is_new_segment: false,
        }
    }

    #[test]
    fn a_carrier_announcing_a_height_plays_it_on_the_height_channel() {
        // Carrier 4.0 (config 20: 4.0_4H): DCA speakers L=1, R=2, Ls=3, Rs=4.
        let speakers = [1usize, 2, 3, 4];
        let blocks = 6;
        let mut carrier: Vec<Vec<i32>> = Vec::new();
        for (c, &id) in [9u8, 1, 4, 5].iter().enumerate() {
            let mut ch = Vec::new();
            for b in 0..blocks {
                let audio: Vec<i32> = (0..BLOCK)
                    .map(|i| (((i + b * BLOCK) as i32 * 37 + c as i32 * 1000) % 20000 - 10000) * 8)
                    .collect();
                ch.extend(block(&audio, 20, id));
            }
            carrier.push(ch);
        }
        let total = blocks * BLOCK;
        let mut state = DtsAuroState::default();
        let mut out = RVec::new();
        let step = 512;
        for start in (0..total).step_by(step) {
            let chans: Vec<&[i32]> = carrier.iter().map(|c| &c[start..start + step]).collect();
            let f = frame(&chans, &speakers);
            let lossless = speakers.iter().copied().zip(chans.iter().copied());
            state.route(f, &speakers, lossless, &mut out);
        }
        assert!(state.is_unfolding());
        let emitted: usize = out.iter().map(|f| f.sample_count as usize).sum();
        // Everything but one block of latency is out, and none of it as the
        // carrier: every frame has the eight unfolded channels.
        assert_eq!(emitted, total - BLOCK);
        assert!(out.iter().all(|f| f.channel_count == 8));
        let labels = &out[0].channel_labels;
        assert_eq!(
            labels.as_slice(),
            &[
                RChannelLabel::L,
                RChannelLabel::R,
                RChannelLabel::Ls,
                RChannelLabel::Rs,
                RChannelLabel::Tfl,
                RChannelLabel::Tfr,
                RChannelLabel::Tbl,
                RChannelLabel::Tbr,
            ]
        );
        // Sample 100: the L carrier announced HL, so L is silent and Tfl
        // carries the carrier with its borrowed bits cleared; R plays as R.
        let f = &out[0];
        let row = |s: usize| &f.pcm[s * 8..(s + 1) * 8];
        let s = 100;
        assert_eq!(row(s)[0], 0, "L is not part of the fold");
        assert_eq!(
            row(s)[4],
            carrier[0][s] & !0b111,
            "HL comes off the L carrier"
        );
        assert_eq!(row(s)[1], carrier[1][s] & !0b111, "R plays as R");
        assert_eq!(row(s)[5], 0, "HR was never announced");
    }

    #[test]
    fn plain_pcm_is_released_unchanged_once_the_stage_gives_up() {
        let speakers = [1usize, 2];
        let mut state = DtsAuroState::default();
        let mut out = RVec::new();
        let n = 512;
        let mut pushed = 0usize;
        while pushed < 3 * auro::block::MAX_BLOCK {
            let l: Vec<i32> = (0..n).map(|i| (pushed + i) as i32 * 3).collect();
            let r: Vec<i32> = (0..n).map(|i| -((pushed + i) as i32) * 3).collect();
            let chans: Vec<&[i32]> = vec![&l, &r];
            let f = frame(&chans, &speakers);
            let lossless = speakers.iter().copied().zip(chans.iter().copied());
            state.route(f, &speakers, lossless, &mut out);
            pushed += n;
        }
        assert!(!state.is_unfolding());
        let emitted: usize = out.iter().map(|f| f.sample_count as usize).sum();
        assert_eq!(emitted, pushed, "every held frame came out");
        assert!(out.iter().all(|f| f.channel_count == 2));
        assert_eq!(out[1].pcm[0], 512 * 3, "frames came out in order");
    }
}
