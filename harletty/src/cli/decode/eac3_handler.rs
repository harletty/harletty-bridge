use super::atmos::create_damf_header_file;
use super::output::{AudioWriter, create_output_paths};
use crate::cli::command::{AudioFormat, WarpMode};
use damf::{Configuration, Event, SourceCodec};
use crate::eac3_to_oamd::convert_oamd;
use anyhow::Result;
use eac3::{CorePcmFrame, ObjectPcmPushResult, PcmPushResult};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

const I24_MAX: f32 = 8_388_607.0;

pub enum Eac3FrameMessage {
    Object(ObjectPcmPushResult),
    Core(PcmPushResult),
    Silence {
        sample_count: usize,
        sample_rate: u32,
        channel_count: usize,
    },
}

pub struct Eac3DecodeHandler {
    pub audio_writer: Option<AudioWriter>,
    pub damf_metadata_file_writer: Option<BufWriter<File>>,
    pub has_atmos: bool,
    pub prev_events: Vec<Event>,
    pub decoded_frames: u64,
    pub decoded_samples: u64,
    pub final_sample_rate: u32,
    pub final_channel_count: usize,
    pub warp_mode: Option<WarpMode>,
    pub warned_experimental: bool,
}

impl Default for Eac3DecodeHandler {
    fn default() -> Self {
        Self {
            audio_writer: None,
            damf_metadata_file_writer: None,
            has_atmos: false,
            prev_events: Vec::new(),
            decoded_frames: 0,
            decoded_samples: 0,
            final_sample_rate: 48000,
            final_channel_count: 0,
            warp_mode: None,
            warned_experimental: false,
        }
    }
}

impl Eac3DecodeHandler {
    pub fn handle_message(
        &mut self,
        msg: Eac3FrameMessage,
        base_path: &Option<PathBuf>,
        format: AudioFormat,
        no_audio: bool,
    ) -> Result<()> {
        match msg {
            Eac3FrameMessage::Object(result) => {
                self.handle_object_frame(&result, base_path, no_audio)?;
            }
            Eac3FrameMessage::Core(result) => {
                self.handle_core_frame(&result, base_path, format, no_audio)?;
            }
            Eac3FrameMessage::Silence {
                sample_count,
                sample_rate,
                channel_count,
            } => {
                self.handle_silence(sample_count, sample_rate, channel_count, base_path, format, no_audio)?;
            }
        }
        Ok(())
    }

    fn handle_object_frame(
        &mut self,
        result: &ObjectPcmPushResult,
        base_path: &Option<PathBuf>,
        no_audio: bool,
    ) -> Result<()> {
        if !self.warned_experimental {
            log::warn!(
                "EAC3/JOC decoding is experimental — see harletty-bridge EAC3_PATCH_NOTES.md for known limitations"
            );
            self.warned_experimental = true;
        }

        let pcm = &result.pcm;
        let sample_rate = pcm.core.sample_rate;
        let sample_count = pcm.samples_per_channel();
        let has_lfe = pcm.core.lfe_channel.is_some();
        let bed_channel_count = pcm.core.fullband_channels.len() + usize::from(has_lfe);
        let object_count = pcm.object_count();
        let total_channels = bed_channel_count + object_count;

        let was_atmos = self.has_atmos;
        self.has_atmos = true;
        self.final_sample_rate = sample_rate;
        self.final_channel_count = total_channels;

        // First Atmos frame: write .atmos header from converted OAMD (if any).
        if !was_atmos {
            if let Some(base_path) = base_path {
                if let Some((eac3_oamd, sample_offset)) = pcm.oamd_payloads.first() {
                    let converted = convert_oamd(eac3_oamd, *sample_offset);
                    if let Err(e) = create_damf_header_file(
                        base_path,
                        &converted,
                        self.warp_mode,
                        SourceCodec::Eac3Joc,
                    ) {
                        log::error!("failed to write .atmos header: {e}");
                    }
                }
            }
        }

        // Append metadata events.
        if let Some(base_path) = base_path {
            for (eac3_oamd, sample_offset) in &pcm.oamd_payloads {
                let converted = convert_oamd(eac3_oamd, *sample_offset);
                self.write_metadata_event(&converted, sample_rate, base_path)?;
            }
        }

        // Write interleaved PCM (bed channels then objects).
        if !no_audio {
            self.ensure_audio_writer(base_path, AudioFormat::Caf, sample_rate, total_channels)?;
            self.write_object_pcm(&pcm.core, &pcm.object_channels, total_channels)?;
        }

        self.decoded_samples += sample_count as u64;
        self.decoded_frames += 1;
        Ok(())
    }

    fn handle_core_frame(
        &mut self,
        result: &PcmPushResult,
        base_path: &Option<PathBuf>,
        format: AudioFormat,
        no_audio: bool,
    ) -> Result<()> {
        let core = &result.pcm;
        let sample_rate = core.sample_rate;
        let sample_count = core.samples_per_channel();
        let total_channels = core.total_channels();

        self.final_sample_rate = sample_rate;
        self.final_channel_count = total_channels;

        if !no_audio {
            self.ensure_audio_writer(base_path, format, sample_rate, total_channels)?;
            self.write_core_pcm(core, total_channels)?;
        }

        self.decoded_samples += sample_count as u64;
        self.decoded_frames += 1;
        Ok(())
    }

    fn handle_silence(
        &mut self,
        sample_count: usize,
        sample_rate: u32,
        channel_count: usize,
        base_path: &Option<PathBuf>,
        format: AudioFormat,
        no_audio: bool,
    ) -> Result<()> {
        if no_audio || channel_count == 0 || sample_count == 0 {
            return Ok(());
        }
        self.final_sample_rate = sample_rate;
        self.final_channel_count = channel_count;
        self.ensure_audio_writer(base_path, format, sample_rate, channel_count)?;
        if let Some(ref mut writer) = self.audio_writer {
            let buf = vec![0i32; sample_count * channel_count];
            writer.write_pcm_samples(&buf, channel_count)?;
        }
        self.decoded_samples += sample_count as u64;
        Ok(())
    }

    fn ensure_audio_writer(
        &mut self,
        base_path: &Option<PathBuf>,
        format: AudioFormat,
        sample_rate: u32,
        channel_count: usize,
    ) -> Result<()> {
        if self.audio_writer.is_some() {
            return Ok(());
        }
        let Some(base_path) = base_path else {
            return Ok(());
        };
        let (audio_path, _) = create_output_paths(base_path, format, self.has_atmos);
        log::info!("Creating audio file: {}", audio_path.display());
        let writer = match (format, self.has_atmos) {
            (AudioFormat::Caf, _) | (_, true) => {
                AudioWriter::create_caf(audio_path, sample_rate, channel_count as u32)?
            }
            (AudioFormat::W64, false) => {
                AudioWriter::create_w64(audio_path, sample_rate, channel_count as u32)?
            }
            (AudioFormat::Pcm, false) => AudioWriter::create_pcm(audio_path)?,
        };
        self.audio_writer = Some(writer);
        Ok(())
    }

    fn write_object_pcm(
        &mut self,
        core: &CorePcmFrame,
        object_channels: &[Vec<f32>],
        total_channels: usize,
    ) -> Result<()> {
        let Some(ref mut writer) = self.audio_writer else {
            return Ok(());
        };
        let sample_count = core.samples_per_channel();
        let mut interleaved: Vec<i32> = Vec::with_capacity(sample_count * total_channels);
        for sample_idx in 0..sample_count {
            for channel in &core.fullband_channels {
                interleaved.push(float_to_i24(channel.get(sample_idx).copied().unwrap_or(0.0)));
            }
            if let Some(lfe) = &core.lfe_channel {
                interleaved.push(float_to_i24(lfe.get(sample_idx).copied().unwrap_or(0.0)));
            }
            for object in object_channels {
                interleaved.push(float_to_i24(object.get(sample_idx).copied().unwrap_or(0.0)));
            }
        }
        writer.write_pcm_samples(&interleaved, total_channels)?;
        Ok(())
    }

    fn write_core_pcm(&mut self, core: &CorePcmFrame, total_channels: usize) -> Result<()> {
        let Some(ref mut writer) = self.audio_writer else {
            return Ok(());
        };
        let sample_count = core.samples_per_channel();
        let mut interleaved: Vec<i32> = Vec::with_capacity(sample_count * total_channels);
        for sample_idx in 0..sample_count {
            for channel in &core.fullband_channels {
                interleaved.push(float_to_i24(channel.get(sample_idx).copied().unwrap_or(0.0)));
            }
            if let Some(lfe) = &core.lfe_channel {
                interleaved.push(float_to_i24(lfe.get(sample_idx).copied().unwrap_or(0.0)));
            }
        }
        writer.write_pcm_samples(&interleaved, total_channels)?;
        Ok(())
    }

    fn write_metadata_event(
        &mut self,
        oamd: &truehd::structs::oamd::ObjectAudioMetadataPayload,
        sample_rate: u32,
        base_path: &PathBuf,
    ) -> Result<()> {
        let mut conf = Configuration::with_oamd_payload(oamd, sample_rate, self.decoded_samples);
        let (events_diff, remove_header) = if !self.prev_events.is_empty() {
            (Event::compare_event_vectors(&self.prev_events, &conf.events), true)
        } else {
            (conf.events.clone(), false)
        };
        self.prev_events = conf.events.clone();
        conf.events = events_diff;
        let serialized = conf.serialize_events(remove_header);

        if self.damf_metadata_file_writer.is_none() {
            let (_, metadata_path) = create_output_paths(base_path, AudioFormat::Caf, true);
            log::info!("Creating metadata file: {}", metadata_path.display());
            self.damf_metadata_file_writer =
                Some(BufWriter::new(File::create(metadata_path)?));
        }
        if let Some(ref mut writer) = self.damf_metadata_file_writer {
            write!(writer, "{serialized}")?;
            writer.flush()?;
        }
        Ok(())
    }

    pub fn finalize(&mut self) -> Result<()> {
        if let Some(mut writer) = self.audio_writer.take() {
            writer.finish()?;
        }
        if let Some(mut writer) = self.damf_metadata_file_writer.take() {
            writer.flush()?;
        }
        Ok(())
    }
}

#[inline]
fn float_to_i24(sample: f32) -> i32 {
    // NaN must map to silence, not rely on `as`-cast saturation semantics:
    // a decoder bug upstream must never turn into full-scale output.
    if !sample.is_finite() {
        return 0;
    }
    (sample.clamp(-1.0, 1.0) * I24_MAX) as i32
}
