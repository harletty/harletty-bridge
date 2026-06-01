// SPDX-License-Identifier: Apache-2.0
//
// Streaming DCA core syncframe extractor. Mirrors `eac3/src/extract.rs` but
// frames on the 4-byte big-endian core syncword `0x7FFE8001` and uses the core
// header `frame_size` for boundaries.

use crate::parser::{FrameInfo, ParseError, SYNCWORD_CORE_BE, parse_header};

const SYNC: [u8; 4] = SYNCWORD_CORE_BE.to_be_bytes();

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtractError {
    InvalidHeader(ParseError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    data: Vec<u8>,
    info: FrameInfo,
}

impl Frame {
    #[inline]
    pub fn new(data: Vec<u8>, info: FrameInfo) -> Self {
        Self { data, info }
    }

    #[inline]
    pub fn info(&self) -> FrameInfo {
        self.info
    }

    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }
}

impl AsRef<[u8]> for Frame {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

#[derive(Debug)]
pub struct Extractor {
    buffer: Vec<u8>,
    cursor: usize,
}

impl Default for Extractor {
    fn default() -> Self {
        Self {
            buffer: Vec::with_capacity(8192),
            cursor: 0,
        }
    }
}

impl Extractor {
    #[inline]
    pub fn push_bytes(&mut self, data: &[u8]) {
        self.compact_if_needed(data.len());
        self.buffer.extend_from_slice(data);
    }

    pub fn next_frame(&mut self) -> Result<Option<Frame>, ExtractError> {
        loop {
            if self.buffered_len() < SYNC.len() {
                return Ok(None);
            }

            let Some(sync_offset) = find_syncword(self.buffered()) else {
                // Keep the trailing bytes that might be a partial syncword.
                let keep = partial_sync_tail(self.buffered());
                let consume = self.buffered_len().saturating_sub(keep);
                self.consume_front(consume);
                return Ok(None);
            };
            self.consume_front(sync_offset);

            let header = self.buffered();
            match parse_header(header) {
                Ok(info) => {
                    if header.len() < info.frame_size {
                        return Ok(None);
                    }
                    let data = header[..info.frame_size].to_vec();
                    self.consume_front(info.frame_size);
                    return Ok(Some(Frame::new(data, info)));
                }
                Err(ParseError::InsufficientData) => return Ok(None),
                Err(ParseError::InvalidSyncword) => {
                    self.consume_front(SYNC.len());
                    continue;
                }
                Err(err) => {
                    // Resync past this candidate.
                    self.consume_front(SYNC.len());
                    return Err(ExtractError::InvalidHeader(err));
                }
            }
        }
    }

    #[inline]
    pub fn buffered_len(&self) -> usize {
        self.buffer.len().saturating_sub(self.cursor)
    }

    #[inline]
    fn buffered(&self) -> &[u8] {
        &self.buffer[self.cursor..]
    }

    #[inline]
    fn consume_front(&mut self, count: usize) {
        self.cursor = self.cursor.saturating_add(count);
        self.compact_if_needed(0);
    }

    fn compact_if_needed(&mut self, incoming_len: usize) {
        if self.cursor == 0 {
            return;
        }
        if self.cursor == self.buffer.len() {
            self.buffer.clear();
            self.cursor = 0;
            return;
        }
        let needs_room = self.buffer.len() + incoming_len > self.buffer.capacity();
        let cursor_is_large = self.cursor >= 4096;
        let mostly_consumed = self.cursor * 2 >= self.buffer.len();
        if !(needs_room || cursor_is_large || mostly_consumed) {
            return;
        }
        self.buffer.copy_within(self.cursor.., 0);
        self.buffer.truncate(self.buffer.len() - self.cursor);
        self.cursor = 0;
    }
}

impl Iterator for Extractor {
    type Item = Result<Frame, ExtractError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_frame().transpose()
    }
}

fn find_syncword(data: &[u8]) -> Option<usize> {
    data.windows(SYNC.len()).position(|w| w == SYNC)
}

/// Length of a trailing run that is a strict prefix of the syncword, so we keep
/// it across `push_bytes` boundaries instead of discarding a split syncword.
fn partial_sync_tail(data: &[u8]) -> usize {
    let max = SYNC.len() - 1;
    for keep in (1..=max).rev() {
        if data.len() >= keep && data[data.len() - keep..] == SYNC[..keep] {
            return keep;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a minimal valid-ish core header: sync + fields that pass parse.
    // deficit_samples=32 (code 31), npcmblocks=8 (code 7), frame_size=128
    // (code 127), audio_mode=9, sr_code=13 (48k), pcmr_code=5 (24-bit).
    fn core_frame(frame_size: usize) -> Vec<u8> {
        use crate::bitstream_writer::BitWriter;
        let mut w = BitWriter::new();
        w.write(32, SYNCWORD_CORE_BE);
        w.write(1, 0); // normal_frame
        w.write(5, 31); // deficit_samples-1 -> 32
        w.write(1, 0); // crc_present
        w.write(7, 7); // npcmblocks-1 -> 8
        w.write(14, (frame_size - 1) as u32); // frame_size
        w.write(6, 9); // audio_mode 3F2R
        w.write(4, 13); // sr_code -> 48000
        w.write(5, 0); // br_code
        w.write(1, 0); // reserved
        w.write(1, 0); // drc
        w.write(1, 0); // ts
        w.write(1, 0); // aux
        w.write(1, 0); // hdcd
        w.write(3, 0); // ext_audio_type
        w.write(1, 0); // ext_audio_present
        w.write(1, 0); // sync_ssf
        w.write(2, 1); // lfe_present
        w.write(1, 1); // predictor_history
        w.write(1, 1); // filter_perfect
        w.write(4, 0); // encoder_rev
        w.write(2, 0); // copy_hist
        w.write(3, 5); // pcmr_code -> 24-bit
        w.write(1, 0); // sumdiff_front
        w.write(1, 0); // sumdiff_surround
        w.write(4, 0); // dn_code
        let mut bytes = w.finish();
        bytes.resize(frame_size, 0xAA);
        bytes
    }

    #[test]
    fn extracts_after_garbage() {
        let frame = core_frame(128);
        let mut ex = Extractor::default();
        ex.push_bytes(&[0x00, 0x11, 0x22]);
        ex.push_bytes(&frame);
        let got = ex.next_frame().unwrap().unwrap();
        assert_eq!(got.as_bytes(), frame.as_slice());
        assert_eq!(got.info().frame_size, 128);
        assert_eq!(got.info().sample_rate, 48_000);
        assert_eq!(got.info().samples_per_channel(), 256);
        assert!(got.info().has_lfe());
        assert_eq!(ex.next_frame().unwrap(), None);
    }

    #[test]
    fn waits_for_full_frame() {
        let frame = core_frame(128);
        let mut ex = Extractor::default();
        ex.push_bytes(&frame[..40]);
        assert_eq!(ex.next_frame().unwrap(), None);
        ex.push_bytes(&frame[40..]);
        assert_eq!(ex.next_frame().unwrap().unwrap().as_bytes(), frame.as_slice());
    }
}
