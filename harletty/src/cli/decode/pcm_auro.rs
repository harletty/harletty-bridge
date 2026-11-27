//! Auro-3D in PCM input: a WAV carrying the decoded carrier.
//!
//! The DTS path hands the Auro code the lossless channels of a DTS-HD MA
//! track; here they arrive as the samples of a WAV file, which a front end
//! writes from any lossless source it decoded bit-exactly. The carrier is
//! mapped onto the same bed streams, detected by the same detector and
//! unfolded by the same unfolder, and what comes out is the same
//! [`DtsFrameMessage`]s, so one handler writes the master set for both.
//!
//! Whether the PCM is a carrier is known a few blocks in, as for DTS:
//! the samples decoded so far are held until the detector latches (the
//! unfolding then starts from the first of them) or the same limit as the
//! DTS stage's is passed (this is not an Auro-3D stream).

use super::auro_stage::HOLD_LIMIT;
use super::dts_handler::{AuroFrame, DtsFrameMessage};
use crate::pcm::{PcmFormat, position};
use anyhow::{Result, anyhow, bail};
use auro::{Detection, Detector, StreamId, Unfolder};

/// What `decode` says of a PCM input that is not an Auro-3D carrier.
pub const NO_AURO: &str = "no Auro-3D found in the PCM input";

/// The bed stream a speaker position of a WAV channel mask plays as.
///
/// A 7.1 carrier's side pair is the Auro bed's surround pair and its back
/// pair the rear one (what the DTS path makes of a 7.1 carrier's Lss/Rss and
/// Lsr/Rsr); a 5.1 one has a single pair, which is the surround pair
/// whether the file calls it back or side. `sides` says whether the file
/// has both.
fn bed_stream(bit: u32, sides: bool) -> Option<StreamId> {
    Some(StreamId(match bit {
        position::FRONT_LEFT => 0,
        position::FRONT_RIGHT => 1,
        position::FRONT_CENTER => 2,
        position::LOW_FREQUENCY => 3,
        position::SIDE_LEFT => 4,
        position::SIDE_RIGHT => 5,
        position::BACK_LEFT if sides => 7,
        position::BACK_RIGHT if sides => 8,
        position::BACK_LEFT => 4,
        position::BACK_RIGHT => 5,
        position::BACK_CENTER => 6,
        _ => return None,
    }))
}

/// The channels of a PCM input a carrier can be made of, as integers.
pub struct Carrier {
    format: PcmFormat,
    /// File channel of each carrier channel, in file order, and the bed
    /// stream it plays as.
    channels: Vec<(usize, StreamId)>,
    /// The channels of the last chunk, one `Vec` per carrier channel.
    columns: Vec<Vec<i32>>,
}

impl Carrier {
    /// The carrier an input would be, or why it cannot be one.
    pub fn new(format: PcmFormat) -> Result<Self, String> {
        if format.bits != 24 {
            return Err(format!(
                "Auro-3D is carried in 24-bit PCM, this is {}-bit",
                format.bits
            ));
        }
        let Some(positions) = format.positions() else {
            return Err(format!(
                "{} channels and no channel mask: the channel order is unknown",
                format.channels
            ));
        };
        let sides = positions.contains(&position::SIDE_LEFT);
        let channels: Vec<(usize, StreamId)> = positions
            .iter()
            .enumerate()
            .filter_map(|(channel, &bit)| bed_stream(bit, sides).map(|id| (channel, id)))
            .collect();
        if channels.len() < 2 {
            return Err("no bed channels in the channel mask".to_string());
        }
        Ok(Self {
            format,
            columns: vec![Vec::new(); channels.len()],
            channels,
        })
    }

    /// Channels of the carrier, the LFE included.
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Take the carrier's channels out of `frames` interleaved sample frames
    /// of 24-bit PCM. They are then `self.columns()`.
    pub fn split(&mut self, bytes: &[u8]) {
        let frame_bytes = self.format.frame_bytes();
        for column in &mut self.columns {
            column.clear();
        }
        for frame in bytes.chunks_exact(frame_bytes) {
            for (column, &(channel, _)) in self.columns.iter_mut().zip(&self.channels) {
                let at = channel * 3;
                // 24 bits, sign-extended: the integers the side channel
                // was written into.
                column.push(i32::from_le_bytes([0, frame[at], frame[at + 1], frame[at + 2]]) >> 8);
            }
        }
    }

    pub fn columns(&self) -> &[Vec<i32>] {
        &self.columns
    }

    /// A detector for the carrier's channels.
    pub fn detector(&self) -> Detector {
        Detector::new(self.channels.len())
    }

    fn stream_ids(&self) -> Vec<StreamId> {
        self.channels.iter().map(|&(_, id)| id).collect()
    }
}

enum Phase {
    /// Detecting; the chunks not yet unfolded are held.
    Undecided {
        detector: Detector,
        held: Vec<Vec<Vec<i32>>>,
        held_samples: usize,
    },
    /// Confirmed: everything flows through the unfolder.
    Unfolding {
        unfolder: Unfolder,
        outputs: Vec<StreamId>,
        scratch: Vec<i32>,
    },
}

/// Detection and unfolding of a PCM input, chunk by chunk.
pub struct PcmAuroStage {
    carrier: Carrier,
    phase: Phase,
}

impl PcmAuroStage {
    pub fn new(carrier: Carrier) -> Self {
        let detector = carrier.detector();
        Self {
            carrier,
            phase: Phase::Undecided {
                detector,
                held: Vec::new(),
                held_samples: 0,
            },
        }
    }

    /// The next sample frames of the input, at most [`auro::unfold::MAX_PUSH`]
    /// of them. Sends whatever can go out now; fails when the input is
    /// found not to be a carrier.
    pub fn push(
        &mut self,
        bytes: &[u8],
        sink: &mut dyn FnMut(DtsFrameMessage) -> Result<()>,
    ) -> Result<()> {
        self.carrier.split(bytes);
        let sample_rate = self.carrier.format.sample_rate;
        match &mut self.phase {
            Phase::Unfolding {
                unfolder,
                outputs,
                scratch,
            } => {
                for (index, column) in self.carrier.columns.iter().enumerate() {
                    unfolder.push(index, column);
                }
                drain(unfolder, outputs, sample_rate, scratch, sink)
            }
            Phase::Undecided {
                detector,
                held,
                held_samples,
            } => {
                let mut latched = None;
                for (index, column) in self.carrier.columns.iter().enumerate() {
                    if let Some(detection) = detector.push(index, column) {
                        latched = Some(detection);
                    }
                }
                *held_samples += self.carrier.columns[0].len();
                held.push(self.carrier.columns.clone());
                if let Some(detection) = latched {
                    sink(DtsFrameMessage::Auro(detection))?;
                    self.start_unfolding(detection, sink)
                } else if *held_samples > HOLD_LIMIT {
                    bail!(NO_AURO)
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Input is over: release what the latency held back.
    pub fn finish(&mut self, sink: &mut dyn FnMut(DtsFrameMessage) -> Result<()>) -> Result<()> {
        let sample_rate = self.carrier.format.sample_rate;
        match &mut self.phase {
            Phase::Unfolding {
                unfolder,
                outputs,
                scratch,
            } => {
                unfolder.finish();
                drain(unfolder, outputs, sample_rate, scratch, sink)?;
                log::info!(
                    "Auro-3D unfold complete: {} blocks failed to decode",
                    unfolder.decode_errors()
                );
                Ok(())
            }
            Phase::Undecided { .. } => bail!(NO_AURO),
        }
    }

    fn start_unfolding(
        &mut self,
        detection: Detection,
        sink: &mut dyn FnMut(DtsFrameMessage) -> Result<()>,
    ) -> Result<()> {
        let Phase::Undecided { held, .. } = &mut self.phase else {
            return Ok(());
        };
        let held = std::mem::take(held);
        let outputs: Vec<StreamId> = match detection.original.streams() {
            Some(streams) => streams.as_slice().iter().map(|&id| StreamId(id)).collect(),
            None => {
                return Err(anyhow!(
                    "Auro-3D layout {:?} has no known stream list",
                    detection.original
                ));
            }
        };
        let sample_rate = self.carrier.format.sample_rate;
        let mut unfolder = Unfolder::new(&self.carrier.stream_ids(), &outputs);
        unfolder.set_latency(usize::from(detection.block_size));
        let mut scratch = Vec::new();
        // Replay what was held.
        for chunk in &held {
            for (index, column) in chunk.iter().enumerate() {
                unfolder.push(index, column);
            }
            drain(&mut unfolder, &outputs, sample_rate, &mut scratch, sink)?;
        }
        self.phase = Phase::Unfolding {
            unfolder,
            outputs,
            scratch,
        };
        Ok(())
    }
}

fn drain(
    unfolder: &mut Unfolder,
    outputs: &[StreamId],
    sample_rate: u32,
    scratch: &mut Vec<i32>,
    sink: &mut dyn FnMut(DtsFrameMessage) -> Result<()>,
) -> Result<()> {
    let ready = unfolder.ready();
    if ready == 0 {
        return Ok(());
    }
    scratch.clear();
    scratch.resize(ready * outputs.len(), 0);
    let frames = unfolder.take(outputs, scratch);
    scratch.truncate(frames * outputs.len());
    sink(DtsFrameMessage::AuroFrame(Box::new(AuroFrame {
        sample_rate,
        streams: outputs.to_vec(),
        samples: std::mem::take(scratch),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(channels: u16, bits: u16, mask: Option<u32>) -> PcmFormat {
        PcmFormat {
            channels,
            sample_rate: 48_000,
            bits,
            channel_mask: mask,
        }
    }

    fn ids(carrier: &Carrier) -> Vec<u8> {
        carrier.channels.iter().map(|&(_, id)| id.0).collect()
    }

    #[test]
    fn a_five_one_is_the_bed_whichever_pair_it_calls_its_surrounds() {
        // FL FR FC LFE BL BR, and FL FR FC LFE SL SR.
        for mask in [0x3F, 0x60F] {
            let carrier = Carrier::new(format(6, 24, Some(mask))).unwrap();
            assert_eq!(ids(&carrier), [0, 1, 2, 3, 4, 5], "mask {mask:#x}");
        }
        let default = Carrier::new(format(6, 24, None)).unwrap();
        assert_eq!(ids(&default), [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_seven_one_has_its_sides_as_surrounds_and_its_backs_as_rears() {
        // FL FR FC LFE BL BR SL SR: the WAV order of the default 7.1.
        let carrier = Carrier::new(format(8, 24, None)).unwrap();
        assert_eq!(ids(&carrier), [0, 1, 2, 3, 7, 8, 4, 5]);
        let carrier = Carrier::new(format(8, 24, Some(0x63F))).unwrap();
        assert_eq!(ids(&carrier), [0, 1, 2, 3, 7, 8, 4, 5]);
    }

    #[test]
    fn channels_with_no_bed_stream_are_left_out() {
        // 5.1.2: FL FR FC LFE BL BR TFL TFR.
        let carrier = Carrier::new(format(8, 24, Some(0x3F | 1 << 12 | 1 << 14))).unwrap();
        assert_eq!(ids(&carrier), [0, 1, 2, 3, 4, 5]);
        assert_eq!(
            carrier.channels.iter().map(|c| c.0).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn only_24_bit_pcm_with_a_known_order_can_be_a_carrier() {
        assert!(Carrier::new(format(6, 16, None)).is_err());
        assert!(Carrier::new(format(6, 32, None)).is_err());
        assert!(Carrier::new(format(2, 24, None)).is_err());
        assert!(Carrier::new(format(4, 24, None)).is_err());
    }

    #[test]
    fn split_reads_signed_24_bit_samples_of_the_carrier_channels() {
        let mut carrier = Carrier::new(format(6, 24, None)).unwrap();
        let mut bytes = Vec::new();
        for frame in 0..3i32 {
            for channel in 0..6i32 {
                let value = (channel + 1) * 1000 * if frame == 1 { -1 } else { 1 } + frame;
                bytes.extend(&value.to_le_bytes()[..3]);
            }
        }
        carrier.split(&bytes);
        assert_eq!(carrier.columns()[0], [1000, -1000 + 1, 1000 + 2]);
        assert_eq!(carrier.columns()[5], [6000, -6000 + 1, 6000 + 2]);
    }
}
