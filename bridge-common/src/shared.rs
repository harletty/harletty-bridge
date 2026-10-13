use abi_stable::std_types::RVec;

/// The state every codec path reads or writes besides its own: what the host
/// set at creation, the stream position, and the object↔channel declaration
/// last emitted. Kept apart from the per-codec state so each path can be
/// handed exactly its own fields plus this, instead of the whole bridge.
pub struct SharedState {
    /// Report decode errors instead of recovering silently (see
    /// `BridgeLib::new_bridge`).
    pub strict: bool,
    /// Running total of decoded samples (used for metadata timestamping).
    /// Not cleared by a reset: it tracks the global position for
    /// continuous-mode timestamping.
    pub total_samples: u64,
    /// Last object↔channel declaration emitted (sparse re-emission on change
    /// and after reset). Shared by the TrueHD, E-AC-3 and DTS metadata paths.
    pub declared_object_channels: Option<RVec<bridge_api::RObjectChannel>>,
}

impl SharedState {
    pub fn new(strict: bool) -> Self {
        Self {
            strict,
            total_samples: 0,
            declared_object_channels: None,
        }
    }

    /// The level a decoder reports its failures at: a strict host sees them
    /// as warnings, a lenient one only when they are errors.
    pub fn fail_level(&self) -> log::Level {
        if self.strict {
            log::Level::Warn
        } else {
            log::Level::Error
        }
    }
}

/// What a codec path asks of the bridge once it has handled a packet. A path
/// owns only its own state, so resetting the whole pipeline — every codec, the
/// sniffed codec, the shared declarations — is the bridge's to do.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterPush {
    Continue,
    /// Reset the whole pipeline before returning the result.
    ResetPipeline,
}
