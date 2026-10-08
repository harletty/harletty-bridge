use abi_stable::std_types::{RString, RVec};
use bridge_api::{RChannelPose, RChannelTag, RProbe, RPushResult, RSourceFamily};
use bridge_common::family::FamilyPipeline;
#[cfg(test)]
use {
    abi_stable::std_types::RSlice, bridge_api::FormatBridge, bridge_api::RInputTransport,
    bridge_common::family::PluginBridge,
};

#[cfg(not(all(feature = "dolby", feature = "dts", feature = "iamf")))]
use crate::logging::bridge_diag_log;
use crate::shared::{AfterPush, SharedState};
#[cfg(feature = "dolby")]
use bridge_family_dolby::{DolbyPipeline, FAMILY_DOLBY};
#[cfg(feature = "dts")]
use bridge_family_dts::{DtsPipeline, FAMILY_AURO, FAMILY_DTS};

#[cfg(not(any(feature = "dolby", feature = "dts", feature = "iamf")))]
compile_error!("harletty-bridge needs a codec family: enable `dolby`, `dts` or `iamf`");

/// The source family IAMF streams report. The renderer knows no family by
/// name: the catalogue below is what it, and Studio, offer.
const FAMILY_IAMF: &str = "iamf";
/// What a refused stream reports (its family's own name), in a build
/// without that family.
#[cfg(not(feature = "dts"))]
const FAMILY_DTS: &str = "dts";
#[cfg(not(feature = "dolby"))]
const FAMILY_DOLBY: &str = "dolby";
/// What the bridge reports before a stream is identified: Dolby, or with no
/// Dolby in the build, the first family it has.
const IDLE_FAMILY: &str = if cfg!(feature = "dolby") {
    "dolby"
} else if cfg!(feature = "dts") {
    "dts"
} else {
    FAMILY_IAMF
};

/// The catalogue: Dolby's codecs share the room-cube bed; DTS states ITU
/// angles but has always rendered in the room; an unfolded Auro-3D carrier
/// asks for its speakers equidistant on a sphere; IAMF's loudspeaker
/// layouts are ITU BS.2051 angles, a sphere too. IAMF only when this build
/// decodes it.
pub(crate) fn source_families() -> RVec<RSourceFamily> {
    let family = |name: &str, label: &str, default_mode: &str| RSourceFamily {
        name: name.into(),
        label: label.into(),
        default_mode: default_mode.into(),
    };
    let mut families = RVec::new();
    #[cfg(feature = "dolby")]
    families.push(family(FAMILY_DOLBY, "Dolby", "room"));
    #[cfg(feature = "dts")]
    {
        families.push(family(FAMILY_DTS, "DTS", "room"));
        families.push(family(FAMILY_AURO, "Auro-3D", "sphere"));
    }
    if cfg!(feature = "iamf") {
        families.push(family(FAMILY_IAMF, "Eclipsa / IAMF", "sphere"));
    }
    families
}

/// Codec carried by a [`bridge_api::RInputTransport::Raw`] packet, which (unlike the IEC
/// 61937 transport) has no `data_type` to disambiguate TrueHD from E-AC3.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RawCodec {
    TrueHd,
    Eac3,
    Dts,
    /// An IAMF OBU stream.
    Iamf,
}

impl RawCodec {
    /// Every codec is recognised whatever the build, so a stream whose
    /// family is not built is refused by name rather than fed to another
    /// family's decoder. Said once per stream, not on every packet.
    #[cfg(not(all(feature = "dolby", feature = "dts", feature = "iamf")))]
    fn refusal(self) -> &'static str {
        match self {
            RawCodec::TrueHd => "truehd: this bridge was built without TrueHD support",
            RawCodec::Eac3 => "eac3: this bridge was built without E-AC-3 support",
            RawCodec::Dts => "dts: this bridge was built without DTS support",
            RawCodec::Iamf => "iamf: this bridge was built without IAMF support",
        }
    }
}

/// Test-only panic injection at the entry of each codec path, for the tests
/// of [`FormatBridge::push_packet`]'s guard. Armed for one codec, it panics
/// once, the next time that path is entered on this thread.
#[cfg(test)]
pub(crate) mod injected_panic {
    use super::RawCodec;
    use std::cell::Cell;

    thread_local! {
        static ARMED: Cell<Option<RawCodec>> = const { Cell::new(None) };
    }

    pub(crate) fn arm(codec: RawCodec) {
        ARMED.with(|armed| armed.set(Some(codec)));
    }

    /// The armed panic has not been hit yet.
    pub(crate) fn is_armed() -> bool {
        ARMED.with(|armed| armed.get().is_some())
    }

    pub(crate) fn hit(codec: RawCodec) {
        if ARMED.with(|armed| armed.get()) == Some(codec) {
            ARMED.with(|armed| armed.set(None));
            panic!("injected panic in the {codec:?} path");
        }
    }
}

/// Best-effort codec detection on a raw access unit, used when the host did not
/// declare the codec via `configure("input_codec", …)`. Checks the most
/// specific pattern first: the TrueHD major-sync word `0xF8726FBA` at offset 4,
/// then the E-AC3/AC-3 sync word `0x0B77` at offset 0 (incl. byte-swapped).
/// An IAMF stream opens with its sequence header, whose `iamf` code is checked
/// before all of them: its first byte (`0xF8`–`0xFF`) is also where a TrueHD
/// major sync's first byte can sit.
fn sniff_raw_codec(data: &[u8]) -> Option<RawCodec> {
    if is_iamf_sequence_header(data) {
        return Some(RawCodec::Iamf);
    }
    if data.len() >= 8 && data[4] == 0xF8 && data[5] == 0x72 && data[6] == 0x6F && data[7] == 0xBA {
        return Some(RawCodec::TrueHd);
    }
    // DTS core (0x7FFE8001) or extension substream (0x64582025) at offset 0.
    if data.len() >= 4 {
        let w = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        if w == 0x7FFE_8001 || w == 0x6458_2025 {
            return Some(RawCodec::Dts);
        }
    }
    if data.len() >= 2
        && ((data[0] == 0x0B && data[1] == 0x77) || (data[0] == 0x77 && data[1] == 0x0B))
    {
        return Some(RawCodec::Eac3);
    }
    None
}

/// The codec family behind `codec` is compiled into this build.
fn is_built(codec: RawCodec) -> bool {
    match codec {
        RawCodec::TrueHd | RawCodec::Eac3 => cfg!(feature = "dolby"),
        RawCodec::Dts => cfg!(feature = "dts"),
        RawCodec::Iamf => cfg!(feature = "iamf"),
    }
}

/// An IEC 61937 burst type one of the built families decodes.
fn accepts_data_type(data_type: u8) -> bool {
    #[cfg(feature = "dolby")]
    if DolbyPipeline::accepts_data_type(data_type) {
        return true;
    }
    #[cfg(feature = "dts")]
    if bridge_family_dts::accepts_data_type(data_type) {
        return true;
    }
    let _ = data_type;
    false
}

/// An IA sequence header OBU (type 31) at offset 0 whose payload starts with
/// the `iamf` code (IAMF §3.4). The size field between the two is a leb128.
fn is_iamf_sequence_header(data: &[u8]) -> bool {
    if data.first().is_none_or(|&header| header >> 3 != 31) {
        return false;
    }
    let size_len = data
        .iter()
        .skip(1)
        .take(8)
        .position(|&byte| byte & 0x80 == 0);
    size_len.is_some_and(|n| data.get(2 + n..6 + n) == Some(b"iamf".as_slice()))
}

/// Every codec family this build holds, behind one bridge: the router. It
/// finds the codec (sniffed, declared, or the IEC 61937 data type), hands
/// each packet to that family, and answers the host's questions from the
/// family the stream is on. What every bridge does besides (the panic guard,
/// resets, the raw codec lock, `input_codec`, `log_level`) is
/// [`bridge_common::family::PluginBridge`]'s.
pub(crate) struct Families {
    // ── Dolby (TrueHD, E-AC-3) ───────────────────────────────────────
    #[cfg(feature = "dolby")]
    pub(crate) dolby: DolbyPipeline,
    // ── DTS (DCA) pipeline ───────────────────────────────────────────
    #[cfg(feature = "dts")]
    pub(crate) dts: DtsPipeline,
    /// True when the most recent `push_packet` carried DTS, decoded or
    /// refused.
    pub(crate) dts_active: bool,
    // ── IAMF pipeline ────────────────────────────────────────────────
    #[cfg(feature = "iamf")]
    pub(crate) iamf: Box<crate::iamf_pipeline::IamfState>,
    /// True when the most recent `push_packet` carried IAMF, decoded or
    /// refused.
    pub(crate) iamf_active: bool,
    /// The codec of this stream that was refused, its family not being
    /// built: said once per stream rather than on every packet.
    #[cfg(not(all(feature = "dolby", feature = "dts", feature = "iamf")))]
    pub(crate) refused: Option<RawCodec>,
}

/// The bridge a host holds: the families of this build, as one bridge.
#[cfg(test)]
pub(crate) type AtmosBridge = PluginBridge<Families>;

impl Families {
    /// The stream is a Dolby one this build refused, as opposed to no stream
    /// identified yet: both leave the DTS and IAMF flags down.
    fn refused_dolby(&self) -> bool {
        #[cfg(not(feature = "dolby"))]
        return matches!(self.refused, Some(RawCodec::TrueHd | RawCodec::Eac3));
        #[cfg(feature = "dolby")]
        return false;
    }

    /// Refuse a stream whose family this build does not decode: by name, and
    /// once per stream.
    #[cfg(not(all(feature = "dolby", feature = "dts", feature = "iamf")))]
    fn refuse(&mut self, codec: RawCodec, result: &mut RPushResult) {
        if self.refused != Some(codec) {
            self.refused = Some(codec);
            let msg = codec.refusal();
            bridge_diag_log(log::Level::Warn, msg);
            result.error_message = msg.into();
        }
    }
}

impl FamilyPipeline for Families {
    type Codec = RawCodec;
    /// Every name `input_codec` takes, whatever the build.
    const INPUT_CODECS: &'static [&'static str] = &[
        "truehd", "mlp", "eac3", "ac3", "ec3", "e-ac3", "dts", "dca", "dtshd", "dts-hd", "dtsx",
        "dts:x", "iamf",
    ];

    /// The router's own sniff, at offset 0 only, as `push_packet` detects a
    /// stream: what the combined bridge answered as a plugin. The family
    /// plugins claim validated starts anywhere in the window.
    fn probe_raw(data: &[u8]) -> RProbe {
        if sniff_raw_codec(data).is_some_and(is_built) {
            RProbe::claim(0)
        } else {
            RProbe::none(0)
        }
    }

    fn new(shared: &SharedState) -> Self {
        #[cfg(not(feature = "dolby"))]
        let _ = shared;
        Self {
            #[cfg(feature = "dolby")]
            dolby: DolbyPipeline::new(shared),
            #[cfg(feature = "dts")]
            dts: DtsPipeline::new(),
            dts_active: false,
            #[cfg(feature = "iamf")]
            iamf: Box::default(),
            iamf_active: false,
            #[cfg(not(all(feature = "dolby", feature = "dts", feature = "iamf")))]
            refused: None,
        }
    }

    /// Every name is taken whatever the build, so a stream declared of a
    /// family left out is refused by name rather than sniffed.
    fn input_codec(name: &str) -> Option<RawCodec> {
        match name {
            "eac3" | "ec3" | "e-ac3" | "ac3" => Some(RawCodec::Eac3),
            "truehd" | "mlp" => Some(RawCodec::TrueHd),
            "dts" | "dca" | "dtsx" | "dts:x" | "dts-hd" | "dtshd" => Some(RawCodec::Dts),
            "iamf" => Some(RawCodec::Iamf),
            _ => None,
        }
    }

    fn sniff(data: &[u8]) -> Option<RawCodec> {
        sniff_raw_codec(data)
    }

    /// An unrecognised first packet falls back to TrueHD for that packet
    /// without locking, so a later syncful packet can still pin the codec;
    /// with no Dolby in the build, it is dropped.
    fn unsniffed(&self) -> Option<RawCodec> {
        // An IAMF stream only announces itself in its sequence header, so a
        // reset (a seek) is followed by temporal units with nothing to sniff.
        // While the sequence is still configured they are its continuation.
        #[cfg(feature = "iamf")]
        if self.iamf.has_sequence() {
            return Some(RawCodec::Iamf);
        }
        cfg!(feature = "dolby").then_some(RawCodec::TrueHd)
    }

    fn push_raw(
        &mut self,
        codec: Option<RawCodec>,
        shared: &mut SharedState,
        data: &[u8],
        result: &mut RPushResult,
    ) -> AfterPush {
        #[cfg(all(feature = "bridge-perf", feature = "dolby"))]
        self.dolby.note_raw_packet(data.len());
        let Some(codec) = codec else {
            // Nothing to sniff, and no Dolby to fall back to.
            self.iamf_active = false;
            return AfterPush::Continue;
        };
        self.iamf_active = codec == RawCodec::Iamf;
        if codec == RawCodec::Iamf || codec == RawCodec::Dts {
            #[cfg(feature = "dolby")]
            self.dolby.leave();
        }
        self.dts_active = codec == RawCodec::Dts;
        #[cfg(test)]
        injected_panic::hit(codec);
        match codec {
            #[cfg(feature = "dolby")]
            RawCodec::Eac3 => self.dolby.push_raw_eac3(shared, data, result),
            #[cfg(feature = "dolby")]
            RawCodec::TrueHd => self.dolby.push_raw_truehd(shared, data, result),
            #[cfg(feature = "dts")]
            RawCodec::Dts => self.dts.push_raw(shared, data, result),
            #[cfg(feature = "iamf")]
            RawCodec::Iamf => crate::iamf_pipeline::push_iamf(&mut self.iamf, shared, data, result),
            #[cfg(not(all(feature = "dolby", feature = "dts", feature = "iamf")))]
            refused => {
                let _ = (shared, data);
                self.refuse(refused, result);
                AfterPush::Continue
            }
        }
    }

    fn accepts_data_type(data_type: u8) -> bool {
        accepts_data_type(data_type)
    }

    fn enter_iec61937(&mut self) {
        // IAMF has no IEC 61937 data type: it only comes raw.
        self.iamf_active = false;
    }

    fn push_iec61937(
        &mut self,
        shared: &mut SharedState,
        data: &[u8],
        data_type: u8,
        result: &mut RPushResult,
    ) -> AfterPush {
        // ── Dolby: TrueHD in MAT (0x16), E-AC-3 (0x15) ────────────────
        #[cfg(feature = "dolby")]
        if DolbyPipeline::accepts_data_type(data_type) {
            return self.dolby.push_iec61937(shared, data, data_type, result);
        }

        // ── DTS (data types 0x0B/0x0C/0x0D/0x11) ──────────────────────
        #[cfg(feature = "dts")]
        if bridge_family_dts::accepts_data_type(data_type) {
            #[cfg(feature = "dolby")]
            self.dolby.leave();
            self.dts_active = true;
            #[cfg(test)]
            injected_panic::hit(RawCodec::Dts);
            return self.dts.push_iec61937(shared, data, data_type, result);
        }

        // Only called for a type `accepts_data_type` takes.
        let _ = (shared, data, data_type, result);
        AfterPush::Continue
    }

    fn reset(&mut self, shared: &SharedState) {
        #[cfg(feature = "dolby")]
        self.dolby.reset(shared);
        #[cfg(not(feature = "dolby"))]
        let _ = shared;

        // DTS reset.
        #[cfg(feature = "dts")]
        self.dts.reset();
        self.dts_active = false;

        // IAMF reset: the stream position goes, the sequence's configuration
        // stays, so decoding resumes without waiting for a sequence header.
        #[cfg(feature = "iamf")]
        self.iamf.reset();
        self.iamf_active = false;
        #[cfg(not(all(feature = "dolby", feature = "dts", feature = "iamf")))]
        {
            self.refused = None;
        }
    }

    /// IAMF keeps its sequence configuration across a reset: after a panic in
    /// its path, it is rebuilt from the next sequence header.
    fn discard_after_panic(&mut self) {
        #[cfg(feature = "iamf")]
        if self.iamf_active {
            *self.iamf = crate::iamf_pipeline::IamfState::default();
        }
    }

    fn configure(&mut self, key: &str, value: &str) -> Option<bool> {
        #[cfg(feature = "dolby")]
        return self.dolby.configure(key, value);
        #[cfg(not(feature = "dolby"))]
        {
            let _ = (key, value);
            None
        }
    }

    fn is_ready(&self) -> bool {
        #[cfg(feature = "iamf")]
        if self.iamf.is_ready() {
            return true;
        }
        #[cfg(feature = "dts")]
        if self.dts.is_ready() {
            return true;
        }
        #[cfg(feature = "dolby")]
        if self.dolby.is_ready() {
            return true;
        }
        false
    }

    fn has_objects(&self) -> bool {
        if self.iamf_active {
            // Channel-based and scene-based elements are rendered to a 7.1.4
            // bed in the bridge; IAMF v2.0 objects reach the renderer as
            // objects.
            #[cfg(feature = "iamf")]
            return self.iamf.has_objects();
            #[cfg(not(feature = "iamf"))]
            return false;
        }
        if self.dts_active {
            #[cfg(feature = "dts")]
            return self.dts.has_objects();
            #[cfg(not(feature = "dts"))]
            return false;
        }
        #[cfg(feature = "dolby")]
        return self.dolby.has_objects();
        #[cfg(not(feature = "dolby"))]
        return false;
    }

    fn source_family(&self) -> &'static str {
        if self.iamf_active {
            FAMILY_IAMF
        } else if self.dts_active {
            #[cfg(feature = "dts")]
            let family = self.dts.source_family();
            #[cfg(not(feature = "dts"))]
            let family = FAMILY_DTS;
            family
        } else if self.refused_dolby() {
            FAMILY_DOLBY
        } else {
            IDLE_FAMILY
        }
    }

    fn source_label(&self, label: &mut String) {
        #[cfg(feature = "iamf")]
        if self.iamf_active {
            label.push_str(self.iamf.description().unwrap_or("IAMF"));
            return;
        }
        if self.dts_active {
            #[cfg(feature = "dts")]
            self.dts.source_label(label);
        } else {
            #[cfg(feature = "dolby")]
            self.dolby.source_label(label);
        }
        let _ = label;
    }

    fn fixed_channel_poses(&self) -> RVec<RChannelPose> {
        // Two formats here state an angle for their channels: an unfolded
        // Auro-3D carrier declares its whole layout from Auro's setup table,
        // and DTS declares its lower layer from the ETSI loudspeaker table.
        // Dolby's bed is defined in its room cube, not by angles, and
        // declares nothing: the renderer's room model is its model. IAMF's
        // bed is the layout the decoder rendered to (BS.2051 System J, or
        // 9.1.6 with its wides), whose angles the recommendation states.
        #[cfg(feature = "iamf")]
        if self.iamf_active {
            return self.iamf.declared_poses();
        }
        #[cfg(feature = "dts")]
        if self.dts_active {
            return self.dts.fixed_channel_poses();
        }
        RVec::new()
    }

    fn channel_tags(&self) -> RVec<RChannelTag> {
        // IAMF's dialogue element, when a mix codes it apart; no other
        // format here tags anything.
        #[cfg(feature = "iamf")]
        if self.iamf_active {
            return self.iamf.channel_tags();
        }
        RVec::new()
    }

    /// DRC is Dolby's: a build without it offers none.
    fn supported_drc_modes() -> RVec<RString> {
        #[cfg(feature = "dolby")]
        return DolbyPipeline::supported_drc_modes();
        #[cfg(not(feature = "dolby"))]
        return RVec::new();
    }

    fn set_drc_mode(&mut self, mode: &str) -> bool {
        #[cfg(feature = "dolby")]
        return self.dolby.set_drc_mode(mode);
        #[cfg(not(feature = "dolby"))]
        {
            let _ = mode;
            false
        }
    }

    fn source_families(out: &mut RVec<RSourceFamily>) {
        out.extend(source_families());
    }
}

#[cfg(test)]
mod raw_transport_tests {
    use super::*;
    #[cfg(feature = "dts")]
    use std::io::Read;

    #[cfg(feature = "dts")]
    fn read_prefix(path: &str, bytes: u64) -> Option<Vec<u8>> {
        let mut input = std::fs::File::open(path).ok()?;
        let mut prefix = Vec::with_capacity(bytes as usize);
        input.by_ref().take(bytes).read_to_end(&mut prefix).ok()?;
        Some(prefix)
    }

    #[cfg(feature = "dts")]
    fn corpus_path(variable: &str) -> Option<String> {
        let path = std::env::var(variable).ok()?;
        std::path::Path::new(&path).is_file().then_some(path)
    }

    #[test]
    fn sniff_detects_eac3_syncword() {
        assert_eq!(
            sniff_raw_codec(&[0x0B, 0x77, 0x00, 0x00]),
            Some(RawCodec::Eac3)
        );
        // Byte-swapped 16-bit order is still E-AC3.
        assert_eq!(
            sniff_raw_codec(&[0x77, 0x0B, 0x00, 0x00]),
            Some(RawCodec::Eac3)
        );
    }

    #[test]
    fn sniff_detects_truehd_major_sync() {
        let buf = [0x00, 0x00, 0x00, 0x00, 0xF8, 0x72, 0x6F, 0xBA];
        assert_eq!(sniff_raw_codec(&buf), Some(RawCodec::TrueHd));
    }

    /// The router's probe claims what this build decodes, at offset 0, and
    /// nothing else: a codec whose family is not built is not claimed.
    #[test]
    fn probe_claims_the_built_families_only() {
        use bridge_api::RProbeVerdict::{Claim, None as Nothing};
        let verdict = |data: &[u8], transport, data_type| {
            bridge_common::plugin::probe::<Families>(data.into(), transport, data_type).verdict
        };
        let truehd = [0x00, 0x00, 0x00, 0x00, 0xF8, 0x72, 0x6F, 0xBA];
        let eac3 = [0x0B, 0x77, 0x00, 0x00];
        let dts = [0x7F, 0xFE, 0x80, 0x01];
        let iamf = [0xF8, 0x06, b'i', b'a', b'm', b'f', 0x00, 0x00];
        let built = |yes: bool| if yes { Claim } else { Nothing };
        let dolby = cfg!(feature = "dolby");
        let dts_built = cfg!(feature = "dts");
        assert_eq!(verdict(&truehd, RInputTransport::Raw, 0), built(dolby));
        assert_eq!(verdict(&eac3, RInputTransport::Raw, 0), built(dolby));
        assert_eq!(verdict(&dts, RInputTransport::Raw, 0), built(dts_built));
        assert_eq!(
            verdict(&iamf, RInputTransport::Raw, 0),
            built(cfg!(feature = "iamf"))
        );
        assert_eq!(verdict(&[0x12, 0x34], RInputTransport::Raw, 0), Nothing);
        assert_eq!(verdict(&[], RInputTransport::Iec61937, 0x16), built(dolby));
        assert_eq!(verdict(&[], RInputTransport::Iec61937, 0x15), built(dolby));
        assert_eq!(
            verdict(&[], RInputTransport::Iec61937, 0x0B),
            built(dts_built)
        );
        assert_eq!(verdict(&[], RInputTransport::Iec61937, 0x07), Nothing);
    }

    /// Every codec name the router lists is one `configure` takes.
    #[test]
    fn every_listed_input_codec_is_accepted() {
        bridge_common::family::assert_input_codecs_are_taken::<Families>();
    }

    #[test]
    fn sniff_unknown_is_none() {
        assert_eq!(sniff_raw_codec(&[0x12, 0x34, 0x56, 0x78]), None);
        assert_eq!(sniff_raw_codec(&[0x0B]), None); // too short
    }

    #[test]
    fn sniff_detects_an_iamf_sequence_header() {
        // OBU type 31, obu_size 6, then ia_code "iamf" and the profiles.
        let header = [0xF8, 0x06, b'i', b'a', b'm', b'f', 0x00, 0x00];
        assert_eq!(sniff_raw_codec(&header), Some(RawCodec::Iamf));
        // A two-byte obu_size moves the code along.
        let long = [0xF8, 0x86, 0x00, b'i', b'a', b'm', b'f', 0x00];
        assert_eq!(sniff_raw_codec(&long), Some(RawCodec::Iamf));
        // The same first byte without the code is not IAMF, and a TrueHD
        // major sync at offset 4 still reads as TrueHD.
        assert_eq!(sniff_raw_codec(&[0xF8, 0x06, b'x', b'a', b'm', b'f']), None);
        let thd = [0xF8, 0x00, 0x00, 0x00, 0xF8, 0x72, 0x6F, 0xBA];
        assert_eq!(sniff_raw_codec(&thd), Some(RawCodec::TrueHd));
        // Truncated before the code.
        assert_eq!(sniff_raw_codec(&[0xF8, 0x06, b'i', b'a']), None);
    }

    #[test]
    fn configure_log_level_sets_the_forwarded_level() {
        let _guard = crate::logging::LEVEL_TEST_LOCK.lock().unwrap();
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("log_level".into(), "debug".into()));
        assert!(crate::logging::log_enabled(log::Level::Debug));
        assert!(bridge.configure("log_level".into(), "WARN".into()));
        assert!(!crate::logging::log_enabled(log::Level::Info));
        assert!(!bridge.configure("log_level".into(), "loud".into()));
        assert!(!crate::logging::log_enabled(log::Level::Info));
        crate::logging::set_max_level(log::LevelFilter::Info);
    }

    #[test]
    fn configure_input_codec_accepts_iamf() {
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("input_codec".into(), "iamf".into()));
        assert_eq!(bridge.forced_codec(), Some(RawCodec::Iamf));
    }

    #[test]
    fn resolve_prefers_forced_codec_and_locks() {
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("input_codec".into(), "eac3".into()));
        // Unrecognisable bytes, but the host-declared codec wins.
        assert_eq!(
            bridge.resolve_raw_codec(&[0x12, 0x34, 0x56, 0x78]),
            Some(RawCodec::Eac3)
        );
        assert_eq!(bridge.locked_codec(), Some(RawCodec::Eac3));
    }

    #[test]
    fn resolve_sniffs_when_unforced() {
        let mut eac3 = AtmosBridge::new(false);
        assert_eq!(
            eac3.resolve_raw_codec(&[0x0B, 0x77, 0, 0]),
            Some(RawCodec::Eac3)
        );
        assert_eq!(eac3.locked_codec(), Some(RawCodec::Eac3));

        let mut thd = AtmosBridge::new(false);
        let buf = [0, 0, 0, 0, 0xF8, 0x72, 0x6F, 0xBA];
        assert_eq!(thd.resolve_raw_codec(&buf), Some(RawCodec::TrueHd));
        assert_eq!(thd.locked_codec(), Some(RawCodec::TrueHd));
    }

    #[cfg(feature = "dolby")]
    #[test]
    fn resolve_unknown_first_packet_defaults_truehd_without_locking() {
        let mut bridge = AtmosBridge::new(false);
        // No recognisable sync → treat as TrueHD for this packet but do NOT
        // lock, so a later syncful packet can still pin the codec.
        assert_eq!(
            bridge.resolve_raw_codec(&[0x12, 0x34, 0x56, 0x78]),
            Some(RawCodec::TrueHd)
        );
        assert_eq!(bridge.locked_codec(), None);
    }

    /// With no Dolby to fall back to, a packet nothing recognises is dropped,
    /// quietly, and the codec stays open.
    #[cfg(not(feature = "dolby"))]
    #[test]
    fn an_unknown_packet_without_dolby_is_dropped_without_locking() {
        let mut bridge = AtmosBridge::new(false);
        assert_eq!(bridge.resolve_raw_codec(&[0x12, 0x34, 0x56, 0x78]), None);
        assert_eq!(bridge.locked_codec(), None);
        let result = bridge.push_packet(
            RSlice::from_slice(&[0x12, 0x34, 0x56, 0x78]),
            RInputTransport::Raw,
            0,
        );
        assert!(result.frames.is_empty());
        assert!(result.error_message.is_empty());
    }

    #[test]
    fn sniff_detects_dts_syncwords() {
        // Core syncword 0x7FFE8001.
        assert_eq!(
            sniff_raw_codec(&[0x7F, 0xFE, 0x80, 0x01]),
            Some(RawCodec::Dts)
        );
        // Extension substream syncword 0x64582025.
        assert_eq!(
            sniff_raw_codec(&[0x64, 0x58, 0x20, 0x25]),
            Some(RawCodec::Dts)
        );
    }

    #[test]
    fn configure_input_codec_accepts_dts() {
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("input_codec".into(), "dts".into()));
        assert_eq!(bridge.forced_codec(), Some(RawCodec::Dts));
    }

    /// The family follows the codec path the last packet took: Dolby until
    /// a DTS packet, DTS until an Auro carrier is confirmed. Before any
    /// packet the bridge is a Dolby bridge, which is also what an older host
    /// that never asks would assume.
    #[cfg(all(feature = "dolby", feature = "dts"))]
    #[test]
    fn source_family_follows_the_active_codec() {
        let mut bridge = AtmosBridge::new(false);
        assert_eq!(bridge.source_family().as_str(), "dolby");
        assert!(
            bridge.fixed_channel_poses().is_empty(),
            "Dolby declares no angles"
        );
        bridge.family_mut().dts_active = true;
        assert_eq!(bridge.source_family().as_str(), "dts");
        let poses = bridge.fixed_channel_poses();
        assert!(
            poses
                .iter()
                .any(|p| p.label == bridge_api::RChannelLabel::Ls && p.azimuth_deg == -110.0),
            "DTS declares its ETSI angles"
        );
        bridge.family_mut().dts_active = false;
        assert_eq!(bridge.source_family().as_str(), "dolby");
    }

    /// Nothing is named before a frame decoded: until then the codec path is
    /// a guess, and the host has its own. The labels themselves are each
    /// family's (their own tests; the corpus tests read them through here).
    #[test]
    fn source_label_is_empty_before_a_frame() {
        let bridge = AtmosBridge::new(false);
        assert_eq!(bridge.source_label().as_str(), "");
    }

    /// Every family a stream can name is in the catalogue the renderer
    /// learns them from — a name missing there would render as generic and
    /// could not be set apart.
    #[test]
    fn every_declared_family_is_in_the_catalogue() {
        let catalogue: Vec<String> = source_families()
            .iter()
            .map(|family| family.name.to_string())
            .collect();
        let mut bridge = AtmosBridge::new(false);
        let mut seen = vec![bridge.source_family().to_string()];
        bridge.family_mut().dts_active = true;
        if cfg!(feature = "dts") {
            seen.push(bridge.source_family().to_string());
        }
        bridge.family_mut().dts_active = false;
        bridge.family_mut().iamf_active = true;
        if cfg!(feature = "iamf") {
            seen.push(bridge.source_family().to_string());
        }
        #[cfg(feature = "dts")]
        seen.push(FAMILY_AURO.to_owned());
        for name in seen {
            assert!(catalogue.contains(&name), "{name} not in {catalogue:?}");
        }
        for family in source_families().iter() {
            assert!(
                matches!(family.default_mode.as_str(), "room" | "sphere"),
                "{}",
                family.default_mode
            );
        }
    }

    // End-to-end: feed a raw DTS core stream through the FormatBridge and check
    // it emits 5.1 bed frames with the expected channel labels. Skips when the
    // (uncommitted) corpus is absent.
    #[cfg(feature = "dts")]
    #[test]
    fn dts_hd_auro_carrier_unfolds_into_its_layout() {
        let Some(dts) = corpus_path("HARLETTY_AURO_DTS_CORPUS") else {
            eprintln!("skipping: HARLETTY_AURO_DTS_CORPUS is not set to a readable file");
            return;
        };
        let bytes = std::fs::read(dts).unwrap();
        let mut bridge = AtmosBridge::new(false);
        let mut frames = Vec::new();
        for chunk in bytes.chunks(16 * 1024) {
            let result = bridge.push_packet(RSlice::from_slice(chunk), RInputTransport::Raw, 0);
            assert!(result.error_message.is_empty(), "{}", result.error_message);
            frames.extend(result.frames.into_iter());
        }
        assert_eq!(
            bridge.source_family().as_str(),
            "auro",
            "the carrier was not confirmed"
        );
        assert!(!bridge.has_objects(), "Auro is fixed channels, not objects");
        assert_eq!(bridge.source_family().as_str(), "auro");
        assert!(
            bridge
                .source_label()
                .as_str()
                .starts_with("DTS-HD MA + Auro-3D"),
            "label {}",
            bridge.source_label()
        );
        // Every frame that came out is the unfolded layout: the carrier was
        // held back until the verdict, never emitted as 7.1.
        let channels = frames[0].channel_count;
        assert!(
            channels > 8,
            "expected more than the carrier's channels, got {channels}"
        );
        assert!(frames.iter().all(|f| f.channel_count == channels));
        let labels = &frames[0].channel_labels;
        assert!(
            labels.contains(&bridge_api::RChannelLabel::Lh)
                && labels.contains(&bridge_api::RChannelLabel::Rhs),
            "the height layer is the height tier, not the top one: {labels:?}"
        );
        // The bridge declares where Auro puts every one of them.
        let poses = bridge.fixed_channel_poses();
        let lhs = poses
            .iter()
            .find(|p| p.label == bridge_api::RChannelLabel::Lhs)
            .expect("Lhs is declared");
        assert_eq!((lhs.azimuth_deg, lhs.elevation_deg), (-110.0, 30.0));
        assert!(
            poses.iter().all(|p| labels.contains(&p.label)),
            "declared poses name only channels of the frame"
        );
        // Output is one block behind input, and no more.
        let emitted: u64 = frames.iter().map(|f| u64::from(f.sample_count)).sum();
        assert!(
            bridge.shared().total_samples - emitted <= 4096,
            "{} held back",
            bridge.shared().total_samples - emitted
        );
        eprintln!(
            "{} frames, {} channels, {emitted}/{} samples out",
            frames.len(),
            channels,
            bridge.shared().total_samples
        );
    }

    #[cfg(feature = "dts")]
    #[test]
    fn dts_raw_transport_emits_bed_frames() {
        let Some(dts) = corpus_path("HARLETTY_DTS_CORE_CORPUS") else {
            eprintln!("skipping: HARLETTY_DTS_CORE_CORPUS is not set to a readable file");
            return;
        };
        let bytes = std::fs::read(dts).unwrap();
        let mut bridge = AtmosBridge::new(false);
        let result = bridge.push_packet(RSlice::from_slice(&bytes), RInputTransport::Raw, 0);
        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(!result.frames.is_empty(), "no frames decoded");
        // DTS core is plain channel-based audio, not objects: it reports
        // non-spatial and lets the host's channel-render mode place/virtualise
        // the bed (same as AC-3).
        assert!(!bridge.has_objects());
        assert!(bridge.is_ready());

        let f = &result.frames[0];
        assert_eq!(f.channel_count, 6, "expected 5.1 bed");
        assert_eq!(f.sampling_frequency, 48_000);
        // DCA primary order for 3F2R is C,L,R,Ls,Rs then LFE.
        use bridge_api::RChannelLabel::*;
        let labels: Vec<_> = f.channel_labels.iter().copied().collect();
        assert_eq!(labels, vec![C, L, R, Ls, Rs, LFE]);
    }

    // End-to-end lossy carrier (DTS-HD HRA + DTS:X): the core + XXCH bed and
    // the height quartet come out as the same fixed 7.1.4 shape as a
    // lossless carrier's, named for what it is.
    #[cfg(feature = "dts")]
    #[test]
    fn lossy_carrier_raw_transport_emits_labeled_7_1_4_channels() {
        let Some(dump) = corpus_path("HARLETTY_LOSSY_X_CORPUS") else {
            eprintln!("skipping: HARLETTY_LOSSY_X_CORPUS is not set to a readable file");
            return;
        };
        let bytes = std::fs::read(dump).unwrap();
        let chunk = &bytes[..bytes.len().min(2_000_000)];
        let mut bridge = AtmosBridge::new(false);
        bridge.configure("input_codec".into(), "dts".into());
        let result = bridge.push_packet(RSlice::from_slice(chunk), RInputTransport::Raw, 0);
        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(!result.frames.is_empty(), "no HD frames decoded");
        assert!(!bridge.has_objects());
        assert_eq!(bridge.source_family().as_str(), "dts");
        assert_eq!(bridge.source_label().as_str(), "DTS-HD HRA + DTS:X 7.1.4");
        let f = result
            .frames
            .iter()
            .find(|f| f.channel_count == 12)
            .expect("expected a 12-channel 7.1.4 frame");
        assert_eq!(f.sampling_frequency, 48_000);
        use bridge_api::RChannelLabel::*;
        let labels: Vec<_> = f.channel_labels.iter().copied().collect();
        assert_eq!(
            labels,
            vec![C, L, R, Ls, Rs, LFE, Lb, Rb, Tfl, Tfr, Tbl, Tbr]
        );
        // Every frame after the lock has the full shape: the extension
        // decodes on every frame, no dropout.
        let first = result
            .frames
            .iter()
            .position(|f| f.channel_count == 12)
            .unwrap();
        assert!(result.frames[first..].iter().all(|f| f.channel_count == 12));
    }

    // End-to-end DTS-HD MA: feed the raw 7.1 dump and check it emits 8-channel
    // lossless bed frames. Skips when the (uncommitted) dump is absent.
    #[cfg(feature = "dts")]
    #[test]
    fn dtshd_raw_transport_emits_labeled_7_1_4_channels() {
        let Some(dump) = corpus_path("HARLETTY_DTSX_STANDARD_CORPUS") else {
            eprintln!("skipping: HARLETTY_DTSX_STANDARD_CORPUS is not set to a readable file");
            return;
        };
        // Feed ~2 MB — enough for many frames past the silent intro.
        let bytes = std::fs::read(dump).unwrap();
        let chunk = &bytes[..bytes.len().min(2_000_000)];
        let mut bridge = AtmosBridge::new(false);
        bridge.configure("input_codec".into(), "dts".into());
        let result = bridge.push_packet(RSlice::from_slice(chunk), RInputTransport::Raw, 0);
        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(!result.frames.is_empty(), "no HD frames decoded");
        // A DTS:X fixed 7.1.4 presentation is twelve labeled fixed channels —
        // no dynamic objects, no fabricated metadata: the renderer decides
        // placement (docs/channel-object-contract.md).
        assert!(!bridge.has_objects());

        // Once the XLL-X quartet locks, frames carry the fixed 7.1.4 shape.
        let f = result
            .frames
            .iter()
            .find(|f| f.channel_count == 12)
            .expect("expected a 12-channel 7.1.4 frame");
        assert_eq!(f.sampling_frequency, 48_000);
        assert!(
            f.metadata.is_empty(),
            "fixed presentation must carry no metadata"
        );
        use bridge_api::RChannelLabel::*;
        let labels: Vec<_> = f.channel_labels.iter().copied().collect();
        // Active speakers ascending (C,L,R,Ls,Rs,LFE,Lsr,Rsr) + the height quartet.
        assert_eq!(
            labels,
            vec![C, L, R, Ls, Rs, LFE, Lb, Rb, Tfl, Tfr, Tbl, Tbr]
        );
    }

    #[cfg(feature = "dts")]
    #[test]
    fn alternate_profiles_emit_automatic_presentations() {
        let Some(d0_path) = corpus_path("HARLETTY_D0_CORPUS") else {
            eprintln!("skipping: HARLETTY_D0_CORPUS is not set to a readable file");
            return;
        };
        let Some(bytes) = read_prefix(&d0_path, 2_000_000) else {
            eprintln!("skipping: D0 corpus could not be read");
            return;
        };
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("input_codec".into(), "dts".into()));
        let result = bridge.push_packet(RSlice::from_slice(&bytes), RInputTransport::Raw, 0);
        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(
            !bridge.has_objects(),
            "fixed D0 presentation must not set the object stream fact"
        );
        use bridge_api::RChannelLabel::*;
        let frame = result
            .frames
            .iter()
            .find(|frame| {
                [Tfc, Tfl, Tfr, Tbl, Tbr]
                    .iter()
                    .all(|label| frame.channel_labels.contains(label))
            })
            .expect("no experimental fixed D0 declaration");
        assert!(frame.metadata.is_empty());
        assert!(
            !frame.channel_labels.contains(&Object),
            "fixed D0 presentation must not fabricate objects"
        );

        let Some(d1_path) = corpus_path("HARLETTY_D1_CORPUS") else {
            eprintln!("skipping: HARLETTY_D1_CORPUS is not set to a readable file");
            return;
        };
        let Some(bytes) = read_prefix(&d1_path, 2_000_000) else {
            eprintln!("skipping: D1 corpus could not be read");
            return;
        };
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("input_codec".into(), "dts".into()));
        let result = bridge.push_packet(RSlice::from_slice(&bytes), RInputTransport::Raw, 0);
        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(bridge.has_objects(), "D1 declares its two objects");

        let frame = result
            .frames
            .iter()
            .find(|frame| {
                [Tfl, Tfr, Tbl, Tbr]
                    .iter()
                    .all(|label| frame.channel_labels.contains(label))
                    && frame
                        .channel_labels
                        .iter()
                        .filter(|&&label| label == Object)
                        .count()
                        == 2
            })
            .expect("no D1 presentation with four heights and two objects");
        assert_eq!(frame.channel_count, 8 + 6);
        assert!(
            !frame.channel_labels.contains(&Lw),
            "D1 carries no wide channels; its first two feeds are objects"
        );

        let Some(d3_path) = corpus_path("HARLETTY_D3_CORPUS") else {
            eprintln!("skipping: HARLETTY_D3_CORPUS is not set to a readable file");
            return;
        };
        let Some(bytes) = read_prefix(&d3_path, 2_000_000) else {
            eprintln!("skipping D3: alternate-extension corpus not present: {d3_path}");
            return;
        };
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("input_codec".into(), "dts".into()));
        let result = bridge.push_packet(RSlice::from_slice(&bytes), RInputTransport::Raw, 0);
        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(bridge.has_objects(), "D3 must expose object channels");

        let frame = result
            .frames
            .iter()
            .find(|frame| {
                frame
                    .channel_labels
                    .iter()
                    .filter(|&&label| label == Object)
                    .count()
                    == 4
                    && frame
                        .metadata
                        .iter()
                        .any(|metadata| metadata.name_updates.len() == 4)
            })
            .expect("no D3 object declaration");
        assert_eq!(frame.channel_count, 16);
        assert!(
            [Tfl, Tfr, Tbl, Tbr]
                .iter()
                .all(|label| frame.channel_labels.contains(label)),
            "the last four D3 feeds are the fixed heights"
        );
        let metadata = frame
            .metadata
            .iter()
            .find(|metadata| metadata.name_updates.len() == 4)
            .expect("no D3 name declaration");
        for source in 0..4 {
            assert_eq!(
                metadata.name_updates[source].name.as_str(),
                format!("X{source}")
            );
        }
        assert_eq!(
            metadata.events.len(),
            4,
            "every object announces a position"
        );
        assert!(metadata.events.iter().all(|event| event.has_pos));
    }

    /// Local-only end-to-end check: decode a real DTS (DTS-HD MA / DTS:X) stream
    /// through the bridge's raw path and confirm it emits a non-silent
    /// multichannel bed (the 2D core/HD path; DTS:X objects are ignored). Skips
    /// when the local capture is absent; dumps PCM for `compare_pcm` vs ffmpeg.
    #[cfg(feature = "dts")]
    #[test]
    fn dts_decodes_nonsilent_bed() {
        use abi_stable::std_types::RSlice;
        use bridge_api::{FormatBridge, RInputTransport};

        let path = "/tmp/dts/sample.dts";
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("skip: {path} not present");
            return;
        };

        let mut bridge = AtmosBridge::new(false);
        let mut pcm_f32: Vec<f32> = Vec::new();
        let mut channels = 0u32;
        let mut frames = 0u64;
        let mut labels = String::new();
        for chunk in bytes.chunks(4096) {
            let r = bridge.push_packet(RSlice::from_slice(chunk), RInputTransport::Raw, 0);
            for f in r.frames.iter() {
                frames += 1;
                channels = f.channel_count;
                if labels.is_empty() {
                    labels = format!("{:?}", f.channel_labels);
                }
                for &s in f.pcm.iter() {
                    pcm_f32.push(s as f32 / 8_388_608.0);
                }
            }
        }
        eprintln!("dts labels: {labels}");

        assert!(frames > 0, "no frames decoded from the DTS stream");
        assert!(channels >= 6, "expected >= 5.1, got {channels} channels");
        let peak = pcm_f32.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(peak > 1e-4, "decoded DTS bed is silent (peak={peak})");
        eprintln!("dts decoded {frames} frames, {channels} ch, peak={peak:.4}");
        let _ = std::fs::write(
            "/tmp/dts/harletty_bridge.f32",
            pcm_f32
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect::<Vec<u8>>(),
        );
    }

    /// Push one raw packet into a fresh bridge, or after a reset, and say
    /// what came of it.
    #[cfg(not(all(feature = "dolby", feature = "dts", feature = "iamf")))]
    fn refused_twice_then_after_reset(packet: &[u8]) -> (String, String, String, String) {
        let mut bridge = AtmosBridge::new(false);
        let push = |bridge: &mut AtmosBridge| {
            let result = bridge.push_packet(RSlice::from_slice(packet), RInputTransport::Raw, 0);
            assert!(result.frames.is_empty());
            result.error_message.to_string()
        };
        let first = push(&mut bridge);
        let second = push(&mut bridge);
        let family = bridge.source_family().to_string();
        bridge.reset();
        let after_reset = push(&mut bridge);
        (first, second, after_reset, family)
    }

    /// A stream whose family the build leaves out is refused by name: once
    /// per stream rather than on every packet, and again after a reset. It
    /// still names its own family.
    #[cfg(not(feature = "dts"))]
    #[test]
    fn a_dts_stream_without_dts_is_refused_by_name() {
        let msg = "dts: this bridge was built without DTS support";
        let (first, second, after_reset, family) =
            refused_twice_then_after_reset(&[0x7F, 0xFE, 0x80, 0x01, 0, 0, 0, 0]);
        assert_eq!(first, msg);
        assert_eq!(second, "");
        assert_eq!(after_reset, msg);
        assert_eq!(family, "dts");
    }

    #[cfg(not(feature = "dolby"))]
    #[test]
    fn a_dolby_stream_without_dolby_is_refused_by_name() {
        let (first, second, after_reset, family) =
            refused_twice_then_after_reset(&[0x0B, 0x77, 0, 0, 0, 0, 0, 0]);
        assert_eq!(first, "eac3: this bridge was built without E-AC-3 support");
        assert_eq!(second, "");
        assert_eq!(after_reset, first);
        assert_eq!(family, "dolby");
        let (first, .., family) =
            refused_twice_then_after_reset(&[0, 0, 0, 0, 0xF8, 0x72, 0x6F, 0xBA]);
        assert_eq!(
            first,
            "truehd: this bridge was built without TrueHD support"
        );
        assert_eq!(family, "dolby");
    }

    #[cfg(not(feature = "iamf"))]
    #[test]
    fn an_iamf_stream_without_iamf_is_refused_by_name() {
        let msg = "iamf: this bridge was built without IAMF support";
        let (first, second, after_reset, family) =
            refused_twice_then_after_reset(&[0xF8, 0x06, b'i', b'a', b'm', b'f', 0x00, 0x00]);
        assert_eq!(first, msg);
        assert_eq!(second, "");
        assert_eq!(after_reset, msg);
        assert_eq!(family, "iamf");
    }
}

#[cfg(test)]
mod unsupported_data_type_tests {
    use super::*;
    use bridge_api::RInputTransport;

    /// No family takes IEC 61937 data type 0x07.
    const UNSUPPORTED: u8 = 0x07;

    /// A host that keeps sending bursts of a type no family decodes gets them
    /// counted, not a warning for each; strict mode still fails every one.
    #[test]
    fn unsupported_bursts_are_counted_and_strict_mode_reports_each() {
        let burst = [0u8; 64];
        let mut bridge = AtmosBridge::new(false);
        for bursts in 1..=20 {
            let result = bridge.push_packet(
                RSlice::from_slice(&burst),
                RInputTransport::Iec61937,
                UNSUPPORTED,
            );
            assert!(result.frames.is_empty());
            assert!(result.error_message.is_empty(), "{}", result.error_message);
            assert!(!result.did_reset);
            assert_eq!(bridge.unsupported_bursts(), bursts);
        }
        bridge.reset_pipeline();
        assert_eq!(bridge.unsupported_bursts(), 0);

        let mut bridge = AtmosBridge::new(true);
        for _ in 0..3 {
            let result = bridge.push_packet(
                RSlice::from_slice(&burst),
                RInputTransport::Iec61937,
                UNSUPPORTED,
            );
            assert!(result.did_reset);
            assert_eq!(
                result.error_message.as_str(),
                "Unsupported IEC 61937 data type for this bridge: 0x07"
            );
        }
    }
}

#[cfg(all(test, feature = "dts"))]
mod dts_object_only_tests {
    use super::*;
    use abi_stable::std_types::RSlice;
    use bridge_api::RInputTransport;

    /// The object-only variant on a 5.1 bed presents six labeled bed channels
    /// plus one object channel, with a position on the object.
    #[test]
    fn object_only_stream_presents_5_1_plus_one_object() {
        let Some(path) = std::env::var("HARLETTY_ALT_51_CORPUS")
            .ok()
            .filter(|p| std::path::Path::new(p).is_file())
        else {
            eprintln!("skipping: HARLETTY_ALT_51_CORPUS is not set to a readable file");
            return;
        };
        let bytes = std::fs::read(&path).expect("read corpus");
        let bytes = &bytes[..bytes.len().min(4_000_000)];
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("input_codec".into(), "dts".into()));
        let result = bridge.push_packet(RSlice::from_slice(bytes), RInputTransport::Raw, 0);
        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(
            bridge.has_objects(),
            "the object-only variant declares its object"
        );
        use bridge_api::RChannelLabel::*;
        let frame = result
            .frames
            .iter()
            .find(|frame| frame.metadata.iter().any(|m| m.name_updates.len() == 1))
            .expect("no object declaration");
        assert_eq!(frame.channel_count, 7);
        assert_eq!(
            frame.channel_labels.as_slice(),
            &[C, L, R, Ls, Rs, LFE, Object]
        );
        let metadata = frame
            .metadata
            .iter()
            .find(|m| m.name_updates.len() == 1)
            .unwrap();
        assert_eq!(metadata.events.len(), 1);
        assert!(metadata.events[0].has_pos && metadata.events[0].pos[2] > 0.0);
        assert!(result.frames.len() > 100, "corpus was not exercised");
    }
}

#[cfg(all(test, feature = "dts"))]
mod dts_object_motion_tests {
    use super::*;
    use abi_stable::std_types::RSlice;
    use bridge_api::RInputTransport;

    /// A D3 stream with moving objects must produce position events whose
    /// coordinates change over time: the bridge follows the per-frame
    /// metadata rather than freezing the first position.
    #[test]
    fn d3_object_positions_follow_the_stream() {
        let Some(path) = std::env::var("HARLETTY_D3_MOTION_CORPUS")
            .ok()
            .filter(|p| std::path::Path::new(p).is_file())
        else {
            eprintln!("skipping: HARLETTY_D3_MOTION_CORPUS is not set to a readable file");
            return;
        };
        let bytes = std::fs::read(&path).expect("read corpus");
        let bytes = &bytes[..bytes.len().min(8_000_000)];
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("input_codec".into(), "dts".into()));
        let mut positions: std::collections::BTreeMap<u32, Vec<[i64; 3]>> = Default::default();
        let mut frames = 0usize;
        for chunk in bytes.chunks(256 * 1024) {
            let result = bridge.push_packet(RSlice::from_slice(chunk), RInputTransport::Raw, 0);
            assert!(result.error_message.is_empty(), "{}", result.error_message);
            for frame in result.frames.iter() {
                frames += 1;
                for metadata in frame.metadata.iter() {
                    for event in metadata.events.iter() {
                        let quantised =
                            [event.pos[0], event.pos[1], event.pos[2]].map(|v| (v * 1000.0) as i64);
                        let list = positions.entry(event.id).or_default();
                        if list.last() != Some(&quantised) {
                            list.push(quantised);
                        }
                    }
                }
            }
        }
        assert!(frames > 100, "corpus was not exercised ({frames} frames)");
        assert!(bridge.has_objects());
        let moving = positions.values().filter(|list| list.len() > 3).count();
        eprintln!(
            "frames={frames} objects={} distinct positions per object={:?}",
            positions.len(),
            positions.values().map(Vec::len).collect::<Vec<_>>()
        );
        assert!(
            moving >= 2,
            "at least two objects must change position: {positions:?}"
        );
    }
}

#[cfg(test)]
mod stack_footprint_tests {
    use super::*;

    /// Hosts may create the bridge on a small-stack thread — mpv's macOS
    /// playback thread is a bare `pthread_create`, so 512 KiB. Keeping the
    /// struct pointer-sized per decoder is what stops `new()` from copying
    /// hundreds of KiB across the three frames between the constructor and the
    /// heap. Box any decoder state added here; do not inline it.
    #[test]
    fn atmos_bridge_stack_footprint_stays_small() {
        const BUDGET: usize = 4 * 1024;
        let actual = std::mem::size_of::<AtmosBridge>();
        assert!(
            actual <= BUDGET,
            "AtmosBridge grew to {actual} bytes (budget {BUDGET}); \
             box the newly inlined decoder state instead"
        );
    }

    /// End-to-end guard for the same invariant: build a bridge on a thread
    /// sized like mpv's macOS playback thread. A by-value decoder field would
    /// overflow the guard page here exactly as it did in the field report
    /// (`SIGBUS` in `AtmosBridge::new`, Omniphony issue #205).
    #[test]
    fn new_fits_on_a_macos_sized_playback_thread() {
        const MACOS_PLAYBACK_STACK: usize = 512 * 1024;
        std::thread::Builder::new()
            .stack_size(MACOS_PLAYBACK_STACK)
            .spawn(|| {
                let bridge = AtmosBridge::new(false);
                assert!(!bridge.is_ready());
            })
            .expect("spawn small-stack thread")
            .join()
            .expect("bridge construction overflowed a 512 KiB stack");
    }
}

/// What a host sends a bridge it has just created, through the same entry
/// point (`get_library().new_bridge`) and in the same order: Omniphony's CLI,
/// live input and engine all stop when `presentation` is refused, before any
/// audio. Every family set must get through it.
#[cfg(test)]
mod host_initialization_tests {
    use bridge_api::FormatBridgeBox;

    fn host_bridge() -> FormatBridgeBox {
        crate::new_bridge(false)
    }

    #[test]
    fn the_default_presentation_is_accepted_by_every_build() {
        let mut bridge = host_bridge();
        assert!(bridge.configure("presentation".into(), "best".into()));
    }

    #[test]
    fn every_build_takes_the_same_presentation_values() {
        let mut bridge = host_bridge();
        for p in ["0", "1", "2", "3"] {
            assert!(
                bridge.configure("presentation".into(), p.into()),
                "presentation {p} refused"
            );
        }
        for p in ["4", "255", "-1", "highest", ""] {
            assert!(
                !bridge.configure("presentation".into(), p.into()),
                "presentation {p:?} accepted"
            );
        }
    }

    #[test]
    fn a_configured_presentation_leaves_the_bridge_idle_and_ready_for_audio() {
        let mut bridge = host_bridge();
        assert!(bridge.configure("presentation".into(), "best".into()));
        assert!(bridge.configure("input_codec".into(), "auto".into()));
        assert!(!bridge.is_ready());
        assert_eq!(bridge.source_family().as_str(), super::IDLE_FAMILY);
    }
}

#[cfg(test)]
mod panic_guard_tests {
    use super::*;

    #[cfg(feature = "dolby")]
    const EAC3: &[u8] = include_bytes!("../../harletty/tests/fixtures/joc_atmos_1s.eac3");
    #[cfg(feature = "dts")]
    const DTS: &[u8] = include_bytes!("../../harletty/tests/fixtures/dts_core_tone_10f.dts");

    fn push(bridge: &mut AtmosBridge, data: &[u8]) -> RPushResult {
        bridge.push_packet(RSlice::from_slice(data), RInputTransport::Raw, 0)
    }

    /// What a caught panic comes back as from a non-strict bridge, the kind
    /// every host creates: the pipeline reset, nothing decoded, no error. A
    /// panic still armed would mean `codec`'s path was never entered.
    fn assert_caught(codec: RawCodec, result: &RPushResult) {
        assert!(
            !injected_panic::is_armed(),
            "{codec:?}: the path was not entered"
        );
        assert!(result.did_reset, "{codec:?}: the pipeline is reset");
        assert!(result.frames.is_empty());
        assert!(
            result.error_message.is_empty(),
            "{codec:?}: a reset rather than an error, got {:?}",
            result.error_message
        );
    }

    /// Arm a panic in `codec`'s path, push `packet`: the panic comes back as
    /// a reset, not through the FFI boundary.
    fn assert_panic_is_caught(bridge: &mut AtmosBridge, codec: RawCodec, packet: &[u8]) {
        injected_panic::arm(codec);
        let result = push(bridge, packet);
        assert_caught(codec, &result);
    }

    /// Push `stream` in packets and count the frames it decodes.
    #[cfg(any(feature = "dolby", feature = "dts"))]
    fn decoded_frames(bridge: &mut AtmosBridge, stream: &[u8]) -> usize {
        stream
            .chunks(4096)
            .map(|packet| {
                let result = push(bridge, packet);
                assert!(result.error_message.is_empty(), "{}", result.error_message);
                result.frames.len()
            })
            .sum()
    }

    #[cfg(feature = "dolby")]
    #[test]
    fn a_panic_in_the_eac3_path_is_a_reset_and_the_stream_decodes_after_it() {
        let mut bridge = AtmosBridge::new(false);
        assert_panic_is_caught(&mut bridge, RawCodec::Eac3, &EAC3[..8192]);
        assert!(decoded_frames(&mut bridge, EAC3) > 0);
    }

    #[cfg(feature = "dts")]
    #[test]
    fn a_panic_in_the_dts_path_is_a_reset_and_the_stream_decodes_after_it() {
        let mut bridge = AtmosBridge::new(false);
        assert_panic_is_caught(&mut bridge, RawCodec::Dts, DTS);
        assert!(decoded_frames(&mut bridge, DTS) > 0);
    }

    /// Over IEC 61937 too: the guard is around the whole of `push_packet`.
    #[cfg(feature = "dts")]
    #[test]
    fn a_panic_in_the_iec61937_dts_path_is_a_reset() {
        let mut bridge = AtmosBridge::new(false);
        injected_panic::arm(RawCodec::Dts);
        let result = bridge.push_packet(
            RSlice::from_slice(&DTS[..2048]),
            RInputTransport::Iec61937,
            0x0B, // DTS type I: the frame goes in as it is
        );
        assert_caught(RawCodec::Dts, &result);
    }

    /// Strict mode gets the panic as an error as well, as it does every other
    /// decode failure.
    #[cfg(feature = "dts")]
    #[test]
    fn a_strict_bridge_reports_the_panic_as_an_error() {
        let mut bridge = AtmosBridge::new(true);
        injected_panic::arm(RawCodec::Dts);
        let result = push(&mut bridge, DTS);
        assert!(result.did_reset);
        assert!(
            result.error_message.contains("injected panic"),
            "{:?}",
            result.error_message
        );
    }

    /// TrueHD has no small fixture here: the panic is caught, and the bridge
    /// takes the next packet without one.
    #[cfg(feature = "dolby")]
    #[test]
    fn a_panic_in_the_truehd_path_is_a_reset() {
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("input_codec".into(), "truehd".into()));
        let packet = [0x00, 0x00, 0x00, 0x00, 0xF8, 0x72, 0x6F, 0xBA, 0, 0, 0, 0];
        assert_panic_is_caught(&mut bridge, RawCodec::TrueHd, &packet);
        let after = push(&mut bridge, &packet);
        assert!(
            !after.error_message.contains("panic"),
            "{}",
            after.error_message
        );
    }

    /// IAMF reaches the guard only in a build with the decoder in; without it
    /// the path refuses the stream before decoding anything.
    #[cfg(feature = "iamf")]
    #[test]
    fn a_panic_in_the_iamf_path_is_a_reset() {
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("input_codec".into(), "iamf".into()));
        let header = [0xF8, 0x06, b'i', b'a', b'm', b'f', 0x00, 0x00];
        assert_panic_is_caught(&mut bridge, RawCodec::Iamf, &header);
        let after = push(&mut bridge, &header);
        assert!(
            !after.error_message.contains("panic"),
            "{}",
            after.error_message
        );
    }
}
