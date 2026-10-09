//! Turn a decoded IAMF stream into a DAMF master set.
//!
//! Each object the decoder hands out (IAMF v2.0) becomes an object of the
//! master set, and the elements that are not objects - an LFE alone, a v1.1
//! channel bed - are rendered to System J and become its bed, as far as they
//! reach into it: an LFE-only element is a bed of one LFE, a 7.1.4 element a
//! 7.1.4 bed. A stream without objects is a bed-only master set.
//!
//! The objects' moves are the stream's own. Each position subblock the
//! decoder reports ([`DecodedObject::moves`]) is one event: a jump where a
//! step lands, a ramp to where a line ends over its length. A straight run
//! the encoder cut at its unit boundaries is put back together: a piece
//! starting at a unit's first sample, where the one before it ended, on
//! the line through it to within a step of the coding ([`joint_tolerance`]),
//! lengthens that ramp instead of starting one. A stream harlettizer wrote
//! from a master set gives that master's events back - the same samples,
//! the same ramps, each position to the coding's step - rather than a
//! reading of the trajectory through them. What the stream cannot carry is
//! not recovered: a master's ramp its next event cuts short is the trajectory
//! the stream codes, a ramp to where it was cut.
//!
//! What one event cannot state as a straight ramp - a Bezier subblock, a
//! polar line, which runs along a great-circle arc - is followed through the
//! positions the decoder evaluates every
//! [`POSITION_INTERVAL`](crate::iamf::POSITION_INTERVAL) samples: a run at
//! constant speed is one event ramping to where it ends, to within
//! [`TOLERANCE`], and the subblock's own end closes the last one exactly.
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
use iamf_dec::position::{PositionAnimationType, PositionMove};
use iamf_dec::stream::DecodedObject;
use iamf_obu::descriptors::PositionKind;
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

/// Longest ramp an event states for a run followed through the evaluated
/// positions. A longer run at constant speed is cut into ramps of this
/// length, the longest of OAMD's coded ramp durations, so the master set's
/// ramps stay within what an Atmos bitstream carries.
const MAX_RAMP: u64 = 2048;

/// Where an object first is when the stream says nothing: straight ahead.
const FRONT: [f64; 3] = [0.0, 1.0, 0.0];

/// How far the joint of two pieces of one straight run may sit from the
/// line through them: the encoder stated the run's start, the joint and
/// its end each to the coding's step, so the joint is within a step of the
/// line through the other two, and a little float noise. `None` for a polar
/// coding, whose lines are arcs: they are followed through the evaluated
/// positions instead.
fn joint_tolerance(kind: PositionKind) -> Option<f64> {
    match kind {
        PositionKind::Cart16 => Some(1.0 / 32767.0 + 1e-6),
        PositionKind::Cart8 => Some(1.0 / 127.0 + 1e-6),
        PositionKind::Polar => None,
    }
}

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

/// A position event of the metadata file, before it is written.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Move {
    sample_pos: u64,
    id: u32,
    pos: [f64; 3],
    ramp: u32,
}

/// A ramp stated and still open: a subblock continuing it on the same line
/// may still come, and then it ends later and further.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Ramp {
    start_at: u64,
    from: [f64; 3],
    end_at: u64,
    to: [f64; 3],
}

impl Ramp {
    fn event(&self, id: u32) -> Move {
        Move {
            sample_pos: self.start_at,
            id,
            pos: self.to,
            ramp: (self.end_at - self.start_at) as u32,
        }
    }

    /// Whether a line from this ramp's end to `to` at `end_at` continues
    /// it: the joint sits on the straight line from the ramp's start to
    /// `to`, within `tolerance` on every axis.
    fn continues_to(&self, end_at: u64, to: [f64; 3], tolerance: f64) -> bool {
        let along = (self.end_at - self.start_at) as f64 / (end_at - self.start_at) as f64;
        (0..3).all(|axis| {
            let line = self.from[axis] + (to[axis] - self.from[axis]) * along;
            (line - self.to[axis]).abs() <= tolerance
        })
    }
}

/// A subblock no event states as one straight ramp, followed through the
/// evaluated positions until `until`, where the object is at `end`.
#[derive(Clone, Copy, Debug)]
struct Sampled {
    until: u64,
    end: [f64; 3],
    follower: Trajectory,
}

/// One object's path as the master set states it.
#[derive(Clone, Copy, Debug)]
struct ObjectPath {
    /// Where the object is, as far as the moves have stated it: the end of
    /// the last one.
    position: [f64; 3],
    open: Option<Ramp>,
    sampled: Option<Sampled>,
}

impl ObjectPath {
    fn new(position: [f64; 3]) -> Self {
        Self {
            position,
            open: None,
            sampled: None,
        }
    }

    /// The earliest sample an event of this object can still be stated at,
    /// if any is pending: nothing earlier than it is still to come.
    fn pending_from(&self) -> Option<u64> {
        let open = self.open.map(|ramp| ramp.start_at);
        let sampled = self.sampled.map(|sampled| sampled.follower.anchor_at);
        match (open, sampled) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn close_open(&mut self, id: u32, out: &mut Vec<Move>) {
        if let Some(ramp) = self.open.take() {
            out.push(ramp.event(id));
        }
    }

    /// The followed subblock ends: the object is at its end, exactly.
    fn close_sampled(&mut self, id: u32, out: &mut Vec<Move>) {
        if let Some(mut sampled) = self.sampled.take() {
            sampled.follower.point(id, sampled.until, sampled.end, out);
            sampled.follower.close(id, out);
        }
    }

    /// The object is at `to` from `at` on: a jump there, unless it is there.
    fn jump(&mut self, id: u32, at: u64, to: [f64; 3], out: &mut Vec<Move>) {
        if to != self.position {
            self.close_open(id, out);
            out.push(Move {
                sample_pos: at,
                id,
                pos: to,
                ramp: 0,
            });
            self.position = to;
        }
    }

    /// A position subblock starts at `at`, which is a unit's first sample
    /// when `at_unit_start`: where the encoder cuts a run into pieces, and
    /// so the only place a piece continuing the open ramp is put back on
    /// it. Two of a master's own ramps meeting elsewhere stay two events,
    /// though they may be one straight line.
    fn subblock(
        &mut self,
        id: u32,
        at: u64,
        at_unit_start: bool,
        subblock: &PositionMove,
        tolerance: Option<f64>,
        out: &mut Vec<Move>,
    ) {
        let from = adm_position(subblock.from);
        let to = adm_position(subblock.to);
        // The subblock followed through the evaluated positions ends where
        // this one starts.
        self.close_sampled(id, out);
        let end_at = at + u64::from(subblock.duration);
        match (subblock.animation, tolerance) {
            (PositionAnimationType::Step, _) => self.jump(id, at, to, out),
            (
                PositionAnimationType::Linear | PositionAnimationType::InterLinear,
                Some(tolerance),
            ) => {
                self.jump(id, at, from, out);
                if to == from {
                    // A standstill coded as a line.
                    return;
                }
                if let Some(open) = &mut self.open {
                    if at_unit_start
                        && open.end_at == at
                        && open.continues_to(end_at, to, tolerance)
                    {
                        open.end_at = end_at;
                        open.to = to;
                        self.position = to;
                        return;
                    }
                }
                self.close_open(id, out);
                self.open = Some(Ramp {
                    start_at: at,
                    from,
                    end_at,
                    to,
                });
                self.position = to;
            }
            _ => {
                // A curve, or a polar line, which is an arc: followed
                // through the evaluated positions.
                self.jump(id, at, from, out);
                self.close_open(id, out);
                self.sampled = Some(Sampled {
                    until: end_at,
                    end: to,
                    follower: Trajectory::new(at, from),
                });
                self.position = to;
            }
        }
    }

    /// The decoder evaluated the object at `pos` at sample `at`.
    fn evaluated(&mut self, id: u32, at: u64, pos: [f64; 3], out: &mut Vec<Move>) {
        if let Some(sampled) = &mut self.sampled {
            if at > sampled.follower.last().0 && at < sampled.until {
                sampled.follower.point(id, at, pos, out);
            }
        }
    }

    /// The unit ends and the next starts at `next`: a ramp that ended
    /// before it can no longer be continued, a followed subblock that ended
    /// before it is over.
    fn unit_ended(&mut self, id: u32, next: u64, out: &mut Vec<Move>) {
        if self.open.is_some_and(|ramp| ramp.end_at < next) {
            self.close_open(id, out);
        }
        if self.sampled.is_some_and(|sampled| sampled.until < next) {
            self.close_sampled(id, out);
        }
    }

    /// The stream ends: every path ends where it got to.
    fn finish(&mut self, id: u32, out: &mut Vec<Move>) {
        self.close_sampled(id, out);
        self.close_open(id, out);
    }
}

/// Follow one object through a unit starting at sample `base`: its
/// subblocks and the positions evaluated between them, in time order, then
/// the unit's end.
fn follow_unit(
    path: &mut ObjectPath,
    id: u32,
    base: u64,
    object: &DecodedObject,
    samples: usize,
    out: &mut Vec<Move>,
) {
    let tolerance = joint_tolerance(object.position_kind);
    let mut points = object.positions.iter().peekable();
    for subblock in &object.moves {
        while let Some(&&(offset, position)) = points.peek() {
            if offset >= subblock.offset {
                break;
            }
            path.evaluated(id, base + u64::from(offset), adm_position(position), out);
            points.next();
        }
        path.subblock(
            id,
            base + u64::from(subblock.offset),
            subblock.offset == 0,
            subblock,
            tolerance,
            out,
        );
    }
    for &(offset, position) in points {
        path.evaluated(id, base + u64::from(offset), adm_position(position), out);
    }
    path.unit_ended(id, base + samples as u64, out);
}

/// A run followed through the evaluated positions: the last position stated
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
    paths: Vec<ObjectPath>,
    /// Events not written yet, until nothing still pending can come before
    /// them.
    moves: Vec<Move>,
    /// Interleaving scratch, reused across units.
    interleaved: Vec<i32>,
    warned_layout: bool,
    /// A sequence opened after the first: its first unit puts every object
    /// where it evaluates it, the paths of the sequence before closed.
    resync: bool,
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
            paths: Vec::new(),
            moves: Vec::new(),
            interleaved: Vec::new(),
            warned_layout: false,
            resync: false,
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
        self.paths = positions
            .iter()
            .map(|&position| ObjectPath::new(position))
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

    /// Follow each object's path over the unit, and write the events
    /// nothing still pending can come before.
    fn track_objects(&mut self, unit: &Unit<'_>) -> Result<()> {
        let base = self.decoded_samples;
        let resync = std::mem::take(&mut self.resync);
        for (rank, path) in self.paths.iter_mut().enumerate() {
            let id = 10 + rank as u32;
            // A sequence with fewer objects than the file: the missing ones
            // stay where they were.
            if let Some(object) = unit.objects.get(rank) {
                if resync {
                    // A new sequence: its moves start from where it puts
                    // the object, which the one before did not state.
                    path.finish(id, &mut self.moves);
                    if let Some(&(_, position)) = object.positions.first() {
                        path.jump(id, base, adm_position(position), &mut self.moves);
                    }
                }
                follow_unit(path, id, base, object, unit.samples, &mut self.moves);
            }
        }
        let pending = self.paths.iter().filter_map(ObjectPath::pending_from).min();
        self.write_moves(pending)
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

    /// Close the paths: every object ends where the stream left it.
    fn close_paths(&mut self) {
        for (rank, path) in self.paths.iter_mut().enumerate() {
            path.finish(10 + rank as u32, &mut self.moves);
        }
    }

    pub fn finalize(&mut self) -> Result<()> {
        if self.layout.is_none() {
            bail!("no IA sequence header found in the input");
        }
        if !self.opened {
            bail!("the IA sequence has no temporal unit that decodes");
        }
        self.close_paths();
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
        self.resync = self.layout.is_some();
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
    use iamf_dec::position::ObjectPosition;

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

    // The subblocks, as the decoder reports them.

    /// A coordinate as a 16-bit cartesian coding rounds it.
    fn q16(v: f64) -> f32 {
        ((v * 32767.0).round() / 32767.0) as f32
    }

    fn cart(p: [f64; 3]) -> ObjectPosition {
        ObjectPosition::Cartesian {
            x: q16(p[0]),
            y: q16(p[1]),
            z: q16(p[2]),
        }
    }

    fn polar(azimuth: f32) -> ObjectPosition {
        ObjectPosition::Polar {
            azimuth,
            elevation: 0.0,
            distance: 1.0,
        }
    }

    fn subblock(
        offset: u32,
        duration: u32,
        animation: PositionAnimationType,
        from: ObjectPosition,
        to: ObjectPosition,
    ) -> PositionMove {
        PositionMove {
            offset,
            duration,
            animation,
            from,
            to,
        }
    }

    fn step(offset: u32, duration: u32, at: ObjectPosition) -> PositionMove {
        subblock(offset, duration, PositionAnimationType::Step, at, at)
    }

    fn line(offset: u32, duration: u32, from: ObjectPosition, to: ObjectPosition) -> PositionMove {
        subblock(offset, duration, PositionAnimationType::Linear, from, to)
    }

    /// One unit of a stream: its length, the subblocks starting in it and
    /// the positions evaluated in it.
    struct Fed {
        samples: usize,
        moves: Vec<PositionMove>,
        positions: Vec<(u32, ObjectPosition)>,
    }

    fn unit(samples: usize, moves: Vec<PositionMove>) -> Fed {
        Fed {
            samples,
            moves,
            positions: Vec::new(),
        }
    }

    /// The events of one object starting at `start`, fed `units` in the
    /// given coding, then the stream's end.
    fn events_of(start: ObjectPosition, kind: PositionKind, units: &[Fed]) -> Vec<Move> {
        let mut out = Vec::new();
        let mut path = ObjectPath::new(adm_position(start));
        let mut base = 0;
        for fed in units {
            let object = DecodedObject {
                audio_element_id: 300,
                index: 0,
                samples: Vec::new(),
                positions: fed.positions.clone(),
                position_kind: kind,
                moves: fed.moves.clone(),
            };
            follow_unit(&mut path, 10, base, &object, fed.samples, &mut out);
            base += fed.samples as u64;
        }
        path.finish(10, &mut out);
        out.sort_by_key(|m| m.sample_pos);
        out
    }

    /// A master's ramp, cut by the encoder at a unit boundary into two
    /// subblocks that meet at a rounded joint, is one event again: the
    /// master's sample, ramp and end.
    #[test]
    fn a_ramp_cut_at_a_unit_boundary_is_one_event() {
        let a = [-1.0, 1.0, 0.0];
        let b = [1.0, 0.5, 0.0];
        let joint = [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * 460.0 / 1519.0);
        let units = [
            unit(
                960,
                vec![step(0, 500, cart(a)), line(500, 460, cart(a), cart(joint))],
            ),
            unit(
                960,
                vec![line(0, 1059, cart(joint), cart(b)), step(1059, 17, cart(b))],
            ),
            unit(960, vec![step(116, 4096, cart(b))]),
        ];
        let moves = events_of(cart(a), PositionKind::Cart16, &units);
        assert_eq!(
            moves,
            [Move {
                sample_pos: 500,
                id: 10,
                pos: adm_position(cart(b)),
                ramp: 1519
            }]
        );
    }

    /// Two of a master's ramps on one line at one speed, meeting inside a
    /// unit, are the master's two events: only a piece starting at a unit
    /// boundary, where the encoder cuts, lengthens the ramp before it.
    #[test]
    fn ramps_meeting_inside_a_unit_stay_two_events() {
        let a = [-1.0, 1.0, 0.0];
        let m = [0.0, 1.0, 0.0];
        let b = [1.0, 1.0, 0.0];
        let units = [unit(
            4096,
            vec![
                line(100, 1536, cart(a), cart(m)),
                line(1636, 1536, cart(m), cart(b)),
            ],
        )];
        let moves = events_of(cart(a), PositionKind::Cart16, &units);
        assert_eq!(moves.len(), 2, "{moves:?}");
        assert_eq!((moves[0].sample_pos, moves[0].ramp), (100, 1536));
        assert_eq!((moves[1].sample_pos, moves[1].ramp), (1636, 1536));
    }

    /// A line that turns at the joint is two events.
    #[test]
    fn a_corner_is_two_events() {
        let a = [0.0, 0.0, 0.0];
        let m = [0.2, 0.0, 0.0];
        let c = [0.2, 0.2, 0.0];
        let units = [
            unit(512, vec![line(0, 512, cart(a), cart(m))]),
            unit(1536, vec![line(0, 512, cart(m), cart(c))]),
        ];
        let moves = events_of(cart(a), PositionKind::Cart16, &units);
        assert_eq!(moves.len(), 2, "{moves:?}");
        assert_eq!((moves[0].sample_pos, moves[0].ramp), (0, 512));
        assert_eq!(moves[0].pos, adm_position(cart(m)));
        assert_eq!((moves[1].sample_pos, moves[1].ramp), (512, 512));
        assert_eq!(moves[1].pos, adm_position(cart(c)));
    }

    /// A step is a jump where it lands, and none where the object already
    /// is; a line to where the object is states nothing.
    #[test]
    fn a_step_is_a_jump() {
        let a = [0.0, 1.0, 0.0];
        let b = [1.0, 0.0, 0.0];
        let units = [
            unit(1024, vec![step(0, 1024, cart(a))]),
            unit(
                1024,
                vec![step(0, 512, cart(b)), line(512, 512, cart(b), cart(b))],
            ),
        ];
        let moves = events_of(cart(a), PositionKind::Cart16, &units);
        assert_eq!(
            moves,
            [Move {
                sample_pos: 1024,
                id: 10,
                pos: adm_position(cart(b)),
                ramp: 0
            }]
        );
    }

    /// A subblock starting away from where the object is jumps there first.
    #[test]
    fn a_subblock_starting_elsewhere_jumps_there_first() {
        let a = [0.0, 1.0, 0.0];
        let b = [-1.0, 0.0, 0.0];
        let c = [1.0, 0.0, 0.0];
        let units = [unit(2048, vec![line(100, 1000, cart(b), cart(c))])];
        let moves = events_of(cart(a), PositionKind::Cart16, &units);
        assert_eq!(
            moves,
            [
                Move {
                    sample_pos: 100,
                    id: 10,
                    pos: adm_position(cart(b)),
                    ramp: 0
                },
                Move {
                    sample_pos: 100,
                    id: 10,
                    pos: adm_position(cart(c)),
                    ramp: 1000
                },
            ]
        );
    }

    /// A ramp still open at a unit's end waits for the next unit, which may
    /// continue it; one that ended before the unit's end is written.
    #[test]
    fn an_open_ramp_holds_the_events_after_it() {
        let a = [0.0, 0.0, 0.0];
        let b = [1.0, 0.0, 0.0];
        let mut out = Vec::new();
        let mut path = ObjectPath::new(adm_position(cart(a)));
        let object = DecodedObject {
            audio_element_id: 300,
            index: 0,
            samples: Vec::new(),
            positions: Vec::new(),
            position_kind: PositionKind::Cart16,
            moves: vec![line(0, 1024, cart(a), cart(b))],
        };
        follow_unit(&mut path, 10, 0, &object, 1024, &mut out);
        assert_eq!(path.pending_from(), Some(0));
        assert!(out.is_empty());
        let object = DecodedObject {
            moves: vec![step(0, 1024, cart(b))],
            ..object
        };
        follow_unit(&mut path, 10, 1024, &object, 1024, &mut out);
        assert_eq!(path.pending_from(), None);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].sample_pos, out[0].ramp), (0, 1024));
    }

    /// A curve is followed through the evaluated positions, and ends
    /// exactly where the subblock says.
    #[test]
    fn a_curve_is_followed_through_the_evaluated_positions() {
        let a = [0.0, 0.0, 0.0];
        let b = [1.0, 1.0, 0.0];
        // Evaluated along a parabola: not one straight ramp.
        let positions: Vec<(u32, ObjectPosition)> = (1..4)
            .map(|k| {
                let t = k as f64 / 4.0;
                (k * 256, cart([t, t * t, 0.0]))
            })
            .collect();
        let units = [
            Fed {
                samples: 1024,
                moves: vec![subblock(
                    0,
                    1024,
                    PositionAnimationType::Bezier,
                    cart(a),
                    cart(b),
                )],
                positions,
            },
            unit(1024, vec![step(0, 1024, cart(b))]),
        ];
        let moves = events_of(cart(a), PositionKind::Cart16, &units);
        assert!(moves.len() >= 2, "{moves:?}");
        let last = moves.last().unwrap();
        assert_eq!(last.pos, adm_position(cart(b)));
        assert_eq!(last.sample_pos + u64::from(last.ramp), 1024);
    }

    /// A polar line runs along an arc: followed through the evaluated
    /// positions too, to its exact end.
    #[test]
    fn a_polar_line_is_followed_as_an_arc() {
        let positions: Vec<(u32, ObjectPosition)> = (1..4)
            .map(|k| (k * 256, polar(90.0 * k as f32 / 4.0)))
            .collect();
        let units = [
            Fed {
                samples: 1024,
                moves: vec![line(0, 1024, polar(0.0), polar(90.0))],
                positions,
            },
            unit(1024, vec![step(0, 1024, polar(90.0))]),
        ];
        let moves = events_of(polar(0.0), PositionKind::Polar, &units);
        assert!(moves.len() >= 2, "{moves:?}");
        let last = moves.last().unwrap();
        assert_eq!(last.pos, adm_position(polar(90.0)));
        assert_eq!(last.sample_pos + u64::from(last.ramp), 1024);
    }
}
