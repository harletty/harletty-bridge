// SPDX-License-Identifier: Apache-2.0
//! Whether a TrueHD stream's Evolution frames are signed by the key this
//! machine holds.
//!
//! An Evolution frame closes with a protection word the format leaves to the
//! implementation and a decoder is told it may ignore. It holds the leading
//! bytes of a keyed digest over the access unit and the frame — the decoder
//! library computes it, `ExtraData::verify_evo_protection`, and this module
//! only counts what it answers, access unit by access unit.
//!
//! What the counts are worth:
//!
//! - every access unit verifying means the stream's metadata came out of an
//!   encoder holding this key, and has not been touched since: the digest
//!   covers the audio of the unit as well as the metadata;
//! - a mismatch means the metadata or the audio was rewritten after signing,
//!   or that the stream was signed with another key;
//! - no protection word at all means whatever wrote the stream did not sign
//!   it. A remux does not strip one, so for a stream that carries Evolution
//!   frames this is a re-encode.
//!
//! What it is not: a checksum over the whole stream. Each digest covers the
//! access unit that carries the frame and nothing else, and a stream carries
//! one Evolution frame per object-metadata update — about one access unit in
//! forty, measured. So a byte changed in one of the other thirty-nine is not
//! caught, while anything that rewrites the stream — a re-encode, a different
//! encoder, a metadata edit — is, because it does not reproduce the digests
//! it did not have the key to compute.
//!
//! Frames carried in an access unit that also carries a major sync are
//! counted apart and do not decide the verdict. Across a library of
//! commercial discs, every frame in a unit without a major sync verified, in
//! every title, while in some titles the frames in major-sync units failed —
//! all of them in one title, a few in another — and in others verified too.
//! Whether a tool rewrote those headers after encoding or the digest covers
//! them differently is not established; what is, is that such a failure sits
//! next to thousands of units whose audio and metadata carry the key's
//! digest, so it is no evidence that the stream was re-encoded. A match there
//! still counts: a digest cannot be produced without the key.
//!
//! Without a key nothing is checked and nothing is claimed: [`State::Unchecked`]
//! is not a verdict on the stream. The key itself is never logged, never
//! reported and never written anywhere — only the path it was read from.

use std::path::{Path, PathBuf};

use truehd::structs::access_unit::AccessUnit;
use truehd::structs::evolution::EvoProtectionStatus;

/// What a run of access units says about the stream's signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No key on this machine, so the question was not asked.
    Unchecked,
    /// Nothing that could be judged in what was read: no Evolution frame —
    /// a stream without object metadata, or a read too short to reach its
    /// first one — or only words in major-sync units that do not verify.
    /// This says nothing about how the stream was made.
    Absent,
    /// Evolution frames were read, and none carried a protection word.
    Unsigned,
    /// No word in a unit without a major sync failed, and at least one word
    /// read is the digest the key produces.
    Verified,
    /// A word in a unit without a major sync is not the key's digest.
    Mismatch,
}

impl State {
    /// The word the JSON report carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unchecked => "unchecked",
            Self::Absent => "absent",
            Self::Unsigned => "unsigned",
            Self::Verified => "verified",
            Self::Mismatch => "mismatch",
        }
    }
}

/// The counts behind the verdict.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally {
    /// Access units parsed.
    pub units: u64,
    /// Of those, the ones carrying an Evolution frame. A stream with none
    /// carries no object metadata either: there is nothing to sign.
    pub frames: u64,
    /// Of those, in units without a major sync, the ones carrying a primary
    /// protection word: the words the verdict rests on.
    pub checked: u64,
    /// Of those, the ones whose word is the digest of the key.
    pub verified: u64,
    /// Of those, the ones whose word is not.
    pub mismatched: u64,
    /// Words in units that carry a major sync, counted apart; see the
    /// module documentation.
    pub sync_checked: u64,
    /// Of those, the ones whose word is the digest of the key.
    pub sync_verified: u64,
}

impl Tally {
    pub fn state(&self) -> State {
        if self.mismatched > 0 {
            State::Mismatch
        } else if self.verified + self.sync_verified > 0 {
            State::Verified
        } else if self.frames > 0 && self.sync_checked == 0 {
            State::Unsigned
        } else {
            State::Absent
        }
    }
}

/// What a finished run of access units says, with the key it was run under.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub tally: Tally,
    /// Where the key came from, for the report; `None` when there was none.
    pub from: Option<PathBuf>,
    /// Whether there was a key at all. Without one nothing was counted and
    /// the verdict is [`State::Unchecked`], which says nothing about the
    /// stream.
    pub keyed: bool,
}

impl Outcome {
    pub fn state(&self) -> State {
        if self.keyed {
            self.tally.state()
        } else {
            State::Unchecked
        }
    }
}

/// The key, and what the stream has said so far.
pub struct Verifier {
    key: Vec<u8>,
    from: Option<PathBuf>,
    tally: Tally,
    /// The first access unit whose word did not verify, so the log says it
    /// once rather than once a frame.
    reported: bool,
}

impl Verifier {
    /// A verifier for `key`, read from `from`.
    pub fn new(key: Vec<u8>, from: Option<&Path>) -> Self {
        Self {
            key,
            from: from.map(Path::to_path_buf),
            tally: Tally::default(),
            reported: false,
        }
    }

    /// Where the key came from, for the report.
    pub fn source(&self) -> Option<&Path> {
        self.from.as_deref()
    }

    pub fn tally(&self) -> &Tally {
        &self.tally
    }

    /// The run, finished: the key is dropped with the verifier.
    pub fn finish(self) -> Outcome {
        Outcome {
            tally: self.tally,
            from: self.from,
            keyed: true,
        }
    }

    /// Count one access unit. `bytes` is the unit as it was read, which is
    /// what the digest covers.
    pub fn check(&mut self, unit: &AccessUnit, bytes: &[u8], index: u64) {
        self.tally.units += 1;
        let Some(extra) = unit.extra_data.as_ref() else {
            return;
        };
        if extra.evo_frame.is_none() {
            return;
        }
        self.tally.frames += 1;
        let sync = unit.major_sync_info.is_some();
        match extra.verify_evo_protection(bytes, &self.key) {
            EvoProtectionStatus::Absent => {}
            EvoProtectionStatus::Match if sync => {
                self.tally.sync_checked += 1;
                self.tally.sync_verified += 1;
            }
            EvoProtectionStatus::Match => {
                self.tally.checked += 1;
                self.tally.verified += 1;
            }
            EvoProtectionStatus::Mismatch { .. } if sync => self.tally.sync_checked += 1,
            status @ EvoProtectionStatus::Mismatch { .. } => {
                self.tally.checked += 1;
                self.tally.mismatched += 1;
                if !self.reported {
                    self.reported = true;
                    log::warn!(
                        "access unit {index}: the protection word is {}, the key gives {}",
                        hex(status.actual().unwrap_or_default()),
                        hex(status.expected().unwrap_or_default()),
                    );
                }
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_nothing_signed_is_unsigned_not_verified() {
        let tally = Tally {
            units: 100,
            frames: 100,
            ..Tally::default()
        };
        assert_eq!(tally.state(), State::Unsigned);
    }

    /// A plain 7.1 stream, or a read that stopped before the first frame:
    /// nothing carried metadata, so nothing could have been signed, and
    /// calling it unsigned would accuse a stream of something it never had.
    #[test]
    fn a_stream_without_frames_is_absent_not_unsigned() {
        let tally = Tally {
            units: 8,
            ..Tally::default()
        };
        assert_eq!(tally.state(), State::Absent);
        assert_eq!(tally.state().as_str(), "absent");
    }

    #[test]
    fn one_bad_word_in_a_signed_stream_is_a_mismatch() {
        let tally = Tally {
            units: 100,
            frames: 100,
            checked: 100,
            verified: 99,
            mismatched: 1,
            ..Tally::default()
        };
        assert_eq!(tally.state(), State::Mismatch);
        assert_eq!(tally.state().as_str(), "mismatch");
    }

    /// Measured on a commercial disc: every frame in an ordinary unit
    /// verifies, every frame in a major-sync unit does not. That is not a
    /// re-encode.
    #[test]
    fn failures_confined_to_major_sync_units_leave_the_stream_verified() {
        let tally = Tally {
            units: 72_000,
            frames: 1_877,
            checked: 1_836,
            verified: 1_836,
            mismatched: 0,
            sync_checked: 41,
            sync_verified: 0,
        };
        assert_eq!(tally.state(), State::Verified);
    }

    /// A failure in an ordinary unit is a mismatch whatever the major-sync
    /// units say.
    #[test]
    fn one_failure_in_an_ordinary_unit_is_a_mismatch_even_beside_good_syncs() {
        let tally = Tally {
            units: 480,
            frames: 13,
            checked: 11,
            verified: 10,
            mismatched: 1,
            sync_checked: 2,
            sync_verified: 2,
        };
        assert_eq!(tally.state(), State::Mismatch);
    }

    /// Only failing major-sync words: nothing that can be judged.
    #[test]
    fn only_failing_major_sync_words_are_absent() {
        let tally = Tally {
            units: 8,
            frames: 1,
            sync_checked: 1,
            ..Tally::default()
        };
        assert_eq!(tally.state(), State::Absent);
    }

    #[test]
    fn every_word_verifying_is_a_verified_stream() {
        let tally = Tally {
            units: 100,
            frames: 100,
            checked: 100,
            verified: 100,
            mismatched: 0,
            ..Tally::default()
        };
        assert_eq!(tally.state(), State::Verified);
    }
}
