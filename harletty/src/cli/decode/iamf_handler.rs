//! Turn a decoded IAMF stream into a DAMF master set.
//!
//! Each object the decoder hands out (IAMF v2.0) becomes an object of the
//! master set, and the elements that are not objects - an LFE alone, a v1.1
//! channel bed - are rendered to System J and become its bed, as far as they
//! reach into it: an LFE-only element is a bed of one LFE, a 7.1.4 element a
//! 7.1.4 bed. A stream without objects is a bed-only master set.
//!
//! The objects' positions come as a trajectory, evaluated every
//! [`POSITION_INTERVAL`](crate::iamf::POSITION_INTERVAL) samples. The master
//! set states them as moves: a run at constant speed is one event ramping to
//! where it ends, a stop is where the next one starts, and a static object is
//! stated once. That is what the trajectory says, to within [`TOLERANCE`],
//! without one event per evaluation; for a stream encoded from a master set,
//! it gives back as many moves as that master stated.
//!
//! The audio file's layout is fixed by the first sequence. A later sequence
//! is written in it, each bed channel where its speaker is and each object
//! by its rank, and a mismatch is reported once.

use super::atmos::create_damf_header_file;
use super::output::{AudioWriter, create_output_paths, float_to_i24, mono_path};
use crate::cli::command::{AudioFormat, WarpMode};
use crate::dts_to_oamd::{is_atmos_bed_speaker, static_speaker_position};
use crate::iamf::{BED_CHANNELS, Sequence, Sink, Unit, adm_position};
use crate::iamf_to_oamd::{SYSTEM_J_SPEAKERS, convert_iamf};
use anyhow::{Result, bail};
use damf::{Configuration, Event, SourceCodec};
use indicatif::ProgressBar;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use truehd::structs::oamd::SpeakerLabels;

/// How far a position the decoder evaluated may stray from the straight
/// ramp an event states through it: a few times the float noise of a linear
/// animation evaluated in single precision and rounded, so that the
/// trajectory read back from the events is the decoder's.
const TOLERANCE: f64 = 5e-7;

/// Longest ramp an event states. A longer run at constant speed is cut into
/// ramps of this length, the longest of OAMD's coded ramp durations, so the
/// master set's ramps stay within what an Atmos bitstream carries.
const MAX_RAMP: u64 = 2048;

/// Where an object first is when the stream says nothing: straight ahead.
const FRONT: [f64; 3] = [0.0, 1.0, 0.0];

/// The channels of the audio file, in file order: fixed by the first
/// sequence.
#[derive(Clone, Debug, PartialEq)]
struct FileLayout {
    /// System J channels (indices into the decoder's bed) the bed holds, in
    /// DAMF order.
    bed: Vec<usize>,
    /// IAMF objects, after the bed.
    objects: usize,
    /// System J channels an Atmos bed cannot hold (`--bed-conform`),
    /// written as static objects at their speakers after the IAMF objects.
    statics: Vec<usize>,
}

impl FileLayout {
    fn of(sequence: &Sequence, conform: bool) -> Self {
        let (bed, statics) = sequence
            .bed
            .iter()
            .partition(|&&j| !conform || is_atmos_bed_speaker(SYSTEM_J_SPEAKERS[j]));
        Self {
            bed,
            objects: sequence.objects,
            statics,
        }
    }

    fn channel_count(&self) -> usize {
        self.bed.len() + self.objects + self.statics.len()
    }

    fn speakers(&self) -> Vec<SpeakerLabels> {
        self.bed.iter().map(|&j| SYSTEM_J_SPEAKERS[j]).collect()
    }
}

/// One object's path as the master set states it: the last position stated
/// (the anchor) and the points of the straight run followed from it since.
#[derive(Clone, Copy, Debug)]
struct Trajectory {
    /// The sample from which the anchor holds.
    anchor_at: u64,
    anchor: [f64; 3],
    /// The run's points, in order: each within [`TOLERANCE`] of the line
    /// from the anchor to the last.
    run: [(u64, [f64; 3]); RUN_POINTS],
    run_len: usize,
}

/// Points a run holds at most: a [`MAX_RAMP`] at the decoder's 256-sample
/// evaluation interval is 8; units cut short by a trim put some closer.
const RUN_POINTS: usize = 16;

/// A position event of the metadata file, before it is written.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Move {
    sample_pos: u64,
    id: u32,
    pos: [f64; 3],
    ramp: u32,
}

impl Trajectory {
    fn new(at: u64, pos: [f64; 3]) -> Self {
        Self {
            anchor_at: at,
            anchor: pos,
            run: [(0, [0.0; 3]); RUN_POINTS],
            run_len: 0,
        }
    }

    /// The last point followed: the run's end, or the anchor.
    fn last(&self) -> (u64, [f64; 3]) {
        match self.run_len {
            0 => (self.anchor_at, self.anchor),
            len => self.run[len - 1],
        }
    }

    /// Where the object is as far as the trajectory has followed it.
    fn position(&self) -> [f64; 3] {
        self.last().1
    }

    /// Whether the run, extended to `pos` at `at`, is still one straight
    /// ramp from the anchor: every point it went through within
    /// [`TOLERANCE`] of the line, and no longer than [`MAX_RAMP`].
    fn extends_to(&self, at: u64, pos: [f64; 3]) -> bool {
        let span = at - self.anchor_at;
        if span > MAX_RAMP || self.run_len == RUN_POINTS {
            return false;
        }
        self.run[..self.run_len].iter().all(|&(t, p)| {
            let along = (t - self.anchor_at) as f64 / span as f64;
            (0..3).all(|axis| {
                let line = self.anchor[axis] + (pos[axis] - self.anchor[axis]) * along;
                (line - p[axis]).abs() <= TOLERANCE
            })
        })
    }

    /// The object is at `pos` at sample `at`: extend the run, or close it
    /// at its last point and start the next one from there.
    fn point(&mut self, id: u32, at: u64, pos: [f64; 3], out: &mut Vec<Move>) {
        if at <= self.last().0 {
            // The point the trajectory starts from, stated already.
            return;
        }
        if !self.extends_to(at, pos) {
            self.close(id, out);
        }
        self.run[self.run_len] = (at, pos);
        self.run_len += 1;
    }

    /// End the run where it got to: one event ramping there from the
    /// anchor, unless it never left it.
    fn close(&mut self, id: u32, out: &mut Vec<Move>) {
        if self.run_len == 0 {
            return;
        }
        let (end_at, end) = self.last();
        if end != self.anchor {
            out.push(Move {
                sample_pos: self.anchor_at,
                id,
                pos: end,
                ramp: (end_at - self.anchor_at) as u32,
            });
        }
        self.anchor_at = end_at;
        self.anchor = end;
        self.run_len = 0;
    }
}

pub struct IamfDecodeHandler {
    pub base_path: Option<PathBuf>,
    /// Write one mono WAV per channel, `<prefix>_<n>.wav`, instead of the
    /// interleaved audio file.
    pub mono_prefix: Option<PathBuf>,
    pub no_audio: bool,
    pub warp_mode: Option<WarpMode>,
    /// Keep the bed to what an Atmos bed can hold: the System J heights
    /// become static objects.
    pub bed_conform: bool,
    pub pb: Option<ProgressBar>,
    pub decoded_samples: u64,
    pub final_sample_rate: u32,
    /// What the audio file holds, from the first sequence.
    layout: Option<FileLayout>,
    /// The outputs are open: the first unit came.
    opened: bool,
    audio_writer: Option<AudioWriter>,
    metadata_writer: Option<BufWriter<File>>,
    /// One per IAMF object of the file; the static ones never move.
    trajectories: Vec<Trajectory>,
    /// Events not written yet, until no run still open can come before them.
    moves: Vec<Move>,
    /// Interleaving scratch, reused across units.
    interleaved: Vec<i32>,
    warned_layout: bool,
    discontinuities: u64,
}

impl IamfDecodeHandler {
    pub fn new(base_path: Option<PathBuf>) -> Self {
        Self {
            base_path,
            mono_prefix: None,
            no_audio: false,
            warp_mode: None,
            bed_conform: false,
            pb: None,
            decoded_samples: 0,
            final_sample_rate: 48_000,
            layout: None,
            opened: false,
            audio_writer: None,
            metadata_writer: None,
            trajectories: Vec::new(),
            moves: Vec::new(),
            interleaved: Vec::new(),
            warned_layout: false,
            discontinuities: 0,
        }
    }

    /// Open the master set on the first unit, whose sample rate and object
    /// positions it starts with: the header, the first events, the audio.
    fn open(&mut self, unit: &Unit<'_>) -> Result<()> {
        self.opened = true;
        let Some(layout) = &self.layout else {
            return Ok(());
        };
        let mut positions: Vec<[f64; 3]> = (0..layout.objects)
            .map(|rank| {
                unit.objects
                    .get(rank)
                    .and_then(|object| object.positions.first())
                    .map_or(FRONT, |&(_, position)| adm_position(position))
            })
            .collect();
        self.trajectories = positions
            .iter()
            .map(|&position| Trajectory::new(self.decoded_samples, position))
            .collect();
        positions.extend(
            layout
                .statics
                .iter()
                .map(|&j| static_speaker_position(SYSTEM_J_SPEAKERS[j])),
        );
        let channel_count = layout.channel_count();
        let Some(base_path) = &self.base_path else {
            return Ok(());
        };

        let oamd = convert_iamf(&layout.speakers(), &positions);
        if layout.bed.is_empty() {
            // Objects alone: a header with no bed instance rather than an
            // empty one. The events' projection wants its one bed instance,
            // which states nothing about the objects.
            let mut objects_only = oamd.clone();
            objects_only.program_assignment.bed_assignment.clear();
            create_damf_header_file(base_path, &objects_only, self.warp_mode, SourceCodec::Iamf)?;
        } else {
            create_damf_header_file(base_path, &oamd, self.warp_mode, SourceCodec::Iamf)?;
        }

        let (audio_path, metadata_path) = create_output_paths(base_path, AudioFormat::Caf, true);
        log::info!("Creating metadata file: {}", metadata_path.display());
        let mut metadata = BufWriter::new(File::create(metadata_path)?);
        let mut first =
            Configuration::with_oamd_payload(&oamd, unit.sample_rate, self.decoded_samples)?;
        // The positions as the later events write them, not as the payload's
        // encoding rounds them.
        for (event, &position) in first.events.iter_mut().zip(&positions) {
            event.set_pos(position);
        }
        write!(metadata, "{}", first.serialize_events(false))?;
        self.metadata_writer = Some(metadata);

        if !self.no_audio {
            self.audio_writer = Some(match &self.mono_prefix {
                Some(prefix) => {
                    log::info!(
                        "Creating {channel_count} mono audio files: {}",
                        mono_path(prefix, 0).display()
                    );
                    AudioWriter::create_mono(prefix, unit.sample_rate, channel_count)?
                }
                None => {
                    log::info!("Creating audio file: {}", audio_path.display());
                    AudioWriter::create_caf(
                        audio_path,
                        unit.sample_rate,
                        channel_count as u32,
                        &[],
                    )?
                }
            });
        }
        Ok(())
    }

    /// Write one unit's audio in the file's layout: each bed channel from
    /// the decoder's System J bed, each object by its rank, silence where
    /// the unit has nothing for a channel.
    fn write_audio(&mut self, unit: &Unit<'_>) -> Result<()> {
        let (Some(layout), Some(writer)) = (&self.layout, &mut self.audio_writer) else {
            return Ok(());
        };
        let channel_count = layout.channel_count();
        if channel_count == 0 || unit.samples == 0 {
            return Ok(());
        }
        self.interleaved.clear();
        self.interleaved.resize(unit.samples * channel_count, 0);
        // The bed, then the IAMF objects, then the static objects; a channel
        // the unit has nothing for stays silent.
        let statics_from = layout.bed.len() + layout.objects;
        if let Some(bed) = unit.bed {
            let bed_channel = |j: usize| bed[j..].iter().step_by(BED_CHANNELS);
            for (column, &j) in layout.bed.iter().enumerate() {
                fill_column(&mut self.interleaved, column, channel_count, bed_channel(j));
            }
            for (column, &j) in layout.statics.iter().enumerate() {
                let column = statics_from + column;
                fill_column(&mut self.interleaved, column, channel_count, bed_channel(j));
            }
        }
        for (rank, object) in unit.objects.iter().take(layout.objects).enumerate() {
            let column = layout.bed.len() + rank;
            fill_column(
                &mut self.interleaved,
                column,
                channel_count,
                object.samples.iter(),
            );
        }
        writer.write_pcm_samples(&self.interleaved, channel_count)
    }

    /// Follow each object's trajectory over the unit, and write the events
    /// no run still open can come before.
    fn track_objects(&mut self, unit: &Unit<'_>) -> Result<()> {
        for (rank, trajectory) in self.trajectories.iter_mut().enumerate() {
            let id = 10 + rank as u32;
            match unit.objects.get(rank) {
                Some(object) => {
                    for &(offset, position) in &object.positions {
                        let at = self.decoded_samples + u64::from(offset);
                        trajectory.point(id, at, adm_position(position), &mut self.moves);
                    }
                }
                // A sequence with fewer objects than the file: the missing
                // ones stay where they were.
                None => {
                    let position = trajectory.position();
                    trajectory.point(id, self.decoded_samples, position, &mut self.moves);
                }
            }
        }
        // A trajectory states its next move from its anchor at the earliest.
        let open_from = self.trajectories.iter().map(|t| t.anchor_at).min();
        self.write_moves(open_from)
    }

    /// Write the events before `before` (all of them for `None`), in
    /// sample order.
    fn write_moves(&mut self, before: Option<u64>) -> Result<()> {
        let ready = match before {
            Some(limit) => self.moves.iter().filter(|m| m.sample_pos < limit).count(),
            None => self.moves.len(),
        };
        if ready == 0 {
            return Ok(());
        }
        self.moves.sort_by_key(|m| (m.sample_pos, m.id));
        let events: Vec<Event> = self
            .moves
            .drain(..ready)
            .map(|m| Event::position_update(m.id, m.sample_pos, m.pos, m.ramp))
            .collect();
        if let Some(writer) = &mut self.metadata_writer {
            let mut batch = Configuration {
                sample_rate: None,
                events,
                restates_current_state: false,
            };
            write!(writer, "{}", batch.serialize_events(true))?;
        }
        Ok(())
    }

    /// Close the runs: every trajectory ends where the stream left it.
    fn close_runs(&mut self) {
        for (rank, trajectory) in self.trajectories.iter_mut().enumerate() {
            trajectory.close(10 + rank as u32, &mut self.moves);
        }
    }

    pub fn finalize(&mut self) -> Result<()> {
        if self.layout.is_none() {
            bail!("no IA sequence header found in the input");
        }
        if !self.opened {
            bail!("the IA sequence has no temporal unit that decodes");
        }
        self.close_runs();
        self.write_moves(None)?;
        if self.discontinuities > 0 {
            log::warn!(
                "{} decode error(s): the units they cost are missing from the output, which is \
                 that much shorter than the stream",
                self.discontinuities
            );
        }
        if let Some(mut writer) = self.audio_writer.take() {
            writer.finish()?;
        }
        if let Some(mut writer) = self.metadata_writer.take() {
            writer.flush()?;
        }
        Ok(())
    }
}

/// Write one channel's samples into its column of interleaved frames.
fn fill_column<'a>(
    interleaved: &mut [i32],
    column: usize,
    channel_count: usize,
    samples: impl Iterator<Item = &'a f32>,
) {
    let slots = interleaved[column..].iter_mut().step_by(channel_count);
    for (slot, &sample) in slots.zip(samples) {
        *slot = float_to_i24(sample);
    }
}

impl Sink for IamfDecodeHandler {
    fn sequence(&mut self, sequence: &Sequence) -> Result<()> {
        let layout = FileLayout::of(sequence, self.bed_conform);
        match &self.layout {
            None => self.layout = Some(layout),
            Some(file) if *file != layout && !self.warned_layout => {
                self.warned_layout = true;
                log::warn!(
                    "IAMF sequence with another layout ({} bed channel(s), {} object(s)) in a \
                     file of {} bed channel(s) and {} object(s): written in the file's layout, \
                     each bed channel where its speaker is, each object by its rank, the rest \
                     dropped or silent",
                    layout.bed.len(),
                    layout.objects,
                    file.bed.len(),
                    file.objects
                );
            }
            Some(_) => {}
        }
        Ok(())
    }

    fn unit(&mut self, unit: Unit<'_>) -> Result<()> {
        if !self.opened {
            self.open(&unit)?;
        }
        self.final_sample_rate = unit.sample_rate;
        self.write_audio(&unit)?;
        self.track_objects(&unit)?;
        self.decoded_samples += unit.samples as u64;
        if let Some(pb) = &self.pb {
            pb.inc(1);
        }
        Ok(())
    }

    fn discontinuity(&mut self) {
        self.discontinuities += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn follow(points: &[(u64, [f64; 3])]) -> Vec<Move> {
        let mut moves = Vec::new();
        let mut trajectory = Trajectory::new(points[0].0, points[0].1);
        for &(at, pos) in points {
            trajectory.point(10, at, pos, &mut moves);
        }
        trajectory.close(10, &mut moves);
        moves
    }

    #[test]
    fn a_static_object_states_no_move() {
        let points: Vec<_> = (0..100).map(|k| (k * 256, [0.5, 0.5, 0.0])).collect();
        assert!(follow(&points).is_empty());
    }

    /// A run at constant speed is one event per [`MAX_RAMP`], ramping from
    /// where it starts to where it gets.
    #[test]
    fn a_straight_run_is_one_event_per_longest_ramp() {
        let points: Vec<_> = (0..=16)
            .map(|k| (k * 256, [-1.0 + k as f64 / 8.0, 1.0, 0.0]))
            .collect();
        let moves = follow(&points);
        assert_eq!(
            moves,
            [
                Move {
                    sample_pos: 0,
                    id: 10,
                    pos: [0.0, 1.0, 0.0],
                    ramp: 2048
                },
                Move {
                    sample_pos: 2048,
                    id: 10,
                    pos: [1.0, 1.0, 0.0],
                    ramp: 2048
                },
            ]
        );
    }

    /// A stop ends the run, and the next move starts from the last point at
    /// rest, not from where the rest began.
    #[test]
    fn a_move_after_a_rest_ramps_from_the_last_point_at_rest() {
        let mut points: Vec<_> = (0..4).map(|k| (k * 256, [0.0, 0.0, 0.0])).collect();
        points.push((1024, [0.25, 0.0, 0.0]));
        points.push((1280, [0.5, 0.0, 0.0]));
        points.extend((6..10).map(|k| (k * 256, [0.5, 0.0, 0.0])));
        let moves = follow(&points);
        assert_eq!(
            moves,
            [Move {
                sample_pos: 768,
                id: 10,
                pos: [0.5, 0.0, 0.0],
                ramp: 512
            }]
        );
    }

    /// A turn cuts the run at the corner.
    #[test]
    fn a_turn_cuts_the_run_at_the_corner() {
        let points = [
            (0, [0.0, 0.0, 0.0]),
            (256, [0.1, 0.0, 0.0]),
            (512, [0.2, 0.0, 0.0]),
            (768, [0.2, 0.1, 0.0]),
            (1024, [0.2, 0.2, 0.0]),
        ];
        let moves = follow(&points);
        assert_eq!(moves.len(), 2);
        assert_eq!((moves[0].sample_pos, moves[0].ramp), (0, 512));
        assert_eq!(moves[0].pos, [0.2, 0.0, 0.0]);
        assert_eq!((moves[1].sample_pos, moves[1].ramp), (512, 512));
        assert_eq!(moves[1].pos, [0.2, 0.2, 0.0]);
    }
}
