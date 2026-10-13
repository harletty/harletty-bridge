//! IAMF on the IEC 61937 transport: a private burst type, for a sender and
//! a receiver that are both ours (Omniphony `docs/iamf-transport-plan.md`).
//!
//! The IEC assigned IAMF no data type, so no third-party equipment knows
//! this one. A burst's payload is whole OBUs, `Pd` counting its bytes: the
//! OBUs of one temporal unit, preceded now and then by a copy of the
//! sequence's descriptors with `obu_redundant_copy` set, so a receiver that
//! joins mid-stream opens the sequence at the next copy. A sequence header
//! without the flag starts a new sequence, as on the raw transport.
//!
//! Unlike a raw stream, a burst is a resync point: one damaged on the
//! carrier costs its own unit and no other. So no audio of a damaged burst is
//! decoded. A substream decoded from the part that survived would wait in
//! the decoder for the rest of its unit and be matched with the next
//! unit's, shifting every unit after it. A burst is damaged when it is not
//! whole OBUs, or when its audio frames are not one for each substream of
//! the sequence, which is what a temporal unit is: a burst that lost exactly
//! its last OBU is still whole OBUs.
//!
//! The descriptors a damaged burst leads with are taken all the same: they
//! framed, and a receiver left without them would decode nothing until the
//! next copy, which a sender need not repeat.

use bridge_api::RPushResult;
use bridge_common::logging::bridge_log;
use bridge_common::shared::{AfterPush, SharedState};

use iamf_dec::presentation::Descriptors;

use crate::{IamfState, ObuKind, frame_obu, obu_kind, push_iamf};

/// The private IEC 61937 data type (`Pc` bits 0-4) of an IAMF burst. Kept in
/// step with `spdif::DATA_TYPE_IAMF_PRIVATE` in Omniphony, which the sender
/// and the host's carrier table use.
pub const IAMF_DATA_TYPE: u8 = 0x1D;

/// The IEC 61937 data type carrying IAMF.
pub fn accepts_data_type(data_type: u8) -> bool {
    data_type == IAMF_DATA_TYPE
}

const OBU_AUDIO_FRAME: u8 = 5;
const OBU_AUDIO_FRAME_ID0: u8 = 6;
const OBU_AUDIO_FRAME_ID17: u8 = 23;
const OBU_TRIMMING_STATUS: u8 = 0x02;
const OBU_EXTENSION: u8 = 0x01;

/// A leb128 of at most 8 bytes (IAMF §2.4): its value and the bytes it took.
fn leb128(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (index, &byte) in data.iter().take(8).enumerate() {
        value |= u64::from(byte & 0x7F) << (7 * index);
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

/// The substream an audio frame OBU belongs to: implied by its type, or,
/// for type 5, stated after its trimming and extension fields.
fn audio_substream(obu: &[u8]) -> Option<u64> {
    let obu_type = obu[0] >> 3;
    if obu_type != OBU_AUDIO_FRAME {
        return Some(u64::from(obu_type - OBU_AUDIO_FRAME_ID0));
    }
    let (_, size_len) = leb128(&obu[1..])?;
    let mut body = &obu[1 + size_len..];
    if obu[0] & OBU_TRIMMING_STATUS != 0 {
        for _ in 0..2 {
            let (_, len) = leb128(body)?;
            body = &body[len..];
        }
    }
    if obu[0] & OBU_EXTENSION != 0 {
        let (size, len) = leb128(body)?;
        body = body.get(len.checked_add(usize::try_from(size).ok()?)?..)?;
    }
    leb128(body).map(|(id, _)| id)
}

/// Why a burst is not decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Damage {
    /// Not whole OBUs.
    Framing,
    /// Whole OBUs, but not one audio frame for each substream.
    Unit,
}

/// What [`check`] made of a burst.
struct Checked {
    /// Where the descriptors the burst leads with end: whole OBUs, whatever
    /// became of what follows them.
    lead: usize,
    verdict: Result<(), Damage>,
}

/// Whether `payload` crossed the carrier intact, as far as can be told:
/// whole OBUs, and one audio frame for each substream of the sequence they
/// belong to. That sequence is the one the burst's own descriptors open
/// (from its last sequence header that opens one: a sender may lead a new
/// sequence with its copy of the old), else the one that is open, else the
/// one whose descriptors came in a burst of their own and wait for their
/// first unit. A burst without audio frames and one for which no sequence
/// is known are taken as they are.
fn check(state: &mut IamfState, payload: &[u8]) -> Checked {
    let mut pos = 0;
    let mut lead = 0;
    // The sequence header the unit's sequence starts at, when the burst
    // opens one: a header that is not a copy, or a copy while none is open.
    let mut opens_at = None;
    let mut audio = 0usize;
    while pos < payload.len() {
        let obu = match frame_obu(&payload[pos..]) {
            Ok(Some(obu)) => obu,
            Ok(None) | Err(_) => {
                return Checked {
                    lead,
                    verdict: Err(Damage::Framing),
                };
            }
        };
        match obu_kind(obu.obu_type) {
            ObuKind::SequenceHeader if lead == pos => {
                if !obu.redundant || !(state.has_sequence() || opens_at.is_some()) {
                    opens_at = Some(pos);
                }
                lead = pos + obu.len;
            }
            ObuKind::Descriptor if lead == pos => lead = pos + obu.len,
            _ if (OBU_AUDIO_FRAME..=OBU_AUDIO_FRAME_ID17).contains(&obu.obu_type) => audio += 1,
            _ => {}
        }
        pos += obu.len;
    }
    Checked {
        lead,
        verdict: whole_unit(state, payload, lead, opens_at, audio),
    }
}

/// The second half of [`check`], on a payload that is whole OBUs with
/// `audio` audio frames after its `lead`.
fn whole_unit(
    state: &mut IamfState,
    payload: &[u8],
    lead: usize,
    opens_at: Option<usize>,
    audio: usize,
) -> Result<(), Damage> {
    if audio == 0 {
        return Ok(());
    }
    // Descriptors are parsed on the bursts that open a sequence and until
    // its first unit, not on every burst: after that the list is the
    // state's.
    let parsed_here;
    let expected: &[u32] = if let Some(at) = opens_at {
        match Descriptors::collect(&payload[at..lead]) {
            Ok(parsed) => {
                parsed_here = crate::sequence_substreams(&parsed);
                &parsed_here
            }
            // Not a sequence the decoder will open: its refusal says why.
            Err(_) => return Ok(()),
        }
    } else if state.has_sequence() {
        &state.substreams
    } else if !state.descriptors.is_empty() && !state.refused {
        match Descriptors::collect(&state.descriptors) {
            Ok(parsed) => {
                parsed_here = crate::sequence_substreams(&parsed);
                &parsed_here
            }
            Err(_) => return Ok(()),
        }
    } else {
        return Ok(());
    };
    if audio != expected.len() {
        return Err(Damage::Unit);
    }
    let seen = &mut state.unit_substreams;
    seen.clear();
    seen.resize(expected.len(), false);
    let mut pos = lead;
    while pos < payload.len() {
        // Framed by the caller.
        let Ok(Some(obu)) = frame_obu(&payload[pos..]) else {
            return Err(Damage::Framing);
        };
        if (OBU_AUDIO_FRAME..=OBU_AUDIO_FRAME_ID17).contains(&obu.obu_type) {
            let id = audio_substream(&payload[pos..pos + obu.len]).ok_or(Damage::Framing)?;
            let at = expected.iter().position(|&known| u64::from(known) == id);
            match at {
                Some(at) if !seen[at] => seen[at] = true,
                // A substream twice, or one the sequence does not have.
                _ => return Err(Damage::Unit),
            }
        }
        pos += obu.len;
    }
    Ok(())
}

/// Decode one burst payload: a whole temporal unit, nothing carried over
/// from the burst before. Of a damaged one only the descriptors it leads
/// with are taken.
pub fn push_burst(
    state: &mut IamfState,
    shared: &mut SharedState,
    payload: &[u8],
    result: &mut RPushResult,
) -> AfterPush {
    // A burst never continues the one before.
    state.buf.clear();
    let checked = check(state, payload);
    let Err(damage) = checked.verdict else {
        return push_iamf(state, shared, payload, result);
    };
    if let Some(bursts) = state.truncated_bursts.note() {
        bridge_log!(
            log::Level::Warn,
            "iamf: IEC 61937 burst dropped, {} ({} byte(s), {bursts} burst(s) so far)",
            match damage {
                Damage::Framing => "not whole OBUs",
                Damage::Unit => "not a whole temporal unit",
            },
            payload.len()
        );
    }
    if checked.lead == 0 {
        return AfterPush::Continue;
    }
    push_iamf(state, shared, &payload[..checked.lead], result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use abi_stable::std_types::{RString, RVec};
    use bridge_api::RDecodedFrame;

    /// `obu_redundant_copy` in an OBU header.
    const REDUNDANT: u8 = 0x04;

    struct Harness {
        state: IamfState,
        shared: SharedState,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                state: IamfState::default(),
                shared: SharedState::new(false),
            }
        }

        fn result() -> RPushResult {
            RPushResult {
                frames: RVec::new(),
                error_message: RString::new(),
                did_reset: false,
            }
        }

        fn raw(&mut self, data: &[u8]) -> RPushResult {
            let mut result = Self::result();
            let _ = push_iamf(&mut self.state, &mut self.shared, data, &mut result);
            result
        }

        fn burst(&mut self, payload: &[u8]) -> RPushResult {
            let mut result = Self::result();
            let _ = push_burst(&mut self.state, &mut self.shared, payload, &mut result);
            result
        }

        fn bursts(&mut self, bursts: &[Vec<u8>]) -> Vec<RDecodedFrame> {
            let mut frames = Vec::new();
            for payload in bursts {
                let result = self.burst(payload);
                assert!(result.error_message.is_empty(), "{}", result.error_message);
                frames.extend(result.frames);
            }
            frames
        }
    }

    fn vector(name: &str) -> Option<Vec<u8>> {
        let dir = std::env::var_os("HARLETTY_IAMF_VECTORS")?;
        std::fs::read(std::path::Path::new(&dir).join(name)).ok()
    }

    fn pcm(frames: &[RDecodedFrame]) -> Vec<i32> {
        frames.iter().flat_map(|f| f.pcm.iter().copied()).collect()
    }

    /// A raw sequence as a sender puts it on the carrier: its descriptors,
    /// then its temporal units, one to a burst. For a sequence of one
    /// substream: a unit ends with its audio frame.
    struct Carried {
        descriptors: Vec<u8>,
        units: Vec<Vec<u8>>,
    }

    impl Carried {
        fn of(stream: &[u8]) -> Self {
            let mut descriptors = Vec::new();
            let mut data: Vec<Vec<u8>> = Vec::new();
            let mut unit = Vec::new();
            let mut pos = 0;
            while let Some(frame) = frame_obu(&stream[pos..]).unwrap() {
                let obu = &stream[pos..pos + frame.len];
                match obu_kind(frame.obu_type) {
                    ObuKind::SequenceHeader | ObuKind::Descriptor
                        if data.is_empty() && unit.is_empty() =>
                    {
                        descriptors.extend_from_slice(obu);
                    }
                    ObuKind::Data => {
                        unit.extend_from_slice(obu);
                        if (OBU_AUDIO_FRAME..=OBU_AUDIO_FRAME_ID17).contains(&frame.obu_type) {
                            data.push(std::mem::take(&mut unit));
                        }
                    }
                    _ => {}
                }
                pos += frame.len;
            }
            assert_eq!(pos, stream.len(), "a whole number of OBUs");
            Self {
                descriptors,
                units: data,
            }
        }

        /// The descriptors as the copy a sender repeats.
        fn redundant_copy(&self) -> Vec<u8> {
            let mut copy = self.descriptors.clone();
            let mut pos = 0;
            while let Some(frame) = frame_obu(&copy[pos..]).unwrap() {
                copy[pos] |= REDUNDANT;
                pos += frame.len;
            }
            copy
        }
    }

    // Sequence header: type 31, six bytes, `iamf` and two profiles.
    const HEADER: [u8; 8] = [31 << 3, 0x06, b'i', b'a', b'm', b'f', 0x00, 0x00];

    #[test]
    fn only_the_private_type_is_taken() {
        for data_type in 0..=u8::MAX {
            assert_eq!(accepts_data_type(data_type), data_type == 0x1D);
        }
    }

    /// A burst cut short on the carrier does not swallow the next one.
    #[test]
    fn a_truncated_burst_does_not_reach_into_the_next() {
        let mut bridge = Harness::new();
        // An audio frame OBU announcing 100 bytes, 3 of them there.
        bridge.burst(&[6 << 3, 100, 0xAA, 0xBB, 0xCC]);
        assert!(bridge.state.buf.is_empty());
        assert_eq!(bridge.state.truncated_bursts.count(), 1);
        // Raw, those bytes would be the start of that OBU and the header
        // would be lost in it.
        bridge.burst(&HEADER);
        assert_eq!(bridge.state.descriptors, HEADER);
        assert_eq!(bridge.state.truncated_bursts.count(), 1);

        let mut raw = Harness::new();
        raw.raw(&[6 << 3, 100, 0xAA, 0xBB, 0xCC]);
        raw.raw(&HEADER);
        assert!(raw.state.descriptors.is_empty());
    }

    /// The audio of a damaged burst is dropped, the frames that still frame
    /// included: they are part of a unit the rest of which is lost. The
    /// descriptors it leads with are taken: without them a receiver has no
    /// sequence until the next copy, which may never come.
    #[test]
    fn a_damaged_burst_keeps_the_descriptors_it_leads_with() {
        let mut bridge = Harness::new();
        let mut damaged = HEADER.to_vec();
        damaged.extend_from_slice(&[6 << 3, 100, 0xAA, 0xBB, 0xCC]);
        bridge.burst(&damaged);
        assert_eq!(bridge.state.descriptors, HEADER);
        assert!(bridge.state.buf.is_empty());
        assert_eq!(bridge.state.truncated_bursts.count(), 1);
        // An OBU size that does not end is damage as well.
        bridge.burst(&[6 << 3, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00]);
        assert_eq!(bridge.state.truncated_bursts.count(), 2);
    }

    /// An audio frame names its substream by its type, or, for type 5,
    /// after its trimming and extension fields.
    #[test]
    fn an_audio_frame_names_its_substream() {
        assert_eq!(audio_substream(&[6 << 3, 1, 0xAA]), Some(0));
        assert_eq!(audio_substream(&[23 << 3, 1, 0xAA]), Some(17));
        assert_eq!(audio_substream(&[5 << 3, 2, 40, 0xAA]), Some(40));
        // Trimmed: two leb128 first, one of them on two bytes.
        let trimmed = [(5 << 3) | 0x02, 5, 0, 0x90, 0x01, 41, 0xAA];
        assert_eq!(audio_substream(&trimmed), Some(41));
        // An extension of two bytes before the id.
        let extended = [(5 << 3) | 0x01, 5, 2, 0xEE, 0xEE, 42, 0xAA];
        assert_eq!(audio_substream(&extended), Some(42));
        // An extension that runs past the OBU.
        assert_eq!(audio_substream(&[(5 << 3) | 0x01, 2, 9, 0xEE]), None);
    }

    /// A unit is one audio frame for each substream of its sequence: audio
    /// frames opened by descriptors that declare no substream are not one.
    #[test]
    fn audio_frames_the_sequence_does_not_have_are_not_a_unit() {
        let mut bridge = Harness::new();
        let mut payload = HEADER.to_vec();
        payload.extend_from_slice(&[6 << 3, 0x02, 0xAB, 0xCD]);
        assert_eq!(
            check(&mut bridge.state, &payload).verdict,
            Err(Damage::Unit)
        );
        assert_eq!(check(&mut bridge.state, &payload).lead, HEADER.len());
        // The descriptors alone are a burst like any other.
        assert_eq!(check(&mut bridge.state, &HEADER).verdict, Ok(()));
        // Without a sequence nothing says what a unit is: the framer drops
        // the data as it does raw.
        assert_eq!(
            check(&mut bridge.state, &[6 << 3, 0x02, 0xAB, 0xCD]).verdict,
            Ok(())
        );
        assert_eq!(
            check(&mut bridge.state, &[6 << 3, 0x09, 0xAB]).verdict,
            Err(Damage::Framing)
        );
    }

    /// Before a sequence is open, a repeated copy of the descriptors opens
    /// it: that is how a receiver joins a stream already running.
    #[test]
    fn a_redundant_copy_opens_the_sequence_of_a_late_receiver() {
        let mut bridge = Harness::new();
        let mut copy = HEADER;
        copy[0] |= REDUNDANT;
        // Units of a sequence it never saw the start of: dropped.
        bridge.burst(&[6 << 3, 0x02, 0xAB, 0xCD]);
        assert!(bridge.state.descriptors.is_empty());
        bridge.burst(&copy);
        assert_eq!(bridge.state.descriptors, copy);
    }

    #[test]
    fn bursts_decode_as_the_raw_stream_does() {
        let Some(stream) = vector("test_000800.iamf") else {
            eprintln!("skipping: set HARLETTY_IAMF_VECTORS to the libiamf test vectors");
            return;
        };
        let mut raw = Harness::new();
        let expected = pcm(&raw.raw(&stream).frames);
        assert!(expected.len() > 100_000);

        let carried = Carried::of(&stream);
        let mut bridge = Harness::new();
        let mut frames = Vec::new();
        for (index, unit) in carried.units.iter().enumerate() {
            // The descriptors open the first burst; a copy now and then, as
            // a sender repeats them, is ignored while the sequence is open.
            let mut payload = match index {
                0 => carried.descriptors.clone(),
                _ if index % 5 == 4 => carried.redundant_copy(),
                _ => Vec::new(),
            };
            payload.extend_from_slice(unit);
            frames.extend(bridge.bursts(&[payload]));
        }
        assert_eq!(pcm(&frames), expected);
        assert_eq!(bridge.state.truncated_bursts.count(), 0);
    }

    /// A receiver that joins after the start decodes from the first copy of
    /// the descriptors on, and what it decodes is the stream's.
    #[test]
    fn a_late_receiver_decodes_from_the_next_copy() {
        let Some(stream) = vector("test_000800.iamf") else {
            eprintln!("skipping: set HARLETTY_IAMF_VECTORS to the libiamf test vectors");
            return;
        };
        let carried = Carried::of(&stream);
        let mut whole = Harness::new();
        let mut per_unit = vec![
            whole
                .bursts(std::slice::from_ref(&carried.descriptors))
                .len(),
        ];
        for unit in &carried.units {
            per_unit.push(whole.bursts(std::slice::from_ref(unit)).len());
        }

        let joined_at = carried.units.len() / 3;
        let mut late = Harness::new();
        // Missed: the descriptors and the first third of the units. Heard
        // without a sequence: a few units, which decode to nothing.
        let heard_blind = 3;
        let frames = late.bursts(&carried.units[joined_at..joined_at + heard_blind]);
        assert!(frames.is_empty());
        let mut payload = carried.redundant_copy();
        payload.extend_from_slice(&carried.units[joined_at + heard_blind]);
        let mut frames = late.bursts(&[payload]);
        frames.extend(late.bursts(&carried.units[joined_at + heard_blind + 1..]));

        // LPCM units stand alone: the tail is the stream's tail, sample for
        // sample.
        let mut reference = Harness::new();
        let all = reference.raw(&stream).frames;
        let skipped: usize = per_unit[..1 + joined_at + heard_blind].iter().sum();
        assert!(!frames.is_empty());
        assert_eq!(pcm(&frames), pcm(&all[skipped..]));
    }
}
