// SPDX-License-Identifier: Apache-2.0
//
// E-AC-3 IEC 61937 payload parser.
//
// E-AC-3 over S/PDIF uses data type 0x15 (IEC 61937-3).
// Unlike TrueHD (0x16), there is no MAT encapsulation.
// The burst payload contains one or more E-AC-3 syncframes, each
// prefixed by a 16-bit length code (in bits).  Padding bytes may
// fill the remainder of the burst.
//
// Reference: ffmpeg libavformat/spdifenc.c

use log::warn;

use crate::logging::bridge_external_log;

/// IEC 61937 data type for E-AC-3.
const IEC61937_EAC3_DATA_TYPE: u8 = 0x15;

/// IEC 61937 data type for AC-3 (IEC 61937-3).
///
/// Legacy AC-3 rides its own burst type and its own 48 kHz carrier, but the
/// bitstream inside shares the 0x0B77 syncword with E-AC-3 and is already
/// handled here: `ac3_frame_size_from_header` is tried before the E-AC-3 sizing
/// on every frame boundary, and the decoder accepts `bsid <= 10`.
const IEC61937_AC3_DATA_TYPE: u8 = 0x01;

/// E-AC-3 syncword (16-bit big-endian: 0x0B77).
const EAC3_SYNCWORD: u16 = 0x0B77;

/// Minimum payload size: at least 2 bytes for the length code plus
/// enough for a syncword and basic header.
const _MIN_PAYLOAD_SIZE: usize = 8;

/// Swap adjacent bytes (16-bit word endianness swap), like the MAT
/// parser's `copy_swapped_words` but in-place.
fn unswap_words(data: &mut [u8]) {
    let even_len = data.len() & !1;
    for i in (0..even_len).step_by(2) {
        data.swap(i, i + 1);
    }
}

fn needs_unswap(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0x77 && data[1] == 0x0B
}

fn frame_size_from_header(data: &[u8]) -> Option<usize> {
    if data.len() < 4 {
        return None;
    }

    let (header, low) = if needs_unswap(data) {
        (data[3], data[2])
    } else if data[0] == 0x0B && data[1] == 0x77 {
        (data[2], data[3])
    } else {
        return None;
    };

    let frmsiz = (((header & 0x07) as usize) << 8) | low as usize;
    Some((frmsiz + 1) * 2)
}

fn ac3_frame_size_from_header(data: &[u8]) -> Option<usize> {
    if data.len() < 5 {
        return None;
    }

    let header = if needs_unswap(data) {
        data[5]
    } else if data[0] == 0x0B && data[1] == 0x77 {
        data[4]
    } else {
        return None;
    };

    let fscod = header >> 6;
    let frmsizecod = header & 0x3F;
    let bitrate_index = usize::from(frmsizecod >> 1);
    if fscod > 2 || bitrate_index >= AC3_FRAME_SIZE_WORDS.len() {
        return None;
    }

    Some(AC3_FRAME_SIZE_WORDS[bitrate_index][usize::from(fscod)] * 2)
}

const AC3_FRAME_SIZE_WORDS: [[usize; 3]; 19] = [
    [64, 69, 96],
    [80, 87, 120],
    [96, 104, 144],
    [112, 121, 168],
    [128, 139, 192],
    [160, 174, 240],
    [192, 208, 288],
    [224, 243, 336],
    [256, 278, 384],
    [320, 348, 480],
    [384, 417, 576],
    [448, 487, 672],
    [512, 557, 768],
    [640, 696, 960],
    [768, 835, 1152],
    [896, 975, 1344],
    [1024, 1114, 1536],
    [1152, 1253, 1728],
    [1280, 1393, 1920],
];

#[derive(Debug)]
enum ParserState {
    WaitingForPayload,
    ReadingPayload {
        /// Remaining unprocessed bytes in the current payload.
        bytes_remaining: usize,
    },
}

/// Parses IEC 61937 payloads for E-AC-3 data type 0x15.
///
/// Each call to [`push_payload`] replaces the current payload.
/// [`next_frame`] extracts complete E-AC-3 access units one at a time.
pub struct Eac3SpdifStream {
    buffer: Vec<u8>,
    cursor: usize,
    state: ParserState,
    /// When a frame straddles the end of the current buffer, we save
    /// the incomplete portion here and resume when more data arrives.
    pending_frame: Option<Vec<u8>>,
    /// Expected total frame size from the pending_frame's length code.
    pending_frame_target: usize,
    /// Bytes already collected for the pending frame.
    pending_frame_consumed: usize,
    /// Whether the pending frame was captured in 16-bit word-swapped order.
    pending_frame_needs_unswap: bool,
}

impl Default for Eac3SpdifStream {
    fn default() -> Self {
        Self {
            buffer: Vec::with_capacity(64 * 1024),
            cursor: 0,
            state: ParserState::WaitingForPayload,
            pending_frame: None,
            pending_frame_target: 0,
            pending_frame_consumed: 0,
            pending_frame_needs_unswap: false,
        }
    }
}

impl Eac3SpdifStream {
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.state = ParserState::WaitingForPayload;
        self.pending_frame = None;
        self.pending_frame_target = 0;
        self.pending_frame_consumed = 0;
        self.pending_frame_needs_unswap = false;
    }

    /// Returns `true` if this parser handles the given IEC 61937 data type.
    pub fn accepts_data_type(data_type: u8) -> bool {
        matches!(data_type, IEC61937_EAC3_DATA_TYPE | IEC61937_AC3_DATA_TYPE)
    }

    /// Replace the current payload with a new one.
    ///
    /// Any unprocessed data from the previous payload is discarded.
    /// If a frame was partially received, the pending data is preserved
    /// separately and the new payload provides the continuation bytes.
    pub fn push_payload(&mut self, payload: &[u8]) {
        // Log the first bytes to diagnose endianness.
        let preview_len = payload.len().min(16);
        let preview: String = payload[..preview_len]
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ");
        bridge_external_log(
            log::Level::Debug,
            "harletty-bridge::eac3_spdif",
            &format!(
                "Eac3SpdifStream push_payload: {} bytes, preview=[{}], pending_target={}, pending_consumed={}",
                payload.len(),
                preview,
                self.pending_frame_target,
                self.pending_frame_consumed
            ),
        );
        self.buffer.clear();
        self.buffer.extend_from_slice(payload);
        self.cursor = 0;
        let total = self.buffer.len();
        self.state = ParserState::ReadingPayload {
            bytes_remaining: total,
        };
    }

    fn remaining_buffer_len(&self) -> usize {
        self.buffer.len().saturating_sub(self.cursor)
    }

    fn advance(&mut self, len: usize) {
        self.cursor = self.cursor.saturating_add(len);
    }

    /// Extract the next complete E-AC-3 access unit (syncframe).
    ///
    /// Returns `Ok(Some(frame))` when a complete frame is available.
    /// Returns `Ok(None)` when more data is needed.
    /// Returns `Err(...)` on a protocol error (triggers pipeline reset).
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, String> {
        loop {
            match self.state {
                ParserState::WaitingForPayload => return Ok(None),

                ParserState::ReadingPayload { bytes_remaining } => {
                    if bytes_remaining == 0 {
                        self.buffer.clear();
                        self.cursor = 0;
                        self.state = ParserState::WaitingForPayload;
                        self.pending_frame_target = 0;
                        self.pending_frame_consumed = 0;
                        continue;
                    }

                    // ── Continuing a pending frame? ──────────────────
                    if self.pending_frame_target > 0 {
                        let needed = self.pending_frame_target - self.pending_frame_consumed;
                        let available = self.remaining_buffer_len().min(bytes_remaining);

                        if available < needed {
                            // Still not enough data.
                            let mut pending = self.pending_frame.take().unwrap_or_default();
                            pending.extend_from_slice(
                                &self.buffer[self.cursor..self.cursor + available],
                            );
                            if !self.pending_frame_needs_unswap {
                                self.pending_frame_needs_unswap = needs_unswap(&pending);
                            }
                            self.pending_frame = Some(pending);
                            self.pending_frame_consumed += available;
                            self.buffer.clear();
                            self.cursor = 0;
                            self.state = ParserState::WaitingForPayload;
                            return Ok(None);
                        }

                        // We have enough to complete the frame.
                        let mut frame = self.pending_frame.take().unwrap_or_default();
                        frame.extend_from_slice(&self.buffer[self.cursor..self.cursor + needed]);
                        self.advance(needed);

                        let pending_needs_unswap =
                            self.pending_frame_needs_unswap || needs_unswap(&frame);
                        if pending_needs_unswap {
                            unswap_words(&mut frame);
                        }
                        self.pending_frame_needs_unswap = false;

                        // Verify syncword on the full frame.
                        if frame.len() >= 2 {
                            let sync = u16::from_be_bytes([frame[0], frame[1]]);
                            if sync != EAC3_SYNCWORD {
                                warn!(
                                    "E-AC3 SPDIF: bad syncword 0x{:04X} (expected 0x{:04X}) in {}B frame",
                                    sync,
                                    EAC3_SYNCWORD,
                                    frame.len()
                                );
                            }
                        }

                        let new_remaining = bytes_remaining.saturating_sub(needed);
                        self.pending_frame_target = 0;
                        self.pending_frame_consumed = 0;
                        self.state = ParserState::ReadingPayload {
                            bytes_remaining: new_remaining,
                        };

                        return Ok(Some(frame));
                    }

                    // ── Auto-detect payload format ─────────────────────
                    // Some SPDIF senders prepend a 16-bit length code (in bits).
                    // Others send raw E-AC3 syncframes directly.
                    // The PipeWire IEC958 transport may byte-swap 16-bit words,
                    // so we check for the syncword in both orders.
                    let has_length_prefix = if self.remaining_buffer_len() >= 4 {
                        let b0 = self.buffer[self.cursor];
                        let b1 = self.buffer[self.cursor + 1];
                        // Syncword 0x0B77 can appear as [0x0B, 0x77] (big-endian)
                        // or [0x77, 0x0B] (byte-swapped by IEC958 transport).
                        let looks_like_sync_be = b0 == 0x0B && b1 == 0x77;
                        let looks_like_sync_swapped = b0 == 0x77 && b1 == 0x0B;
                        if looks_like_sync_be || looks_like_sync_swapped {
                            bridge_external_log(
                                log::Level::Debug,
                                "harletty-bridge::eac3_spdif",
                                &format!(
                                    "Eac3SpdifStream: detected raw syncframe (no length prefix), bytes=0x{:02X} 0x{:02X} order={}",
                                    b0,
                                    b1,
                                    if looks_like_sync_be { "BE" } else { "swapped" }
                                ),
                            );
                        }
                        !(looks_like_sync_be || looks_like_sync_swapped)
                    } else {
                        true
                    };

                    if !has_length_prefix {
                        // Raw syncframe mode.
                        // If the syncword is byte-swapped, the entire frame is
                        // byte-swapped and we need to unswap it.
                        let need_unswap = self.buffer[self.cursor] == 0x77
                            && self.buffer[self.cursor + 1] == 0x0B;

                        // Read frame size from the syncframe header. E-AC3 and
                        // legacy AC-3 use different header layouts.
                        let frame_bytes = if self.remaining_buffer_len() >= 6 {
                            let data = &self.buffer[self.cursor..];
                            ac3_frame_size_from_header(data)
                                .or_else(|| frame_size_from_header(data))
                                .unwrap_or(0)
                        } else {
                            0
                        };

                        if frame_bytes == 0 {
                            if self.remaining_buffer_len() > 0 {
                                let available = self.remaining_buffer_len();
                                let mut pending = self.pending_frame.take().unwrap_or_default();
                                pending.extend_from_slice(
                                    &self.buffer[self.cursor..self.cursor + available],
                                );
                                self.pending_frame = Some(pending);
                                self.pending_frame_target = self.pending_frame_target.max(1);
                                self.pending_frame_consumed += available;
                            }
                            self.buffer.clear();
                            self.cursor = 0;
                            self.state = ParserState::WaitingForPayload;
                            return Ok(None);
                        }

                        if self.remaining_buffer_len() < frame_bytes {
                            let available = self.remaining_buffer_len();
                            let mut pending = self.pending_frame.take().unwrap_or_default();
                            pending.extend_from_slice(
                                &self.buffer[self.cursor..self.cursor + available],
                            );
                            self.pending_frame_needs_unswap = need_unswap;
                            self.pending_frame = Some(pending);
                            self.pending_frame_target = frame_bytes;
                            self.pending_frame_consumed = available;
                            self.buffer.clear();
                            self.cursor = 0;
                            self.state = ParserState::WaitingForPayload;
                            return Ok(None);
                        }

                        let mut frame =
                            self.buffer[self.cursor..self.cursor + frame_bytes].to_vec();
                        self.advance(frame_bytes);

                        // Unswap if needed.
                        if need_unswap {
                            unswap_words(&mut frame);
                        }

                        let preview: Vec<String> =
                            frame.iter().take(8).map(|b| format!("{b:02X}")).collect();
                        bridge_external_log(
                            log::Level::Debug,
                            "harletty-bridge::eac3_spdif",
                            &format!(
                                "eac3_spdif_raw frame={}B need_unswap={} first8=[{}]",
                                frame.len(),
                                need_unswap,
                                preview.join(" ")
                            ),
                        );

                        let new_remaining = bytes_remaining.saturating_sub(frame_bytes);
                        self.state = ParserState::ReadingPayload {
                            bytes_remaining: new_remaining,
                        };

                        return Ok(Some(frame));
                    }

                    // ── Length-prefix mode ────────────────────────────
                    // Read the 16-bit length code. Some sources expose it
                    // as bits, others as bytes, so the E-AC3 header decides.
                    let length_code = u16::from_le_bytes([
                        self.buffer[self.cursor],
                        self.buffer[self.cursor + 1],
                    ]) as usize;
                    self.advance(2);

                    let remaining = bytes_remaining.saturating_sub(2);

                    bridge_external_log(
                        log::Level::Debug,
                        "harletty-bridge::eac3_spdif",
                        &format!(
                            "Eac3SpdifStream length_code: raw={} as_bits={}B remaining_in_payload={}",
                            length_code,
                            (length_code + 7) / 8,
                            remaining
                        ),
                    );

                    if length_code == 0 {
                        // Zero length means no frame data. Skip any padding and finish.
                        self.buffer.clear();
                        self.cursor = 0;
                        self.state = ParserState::WaitingForPayload;
                        continue;
                    }

                    let length_code_bytes = length_code.div_ceil(8);
                    let header_frame_bytes =
                        ac3_frame_size_from_header(&self.buffer[self.cursor..])
                            .or_else(|| frame_size_from_header(&self.buffer[self.cursor..]))
                            .unwrap_or(0);
                    let frame_bytes = if header_frame_bytes > length_code_bytes
                        && header_frame_bytes <= remaining
                    {
                        bridge_external_log(
                            log::Level::Warn,
                            "harletty-bridge::eac3_spdif",
                            &format!(
                                "Eac3SpdifStream length_code disagrees with E-AC3 header: code={} as_bits={}B header={}B; using header size",
                                length_code, length_code_bytes, header_frame_bytes
                            ),
                        );
                        header_frame_bytes
                    } else {
                        length_code_bytes
                    };
                    if frame_bytes > remaining || self.remaining_buffer_len() < frame_bytes {
                        // Frame crosses payload boundary — save what we have.
                        let mut pending = Vec::with_capacity(frame_bytes);
                        let available = self.remaining_buffer_len();
                        pending
                            .extend_from_slice(&self.buffer[self.cursor..self.cursor + available]);
                        self.pending_frame_needs_unswap = needs_unswap(&pending);
                        self.pending_frame = Some(pending);
                        self.pending_frame_target = frame_bytes;
                        self.pending_frame_consumed = available;
                        self.buffer.clear();
                        self.cursor = 0;
                        self.state = ParserState::WaitingForPayload;
                        return Ok(None);
                    }

                    let mut frame = self.buffer[self.cursor..self.cursor + frame_bytes].to_vec();
                    self.advance(frame_bytes);

                    let need_unswap = needs_unswap(&frame);
                    if need_unswap {
                        unswap_words(&mut frame);
                    }

                    // Verify syncword.
                    if frame.len() >= 2 {
                        let sync = u16::from_be_bytes([frame[0], frame[1]]);
                        if sync != EAC3_SYNCWORD {
                            warn!(
                                "E-AC-3 SPDIF: bad syncword 0x{:04X} (expected 0x{:04X}) in {}B frame",
                                sync,
                                EAC3_SYNCWORD,
                                frame.len()
                            );
                        }
                    }

                    if need_unswap {
                        let preview: Vec<String> =
                            frame.iter().take(8).map(|b| format!("{b:02X}")).collect();
                        bridge_external_log(
                            log::Level::Debug,
                            "harletty-bridge::eac3_spdif",
                            &format!(
                                "eac3_spdif_length_prefixed unswapped frame={}B first8=[{}]",
                                frame.len(),
                                preview.join(" ")
                            ),
                        );
                    }

                    let new_remaining = remaining.saturating_sub(frame_bytes);
                    self.state = ParserState::ReadingPayload {
                        bytes_remaining: new_remaining,
                    };

                    return Ok(Some(frame));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build an IEC 61937 E-AC-3 payload with a single frame.
    fn build_payload(frame: &[u8]) -> Vec<u8> {
        let mut payload = Vec::with_capacity(2 + frame.len());
        let length_bits = (frame.len() * 8) as u16;
        payload.extend_from_slice(&length_bits.to_le_bytes());
        payload.extend_from_slice(frame);
        payload
    }

    /// Helper: build a minimal valid-looking E-AC-3 syncframe.
    fn build_minimal_eac3_frame(frame_words: usize) -> Vec<u8> {
        let frame_size = frame_words * 2;
        let frmsiz = frame_words.saturating_sub(1) as u32;
        let mut bytes = vec![0u8; frame_size];

        // Syncword 0x0B77 (big-endian)
        bytes[0] = 0x0B;
        bytes[1] = 0x77;

        // Write the basic BSI fields into the bitstream.
        // The code below writes them MSB-first into the byte array.
        let mut bits: Vec<bool> = Vec::new();
        let push = |bits: &mut Vec<bool>, value: u32, width: usize| {
            for bit in (0..width).rev() {
                bits.push(((value >> bit) & 1) != 0);
            }
        };

        push(&mut bits, 0x0B77, 16); // syncword
        push(&mut bits, 0, 2); // frame_type = independent
        push(&mut bits, 0, 3); // substreamid
        push(&mut bits, frmsiz, 11); // frmsiz
        push(&mut bits, 0, 2); // fscod = 48 kHz
        push(&mut bits, 3, 2); // numblkscod = 6 blocks
        push(&mut bits, 7, 3); // acmod = 7 (5.1)
        push(&mut bits, 1, 1); // lfeon
        push(&mut bits, 16, 5); // bsid = 16 (E-AC-3)
        push(&mut bits, 0, 5); // dialnorm
        push(&mut bits, 0, 1); // compr
        push(&mut bits, 0, 1); // compr2
        push(&mut bits, 0, 1); // dialnorm2
        push(&mut bits, 0, 1); // compr2e

        for (index, bit) in bits.iter().enumerate() {
            if *bit {
                bytes[index >> 3] |= 1 << (7 - (index & 7));
            }
        }
        bytes
    }

    fn swapped_words(frame: &[u8]) -> Vec<u8> {
        let mut swapped = frame.to_vec();
        unswap_words(&mut swapped);
        swapped
    }

    #[test]
    fn accepts_expected_data_type() {
        assert!(Eac3SpdifStream::accepts_data_type(0x15));
        // AC-3 rides burst type 0x01, and its frames are sized and decoded by
        // this same parser.
        assert!(Eac3SpdifStream::accepts_data_type(0x01));
        // TrueHD/MAT stays with the MAT stream.
        assert!(!Eac3SpdifStream::accepts_data_type(0x16));
        // DTS types belong to the DTS path.
        assert!(!Eac3SpdifStream::accepts_data_type(0x0B));
    }

    #[test]
    fn extracts_single_frame() {
        let frame = build_minimal_eac3_frame(32);
        let payload = build_payload(&frame);

        let mut stream = Eac3SpdifStream::default();
        stream.push_payload(&payload);

        let result = stream.next_frame().unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap(), frame);

        // No more frames.
        assert_eq!(stream.next_frame().unwrap(), None);
    }

    #[test]
    fn length_prefixed_swapped_frame_is_normalized() {
        let frame = build_minimal_eac3_frame(32);
        let payload = build_payload(&swapped_words(&frame));

        let mut stream = Eac3SpdifStream::default();
        stream.push_payload(&payload);

        let result = stream.next_frame().unwrap();
        assert_eq!(result, Some(frame));
        assert_eq!(stream.next_frame().unwrap(), None);
    }

    #[test]
    fn byte_count_length_code_uses_header_frame_size() {
        let frame = build_minimal_eac3_frame(896);
        let mut payload = Vec::new();
        payload.extend_from_slice(&(frame.len() as u16).to_le_bytes());
        payload.extend_from_slice(&frame);

        let mut stream = Eac3SpdifStream::default();
        stream.push_payload(&payload);

        let result = stream.next_frame().unwrap();
        assert_eq!(result, Some(frame));
        assert_eq!(stream.next_frame().unwrap(), None);
    }

    #[test]
    fn extracts_multiple_frames() {
        let frame1 = build_minimal_eac3_frame(32);
        let frame2 = build_minimal_eac3_frame(64);

        let mut payload = Vec::new();
        // Frame 1
        let len1 = (frame1.len() * 8) as u16;
        payload.extend_from_slice(&len1.to_le_bytes());
        payload.extend_from_slice(&frame1);
        // Frame 2
        let len2 = (frame2.len() * 8) as u16;
        payload.extend_from_slice(&len2.to_le_bytes());
        payload.extend_from_slice(&frame2);

        let mut stream = Eac3SpdifStream::default();
        stream.push_payload(&payload);

        let result1 = stream.next_frame().unwrap();
        assert_eq!(result1, Some(frame1));

        let result2 = stream.next_frame().unwrap();
        assert_eq!(result2, Some(frame2));

        assert_eq!(stream.next_frame().unwrap(), None);
    }

    #[test]
    fn handles_padding_after_frame() {
        let frame = build_minimal_eac3_frame(32);
        let mut payload = build_payload(&frame);
        // Add zero padding after the frame.
        payload.extend_from_slice(&[0u8; 16]);

        let mut stream = Eac3SpdifStream::default();
        stream.push_payload(&payload);

        let result = stream.next_frame().unwrap();
        assert_eq!(result, Some(frame));
        assert_eq!(stream.next_frame().unwrap(), None);
    }

    #[test]
    fn pending_frame_across_payloads() {
        let frame = build_minimal_eac3_frame(128);
        let midway = frame.len() / 2;

        let mut stream = Eac3SpdifStream::default();

        // First payload: length code + first half of frame
        let len_bits = (frame.len() * 8) as u16;
        let mut payload1 = Vec::new();
        payload1.extend_from_slice(&len_bits.to_le_bytes());
        payload1.extend_from_slice(&frame[..midway]);
        stream.push_payload(&payload1);
        assert_eq!(stream.next_frame().unwrap(), None); // incomplete

        // Second payload: second half of frame
        let mut payload2 = Vec::new();
        payload2.extend_from_slice(&frame[midway..]);
        stream.push_payload(&payload2);

        let result = stream.next_frame().unwrap();
        assert_eq!(result, Some(frame));
        assert_eq!(stream.next_frame().unwrap(), None);
    }

    #[test]
    fn pending_swapped_frame_is_normalized() {
        let frame = build_minimal_eac3_frame(128);
        let swapped = swapped_words(&frame);
        let midway = swapped.len() / 2;

        let mut stream = Eac3SpdifStream::default();

        let len_bits = (swapped.len() * 8) as u16;
        let mut payload1 = Vec::new();
        payload1.extend_from_slice(&len_bits.to_le_bytes());
        payload1.extend_from_slice(&swapped[..midway]);
        stream.push_payload(&payload1);
        assert_eq!(stream.next_frame().unwrap(), None);

        stream.push_payload(&swapped[midway..]);

        let result = stream.next_frame().unwrap();
        assert_eq!(result, Some(frame));
        assert_eq!(stream.next_frame().unwrap(), None);
    }

    #[test]
    fn extracts_raw_syncframe_without_length_prefix() {
        let frame = build_minimal_eac3_frame(64);
        // Send raw syncframe directly (no length prefix).
        let payload = frame.clone();

        let mut stream = Eac3SpdifStream::default();
        stream.push_payload(&payload);

        let result = stream.next_frame().unwrap();
        assert_eq!(result, Some(frame));
        assert_eq!(stream.next_frame().unwrap(), None);
    }

    #[test]
    fn extracts_legacy_ac3_raw_frames_with_ac3_header_size() {
        let mut frame = vec![0u8; 2304];
        frame[..8].copy_from_slice(&[0x0B, 0x77, 0x2A, 0x68, 0x22, 0x30, 0xE1, 0xFF]);
        let mut payload = Vec::with_capacity(frame.len() * 2);
        payload.extend_from_slice(&frame);
        payload.extend_from_slice(&frame);

        let mut stream = Eac3SpdifStream::default();
        stream.push_payload(&payload);

        let first = stream.next_frame().unwrap().unwrap();
        let second = stream.next_frame().unwrap().unwrap();
        assert_eq!(first.len(), 2304);
        assert_eq!(second.len(), 2304);
        assert_eq!(&first[..8], &frame[..8]);
        assert_eq!(&second[..8], &frame[..8]);
        assert_eq!(stream.next_frame().unwrap(), None);
    }

    #[test]
    fn raw_swapped_syncframe_is_normalized() {
        let frame = build_minimal_eac3_frame(64);
        let payload = swapped_words(&frame);

        let mut stream = Eac3SpdifStream::default();
        stream.push_payload(&payload);

        let result = stream.next_frame().unwrap();
        assert_eq!(result, Some(frame));
        assert_eq!(stream.next_frame().unwrap(), None);
    }

    #[test]
    fn reset_clears_state() {
        let frame = build_minimal_eac3_frame(32);
        let payload = build_payload(&frame);

        let mut stream = Eac3SpdifStream::default();
        stream.push_payload(&payload);
        stream.reset();
        assert_eq!(stream.next_frame().unwrap(), None);
    }
}
