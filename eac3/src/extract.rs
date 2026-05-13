use crate::parser::{FrameInfo, ParseError, SYNCWORD, parse_header};

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
            if self.buffered_len() < 2 {
                return Ok(None);
            }

            let Some(sync_offset) = find_syncword(self.buffered()) else {
                let keep = usize::from(self.buffered().last() == Some(&((SYNCWORD >> 8) as u8)));
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
                Err(err) => {
                    self.consume_front(2);
                    if matches!(err, ParseError::InvalidSyncword) {
                        continue;
                    }
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
    data.windows(2)
        .position(|bytes| bytes == [(SYNCWORD >> 8) as u8, SYNCWORD as u8])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Vec<u8> {
        let mut frame = vec![0x0B, 0x77, 0x00, 0x07, 0x05, 0x80, 0x00];
        frame.resize(16, 0xAA);
        frame
    }

    #[test]
    fn extracts_complete_frame_after_garbage() {
        let frame = frame();
        let mut extractor = Extractor::default();

        extractor.push_bytes(&[0x00, 0x01, 0x02]);
        extractor.push_bytes(&frame);

        let extracted = extractor.next_frame().unwrap().unwrap();
        assert_eq!(extracted.as_bytes(), frame.as_slice());
        assert_eq!(extracted.info().frame_size, 16);
        assert_eq!(extractor.next_frame().unwrap(), None);
    }

    #[test]
    fn waits_for_complete_frame() {
        let frame = frame();
        let mut extractor = Extractor::default();

        extractor.push_bytes(&frame[..8]);
        assert_eq!(extractor.next_frame().unwrap(), None);
        extractor.push_bytes(&frame[8..]);

        assert_eq!(
            extractor.next_frame().unwrap().unwrap().as_bytes(),
            frame.as_slice()
        );
    }
}
