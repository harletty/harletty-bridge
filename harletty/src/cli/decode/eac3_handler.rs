use super::atmos::create_damf_header_file;
use super::output::{AudioWriter, PcmReadBack, create_output_paths, float_to_i24, mono_path};
use crate::cli::command::{AudioFormat, WarpMode};
use damf::{Configuration, Event, SourceCodec};
use crate::eac3_to_oamd::convert_oamd;
use anyhow::Result;
use eac3::{BedChannel, CorePcmFrame, ObjectPcmPushResult, PcmPushResult};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// Sample frames handled at a time when silence is written or a file is
/// rewritten: bounds the scratch whatever the length of the span.
const CHUNK_FRAMES: usize = 4096;

/// Sample frames interleaved at a time: one channel after another is written
/// into them, and 256 frames of 21 channels are 21 KB.
const INTERLEAVE_BLOCK: usize = 256;

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
    /// Channels of the audio file: those of its layout, not of whichever
    /// frame came last.
    pub final_channel_count: usize,
    pub warp_mode: Option<WarpMode>,
    /// Write one mono WAV per channel, `<prefix>_<n>.wav`, instead of the
    /// interleaved audio file.
    pub mono_prefix: Option<PathBuf>,
    pub warned_experimental: bool,
    /// What the audio file holds, once a decoded frame has said so.
    layout: Option<FileLayout>,
    /// The files `audio_writer` fills.
    target: Option<AudioTarget>,
    /// Sample frames written to them so far.
    written_frames: u64,
    /// Silence substituted before any frame decoded, not yet written.
    leading_silence: Option<LeadingSilence>,
    /// Interleaving scratch, reused across frames.
    interleaved: Vec<i32>,
    warned_bed_only_frame: bool,
    warned_dropped_channels: bool,
}

/// The channels of the audio file, in file order: fixed by the frame the file
/// is opened for, and what every later frame is written in.
///
/// An audio file has one sample frame size and a stream does not. An object
/// stream delivers a plain core frame wherever an access unit has no usable
/// JOC payload and substituted silence wherever one fails to decode; a 7.1
/// pair falls back to its 5.1 core when the merge is declined. Written with
/// its own channel count, such a frame is not a whole number of the file's
/// sample frames - 1536 x 6 x 3 bytes against the 63 of a 21-channel file -
/// and every channel after it is rotated for the rest of the file.
#[derive(Debug, Clone, PartialEq)]
struct FileLayout {
    /// Full-band bed channels, in the decoder's order.
    fullband: Vec<BedChannel>,
    /// Whether the LFE follows them.
    lfe: bool,
    /// Object channels, after the bed.
    object_count: usize,
    /// An object (Atmos) file: `.atmos.audio`, CAF whatever format was asked
    /// for.
    atmos: bool,
}

impl FileLayout {
    /// The layout of a file opened for this frame. `objects` is `Some` for an
    /// object frame.
    fn of(core: &CorePcmFrame, objects: Option<&[Vec<f32>]>) -> Self {
        Self {
            fullband: core.fullband_channel_order.clone(),
            lfe: core.lfe_channel.is_some(),
            object_count: objects.map_or(0, <[_]>::len),
            atmos: objects.is_some(),
        }
    }

    fn bed_count(&self) -> usize {
        self.fullband.len() + usize::from(self.lfe)
    }

    fn channel_count(&self) -> usize {
        self.bed_count() + self.object_count
    }

    /// Whether `core` carries exactly this bed, in this order: every frame of
    /// an undamaged stream.
    fn bed_is(&self, core: &CorePcmFrame) -> bool {
        self.fullband == core.fullband_channel_order
            && self.fullband.len() == core.fullband_channels.len()
            && self.lfe == core.lfe_channel.is_some()
    }

    /// For each bed channel of the file, the index in another bed - a
    /// frame's, or the file's own before a rewrite - of the channel that goes
    /// there, matched by the position it is named for; `None` where the other
    /// bed has no such channel. Indices count the full-band channels, then
    /// the LFE.
    fn place_bed(&self, fullband: &[BedChannel], lfe: bool) -> Vec<Option<usize>> {
        // Dual mono names both its channels alike: each is placed once.
        let mut placed = vec![false; fullband.len()];
        let mut sources: Vec<Option<usize>> = self
            .fullband
            .iter()
            .map(|slot| {
                let index = (0..fullband.len()).find(|&i| fullband[i] == *slot && !placed[i])?;
                placed[index] = true;
                Some(index)
            })
            .collect();
        if self.lfe {
            sources.push(lfe.then_some(fullband.len()));
        }
        sources
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

/// Silence that stood in for the access units at the head of a stream,
/// before one decoded.
///
/// It is held back because silence has no layout of its own: the channel
/// count of the frame header it comes with is the 5.1 core's even where the
/// stream is 7.1 or carries objects, and a file opened on it would be the
/// wrong one for everything that follows.
struct LeadingSilence {
    samples: u64,
    sample_rate: u32,
    /// From the frame header: what is written if nothing ever decodes.
    channel_count: usize,
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
            mono_prefix: None,
            warned_experimental: false,
            layout: None,
            target: None,
            written_frames: 0,
            leading_silence: None,
            interleaved: Vec::new(),
            warned_bed_only_frame: false,
            warned_dropped_channels: false,
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
                self.handle_silence(sample_count, sample_rate, channel_count)?;
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

        let was_atmos = self.has_atmos;
        self.has_atmos = true;
        self.final_sample_rate = sample_rate;

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
        self.write_frame(
            &pcm.core,
            Some(&pcm.object_channels),
            base_path,
            AudioFormat::Caf,
            no_audio,
        )?;

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

        self.final_sample_rate = sample_rate;
        self.write_frame(core, None, base_path, format, no_audio)?;

        self.decoded_samples += sample_count as u64;
        self.decoded_frames += 1;
        Ok(())
    }

    /// Stand silence in for an access unit that did not decode.
    ///
    /// `channel_count` is the frame header's, and only decides anything for a
    /// stream in which nothing decodes at all (see [`LeadingSilence`]): once
    /// the file has a layout, the silence is that of every channel in it.
    ///
    /// The samples count towards `decoded_samples` whether or not audio is
    /// written, as every other frame's do: the metadata events are placed by
    /// it, and must not move when the same stream is decoded without audio.
    fn handle_silence(
        &mut self,
        sample_count: usize,
        sample_rate: u32,
        channel_count: usize,
    ) -> Result<()> {
        if channel_count == 0 || sample_count == 0 {
            return Ok(());
        }
        self.final_sample_rate = sample_rate;
        match &self.layout {
            Some(layout) => {
                let channel_count = layout.channel_count();
                self.write_silence(sample_count as u64, channel_count)?;
            }
            None => {
                self.leading_silence
                    .get_or_insert(LeadingSilence {
                        samples: 0,
                        sample_rate,
                        channel_count,
                    })
                    .samples += sample_count as u64;
            }
        }
        self.decoded_samples += sample_count as u64;
        Ok(())
    }

    /// Write one decoded frame to the audio file, in the file's layout.
    ///
    /// `objects` is `Some` for an object frame. The first frame opens the
    /// file and its layout stands from then on: each bed channel is written
    /// to the place of the position it is named for and each object to its
    /// own, a channel of the file this frame does not carry is silent - the
    /// objects of a plain core frame in an object file - and one the file has
    /// no place for is dropped. The one frame that changes the file is the
    /// first object frame of a stream that began as a plain bed: see
    /// [`Self::promote_to_object_file`].
    fn write_frame(
        &mut self,
        core: &CorePcmFrame,
        objects: Option<&[Vec<f32>]>,
        base_path: &Option<PathBuf>,
        format: AudioFormat,
        no_audio: bool,
    ) -> Result<()> {
        match &self.layout {
            None => {
                let layout = FileLayout::of(core, objects);
                self.open_audio(layout, base_path, format, core.sample_rate, no_audio)?;
            }
            Some(open) if objects.is_some() && !open.atmos => {
                let layout = FileLayout::of(core, objects);
                self.promote_to_object_file(layout, base_path, core.sample_rate)?;
            }
            Some(_) => {}
        }
        let (Some(layout), Some(writer)) = (&self.layout, &mut self.audio_writer) else {
            return Ok(());
        };
        let objects = objects.unwrap_or_default();

        // What this frame has for each channel of the file: an empty slice
        // where it has nothing, which reads as silence below.
        let channel_count = layout.channel_count();
        let mut sources: Vec<&[f32]> = Vec::with_capacity(channel_count);
        let placed_bed = if layout.bed_is(core) {
            sources.extend(core.fullband_channels.iter().map(|channel| &channel[..]));
            sources.extend(core.lfe_channel.as_deref());
            sources.len()
        } else {
            let fullband_count = core.fullband_channel_order.len();
            let lfe = core.lfe_channel.as_deref();
            let placement = layout.place_bed(&core.fullband_channel_order, lfe.is_some());
            sources.extend(placement.iter().map(|index| match *index {
                Some(index) if index < fullband_count => {
                    core.fullband_channels.get(index).map_or(&[][..], |c| &c[..])
                }
                Some(_) => lfe.unwrap_or_default(),
                None => &[],
            }));
            placement.iter().flatten().count()
        };
        sources.extend(
            (0..layout.object_count).map(|index| objects.get(index).map_or(&[][..], |o| &o[..])),
        );

        if layout.atmos && objects.is_empty() && !self.warned_bed_only_frame {
            log::warn!(
                "E-AC-3 access unit without objects in an object stream: its bed is written, the object channels are silent for it"
            );
            self.warned_bed_only_frame = true;
        }
        let dropped = (core.total_channels() - placed_bed)
            + objects.len().saturating_sub(layout.object_count);
        if dropped > 0 && !self.warned_dropped_channels {
            log::warn!(
                "E-AC-3 frame carries {dropped} channel(s) the audio file has no place for: dropped (the file's {channel_count} channels were fixed by the frame it was opened for)"
            );
            self.warned_dropped_channels = true;
        }

        let sample_count = core.samples_per_channel();
        self.written_frames += sample_count as u64;
        if channel_count == 0 || sample_count == 0 {
            return Ok(());
        }
        // One channel at a time into its stride of the frame, a block of
        // sample frames at a time so that the passes stay in the cache. A
        // channel the frame does not carry, or the end of a short one, is
        // left as the silence the buffer is cleared to; when every channel
        // is whole, every sample is overwritten and nothing needs clearing.
        let whole = sources.iter().all(|channel| channel.len() >= sample_count);
        if !whole || self.interleaved.len() != sample_count * channel_count {
            self.interleaved.clear();
            self.interleaved.resize(sample_count * channel_count, 0);
        }
        let blocks = self.interleaved.chunks_mut(INTERLEAVE_BLOCK * channel_count);
        for (block_index, block) in blocks.enumerate() {
            let first = block_index * INTERLEAVE_BLOCK;
            for (index, channel) in sources.iter().enumerate() {
                let samples = channel.get(first..).unwrap_or_default();
                let slots = block[index..].iter_mut().step_by(channel_count);
                for (slot, &sample) in slots.zip(samples) {
                    *slot = float_to_i24(sample);
                }
            }
        }
        writer.write_pcm_samples(&self.interleaved, channel_count)?;
        Ok(())
    }

    /// Write `sample_count` sample frames of silence to the audio file.
    fn write_silence(&mut self, sample_count: u64, channel_count: usize) -> Result<()> {
        let Some(writer) = &mut self.audio_writer else {
            return Ok(());
        };
        self.written_frames += sample_count;
        if channel_count == 0 {
            return Ok(());
        }
        self.interleaved.clear();
        self.interleaved
            .resize(sample_count.min(CHUNK_FRAMES as u64) as usize * channel_count, 0);
        let mut left = sample_count;
        while left > 0 {
            let frames = left.min(CHUNK_FRAMES as u64) as usize;
            writer.write_pcm_samples(&self.interleaved[..frames * channel_count], channel_count)?;
            left -= frames as u64;
        }
        Ok(())
    }

    /// Fix the layout of the audio file on the first decoded frame and create
    /// it, then write the silence that was waiting for a layout.
    fn open_audio(
        &mut self,
        layout: FileLayout,
        base_path: &Option<PathBuf>,
        format: AudioFormat,
        sample_rate: u32,
        no_audio: bool,
    ) -> Result<()> {
        let channel_count = layout.channel_count();
        self.final_channel_count = channel_count;
        if let (Some(base_path), false) = (base_path, no_audio) {
            let (writer, target) =
                self.create_writer(base_path, format, sample_rate, channel_count, layout.atmos)?;
            self.audio_writer = Some(writer);
            self.target = Some(target);
        }
        self.layout = Some(layout);
        if let Some(silence) = self.leading_silence.take() {
            self.write_silence(silence.samples, channel_count)?;
        }
        Ok(())
    }

    /// Rewrite the open bed file as an object file, on the first object frame
    /// of a stream that began without objects.
    ///
    /// The frames before it were written as a plain bed, in the format that
    /// was asked for. The master set this frame starts names an
    /// `.atmos.audio` beside its `.atmos`, and its events count their samples
    /// from the start of the stream. So what is on disk is carried over,
    /// rather than left behind or continued: each bed channel goes to its
    /// place in the object file and the object channels are silent up to
    /// here. Keeping the bed file instead would drop every object of the
    /// programme, the price of one access unit without JOC at the head of an
    /// Atmos stream; and going on writing to it is what this used to do,
    /// 21-channel frames into a 6-channel file.
    fn promote_to_object_file(
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
            "E-AC-3 objects begin {} samples into a stream that started as a plain bed: rewriting the audio written so far as an object file",
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
        let (mut writer, target) =
            self.create_writer(base_path, AudioFormat::Caf, sample_rate, channel_count, true)?;

        // What the bed file has for each channel of the object file: nothing
        // for the objects, nor for a bed channel the stream did not carry
        // until now.
        let mut sources = layout.place_bed(&bed_layout.fullband, bed_layout.lfe);
        let dropped = bed_channel_count - sources.iter().flatten().count();
        if dropped > 0 {
            log::warn!(
                "{dropped} channel(s) of the plain bed have no place in the object file: dropped"
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
        atmos: bool,
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
        let (path, _) = create_output_paths(base_path, format, atmos);
        log::info!("Creating audio file: {}", path.display());
        let (writer, container) = match (format, atmos) {
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

    fn write_metadata_event(
        &mut self,
        oamd: &truehd::structs::oamd::ObjectAudioMetadataPayload,
        sample_rate: u32,
        base_path: &PathBuf,
    ) -> Result<()> {
        let mut conf = Configuration::with_oamd_payload(oamd, sample_rate, self.decoded_samples)?;
        let (mut events_diff, remove_header) = if !self.prev_events.is_empty() {
            (Event::compare_event_vectors(&self.prev_events, &conf.events), true)
        } else {
            (conf.events.clone(), false)
        };

        if conf.restates_current_state && remove_header {
            Event::drop_re_asserted_ramps(&mut events_diff);
        }

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

    /// Close the outputs. Takes what a frame handler takes, because a stream
    /// in which nothing decoded has no file yet: the silence that stood in
    /// for it is all there is to write, with the channel count its frame
    /// headers gave.
    pub fn finalize(
        &mut self,
        base_path: &Option<PathBuf>,
        format: AudioFormat,
        no_audio: bool,
    ) -> Result<()> {
        if let Some(silence) = self.leading_silence.take() {
            self.final_channel_count = silence.channel_count;
            if let (Some(base_path), false) = (base_path, no_audio) {
                let (writer, target) = self.create_writer(
                    base_path,
                    format,
                    silence.sample_rate,
                    silence.channel_count,
                    false,
                )?;
                self.audio_writer = Some(writer);
                self.target = Some(target);
            }
            self.write_silence(silence.samples, silence.channel_count)?;
        }
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
mod tests {
    use super::*;
    use eac3::{Extractor, ObjectPcmDecoder, PcmDecoder};
    use std::io::{Read, Seek, SeekFrom};

    /// One sample of a marked channel: channel `value` reads back as `value`.
    const MARK: i32 = 32_768;

    /// The first access unit of the golden JOC stream, decoded as the object
    /// frame it is and as the plain core frame a unit without a usable JOC
    /// payload comes out as - with every channel then set to a constant that
    /// names it, so that a sample that lands in another channel's place shows.
    /// Bed channel `n` (from 1) holds `n` in the object frame and `-n` in the
    /// core frame; object `k` (from 1) holds `100 + k`.
    fn frames() -> (ObjectPcmPushResult, PcmPushResult) {
        let stream = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/joc_atmos_1s.eac3"
        ))
        .unwrap();
        let mut extractor = Extractor::default();
        extractor.push_bytes(&stream);
        let unit = extractor.next_frame().unwrap().unwrap();
        let mut object = ObjectPcmDecoder::default()
            .push_access_unit(unit.as_bytes())
            .unwrap()
            .expect("the fixture carries JOC");
        let mut core = PcmDecoder::default()
            .push_access_unit(unit.as_bytes())
            .unwrap();
        mark_bed(&mut object.pcm.core, 1);
        for (k, channel) in object.pcm.object_channels.iter_mut().enumerate() {
            channel.fill((101 + k as i32) as f32 / 256.0);
        }
        mark_bed(&mut core.pcm, -1);
        assert_eq!(core.pcm.total_channels(), 6, "a 5.1 core");
        assert_eq!(object.pcm.object_count(), 15);
        assert_eq!(core.pcm.samples_per_channel(), UNIT);
        (object, core)
    }

    /// Sample frames in one access unit of the fixture.
    const UNIT: usize = 1536;

    fn mark_bed(core: &mut CorePcmFrame, sign: i32) {
        let channels = core.fullband_channels.iter_mut().chain(&mut core.lfe_channel);
        for (n, channel) in channels.enumerate() {
            channel.fill((sign * (n as i32 + 1)) as f32 / 256.0);
        }
    }

    const OBJECT_UNIT: [i32; 21] = [
        1, 2, 3, 4, 5, 6, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115,
    ];
    const CORE_UNIT: [i32; 21] = [-1, -2, -3, -4, -5, -6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    const SILENT_UNIT: [i32; 21] = [0; 21];

    fn silence() -> Eac3FrameMessage {
        // What the decoder thread sends: the channel count of the frame
        // header, which is the 5.1 core's.
        Eac3FrameMessage::Silence {
            sample_count: UNIT,
            sample_rate: 48_000,
            channel_count: 6,
        }
    }

    /// An empty directory of this test's own, and the output base path in it.
    fn out_dir(test: &str) -> (PathBuf, Option<PathBuf>) {
        let dir = std::env::temp_dir().join(format!("harletty-eac3-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = Some(dir.join("out"));
        (dir, base)
    }

    fn run(
        handler: &mut Eac3DecodeHandler,
        base: &Option<PathBuf>,
        format: AudioFormat,
        messages: Vec<Eac3FrameMessage>,
    ) {
        for message in messages {
            handler.handle_message(message, base, format, false).unwrap();
        }
        handler.finalize(base, format, false).unwrap();
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
        let samples = bytes.chunks_exact(3).map(|b| sample(b, big_endian)).collect();
        (channels, samples)
    }

    fn read_raw(path: &Path, skip: usize) -> Vec<i32> {
        let bytes = std::fs::read(path).unwrap();
        assert_eq!((bytes.len() - skip) % 3, 0);
        bytes[skip..].chunks_exact(3).map(|b| sample(b, false)).collect()
    }

    /// Every sample frame of the file, checked against the access unit it
    /// belongs to: `units[u]` is what each channel holds throughout unit `u`.
    /// One sample frame written with another channel count would shift every
    /// one after it.
    #[track_caller]
    fn assert_units(samples: &[i32], channels: usize, units: &[&[i32]]) {
        assert_eq!(
            samples.len(),
            units.len() * UNIT * channels,
            "{} access units of {channels} channels",
            units.len()
        );
        for (index, frame) in samples.chunks_exact(channels).enumerate() {
            let expected: Vec<i32> = units[index / UNIT].iter().map(|v| v * MARK).collect();
            assert_eq!(frame, expected, "sample frame {index}");
        }
    }

    /// The bug this guards: a core frame and a silence frame written into an
    /// object file with their own 6 channels, after which nothing in the
    /// 21-channel file is where it belongs.
    #[test]
    fn core_and_silence_frames_are_written_in_the_object_files_layout() {
        let (object, core) = frames();
        let (dir, base) = out_dir("object-file");
        let mut handler = Eac3DecodeHandler::default();
        run(
            &mut handler,
            &base,
            AudioFormat::Pcm,
            vec![
                Eac3FrameMessage::Object(object.clone()),
                Eac3FrameMessage::Core(core),
                silence(),
                Eac3FrameMessage::Object(object),
            ],
        );

        let (channels, samples) = read_caf(&dir.join("out.atmos.audio"));
        assert_eq!(channels, 21);
        assert_units(
            &samples,
            21,
            &[&OBJECT_UNIT, &CORE_UNIT, &SILENT_UNIT, &OBJECT_UNIT],
        );
        assert_eq!(handler.final_channel_count, 21, "the file's, not the last frame's");
        assert_eq!(handler.decoded_samples, 4 * UNIT as u64);
        assert_eq!(
            file_names(&dir),
            ["out.atmos", "out.atmos.audio", "out.atmos.metadata"]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An object frame is held to the file's layout as well: objects it lacks
    /// are silent, and one more than the file has is dropped.
    #[test]
    fn an_object_frame_with_another_object_count_keeps_the_files_layout() {
        let (object, _) = frames();
        let mut fewer = object.clone();
        fewer.pcm.object_channels.truncate(11);
        let mut more = object.clone();
        more.pcm.object_channels.push(vec![0.5; UNIT]);

        let (dir, base) = out_dir("object-count");
        let mut handler = Eac3DecodeHandler::default();
        run(
            &mut handler,
            &base,
            AudioFormat::Pcm,
            vec![
                Eac3FrameMessage::Object(object.clone()),
                Eac3FrameMessage::Object(fewer),
                Eac3FrameMessage::Object(more),
                Eac3FrameMessage::Object(object),
            ],
        );
        let (channels, samples) = read_caf(&dir.join("out.atmos.audio"));
        assert_eq!(channels, 21);
        let mut fewer_unit = OBJECT_UNIT;
        fewer_unit[17..].fill(0);
        assert_units(
            &samples,
            21,
            &[&OBJECT_UNIT, &fewer_unit, &OBJECT_UNIT, &OBJECT_UNIT],
        );
        assert_eq!(handler.final_channel_count, 21);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The mirror case: the stream starts without objects, the audio file is
    /// opened as the plain bed that was asked for, and then an object frame
    /// arrives. The bed already written moves into the object file.
    #[test]
    fn a_bed_file_is_rewritten_as_an_object_file_on_the_first_object_frame() {
        for (format, bed_file) in [
            (AudioFormat::Pcm, "out.pcm"),
            (AudioFormat::Caf, "out.caf"),
            (AudioFormat::W64, "out.wav"),
        ] {
            let (object, core) = frames();
            let (dir, base) = out_dir(&format!("promoted-{bed_file}"));
            let mut handler = Eac3DecodeHandler::default();
            for message in [Eac3FrameMessage::Core(core), silence()] {
                handler.handle_message(message, &base, format, false).unwrap();
            }
            assert_eq!(file_names(&dir), [bed_file], "{format:?}: a plain bed so far");
            assert_eq!(handler.final_channel_count, 6);

            run(
                &mut handler,
                &base,
                format,
                vec![Eac3FrameMessage::Object(object)],
            );
            let (channels, samples) = read_caf(&dir.join("out.atmos.audio"));
            assert_eq!(channels, 21, "{format:?}");
            assert_units(&samples, 21, &[&CORE_UNIT, &SILENT_UNIT, &OBJECT_UNIT]);
            assert_eq!(handler.final_channel_count, 21);
            assert_eq!(
                file_names(&dir),
                ["out.atmos", "out.atmos.audio", "out.atmos.metadata"],
                "{format:?}: the bed file is gone, and no temporary is left"
            );
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// Silence before anything decoded waits for the first decoded frame to
    /// say what the file holds, instead of opening a 5.1 file on the count in
    /// a frame header.
    #[test]
    fn leading_silence_is_written_in_the_layout_of_the_first_decoded_frame() {
        let (object, _) = frames();
        let (dir, base) = out_dir("leading-silence");
        let mut handler = Eac3DecodeHandler::default();
        run(
            &mut handler,
            &base,
            AudioFormat::Pcm,
            vec![silence(), silence(), Eac3FrameMessage::Object(object)],
        );
        let (channels, samples) = read_caf(&dir.join("out.atmos.audio"));
        assert_eq!(channels, 21);
        assert_units(&samples, 21, &[&SILENT_UNIT, &SILENT_UNIT, &OBJECT_UNIT]);
        assert_eq!(handler.decoded_samples, 3 * UNIT as u64);
        assert_eq!(
            file_names(&dir),
            ["out.atmos", "out.atmos.audio", "out.atmos.metadata"]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A stream in which nothing decodes still gets its silence, with the
    /// channel count of its frame headers - and the same sample count
    /// whether or not audio is written.
    #[test]
    fn a_stream_of_silence_alone_is_written_on_finalize() {
        let (dir, base) = out_dir("silence-only");
        let mut handler = Eac3DecodeHandler::default();
        run(&mut handler, &base, AudioFormat::Pcm, vec![silence(), silence()]);
        let samples = read_raw(&dir.join("out.pcm"), 0);
        assert_eq!(samples, vec![0; 2 * UNIT * 6]);
        assert_eq!(handler.final_channel_count, 6);
        assert_eq!(handler.decoded_samples, 2 * UNIT as u64);
        assert_eq!(file_names(&dir), ["out.pcm"]);

        let mut no_audio = Eac3DecodeHandler::default();
        for message in [silence(), silence()] {
            no_audio
                .handle_message(message, &base, AudioFormat::Pcm, true)
                .unwrap();
        }
        no_audio.finalize(&base, AudioFormat::Pcm, true).unwrap();
        assert_eq!(no_audio.decoded_samples, handler.decoded_samples);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A plain bed file keeps its layout too. A 7.1 file that meets the 5.1
    /// core a declined merge falls back to has that core's channels where
    /// their names put them - the LFE last, not sixth - and the back pair
    /// silent; and substituted silence spans all eight channels.
    #[test]
    fn a_narrower_bed_frame_is_placed_by_channel_name() {
        let (_, core) = frames();
        let mut wide = core.clone();
        for back in [BedChannel::RearLeft, BedChannel::RearRight] {
            wide.pcm.fullband_channel_order.push(back);
            wide.pcm.fullband_channels.push(vec![0.0; UNIT]);
        }
        // Full-band channels 1 to 7, then the LFE as 8.
        mark_bed(&mut wide.pcm, 1);

        let (dir, base) = out_dir("bed-by-name");
        let mut handler = Eac3DecodeHandler::default();
        run(
            &mut handler,
            &base,
            AudioFormat::Pcm,
            vec![
                Eac3FrameMessage::Core(wide.clone()),
                Eac3FrameMessage::Core(core.clone()),
                silence(),
                Eac3FrameMessage::Core(wide.clone()),
            ],
        );
        let samples = read_raw(&dir.join("out.pcm"), 0);
        let wide_unit = [1, 2, 3, 4, 5, 6, 7, 8];
        assert_units(
            &samples,
            8,
            &[&wide_unit, &[-1, -2, -3, -4, -5, 0, 0, -6], &[0; 8], &wide_unit],
        );
        assert_eq!(handler.final_channel_count, 8);
        std::fs::remove_dir_all(&dir).unwrap();

        // The other way round the file is 5.1 and stays 5.1: the back pair of
        // a later 7.1 frame has no place in it.
        let (dir, base) = out_dir("bed-by-name-narrow");
        let mut handler = Eac3DecodeHandler::default();
        run(
            &mut handler,
            &base,
            AudioFormat::Pcm,
            vec![Eac3FrameMessage::Core(core), Eac3FrameMessage::Core(wide)],
        );
        let samples = read_raw(&dir.join("out.pcm"), 0);
        assert_units(
            &samples,
            6,
            &[&[-1, -2, -3, -4, -5, -6], &[1, 2, 3, 4, 5, 8]],
        );
        assert_eq!(handler.final_channel_count, 6);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The mono set refuses a write of another channel count outright, so a
    /// core or silence frame in an object stream used to end the decode.
    /// Here the files stay in lockstep, through the rewrite as well.
    #[test]
    fn mono_files_keep_one_layout() {
        let (object, core) = frames();
        let (dir, base) = out_dir("mono");
        let prefix = dir.join("ch");
        let mut handler = Eac3DecodeHandler::default();
        handler.mono_prefix = Some(prefix.clone());
        run(
            &mut handler,
            &base,
            AudioFormat::Pcm,
            vec![
                Eac3FrameMessage::Core(core.clone()),
                Eac3FrameMessage::Object(object.clone()),
                Eac3FrameMessage::Core(core),
                silence(),
                Eac3FrameMessage::Object(object),
            ],
        );
        let units = [&CORE_UNIT, &OBJECT_UNIT, &CORE_UNIT, &SILENT_UNIT, &OBJECT_UNIT];
        for channel in 0..21 {
            let samples = read_raw(&mono_path(&prefix, channel), 44);
            let expected: Vec<&[i32]> = units.iter().map(|unit| &unit[channel..=channel]).collect();
            assert_units(&samples, 1, &expected);
        }
        let mut expected_files: Vec<String> = (0..21).map(|n| format!("ch_{n}.wav")).collect();
        expected_files.extend(["out.atmos".into(), "out.atmos.metadata".into()]);
        expected_files.sort();
        assert_eq!(file_names(&dir), expected_files);
        assert_eq!(handler.final_channel_count, 21);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
