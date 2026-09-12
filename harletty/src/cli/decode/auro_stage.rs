//! The Auro-3D stage of the DTS decode thread.
//!
//! A lossless DTS-HD track may be an Auro-Codec carrier: the low bits of
//! its PCM then hold the fold of a larger layout. Whether it is one is only
//! known a few blocks in, and unfolding lags input by a block, so the
//! frames decoded so far are held back until the question is settled. If
//! the carrier is confirmed, the held frames are replayed into the
//! unfolder and the output switches to unfolded frames; if it is not, they
//! go out as they are and the stage steps aside.

use super::dts_handler::{AuroFrame, DtsFrameMessage};
use anyhow::Result;
use auro::{Detector, StreamId, Unfolder};
use dca::HdFrame;
use std::collections::VecDeque;
use std::sync::mpsc;

/// Samples to hold back while waiting for a verdict: three of the largest
/// blocks, which is what the detector needs to latch.
const HOLD_LIMIT: usize = 3 * auro::block::MAX_BLOCK + 4096;

const PCM_SCALE: f32 = 8_388_608.0;

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

enum Phase {
    /// Detecting; `held` accumulates the frames not yet sent.
    Undecided {
        detector: Detector,
        held: VecDeque<HeldFrame>,
        held_samples: usize,
    },
    /// Confirmed: everything flows through the unfolder.
    Unfolding {
        unfolder: Unfolder,
        /// DCA speaker index of each carrier channel, in unfolder order.
        speakers: Vec<usize>,
        outputs: Vec<StreamId>,
        sample_rate: u32,
        scratch: Vec<i32>,
    },
    /// Not a carrier (or given up): frames pass straight through.
    Plain,
}

struct HeldFrame {
    frame: Box<HdFrame>,
    presentation: Option<dca::XPresentation>,
    metadata: Option<dca::XMetadata>,
}

pub struct AuroStage {
    phase: Phase,
}

impl AuroStage {
    pub fn new() -> Self {
        Self {
            phase: Phase::Undecided {
                detector: Detector::new(0),
                held: VecDeque::new(),
                held_samples: 0,
            },
        }
    }

    /// One decoded HD frame, with the decoder's integer output for it.
    /// Sends whatever can go out now.
    pub fn frame(
        &mut self,
        frame: Box<HdFrame>,
        presentation: Option<dca::XPresentation>,
        metadata: Option<dca::XMetadata>,
        lossless: impl Iterator<Item = (usize, Vec<i32>)>,
        tx: &mpsc::Sender<Result<DtsFrameMessage>>,
    ) {
        match &mut self.phase {
            Phase::Plain => {
                let _ = tx.send(Ok(DtsFrameMessage::Hd {
                    frame,
                    presentation,
                    metadata,
                }));
            }
            Phase::Unfolding {
                unfolder,
                speakers,
                outputs,
                sample_rate,
                scratch,
            } => {
                for (speaker, samples) in lossless {
                    if let Some(index) = speakers.iter().position(|&s| s == speaker) {
                        for chunk in samples.chunks(auro::unfold::MAX_PUSH) {
                            unfolder.push(index, chunk);
                        }
                    }
                }
                Self::drain(unfolder, outputs, *sample_rate, scratch, tx);
            }
            Phase::Undecided {
                detector,
                held,
                held_samples,
            } => {
                if detector.channel_count() == 0 {
                    *detector = Detector::new(frame.samples.len());
                }
                let n = frame.bed_sample_count();
                let mut latched = None;
                for (speaker, samples) in lossless {
                    if let Some(d) = detector.push(speaker, &samples) {
                        latched = Some(d);
                    }
                }
                held.push_back(HeldFrame {
                    frame,
                    presentation,
                    metadata,
                });
                *held_samples += n;
                if let Some(detection) = latched {
                    let _ = tx.send(Ok(DtsFrameMessage::Auro(detection)));
                    self.start_unfolding(detection, tx);
                } else if *held_samples > HOLD_LIMIT {
                    self.give_up(tx);
                }
            }
        }
    }

    /// A frame without lossless output (core only): not a carrier.
    pub fn not_a_carrier(&mut self, tx: &mpsc::Sender<Result<DtsFrameMessage>>) {
        if matches!(self.phase, Phase::Undecided { .. }) {
            self.give_up(tx);
        }
    }

    /// Input is over: release what the latency held back.
    pub fn finish(&mut self, tx: &mpsc::Sender<Result<DtsFrameMessage>>) {
        match &mut self.phase {
            Phase::Unfolding {
                unfolder,
                outputs,
                sample_rate,
                scratch,
                ..
            } => {
                unfolder.finish();
                Self::drain(unfolder, outputs, *sample_rate, scratch, tx);
                log::info!(
                    "Auro-3D unfold complete: {} blocks failed to decode",
                    unfolder.decode_errors()
                );
            }
            Phase::Undecided { .. } => self.give_up(tx),
            Phase::Plain => {}
        }
    }

    fn give_up(&mut self, tx: &mpsc::Sender<Result<DtsFrameMessage>>) {
        let phase = std::mem::replace(&mut self.phase, Phase::Plain);
        if let Phase::Undecided { held, .. } = phase {
            for h in held {
                let _ = tx.send(Ok(DtsFrameMessage::Hd {
                    frame: h.frame,
                    presentation: h.presentation,
                    metadata: h.metadata,
                }));
            }
        }
    }

    fn start_unfolding(
        &mut self,
        detection: auro::Detection,
        tx: &mpsc::Sender<Result<DtsFrameMessage>>,
    ) {
        let phase = std::mem::replace(&mut self.phase, Phase::Plain);
        let Phase::Undecided { held, .. } = phase else {
            return;
        };
        let Some(first) = held.front() else {
            return;
        };
        let speakers: Vec<usize> = (0..first.frame.samples.len())
            .filter(|&s| first.frame.samples[s].is_some())
            .collect();
        let carrier_ids: Vec<StreamId> = speakers.iter().map(|&s| speaker_stream(s)).collect();
        let outputs: Vec<StreamId> = match detection.original.streams() {
            Some(streams) => streams.as_slice().iter().map(|&id| StreamId(id)).collect(),
            None => {
                log::warn!(
                    "Auro-3D layout {:?} has no known stream list; keeping the carrier as is",
                    detection.original
                );
                self.give_up_with(held, tx);
                return;
            }
        };
        let sample_rate = first.frame.sample_rate;
        let mut unfolder = Unfolder::new(&carrier_ids, &outputs);
        let mut scratch = Vec::new();
        // Replay what was held: the floats are the 24-bit integers exactly.
        for h in held {
            for (index, &speaker) in speakers.iter().enumerate() {
                if let Some(channel) = h.frame.samples.get(speaker).and_then(|c| c.as_ref()) {
                    let ints: Vec<i32> = channel
                        .iter()
                        .map(|&v| (v * PCM_SCALE).round() as i32)
                        .collect();
                    for chunk in ints.chunks(auro::unfold::MAX_PUSH) {
                        unfolder.push(index, chunk);
                    }
                }
            }
            Self::drain(&mut unfolder, &outputs, sample_rate, &mut scratch, tx);
        }
        self.phase = Phase::Unfolding {
            unfolder,
            speakers,
            outputs,
            sample_rate,
            scratch,
        };
    }

    fn give_up_with(
        &mut self,
        held: VecDeque<HeldFrame>,
        tx: &mpsc::Sender<Result<DtsFrameMessage>>,
    ) {
        self.phase = Phase::Plain;
        for h in held {
            let _ = tx.send(Ok(DtsFrameMessage::Hd {
                frame: h.frame,
                presentation: h.presentation,
                metadata: h.metadata,
            }));
        }
    }

    fn drain(
        unfolder: &mut Unfolder,
        outputs: &[StreamId],
        sample_rate: u32,
        scratch: &mut Vec<i32>,
        tx: &mpsc::Sender<Result<DtsFrameMessage>>,
    ) {
        let ready = unfolder.ready();
        if ready == 0 {
            return;
        }
        scratch.clear();
        scratch.resize(ready * outputs.len(), 0);
        let frames = unfolder.take(outputs, scratch);
        scratch.truncate(frames * outputs.len());
        let _ = tx.send(Ok(DtsFrameMessage::AuroFrame(Box::new(AuroFrame {
            sample_rate,
            streams: outputs.to_vec(),
            samples: std::mem::take(scratch),
        }))));
    }
}

impl Default for AuroStage {
    fn default() -> Self {
        Self::new()
    }
}
