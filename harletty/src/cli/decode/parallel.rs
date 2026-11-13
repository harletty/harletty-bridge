//! TrueHD decoded on several threads, one run of access units at a time.
//!
//! A TrueHD stream restarts every major sync (every 128 access units, about
//! 107 ms, on the streams we have): each substream's restart header resets
//! the prediction filters, the matrices and the dither, so a decoder that
//! starts at a major sync produces the same samples as one that ran through.
//! That makes the stream cut into runs of major-sync intervals that decode
//! independently:
//!
//! - this thread reads the input, frames it and cuts it at major syncs into
//!   runs of [`CHUNK_INTERVALS`] intervals;
//! - workers decode the runs, each with a parser and decoder of its own;
//! - a collector puts the access units back in stream order and does what
//!   depends on the whole history: the substream-info change flag, the
//!   silence standing in for an access unit that did not decode (sized from
//!   the last one that did), and the totals.
//!
//! Two checks at a major sync look back at the interval before it: the
//! restart header's lossless check covers the previous interval's samples,
//! and an access unit that repeats the previous one's output timing and
//! samples is a seamless-branch duplicate the writer drops. A worker
//! therefore first decodes the run's preceding interval with its log muted
//! and its output discarded, and reaches the run in the state a decoder that
//! never stopped would be in. That replay costs one interval per run.
//!
//! A run also holds at most [`MAX_RUN_FRAMES`] access units, so that a
//! stream whose major syncs stop coming (lost to damage, or never sent) does
//! not pile up into one run until the end of the input. A run that fills up
//! is cut where it stands, inside an interval if need be, and the run after it
//! takes over the parser and decoder the cut run ends with instead of
//! replaying: such a stretch decodes one run after the other, as on the
//! single-threaded path, but in bounded memory and with output flowing.
//!
//! The output matches the single-threaded path byte for byte, and so does
//! the log of a clean stream. On a damaged one, a warning that counts access
//! units since an event long past (the parser's late-DRC-update warning,
//! repeated for every access unit once the count is over) restarts its count
//! at each run's replay and comes out fewer times; the samples do not depend
//! on it. `--strict` keeps the single-threaded path, since it stops at the
//! first error and a parallel run would already have logged errors past it.

use super::processor::AccessUnitShape;
use crate::input::InputReader;
use crate::log_mute::Mute;
use anyhow::{Result, anyhow};
use indicatif::ProgressBar;
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use truehd::process::MAX_PRESENTATIONS;
use truehd::process::decode::{DecodedAccessUnit, Decoder};
use truehd::process::extract::{Extractor, Frame};
use truehd::process::parse::Parser;
use truehd::structs::access_unit::AccessUnit;
use truehd::structs::channel::ChannelLabel;
use truehd::structs::oamd::ObjectAudioMetadataPayload;
use truehd::utils::errors::ExtractError;

/// The log targets of the single-threaded path, kept so that its messages read
/// (and filter) the same whichever path decoded the stream.
const PROCESSOR: &str = "harletty::cli::decode::processor";
const DECODER_THREAD: &str = "harletty::cli::decode::decoder_thread";

/// Major-sync intervals per run handed to a worker. Each run also replays
/// the interval before it, so the replay costs 1/16 of the decode; a run is
/// about 1.7 s of audio, and a feature film gives a few thousand of them.
const CHUNK_INTERVALS: usize = 16;

/// Access units a run holds at most: twice a run of [`CHUNK_INTERVALS`]
/// intervals of 128 access units, the interval of the streams we have.
const MAX_RUN_FRAMES: usize = 2 * CHUNK_INTERVALS * 128;

/// Where the reader cuts runs.
#[derive(Clone, Copy)]
struct RunSize {
    /// Major-sync intervals per run.
    intervals: usize,
    /// Access units per run at most.
    frames: usize,
}

impl RunSize {
    /// [`CHUNK_INTERVALS`] and [`MAX_RUN_FRAMES`], or
    /// `HARLETTY_TRUEHD_RUN_INTERVALS` and `HARLETTY_TRUEHD_RUN_FRAMES` when
    /// set: verification knobs, so that a short stream crosses many run
    /// boundaries, or is cut inside its intervals.
    fn from_env() -> Self {
        fn knob(name: &str, default: usize) -> usize {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&n| n > 0)
                .unwrap_or(default)
        }
        Self {
            intervals: knob("HARLETTY_TRUEHD_RUN_INTERVALS", CHUNK_INTERVALS),
            frames: knob("HARLETTY_TRUEHD_RUN_FRAMES", MAX_RUN_FRAMES),
        }
    }
}

pub struct ParallelDecoderConfig {
    pub input_path: PathBuf,
    pub presentation: u8,
    pub threads: usize,
    pub fail_level: log::Level,
    pub required_presentations: [bool; MAX_PRESENTATIONS],
    /// Bounded: decoding now outruns the writer, and an access unit is 10 KiB.
    pub tx: mpsc::SyncSender<Result<Box<DecodedAccessUnit>>>,
    pub spare: mpsc::Receiver<Box<DecodedAccessUnit>>,
    pub pb_clone: Option<ProgressBar>,
    pub prefix: Vec<u8>,
}

/// A run of access units and what became of them, handed round in a circle:
/// the reader fills `warmup` and `frames`, a worker `outcomes` and `rows`,
/// and the collector, once the run is written out, empties it and hands it
/// back to the reader as the ticket for the next run. Its buffers keep their
/// capacity on the way, so past the first runs in flight a stream decodes
/// without allocating any of them again.
#[derive(Default)]
struct Run {
    seq: u64,
    /// The interval before the run, decoded first and thrown away. Frames
    /// carry their 1-based position in the stream (the frame number the log
    /// messages carry).
    warmup: Vec<(u64, Frame)>,
    frames: Vec<(u64, Frame)>,
    /// One per access unit of `frames`.
    outcomes: Vec<Outcome>,
    /// The rows of every decoded access unit of the run, back to back.
    rows: Vec<[i32; 16]>,
    /// Where the run's parser and decoder come from when it does not start
    /// afresh: the run before it, which ended where the interval before
    /// this run's start cannot be replayed. `warmup` is then empty.
    carry_in: Option<mpsc::Receiver<Box<Carry>>>,
    /// Where the run hands its parser and decoder on, when the run after it
    /// takes them over.
    carry_out: Option<mpsc::SyncSender<Box<Carry>>>,
    /// Set when the worker panicked on this run.
    panic: Option<String>,
}

impl Run {
    /// Empties the run for the next, keeping its buffers.
    fn clear(&mut self) {
        self.warmup.clear();
        self.frames.clear();
        self.outcomes.clear();
        self.rows.clear();
        self.carry_in = None;
        self.carry_out = None;
        self.panic = None;
    }
}

/// The decoding state one run hands to the next.
struct Carry {
    parser: Parser,
    decoder: Decoder,
    recovering_until_major_sync: bool,
}

/// What became of one access unit, for the collector to act on in order.
enum Outcome {
    /// Decoded; its samples are the next `sample_length` rows of the run's.
    Decoded {
        major_sync: Option<(u8, u8)>,
        head: UnitHead,
    },
    /// Parsed but not decoded: a gap, though its major sync still counts for
    /// the substream-info tracking, as on the single-threaded path.
    DecodeFailed { major_sync: Option<(u8, u8)> },
    /// Not parsed: a gap, and the parser and decoder wait for a major sync.
    ParseFailed,
    /// Skipped while waiting for that major sync: a gap.
    Skipped,
}

/// Everything of a [`DecodedAccessUnit`] but its sample rows.
struct UnitHead {
    sampling_frequency: u32,
    sample_length: usize,
    channel_count: usize,
    /// `None`: the same as the previous decoded access unit of the run.
    channel_labels: Option<Vec<ChannelLabel>>,
    oamd: Vec<ObjectAudioMetadataPayload>,
    is_duplicate: bool,
    overlaps_previous: bool,
    substream_info_changed: bool,
}

/// Hands the input to its callback chunk by chunk, until the callback says stop.
type Feed<'a> = &'a mut dyn FnMut(&[u8]) -> Result<bool>;

/// What the collector hands back once the stream is through.
#[derive(Default)]
struct Totals {
    total_samples: u64,
    gap_access_units: u64,
    gap_samples: u64,
    sample_rate: Option<u32>,
}

pub fn spawn_parallel_decoder_thread(
    config: ParallelDecoderConfig,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || run(config))
}

fn run(config: ParallelDecoderConfig) -> Result<()> {
    let ParallelDecoderConfig {
        input_path,
        presentation,
        threads,
        fail_level,
        required_presentations,
        tx,
        spare,
        pb_clone,
        prefix,
    } = config;
    let setup = WorkerSetup {
        presentation: presentation as usize,
        fail_level,
        required_presentations,
    };
    run_on(
        &setup,
        threads,
        RunSize::from_env(),
        tx,
        spare,
        &pb_clone,
        move |push: Feed| {
            // What the probe read off a pipe comes first.
            if !prefix.is_empty() && !push(&prefix)? {
                return Ok(());
            }
            InputReader::new(&input_path)?.process_chunks(64 * 1024, push)
        },
    )
}

/// The decode itself, over whatever `feed` reads.
fn run_on(
    setup: &WorkerSetup,
    threads: usize,
    run_size: RunSize,
    tx: mpsc::SyncSender<Result<Box<DecodedAccessUnit>>>,
    spare: mpsc::Receiver<Box<DecodedAccessUnit>>,
    pb: &Option<ProgressBar>,
    feed: impl FnOnce(Feed) -> Result<()>,
) -> Result<()> {
    let threads = threads.max(1);
    // Runs read ahead of the one being written out: enough to keep every
    // worker busy while the collector waits on the oldest.
    let in_flight = threads + 2;

    let (job_tx, job_rx) = mpsc::channel::<Run>();
    let job_rx = Mutex::new(job_rx);
    let (done_tx, done_rx) = mpsc::channel::<Run>();
    // The runs in flight, as tickets: never more of them than the channel holds.
    let (ticket_tx, ticket_rx) = mpsc::sync_channel::<Run>(in_flight);
    for _ in 0..in_flight {
        ticket_tx
            .send(Run::default())
            .expect("the receiver is held here");
    }

    thread::scope(|scope| -> Result<()> {
        for _ in 0..threads {
            let done_tx = done_tx.clone();
            let job_rx = &job_rx;
            scope.spawn(move || worker_loop(job_rx, done_tx, setup));
        }
        drop(done_tx);

        let collector = scope.spawn(move || collect(done_rx, ticket_tx, tx, spare));

        let read = read_and_dispatch(feed, run_size, pb, job_tx, ticket_rx);
        let totals = collector
            .join()
            .map_err(|_| anyhow!("TrueHD collector thread panicked"))??;
        let frame_count = read?;

        log::info!(
            target: DECODER_THREAD,
            "Processing complete: {frame_count} frames, {} samples",
            totals.total_samples
        );
        if totals.gap_access_units > 0 {
            let sample_rate = totals.sample_rate.unwrap_or(48000).max(1) as f64;
            log::warn!(
                target: DECODER_THREAD,
                "{} access units could not be decoded and were replaced with \
                 silence ({} samples, {:.0} ms). Output length is preserved; \
                 audio stays aligned with the source timeline.",
                totals.gap_access_units,
                totals.gap_samples,
                totals.gap_samples as f64 / sample_rate * 1000.0
            );
        }
        Ok(())
    })
}

/// Reads the input, cuts it into runs and hands them out; returns the number
/// of access units framed. Stops early, without an error, once the collector
/// has stopped (the writer is gone or an error is on its way to it).
fn read_and_dispatch(
    feed: impl FnOnce(Feed) -> Result<()>,
    run_size: RunSize,
    pb: &Option<ProgressBar>,
    job_tx: mpsc::Sender<Run>,
    ticket_rx: mpsc::Receiver<Run>,
) -> Result<u64> {
    let mut extractor = Extractor::default();
    let mut chunker = Chunker::new(job_tx, ticket_rx, run_size);
    let mut frame_count = 0u64;

    feed(&mut |chunk: &[u8]| {
        extractor.push_bytes(chunk);
        loop {
            match extractor.next() {
                Some(Ok(frame)) => {
                    frame_count += 1;
                    if let Some(pb) = pb {
                        pb.set_position(frame_count);
                    }
                    chunker.push(frame_count, frame);
                    if chunker.stopped {
                        return Ok(false);
                    }
                }
                Some(Err(ExtractError::InsufficientData)) | None => break,
                Some(Err(_)) => {
                    if let Some(pb) = pb {
                        pb.set_message("processing (some extraction errors)");
                    }
                }
            }
        }
        Ok(true)
    })?;
    chunker.dispatch(true);
    Ok(frame_count)
}

struct Chunker {
    job_tx: mpsc::Sender<Run>,
    ticket_rx: mpsc::Receiver<Run>,
    run_size: RunSize,
    seq: u64,
    current: Vec<(u64, Frame)>,
    /// Major syncs in `current`.
    intervals: usize,
    /// Where the last major sync of `current` is.
    last_major_sync: Option<usize>,
    /// The interval the next run replays first.
    warmup: Vec<(u64, Frame)>,
    /// Where the next run takes its parser and decoder from, when it cannot
    /// replay the interval before it.
    carry: Option<mpsc::Receiver<Box<Carry>>>,
    stopped: bool,
}

impl Chunker {
    fn new(job_tx: mpsc::Sender<Run>, ticket_rx: mpsc::Receiver<Run>, run_size: RunSize) -> Self {
        Self {
            job_tx,
            ticket_rx,
            run_size,
            seq: 0,
            current: Vec::new(),
            intervals: 0,
            last_major_sync: None,
            warmup: Vec::new(),
            carry: None,
            stopped: false,
        }
    }

    fn push(&mut self, number: u64, frame: Frame) {
        let full = self.current.len() >= self.run_size.frames;
        if frame.is_major_sync() {
            if self.intervals >= self.run_size.intervals || full {
                self.dispatch(true);
            }
            self.intervals += 1;
            self.last_major_sync = Some(self.current.len());
        } else if full {
            // No major sync to cut at: cut inside the interval.
            self.dispatch(false);
        }
        self.current.push((number, frame));
    }

    /// Hands `current` out as a run; `at_major_sync` when the run after it
    /// starts at one (or there is none).
    fn dispatch(&mut self, at_major_sync: bool) {
        if self.current.is_empty() || self.stopped {
            return;
        }
        // A ticket per run in flight, an emptied run whose buffers this one
        // fills; none left once the collector is gone.
        let Ok(mut run) = self.ticket_rx.recv() else {
            self.stopped = true;
            return;
        };
        run.seq = self.seq;
        self.seq += 1;
        // The run takes the frames read, and the next run's frames go in the
        // buffer it came with; likewise the interval it replays.
        std::mem::swap(&mut run.frames, &mut self.current);
        std::mem::swap(&mut run.warmup, &mut self.warmup);
        run.carry_in = self.carry.take();
        match self.last_major_sync {
            // The next run starts at a major sync and this run holds the
            // whole interval before it: the next run replays it.
            Some(at) if at_major_sync => {
                // Frames are reference-counted: the copy is of the handles.
                self.warmup.extend_from_slice(&run.frames[at..]);
            }
            // The next run starts inside an interval, or after one that
            // began in an earlier run: it takes this run's state over.
            _ => {
                let (carry_out, carry_in) = mpsc::sync_channel(1);
                run.carry_out = Some(carry_out);
                self.carry = Some(carry_in);
            }
        }
        self.intervals = 0;
        self.last_major_sync = None;

        if self.job_tx.send(run).is_err() {
            self.stopped = true;
        }
    }
}

struct WorkerSetup {
    presentation: usize,
    fail_level: log::Level,
    required_presentations: [bool; MAX_PRESENTATIONS],
}

fn worker_loop(
    job_rx: &Mutex<mpsc::Receiver<Run>>,
    done_tx: mpsc::Sender<Run>,
    setup: &WorkerSetup,
) {
    // One result buffer for the worker's whole life: its rows are copied out.
    let mut decoded = Box::<DecodedAccessUnit>::default();
    let mut access_unit = AccessUnit::default();
    loop {
        let run = {
            let rx = job_rx.lock().unwrap_or_else(|e| e.into_inner());
            rx.recv()
        };
        let Ok(mut run) = run else { return };
        let carry_out = run.carry_out.take();
        let hand_on = carry_out.is_some();
        let decoded_run = catch_unwind(AssertUnwindSafe(|| {
            decode_run(&mut run, setup, &mut access_unit, &mut decoded, hand_on)
        }));
        match decoded_run {
            // The next run may be waiting for it on another worker. A run
            // that did not finish drops `carry_out`, which that run sees.
            Ok(carry) => {
                if let (Some(carry_out), Some(carry)) = (carry_out, carry) {
                    let _ = carry_out.send(carry);
                }
            }
            Err(panic) => {
                run.panic = Some(
                    panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string()),
                );
                // State left half-updated by the panic is not reused.
                access_unit = AccessUnit::default();
                *decoded = DecodedAccessUnit::default();
            }
        }
        if done_tx.send(run).is_err() {
            return;
        }
    }
}

/// The per-access-unit half of [`super::processor::process_frames`]: what a
/// worker can do without knowing what came before its run.
struct RunDecoder<'a> {
    parser: Parser,
    decoder: Decoder,
    access_unit: &'a mut AccessUnit,
    decoded: &'a mut DecodedAccessUnit,
    presentation: usize,
    recovering_until_major_sync: bool,
    last_labels: Option<Vec<ChannelLabel>>,
}

impl RunDecoder<'_> {
    /// Decodes one access unit; `None` when replaying (nothing is kept).
    fn step(
        &mut self,
        number: u64,
        frame: &Frame,
        keep: Option<(&mut Vec<Outcome>, &mut Vec<[i32; 16]>)>,
    ) {
        if self.recovering_until_major_sync {
            if !frame.is_major_sync() {
                if let Some((outcomes, _)) = keep {
                    outcomes.push(Outcome::Skipped);
                }
                return;
            }
            log::info!(target: PROCESSOR, "Major sync found at frame {number}; resuming after parse recovery");
            self.recovering_until_major_sync = false;
        }

        match self.parser.parse_into(frame, self.access_unit) {
            Ok(()) => {
                let major_sync = self
                    .access_unit
                    .major_sync_info
                    .as_ref()
                    .map(|m| (m.substream_info, m.extended_substream_info));
                match self.decoder.decode_presentation_into(
                    self.access_unit,
                    self.presentation,
                    self.decoded,
                ) {
                    Ok(()) => {
                        let Some((outcomes, rows)) = keep else { return };
                        let d = &mut *self.decoded;
                        let length = d.sample_length.min(d.pcm_data.len());
                        rows.extend_from_slice(&d.pcm_data[..length]);
                        let channel_labels = if self.last_labels.as_ref() == Some(&d.channel_labels)
                        {
                            None
                        } else {
                            self.last_labels = Some(d.channel_labels.clone());
                            Some(d.channel_labels.clone())
                        };
                        outcomes.push(Outcome::Decoded {
                            major_sync,
                            head: UnitHead {
                                sampling_frequency: d.sampling_frequency,
                                sample_length: d.sample_length,
                                channel_count: d.channel_count,
                                channel_labels,
                                oamd: std::mem::take(&mut d.oamd),
                                is_duplicate: d.is_duplicate,
                                overlaps_previous: d.overlaps_previous,
                                substream_info_changed: d.substream_info_changed,
                            },
                        });
                    }
                    Err(e) => {
                        log::error!(target: PROCESSOR, "Decode error at frame {number}: {e}");
                        if let Some((outcomes, _)) = keep {
                            outcomes.push(Outcome::DecodeFailed { major_sync });
                        }
                    }
                }
            }
            Err(e) => {
                log::error!(target: PROCESSOR, "Parse error at frame {number}: {e}");
                self.parser.reset_for_next_major_sync();
                self.decoder.reset_for_next_major_sync();
                self.recovering_until_major_sync = true;
                if let Some((outcomes, _)) = keep {
                    outcomes.push(Outcome::ParseFailed);
                }
            }
        }
    }
}

/// Decodes `run.frames` into `run.outcomes` and `run.rows`, after replaying
/// `run.warmup` or taking over the state the run before ended with; returns
/// the state this run ends with when `hand_on`, `None` when it could not
/// start.
fn decode_run(
    run: &mut Run,
    setup: &WorkerSetup,
    access_unit: &mut AccessUnit,
    decoded: &mut DecodedAccessUnit,
    hand_on: bool,
) -> Option<Box<Carry>> {
    let carry = match run.carry_in.take() {
        // The run before it is being decoded on another worker, or is done.
        Some(carry_in) => match carry_in.recv() {
            Ok(carry) => Some(carry),
            Err(_) => {
                run.panic = Some("the run before it did not finish".to_string());
                return None;
            }
        },
        None => None,
    };
    let Run {
        warmup,
        frames,
        outcomes,
        rows,
        ..
    } = run;
    let (parser, decoder, recovering_until_major_sync) = match carry {
        Some(carry) => {
            let Carry {
                parser,
                decoder,
                recovering_until_major_sync,
            } = *carry;
            (parser, decoder, recovering_until_major_sync)
        }
        // A parser and decoder as the single-threaded path sets them up:
        // state of fixed size, rebuilt for a run that starts afresh.
        None => {
            let mut parser = Parser::default();
            parser.set_fail_level(setup.fail_level);
            parser.set_required_presentations(&setup.required_presentations);
            let mut decoder = Decoder::default();
            decoder.set_fail_level(setup.fail_level);
            (parser, decoder, false)
        }
    };

    let mut decoder = RunDecoder {
        parser,
        decoder,
        access_unit,
        decoded,
        presentation: setup.presentation,
        recovering_until_major_sync,
        last_labels: None,
    };

    if !warmup.is_empty() {
        let _mute = Mute::new();
        for (number, frame) in warmup.iter() {
            decoder.step(*number, frame, None);
        }
    }

    outcomes.reserve(frames.len());
    rows.reserve(frames.len() * 40);
    for (number, frame) in frames.iter() {
        decoder.step(*number, frame, Some((outcomes, rows)));
    }

    hand_on.then(|| {
        Box::new(Carry {
            parser: decoder.parser,
            decoder: decoder.decoder,
            recovering_until_major_sync: decoder.recovering_until_major_sync,
        })
    })
}

/// Puts the runs back in order and sends their access units to the writer,
/// doing what the single-threaded path does across access units.
fn collect(
    done_rx: mpsc::Receiver<Run>,
    ticket_tx: mpsc::SyncSender<Run>,
    tx: mpsc::SyncSender<Result<Box<DecodedAccessUnit>>>,
    spare: mpsc::Receiver<Box<DecodedAccessUnit>>,
) -> Result<Totals> {
    let mut pending = BTreeMap::new();
    let mut next = 0u64;
    let mut state = CollectState {
        tx,
        spare,
        totals: Totals::default(),
        last_shape: None,
        current_substream_info: None,
        current_extended_substream_info: None,
        labels: Vec::new(),
    };

    while let Ok(run) = done_rx.recv() {
        pending.insert(run.seq, run);
        while let Some(mut run) = pending.remove(&next) {
            next += 1;
            if let Some(panic) = run.panic.take() {
                return Err(anyhow!("TrueHD decoder thread panicked: {panic}"));
            }
            if !state.forward(&mut run.outcomes, &run.rows) {
                // The writer is gone; dropping the tickets stops the reader.
                return Ok(state.totals);
            }
            run.clear();
            let _ = ticket_tx.send(run);
        }
    }
    if !pending.is_empty() {
        return Err(anyhow!(
            "TrueHD decoder threads stopped with runs undelivered"
        ));
    }
    Ok(state.totals)
}

struct CollectState {
    tx: mpsc::SyncSender<Result<Box<DecodedAccessUnit>>>,
    spare: mpsc::Receiver<Box<DecodedAccessUnit>>,
    totals: Totals,
    last_shape: Option<AccessUnitShape>,
    current_substream_info: Option<u8>,
    current_extended_substream_info: Option<u8>,
    /// The channel labels of the last decoded access unit.
    labels: Vec<ChannelLabel>,
}

impl CollectState {
    /// Sends a run's access units on; `false` once the writer is gone.
    fn forward(&mut self, outcomes: &mut Vec<Outcome>, rows: &[[i32; 16]]) -> bool {
        let mut row = 0;
        for outcome in outcomes.drain(..) {
            match outcome {
                Outcome::Decoded { major_sync, head } => {
                    let changed = self.track(major_sync);
                    let mut unit = self.spare.try_recv().unwrap_or_default();
                    let length = head.sample_length.min(unit.pcm_data.len());
                    unit.pcm_data[..length].copy_from_slice(&rows[row..row + length]);
                    row += length;
                    if let Some(labels) = head.channel_labels {
                        self.labels = labels;
                    }
                    unit.channel_labels.clone_from(&self.labels);
                    unit.sampling_frequency = head.sampling_frequency;
                    unit.sample_length = head.sample_length;
                    unit.channel_count = head.channel_count;
                    unit.oamd = head.oamd;
                    unit.is_duplicate = head.is_duplicate;
                    unit.overlaps_previous = head.overlaps_previous;
                    unit.substream_info_changed = head.substream_info_changed || changed;

                    self.last_shape = Some(AccessUnitShape::of(&unit));
                    self.totals.sample_rate = Some(unit.sampling_frequency);
                    self.totals.total_samples += unit.sample_length as u64;
                    if self.tx.send(Ok(unit)).is_err() {
                        return false;
                    }
                }
                Outcome::DecodeFailed { major_sync } => {
                    self.track(major_sync);
                    if !self.gap() {
                        return false;
                    }
                }
                Outcome::ParseFailed => {
                    self.current_substream_info = None;
                    self.current_extended_substream_info = None;
                    if !self.gap() {
                        return false;
                    }
                }
                Outcome::Skipped => {
                    if !self.gap() {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// The substream-info change check of `process_frames`, with its messages.
    fn track(&mut self, major_sync: Option<(u8, u8)>) -> bool {
        let Some((substream_info, extended_substream_info)) = major_sync else {
            return false;
        };
        let mut changed = false;
        match self.current_substream_info {
            Some(current) if current != substream_info => {
                log::info!(
                    target: PROCESSOR,
                    "substream_info changed: {:#X} -> {:#X}",
                    current,
                    substream_info
                );
                changed = true;
            }
            _ => {}
        }
        match self.current_extended_substream_info {
            Some(current) if current != extended_substream_info => {
                log::info!(
                    target: PROCESSOR,
                    "extended_substream_info changed: {:#X} -> {:#X}",
                    current,
                    extended_substream_info
                );
                changed = true;
            }
            _ => {}
        }
        self.current_substream_info = Some(substream_info);
        self.current_extended_substream_info = Some(extended_substream_info);
        changed
    }

    /// Silence in place of an access unit that did not decode, as
    /// `emit_gap` sends it; `false` once the writer is gone.
    fn gap(&mut self) -> bool {
        let Some(shape) = self.last_shape.as_ref() else {
            return true;
        };
        let silence = shape.to_silence();
        self.totals.gap_access_units += 1;
        self.totals.gap_samples += silence.sample_length as u64;
        self.totals.total_samples += silence.sample_length as u64;
        self.tx.send(Ok(Box::new(silence))).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::super::processor::{ProcessFramesContext, process_frames};
    use super::*;
    use truehd::process::EXAMPLE_DATA;

    const PRESENTATION: usize = 3;

    fn setup() -> WorkerSetup {
        let mut required_presentations = [false; MAX_PRESENTATIONS];
        required_presentations[PRESENTATION] = true;
        WorkerSetup {
            presentation: PRESENTATION,
            fail_level: log::Level::Error,
            required_presentations,
        }
    }

    /// `EXAMPLE_DATA` is one major sync plus one continuation access unit:
    /// `copies` of it are as many major-sync intervals.
    fn stream(copies: usize) -> Vec<u8> {
        EXAMPLE_DATA.repeat(copies)
    }

    /// The access units of `bytes`, as the reader frames them.
    fn frames_of(bytes: &[u8]) -> Vec<Frame> {
        let mut extractor = Extractor::default();
        extractor.push_bytes(bytes);
        let mut frames = Vec::new();
        loop {
            match extractor.next() {
                Some(Ok(frame)) => frames.push(frame),
                Some(Err(ExtractError::InsufficientData)) | None => break,
                Some(Err(_)) => {}
            }
        }
        frames
    }

    /// The continuation access unit of `EXAMPLE_DATA`, `count` times: a
    /// stretch with no major sync in it.
    fn continuations(count: usize) -> Vec<u8> {
        let frames = frames_of(EXAMPLE_DATA);
        let continuation = frames
            .iter()
            .find(|f| !f.is_major_sync())
            .expect("EXAMPLE_DATA holds a continuation access unit");
        continuation.data.repeat(count)
    }

    /// Continuation access units ahead of the first major sync, a short
    /// stretch, an interval far longer than a run, and more short ones.
    fn long_interval_stream() -> Vec<u8> {
        let mut bytes = continuations(5);
        bytes.extend(stream(3));
        bytes.extend(EXAMPLE_DATA);
        bytes.extend(continuations(300));
        bytes.extend(stream(4));
        bytes
    }

    /// [`long_interval_stream`] with a continuation access unit damaged early
    /// in the long interval: the decoders wait for a major sync from there,
    /// across every cut after it.
    fn damaged_long_interval_stream() -> Vec<u8> {
        let mut bytes = long_interval_stream();
        let continuation = continuations(1).len();
        let at = 5 * continuation + 4 * EXAMPLE_DATA.len() + 40 * continuation + 6;
        for byte in &mut bytes[at..at + 8] {
            *byte ^= 0xFF;
        }
        bytes
    }

    /// Scrambles 16 bytes of the major sync (`unit` 0) or the continuation
    /// access unit (1) of an interval, past the length field so framing holds.
    fn damage(bytes: &mut [u8], interval: usize, unit: usize) {
        let at = interval * EXAMPLE_DATA.len() + unit * (EXAMPLE_DATA.len() / 2) + 12;
        for byte in &mut bytes[at..at + 16] {
            *byte ^= 0xFF;
        }
    }

    /// The single-threaded path, set up as `cmd_decode` sets it up.
    fn serial(bytes: &[u8]) -> Vec<DecodedAccessUnit> {
        let (tx, rx) = mpsc::channel();
        let (_spare_tx, spare) = mpsc::channel();
        let setup = setup();
        let mut extractor = Extractor::default();
        let mut parser = Parser::default();
        parser.set_fail_level(setup.fail_level);
        parser.set_required_presentations(&setup.required_presentations);
        let mut decoder = Decoder::default();
        decoder.set_fail_level(setup.fail_level);
        let mut access_unit = AccessUnit::default();
        let (mut frames_processed, mut frame_count, mut total_samples) = (0, 0, 0);
        let (mut substream_info, mut extended_substream_info) = (None, None);
        let mut recovering = false;
        let mut last_shape = None;
        let (mut gap_access_units, mut gap_samples) = (0, 0);
        let mut ctx = ProcessFramesContext {
            extractor: &mut extractor,
            parser: &mut parser,
            access_unit: &mut access_unit,
            decoder: &mut decoder,
            frames_processed: &mut frames_processed,
            frame_count: &mut frame_count,
            total_samples: &mut total_samples,
            presentation: PRESENTATION as u8,
            strict_mode: false,
            tx: &tx,
            spare: &spare,
            pb_clone: &None,
            current_substream_info: &mut substream_info,
            current_extended_substream_info: &mut extended_substream_info,
            recovering_until_major_sync: &mut recovering,
            last_shape: &mut last_shape,
            gap_access_units: &mut gap_access_units,
            gap_samples: &mut gap_samples,
        };
        ctx.extractor.push_bytes(bytes);
        process_frames(&mut ctx).expect("processing must not abort in non-strict mode");
        drop(tx);
        rx.into_iter().map(|r| *r.expect("no error sent")).collect()
    }

    fn parallel(bytes: &[u8], threads: usize, run_intervals: usize) -> Vec<DecodedAccessUnit> {
        let run_size = RunSize {
            intervals: run_intervals,
            frames: MAX_RUN_FRAMES,
        };
        parallel_in(bytes, threads, run_size)
    }

    fn parallel_in(bytes: &[u8], threads: usize, run_size: RunSize) -> Vec<DecodedAccessUnit> {
        // Room for the whole stream: nothing reads it until the decode is over.
        let (tx, rx) = mpsc::sync_channel(1 << 12);
        let (_spare_tx, spare) = mpsc::channel();
        run_on(
            &setup(),
            threads,
            run_size,
            tx,
            spare,
            &None,
            |push: Feed| {
                // Pushed in uneven pieces, as a pipe would.
                for piece in bytes.chunks(97) {
                    if !push(piece)? {
                        break;
                    }
                }
                Ok(())
            },
        )
        .expect("the parallel path must not fail on a non-strict decode");
        rx.into_iter().map(|r| *r.expect("no error sent")).collect()
    }

    fn assert_same(parallel: &[DecodedAccessUnit], serial: &[DecodedAccessUnit], case: &str) {
        assert_eq!(parallel.len(), serial.len(), "{case}: access unit count");
        for (i, (p, s)) in parallel.iter().zip(serial).enumerate() {
            let at = format!("{case}: access unit {i}");
            assert_eq!(p.sampling_frequency, s.sampling_frequency, "{at}");
            assert_eq!(p.sample_length, s.sample_length, "{at}");
            assert_eq!(p.channel_count, s.channel_count, "{at}");
            assert_eq!(
                p.pcm_data[..p.sample_length],
                s.pcm_data[..s.sample_length],
                "{at}"
            );
            assert_eq!(p.channel_labels, s.channel_labels, "{at}");
            assert_eq!(format!("{:?}", p.oamd), format!("{:?}", s.oamd), "{at}");
            assert_eq!(p.is_duplicate, s.is_duplicate, "{at}");
            assert_eq!(p.overlaps_previous, s.overlaps_previous, "{at}");
            assert_eq!(p.substream_info_changed, s.substream_info_changed, "{at}");
        }
    }

    /// Whatever the runs and however many threads decode them, the access
    /// units come out as the single-threaded path sends them.
    #[test]
    fn a_clean_stream_decodes_the_same_on_any_number_of_threads() {
        let bytes = stream(40);
        let serial = serial(&bytes);
        assert_eq!(serial.len(), 80);
        for threads in [1, 2, 5] {
            for run_intervals in [1, 2, 3, 16] {
                let case = format!("{threads} threads, runs of {run_intervals}");
                assert_same(&parallel(&bytes, threads, run_intervals), &serial, &case);
            }
        }
    }

    /// Damage in a major sync and in continuation access units, some of it
    /// on the last interval before a run boundary: the recovery to the next
    /// major sync and the silence standing in for what did not decode come
    /// out as on the single-threaded path, wherever the runs are cut.
    #[test]
    fn a_damaged_stream_recovers_the_same_across_run_boundaries() {
        let mut bytes = stream(40);
        for (interval, unit) in [(3, 1), (7, 0), (8, 1), (15, 0), (24, 1), (31, 0)] {
            damage(&mut bytes, interval, unit);
        }
        let serial = serial(&bytes);
        let silent = serial
            .iter()
            .filter(|u| u.pcm_data[..u.sample_length].iter().all(|r| r == &[0; 16]))
            .count();
        assert!(
            silent > 0,
            "the damage no longer trips a failure, so this test is not \
             exercising recovery any more"
        );
        for threads in [2, 4] {
            for run_intervals in [1, 2, 4, 8] {
                let case = format!("{threads} threads, runs of {run_intervals}");
                assert_same(&parallel(&bytes, threads, run_intervals), &serial, &case);
            }
        }
    }

    /// Runs cut inside an interval, and runs after an interval that began in
    /// an earlier run, take over the state of the run before them: the
    /// stream decodes as on the single-threaded path wherever it is cut.
    #[test]
    fn a_long_interval_decodes_the_same_cut_into_bounded_runs() {
        let clean = long_interval_stream();
        let damaged = damaged_long_interval_stream();
        for (name, bytes) in [("clean", &clean), ("damaged", &damaged)] {
            let serial = serial(bytes);
            assert!(serial.len() > 300, "{name}: the long interval decodes");
            for threads in [1, 2, 4] {
                for intervals in [1, 2, 16] {
                    for frames in [1, 2, 7, 50, MAX_RUN_FRAMES] {
                        let case = format!(
                            "{name}, {threads} threads, runs of {intervals} intervals \
                             and at most {frames} access units"
                        );
                        let run_size = RunSize { intervals, frames };
                        assert_same(&parallel_in(bytes, threads, run_size), &serial, &case);
                    }
                }
            }
        }
    }

    /// What became of each access unit (a parse failure, or one skipped while
    /// waiting for a major sync), which the output alone does not tell apart:
    /// both are silence.
    fn kind(outcome: &Outcome) -> &'static str {
        match outcome {
            Outcome::Decoded { .. } => "decoded",
            Outcome::DecodeFailed { .. } => "decode failed",
            Outcome::ParseFailed => "parse failed",
            Outcome::Skipped => "skipped",
        }
    }

    /// The runs a reader cuts `frames` into decode, one after the other as
    /// the workers hand state on, to what one run of them all decodes to.
    #[test]
    fn cut_runs_come_out_as_one_run() {
        let frames = frames_of(&damaged_long_interval_stream());
        let numbered: Vec<(u64, Frame)> = (1..).zip(frames).collect();
        let setup = setup();
        let mut access_unit = AccessUnit::default();
        let mut decoded = DecodedAccessUnit::default();

        let mut whole = Run {
            frames: numbered.clone(),
            ..Run::default()
        };
        decode_run(&mut whole, &setup, &mut access_unit, &mut decoded, false);
        let expected: Vec<_> = whole.outcomes.iter().map(kind).collect();
        assert!(expected.contains(&"parse failed") && expected.contains(&"skipped"));

        for frames in [1, 7, 50] {
            let (job_tx, job_rx) = mpsc::channel();
            let (ticket_tx, ticket_rx) = mpsc::sync_channel(numbered.len());
            for _ in 0..numbered.len() {
                ticket_tx.send(Run::default()).unwrap();
            }
            let run_size = RunSize {
                intervals: 2,
                frames,
            };
            let mut chunker = Chunker::new(job_tx, ticket_rx, run_size);
            for (number, frame) in numbered.iter().cloned() {
                chunker.push(number, frame);
            }
            chunker.dispatch(true);
            drop(chunker);

            let (mut kinds, mut rows) = (Vec::new(), Vec::new());
            for mut run in job_rx.try_iter() {
                let carry_out = run.carry_out.take();
                let hand_on = carry_out.is_some();
                let carry = decode_run(&mut run, &setup, &mut access_unit, &mut decoded, hand_on);
                assert!(run.panic.is_none(), "runs of {frames}: {:?}", run.panic);
                if let (Some(carry_out), Some(carry)) = (carry_out, carry) {
                    let _ = carry_out.send(carry);
                }
                kinds.extend(run.outcomes.iter().map(kind));
                rows.extend_from_slice(&run.rows);
            }
            assert_eq!(kinds, expected, "runs of at most {frames} access units");
            assert!(rows == whole.rows, "runs of at most {frames}: samples");
        }
    }

    /// With no major sync coming, the reader still hands runs out as they
    /// fill, and no run holds more than the most it may.
    #[test]
    fn a_run_stays_bounded_when_major_syncs_stop() {
        let mut bytes = stream(1);
        bytes.extend(continuations(1000));
        let frames = frames_of(&bytes);
        assert_eq!(frames.len(), 1002);

        let (job_tx, job_rx) = mpsc::channel();
        let (ticket_tx, ticket_rx) = mpsc::sync_channel(64);
        for _ in 0..64 {
            ticket_tx.send(Run::default()).unwrap();
        }
        let run_size = RunSize {
            intervals: CHUNK_INTERVALS,
            frames: 50,
        };
        let mut chunker = Chunker::new(job_tx, ticket_rx, run_size);
        for (number, frame) in (1..).zip(frames) {
            chunker.push(number, frame);
            assert!(chunker.current.len() <= 50, "at access unit {number}");
        }
        let runs: Vec<Run> = job_rx.try_iter().collect();
        assert_eq!(
            runs.len(),
            20,
            "runs handed out before the end of the input"
        );
        for run in &runs {
            assert_eq!(run.frames.len(), 50);
            assert!(run.warmup.is_empty(), "a cut run is not replayed");
            assert!(run.carry_out.is_some(), "a cut run hands its state on");
        }
        assert!(runs[0].carry_in.is_none());
        assert!(runs[1..].iter().all(|run| run.carry_in.is_some()));
    }
}
