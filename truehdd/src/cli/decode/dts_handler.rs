//! Turn decoded DTS frames into a DAMF master set (or plain audio).
//!
//! Modelled on the E-AC-3 handler: one message per decoded frame, PCM written
//! interleaved as bed channels then objects, metadata events appended as they
//! arrive. What differs is where the spatial description comes from — `dca`
//! reports a presentation per frame and `dts_to_oamd` projects it.

use super::atmos::create_damf_header_file;
use super::output::{AudioWriter, create_output_paths};
use crate::cli::command::{AudioFormat, WarpMode};
use crate::dts_to_oamd::{BedSource, DtsLayout, convert_dts};
use anyhow::Result;
use damf::{Configuration, Event, SourceCodec};
use dca::{CorePcmFrame, HdFrame, PcmPushResult, XPresentation};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

const I24_MAX: f32 = 8_388_607.0;

pub enum DtsFrameMessage {
    /// A lossless DTS-HD frame, with whatever spatial presentation it carries.
    Hd {
        frame: Box<HdFrame>,
        presentation: Option<XPresentation>,
    },
    /// A plain DTS core frame (5.1 lossy bed, no extension).
    Core(Box<PcmPushResult>),
}

pub struct DtsDecodeHandler {
    pub audio_writer: Option<AudioWriter>,
    pub damf_metadata_file_writer: Option<BufWriter<File>>,
    /// True once a frame carried a DTS:X spatial presentation, i.e. the output
    /// is a DAMF master set rather than a plain multichannel file. Set for any
    /// presentation, not just the object-bearing one: a fixed 7.1.4 DTS:X bed
    /// still needs a `.atmos` for Atmos Ranker to scan and rank the track.
    pub has_spatial: bool,
    pub prev_events: Vec<Event>,
    pub decoded_frames: u64,
    pub decoded_samples: u64,
    pub final_sample_rate: u32,
    pub final_channel_count: usize,
    pub warp_mode: Option<WarpMode>,
    /// Set of presentations already warned about, so an experimental profile is
    /// reported once rather than per frame.
    warned_presentations: Vec<XPresentation>,
    /// Layout of the previous frame, to detect a mid-stream shape change.
    last_layout: Option<DtsLayout>,
    /// Whether the metadata file already carries its `sampleRate` header.
    /// Tracked explicitly rather than inferred from `prev_events`: a fixed-bed
    /// presentation emits no events at all, so an inferred flag would re-emit
    /// the header for every frame.
    metadata_header_written: bool,
    /// Whether the dropped-channel warning has already been emitted.
    warned_dropped_channels: bool,
}

impl Default for DtsDecodeHandler {
    fn default() -> Self {
        Self {
            audio_writer: None,
            damf_metadata_file_writer: None,
            has_spatial: false,
            prev_events: Vec::new(),
            decoded_frames: 0,
            decoded_samples: 0,
            final_sample_rate: 48000,
            final_channel_count: 0,
            warp_mode: None,
            warned_presentations: Vec::new(),
            last_layout: None,
            metadata_header_written: false,
            warned_dropped_channels: false,
        }
    }
}

impl DtsDecodeHandler {
    pub fn handle_message(
        &mut self,
        msg: DtsFrameMessage,
        base_path: &Option<PathBuf>,
        format: AudioFormat,
        no_audio: bool,
    ) -> Result<()> {
        match msg {
            DtsFrameMessage::Hd {
                frame,
                presentation,
            } => self.handle_hd_frame(&frame, presentation, base_path, format, no_audio),
            DtsFrameMessage::Core(push) => {
                self.handle_core_frame(&push.pcm, base_path, format, no_audio)
            }
        }
    }

    fn handle_hd_frame(
        &mut self,
        frame: &HdFrame,
        presentation: Option<XPresentation>,
        base_path: &Option<PathBuf>,
        format: AudioFormat,
        no_audio: bool,
    ) -> Result<()> {
        self.warn_once(presentation);

        let sample_count = frame.bed_sample_count();
        if sample_count == 0 {
            return Ok(());
        }
        let active: Vec<usize> = (0..frame.samples.len())
            .filter(|&s| frame.samples[s].is_some())
            .collect();

        let layout = DtsLayout::from_hd(&active, presentation);
        // The output carries exactly what the master set declares: the bed the
        // layout resolved (speakers with no OAMD name, i.e. rear centre, are
        // dropped from both) plus one channel per object.
        let total_channels = layout.bed.len() + layout.objects.len();
        if total_channels < active.len() {
            self.warn_dropped_channels(active.len() - layout.bed.len());
        }

        self.note_frame(
            frame.sample_rate,
            total_channels,
            &layout,
            presentation.is_some(),
            base_path,
        )?;

        // A master set needs its metadata file even when the presentation is a
        // fixed bed with no objects: the events carry the bed's sample
        // positions and Atmos Ranker expects the pair.
        if let (Some(base_path), true) = (base_path, self.has_spatial) {
            let oamd = convert_dts(&layout);
            self.write_metadata_event(&oamd, frame.sample_rate, base_path)?;
        }

        if !no_audio {
            let audio_format = if self.has_spatial {
                AudioFormat::Caf
            } else {
                format
            };
            self.ensure_audio_writer(base_path, audio_format, frame.sample_rate, total_channels)?;
            self.write_hd_pcm(frame, &active, &layout, sample_count, total_channels)?;
        }

        self.decoded_samples += sample_count as u64;
        self.decoded_frames += 1;
        Ok(())
    }

    fn handle_core_frame(
        &mut self,
        core: &CorePcmFrame,
        base_path: &Option<PathBuf>,
        format: AudioFormat,
        no_audio: bool,
    ) -> Result<()> {
        let sample_count = core
            .fullband_channels
            .first()
            .map(Vec::len)
            .or_else(|| core.lfe_channel.as_ref().map(Vec::len))
            .unwrap_or(0);
        if sample_count == 0 {
            return Ok(());
        }
        let layout = DtsLayout::from_core(&core.fullband_channel_order, core.lfe_channel.is_some());
        let total_channels = layout.bed.len();

        // A plain core frame carries no spatial extension, so it never turns
        // the output into a master set.
        self.note_frame(core.sample_rate, total_channels, &layout, false, base_path)?;

        if !no_audio {
            self.ensure_audio_writer(base_path, format, core.sample_rate, total_channels)?;
            self.write_core_pcm(core, &layout, sample_count, total_channels)?;
        }

        self.decoded_samples += sample_count as u64;
        self.decoded_frames += 1;
        Ok(())
    }

    /// Record this frame's shape, writing the `.atmos` header on the first
    /// spatial frame and warning if the layout changes afterwards.
    fn note_frame(
        &mut self,
        sample_rate: u32,
        total_channels: usize,
        layout: &DtsLayout,
        is_spatial: bool,
        base_path: &Option<PathBuf>,
    ) -> Result<()> {
        self.final_sample_rate = sample_rate;
        self.final_channel_count = total_channels;

        if is_spatial && !self.has_spatial {
            self.has_spatial = true;
            if let Some(base_path) = base_path {
                let oamd = convert_dts(layout);
                if let Err(e) = create_damf_header_file(
                    base_path,
                    &oamd,
                    self.warp_mode,
                    SourceCodec::DtsX,
                ) {
                    log::error!("failed to write .atmos header: {e}");
                }
            }
        }

        match &self.last_layout {
            Some(previous) if previous != layout => {
                log::warn!(
                    "DTS layout changed mid-stream ({} bed / {} objects -> {} bed / {} objects); \
                     the master set describes the first layout",
                    previous.bed.len(),
                    previous.objects.len(),
                    layout.bed.len(),
                    layout.objects.len()
                );
            }
            _ => {}
        }
        self.last_layout = Some(layout.clone());
        Ok(())
    }

    fn warn_once(&mut self, presentation: Option<XPresentation>) {
        let Some(presentation) = presentation else {
            return;
        };
        if self.warned_presentations.contains(&presentation) {
            return;
        }
        self.warned_presentations.push(presentation);
        log::info!(
            "DTS:X spatial presentation: {presentation:?} ({} extension feeds, {})",
            presentation.feed_count(),
            match presentation.object_positions() {
                Some(positions) => format!("{} objects", positions.len()),
                None => "fixed channels".to_string(),
            }
        );
        if presentation.is_experimental() {
            log::warn!(
                "DTS:X {presentation:?} is an experimental presentation: its channel identities \
                 are inferred from a research corpus, not decoded metadata"
            );
        }
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
        let (audio_path, _) = create_output_paths(base_path, format, self.has_spatial);
        log::info!("Creating audio file: {}", audio_path.display());
        let writer = match (format, self.has_spatial) {
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

    /// Interleave in the order the master set declares: the bed in DAMF
    /// speaker order, then the objects. `layout.bed_sources` is what maps each
    /// declared position back to the decoded channel that carries it.
    fn write_hd_pcm(
        &mut self,
        frame: &HdFrame,
        active: &[usize],
        layout: &DtsLayout,
        sample_count: usize,
        total_channels: usize,
    ) -> Result<()> {
        let Some(ref mut writer) = self.audio_writer else {
            return Ok(());
        };
        let sample_at = |source: BedSource, idx: usize| -> f32 {
            match source {
                BedSource::Speaker(position) => active
                    .get(position)
                    .and_then(|&speaker| frame.samples[speaker].as_ref())
                    .and_then(|channel| channel.get(idx).copied())
                    .unwrap_or(0.0),
                BedSource::Feed(feed) => frame
                    .x_samples
                    .get(feed)
                    .and_then(|channel| channel.get(idx).copied())
                    .unwrap_or(0.0),
            }
        };

        let mut interleaved: Vec<i32> = Vec::with_capacity(sample_count * total_channels);
        for sample_idx in 0..sample_count {
            for &source in &layout.bed_sources {
                interleaved.push(float_to_i24(sample_at(source, sample_idx)));
            }
            for &feed in &layout.object_sources {
                interleaved.push(float_to_i24(
                    frame
                        .x_samples
                        .get(feed)
                        .and_then(|channel| channel.get(sample_idx).copied())
                        .unwrap_or(0.0),
                ));
            }
        }
        writer.write_pcm_samples(&interleaved, total_channels)?;
        Ok(())
    }

    fn warn_dropped_channels(&mut self, dropped: usize) {
        if self.warned_dropped_channels || dropped == 0 {
            return;
        }
        self.warned_dropped_channels = true;
        log::warn!(
            "{dropped} decoded channel(s) have no OAMD speaker equivalent (rear centre) and are              omitted, so the audio matches the bed the master set declares"
        );
    }

    /// Same contract as `write_hd_pcm`: declared order, not decoder order. The
    /// core decoder hands back fullband channels then LFE, so LFE's source
    /// index is one past the last fullband channel.
    fn write_core_pcm(
        &mut self,
        core: &CorePcmFrame,
        layout: &DtsLayout,
        sample_count: usize,
        total_channels: usize,
    ) -> Result<()> {
        let Some(ref mut writer) = self.audio_writer else {
            return Ok(());
        };
        let mut interleaved: Vec<i32> = Vec::with_capacity(sample_count * total_channels);
        for sample_idx in 0..sample_count {
            for &source in &layout.bed_sources {
                let BedSource::Speaker(position) = source else {
                    continue; // a core frame has no extension feeds
                };
                let sample = core
                    .fullband_channels
                    .get(position)
                    .or(core.lfe_channel.as_ref())
                    .and_then(|channel| channel.get(sample_idx).copied())
                    .unwrap_or(0.0);
                interleaved.push(float_to_i24(sample));
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
        let events_diff = if self.prev_events.is_empty() {
            conf.events.clone()
        } else {
            Event::compare_event_vectors(&self.prev_events, &conf.events)
        };
        self.prev_events = conf.events.clone();

        // Nothing changed since the last frame and the header is already out:
        // emitting an empty block per frame would just pad the file.
        if events_diff.is_empty() && self.metadata_header_written {
            return Ok(());
        }

        conf.events = events_diff;
        let serialized = conf.serialize_events(self.metadata_header_written);

        if self.damf_metadata_file_writer.is_none() {
            let (_, metadata_path) = create_output_paths(base_path, AudioFormat::Caf, true);
            log::info!("Creating metadata file: {}", metadata_path.display());
            self.damf_metadata_file_writer = Some(BufWriter::new(File::create(metadata_path)?));
        }
        if let Some(ref mut writer) = self.damf_metadata_file_writer {
            write!(writer, "{serialized}")?;
            writer.flush()?;
            self.metadata_header_written = true;
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
    (sample.clamp(-1.0, 1.0) * I24_MAX) as i32
}
