use log::warn;

const IEC61937_TRUEHD_DATA_TYPE: u8 = 0x16;

const MAT_START_CODE: &[u8; 20] = &[
    0x9E, 0x07, 0x03, 0x00, 0x01, 0x84, 0x01, 0x01, 0x00, 0x80, 0xA5, 0x56, 0xF4, 0x3B, 0x83, 0x81,
    0x80, 0x49, 0xE0, 0x77,
];
const MAT_MIDDLE_CODE: &[u8; 12] = &[
    0xC1, 0xC3, 0x49, 0x42, 0xFA, 0x3B, 0x83, 0x82, 0x80, 0x49, 0xE0, 0x77,
];
const MAT_END_CODE: &[u8; 16] = &[
    0xC2, 0xC3, 0xC4, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x11, 0x97,
];
const MAT_MIDDLE_POS: usize = 30_708;
const MAT_FRAME_SIZE: usize = 61_424;

#[derive(Debug)]
enum ParserState {
    WaitingForPayload,
    VerifyingMatStart {
        payload_size: usize,
    },
    ReadingPayload {
        bytes_remaining: usize,
        mat_position: usize,
        middle_code_skipped: bool,
        end_code_skipped: bool,
    },
}

pub struct MatStream {
    buffer: Vec<u8>,
    cursor: usize,
    state: ParserState,
    pending_chunk_bytes: Option<usize>,
    perf_stats: Option<MatPerfStats>,
}

impl Default for MatStream {
    fn default() -> Self {
        Self {
            buffer: Vec::with_capacity(64 * 1024),
            cursor: 0,
            state: ParserState::WaitingForPayload,
            pending_chunk_bytes: None,
            perf_stats: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MatPerfStats {
    pub swap_total: std::time::Duration,
    pub swap_max: std::time::Duration,
    pub swap_calls: u64,
    pub swap_bytes: u64,
    pub padding_total: std::time::Duration,
    pub padding_max: std::time::Duration,
    pub padding_calls: u64,
    pub padding_words: u64,
}

fn copy_swapped_words(src: &[u8]) -> Vec<u8> {
    let mut out = vec![0; src.len()];
    let even_len = src.len() & !1;
    let mut i = 0usize;
    while i < even_len {
        out[i] = src[i + 1];
        out[i + 1] = src[i];
        i += 2;
    }
    if src.len() != even_len {
        out[src.len() - 1] = src[src.len() - 1];
    }
    out
}

enum SkipCodeResult {
    Skipped,
    NotHere,
    NeedMoreData,
}

impl MatStream {
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.state = ParserState::WaitingForPayload;
        self.pending_chunk_bytes = None;
    }

    #[allow(dead_code)]
    pub fn enable_perf_stats(&mut self) {
        self.perf_stats = Some(MatPerfStats::default());
    }

    #[allow(dead_code)]
    pub fn perf_stats(&self) -> Option<MatPerfStats> {
        self.perf_stats
    }

    pub fn accepts_data_type(data_type: u8) -> bool {
        data_type == IEC61937_TRUEHD_DATA_TYPE
    }

    pub fn push_payload(&mut self, payload: &[u8]) {
        self.buffer.clear();
        self.buffer.extend_from_slice(payload);
        self.cursor = 0;
        self.state = ParserState::VerifyingMatStart {
            payload_size: payload.len(),
        };
    }

    fn remaining_buffer_len(&self) -> usize {
        self.buffer.len().saturating_sub(self.cursor)
    }

    fn advance(&mut self, len: usize) {
        self.cursor = self.cursor.saturating_add(len);
    }

    fn take_chunk(&mut self, len: usize) -> Vec<u8> {
        let start = self.cursor;
        let end = start + len;
        let swap_started = self.perf_stats.as_ref().map(|_| std::time::Instant::now());
        let out = copy_swapped_words(&self.buffer[start..end]);
        if let (Some(stats), Some(started)) = (&mut self.perf_stats, swap_started) {
            let elapsed = started.elapsed();
            stats.swap_total += elapsed;
            stats.swap_max = stats.swap_max.max(elapsed);
            stats.swap_calls += 1;
            stats.swap_bytes += len as u64;
        }
        self.cursor = end;
        out
    }

    fn strip_padding_words(
        &mut self,
        bytes_remaining: usize,
        mat_position: usize,
    ) -> Option<(usize, usize)> {
        if self.remaining_buffer_len() < 2
            || self.buffer[self.cursor] != 0x00
            || self.buffer[self.cursor + 1] != 0x00
        {
            return None;
        }

        let padding_started = self.perf_stats.as_ref().map(|_| std::time::Instant::now());
        let max_strip_bytes = bytes_remaining.min(self.remaining_buffer_len()) & !1;
        let start = self.cursor;
        let slice = &self.buffer[start..start + max_strip_bytes];
        let mut strip_bytes = 0usize;

        while strip_bytes + 1 < slice.len()
            && slice[strip_bytes] == 0x00
            && slice[strip_bytes + 1] == 0x00
        {
            strip_bytes += 2;
        }

        let stripped_words = (strip_bytes / 2) as u64;
        self.advance(strip_bytes);
        let bytes_rem = bytes_remaining.saturating_sub(strip_bytes);
        let mat_pos = mat_position + strip_bytes;

        if let (Some(stats), Some(started)) = (&mut self.perf_stats, padding_started) {
            let elapsed = started.elapsed();
            stats.padding_total += elapsed;
            stats.padding_max = stats.padding_max.max(elapsed);
            stats.padding_calls += 1;
            stats.padding_words += stripped_words;
        }

        Some((bytes_rem, mat_pos))
    }

    fn full_chunk_available(
        &self,
        chunk_size: usize,
        bytes_remaining: usize,
        mat_position: usize,
        middle_code_skipped: bool,
        end_code_skipped: bool,
    ) -> bool {
        if chunk_size > bytes_remaining || self.remaining_buffer_len() < chunk_size {
            return false;
        }

        if !middle_code_skipped
            && mat_position < MAT_MIDDLE_POS
            && mat_position + chunk_size > MAT_MIDDLE_POS
        {
            return false;
        }

        let mat_end_pos = MAT_FRAME_SIZE - MAT_END_CODE.len();
        if !end_code_skipped
            && mat_position < mat_end_pos
            && mat_position + chunk_size > mat_end_pos
        {
            return false;
        }

        true
    }

    fn try_skip_code(
        &mut self,
        mat_position: usize,
        code_position: usize,
        code: &[u8],
    ) -> SkipCodeResult {
        if mat_position != code_position {
            return SkipCodeResult::NotHere;
        }

        if self.remaining_buffer_len() < code.len() {
            return SkipCodeResult::NeedMoreData;
        }

        let start = self.cursor;
        let end = start + code.len();
        if self.buffer[start..end] != *code {
            return SkipCodeResult::NotHere;
        }

        self.cursor = end;
        SkipCodeResult::Skipped
    }

    pub fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
        'outer: loop {
            match self.state {
                ParserState::WaitingForPayload => return Ok(None),

                ParserState::VerifyingMatStart { payload_size } => {
                    if self.remaining_buffer_len() < MAT_START_CODE.len() {
                        return Ok(None);
                    }

                    let start = self.cursor;
                    let end = start + MAT_START_CODE.len();
                    if &self.buffer[start..end] != MAT_START_CODE {
                        self.buffer.clear();
                        self.cursor = 0;
                        self.state = ParserState::WaitingForPayload;
                        return Err("Invalid MAT start code in IEC 61937 payload".to_string());
                    }

                    self.advance(MAT_START_CODE.len());
                    self.state = ParserState::ReadingPayload {
                        bytes_remaining: payload_size.saturating_sub(MAT_START_CODE.len()),
                        mat_position: MAT_START_CODE.len(),
                        middle_code_skipped: false,
                        end_code_skipped: false,
                    };
                }

                ParserState::ReadingPayload {
                    bytes_remaining,
                    mat_position,
                    middle_code_skipped,
                    end_code_skipped,
                } => {
                    if bytes_remaining == 0 {
                        self.buffer.clear();
                        self.cursor = 0;
                        self.state = ParserState::WaitingForPayload;
                        continue;
                    }

                    if !middle_code_skipped {
                        match self.try_skip_code(mat_position, MAT_MIDDLE_POS, MAT_MIDDLE_CODE) {
                            SkipCodeResult::Skipped => {
                                let skipped = MAT_MIDDLE_CODE.len();
                                self.state = ParserState::ReadingPayload {
                                    bytes_remaining: bytes_remaining.saturating_sub(skipped),
                                    mat_position: mat_position + skipped,
                                    middle_code_skipped: true,
                                    end_code_skipped,
                                };
                                continue;
                            }
                            SkipCodeResult::NeedMoreData => return Ok(None),
                            SkipCodeResult::NotHere => {}
                        }
                    }

                    if !end_code_skipped {
                        let mat_end_pos = MAT_FRAME_SIZE - MAT_END_CODE.len();
                        match self.try_skip_code(mat_position, mat_end_pos, MAT_END_CODE) {
                            SkipCodeResult::Skipped => {
                                let skipped = MAT_END_CODE.len();
                                self.state = ParserState::ReadingPayload {
                                    bytes_remaining: bytes_remaining.saturating_sub(skipped),
                                    mat_position: mat_position + skipped,
                                    middle_code_skipped,
                                    end_code_skipped: true,
                                };
                                continue;
                            }
                            SkipCodeResult::NeedMoreData => return Ok(None),
                            SkipCodeResult::NotHere => {}
                        }
                    }

                    // A chunk split by a MAT middle/end code must resume as payload immediately.
                    // Do not run the leading-0x0000 padding stripper before handling a pending
                    // continuation, or valid zero words at the start of the resumed payload will
                    // be dropped and the chunk will be corrupted.
                    let continuing_chunk = self.pending_chunk_bytes.take();
                    if let Some(chunk_size) = continuing_chunk {
                        if self.full_chunk_available(
                            chunk_size,
                            bytes_remaining,
                            mat_position,
                            middle_code_skipped,
                            end_code_skipped,
                        ) {
                            let chunk = self.take_chunk(chunk_size);
                            let bytes_remaining_after = bytes_remaining - chunk_size;
                            self.state = if bytes_remaining_after == 0 {
                                ParserState::WaitingForPayload
                            } else {
                                ParserState::ReadingPayload {
                                    bytes_remaining: bytes_remaining_after,
                                    mat_position: mat_position + chunk_size,
                                    middle_code_skipped,
                                    end_code_skipped,
                                }
                            };
                            return Ok(Some(chunk));
                        }

                        let mut available_chunk = chunk_size.min(bytes_remaining);
                        if !middle_code_skipped && mat_position < MAT_MIDDLE_POS {
                            available_chunk = available_chunk.min(MAT_MIDDLE_POS - mat_position);
                        }
                        let mat_end_pos = MAT_FRAME_SIZE - MAT_END_CODE.len();
                        if !end_code_skipped && mat_position < mat_end_pos {
                            available_chunk = available_chunk.min(mat_end_pos - mat_position);
                        }

                        if self.remaining_buffer_len() < available_chunk {
                            self.pending_chunk_bytes = Some(chunk_size);
                            return Ok(None);
                        }

                        let chunk = self.take_chunk(available_chunk);
                        let bytes_remaining_after = bytes_remaining.saturating_sub(available_chunk);
                        let pending_chunk_bytes = chunk_size.saturating_sub(available_chunk);
                        self.pending_chunk_bytes =
                            (pending_chunk_bytes > 0).then_some(pending_chunk_bytes);
                        self.state = if bytes_remaining_after == 0 {
                            ParserState::WaitingForPayload
                        } else {
                            ParserState::ReadingPayload {
                                bytes_remaining: bytes_remaining_after,
                                mat_position: mat_position + available_chunk,
                                middle_code_skipped,
                                end_code_skipped,
                            }
                        };
                        return Ok(Some(chunk));
                    }

                    if let Some((bytes_rem, mat_pos)) =
                        self.strip_padding_words(bytes_remaining, mat_position)
                    {
                        if bytes_rem == 0 {
                            self.buffer.clear();
                            self.cursor = 0;
                            self.state = ParserState::WaitingForPayload;
                            continue 'outer;
                        }
                        self.state = ParserState::ReadingPayload {
                            bytes_remaining: bytes_rem,
                            mat_position: mat_pos,
                            middle_code_skipped,
                            end_code_skipped,
                        };
                        continue;
                    }

                    if self.remaining_buffer_len() < 2 {
                        return Ok(None);
                    }

                    let raw = u16::from_le_bytes([
                        self.buffer[self.cursor],
                        self.buffer[self.cursor + 1],
                    ]);
                    let chunk_size = ((raw & 0x0FFF) << 1) as usize;
                    if chunk_size == 0 {
                        warn!("Invalid MAT chunk size (0), skipping 2 bytes");
                        self.advance(2);
                        self.state = ParserState::ReadingPayload {
                            bytes_remaining: bytes_remaining.saturating_sub(2),
                            mat_position: mat_position + 2,
                            middle_code_skipped,
                            end_code_skipped,
                        };
                        continue;
                    }

                    if self.full_chunk_available(
                        chunk_size,
                        bytes_remaining,
                        mat_position,
                        middle_code_skipped,
                        end_code_skipped,
                    ) {
                        let chunk = self.take_chunk(chunk_size);
                        let bytes_remaining_after = bytes_remaining - chunk_size;
                        self.state = if bytes_remaining_after == 0 {
                            ParserState::WaitingForPayload
                        } else {
                            ParserState::ReadingPayload {
                                bytes_remaining: bytes_remaining_after,
                                mat_position: mat_position + chunk_size,
                                middle_code_skipped,
                                end_code_skipped,
                            }
                        };
                        return Ok(Some(chunk));
                    }

                    let mut available_chunk = chunk_size.min(bytes_remaining);
                    if !middle_code_skipped && mat_position < MAT_MIDDLE_POS {
                        available_chunk = available_chunk.min(MAT_MIDDLE_POS - mat_position);
                    }
                    let mat_end_pos = MAT_FRAME_SIZE - MAT_END_CODE.len();
                    if !end_code_skipped && mat_position < mat_end_pos {
                        available_chunk = available_chunk.min(mat_end_pos - mat_position);
                    }

                    if self.remaining_buffer_len() < available_chunk {
                        return Ok(None);
                    }

                    let chunk = self.take_chunk(available_chunk);
                    let bytes_remaining_after = bytes_remaining.saturating_sub(available_chunk);
                    let pending_chunk_bytes = chunk_size.saturating_sub(available_chunk);
                    self.pending_chunk_bytes =
                        (pending_chunk_bytes > 0).then_some(pending_chunk_bytes);
                    self.state = if bytes_remaining_after == 0 {
                        ParserState::WaitingForPayload
                    } else {
                        ParserState::ReadingPayload {
                            bytes_remaining: bytes_remaining_after,
                            mat_position: mat_position + available_chunk,
                            middle_code_skipped,
                            end_code_skipped,
                        }
                    };
                    return Ok(Some(chunk));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAT_FRAME_SIZE, MAT_MIDDLE_CODE, MAT_MIDDLE_POS, MAT_START_CODE, MatStream,
        copy_swapped_words,
    };

    #[test]
    fn accepts_expected_data_type() {
        assert!(MatStream::accepts_data_type(0x16));
        assert!(!MatStream::accepts_data_type(0x15));
    }

    #[test]
    fn strips_start_code_and_swaps_words() {
        let mut payload = Vec::new();
        payload.extend_from_slice(MAT_START_CODE);
        payload.extend_from_slice(&[0x02, 0x00, 0x34, 0x12]);

        let mut stream = MatStream::default();
        stream.push_payload(&payload);
        let out = stream.next_chunk().unwrap().unwrap();
        assert_eq!(out, vec![0x00, 0x02, 0x12, 0x34]);
    }

    #[test]
    fn preserves_chunk_continuation_across_middle_code() {
        let chunk_start = MAT_MIDDLE_POS - 100;
        let chunk_size = 200usize;
        let bytes_before_middle = MAT_MIDDLE_POS - chunk_start;
        let bytes_after_middle = chunk_size - bytes_before_middle;

        let mut payload = vec![0x00; MAT_FRAME_SIZE];
        payload[..MAT_START_CODE.len()].copy_from_slice(MAT_START_CODE);

        let raw_chunk_header = ((chunk_size / 2) as u16).to_le_bytes();
        let mut raw_chunk = vec![0x5A; chunk_size];
        raw_chunk[0] = raw_chunk_header[0];
        raw_chunk[1] = raw_chunk_header[1];
        for (idx, byte) in raw_chunk.iter_mut().enumerate().skip(2) {
            *byte = (idx & 0xFF) as u8;
        }

        payload[chunk_start..MAT_MIDDLE_POS].copy_from_slice(&raw_chunk[..bytes_before_middle]);
        payload[MAT_MIDDLE_POS..MAT_MIDDLE_POS + MAT_MIDDLE_CODE.len()]
            .copy_from_slice(MAT_MIDDLE_CODE);
        payload[MAT_MIDDLE_POS + MAT_MIDDLE_CODE.len()
            ..MAT_MIDDLE_POS + MAT_MIDDLE_CODE.len() + bytes_after_middle]
            .copy_from_slice(&raw_chunk[bytes_before_middle..]);

        let mut stream = MatStream::default();
        stream.push_payload(&payload);

        let mut out = Vec::new();
        while let Some(chunk) = stream.next_chunk().unwrap() {
            out.extend_from_slice(&chunk);
        }

        let mut expected = copy_swapped_words(&raw_chunk[..bytes_before_middle]);
        expected.extend_from_slice(&copy_swapped_words(&raw_chunk[bytes_before_middle..]));
        assert_eq!(out, expected);
    }

    #[test]
    fn preserves_zero_words_at_start_of_chunk_continuation() {
        let chunk_start = MAT_MIDDLE_POS - 6;
        let chunk_size = 20usize;
        let bytes_before_middle = MAT_MIDDLE_POS - chunk_start;
        let bytes_after_middle = chunk_size - bytes_before_middle;

        let mut payload = vec![0x00; MAT_FRAME_SIZE];
        payload[..MAT_START_CODE.len()].copy_from_slice(MAT_START_CODE);

        let raw_chunk_header = ((chunk_size / 2) as u16).to_le_bytes();
        let raw_chunk: [u8; 20] = [
            raw_chunk_header[0],
            raw_chunk_header[1],
            0xAA,
            0xBB,
            0xCC,
            0xDD,
            0x00,
            0x00,
            0x00,
            0x00,
            0x77,
            0x01,
            0xBC,
            0xCC,
            0xEF,
            0x68,
            0xB3,
            0x9E,
            0x61,
            0xFE,
        ];

        payload[chunk_start..MAT_MIDDLE_POS].copy_from_slice(&raw_chunk[..bytes_before_middle]);
        payload[MAT_MIDDLE_POS..MAT_MIDDLE_POS + MAT_MIDDLE_CODE.len()]
            .copy_from_slice(MAT_MIDDLE_CODE);
        payload[MAT_MIDDLE_POS + MAT_MIDDLE_CODE.len()
            ..MAT_MIDDLE_POS + MAT_MIDDLE_CODE.len() + bytes_after_middle]
            .copy_from_slice(&raw_chunk[bytes_before_middle..]);

        let mut stream = MatStream::default();
        stream.push_payload(&payload);

        let mut out = Vec::new();
        while let Some(chunk) = stream.next_chunk().unwrap() {
            out.extend_from_slice(&chunk);
        }

        assert_eq!(out, copy_swapped_words(&raw_chunk));
    }
}
