//! One codec family as a bridge: the [`FamilyPipeline`] a family crate
//! implements, and the [`PluginBridge`] that turns it into the
//! [`FormatBridge`] a host loads.
//!
//! What every bridge does whatever it decodes lives here, once: the panic
//! guard around `push_packet`, the reset a codec path asks for
//! ([`AfterPush`]), the raw codec lock (declared through `input_codec`,
//! sniffed, or a fallback for one packet), IEC 61937 burst types the family
//! does not decode, and the `log_level` and `presentation` configuration
//! keys. A family only decodes and describes its stream.
//!
//! Statically dispatched: a plugin is `PluginBridge<OneFamily>`, and nothing
//! on the packet path goes through a vtable but the host's own call.

use abi_stable::std_types::{RSlice, RStr, RString, RVec};
use bridge_api::{
    FormatBridge, RChannelPose, RChannelTag, RCoordinateFormat, RInputTransport, RProbe,
    RPushResult, RSourceFamily, RVbapCartesianDefaults, RVbapTableMode,
};

use crate::logging::{RepeatCounter, bridge_diag_log, bridge_log, panic_message};
use crate::shared::{AfterPush, SharedState};

/// TrueHD's presentation count (`truehd::process::MAX_PRESENTATIONS`): a
/// family without presentations takes the values a TrueHD bridge takes, so a
/// host that sends `presentation` to every bridge it loads gets the same
/// answer from each.
pub const TRUEHD_PRESENTATIONS: u8 = 4;

/// One codec family's decode paths and stream description, as a
/// [`PluginBridge`] drives them.
pub trait FamilyPipeline: Send + Sync + 'static {
    /// The raw-transport codecs this family tells apart (TrueHD from E-AC-3
    /// in the Dolby family; a single one elsewhere).
    type Codec: Copy + Eq + core::fmt::Debug + Send + Sync + 'static;

    /// The `input_codec` names a host routes to this family by, lower case
    /// (`BridgeLib::input_codecs`): each one [`Self::input_codec`] takes.
    const INPUT_CODECS: &'static [&'static str];

    fn new(shared: &SharedState) -> Self;

    /// `BridgeLib::probe` for the raw transport: where, if anywhere, a
    /// stream of this family starts in `data`, validated by the family's own
    /// criteria within its bounded header length ([`crate::probe`]).
    /// Stateless; the host calls it before any instance sees the bytes.
    fn probe_raw(data: &[u8]) -> RProbe;

    /// The codec `configure("input_codec", name)` selects, `None` for a name
    /// this family does not take. `auto` and the empty name are the
    /// bridge's: they clear the declared codec.
    fn input_codec(name: &str) -> Option<Self::Codec>;

    /// The codec a raw packet opens with, judged at its first byte, when no
    /// codec is locked: it locks the stream until the next reset.
    fn sniff(data: &[u8]) -> Option<Self::Codec>;

    /// Where a raw packet nothing was sniffed in goes while no codec is
    /// locked: decoded as this codec, for this packet only, so a later packet
    /// can still lock one; `None` hands it to [`Self::push_raw`] as such.
    fn unsniffed(&self) -> Option<Self::Codec>;

    /// Raw transport: decode what `data` completes. `codec` is `None` for a
    /// packet with nothing to sniff and no fallback, which a family drops.
    fn push_raw(
        &mut self,
        codec: Option<Self::Codec>,
        shared: &mut SharedState,
        data: &[u8],
        out: &mut RPushResult,
    ) -> AfterPush;

    /// An IEC 61937 burst type this family decodes.
    fn accepts_data_type(data_type: u8) -> bool;

    /// An IEC 61937 burst arrived, before it is known whether the family
    /// takes its type.
    fn enter_iec61937(&mut self) {}

    /// IEC 61937 transport: one burst payload of a type
    /// [`Self::accepts_data_type`] takes.
    fn push_iec61937(
        &mut self,
        shared: &mut SharedState,
        data: &[u8],
        data_type: u8,
        out: &mut RPushResult,
    ) -> AfterPush;

    /// Forget the stream (seek, sync loss, or a path that asked for it). The
    /// configuration stays.
    fn reset(&mut self, shared: &SharedState);

    /// A decoder panicked: drop what [`Self::reset`] keeps, its state being
    /// unknown (IAMF's sequence configuration).
    fn discard_after_panic(&mut self) {}

    /// A configuration key this family answers, `None` for one it does not
    /// know. `input_codec` and `log_level` never reach it.
    fn configure(&mut self, _key: &str, _value: &str) -> Option<bool> {
        None
    }

    fn is_ready(&self) -> bool;
    fn has_objects(&self) -> bool;
    /// One of the names [`Self::source_families`] declares.
    fn source_family(&self) -> &'static str;
    /// The carrier, then the spatial layer decoded over it. Only asked once
    /// [`Self::is_ready`].
    fn source_label(&self, label: &mut String);
    fn fixed_channel_poses(&self) -> RVec<RChannelPose> {
        RVec::new()
    }
    fn channel_tags(&self) -> RVec<RChannelTag> {
        RVec::new()
    }
    fn supported_drc_modes() -> RVec<RString> {
        RVec::new()
    }
    fn set_drc_mode(&mut self, _mode: &str) -> bool {
        false
    }

    /// Every source family a stream of this pipeline can report.
    fn source_families(out: &mut RVec<RSourceFamily>);
}

/// A source family declaration, for [`FamilyPipeline::source_families`].
pub fn source_family(name: &str, label: &str, default_mode: &str) -> RSourceFamily {
    RSourceFamily {
        name: name.into(),
        label: label.into(),
        default_mode: default_mode.into(),
    }
}

/// [`FamilyPipeline::source_families`] as a list.
pub fn source_families<F: FamilyPipeline>() -> RVec<RSourceFamily> {
    let mut families = RVec::new();
    F::source_families(&mut families);
    families
}

/// Test support: every name [`FamilyPipeline::INPUT_CODECS`] lists is one
/// `configure("input_codec")` takes, lower case.
#[doc(hidden)]
pub fn assert_input_codecs_are_taken<F: FamilyPipeline>() {
    assert!(!F::INPUT_CODECS.is_empty());
    for &name in F::INPUT_CODECS {
        assert_eq!(name, name.to_ascii_lowercase());
        let mut bridge = PluginBridge::<F>::new(false);
        assert!(
            bridge.configure("input_codec".into(), name.into()),
            "input_codec {name} is listed but refused"
        );
    }
}

/// A [`FormatBridge`] around one [`FamilyPipeline`]: what a plugin's
/// `new_bridge` returns.
pub struct PluginBridge<F: FamilyPipeline> {
    family: F,
    /// Codec declared by the host for the `Raw` transport via
    /// `configure("input_codec", …)`. Persists across pipeline resets.
    forced_codec: Option<F::Codec>,
    /// Codec locked for the current raw stream (declared or sniffed).
    /// Cleared on reset so a re-sniff happens after a seek / stream change.
    raw_codec: Option<F::Codec>,
    /// IEC 61937 bursts of a data type the family does not decode, since the
    /// last reset: a host keeps sending them, so only the 1st, 2nd, 4th...
    /// are logged.
    unsupported_data_types: RepeatCounter,
    shared: SharedState,
}

impl<F: FamilyPipeline> PluginBridge<F> {
    pub fn new(strict: bool) -> Self {
        let shared = SharedState::new(strict);
        Self {
            family: F::new(&shared),
            forced_codec: None,
            raw_codec: None,
            unsupported_data_types: RepeatCounter::default(),
            shared,
        }
    }

    pub fn family(&self) -> &F {
        &self.family
    }

    pub fn family_mut(&mut self) -> &mut F {
        &mut self.family
    }

    pub fn shared(&self) -> &SharedState {
        &self.shared
    }

    /// The codec `input_codec` declared, if any.
    pub fn forced_codec(&self) -> Option<F::Codec> {
        self.forced_codec
    }

    /// The codec the current raw stream is locked to, if any.
    pub fn locked_codec(&self) -> Option<F::Codec> {
        self.raw_codec
    }

    /// IEC 61937 bursts of a type the family does not decode, since the last
    /// reset.
    pub fn unsupported_bursts(&self) -> u64 {
        self.unsupported_data_types.count()
    }

    /// Reset the whole pipeline: the family, the raw codec lock, the shared
    /// declarations. The declared codec and the running sample count stay.
    pub fn reset_pipeline(&mut self) {
        self.family.reset(&self.shared);
        self.unsupported_data_types = RepeatCounter::default();
        // Re-sniff after reset, but keep any host-declared codec.
        self.raw_codec = None;
        self.shared.declared_object_channels = None;
    }

    /// Resolve the codec for a `Raw` packet. A host-declared codec
    /// (`configure("input_codec", …)`) wins; otherwise the first recognisable
    /// packet locks the stream. An unrecognised packet goes where the family
    /// says ([`FamilyPipeline::unsniffed`]), without locking.
    pub fn resolve_raw_codec(&mut self, data: &[u8]) -> Option<F::Codec> {
        if let Some(codec) = self.raw_codec {
            return Some(codec);
        }
        if let Some(codec) = self.forced_codec {
            self.raw_codec = Some(codec);
            return Some(codec);
        }
        if let Some(codec) = F::sniff(data) {
            self.raw_codec = Some(codec);
            return Some(codec);
        }
        self.family.unsniffed()
    }

    /// The body of [`FormatBridge::push_packet`], which runs it under its
    /// panic guard.
    fn push_packet_unguarded(
        &mut self,
        data: &[u8],
        transport: RInputTransport,
        data_type: u8,
    ) -> RPushResult {
        let mut result = RPushResult {
            frames: RVec::new(),
            error_message: RString::new(),
            did_reset: false,
        };
        match transport {
            RInputTransport::Raw => {
                let codec = self.resolve_raw_codec(data);
                let after = self
                    .family
                    .push_raw(codec, &mut self.shared, data, &mut result);
                if after == AfterPush::ResetPipeline {
                    self.reset_pipeline();
                }
            }
            RInputTransport::Iec61937 => {
                self.family.enter_iec61937();
                if F::accepts_data_type(data_type) {
                    let after =
                        self.family
                            .push_iec61937(&mut self.shared, data, data_type, &mut result);
                    if after == AfterPush::ResetPipeline {
                        self.reset_pipeline();
                    }
                    return result;
                }
                let bursts = self.unsupported_data_types.note();
                if self.shared.strict {
                    let msg = format!(
                        "Unsupported IEC 61937 data type for this bridge: 0x{data_type:02X}"
                    );
                    bridge_diag_log(log::Level::Warn, &msg);
                    result.error_message = msg.into();
                    self.reset_pipeline();
                    result.did_reset = true;
                } else if let Some(bursts) = bursts {
                    bridge_log!(
                        log::Level::Warn,
                        "Unsupported IEC 61937 data type for this bridge: 0x{data_type:02X} ({bursts} burst(s) so far)"
                    );
                }
            }
        }
        result
    }
}

impl<F: FamilyPipeline> FormatBridge for PluginBridge<F> {
    /// Every packet goes through one panic guard. A panic escaping a
    /// `#[sabi_trait]` method does not unwind into the host: abi_stable
    /// prints "Attempted to panic across the ffi boundary" and exits the
    /// process — the player, mid-film. Here it resets the pipeline and comes
    /// back as a reset, and the next packet decodes from a clean state. Only
    /// strict mode gets the message as an error, as with every other decode
    /// failure here. The finer guards of the TrueHD and IAMF paths stay: they
    /// keep the frames decoded before the panic.
    fn push_packet(
        &mut self,
        data: RSlice<'_, u8>,
        transport: RInputTransport,
        data_type: u8,
    ) -> RPushResult {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.push_packet_unguarded(data.as_slice(), transport, data_type)
        }));
        outcome.unwrap_or_else(|payload| {
            let msg = format!("decoder panic: {}; pipeline reset", panic_message(&payload));
            bridge_diag_log(log::Level::Error, &msg);
            // The panicking decoder's state is unknown: what a reset keeps
            // (IAMF's sequence configuration) is rebuilt as well.
            self.family.discard_after_panic();
            self.reset_pipeline();
            RPushResult {
                frames: RVec::new(),
                // A host that did not ask for strict decoding plays through a
                // reset; an error message would fail its call instead.
                error_message: if self.shared.strict {
                    msg.into()
                } else {
                    RString::new()
                },
                did_reset: true,
            }
        })
    }

    fn reset(&mut self) {
        bridge_log!(log::Level::Info, "Bridge reset requested");
        self.reset_pipeline();
        // Note: total_samples is NOT reset — it tracks the global position for
        // continuous-mode timestamping. The handler manages segment offsets.
    }

    fn is_ready(&self) -> bool {
        self.family.is_ready()
    }

    fn has_objects(&self) -> bool {
        self.family.has_objects()
    }

    fn configure(&mut self, key: RStr<'_>, value: RStr<'_>) -> bool {
        match key.as_str() {
            "input_codec" => {
                self.forced_codec = match value.as_str() {
                    "auto" | "" => None,
                    name => match F::input_codec(name) {
                        Some(codec) => Some(codec),
                        None => {
                            bridge_log!(
                                log::Level::Warn,
                                "atmos-bridge: unknown input_codec {name:?}"
                            );
                            return false;
                        }
                    },
                };
                // Force re-resolution against the new codec on the next packet.
                self.raw_codec = None;
                bridge_log!(
                    log::Level::Debug,
                    "atmos-bridge: input_codec set to {:?}",
                    self.forced_codec
                );
                true
            }
            // Process-wide, like the host's own level: messages above it are
            // never formatted nor handed to the sink (see `logging`).
            "log_level" => match value.as_str().trim().parse::<log::LevelFilter>() {
                Ok(level) => {
                    crate::logging::set_max_level(level);
                    true
                }
                Err(_) => {
                    bridge_log!(
                        log::Level::Warn,
                        "atmos-bridge: unknown log_level {:?}",
                        value.as_str()
                    );
                    false
                }
            },
            key => {
                if let Some(taken) = self.family.configure(key, value.as_str()) {
                    return taken;
                }
                // TrueHD's presentation, for a family that has none. Hosts
                // send it once at start-up (default `best`), to every bridge,
                // and stop when it is refused: take the values a TrueHD
                // bridge takes, as a no-op.
                if key == "presentation" {
                    return match value.as_str() {
                        "best" => true,
                        s => match s.parse::<u8>() {
                            Ok(p) if p < TRUEHD_PRESENTATIONS => true,
                            _ => {
                                bridge_log!(
                                    log::Level::Warn,
                                    "atmos-bridge: invalid presentation value {:?}",
                                    s
                                );
                                false
                            }
                        },
                    };
                }
                bridge_log!(
                    log::Level::Debug,
                    "atmos-bridge: unknown configuration key {:?}",
                    key
                );
                false
            }
        }
    }

    fn coordinate_format(&self) -> RCoordinateFormat {
        RCoordinateFormat::Cartesian
    }

    fn fixed_channel_poses(&self) -> RVec<RChannelPose> {
        self.family.fixed_channel_poses()
    }

    fn source_family(&self) -> RString {
        // One of the families `source_families` declares: the renderer's
        // placement policy is chosen per family (`renderer::placement`).
        RString::from(self.family.source_family())
    }

    fn channel_tags(&self) -> RVec<RChannelTag> {
        self.family.channel_tags()
    }

    fn source_label(&self) -> RString {
        // What the host's track information calls the stream: the carrier
        // the demux found, then the spatial layer actually decoded over it.
        // Nothing until a frame decoded — before that the codec path is a
        // guess, and the host has its own.
        if !self.family.is_ready() {
            return RString::new();
        }
        let mut label = String::with_capacity(40);
        self.family.source_label(&mut label);
        RString::from(label)
    }

    fn vbap_cartesian_defaults(&self) -> RVbapCartesianDefaults {
        // Balanced default grid size for runtime cartesian VBAP table
        // generation. The axis sizes mirror the OAMD position quantisation
        // (x, y on 6 bits / 62, z magnitude on 4 bits / 15).
        RVbapCartesianDefaults {
            x_size: 62,
            y_size: 62,
            z_size: 15,
            // Nothing below the floor in the grid: those positions render at
            // their true place with realtime and polar evaluation (above).
            z_neg_size: 0,
            // The OAMD position decode carries z in [-1, 1] — the bitstream
            // has an explicit sign bit for below-floor objects — so the
            // renderer must not clamp z at the panner. Grids without
            // negative-z cells (the default) clamp such requests onto the
            // z = 0 plane, which is the pre-existing behaviour; realtime and
            // polar evaluation render them at their true position.
            allow_negative_z: true,
        }
    }

    fn preferred_vbap_table_mode(&self) -> RVbapTableMode {
        RVbapTableMode::Cartesian
    }

    fn supported_drc_modes(&self) -> RVec<RString> {
        F::supported_drc_modes()
    }

    fn set_drc_mode(&mut self, mode: RStr<'_>) -> bool {
        self.family.set_drc_mode(mode.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_api::RDecodedFrame;

    /// A family that decodes nothing real: a packet whose first byte is
    /// `SYNC` locks it, `PANIC` panics, `RESET` asks for a reset, anything
    /// else is a frame of one sample per byte.
    #[derive(Default)]
    struct Fake {
        resets: u32,
        discarded: u32,
        frames: u64,
        last_codec: Option<Option<u8>>,
        iec_entered: u32,
        fallback: bool,
    }

    const SYNC: u8 = 0xA5;
    const PANIC: u8 = 0xEE;
    const RESET: u8 = 0xDD;

    impl FamilyPipeline for Fake {
        type Codec = u8;
        const INPUT_CODECS: &'static [&'static str] = &["fake"];

        fn probe_raw(data: &[u8]) -> RProbe {
            crate::probe::scan_raw(data, |s| {
                if s[0] == SYNC {
                    crate::probe::Start::Claim
                } else {
                    crate::probe::Start::Reject
                }
            })
        }

        fn new(_shared: &SharedState) -> Self {
            Self {
                fallback: true,
                ..Self::default()
            }
        }
        fn input_codec(name: &str) -> Option<u8> {
            (name == "fake").then_some(1)
        }
        fn sniff(data: &[u8]) -> Option<u8> {
            (data.first() == Some(&SYNC)).then_some(1)
        }
        fn unsniffed(&self) -> Option<u8> {
            self.fallback.then_some(0)
        }
        fn push_raw(
            &mut self,
            codec: Option<u8>,
            shared: &mut SharedState,
            data: &[u8],
            out: &mut RPushResult,
        ) -> AfterPush {
            self.last_codec = Some(codec);
            match data.first() {
                Some(&PANIC) => panic!("fake decoder panic"),
                Some(&RESET) => AfterPush::ResetPipeline,
                _ if codec.is_none() => AfterPush::Continue,
                _ => {
                    self.frames += 1;
                    shared.total_samples += data.len() as u64;
                    out.frames.push(RDecodedFrame {
                        sampling_frequency: 48_000,
                        sample_count: data.len() as u32,
                        channel_count: 0,
                        pcm: RVec::new(),
                        channel_labels: RVec::new(),
                        metadata: RVec::new(),
                        drc_gain: 1.0,
                        drc_ramp_duration: 0,
                        dialogue_level: abi_stable::std_types::ROption::RNone,
                        is_new_segment: false,
                    });
                    AfterPush::Continue
                }
            }
        }
        fn accepts_data_type(data_type: u8) -> bool {
            data_type == 0x42
        }
        fn enter_iec61937(&mut self) {
            self.iec_entered += 1;
        }
        fn push_iec61937(
            &mut self,
            shared: &mut SharedState,
            data: &[u8],
            _data_type: u8,
            out: &mut RPushResult,
        ) -> AfterPush {
            self.push_raw(Some(1), shared, data, out)
        }
        fn reset(&mut self, _shared: &SharedState) {
            self.resets += 1;
        }
        fn discard_after_panic(&mut self) {
            self.discarded += 1;
        }
        fn is_ready(&self) -> bool {
            self.frames > 0
        }
        fn has_objects(&self) -> bool {
            false
        }
        fn source_family(&self) -> &'static str {
            "fake"
        }
        fn source_label(&self, label: &mut String) {
            label.push_str("Fake");
        }
        fn source_families(out: &mut RVec<RSourceFamily>) {
            out.push(source_family("fake", "Fake", "room"));
        }
    }

    fn push(bridge: &mut PluginBridge<Fake>, data: &[u8]) -> RPushResult {
        bridge.push_packet(RSlice::from_slice(data), RInputTransport::Raw, 0)
    }

    #[test]
    fn a_sniffed_codec_locks_until_a_reset() {
        let mut bridge = PluginBridge::<Fake>::new(false);
        push(&mut bridge, &[0x00, 0x01]);
        assert_eq!(bridge.family().last_codec, Some(Some(0)), "the fallback");
        assert_eq!(bridge.locked_codec(), None, "a fallback does not lock");
        push(&mut bridge, &[SYNC, 0x01]);
        assert_eq!(bridge.locked_codec(), Some(1));
        push(&mut bridge, &[0x00]);
        assert_eq!(bridge.family().last_codec, Some(Some(1)), "still locked");
        bridge.reset();
        assert_eq!(bridge.locked_codec(), None);
        assert_eq!(bridge.family().resets, 1);
    }

    #[test]
    fn nothing_to_sniff_and_no_fallback_reaches_the_family_as_none() {
        let mut bridge = PluginBridge::<Fake>::new(false);
        bridge.family_mut().fallback = false;
        let result = push(&mut bridge, &[0x00, 0x01]);
        assert!(result.frames.is_empty());
        assert_eq!(bridge.family().last_codec, Some(None));
    }

    #[test]
    fn a_declared_codec_wins_and_survives_a_reset() {
        let mut bridge = PluginBridge::<Fake>::new(false);
        assert!(bridge.configure("input_codec".into(), "fake".into()));
        push(&mut bridge, &[0x00]);
        assert_eq!(bridge.locked_codec(), Some(1));
        bridge.reset();
        push(&mut bridge, &[0x00]);
        assert_eq!(bridge.family().last_codec, Some(Some(1)));
        assert!(!bridge.configure("input_codec".into(), "other".into()));
        assert_eq!(
            bridge.forced_codec(),
            Some(1),
            "a refused name changes nothing"
        );
        assert!(bridge.configure("input_codec".into(), "auto".into()));
        assert_eq!(bridge.forced_codec(), None);
        assert_eq!(bridge.locked_codec(), None);
    }

    #[test]
    fn a_path_asking_for_a_reset_gets_one() {
        let mut bridge = PluginBridge::<Fake>::new(false);
        push(&mut bridge, &[SYNC]);
        bridge.shared.declared_object_channels = Some(RVec::new());
        push(&mut bridge, &[RESET]);
        assert_eq!(bridge.family().resets, 1);
        assert_eq!(bridge.locked_codec(), None);
        assert!(bridge.shared().declared_object_channels.is_none());
    }

    #[test]
    fn a_panic_is_a_reset_and_strict_mode_also_gets_it_as_an_error() {
        let mut bridge = PluginBridge::<Fake>::new(false);
        push(&mut bridge, &[SYNC]);
        let result = push(&mut bridge, &[PANIC]);
        assert!(result.did_reset);
        assert!(result.error_message.is_empty());
        assert_eq!(bridge.family().discarded, 1);
        assert_eq!(bridge.family().resets, 1);
        assert_eq!(bridge.locked_codec(), None);
        // The next packet decodes.
        assert_eq!(push(&mut bridge, &[SYNC, 1]).frames.len(), 1);

        let mut strict = PluginBridge::<Fake>::new(true);
        let result = push(&mut strict, &[PANIC]);
        assert!(result.did_reset);
        assert!(
            result.error_message.contains("fake decoder panic"),
            "{}",
            result.error_message
        );
    }

    #[test]
    fn unsupported_bursts_are_counted_and_strict_mode_reports_each() {
        let burst = [0u8; 8];
        let mut bridge = PluginBridge::<Fake>::new(false);
        for bursts in 1..=20 {
            let result =
                bridge.push_packet(RSlice::from_slice(&burst), RInputTransport::Iec61937, 0x07);
            assert!(result.error_message.is_empty());
            assert!(!result.did_reset);
            assert_eq!(bridge.unsupported_bursts(), bursts);
        }
        assert_eq!(bridge.family().iec_entered, 20);
        let result =
            bridge.push_packet(RSlice::from_slice(&burst), RInputTransport::Iec61937, 0x42);
        assert_eq!(result.frames.len(), 1, "a type the family takes decodes");
        bridge.reset_pipeline();
        assert_eq!(bridge.unsupported_bursts(), 0);

        let mut strict = PluginBridge::<Fake>::new(true);
        let result =
            strict.push_packet(RSlice::from_slice(&burst), RInputTransport::Iec61937, 0x07);
        assert!(result.did_reset);
        assert_eq!(
            result.error_message.as_str(),
            "Unsupported IEC 61937 data type for this bridge: 0x07"
        );
    }

    #[test]
    fn a_family_without_presentations_takes_the_truehd_values_as_a_no_op() {
        let mut bridge = PluginBridge::<Fake>::new(false);
        for p in ["best", "0", "1", "2", "3"] {
            assert!(bridge.configure("presentation".into(), p.into()), "{p}");
        }
        for p in ["4", "255", "-1", "highest", ""] {
            assert!(!bridge.configure("presentation".into(), p.into()), "{p:?}");
        }
        assert!(!bridge.configure("no_such_key".into(), "1".into()));
    }

    #[test]
    fn log_level_sets_the_forwarded_level() {
        let _guard = crate::logging::LEVEL_TEST_LOCK.lock().unwrap();
        let mut bridge = PluginBridge::<Fake>::new(false);
        assert!(bridge.configure("log_level".into(), "debug".into()));
        assert!(crate::logging::log_enabled(log::Level::Debug));
        assert!(bridge.configure("log_level".into(), "WARN".into()));
        assert!(!crate::logging::log_enabled(log::Level::Info));
        assert!(!bridge.configure("log_level".into(), "loud".into()));
        crate::logging::set_max_level(log::LevelFilter::Info);
    }

    #[test]
    fn every_listed_input_codec_is_taken() {
        assert_input_codecs_are_taken::<Fake>();
    }

    #[test]
    fn the_label_waits_for_a_frame() {
        let mut bridge = PluginBridge::<Fake>::new(false);
        assert_eq!(bridge.source_label().as_str(), "");
        push(&mut bridge, &[SYNC, 1, 2]);
        assert_eq!(bridge.source_label().as_str(), "Fake");
        assert_eq!(bridge.source_family().as_str(), "fake");
        assert_eq!(source_families::<Fake>().len(), 1);
    }
}
