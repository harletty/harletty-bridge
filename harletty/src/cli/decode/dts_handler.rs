//! Turn decoded DTS frames into a DAMF master set (or plain audio).
//!
//! Modelled on the E-AC-3 handler: one message per decoded frame, PCM written
//! interleaved as bed channels then objects, metadata events appended as they
//! arrive. What differs is where the spatial description comes from — `dca`
//! reports a presentation per frame and `dts_to_oamd` projects it.

use super::atmos::create_damf_header_file;
use super::output::{AudioWriter, create_output_paths, float_to_i24};
use crate::cli::command::{AudioFormat, WarpMode};
use crate::dts_to_oamd::{BedSource, DtsLayout, convert_dts};
use anyhow::Result;
use damf::{Configuration, Event, SourceCodec};
use dca::{
    CorePcmFrame, FoldEstimator, FoldPlan, HdFrame, PcmPushResult, XMetadata, XPresentation,
};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

pub enum DtsFrameMessage {
    /// A lossless DTS-HD frame, with whatever spatial presentation it carries.
    Hd {
        frame: Box<HdFrame>,
        presentation: Option<XPresentation>,
        /// The frame's private metadata (object positions and bed folds),
        /// or `None` when it did not parse.
        metadata: Option<XMetadata>,
    },
    /// A plain DTS core frame (5.1 lossy bed, no extension).
    Core(Box<PcmPushResult>),
    /// The lossless PCM turned out to be an Auro-Codec carrier. Sent once,
    /// when the side channel's configuration is confirmed.
    Auro(auro::Detection),
    /// Unfolded Auro-3D audio: the streams of the original layout,
    /// interleaved. Replaces the `Hd` frames from the moment the carrier
    /// is confirmed.
    AuroFrame(Box<AuroFrame>),
}

pub struct AuroFrame {
    pub sample_rate: u32,
    /// The streams, in interleaving order.
    pub streams: Vec<auro::StreamId>,
    /// `frames * streams.len()` samples, 24-bit in `i32`.
    pub samples: Vec<i32>,
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
    /// Whether the unreadable-metadata warning has already been emitted.
    warned_unreadable_metadata: bool,
    /// Estimate the fold of a waveform the stream states none for, from the
    /// bed's audio, rather than keeping it in the bed muted.
    pub estimate_folds: bool,
    /// Keep the bed to what an Atmos bed can hold (7.1.2 at most): a DTS:X
    /// or Auro-3D layout's corner heights and wides become static objects.
    pub bed_conform: bool,
    /// Write one mono WAV per channel, `<prefix>_<n>.wav`, instead of the
    /// interleaved audio file.
    pub mono_prefix: Option<PathBuf>,
    estimator: FoldEstimator,
    /// Whether the estimation has been announced.
    noted_estimation: bool,
    /// Label for the master set, chosen from the presentation of the first
    /// spatial frame.
    source_codec: SourceCodec,
    /// The Auro-Codec carrier the lossless PCM was found to be, if any.
    pub auro: Option<auro::Detection>,
}

/// Which DAMF codec label an unfolded Auro-3D layout is stored under: its
/// channel count when it is one of the common ones.
fn auro_source_codec(original: auro::Layout) -> SourceCodec {
    match original.source_codec_label() {
        "Auro-3D-9.1" => SourceCodec::Auro3d91,
        "Auro-3D-10.1" => SourceCodec::Auro3d101,
        "Auro-3D-11.1" => SourceCodec::Auro3d111,
        "Auro-3D-13.1" => SourceCodec::Auro3d131,
        _ => SourceCodec::Auro3d,
    }
}

/// The DAMF codec label of a spatial presentation, as the header writes it.
pub(crate) fn presentation_label(presentation: XPresentation) -> &'static str {
    match presentation {
        XPresentation::Height => "DTS:X-7.1.4",
        XPresentation::FixedD0 => "DTS:X-7.1.5",
        XPresentation::ObjectsD1 => "DTS:X-7.1.4+2",
        XPresentation::ObjectsD3 => "DTS:X-7.1.4+4",
        XPresentation::ObjectsD4 => "DTS:X-7.1.4+5",
        XPresentation::ObjectOnly => "DTS:X-5.1+1",
    }
}

/// Which DAMF codec label a spatial presentation is stored under.
///
/// The taxonomy is Atmos Ranker's, derived independently at scan time from the
/// alternate-profile syncwords; both sides must agree or the Rank codec filter
/// splits. [`SourceCodec`] says what that agreement rests on.
fn source_codec_for(presentation: XPresentation) -> SourceCodec {
    match presentation {
        XPresentation::Height => SourceCodec::DtsX714,
        XPresentation::FixedD0 => SourceCodec::DtsX715,
        XPresentation::ObjectsD1 => SourceCodec::DtsX714Plus2,
        XPresentation::ObjectsD3 => SourceCodec::DtsX714Plus4,
        XPresentation::ObjectsD4 => SourceCodec::DtsX714Plus5,
        XPresentation::ObjectOnly => SourceCodec::DtsX51Plus1,
    }
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
            warned_unreadable_metadata: false,
            estimate_folds: true,
            bed_conform: false,
            mono_prefix: None,
            estimator: FoldEstimator::new(),
            noted_estimation: false,
            source_codec: SourceCodec::DtsX714,
            auro: None,
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
                metadata,
            } => self.handle_hd_frame(&frame, presentation, metadata, base_path, format, no_audio),
            DtsFrameMessage::Core(push) => {
                self.handle_core_frame(&push.pcm, base_path, format, no_audio)
            }
            DtsFrameMessage::Auro(detection) => {
                self.note_auro(detection);
                Ok(())
            }
            DtsFrameMessage::AuroFrame(frame) => {
                self.handle_auro_frame(&frame, base_path, no_audio)
            }
        }
    }

    /// Write one unfolded Auro-3D frame. The output is always a master
    /// set: the bed the layout resolved plus the centre height and top as
    /// static objects.
    fn handle_auro_frame(
        &mut self,
        frame: &AuroFrame,
        base_path: &Option<PathBuf>,
        no_audio: bool,
    ) -> Result<()> {
        let ns = frame.streams.len();
        if ns == 0 || frame.samples.len() < ns {
            return Ok(());
        }
        let sample_count = frame.samples.len() / ns;
        let layout = DtsLayout::from_auro(&frame.streams, self.bed_conform);
        let total_channels = layout.bed.len() + layout.objects.len();
        if total_channels < ns {
            self.warn_dropped_channels(ns - total_channels);
        }
        if !self.has_spatial {
            self.source_codec = self
                .auro
                .map(|d| auro_source_codec(d.original))
                .unwrap_or(SourceCodec::Auro3d);
        }
        self.note_frame(frame.sample_rate, total_channels, &layout, true, base_path)?;
        if let Some(base_path) = base_path {
            let oamd = convert_dts(&layout);
            self.write_metadata_event(&oamd, frame.sample_rate, base_path)?;
        }
        if !no_audio {
            self.ensure_audio_writer(
                base_path,
                AudioFormat::Caf,
                frame.sample_rate,
                total_channels,
            )?;
            if let Some(ref mut writer) = self.audio_writer {
                let mut interleaved: Vec<i32> = Vec::with_capacity(sample_count * total_channels);
                for sample_idx in 0..sample_count {
                    let row = &frame.samples[sample_idx * ns..(sample_idx + 1) * ns];
                    for &source in &layout.bed_sources {
                        let BedSource::Speaker(index) = source else {
                            continue;
                        };
                        interleaved.push(row[index]);
                    }
                    for &index in &layout.object_sources {
                        interleaved.push(row[index]);
                    }
                }
                writer.write_pcm_samples(&interleaved, total_channels)?;
            }
        }
        self.decoded_samples += sample_count as u64;
        self.decoded_frames += 1;
        Ok(())
    }

    /// Say what the carrier holds and what the output will be.
    fn note_auro(&mut self, detection: auro::Detection) {
        let name = |layout: auro::Layout| layout.name().unwrap_or("unknown layout");
        log::info!(
            "Auro-3D carrier: {} folded into {} ({}-sample blocks, channel configuration {}); \
             unfolding to a {} master set",
            name(detection.original),
            name(detection.carrier),
            detection.block_size,
            detection.config.0,
            name(detection.original),
        );
        self.auro = Some(detection);
    }

    fn handle_hd_frame(
        &mut self,
        frame: &HdFrame,
        presentation: Option<XPresentation>,
        metadata: Option<XMetadata>,
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

        let layout = DtsLayout::from_hd(&active, presentation, metadata.as_ref(), self.bed_conform);
        let mut plan = self.fold_plan(presentation, metadata.as_ref());
        if presentation.is_some() && self.estimate_folds && plan.has_unknown() {
            if !self.noted_estimation {
                self.noted_estimation = true;
                log::info!(
                    "DTS:X {:?} carries waveform(s) without a stated bed fold; estimating their fold from the bed",
                    presentation.expect("checked")
                );
            }
            self.estimator
                .refine(&mut plan, &frame.samples, &frame.x_samples);
        }
        // The output carries exactly what the master set declares: the bed the
        // layout resolved (speakers with no OAMD name, i.e. rear centre, are
        // dropped from both) plus one channel per object.
        let total_channels = layout.bed.len() + layout.objects.len();
        if total_channels < active.len() {
            self.warn_dropped_channels(active.len() - layout.bed.len());
        }

        if let Some(presentation) = presentation {
            if !self.has_spatial {
                self.source_codec = source_codec_for(presentation);
            }
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
            self.write_hd_pcm(frame, &active, &layout, &plan, sample_count, total_channels)?;
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
                if let Err(e) =
                    create_damf_header_file(base_path, &oamd, self.warp_mode, self.source_codec)
                {
                    log::error!("failed to write .atmos header: {e}");
                }
            }
        }

        match &self.last_layout {
            Some(previous)
                if previous.bed != layout.bed || previous.objects.len() != layout.objects.len() =>
            {
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
            match presentation.object_feeds().len() {
                0 => "fixed channels".to_string(),
                objects => format!("{objects} objects"),
            }
        );
        if presentation.is_experimental() {
            log::warn!(
                "DTS:X {presentation:?} is an experimental presentation: its feed identities rest \
                 on corpus evidence; positions and bed folds come from the stream's metadata"
            );
        }
    }

    /// The bed-fold plan for this frame, mirroring the realtime bridge: the
    /// frame's metadata when readable, the standard -3 dB height fold when a
    /// standard frame's matrix is unreadable, otherwise nothing is removed
    /// and every extension feed is muted so nothing plays twice.
    fn fold_plan(
        &mut self,
        presentation: Option<XPresentation>,
        metadata: Option<&XMetadata>,
    ) -> FoldPlan {
        const STANDARD_HEIGHT_GAIN: f32 = 23_170.0 / 32_768.0;
        match (presentation, metadata) {
            (Some(_), Some(metadata)) => FoldPlan::from_metadata(metadata),
            (Some(presentation), None) => {
                if !self.warned_unreadable_metadata {
                    self.warned_unreadable_metadata = true;
                    log::warn!(
                        "DTS:X {presentation:?} metadata unreadable: {}",
                        if presentation == XPresentation::Height {
                            "assuming the standard -3 dB height fold"
                        } else if self.estimate_folds {
                            "estimating the extension feeds' fold from the bed"
                        } else {
                            "keeping the bed as authored and muting the extension feeds"
                        }
                    );
                }
                if presentation == XPresentation::Height {
                    FoldPlan::standard_heights(STANDARD_HEIGHT_GAIN)
                } else {
                    FoldPlan::all_unknown(presentation.feed_count())
                }
            }
            (None, _) => FoldPlan::all_unknown(0),
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
        if let Some(prefix) = &self.mono_prefix {
            log::info!(
                "Creating {channel_count} mono audio files: {}",
                super::output::mono_path(prefix, 0).display()
            );
            self.audio_writer = Some(AudioWriter::create_mono(prefix, sample_rate, channel_count)?);
            return Ok(());
        }
        let (audio_path, _) = create_output_paths(base_path, format, self.has_spatial);
        log::info!("Creating audio file: {}", audio_path.display());
        let writer = match (format, self.has_spatial) {
            (AudioFormat::Caf, _) | (_, true) => {
                AudioWriter::create_caf(audio_path, sample_rate, channel_count as u32, &[])?
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
        plan: &FoldPlan,
        sample_count: usize,
        total_channels: usize,
    ) -> Result<()> {
        let Some(ref mut writer) = self.audio_writer else {
            return Ok(());
        };
        // A feed whose bed fold is not stated stays in the bed and is muted
        // on its own channel, exactly as the realtime bridge does.
        let feed_at = |feed: usize, idx: usize| -> f32 {
            if !plan.source_is_known(feed) {
                return 0.0;
            }
            frame
                .x_samples
                .get(feed)
                .and_then(|channel| channel.get(idx).copied())
                .unwrap_or(0.0)
        };
        let sample_at = |source: BedSource, idx: usize| -> f32 {
            match source {
                BedSource::Speaker(position) => active
                    .get(position)
                    .and_then(|&speaker| {
                        frame.samples[speaker]
                            .as_ref()
                            .and_then(|channel| channel.get(idx).copied())
                            .map(|value| plan.clean(speaker, value, idx, &frame.x_samples))
                    })
                    .unwrap_or(0.0),
                BedSource::Feed(feed) => feed_at(feed, idx),
            }
        };

        let mut interleaved: Vec<i32> = Vec::with_capacity(sample_count * total_channels);
        for sample_idx in 0..sample_count {
            for &source in &layout.bed_sources {
                interleaved.push(float_to_i24(sample_at(source, sample_idx)));
            }
            for &feed in &layout.object_sources {
                interleaved.push(float_to_i24(feed_at(feed, sample_idx)));
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
        let mut conf = Configuration::with_oamd_payload(oamd, sample_rate, self.decoded_samples)?;
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
