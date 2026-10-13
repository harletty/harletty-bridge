// SPDX-License-Identifier: Apache-2.0
//
// A carrier channel decoded block by block: detection, stream layer,
// residuals, unmix. Storage is allocated once per channel; decoding a block
// allocates nothing.

use crate::block::{BlockInfo, MAX_BLOCK, SyncHeader};
use crate::detect::{ChannelDetector, ChannelStats};
use crate::rice::{Codebook, RiceError, decode_residuals};
use crate::stream::{MAX_STREAMS, Payload, StreamBlock, StreamError, parse_stream};
use crate::unmix::{GainTable, unmix};

/// Stream identities the fold refers to. The numbering is the encoder's;
/// the names come from where each stream showed up on channel
/// identification material.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StreamId(pub u8);

impl StreamId {
    pub fn name(self) -> &'static str {
        match self.0 {
            0 => "L",
            1 => "R",
            2 => "C",
            3 => "LFE",
            4 => "Ls",
            5 => "Rs",
            6 => "Cs",
            7 => "Lb",
            8 => "Rb",
            9 => "HL",
            10 => "HR",
            11 => "HC",
            12 => "T",
            13 => "HLs",
            14 => "HRs",
            _ => "?",
        }
    }

    /// Height layer (and the top) versus the bed.
    pub fn is_height(self) -> bool {
        (9..=14).contains(&self.0)
    }
}

/// Why one block was not decoded. The carrier still plays as authored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Stream(StreamError),
    Rice(RiceError),
    /// A gain index outside the table.
    Gain,
}

/// One decoded block of one carrier.
pub struct Decoded<'a> {
    /// Absolute index of the block's first sample in this channel.
    pub start: u64,
    pub header: SyncHeader,
    pub info: BlockInfo,
    pub stream: StreamBlock,
    /// The restored streams, `stream.mode` of them, each `block_size` long.
    pub outputs: [(StreamId, &'a [i32]); MAX_STREAMS],
}

/// Decoder for one carrier channel.
pub struct ChannelDecoder {
    detector: ChannelDetector,
    scratch: Box<[i32]>,
    payload: Box<Payload>,
    codebook: Box<Codebook>,
    residuals: Box<[i32]>,
    outputs: [Box<[i32]>; MAX_STREAMS],
    gains: GainTable,
    /// Blocks that validated but did not decode, by cause, most recent.
    pub last_error: Option<DecodeError>,
    pub decode_errors: u64,
}

impl ChannelDecoder {
    pub fn new() -> Self {
        Self {
            detector: ChannelDetector::new(),
            scratch: vec![0; MAX_BLOCK].into_boxed_slice(),
            payload: Box::new(Payload::new()),
            codebook: Box::new(Codebook::new()),
            residuals: vec![0; 2 * MAX_BLOCK].into_boxed_slice(),
            outputs: [
                vec![0; MAX_BLOCK].into_boxed_slice(),
                vec![0; MAX_BLOCK].into_boxed_slice(),
                vec![0; MAX_BLOCK].into_boxed_slice(),
            ],
            gains: GainTable::new(),
            last_error: None,
            decode_errors: 0,
        }
    }

    pub fn stats(&self) -> ChannelStats {
        self.detector.stats()
    }

    /// Samples pushed so far.
    pub fn position(&self) -> u64 {
        self.detector.count()
    }

    pub fn reset(&mut self) {
        self.detector.reset();
        self.last_error = None;
        self.decode_errors = 0;
    }

    /// Push samples; every block that validates and decodes is handed to
    /// `on_block`.
    pub fn push(&mut self, samples: &[i32], mut on_block: impl FnMut(Decoded<'_>)) {
        let Self {
            detector,
            scratch,
            payload,
            codebook,
            residuals,
            outputs,
            gains,
            last_error,
            decode_errors,
        } = self;
        detector.push(
            samples,
            scratch,
            |start, block, header, info| match decode_block(
                block, header, payload, codebook, residuals, outputs, gains,
            ) {
                Ok(stream) => {
                    let n = usize::from(header.block_size);
                    let ids = stream.ids;
                    let outs: [(StreamId, &[i32]); MAX_STREAMS] = [
                        (StreamId(ids[0]), &outputs[0][..n]),
                        (StreamId(ids[1]), &outputs[1][..n]),
                        (StreamId(ids[2]), &outputs[2][..n]),
                    ];
                    on_block(Decoded {
                        start,
                        header,
                        info,
                        stream,
                        outputs: outs,
                    });
                }
                Err(e) => {
                    *last_error = Some(e);
                    *decode_errors += 1;
                }
            },
        );
    }
}

impl Default for ChannelDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Decode one validated block into `outputs`.
pub fn decode_block(
    block: &[i32],
    header: SyncHeader,
    payload: &mut Payload,
    codebook: &mut Codebook,
    residuals: &mut [i32],
    outputs: &mut [Box<[i32]>; MAX_STREAMS],
    gains: &GainTable,
) -> Result<StreamBlock, DecodeError> {
    let n = usize::from(header.block_size);
    let block = &block[..n];
    payload.gather(block, header);
    let stream = parse_stream(payload).map_err(DecodeError::Stream)?;
    if stream.mode >= 2 {
        codebook.load(payload, &stream);
        decode_residuals(payload, &stream, codebook, n, residuals).map_err(DecodeError::Rice)?;
    }
    let [o0, o1, o2] = outputs;
    let mut outs: [&mut [i32]; MAX_STREAMS] = [&mut o0[..n], &mut o1[..n], &mut o2[..n]];
    if !unmix(&stream, gains, header.lsb_bits, block, residuals, &mut outs) {
        return Err(DecodeError::Gain);
    }
    Ok(stream)
}
