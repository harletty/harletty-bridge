//! What a family's `probe` is built from (`BridgeLib::probe`, Omniphony's
//! `BRIDGE_API.md`, "Probing"): a scan for the earliest stream start in a
//! window of raw bytes, each family judging a candidate by its own header,
//! and a driver that replays a stream to a probe the way a host does.
//!
//! A probe is stateless and bounded: each candidate costs at most the
//! family's longest header (its `needed` says how much), and a window is
//! scanned once. Nothing here allocates.

use bridge_api::{RInputTransport, RProbe, RProbeVerdict};

/// What a family makes of a candidate stream start at the first byte of the
/// bytes it is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// A validated start.
    Claim,
    /// Possibly a start, undecidable before this many bytes from it are
    /// shown: always more than were shown.
    Need(usize),
    /// Not a start of this family.
    Reject,
}

/// The earliest start in `data`: the first candidate `check` claims or
/// cannot decide yet. With none, every byte of `data` is ruled out.
#[inline]
pub fn scan_raw(data: &[u8], check: impl Fn(&[u8]) -> Start) -> RProbe {
    for at in 0..data.len() {
        match check(&data[at..]) {
            Start::Claim => return RProbe::claim(offset(at)),
            Start::Need(needed) => {
                debug_assert!(
                    needed > data.len() - at,
                    "a pending start must ask for more"
                );
                return RProbe::pending(offset(at), offset(needed));
            }
            Start::Reject => {}
        }
    }
    RProbe::none(offset(data.len()))
}

/// A probe's answer for one transport: an IEC 61937 burst type is claimed
/// whole or not at all; raw bytes are scanned by `raw`.
pub fn probe(
    data: &[u8],
    transport: RInputTransport,
    data_type: u8,
    accepts_data_type: impl Fn(u8) -> bool,
    raw: impl Fn(&[u8]) -> RProbe,
) -> RProbe {
    match transport {
        RInputTransport::Iec61937 if accepts_data_type(data_type) => RProbe::claim(0),
        RInputTransport::Iec61937 => RProbe::none(0),
        RInputTransport::Raw => raw(data),
    }
}

fn offset(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// `Need` the sync word `sync` when `data` is a prefix of it, `Reject` when
/// it is not; `None` once the whole sync word is there.
#[inline]
pub fn sync_prefix(data: &[u8], sync: &[u8]) -> Option<Start> {
    let shown = data.len().min(sync.len());
    if data[..shown] != sync[..shown] {
        Some(Start::Reject)
    } else if shown < sync.len() {
        Some(Start::Need(sync.len()))
    } else {
        None
    }
}

/// A host replaying `stream` to `probe` in `read`-byte reads, as Omniphony's
/// router does while a stream is undecided: it keeps the undecided bytes,
/// shows the probe what it has not ruled out, waits for what a pending start
/// asks for, and stops at the first claim.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Replay {
    /// Where the claimed stream starts, if one was claimed.
    pub claim: Option<usize>,
    /// How many bytes had been read when it was claimed (all of them
    /// otherwise).
    pub read: usize,
    /// Bytes shown to the probe, over every call.
    pub shown: usize,
    /// Calls to the probe.
    pub calls: usize,
    /// The longest a pending start asked for.
    pub most_needed: usize,
}

/// [`replay_reads`] in reads of `read` bytes.
#[doc(hidden)]
pub fn replay(probe: impl Fn(&[u8]) -> RProbe, stream: &[u8], read: usize) -> Replay {
    replay_reads(probe, stream, std::iter::repeat(read.max(1)))
}

/// [`replay_reads`] with the stream cut once, after `first` bytes.
#[doc(hidden)]
pub fn replay_split(probe: impl Fn(&[u8]) -> RProbe, stream: &[u8], first: usize) -> Replay {
    replay_reads(
        probe,
        stream,
        std::iter::once(first.max(1)).chain(std::iter::repeat(usize::MAX)),
    )
}

/// A host replaying `stream` to `probe` in reads of the given lengths, as
/// Omniphony's router does while a stream is undecided: it keeps the
/// undecided bytes, shows the probe those it has not ruled out, waits for
/// what a pending start asks for, and stops at the first claim. Checks the
/// probe's answers on the way (offsets within what it was shown, a pending
/// start asking for more than it was shown).
#[doc(hidden)]
pub fn replay_reads(
    probe: impl Fn(&[u8]) -> RProbe,
    stream: &[u8],
    reads: impl IntoIterator<Item = usize>,
) -> Replay {
    let mut replay = Replay {
        claim: None,
        read: 0,
        shown: 0,
        calls: 0,
        most_needed: 0,
    };
    let mut reads = reads.into_iter();
    // Where the next probe starts, and the read length before which it is
    // not asked again.
    let mut from = 0;
    let mut wait_until = 0;
    while replay.read < stream.len() {
        let read = reads.next().unwrap_or(usize::MAX).max(1);
        replay.read = replay.read.saturating_add(read).min(stream.len());
        if replay.read < wait_until || from >= replay.read {
            continue;
        }
        let window = &stream[from..replay.read];
        let answer = probe(window);
        replay.calls += 1;
        replay.shown += window.len();
        let at = answer.offset as usize;
        assert!(
            at <= window.len(),
            "offset {at} past the {} bytes shown",
            window.len()
        );
        match answer.verdict {
            RProbeVerdict::Claim => {
                replay.claim = Some(from + at);
                return replay;
            }
            RProbeVerdict::Pending => {
                let needed = answer.needed as usize;
                assert!(
                    needed > window.len() - at,
                    "a pending start asked for {needed} bytes, no more than the {} shown",
                    window.len() - at
                );
                replay.most_needed = replay.most_needed.max(needed);
                from += at;
                wait_until = from.saturating_add(needed);
            }
            RProbeVerdict::None => {
                from += at;
                wait_until = replay.read + 1;
            }
        }
    }
    replay
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A family whose stream starts with `AB CD` and a length byte: claimed
    /// when the whole header is there and the length is even.
    fn check(data: &[u8]) -> Start {
        if let Some(start) = sync_prefix(data, &[0xAB, 0xCD]) {
            return start;
        }
        match data.get(2) {
            None => Start::Need(3),
            Some(len) if len % 2 == 0 => Start::Claim,
            Some(_) => Start::Reject,
        }
    }

    fn probe(data: &[u8]) -> RProbe {
        scan_raw(data, check)
    }

    #[test]
    fn the_earliest_start_is_claimed_and_a_false_one_is_passed_over() {
        let stream = [0x00, 0xAB, 0xCD, 0x01, 0x11, 0xAB, 0xCD, 0x02, 0x00];
        assert_eq!(probe(&stream), RProbe::claim(5));
        assert_eq!(probe(&stream[..6]), RProbe::pending(5, 2));
        assert_eq!(probe(&stream[..7]), RProbe::pending(5, 3));
        assert_eq!(probe(&stream[..5]), RProbe::none(5));
        for read in 1..stream.len() {
            let replay = replay(probe, &stream, read);
            assert_eq!(replay.claim, Some(5), "read {read}");
        }
    }

    #[test]
    fn iec_61937_claims_a_burst_type_whole() {
        let takes = |data_type| data_type == 0x15;
        let raw = |_: &[u8]| RProbe::none(0);
        assert_eq!(
            super::probe(&[1, 2], RInputTransport::Iec61937, 0x15, takes, raw),
            RProbe::claim(0)
        );
        assert_eq!(
            super::probe(&[1, 2], RInputTransport::Iec61937, 0x16, takes, raw),
            RProbe::none(0)
        );
    }
}
