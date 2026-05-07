use eac3::{CorePcmFrame, PcmDecoder};

#[derive(Debug, Default)]
pub(crate) struct NativeAc3Decoder {
    decoder: PcmDecoder,
}

impl NativeAc3Decoder {
    pub(crate) fn reset(&mut self) {
        self.decoder.reset();
    }

    pub(crate) fn decode_frame(&mut self, frame: &[u8]) -> Result<CorePcmFrame, String> {
        self.decoder
            .push_legacy_ac3_access_unit(frame)
            .map(|result| result.pcm)
            .map_err(|err| format!("native AC-3 decode error: {err}"))
    }
}
