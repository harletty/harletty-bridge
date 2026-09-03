// SPDX-License-Identifier: Apache-2.0

use super::joc::{JocObjectDecoderState, JocObjectMatrices};
use super::metadata::{JocPayload, MetadataParseState, OamdPayload, ParsedEmdfPayloadData};
use super::qmf::QMF_RECONSTRUCTION_DELAY;
use super::syncframe::{
    AccessUnitInfo, AuxDataDecodeState, CoreDecodeState, ParseError,
    decode_core_pcm_frame_with_state, inspect_access_unit_with_metadata_state,
    inspect_legacy_ac3_access_unit,
};
use crate::BedChannel;

/// How much later than the core an object channel arrives, in samples.
///
/// The objects are reconstructed in the hybrid QMF domain; the core PCM never
/// enters that filter bank, so the objects trail it by exactly the bank's own
/// analysis-to-synthesis delay — 577 samples, 12 ms at 48 kHz. That figure
/// belongs to the filter bank and is measured there rather than restated here,
/// by the impulse round-trip test that pins `QMF_RECONSTRUCTION_DELAY`.
///
/// This matters because an object frame is not purely objects: the whole core
/// bed is carried across beside them, untouched by the reconstruction. The LFE
/// is where that always shows, since JOC never reconstructs it and it is the one
/// bed channel still there to be mixed with the objects however the frame is
/// presented — but the offset is on every carried channel, not only that one.
/// Untouched, the bed arrives 12 ms ahead of every object it belongs with,
/// splitting each transient into two arrivals. `CoreAlignmentDelay`, below, is
/// what closes that gap.
pub const JOC_LATENCY_SAMPLES: usize = QMF_RECONSTRUCTION_DELAY;

#[derive(Debug, Clone, PartialEq)]
/// Decoded core channel PCM for one access unit.
pub struct CorePcmFrame {
    pub sample_rate: u32,
    pub fullband_channel_order: Vec<BedChannel>,
    pub fullband_channels: Vec<Vec<f32>>,
    pub lfe_channel: Option<Vec<f32>>,
}

impl CorePcmFrame {
    /// Number of samples carried by each channel in this frame.
    pub fn samples_per_channel(&self) -> usize {
        self.fullband_channels
            .first()
            .map(|channel| channel.len())
            .or_else(|| self.lfe_channel.as_ref().map(Vec::len))
            .unwrap_or(0)
    }

    /// Total channel count including the optional LFE channel.
    pub fn total_channels(&self) -> usize {
        self.fullband_channels.len() + usize::from(self.lfe_channel.is_some())
    }
}

/// Speaker positions a dependent-substream `chanmap` carries, in coded-channel
/// order (MSB→LSB; pair bits emit left then right). Only the bits that map to a
/// concrete bed position are emitted — enough for the common 5.1/7.1 channel
/// extensions (e.g. 0x1A00 = Ls, Rs, Lrs/Rrs).
pub fn dependent_chanmap_positions(chanmap: u16) -> Vec<BedChannel> {
    use BedChannel::*;
    const TABLE: &[(u16, &[BedChannel])] = &[
        (1 << 15, &[FrontLeft]),
        (1 << 14, &[Center]),
        (1 << 13, &[FrontRight]),
        (1 << 12, &[SurroundLeft]),
        (1 << 11, &[SurroundRight]),
        (1 << 9, &[RearLeft, RearRight]), // Lrs/Rrs (back surrounds)
        (1 << 8, &[RearCenter]),          // Cs
        (1 << 5, &[WideLeft, WideRight]), // Lw/Rw
    ];
    let mut out = Vec::new();
    for &(bit, chans) in TABLE {
        if chanmap & bit != 0 {
            out.extend_from_slice(chans);
        }
    }
    out
}

/// Merge a decoded core (5.1) with its dependent E-AC-3 substream — the discrete
/// surround/back channels of a 7.1 extension — into one bed, placing the
/// dependent's channels by its `chanmap`. Returns `None` (caller falls back to
/// the core alone) when the dependent can't be decoded or doesn't line up.
pub fn merge_core_with_dependent(
    dependent_decoder: &mut PcmDecoder,
    core: &CorePcmFrame,
    dependent_frame: &[u8],
) -> Option<CorePcmFrame> {
    let dep = dependent_decoder.push_access_unit(dependent_frame).ok()?;
    let positions = dependent_chanmap_positions(dep.info.dependent_channel_map?);
    let dep_pcm = dep.pcm;
    if positions.len() != dep_pcm.fullband_channels.len()
        || dep_pcm.samples_per_channel() != core.samples_per_channel()
    {
        return None;
    }

    // Start from the core's fullband channels, then overlay the dependent's:
    // replace a position the core also carried (discrete side surrounds) or
    // append a new one (the back pair).
    let mut order: Vec<BedChannel> = core.fullband_channel_order.clone();
    let mut chans: Vec<Vec<f32>> = core.fullband_channels.clone();
    for (i, &pos) in positions.iter().enumerate() {
        match order.iter().position(|&b| b == pos) {
            Some(idx) => chans[idx] = dep_pcm.fullband_channels[i].clone(),
            None => {
                order.push(pos);
                chans.push(dep_pcm.fullband_channels[i].clone());
            }
        }
    }

    Some(CorePcmFrame {
        sample_rate: core.sample_rate,
        fullband_channel_order: order,
        fullband_channels: chans,
        lfe_channel: core.lfe_channel.clone(),
    })
}

#[derive(Debug, Clone, PartialEq)]
/// Object-audio PCM plus the metadata that was active for the same access unit.
///
/// `object_channels` only covers dynamic objects. Bed channels remain in [`CorePcmFrame`].
pub struct ObjectPcmFrame {
    pub core: CorePcmFrame,
    pub object_channels: Vec<Vec<f32>>,
    pub object_active: Vec<bool>,
    pub joc: JocPayload,
    pub oamd_payloads: Vec<(OamdPayload, Option<u16>)>,
}

impl ObjectPcmFrame {
    /// Number of samples carried by each decoded channel in this frame.
    pub fn samples_per_channel(&self) -> usize {
        self.core.samples_per_channel()
    }

    /// Number of dynamic object channels decoded for this frame.
    pub fn object_count(&self) -> usize {
        self.object_channels.len()
    }
}

#[derive(Debug, Clone, PartialEq)]
/// Result returned by [`PcmDecoder::push_access_unit`].
pub struct PcmPushResult {
    pub frames_seen: u64,
    pub info: AccessUnitInfo,
    pub pcm: CorePcmFrame,
}

#[derive(Debug, Clone, PartialEq)]
/// Result returned by [`ObjectPcmDecoder::push_access_unit`] when the frame contains object data.
pub struct ObjectPcmPushResult {
    pub frames_seen: u64,
    pub info: AccessUnitInfo,
    pub pcm: ObjectPcmFrame,
}

#[derive(Debug)]
/// Stateful decoder for the core channel PCM path.
///
/// This decoder keeps both bitstream syntax state and cross-frame metadata state, so callers must
/// preserve frame order and call [`PcmDecoder::reset`] after discontinuities.
pub struct PcmDecoder {
    frames_seen: u64,
    aux_state: AuxDataDecodeState,
    core_state: CoreDecodeState,
    metadata_state: MetadataParseState,
    debug_log_level: log::Level,
}

impl Default for PcmDecoder {
    fn default() -> Self {
        Self {
            frames_seen: 0,
            aux_state: AuxDataDecodeState::default(),
            core_state: CoreDecodeState::default(),
            metadata_state: MetadataParseState::default(),
            debug_log_level: log::Level::Debug,
        }
    }
}

impl PcmDecoder {
    /// Create a fresh PCM decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset all cross-frame decode state.
    pub fn reset(&mut self) {
        self.frames_seen = 0;
        self.reset_decode_state();
    }

    /// Reset the cross-frame bitstream state (IMDCT overlap-add delay, block
    /// syntax, metadata sequencing) without touching `frames_seen`. A failed
    /// decode can leave this state partially mutated, so it must never be
    /// carried into the next frame.
    fn reset_decode_state(&mut self) {
        self.aux_state.reset();
        self.core_state.reset();
        self.metadata_state.reset();
    }

    /// Number of access units accepted since the last reset.
    pub fn frames_seen(&self) -> u64 {
        self.frames_seen
    }

    /// Whether the most recently decoded block had Spectral Extension
    /// active (`spxinu == 1`). Useful for picking content that exercises
    /// the SPX synthesis path.
    pub fn last_spx_in_use(&self) -> bool {
        self.core_state.spx_in_use_snapshot()
    }

    /// Per-channel SPX participation flags (`chinspx[ch]`) at the end of
    /// the most recently decoded block. Empty before the first frame.
    pub fn last_chinspx(&self) -> &[bool] {
        self.core_state.chinspx_snapshot()
    }

    /// Configure the metadata / aux diagnostic log level for this decoder instance.
    pub fn set_debug_log_level(&mut self, level: log::Level) {
        self.debug_log_level = level;
    }

    fn apply_debug_log_level(&self) {
        super::metadata::set_metadata_log_level(self.debug_log_level);
        super::syncframe::set_aux_log_level(self.debug_log_level);
    }

    /// Decode one complete access unit into core PCM.
    ///
    /// On `Err`, the decoder's cross-frame state is reset: a failed decode may
    /// have left it partially mutated, and reusing it would corrupt every
    /// following frame (stale IMDCT overlap-add delay, stale coupling state).
    pub fn push_access_unit(&mut self, access_unit: &[u8]) -> Result<PcmPushResult, ParseError> {
        self.push_access_unit_inner(access_unit).inspect_err(|_| {
            self.reset_decode_state();
        })
    }

    fn push_access_unit_inner(&mut self, access_unit: &[u8]) -> Result<PcmPushResult, ParseError> {
        self.apply_debug_log_level();
        let info = inspect_access_unit_with_metadata_state(
            access_unit,
            &mut self.metadata_state,
            Some(&mut self.aux_state),
        )?;

        if access_unit.len() < info.frame_size {
            return Err(ParseError::TruncatedFrame {
                expected: info.frame_size,
                available: access_unit.len(),
            });
        }
        if access_unit.len() != info.frame_size {
            return Err(ParseError::TrailingData {
                expected: info.frame_size,
                provided: access_unit.len(),
            });
        }

        let pcm = decode_core_pcm_frame_with_state(access_unit, &info, &mut self.core_state)?;
        self.frames_seen += 1;
        Ok(PcmPushResult {
            frames_seen: self.frames_seen,
            info,
            pcm,
        })
    }

    /// Decode one complete legacy AC-3 syncframe into core PCM.
    ///
    /// On `Err`, the decoder's cross-frame state is reset (see
    /// [`PcmDecoder::push_access_unit`]).
    pub fn push_legacy_ac3_access_unit(
        &mut self,
        access_unit: &[u8],
    ) -> Result<PcmPushResult, ParseError> {
        self.push_legacy_ac3_access_unit_inner(access_unit)
            .inspect_err(|_| {
                self.reset_decode_state();
            })
    }

    fn push_legacy_ac3_access_unit_inner(
        &mut self,
        access_unit: &[u8],
    ) -> Result<PcmPushResult, ParseError> {
        self.apply_debug_log_level();
        let info = inspect_legacy_ac3_access_unit(access_unit)?;

        if access_unit.len() < info.frame_size {
            return Err(ParseError::TruncatedFrame {
                expected: info.frame_size,
                available: access_unit.len(),
            });
        }
        if access_unit.len() != info.frame_size {
            return Err(ParseError::TrailingData {
                expected: info.frame_size,
                provided: access_unit.len(),
            });
        }

        let pcm = decode_core_pcm_frame_with_state(access_unit, &info, &mut self.core_state)?;
        self.frames_seen += 1;
        Ok(PcmPushResult {
            frames_seen: self.frames_seen,
            info,
            pcm,
        })
    }
}

#[derive(Debug)]
/// Stateful decoder for dynamic object PCM.
///
/// This is the highest-level decoder before rendering. It returns `Ok(None)` for frames that do
/// not carry the required dynamic-object payloads.
///
/// Every frame it does return sits on one clock: the core bed is held back by
/// [`JOC_LATENCY_SAMPLES`] so that it lines up with the objects, which leave the
/// reconstruction that much later than the core entered it. That delay only runs
/// while objects are being reconstructed, so crossing into or out of object
/// decoding — a stream that stops carrying JOC, or a caller alternating between
/// this decoder and a plain [`PcmDecoder`] — steps the bed by 577 samples at the
/// boundary, once per crossing. That is deliberate and is not smoothed here; the
/// reasoning is on `CoreAlignmentDelay`.
pub struct ObjectPcmDecoder {
    frames_seen: u64,
    aux_state: AuxDataDecodeState,
    core_state: CoreDecodeState,
    joc_state: JocObjectDecoderState,
    metadata_state: MetadataParseState,
    core_delay: CoreAlignmentDelay,
    joc_input_core: Option<CorePcmFrame>,
    /// `joc_sequence_counter` of the last JOC frame reconstructed, or `None`
    /// while the decoder is cold. See [`continues_joc_sequence`].
    joc_sequence: Option<u16>,
    debug_log_level: log::Level,
}

/// Whether a JOC frame carrying `sequence_counter` continues the frame that
/// carried `previous`.
///
/// Clause 6.3.3.3 gives the counter one job: "The frame sequence counter is
/// used for splice detection in the decoder." It increments by one per frame up
/// to 1 023 and then restarts at 1, and "the value 0 indicates the first frame
/// in the bitstream or the first frame after a splice". So the successor of the
/// last counter seen is the only value that means one continuous programme, and
/// everything else - a zero, a repeat, a gap forwards, a jump backwards - means
/// the frames either side of this one were never adjacent, whatever the decoder
/// is still holding from before.
///
/// Note that 1 023 is followed by 1 rather than 0: the wrap skips the value the
/// clause reserves for a splice, so a stream that has been running for six hours
/// is not mistaken for one that has just been cut.
fn continues_joc_sequence(previous: Option<u16>, sequence_counter: u16) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    if sequence_counter == 0 {
        return false;
    }
    let expected = if previous == 1023 { 1 } else { previous + 1 };
    sequence_counter == expected
}

/// The JOC payload of an access unit, if it carries one.
fn find_joc_payload(info: &AccessUnitInfo) -> Option<JocPayload> {
    info.payloads().find_map(|payload| match &payload.parsed {
        ParsedEmdfPayloadData::Joc(joc) => Some(joc.clone()),
        _ => None,
    })
}

/// Holds the core PCM back by [`JOC_LATENCY_SAMPLES`] so that everything in an
/// [`ObjectPcmFrame`] sits on one clock.
///
/// Each channel keeps the tail of the previous frame, which becomes the head of
/// the next one. After a reset the tails start as silence, matching the QMF's
/// own history: the objects begin with the same 577 samples of filter warm-up.
///
/// The tails are addressed by position, so they only mean anything while the
/// core keeps the same shape. The remembered layout is what says whether they
/// still do — without it, a core dropping from `[FL, C, FR]` to `[FL, FR]` would
/// hand the old centre tail to the new right front, and an LFE that went away
/// and came back would open on 12 ms of whatever was playing when it left.
///
/// # Crossing into and out of object decoding
///
/// The delay only runs on frames that carry objects. A stream that stops
/// reconstructing objects — JOC disappearing mid-stream, or a caller switching
/// between [`ObjectPcmDecoder`] and a plain [`PcmDecoder`] — emits its next core
/// undelayed, so the bed jumps 577 samples (12 ms at 48 kHz) earlier at that
/// boundary, and 577 samples later on the way back in. Both are audible splices:
/// a discontinuity in every bed channel at the frame where the path changes.
///
/// Nothing here smooths that, deliberately. Holding the delay open across the
/// boundary would carry object-era tails into a frame the objects are gone from,
/// and the tails only mean anything against the reconstruction that filled them;
/// cross-fading across it would need a frame of lookahead this decode path does
/// not have. The boundary is also rare — a JOC stream carries JOC throughout —
/// and it costs one splice per crossing rather than a standing per-frame error,
/// where the misalignment this delay exists to remove was on every object frame.
/// That trade is the reason it is left alone; this note is here so the next
/// person to want it smoothed finds the reasoning rather than the symptom.
#[derive(Debug, Clone, Default, PartialEq)]
struct CoreAlignmentDelay {
    sample_rate: Option<u32>,
    fullband_channel_order: Vec<BedChannel>,
    lfe_present: bool,
    fullband_tails: Vec<Vec<f32>>,
    lfe_tail: Vec<f32>,
}

impl CoreAlignmentDelay {
    fn reset(&mut self) {
        self.sample_rate = None;
        self.fullband_channel_order.clear();
        self.lfe_present = false;
        self.fullband_tails.clear();
        self.lfe_tail.clear();
    }

    /// Whether the tails on hand belong to the layout `core` is in.
    ///
    /// This is the same test [`CoreDecodeState::reconfigure`] applies to its own
    /// cross-frame state, one step finer: it compares the speaker order rather
    /// than only the channel count, so a reordering that keeps the count also
    /// counts as a new layout.
    fn belongs_to(&self, core: &CorePcmFrame) -> bool {
        self.sample_rate == Some(core.sample_rate)
            && self.fullband_channel_order == core.fullband_channel_order
            && self.lfe_present == core.lfe_channel.is_some()
    }

    /// Return `core` shifted later by the JOC reconstruction latency.
    ///
    /// The result is a new frame rather than an edit in place: the caller still
    /// needs the undelayed one, which is what the objects were reconstructed
    /// from. See [`ObjectPcmDecoder::take_joc_input_core`].
    fn delayed(&mut self, core: &CorePcmFrame) -> CorePcmFrame {
        if !self.belongs_to(core) {
            self.reset();
            self.sample_rate = Some(core.sample_rate);
            self.fullband_channel_order
                .clone_from(&core.fullband_channel_order);
            self.lfe_present = core.lfe_channel.is_some();
        }

        self.fullband_tails
            .resize(core.fullband_channels.len(), Vec::new());
        let fullband_channels = core
            .fullband_channels
            .iter()
            .zip(self.fullband_tails.iter_mut())
            .map(|(channel, tail)| delay_channel(channel, tail))
            .collect();
        let lfe_channel = core
            .lfe_channel
            .as_ref()
            .map(|lfe| delay_channel(lfe, &mut self.lfe_tail));

        CorePcmFrame {
            sample_rate: core.sample_rate,
            fullband_channel_order: core.fullband_channel_order.clone(),
            fullband_channels,
            lfe_channel,
        }
    }
}

/// Delay one channel by [`JOC_LATENCY_SAMPLES`], carrying the samples that fall
/// off the end into `tail` for the next frame. Correct for frames shorter than
/// the delay, which is why the split is expressed against the joined length.
fn delay_channel(channel: &[f32], tail: &mut Vec<f32>) -> Vec<f32> {
    if tail.len() != JOC_LATENCY_SAMPLES {
        tail.clear();
        tail.resize(JOC_LATENCY_SAMPLES, 0.0);
    }

    let mut joined = Vec::with_capacity(tail.len() + channel.len());
    joined.extend_from_slice(tail);
    joined.extend_from_slice(channel);

    let carried = joined.len() - JOC_LATENCY_SAMPLES;
    tail.clear();
    tail.extend_from_slice(&joined[carried..]);
    joined.truncate(channel.len());
    joined
}

impl Default for ObjectPcmDecoder {
    fn default() -> Self {
        Self {
            frames_seen: 0,
            aux_state: AuxDataDecodeState::default(),
            core_state: CoreDecodeState::default(),
            joc_state: JocObjectDecoderState::default(),
            metadata_state: MetadataParseState::default(),
            core_delay: CoreAlignmentDelay::default(),
            joc_input_core: None,
            joc_sequence: None,
            debug_log_level: log::Level::Debug,
        }
    }
}

impl ObjectPcmDecoder {
    /// Create a fresh object decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset all cross-frame decode state.
    pub fn reset(&mut self) {
        self.frames_seen = 0;
        self.reset_decode_state();
    }

    /// Reset the cross-frame bitstream state without touching `frames_seen`
    /// (see [`PcmDecoder`]'s private counterpart).
    fn reset_decode_state(&mut self) {
        self.aux_state.reset();
        self.core_state.reset();
        self.joc_state.reset();
        self.metadata_state.reset();
        self.core_delay.reset();
        self.joc_input_core = None;
        self.joc_sequence = None;
    }

    /// Tell the decoder that a complete presentation carried no JOC.
    ///
    /// The sequence counter cannot see this. It increments once per JOC frame,
    /// so a programme that stops carrying JOC and later resumes can come back
    /// with the successor of the counter this decoder last saw, and read as
    /// continuous - while the reconstruction slept through an entire interval
    /// and its filter banks still hold the audio from before the gap.
    ///
    /// Only the caller knows where a presentation ends, which is why this is
    /// asked for rather than inferred: a single access unit without JOC is
    /// routinely the independent half of a pair whose dependent carries it, and
    /// resetting on that would cold-start every frame of a healthy 7.1-core
    /// stream. Call this once a presentation has been *resolved* as carrying no
    /// JOC - a standalone core emitted, or a pair completed with neither half
    /// carrying a payload - and never on a half-seen one.
    ///
    /// A caller that never calls it keeps the previous behaviour exactly.
    pub fn note_non_joc_presentation(&mut self) {
        self.reset_decode_state();
    }

    /// Take the JOC payload to reconstruct from, cold-starting first when its
    /// sequence counter says this frame does not follow the last one decoded.
    ///
    /// The counter only becomes readable once the frame has been parsed, and
    /// that parse has already advanced the OAMD and auxiliary syntax state it
    /// inherited from whatever was playing before the cut. So a splice is not
    /// finished by clearing the audio history: the frame that exposed it has to
    /// be read again from cleared state, or the first frame of the new
    /// programme is still interpreted against the last frame of the old one.
    /// That second parse costs nothing on a continuous stream, which is every
    /// frame but the one after a cut.
    fn joc_payload_for_reconstruction(
        &mut self,
        access_unit: &[u8],
        info: AccessUnitInfo,
        joc: JocPayload,
    ) -> Result<(AccessUnitInfo, JocPayload), ParseError> {
        let (info, joc) = if continues_joc_sequence(self.joc_sequence, joc.sequence_counter) {
            (info, joc)
        } else {
            // A cold decoder starting on its first frame is not a splice, and
            // the level this logs at is the caller's `set_debug_log_level` -
            // `Warn` or `Error` for the bridge - so announcing every ordinary
            // start would put a false fault in the player's log. Only a counter
            // that broke a run it was part of is worth a line.
            if let Some(previous) = self.joc_sequence {
                log::log!(
                    target: "starmine_ad::eac3dec::pcm",
                    self.debug_log_level,
                    "joc-splice previous={previous} counter={} - cold starting",
                    joc.sequence_counter,
                );
            }
            self.reset_decode_state();
            let info = inspect_access_unit_with_metadata_state(
                access_unit,
                &mut self.metadata_state,
                Some(&mut self.aux_state),
            )?;
            let joc =
                find_joc_payload(&info).ok_or(ParseError::InvalidHeader("joc-sequence-reparse"))?;
            (info, joc)
        };
        self.joc_sequence = Some(joc.sequence_counter);
        Ok((info, joc))
    }

    /// Number of access units accepted since the last reset.
    pub fn frames_seen(&self) -> u64 {
        self.frames_seen
    }

    /// Take the core PCM the most recent frame's objects were reconstructed
    /// from, before the alignment delay was applied.
    ///
    /// [`ObjectPcmFrame::core`] is held back by [`JOC_LATENCY_SAMPLES`] so that
    /// it lines up with the objects it ships beside, which makes it the wrong
    /// thing to feed back in as the JOC downmix for a following dependent
    /// substream: those objects would be reconstructed from a core that is
    /// already late, and then delayed a second time on the way out. Callers that
    /// pair an independent frame's core with a later dependent frame want this
    /// instead. `None` before the first object frame, after a reset, and on a
    /// second call before the next one.
    ///
    /// It is handed over rather than lent because nothing inside the decoder
    /// reads it: it is held for exactly one caller, which wants it by value, and
    /// a borrow would only make that caller copy a whole core to get it.
    pub fn take_joc_input_core(&mut self) -> Option<CorePcmFrame> {
        self.joc_input_core.take()
    }

    /// Cold-start the object reconstruction when the core changes shape.
    ///
    /// A layout, LFE-presence or sample-rate change invalidates every piece of
    /// cross-frame audio history at once — the QMF banks and interpolation
    /// matrices inside the reconstruction as much as [`CoreAlignmentDelay`]'s
    /// own tails. The delay notices on its own, but only once it is handed the
    /// frame, which is after the objects for that frame have already been
    /// reconstructed through banks holding the old configuration. Asking first
    /// is what keeps both sides starting cold on the same frame, and the frame
    /// itself on one clock.
    fn reset_history_if_reconfigured(&mut self, core: &CorePcmFrame) {
        if !self.core_delay.belongs_to(core) {
            self.joc_state.reset();
        }
    }

    /// Per-object subband matrices reconstructed for the most recent decoded frame.
    ///
    /// This is primarily useful for debugging and offline comparison tools.
    pub fn last_joc_matrices(&self) -> &JocObjectMatrices {
        self.joc_state.last_frame_matrices()
    }

    /// Configure the metadata / aux diagnostic log level for this decoder instance.
    pub fn set_debug_log_level(&mut self, level: log::Level) {
        self.debug_log_level = level;
    }

    fn apply_debug_log_level(&self) {
        super::metadata::set_metadata_log_level(self.debug_log_level);
        super::syncframe::set_aux_log_level(self.debug_log_level);
    }

    /// Decode one complete access unit into dynamic object PCM.
    ///
    /// Returns `Ok(None)` when the frame is valid E-AC-3 but does not contain the object payloads
    /// needed for this stage.
    ///
    /// On `Err`, the decoder's cross-frame state is reset (see
    /// [`PcmDecoder::push_access_unit`]).
    pub fn push_access_unit(
        &mut self,
        access_unit: &[u8],
    ) -> Result<Option<ObjectPcmPushResult>, ParseError> {
        self.push_access_unit_inner(access_unit).inspect_err(|_| {
            self.reset_decode_state();
        })
    }

    fn push_access_unit_inner(
        &mut self,
        access_unit: &[u8],
    ) -> Result<Option<ObjectPcmPushResult>, ParseError> {
        self.apply_debug_log_level();
        let info = inspect_access_unit_with_metadata_state(
            access_unit,
            &mut self.metadata_state,
            Some(&mut self.aux_state),
        )?;

        if access_unit.len() < info.frame_size {
            return Err(ParseError::TruncatedFrame {
                expected: info.frame_size,
                available: access_unit.len(),
            });
        }
        if access_unit.len() != info.frame_size {
            return Err(ParseError::TrailingData {
                expected: info.frame_size,
                provided: access_unit.len(),
            });
        }

        let Some(joc) = find_joc_payload(&info) else {
            return Ok(None);
        };
        let (info, joc) = self.joc_payload_for_reconstruction(access_unit, info, joc)?;

        let joc_input_core =
            decode_core_pcm_frame_with_state(access_unit, &info, &mut self.core_state)?;
        self.reset_history_if_reconfigured(&joc_input_core);
        let object_channels = self.joc_state.decode_frame(&joc_input_core, &joc)?;
        // Only now that the core has been through JOC as input can it be held
        // back to meet the objects coming out the other side. The undelayed one
        // is kept as it was: see `joc_input_core`.
        let core = self.core_delay.delayed(&joc_input_core);
        self.joc_input_core = Some(joc_input_core);
        let object_active = joc.objects.iter().map(|object| object.active).collect();
        let oamd_payloads = info
            .payloads()
            .filter_map(|payload| match &payload.parsed {
                ParsedEmdfPayloadData::Oamd(oamd) => {
                    Some((oamd.clone(), payload.info.sample_offset))
                }
                _ => None,
            })
            .collect();

        self.frames_seen += 1;
        Ok(Some(ObjectPcmPushResult {
            frames_seen: self.frames_seen,
            info,
            pcm: ObjectPcmFrame {
                core,
                object_channels,
                object_active,
                joc,
                oamd_payloads,
            },
        }))
    }

    /// Decode dynamic object PCM from a dependent access unit using externally decoded core PCM.
    ///
    /// On `Err`, the decoder's cross-frame state is reset (see
    /// [`PcmDecoder::push_access_unit`]).
    pub fn push_access_unit_with_core(
        &mut self,
        access_unit: &[u8],
        core: CorePcmFrame,
    ) -> Result<Option<ObjectPcmPushResult>, ParseError> {
        self.push_access_unit_with_core_inner(access_unit, core)
            .inspect_err(|_| {
                self.reset_decode_state();
            })
    }

    fn push_access_unit_with_core_inner(
        &mut self,
        access_unit: &[u8],
        joc_input_core: CorePcmFrame,
    ) -> Result<Option<ObjectPcmPushResult>, ParseError> {
        self.apply_debug_log_level();
        let info = inspect_access_unit_with_metadata_state(
            access_unit,
            &mut self.metadata_state,
            Some(&mut self.aux_state),
        )?;

        if access_unit.len() < info.frame_size {
            return Err(ParseError::TruncatedFrame {
                expected: info.frame_size,
                available: access_unit.len(),
            });
        }
        if access_unit.len() != info.frame_size {
            return Err(ParseError::TrailingData {
                expected: info.frame_size,
                provided: access_unit.len(),
            });
        }

        let Some(joc) = find_joc_payload(&info) else {
            return Ok(None);
        };
        let (info, joc) = self.joc_payload_for_reconstruction(access_unit, info, joc)?;

        self.reset_history_if_reconfigured(&joc_input_core);
        let object_channels = self.joc_state.decode_frame(&joc_input_core, &joc)?;
        // As in `push_access_unit_inner`: hold the core back to meet the
        // objects, once it has served as the reconstruction's input.
        let core = self.core_delay.delayed(&joc_input_core);
        self.joc_input_core = Some(joc_input_core);
        let object_active = joc.objects.iter().map(|object| object.active).collect();
        let oamd_payloads = info
            .payloads()
            .filter_map(|payload| match &payload.parsed {
                ParsedEmdfPayloadData::Oamd(oamd) => {
                    Some((oamd.clone(), payload.info.sample_offset))
                }
                _ => None,
            })
            .collect();

        self.frames_seen += 1;
        Ok(Some(ObjectPcmPushResult {
            frames_seen: self.frames_seen,
            info,
            pcm: ObjectPcmFrame {
                core,
                object_channels,
                object_active,
                joc,
                oamd_payloads,
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clause 6.3.3.3's counter, case by case.
    ///
    /// The successor of the last counter seen is the only value that continues
    /// a programme. A cold decoder continues nothing, whatever it is handed;
    /// zero is the clause's own splice marker and never continues, not even
    /// from 1 023; and the wrap runs 1 023 to 1, so the one value that would
    /// otherwise look like a wrap is the one that means a cut.
    #[test]
    fn joc_sequence_counter_continues_only_on_the_successor() {
        for (previous, counter, expected, what) in [
            (None, 0, false, "cold decoder, splice marker"),
            (None, 37, false, "cold decoder, mid-stream counter"),
            (
                Some(0),
                1,
                true,
                "first frame after a splice, then its successor",
            ),
            (Some(41), 42, true, "ordinary successor"),
            (Some(1022), 1023, true, "successor up to the maximum"),
            (Some(1023), 1, true, "the wrap skips zero"),
            (Some(1023), 0, false, "zero is a splice even at the wrap"),
            (Some(41), 0, false, "splice marker"),
            (Some(41), 43, false, "a frame is missing"),
            (Some(41), 41, false, "the same frame twice"),
            (Some(900), 12, false, "jump backwards"),
        ] {
            assert_eq!(
                continues_joc_sequence(previous, counter),
                expected,
                "{previous:?} -> {counter} ({what})"
            );
        }
    }

    /// A ramp is the easiest signal to read a delay off: sample `n` carries the
    /// value `n`, so wherever the ramp starts is the delay.
    #[test]
    fn the_core_is_held_back_by_exactly_the_joc_latency() {
        let mut delay = CoreAlignmentDelay::default();
        let frame_len = 1536;
        let mut seen: Vec<f32> = Vec::new();

        for frame in 0..3 {
            let base = frame * frame_len;
            let core = CorePcmFrame {
                sample_rate: 48_000,
                fullband_channel_order: vec![BedChannel::FrontLeft],
                fullband_channels: vec![(0..frame_len).map(|n| (base + n) as f32).collect()],
                lfe_channel: Some((0..frame_len).map(|n| (base + n) as f32).collect()),
            };
            let core = delay.delayed(&core);
            // The LFE is the channel that actually reaches the mix; it must be
            // shifted the same way as everything else in the frame.
            assert_eq!(
                core.lfe_channel.as_deref(),
                Some(&core.fullband_channels[0][..])
            );
            seen.extend_from_slice(&core.fullband_channels[0]);
        }

        assert!(seen[..JOC_LATENCY_SAMPLES].iter().all(|&s| s == 0.0));
        for (n, &sample) in seen[JOC_LATENCY_SAMPLES..].iter().enumerate() {
            assert_eq!(sample, n as f32, "sample {n} landed in the wrong slot");
        }
    }

    /// After a seek the objects restart with a cold filter bank, so the core
    /// has to restart with a cold delay - otherwise the first frames of the new
    /// position would be prefixed with audio from the old one.
    #[test]
    fn a_reset_clears_the_carried_tail() {
        let mut delay = CoreAlignmentDelay::default();
        let first = CorePcmFrame {
            sample_rate: 48_000,
            fullband_channel_order: vec![],
            fullband_channels: vec![],
            lfe_channel: Some(vec![1.0; 1536]),
        };
        delay.delayed(&first);
        delay.reset();

        let second = CorePcmFrame {
            sample_rate: 48_000,
            fullband_channel_order: vec![],
            fullband_channels: vec![],
            lfe_channel: Some(vec![2.0; 1536]),
        };
        let lfe = delay.delayed(&second).lfe_channel.unwrap();
        assert!(lfe[..JOC_LATENCY_SAMPLES].iter().all(|&s| s == 0.0));
        assert!(lfe[JOC_LATENCY_SAMPLES..].iter().all(|&s| s == 2.0));
    }

    /// The tails are addressed by position, so a layout change silently
    /// repoints them: dropping the centre channel would slide the old centre
    /// tail onto the new right front. A cold start is the only answer that
    /// cannot play one channel's audio out of another.
    #[test]
    fn a_narrower_core_starts_its_tails_from_silence() {
        let mut delay = CoreAlignmentDelay::default();
        let wide = CorePcmFrame {
            sample_rate: 48_000,
            fullband_channel_order: vec![
                BedChannel::FrontLeft,
                BedChannel::Center,
                BedChannel::FrontRight,
            ],
            fullband_channels: vec![vec![1.0; 1536], vec![2.0; 1536], vec![3.0; 1536]],
            lfe_channel: None,
        };
        delay.delayed(&wide);

        let narrow = CorePcmFrame {
            sample_rate: 48_000,
            fullband_channel_order: vec![BedChannel::FrontLeft, BedChannel::FrontRight],
            fullband_channels: vec![vec![0.0; 1536], vec![0.0; 1536]],
            lfe_channel: None,
        };
        let narrow = delay.delayed(&narrow);
        for (index, channel) in narrow.fullband_channels.iter().enumerate() {
            assert!(
                channel.iter().all(|&sample| sample == 0.0),
                "channel {index} opened on the previous layout's audio",
            );
        }
    }

    /// Same hazard on the channel that actually reaches the mix: a frame
    /// without LFE leaves `lfe_tail` holding the last frame that had one, and
    /// nothing clears it before the LFE comes back.
    #[test]
    fn an_lfe_that_comes_back_starts_from_silence() {
        let mut delay = CoreAlignmentDelay::default();
        let with_lfe = CorePcmFrame {
            sample_rate: 48_000,
            fullband_channel_order: vec![BedChannel::FrontLeft],
            fullband_channels: vec![vec![1.0; 1536]],
            lfe_channel: Some(vec![1.0; 1536]),
        };
        delay.delayed(&with_lfe);

        let without_lfe = CorePcmFrame {
            lfe_channel: None,
            ..with_lfe.clone()
        };
        delay.delayed(&without_lfe);

        let returned = CorePcmFrame {
            lfe_channel: Some(vec![0.0; 1536]),
            ..with_lfe
        };
        let lfe = delay.delayed(&returned).lfe_channel.unwrap();
        assert!(
            lfe.iter().all(|&sample| sample == 0.0),
            "the LFE came back carrying the audio it left with",
        );
    }

    /// A sample-rate change is the third way the tails stop meaning what they
    /// meant, and the one `CoreDecodeState::reconfigure` already resets on.
    #[test]
    fn a_sample_rate_change_starts_the_tails_from_silence() {
        let mut delay = CoreAlignmentDelay::default();
        let at_48k = CorePcmFrame {
            sample_rate: 48_000,
            fullband_channel_order: vec![BedChannel::FrontLeft],
            fullband_channels: vec![vec![1.0; 1536]],
            lfe_channel: None,
        };
        delay.delayed(&at_48k);

        let at_44k = CorePcmFrame {
            sample_rate: 44_100,
            fullband_channels: vec![vec![0.0; 1536]],
            ..at_48k
        };
        let at_44k = delay.delayed(&at_44k);
        assert!(at_44k.fullband_channels[0].iter().all(|&s| s == 0.0));
    }

    /// A presentation the object decoder sat out breaks the run, even though
    /// the counter cannot see it.
    ///
    /// The counter increments per JOC frame, so a programme that stops carrying
    /// JOC and resumes can come back with the successor of the last value seen
    /// and read as continuous - while the filter banks still hold the audio
    /// from before the gap. The caller reports the gap, and reporting it has to
    /// leave the decoder as cold as a fresh one: warm banks here would put the
    /// old programme's 577 samples under the first frame of the new one, and a
    /// retained counter would let the frame after that continue from it.
    #[test]
    fn a_reported_non_joc_presentation_cold_starts_the_reconstruction() {
        use super::super::metadata::{JocObject, JocObjectData};

        let core = CorePcmFrame {
            sample_rate: 48_000,
            fullband_channel_order: vec![BedChannel::FrontLeft, BedChannel::FrontRight],
            fullband_channels: vec![vec![0.5; 64], vec![-0.5; 64]],
            lfe_channel: Some(vec![0.25; 64]),
        };
        let joc = JocPayload {
            downmix_config: 0,
            channel_count: 2,
            object_count: 1,
            gain: 1.0,
            sequence_counter: 41,
            objects: vec![JocObject {
                active: true,
                bands_index: Some(0),
                bands: 1,
                sparse_coded: false,
                quantization_table: Some(0),
                steep_slope: false,
                data_points: 1,
                timeslot_offsets: Vec::new(),
                data: Some(JocObjectData::Dense {
                    matrices: vec![vec![vec![1], vec![1]]],
                }),
            }],
        };

        let mut decoder = ObjectPcmDecoder::new();
        decoder
            .joc_state
            .decode_frame(&core, &joc)
            .expect("a frame to warm the banks");
        decoder.core_delay.delayed(&core);
        decoder.joc_sequence = Some(joc.sequence_counter);
        assert!(
            !decoder.joc_state.is_cold(),
            "the reconstruction should be holding history after a frame",
        );

        decoder.note_non_joc_presentation();

        assert!(
            decoder.joc_state.is_cold(),
            "a reported no-JOC presentation left the filter banks warm",
        );
        assert!(
            decoder.joc_sequence.is_none(),
            "a reported no-JOC presentation left the counter behind, so counter 42 \
             would still read as continuing counter 41 across the gap",
        );
    }

    /// The delay's tails and the reconstruction's filter banks are two halves
    /// of the same history, and the objects are reconstructed before the delay
    /// ever sees the frame. If only the tails cold-start, the frame that
    /// crosses the change carries a core with 577 fresh silent samples against
    /// objects still coloured by the previous configuration - which is exactly
    /// the one-clock invariant the delay exists to hold.
    #[test]
    fn a_core_reconfiguration_cold_starts_the_object_reconstruction() {
        use super::super::metadata::{JocObject, JocObjectData};

        let at_48k = CorePcmFrame {
            sample_rate: 48_000,
            fullband_channel_order: vec![BedChannel::FrontLeft, BedChannel::FrontRight],
            fullband_channels: vec![vec![0.5; 64], vec![-0.5; 64]],
            lfe_channel: Some(vec![0.25; 64]),
        };
        let joc = JocPayload {
            downmix_config: 0,
            channel_count: 2,
            object_count: 1,
            gain: 1.0,
            sequence_counter: 0,
            objects: vec![JocObject {
                active: true,
                bands_index: Some(0),
                bands: 1,
                sparse_coded: false,
                quantization_table: Some(0),
                steep_slope: false,
                data_points: 1,
                timeslot_offsets: Vec::new(),
                data: Some(JocObjectData::Dense {
                    matrices: vec![vec![vec![1], vec![1]]],
                }),
            }],
        };

        let mut decoder = ObjectPcmDecoder::new();
        decoder
            .joc_state
            .decode_frame(&at_48k, &joc)
            .expect("a frame to warm the banks");
        decoder.core_delay.delayed(&at_48k);
        assert!(
            !decoder.joc_state.is_cold(),
            "the reconstruction should be holding history after a frame",
        );

        let at_44k = CorePcmFrame {
            sample_rate: 44_100,
            ..at_48k
        };
        decoder.reset_history_if_reconfigured(&at_44k);
        assert!(
            decoder.joc_state.is_cold(),
            "a sample-rate change left the filter banks warm",
        );
    }

    #[test]
    fn chanmap_0x1a00_maps_to_side_and_back_surrounds() {
        // The common 7.1 channel-extension chanmap (Ls, Rs, Lrs/Rrs).
        assert_eq!(
            dependent_chanmap_positions(0x1A00),
            vec![
                BedChannel::SurroundLeft,
                BedChannel::SurroundRight,
                BedChannel::RearLeft,
                BedChannel::RearRight,
            ]
        );
    }
}
