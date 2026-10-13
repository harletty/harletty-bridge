//! Turn decoded DTS frames into a DAMF master set (or plain audio).
//!
//! Modelled on the E-AC-3 handler: one message per decoded frame, PCM written
//! interleaved as bed channels then objects, metadata events appended as they
//! arrive. What differs is where the spatial description comes from — `dca`
//! reports a presentation per frame and `dts_to_oamd` projects it.
//!
//! The layout is a frame's; the audio file has one, that of the frame it was
//! opened for ([`FileLayout`]), and every frame is written in it.

use super::atmos::create_damf_header_file;
use super::output::{AudioWriter, PcmReadBack, create_output_paths, float_to_i24, mono_path};
use crate::cli::command::{AudioFormat, WarpMode};
use crate::dts_to_oamd::{BedSource, DtsLayout, convert_dts};
use anyhow::Result;
use damf::{Configuration, Event, SourceCodec};
use dca::{
    CorePcmFrame, FoldEstimator, FoldPlan, FoldRenderer, HdFrame, PcmPushResult, XMetadata,
    XPresentation,
};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::iter::StepBy;
use std::path::{Path, PathBuf};
use std::slice::IterMut;
use truehd::structs::oamd::SpeakerLabels;

/// Sample frames handled at a time when a file is rewritten: bounds the
/// scratch whatever the length of what was written.
const CHUNK_FRAMES: usize = 4096;

/// The channels of the audio file, in file order: fixed by the frame the file
/// is opened for, and what every later frame is written in.
///
/// An audio file has one sample frame size and a stream does not. A DTS-HD
/// stream delivers a plain core frame wherever its extension substream is
/// missing or fails to decode, a frame's spatial extension may not be
/// recognised, and streams joined end to end agree on nothing. Written with
/// its own channel count, such a frame is not a whole number of the file's
/// sample frames - 512 x 6 x 3 bytes against the 51 of a 17-channel file -
/// and every channel after it is rotated for the rest of the file.
#[derive(Debug, Clone, PartialEq)]
struct FileLayout {
    /// Bed speakers, in DAMF order.
    bed: Vec<SpeakerLabels>,
    /// Object channels, after the bed.
    object_count: usize,
    /// The audio of a master set: `.atmos.audio`, CAF whatever format was
    /// asked for.
    spatial: bool,
}

/// One channel of a decoded frame, as its [`DtsLayout`] lists it.
#[derive(Clone, Copy)]
enum FrameChannel {
    /// A bed channel.
    Bed(BedSource),
    /// An object, by its entry in `object_sources`.
    Object(usize),
}

impl FileLayout {
    /// The layout of a file opened for a frame of `layout`.
    fn of(layout: &DtsLayout, spatial: bool) -> Self {
        Self {
            bed: layout.bed.clone(),
            object_count: layout.objects.len(),
            spatial,
        }
    }

    fn channel_count(&self) -> usize {
        self.bed.len() + self.object_count
    }

    /// Whether a frame of `layout` has exactly the channels of the file:
    /// every frame of an undamaged stream.
    fn holds(&self, layout: &DtsLayout) -> bool {
        self.bed == layout.bed && self.object_count == layout.objects.len()
    }

    /// For each channel of the file, the channel of a frame of `layout` that
    /// goes there: a bed channel by the speaker it is named for (a layout
    /// names each speaker once), an object by its rank; `None` where the
    /// frame has no such channel.
    fn place<'a>(
        &'a self,
        layout: &'a DtsLayout,
    ) -> impl Iterator<Item = Option<FrameChannel>> + 'a {
        let bed = self.bed.iter().map(|speaker| {
            let index = layout.bed.iter().position(|other| other == speaker)?;
            layout
                .bed_sources
                .get(index)
                .copied()
                .map(FrameChannel::Bed)
        });
        let objects = (0..self.object_count).map(|rank| {
            layout
                .object_sources
                .get(rank)
                .copied()
                .map(FrameChannel::Object)
        });
        bed.chain(objects)
    }

    /// How many channels of a frame of `layout` the file has no place for.
    fn unplaced(&self, layout: &DtsLayout) -> usize {
        let bed = layout
            .bed
            .iter()
            .filter(|speaker| !self.bed.contains(speaker));
        bed.count() + layout.objects.len().saturating_sub(self.object_count)
    }
}

/// The files the open writer fills, kept so that what went into them can be
/// read back if the audio has to be rewritten in another layout.
enum AudioTarget {
    /// One interleaved file, in this container.
    File {
        path: PathBuf,
        container: AudioFormat,
    },
    /// `<prefix>_<n>.wav`, one per channel.
    Mono { prefix: PathBuf },
}

/// Where one output channel of an HD frame takes its samples from.
#[derive(Clone, Copy)]
enum Column<'a> {
    /// Nothing (a missing channel, or a feed with no stated fold): silence.
    Silent,
    /// Written as decoded.
    Plain(&'a [f32]),
    /// A bed channel of this DCA speaker, with the folded feeds removed.
    Bed(usize, &'a [f32]),
}

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
    /// Channels of the audio file: those of its layout, not of whichever
    /// frame came last.
    pub final_channel_count: usize,
    pub warp_mode: Option<WarpMode>,
    /// Set of presentations already warned about, so an experimental profile is
    /// reported once rather than per frame.
    warned_presentations: Vec<XPresentation>,
    /// What the audio file holds, once a frame has said so.
    layout: Option<FileLayout>,
    /// The files `audio_writer` fills.
    target: Option<AudioTarget>,
    /// Sample frames written to them so far.
    written_frames: u64,
    /// Interleaving scratch, reused across frames.
    interleaved: Vec<i32>,
    /// Whether the previous frame had another layout than the file, so that
    /// a run of such frames is reported once.
    off_layout: bool,
    /// Whether the warning for channels the file has no place for has
    /// already been emitted.
    warned_unplaced_channels: bool,
    /// Whether the metadata file already carries its `sampleRate` header.
    /// Tracked explicitly rather than inferred from `prev_events`: a fixed-bed
    /// presentation emits no events at all, so an inferred flag would re-emit
    /// the header for every frame.
    metadata_header_written: bool,
    /// Whether the dropped-channel warning has already been emitted.
    warned_dropped_channels: bool,
    /// Whether the unreadable-metadata warning has already been emitted.
    warned_unreadable_metadata: bool,
    /// Recompute the fold of an object the stream says the encoder rendered
    /// into the bed from its position, and subtract it.
    pub render_folds: bool,
    /// Estimate the fold of a waveform the stream states none for, from the
    /// bed's audio, rather than keeping it in the bed muted.
    pub estimate_folds: bool,
    /// Keep the bed to what an Atmos bed can hold (7.1.2 at most): a DTS:X
    /// or Auro-3D layout's corner heights and wides become static objects.
    pub bed_conform: bool,
    /// Write one mono WAV per channel, `<prefix>_<n>.wav`, instead of the
    /// interleaved audio file.
    pub mono_prefix: Option<PathBuf>,
    renderer: FoldRenderer,
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

/// The DAMF codec label of a spatial presentation, as the header writes it
/// and as `info --json` reports it.
pub(crate) fn presentation_label(presentation: XPresentation) -> &'static str {
    source_codec_for(presentation).label()
}

/// Which DAMF codec label a spatial presentation is stored under.
///
/// The taxonomy is Atmos Ranker's, derived independently at scan time from the
/// alternate-profile syncwords; both sides must agree or the Rank codec filter
/// splits. [`SourceCodec`] says what that agreement rests on.
fn source_codec_for(presentation: XPresentation) -> SourceCodec {
    match presentation {
        XPresentation::Height => SourceCodec::DtsX714,
        XPresentation::ObjectD0 => SourceCodec::DtsX714Plus1,
        XPresentation::ObjectsD1 => SourceCodec::DtsX714Plus2,
        XPresentation::ObjectsD3 => SourceCodec::DtsX714Plus4,
        XPresentation::ObjectsD4 => SourceCodec::DtsX714Plus5,
        XPresentation::ObjectsD0 => SourceCodec::DtsX714Plus3,
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
            layout: None,
            target: None,
            written_frames: 0,
            interleaved: Vec::new(),
            off_layout: false,
            warned_unplaced_channels: false,
            metadata_header_written: false,
            warned_dropped_channels: false,
            warned_unreadable_metadata: false,
            render_folds: true,
            estimate_folds: true,
            bed_conform: false,
            mono_prefix: None,
            renderer: FoldRenderer::new(),
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
        self.note_frame(frame.sample_rate, &layout, true, base_path)?;
        self.settle_audio(
            &layout,
            frame.sample_rate,
            base_path,
            AudioFormat::Caf,
            no_audio,
        )?;
        self.write_frame_metadata(&layout, frame.sample_rate, base_path)?;
        // An unfolded stream is a bed channel or an object alike: one column
        // of the interleaved rows.
        self.write_channels(&layout, sample_count, |channel, out| {
            let (FrameChannel::Bed(BedSource::Speaker(index)) | FrameChannel::Object(index)) =
                channel
            else {
                return; // an unfolded frame has no extension feeds
            };
            for (out, &sample) in out.zip(frame.samples[index..].iter().step_by(ns)) {
                *out = sample;
            }
        })?;
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
        if let (Some(_), Some(metadata), true) =
            (presentation, metadata.as_ref(), self.render_folds)
        {
            self.renderer.apply(&mut plan, metadata, sample_count);
        }
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
            &layout,
            presentation.is_some(),
            base_path,
        )?;
        self.settle_audio(&layout, frame.sample_rate, base_path, format, no_audio)?;

        // A master set needs its metadata file even when the presentation is a
        // fixed bed with no objects: the events carry the bed's sample
        // positions and Atmos Ranker expects the pair.
        if self.has_spatial {
            self.write_frame_metadata(&layout, frame.sample_rate, base_path)?;
        }

        self.write_hd_pcm(frame, &active, &layout, &plan, sample_count)?;

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

        // A plain core frame carries no spatial extension, so it never turns
        // the output into a master set.
        self.note_frame(core.sample_rate, &layout, false, base_path)?;
        self.settle_audio(&layout, core.sample_rate, base_path, format, no_audio)?;
        self.write_core_pcm(core, &layout, sample_count)?;

        self.decoded_samples += sample_count as u64;
        self.decoded_frames += 1;
        Ok(())
    }

    /// Record this frame's shape, writing the `.atmos` header on the first
    /// spatial frame.
    fn note_frame(
        &mut self,
        sample_rate: u32,
        layout: &DtsLayout,
        is_spatial: bool,
        base_path: &Option<PathBuf>,
    ) -> Result<()> {
        self.final_sample_rate = sample_rate;

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
        Ok(())
    }

    /// Settle what the audio file holds before a frame of `layout` is
    /// written to it.
    ///
    /// The first frame opens the file, and its layout stands from then on:
    /// a later frame with another one is written in the file's (see
    /// [`Self::write_channels`]) and reported here. The one frame that
    /// changes the file is the first spatial frame of a stream that began as
    /// a plain bed: see [`Self::promote_to_master_set`].
    fn settle_audio(
        &mut self,
        layout: &DtsLayout,
        sample_rate: u32,
        base_path: &Option<PathBuf>,
        format: AudioFormat,
        no_audio: bool,
    ) -> Result<()> {
        match &self.layout {
            None => {
                let file = FileLayout::of(layout, self.has_spatial);
                let channel_count = file.channel_count();
                self.final_channel_count = channel_count;
                if let (Some(base_path), false) = (base_path, no_audio) {
                    let (writer, target) = self.create_writer(
                        base_path,
                        format,
                        sample_rate,
                        channel_count,
                        file.spatial,
                    )?;
                    self.audio_writer = Some(writer);
                    self.target = Some(target);
                }
                self.layout = Some(file);
            }
            // `has_spatial` was set by this very frame: the file was opened
            // before any spatial frame came.
            Some(open) if self.has_spatial && !open.spatial => {
                let file = FileLayout::of(layout, true);
                self.promote_to_master_set(file, base_path, sample_rate)?;
            }
            Some(_) => {}
        }
        let Some(file) = &self.layout else {
            return Ok(());
        };

        let off_layout = !file.holds(layout);
        if off_layout && !self.off_layout {
            log::warn!(
                "DTS layout changed mid-stream ({} bed / {} objects in a file of {} bed / {} objects): \
                 such frames are written in the layout of the file, each channel where its name puts it \
                 and the channels they lack silent",
                layout.bed.len(),
                layout.objects.len(),
                file.bed.len(),
                file.object_count
            );
        }
        self.off_layout = off_layout;
        let unplaced = file.unplaced(layout);
        if unplaced > 0 && !self.warned_unplaced_channels {
            self.warned_unplaced_channels = true;
            log::warn!(
                "DTS frame carries {unplaced} channel(s) the audio file has no place for: dropped \
                 (the file's {} channels were fixed by the frame it was opened for)",
                file.channel_count()
            );
        }
        Ok(())
    }

    /// Rewrite the open bed file as the audio of a master set, on the first
    /// spatial frame of a stream that began without one.
    ///
    /// The frames before it were written as a plain bed, in the format that
    /// was asked for. The master set this frame starts names an
    /// `.atmos.audio` beside its `.atmos`, and its events count their samples
    /// from the start of the stream. So what is on disk is carried over,
    /// rather than left behind or continued: each bed channel goes to the
    /// place of its speaker in the new file, and the channels the bed did not
    /// have - heights, objects - are silent up to here. Keeping the bed file
    /// instead would drop every height and object of the programme, the price
    /// of one core frame at the head of a DTS:X stream; and going on writing
    /// to it is what this used to do, 17-channel frames into a 6-channel
    /// file, beside a master set with no audio.
    fn promote_to_master_set(
        &mut self,
        layout: FileLayout,
        base_path: &Option<PathBuf>,
        sample_rate: u32,
    ) -> Result<()> {
        let channel_count = layout.channel_count();
        self.final_channel_count = channel_count;
        let Some(bed_layout) = self.layout.replace(layout.clone()) else {
            return Ok(());
        };
        let (Some(writer), Some(target), Some(base_path)) =
            (self.audio_writer.take(), self.target.take(), base_path)
        else {
            return Ok(());
        };
        log::warn!(
            "DTS spatial frames begin {} samples into a stream that started as a plain bed: \
             rewriting the audio written so far as the head of the master set",
            self.written_frames
        );
        writer.close_and_drop()?;
        let bed_channel_count = bed_layout.channel_count();
        let mut written = match &target {
            AudioTarget::File { path, container } => {
                PcmReadBack::interleaved(path, *container, bed_channel_count, self.written_frames)?
            }
            AudioTarget::Mono { prefix } => {
                PcmReadBack::mono(prefix, bed_channel_count, self.written_frames)?
            }
        };
        let (mut writer, target) = self.create_writer(
            base_path,
            AudioFormat::Caf,
            sample_rate,
            channel_count,
            true,
        )?;

        // What the bed file has for each channel of the new one: nothing for
        // the objects, nor for a speaker the stream did not carry until now.
        let mut sources: Vec<Option<usize>> = layout
            .bed
            .iter()
            .map(|speaker| bed_layout.bed.iter().position(|other| other == speaker))
            .collect();
        let dropped = bed_channel_count - sources.iter().flatten().count();
        if dropped > 0 {
            log::warn!(
                "{dropped} channel(s) of the plain bed have no place in the master set: dropped"
            );
        }
        sources.resize(channel_count, None);
        let mut bed_samples = Vec::new();
        loop {
            let frames = written.read(CHUNK_FRAMES, &mut bed_samples)?;
            if frames == 0 {
                break;
            }
            self.interleaved.clear();
            self.interleaved.reserve(frames * channel_count);
            for frame in 0..frames {
                let first = frame * bed_channel_count;
                self.interleaved.extend(
                    sources
                        .iter()
                        .map(|source| source.map_or(0, |channel| bed_samples[first + channel])),
                );
            }
            if channel_count > 0 {
                writer.write_pcm_samples(&self.interleaved, channel_count)?;
            }
        }
        written.remove()?;
        self.audio_writer = Some(writer);
        self.target = Some(target);
        Ok(())
    }

    /// Create the audio file - or the mono set - for `channel_count` channels.
    fn create_writer(
        &self,
        base_path: &Path,
        format: AudioFormat,
        sample_rate: u32,
        channel_count: usize,
        spatial: bool,
    ) -> Result<(AudioWriter, AudioTarget)> {
        if let Some(prefix) = &self.mono_prefix {
            log::info!(
                "Creating {channel_count} mono audio files: {}",
                mono_path(prefix, 0).display()
            );
            let writer = AudioWriter::create_mono(prefix, sample_rate, channel_count)?;
            let target = AudioTarget::Mono {
                prefix: prefix.clone(),
            };
            return Ok((writer, target));
        }
        let (path, _) = create_output_paths(base_path, format, spatial);
        log::info!("Creating audio file: {}", path.display());
        let (writer, container) = match (format, spatial) {
            (AudioFormat::Caf, _) | (_, true) => (
                AudioWriter::create_caf(path.clone(), sample_rate, channel_count as u32, &[])?,
                AudioFormat::Caf,
            ),
            (AudioFormat::W64, false) => (
                AudioWriter::create_w64(path.clone(), sample_rate, channel_count as u32)?,
                AudioFormat::W64,
            ),
            (AudioFormat::Pcm, false) => (AudioWriter::create_pcm(path.clone())?, AudioFormat::Pcm),
        };
        Ok((writer, AudioTarget::File { path, container }))
    }

    /// Write one decoded frame to the audio file, in the file's layout.
    ///
    /// Each bed channel is written to the place of the speaker it is named
    /// for and each object to its own; a channel of the file the frame does
    /// not carry is silent - the heights and objects of a plain core frame
    /// in a master set - and one the file has no place for is dropped.
    /// `fill` writes one channel of the frame into its stride of the
    /// interleaved samples, or leaves it silent.
    fn write_channels(
        &mut self,
        layout: &DtsLayout,
        sample_count: usize,
        mut fill: impl FnMut(FrameChannel, StepBy<IterMut<'_, i32>>),
    ) -> Result<()> {
        let (Some(file), Some(writer)) = (&self.layout, &mut self.audio_writer) else {
            return Ok(());
        };
        let channel_count = file.channel_count();
        self.written_frames += sample_count as u64;
        if channel_count == 0 || sample_count == 0 {
            return Ok(());
        }
        self.interleaved.clear();
        self.interleaved.resize(sample_count * channel_count, 0);
        for (index, channel) in file.place(layout).enumerate() {
            let Some(channel) = channel else { continue };
            let out = self.interleaved[index..].iter_mut().step_by(channel_count);
            fill(channel, out);
        }
        writer.write_pcm_samples(&self.interleaved, channel_count)?;
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

    /// Interleave in the order the audio file declares: the bed in DAMF
    /// speaker order, then the objects. `layout.bed_sources` is what maps each
    /// declared position back to the decoded channel that carries it.
    fn write_hd_pcm(
        &mut self,
        frame: &HdFrame,
        active: &[usize],
        layout: &DtsLayout,
        plan: &FoldPlan,
        sample_count: usize,
    ) -> Result<()> {
        // A feed whose bed fold is not stated stays in the bed and is muted
        // on its own channel, exactly as the realtime bridge does.
        let feed = |feed: usize| -> Column<'_> {
            match frame.x_samples.get(feed) {
                Some(samples) if plan.source_is_known(feed) => Column::Plain(samples),
                _ => Column::Silent,
            }
        };
        // Channel by channel rather than sample by sample: each column's
        // source is resolved once, and the bed fold is removed over the
        // whole channel with the same per-sample arithmetic.
        let mut cleaned = Vec::new();
        self.write_channels(layout, sample_count, |channel, out| {
            let column = match channel {
                FrameChannel::Bed(BedSource::Speaker(position)) => active
                    .get(position)
                    .and_then(|&speaker| {
                        frame.samples[speaker]
                            .as_deref()
                            .map(|samples| Column::Bed(speaker, samples))
                    })
                    .unwrap_or(Column::Silent),
                FrameChannel::Bed(BedSource::Feed(index)) | FrameChannel::Object(index) => {
                    feed(index)
                }
            };
            let samples = match column {
                Column::Silent => return,
                Column::Plain(samples) => samples,
                Column::Bed(speaker, bed) => {
                    cleaned.resize(bed.len(), 0.0);
                    plan.clean_channel(speaker, bed, &frame.x_samples, &mut cleaned);
                    &cleaned[..]
                }
            };
            for (out, &sample) in out.zip(samples) {
                *out = float_to_i24(sample);
            }
        })
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
    ) -> Result<()> {
        self.write_channels(layout, sample_count, |channel, out| {
            let FrameChannel::Bed(BedSource::Speaker(position)) = channel else {
                return; // a core frame has no extension feeds and no objects
            };
            let samples = core
                .fullband_channels
                .get(position)
                .or(core.lfe_channel.as_ref());
            for (out, &sample) in out.zip(samples.into_iter().flatten()) {
                *out = float_to_i24(sample);
            }
        })
    }

    /// Append the object positions of a frame of `layout` to the metadata
    /// file, as far as the objects of the master set go: an object the audio
    /// file has no channel for gets no event either.
    fn write_frame_metadata(
        &mut self,
        layout: &DtsLayout,
        sample_rate: u32,
        base_path: &Option<PathBuf>,
    ) -> Result<()> {
        let Some(base_path) = base_path else {
            return Ok(());
        };
        let object_count = self.layout.as_ref().map_or(0, |file| file.object_count);
        let oamd = if layout.objects.len() > object_count {
            let mut placed = layout.clone();
            placed.objects.truncate(object_count);
            placed.object_sources.truncate(object_count);
            convert_dts(&placed)
        } else {
            convert_dts(layout)
        };
        self.write_metadata_event(&oamd, sample_rate, base_path)
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
        // A frame that carries fewer objects than the master set states the
        // positions of those it has. The others stand as last stated, which
        // is what the next frame to carry them is compared with.
        if conf.events.len() < self.prev_events.len() {
            self.prev_events[..conf.events.len()].clone_from_slice(&conf.events);
        } else {
            self.prev_events = conf.events.clone();
        }

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

#[cfg(test)]
mod presentation_label_tests {
    use super::presentation_label;
    use dca::XPresentation;

    /// The DAMF label is the taxonomy prefix over the presentation's own
    /// layout name, so the ranker's classification and a display label
    /// built from `layout_label` name the same thing.
    #[test]
    fn damf_label_is_the_layout_label_under_the_taxonomy_prefix() {
        use XPresentation::*;
        for p in [
            Height, ObjectD0, ObjectsD1, ObjectsD3, ObjectsD4, ObjectsD0, ObjectOnly,
        ] {
            assert_eq!(presentation_label(p), format!("DTS:X-{}", p.layout_label()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dca::{
        BedChannel, BedFold, PcmDecoder, REFERENCE_CHANNELS, SourceMetadata, SourceRole,
        SpatialChannel, SphericalPosition,
    };
    use std::io::{Read, Seek, SeekFrom};

    /// One sample of a marked channel: channel `value` reads back as `value`.
    const MARK: i32 = 32_768;
    /// Sample frames in one frame of the fixture, and in every frame here.
    const UNIT: usize = 512;

    fn mark(value: i32) -> Vec<f32> {
        vec![value as f32 / 256.0; UNIT]
    }

    /// The first frame of the stereo fixture, decoded, with L = -21 and
    /// R = -22. The one frame here that came through the decoder; its header
    /// is what the other core frames are patched onto.
    fn stereo() -> PcmPushResult {
        let stream = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/dts_core_tone_10f.dts"
        ))
        .unwrap();
        let mut push = PcmDecoder::new().push_access_unit(&stream[..428]).unwrap();
        assert_eq!(
            push.pcm.fullband_channel_order,
            [BedChannel::FrontLeft, BedChannel::FrontRight]
        );
        assert!(push.pcm.lfe_channel.is_none());
        assert_eq!(push.pcm.samples_per_channel(), UNIT);
        push.pcm.fullband_channels = vec![mark(-21), mark(-22)];
        push
    }

    /// A plain 5.1 core frame, in the decoder's order (C L R Ls Rs, LFE),
    /// each speaker marked with minus its place in a DAMF bed: L = -1,
    /// R = -2, C = -3, LFE = -4, Lss = -5, Rss = -6.
    fn core51() -> PcmPushResult {
        let mut push = stereo();
        use BedChannel::*;
        push.pcm.fullband_channel_order =
            vec![Center, FrontLeft, FrontRight, SurroundLeft, SurroundRight];
        push.pcm.fullband_channels = [-3, -1, -2, -5, -6].map(mark).to_vec();
        push.pcm.lfe_channel = Some(mark(-4));
        push
    }

    /// A DTS-HD frame with the bed speakers `C L R Lss Rss LFE [Lrs Rrs]`
    /// (DCA indices 0-5, 7, 8) marked with `base` plus their place in a DAMF
    /// bed (L first), and the extension feeds marked as given.
    fn hd(base: i32, with_backs: bool, feeds: &[i32]) -> Box<HdFrame> {
        let speaker = |place: i32| Some(mark(base + place));
        let mut samples = vec![
            speaker(3),
            speaker(1),
            speaker(2),
            speaker(5),
            speaker(6),
            speaker(4),
        ];
        if with_backs {
            samples.extend([None, speaker(7), speaker(8)]);
        }
        Box::new(HdFrame {
            sample_rate: 48_000,
            samples,
            x_samples: feeds.iter().map(|&value| mark(value)).collect(),
            ..HdFrame::default()
        })
    }

    /// Metadata for `count` feeds, the first `objects` of them objects at a
    /// position off the origin, none folded into the bed - so every feed is
    /// written as it is.
    fn metadata(count: usize, objects: usize) -> XMetadata {
        let sources: Vec<SourceMetadata> = (0..count)
            .map(|feed| SourceMetadata {
                role: if feed < objects {
                    SourceRole::Object {
                        position: SphericalPosition {
                            azimuth_half_degrees: 60,
                            elevation_half_degrees: 30,
                            distance_64ths: 64,
                        },
                        centre_height_alternative: false,
                    }
                } else {
                    SourceRole::Height(SpatialChannel::TopFrontLeft)
                },
                fold: BedFold::Known([0.0; REFERENCE_CHANNELS]),
            })
            .collect();
        XMetadata::from_sources(&sources).unwrap()
    }

    /// A DTS:X 7.1.4+5 frame: bed 1-8, objects 101-105, heights 9-12.
    fn x() -> DtsFrameMessage {
        DtsFrameMessage::Hd {
            frame: hd(0, true, &[101, 102, 103, 104, 105, 9, 10, 11, 12]),
            presentation: Some(XPresentation::ObjectsD4),
            metadata: Some(metadata(9, 5)),
        }
    }

    /// A lossless 7.1 frame without a spatial extension: bed 31-38.
    fn hd71() -> DtsFrameMessage {
        DtsFrameMessage::Hd {
            frame: hd(30, true, &[]),
            presentation: None,
            metadata: None,
        }
    }

    /// A DTS:X 5.1+1 frame: bed 51-56, one object 201.
    fn imax() -> DtsFrameMessage {
        DtsFrameMessage::Hd {
            frame: hd(50, false, &[201]),
            presentation: Some(XPresentation::ObjectOnly),
            metadata: Some(metadata(1, 1)),
        }
    }

    /// An unfolded Auro-3D frame: 5.1, the four corner heights (1-10 in
    /// DAMF bed order) and the top, which becomes the object 101.
    fn auro() -> DtsFrameMessage {
        let streams: Vec<auro::StreamId> = [0, 1, 2, 3, 4, 5, 9, 10, 13, 14, 12]
            .into_iter()
            .map(auro::StreamId)
            .collect();
        let row: Vec<i32> = AURO_UNIT.iter().map(|value| value * MARK).collect();
        DtsFrameMessage::AuroFrame(Box::new(AuroFrame {
            sample_rate: 48_000,
            streams,
            samples: row.repeat(UNIT),
        }))
    }

    fn core(push: PcmPushResult) -> DtsFrameMessage {
        DtsFrameMessage::Core(Box::new(push))
    }

    // What each frame holds in a 17-channel 7.1.4+5 file: the bed L R C LFE
    // Lss Rss Lrs Rrs Lfh Rfh Lrh Rrh, then the five objects.
    const X_UNIT: [i32; 17] = [
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 101, 102, 103, 104, 105,
    ];
    const CORE_IN_X: [i32; 17] = [-1, -2, -3, -4, -5, -6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    const HD71_IN_X: [i32; 17] = [31, 32, 33, 34, 35, 36, 37, 38, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    const IMAX_IN_X: [i32; 17] = [51, 52, 53, 54, 55, 56, 0, 0, 0, 0, 0, 0, 201, 0, 0, 0, 0];
    const CORE_UNIT: [i32; 6] = [-1, -2, -3, -4, -5, -6];
    const AURO_UNIT: [i32; 11] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 101];

    /// An empty directory of this test's own, and the output base path in it.
    fn out_dir(test: &str) -> (PathBuf, Option<PathBuf>) {
        let dir = std::env::temp_dir().join(format!("harletty-dts-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = Some(dir.join("out"));
        (dir, base)
    }

    fn run(
        handler: &mut DtsDecodeHandler,
        base: &Option<PathBuf>,
        format: AudioFormat,
        messages: Vec<DtsFrameMessage>,
    ) {
        for message in messages {
            handler
                .handle_message(message, base, format, false)
                .unwrap();
        }
        handler.finalize().unwrap();
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn sample(bytes: &[u8], big_endian: bool) -> i32 {
        let value = if big_endian {
            i32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]])
        } else {
            i32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
        };
        (value << 8) >> 8
    }

    /// The channel count a CAF file declares and its samples.
    fn read_caf(path: &Path) -> (usize, Vec<i32>) {
        let mut file = File::open(path).unwrap();
        let info = damf::caf::parse_caf_file(&mut file).unwrap();
        let channels = info.audio_format.unwrap().channels_per_frame as usize;
        let big_endian = matches!(info.endianness, damf::caf::Endianness::BigEndian);
        file.seek(SeekFrom::Start(info.data_chunk_start)).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len() % 3, 0);
        let samples = bytes
            .chunks_exact(3)
            .map(|b| sample(b, big_endian))
            .collect();
        (channels, samples)
    }

    fn read_raw(path: &Path, skip: usize) -> Vec<i32> {
        let bytes = std::fs::read(path).unwrap();
        assert_eq!((bytes.len() - skip) % 3, 0);
        bytes[skip..]
            .chunks_exact(3)
            .map(|b| sample(b, false))
            .collect()
    }

    /// Every sample frame of the file, checked against the frame it belongs
    /// to: `units[u]` is what each channel holds throughout frame `u`. One
    /// sample frame written with another channel count would shift every
    /// one after it.
    #[track_caller]
    fn assert_units(samples: &[i32], channels: usize, units: &[&[i32]]) {
        assert_eq!(
            samples.len(),
            units.len() * UNIT * channels,
            "{} frames of {channels} channels",
            units.len()
        );
        for (index, frame) in samples.chunks_exact(channels).enumerate() {
            let expected: Vec<i32> = units[index / UNIT].iter().map(|v| v * MARK).collect();
            assert_eq!(frame, expected, "sample frame {index}");
        }
    }

    const MASTER_SET: [&str; 3] = ["out.atmos", "out.atmos.audio", "out.atmos.metadata"];

    /// The bug this guards: a core frame and a lossless frame without the
    /// extension, written into a 17-channel master set with their own 6 and 8
    /// channels, after which nothing in the file is where it belongs.
    #[test]
    fn frames_without_the_extension_are_written_in_the_master_sets_layout() {
        let (dir, base) = out_dir("master-set");
        let mut handler = DtsDecodeHandler::default();
        run(
            &mut handler,
            &base,
            AudioFormat::Pcm,
            vec![x(), core(core51()), hd71(), x()],
        );
        let (channels, samples) = read_caf(&dir.join("out.atmos.audio"));
        assert_eq!(channels, 17);
        assert_units(&samples, 17, &[&X_UNIT, &CORE_IN_X, &HD71_IN_X, &X_UNIT]);
        assert_eq!(
            handler.final_channel_count, 17,
            "the file's, not the last frame's"
        );
        assert_eq!(handler.decoded_samples, 4 * UNIT as u64);
        assert_eq!(file_names(&dir), MASTER_SET);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A plain bed file keeps its layout too: a frame's channels go where
    /// their speakers are, whatever order its decoder listed them in, the
    /// speakers it lacks are silent, and the ones the file has no place for
    /// are dropped.
    #[test]
    fn a_bed_file_keeps_the_layout_it_was_opened_with() {
        let (dir, base) = out_dir("bed-51");
        let mut handler = DtsDecodeHandler::default();
        run(
            &mut handler,
            &base,
            AudioFormat::Pcm,
            vec![core(core51()), core(stereo()), hd71(), core(core51())],
        );
        let samples = read_raw(&dir.join("out.pcm"), 0);
        assert_units(
            &samples,
            6,
            &[
                &CORE_UNIT,
                &[-21, -22, 0, 0, 0, 0],
                &[31, 32, 33, 34, 35, 36],
                &CORE_UNIT,
            ],
        );
        assert_eq!(handler.final_channel_count, 6);
        assert_eq!(file_names(&dir), ["out.pcm"]);
        std::fs::remove_dir_all(&dir).unwrap();

        // The other way round the file is stereo and stays stereo.
        let (dir, base) = out_dir("bed-stereo");
        let mut handler = DtsDecodeHandler::default();
        run(
            &mut handler,
            &base,
            AudioFormat::Pcm,
            vec![core(stereo()), core(core51()), hd71()],
        );
        let samples = read_raw(&dir.join("out.pcm"), 0);
        assert_units(&samples, 2, &[&[-21, -22], &[-1, -2], &[31, 32]]);
        assert_eq!(handler.final_channel_count, 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The mirror case: the stream starts without a spatial extension, the
    /// audio file is opened as the plain bed that was asked for, and then a
    /// DTS:X frame arrives. The bed already written moves into the master
    /// set, and the events start where the objects do.
    #[test]
    fn a_bed_file_is_rewritten_as_a_master_set_on_the_first_spatial_frame() {
        for (format, bed_file) in [
            (AudioFormat::Pcm, "out.pcm"),
            (AudioFormat::Caf, "out.caf"),
            (AudioFormat::W64, "out.wav"),
        ] {
            let (dir, base) = out_dir(&format!("promoted-{bed_file}"));
            let mut handler = DtsDecodeHandler::default();
            for message in [core(core51()), hd71()] {
                handler
                    .handle_message(message, &base, format, false)
                    .unwrap();
            }
            assert_eq!(
                file_names(&dir),
                [bed_file],
                "{format:?}: a plain bed so far"
            );
            assert_eq!(handler.final_channel_count, 6);

            run(&mut handler, &base, format, vec![x()]);
            let (channels, samples) = read_caf(&dir.join("out.atmos.audio"));
            assert_eq!(channels, 17, "{format:?}");
            let mut hd71_in_51 = [0; 17];
            hd71_in_51[..6].copy_from_slice(&HD71_IN_X[..6]);
            assert_units(&samples, 17, &[&CORE_IN_X, &hd71_in_51, &X_UNIT]);
            assert_eq!(handler.final_channel_count, 17);
            assert_eq!(
                file_names(&dir),
                MASTER_SET,
                "{format:?}: the bed file is gone, and no temporary is left"
            );
            let metadata = std::fs::read_to_string(dir.join("out.atmos.metadata")).unwrap();
            assert!(
                metadata.contains(&format!("samplePos: {}", 2 * UNIT)),
                "{format:?}: the objects begin after the bed frames:\n{metadata}"
            );
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// Objects are placed by rank: a frame with fewer than the file has
    /// leaves the rest silent, one with more has the extra dropped - and
    /// gets no event for it, since the master set does not name it. The
    /// metadata is the same whether or not audio is written.
    #[test]
    fn objects_are_placed_by_rank_and_the_events_name_only_the_files() {
        let (dir, base) = out_dir("objects-fewer");
        let mut handler = DtsDecodeHandler::default();
        run(
            &mut handler,
            &base,
            AudioFormat::Pcm,
            vec![x(), imax(), x()],
        );
        let (channels, samples) = read_caf(&dir.join("out.atmos.audio"));
        assert_eq!(channels, 17);
        assert_units(&samples, 17, &[&X_UNIT, &IMAX_IN_X, &X_UNIT]);
        assert_eq!(file_names(&dir), MASTER_SET);
        std::fs::remove_dir_all(&dir).unwrap();

        let (dir, base) = out_dir("objects-more");
        let mut handler = DtsDecodeHandler::default();
        run(
            &mut handler,
            &base,
            AudioFormat::Pcm,
            vec![imax(), x(), imax()],
        );
        let (channels, samples) = read_caf(&dir.join("out.atmos.audio"));
        assert_eq!(channels, 7);
        let imax_unit = [51, 52, 53, 54, 55, 56, 201];
        assert_units(
            &samples,
            7,
            &[&imax_unit, &[1, 2, 3, 4, 5, 6, 101], &imax_unit],
        );
        assert_eq!(handler.final_channel_count, 7);
        let metadata = std::fs::read_to_string(dir.join("out.atmos.metadata")).unwrap();
        assert!(metadata.contains("ID: 10"), "{metadata}");
        assert!(!metadata.contains("ID: 11"), "{metadata}");

        let (silent_dir, silent_base) = out_dir("objects-more-no-audio");
        let mut no_audio = DtsDecodeHandler::default();
        for message in [imax(), x(), imax()] {
            no_audio
                .handle_message(message, &silent_base, AudioFormat::Pcm, true)
                .unwrap();
        }
        no_audio.finalize().unwrap();
        assert_eq!(file_names(&silent_dir), ["out.atmos", "out.atmos.metadata"]);
        assert_eq!(
            std::fs::read_to_string(silent_dir.join("out.atmos.metadata")).unwrap(),
            metadata
        );
        assert_eq!(no_audio.decoded_samples, handler.decoded_samples);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&silent_dir).unwrap();
    }

    /// An unfolded Auro-3D stream is a master set from its first frame; a
    /// core frame that falls back inside it is its bed, in place.
    #[test]
    fn a_core_frame_in_an_unfolded_auro_stream_is_written_in_its_layout() {
        let (dir, base) = out_dir("auro");
        let mut handler = DtsDecodeHandler::default();
        run(
            &mut handler,
            &base,
            AudioFormat::Pcm,
            vec![auro(), core(core51()), auro()],
        );
        let (channels, samples) = read_caf(&dir.join("out.atmos.audio"));
        assert_eq!(channels, 11);
        let core_in_auro = [-1, -2, -3, -4, -5, -6, 0, 0, 0, 0, 0];
        assert_units(&samples, 11, &[&AURO_UNIT, &core_in_auro, &AURO_UNIT]);
        assert_eq!(handler.final_channel_count, 11);
        assert_eq!(file_names(&dir), MASTER_SET);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The mono set refuses a write of another channel count outright, so a
    /// core frame in a DTS:X stream used to end the decode. Here the files
    /// stay in lockstep, through the rewrite as well.
    #[test]
    fn mono_files_keep_one_layout() {
        let (dir, base) = out_dir("mono");
        let prefix = dir.join("ch");
        let mut handler = DtsDecodeHandler::default();
        handler.mono_prefix = Some(prefix.clone());
        run(
            &mut handler,
            &base,
            AudioFormat::Pcm,
            vec![core(core51()), x(), core(core51()), hd71(), x()],
        );
        let units = [&CORE_IN_X, &X_UNIT, &CORE_IN_X, &HD71_IN_X, &X_UNIT];
        for channel in 0..17 {
            let samples = read_raw(&mono_path(&prefix, channel), 44);
            let expected: Vec<&[i32]> = units.iter().map(|unit| &unit[channel..=channel]).collect();
            assert_units(&samples, 1, &expected);
        }
        let mut expected_files: Vec<String> = (0..17).map(|n| format!("ch_{n}.wav")).collect();
        expected_files.extend(["out.atmos".into(), "out.atmos.metadata".into()]);
        expected_files.sort();
        assert_eq!(file_names(&dir), expected_files);
        assert_eq!(handler.final_channel_count, 17);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
