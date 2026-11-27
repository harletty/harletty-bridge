//! DTS decoded on several threads, one run of frames at a time.
//!
//! Unlike TrueHD, a DTS stream has no point where a decoder starts afresh.
//! The core's subband ADPCM predicts from the previous frame's samples, and
//! the streams we have never erase that history (every frame header sets the
//! predictor-history flag). A decoder started part-way through a stream
//! therefore decodes differently at first. It falls in step once every
//! subband has had a frame coded without prediction and the filter banks
//! have been refilled: on the streams we have, within 3 to 12 frames
//! (median), 26 for 99 starts out of 100, and 47 at most.
//!
//! - this thread reads the input, frames it and cuts it into runs of
//!   [`RUN_FRAMES`] frames;
//! - workers decode the runs, each with an HD decoder of its own that first
//!   decodes the [`WARMUP_FRAMES`] frames before its run, output discarded;
//! - a collector puts the frames back in stream order and does what depends
//!   on the whole history ([`Downstream`]): the core-only decoder that stands
//!   in for a frame the HD decoder cannot decode, the Auro-3D stage, and the
//!   frame count.
//!
//! The warm-up is checked, not trusted. A worker records the fingerprint of
//! its decoder's carried state ([`dca::HdDecoder::carried_state`]) at the
//! start of its run and at a few points into it, and hands its decoder on
//! with the run. The collector keeps the decoder that is in step with the
//! stream: the one the run before ended with. When that decoder's
//! fingerprint is the one the worker had at the start of the run, the run is
//! what the single-threaded path decodes. When it is not, the collector
//! decodes the run itself with that decoder until the two states meet at one
//! of the points, and takes the rest of the run from there; if they never
//! meet, it decodes the whole run and keeps its decoder.
//!
//! The output and the log match the single-threaded path byte for byte.
//! `--strict` keeps the single-threaded path, since it stops at the first
//! error. A stream of core frames alone gains nothing: the HD decoder has
//! nothing to do, and the core-only decoder, which depends on the frames
//! before it, runs on the collector. Neither does the Auro-3D unfolding,
//! which runs there too.

use super::dts_handler::DtsFrameMessage;
use super::dts_thread::{Downstream, FrameSink, Outcome, decode_hd, drain_frames};
use crate::input::InputReader;
use anyhow::{Result, anyhow};
use dca::{HdDecoder, HdFrame};
use indicatif::ProgressBar;
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;

/// Frames per run handed to a worker: about 1.4 s of 48 kHz audio in frames
/// of 512 samples.
const RUN_FRAMES: usize = 128;

/// Frames a worker decodes before its run, output discarded: enough for 99
/// starts out of 100 to fall in step. The collector corrects the others: on
/// the streams we have, up to 1 run in 40, by a few frames each.
const WARMUP_FRAMES: usize = 24;

/// Where the reader cuts runs.
#[derive(Clone, Copy)]
struct RunSize {
    frames: usize,
    warmup: usize,
}

impl RunSize {
    /// [`RUN_FRAMES`] and [`WARMUP_FRAMES`], or `HARLETTY_DTS_RUN_FRAMES` and
    /// `HARLETTY_DTS_WARMUP_FRAMES` when set: verification knobs, so that a
    /// short stream crosses many run boundaries, or so that runs start out of
    /// step and the collector has to correct them.
    fn from_env() -> Self {
        fn knob(name: &str) -> Option<usize> {
            std::env::var(name).ok().and_then(|v| v.parse().ok())
        }
        Self {
            frames: knob("HARLETTY_DTS_RUN_FRAMES")
                .filter(|&n| n > 0)
                .unwrap_or(RUN_FRAMES),
            warmup: knob("HARLETTY_DTS_WARMUP_FRAMES").unwrap_or(WARMUP_FRAMES),
        }
    }
}

pub struct DtsParallelConfig {
    pub input_path: PathBuf,
    pub threads: usize,
    pub prefix: Vec<u8>,
    /// Bounded: decoding now outruns the writer.
    pub tx: mpsc::SyncSender<Result<DtsFrameMessage>>,
    pub pb_clone: Option<ProgressBar>,
}

pub fn spawn_dts_parallel_decoder_thread(
    config: DtsParallelConfig,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || {
        let DtsParallelConfig {
            input_path,
            threads,
            prefix,
            tx,
            pb_clone,
        } = config;
        let tally = decode(
            &input_path,
            prefix,
            threads,
            RunSize::from_env(),
            tx,
            &pb_clone,
        )?;
        log::debug!(
            "{} runs, {} started out of step, {} frames decoded again in order",
            tally.runs,
            tally.out_of_step,
            tally.frames_again
        );
        Ok(())
    })
}

/// The decode of the stream at `input_path` (after `prefix`, what the probe
/// read off a pipe) on `threads` threads, frames out through `tx`.
fn decode(
    input_path: &Path,
    prefix: Vec<u8>,
    threads: usize,
    run_size: RunSize,
    tx: mpsc::SyncSender<Result<DtsFrameMessage>>,
    pb: &Option<ProgressBar>,
) -> Result<Tally> {
    let out = Out {
        tx,
        gone: AtomicBool::new(false),
    };
    let mut downstream = Downstream::new(false);
    let tally = run_on(
        &Dts,
        threads,
        run_size,
        |outcome, frame| {
            downstream.take(outcome, frame.core, pb, &out)?;
            Ok(!out.gone())
        },
        |push: Source| read_dts(input_path, prefix, push),
    )?;
    if !out.gone() {
        downstream.finish(&out);
    }
    Ok(tally)
}

/// Frames the input as the single-threaded path does, while `push` takes
/// the frames.
fn read_dts(input_path: &Path, prefix: Vec<u8>, push: Source) -> Result<()> {
    let mut buffer = prefix;
    let mut more = true;
    InputReader::new(input_path)?.process_chunks(64 * 1024, |chunk| {
        buffer.extend_from_slice(chunk);
        drain_frames(&mut buffer, false, false, &mut |core, exss| {
            more = more && push(FrameRef { core, exss });
            Ok(())
        })?;
        Ok(more)
    })?;
    if more {
        // The final frame has nothing after it, so it is only decodable once
        // the demux knows no EXSS can follow.
        drain_frames(&mut buffer, false, true, &mut |core, exss| {
            more = more && push(FrameRef { core, exss });
            Ok(())
        })?;
    }
    Ok(())
}

/// The bounded channel to the writer, noting when the writer has gone.
struct Out {
    tx: mpsc::SyncSender<Result<DtsFrameMessage>>,
    gone: AtomicBool,
}

impl Out {
    fn gone(&self) -> bool {
        self.gone.load(Ordering::Relaxed)
    }
}

impl FrameSink for Out {
    fn deliver(&self, message: Result<DtsFrameMessage>) {
        if self.tx.send(message).is_err() {
            self.gone.store(true, Ordering::Relaxed);
        }
    }
}

/// One frame as the demux hands it out.
#[derive(Clone, Copy)]
struct FrameRef<'a> {
    core: &'a [u8],
    /// The EXSS substream the HD decoder reads; `None`: the core alone.
    exss: Option<&'a [u8]>,
}

/// Hands every frame of the input to its callback, in order, while the
/// callback says go on.
type Source<'a> = &'a mut dyn FnMut(FrameRef<'_>) -> bool;

/// What the run machinery needs of a codec: decoders that start a stream as
/// the single-threaded path does, and a fingerprint of their state.
trait RunCodec: Sync {
    type Decoder: Send;
    type Outcome: Send;

    /// A decoder as the single-threaded path starts the stream with.
    fn fresh(&self) -> Self::Decoder;
    /// Decodes `frame` for its effect on `decoder` alone.
    fn warm_up(&self, decoder: &mut Self::Decoder, frame: FrameRef<'_>);
    fn decode(&self, decoder: &mut Self::Decoder, frame: FrameRef<'_>) -> Self::Outcome;
    /// A fingerprint of everything `decoder` carries into the next frame:
    /// two decoders with the same one decode what follows identically.
    fn carried_state(&self, decoder: &Self::Decoder) -> u64;
}

/// The HD decoder, and a frame for the warm-up to decode into.
struct Dts;

struct DtsDecoder {
    hd: Box<HdDecoder>,
    scratch: HdFrame,
}

impl RunCodec for Dts {
    type Decoder = DtsDecoder;
    type Outcome = Outcome;

    fn fresh(&self) -> DtsDecoder {
        DtsDecoder {
            hd: Box::new(HdDecoder::new()),
            scratch: HdFrame::default(),
        }
    }

    fn warm_up(&self, decoder: &mut DtsDecoder, frame: FrameRef<'_>) {
        // As `decode_hd` does: the core alone does not reach the HD decoder.
        if let Some(exss) = frame.exss {
            let _ = decoder
                .hd
                .decode_into(frame.core, exss, &mut decoder.scratch);
        }
    }

    fn decode(&self, decoder: &mut DtsDecoder, frame: FrameRef<'_>) -> Outcome {
        decode_hd(&mut decoder.hd, frame.core, frame.exss)
    }

    fn carried_state(&self, decoder: &DtsDecoder) -> u64 {
        decoder.hd.carried_state()
    }
}

/// Frames back to back: their bytes, and the length of each one's core and
/// EXSS (0: none).
#[derive(Default)]
struct Frames {
    bytes: Vec<u8>,
    spans: Vec<(usize, usize)>,
}

impl Frames {
    fn len(&self) -> usize {
        self.spans.len()
    }

    fn clear(&mut self) {
        self.bytes.clear();
        self.spans.clear();
    }

    fn push(&mut self, frame: FrameRef<'_>) {
        let exss = frame.exss.unwrap_or_default();
        self.bytes.extend_from_slice(frame.core);
        self.bytes.extend_from_slice(exss);
        self.spans.push((frame.core.len(), exss.len()));
    }

    /// Appends the last `n` frames of `from` (all of them if it has fewer).
    fn push_tail(&mut self, from: &Frames, n: usize) {
        let skip = from.len().saturating_sub(n);
        let start: usize = from.spans[..skip].iter().map(|&(c, e)| c + e).sum();
        self.bytes.extend_from_slice(&from.bytes[start..]);
        self.spans.extend_from_slice(&from.spans[skip..]);
    }

    fn iter(&self) -> impl Iterator<Item = FrameRef<'_>> {
        let mut at = 0;
        self.spans.iter().map(move |&(core, exss)| {
            let frame = FrameRef {
                core: &self.bytes[at..at + core],
                exss: (exss > 0).then(|| &self.bytes[at + core..at + core + exss]),
            };
            at += core + exss;
            frame
        })
    }
}

/// A run of frames and what became of them, handed round in a circle: the
/// reader fills `warmup` and `frames`, a worker the rest, and the collector,
/// once the run is through, empties it and hands it back to the reader as
/// the ticket for the next run. Its buffers keep their capacity on the way.
struct Run<C: RunCodec> {
    seq: u64,
    /// The frames just before the run, decoded first for their effect on
    /// the decoder.
    warmup: Frames,
    frames: Frames,
    /// One per frame of `frames`.
    outcomes: Vec<C::Outcome>,
    /// `(k, state)`: the fingerprint of the worker's decoder before frame
    /// `k` of the run, `k` rising.
    checkpoints: Vec<(usize, u64)>,
    /// The decoder the worker ended the run with, and its fingerprint.
    decoder: Option<C::Decoder>,
    end_state: u64,
    /// Set when the worker panicked on this run.
    panic: Option<String>,
}

impl<C: RunCodec> Default for Run<C> {
    fn default() -> Self {
        Self {
            seq: 0,
            warmup: Frames::default(),
            frames: Frames::default(),
            outcomes: Vec::new(),
            checkpoints: Vec::new(),
            decoder: None,
            end_state: 0,
            panic: None,
        }
    }
}

impl<C: RunCodec> Run<C> {
    /// Empties the run for the next, keeping its buffers.
    fn clear(&mut self) {
        self.warmup.clear();
        self.frames.clear();
        self.outcomes.clear();
        self.checkpoints.clear();
        self.decoder = None;
        self.panic = None;
    }
}

/// How often the collector had to correct a run.
#[derive(Default, Debug)]
struct Tally {
    runs: u64,
    out_of_step: u64,
    frames_again: u64,
}

/// The decode itself, over the frames `source` hands out: `forward` gets
/// every frame's outcome in stream order, and says whether to go on.
fn run_on<C: RunCodec>(
    codec: &C,
    threads: usize,
    run_size: RunSize,
    forward: impl FnMut(C::Outcome, FrameRef<'_>) -> Result<bool> + Send,
    source: impl FnOnce(Source) -> Result<()>,
) -> Result<Tally> {
    let threads = threads.max(1);
    // Runs read ahead of the one being taken in: enough to keep every
    // worker busy while the collector waits on the oldest.
    let in_flight = threads + 2;

    let (job_tx, job_rx) = mpsc::channel::<Run<C>>();
    let job_rx = Mutex::new(job_rx);
    let (done_tx, done_rx) = mpsc::channel::<Run<C>>();
    // The runs in flight, as tickets: never more of them than the channel holds.
    let (ticket_tx, ticket_rx) = mpsc::sync_channel::<Run<C>>(in_flight);
    for _ in 0..in_flight {
        ticket_tx
            .send(Run::default())
            .expect("the receiver is held here");
    }

    thread::scope(|scope| -> Result<Tally> {
        for _ in 0..threads {
            let done_tx = done_tx.clone();
            let job_rx = &job_rx;
            scope.spawn(move || worker_loop(codec, job_rx, done_tx));
        }
        drop(done_tx);

        let collector = scope.spawn(move || collect(codec, done_rx, ticket_tx, forward));

        let read = read_and_dispatch(source, run_size, job_tx, ticket_rx);
        let tally = collector
            .join()
            .map_err(|_| anyhow!("DTS collector thread panicked"))??;
        read?;
        Ok(tally)
    })
}

/// Cuts the frames `source` hands out into runs and hands them to the
/// workers. Stops early, without an error, once the collector has stopped
/// (the writer is gone or an error is on its way to it).
fn read_and_dispatch<C: RunCodec>(
    source: impl FnOnce(Source) -> Result<()>,
    run_size: RunSize,
    job_tx: mpsc::Sender<Run<C>>,
    ticket_rx: mpsc::Receiver<Run<C>>,
) -> Result<()> {
    let mut chunker = Chunker {
        job_tx,
        ticket_rx,
        run_size,
        seq: 0,
        current: None,
        warmup: Frames::default(),
        stopped: false,
    };
    source(&mut |frame| {
        chunker.push(frame);
        !chunker.stopped
    })?;
    chunker.dispatch(true);
    Ok(())
}

struct Chunker<C: RunCodec> {
    job_tx: mpsc::Sender<Run<C>>,
    ticket_rx: mpsc::Receiver<Run<C>>,
    run_size: RunSize,
    seq: u64,
    /// The run being filled, once a frame has come for it.
    current: Option<Run<C>>,
    /// The frames the next run warms up on: the last ones before it.
    warmup: Frames,
    stopped: bool,
}

impl<C: RunCodec> Chunker<C> {
    fn push(&mut self, frame: FrameRef<'_>) {
        if self.stopped {
            return;
        }
        if self.current.is_none() {
            // A ticket per run in flight, an emptied run whose buffers this
            // one fills; none left once the collector is gone.
            let Ok(mut run) = self.ticket_rx.recv() else {
                self.stopped = true;
                return;
            };
            run.seq = self.seq;
            self.seq += 1;
            std::mem::swap(&mut run.warmup, &mut self.warmup);
            self.current = Some(run);
        }
        let run = self.current.as_mut().expect("filled above");
        run.frames.push(frame);
        if run.frames.len() >= self.run_size.frames {
            self.dispatch(false);
        }
    }

    /// Hands the run being filled out; `last` when no run follows it.
    fn dispatch(&mut self, last: bool) {
        let Some(run) = self.current.take() else {
            return;
        };
        if !last {
            // The frames just before the next run: the end of this one, and
            // of its warm-up when the run is shorter than a warm-up.
            let from_run = self.run_size.warmup.min(run.frames.len());
            let from_warmup = (self.run_size.warmup - from_run).min(run.warmup.len());
            self.warmup.clear();
            self.warmup.push_tail(&run.warmup, from_warmup);
            self.warmup.push_tail(&run.frames, from_run);
        }
        if self.job_tx.send(run).is_err() {
            self.stopped = true;
        }
    }
}

fn worker_loop<C: RunCodec>(
    codec: &C,
    job_rx: &Mutex<mpsc::Receiver<Run<C>>>,
    done_tx: mpsc::Sender<Run<C>>,
) {
    loop {
        let run = {
            let rx = job_rx.lock().unwrap_or_else(|e| e.into_inner());
            rx.recv()
        };
        let Ok(mut run) = run else { return };
        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| decode_run(codec, &mut run))) {
            run.panic = Some(
                panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string()),
            );
        }
        if done_tx.send(run).is_err() {
            return;
        }
    }
}

/// Whether a worker records its state before frame `k` of its run: at the
/// start and at every power of two, so that the collector, correcting a
/// run, decodes it at most twice as far as it has to.
fn is_checkpoint(k: usize) -> bool {
    k == 0 || k.is_power_of_two()
}

/// Decodes `run.frames` into `run.outcomes` with a fresh decoder, after the
/// warm-up, recording its state on the way.
fn decode_run<C: RunCodec>(codec: &C, run: &mut Run<C>) {
    let mut decoder = codec.fresh();
    for frame in run.warmup.iter() {
        codec.warm_up(&mut decoder, frame);
    }
    run.outcomes.reserve(run.frames.len());
    for (k, frame) in run.frames.iter().enumerate() {
        // The first run starts where the stream does: nothing to check.
        if run.seq > 0 && is_checkpoint(k) {
            run.checkpoints.push((k, codec.carried_state(&decoder)));
        }
        run.outcomes.push(codec.decode(&mut decoder, frame));
    }
    run.end_state = codec.carried_state(&decoder);
    run.decoder = Some(decoder);
}

/// Puts the runs back in order and takes their frames on through `forward`,
/// correcting a run whose decoder did not start in step with the stream.
fn collect<C: RunCodec>(
    codec: &C,
    done_rx: mpsc::Receiver<Run<C>>,
    ticket_tx: mpsc::SyncSender<Run<C>>,
    mut forward: impl FnMut(C::Outcome, FrameRef<'_>) -> Result<bool>,
) -> Result<Tally> {
    let mut pending = BTreeMap::new();
    let mut next = 0u64;
    // The decoder in step with the stream where the next run starts, and
    // its fingerprint when known.
    let mut in_step: Option<(C::Decoder, Option<u64>)> = None;
    let mut tally = Tally::default();

    while let Ok(run) = done_rx.recv() {
        pending.insert(run.seq, run);
        while let Some(mut run) = pending.remove(&next) {
            next += 1;
            if let Some(panic) = run.panic.take() {
                return Err(anyhow!("DTS decoder thread panicked: {panic}"));
            }
            if !take_run(codec, &mut run, &mut in_step, &mut tally, &mut forward)? {
                // The writer is gone; dropping the tickets stops the reader.
                return Ok(tally);
            }
            run.clear();
            let _ = ticket_tx.send(run);
        }
    }
    if !pending.is_empty() {
        return Err(anyhow!("DTS decoder threads stopped with runs undelivered"));
    }
    Ok(tally)
}

/// Takes one run on, in order; `false` once `forward` says stop.
fn take_run<C: RunCodec>(
    codec: &C,
    run: &mut Run<C>,
    in_step: &mut Option<(C::Decoder, Option<u64>)>,
    tally: &mut Tally,
    forward: &mut impl FnMut(C::Outcome, FrameRef<'_>) -> Result<bool>,
) -> Result<bool> {
    tally.runs += 1;
    let mut frames = run.frames.iter();
    let mut outcomes = run.outcomes.drain(..);
    // Whether the worker's decoder fell in step with the stream within the
    // run, so that its outcomes are the stream's from where `frames` stands.
    let worker_in_step = match in_step {
        // The first run starts where the stream does, with a decoder as the
        // single-threaded path's.
        None => true,
        Some((decoder, state)) => {
            let mut at = 0;
            let mut met = false;
            for &(k, worker_state) in &run.checkpoints {
                while at < k {
                    let frame = frames.next().expect("checkpoints lie in the run");
                    outcomes.next();
                    *state = None;
                    tally.frames_again += 1;
                    if !forward(codec.decode(decoder, frame), frame)? {
                        return Ok(false);
                    }
                    at += 1;
                }
                let now = *state.get_or_insert_with(|| codec.carried_state(decoder));
                if now == worker_state {
                    met = true;
                    break;
                }
            }
            if !met {
                // Never in step: the run is decoded here, to its end.
                for frame in frames.by_ref() {
                    outcomes.next();
                    *state = None;
                    tally.frames_again += 1;
                    if !forward(codec.decode(decoder, frame), frame)? {
                        return Ok(false);
                    }
                }
            }
            if at > 0 || !met {
                tally.out_of_step += 1;
            }
            met
        }
    };
    if worker_in_step {
        for (outcome, frame) in outcomes.by_ref().zip(frames.by_ref()) {
            if !forward(outcome, frame)? {
                return Ok(false);
            }
        }
        let decoder = run
            .decoder
            .take()
            .expect("a run that did not panic has its decoder");
        *in_step = Some((decoder, Some(run.end_state)));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::super::dts_thread::{DtsDecoderThreadConfig, spawn_dts_decoder_thread};
    use super::*;

    /// A codec with a history of its own, as the core's ADPCM: a frame
    /// `[1, value]` sets the state to `value` (a frame coded without
    /// prediction), any other `[_, value]` folds `value` into it. The
    /// outcome of a frame is the state after it.
    struct Toy {
        /// A frame of this value panics.
        panic_at: Option<u8>,
    }

    impl RunCodec for Toy {
        type Decoder = u64;
        type Outcome = u64;

        fn fresh(&self) -> u64 {
            0
        }

        fn warm_up(&self, decoder: &mut u64, frame: FrameRef<'_>) {
            self.decode(decoder, frame);
        }

        fn decode(&self, decoder: &mut u64, frame: FrameRef<'_>) -> u64 {
            let &[reset, value] = frame.core else {
                panic!("toy frames are two bytes");
            };
            if Some(value) == self.panic_at {
                panic!("toy frame {value}");
            }
            *decoder = if reset == 1 {
                u64::from(value)
            } else {
                decoder.wrapping_mul(31).wrapping_add(u64::from(value))
            };
            *decoder
        }

        fn carried_state(&self, decoder: &u64) -> u64 {
            *decoder
        }
    }

    const TOY: Toy = Toy { panic_at: None };

    /// `count` toy frames, a reset every `period` frames (`None`: never).
    fn stream(count: usize, period: Option<usize>) -> Vec<[u8; 2]> {
        (0..count)
            .map(|i| {
                let reset = period.is_some_and(|p| i % p == p - 1);
                [u8::from(reset), (i * 7 + 3) as u8]
            })
            .collect()
    }

    fn serial(frames: &[[u8; 2]]) -> Vec<u64> {
        let mut decoder = TOY.fresh();
        frames
            .iter()
            .map(|f| {
                TOY.decode(
                    &mut decoder,
                    FrameRef {
                        core: f,
                        exss: None,
                    },
                )
            })
            .collect()
    }

    /// Hands `frames` out while the reader takes them.
    fn source(frames: &[[u8; 2]]) -> impl FnOnce(Source) -> Result<()> + '_ {
        move |push: Source| {
            for f in frames {
                if !push(FrameRef {
                    core: f,
                    exss: None,
                }) {
                    break;
                }
            }
            Ok(())
        }
    }

    fn parallel(
        codec: &Toy,
        frames: &[[u8; 2]],
        threads: usize,
        (run_frames, warmup): (usize, usize),
    ) -> Result<(Vec<u64>, Tally)> {
        let mut got = Vec::new();
        let run_size = RunSize {
            frames: run_frames,
            warmup,
        };
        let tally = run_on(
            codec,
            threads,
            run_size,
            |outcome, _| {
                got.push(outcome);
                Ok(true)
            },
            source(frames),
        )?;
        Ok((got, tally))
    }

    #[test]
    fn runs_decode_as_one_decoder_does() {
        for period in [Some(1), Some(3), Some(10), Some(40), None] {
            let frames = stream(400, period);
            let expected = serial(&frames);
            for threads in [1, 2, 4, 8] {
                for size in [
                    (1, 0),
                    (1, 3),
                    (2, 1),
                    (7, 4),
                    (50, 24),
                    (128, 24),
                    (500, 24),
                ] {
                    let (got, _) = parallel(&TOY, &frames, threads, size).unwrap();
                    assert!(
                        got == expected,
                        "reset every {period:?}, {threads} threads, runs of {size:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_warm_up_past_a_reset_needs_no_correction() {
        let frames = stream(400, Some(3));
        let (got, tally) = parallel(&TOY, &frames, 4, (16, 3)).unwrap();
        assert_eq!(got, serial(&frames));
        assert_eq!(
            (tally.runs, tally.out_of_step, tally.frames_again),
            (25, 0, 0)
        );
    }

    #[test]
    fn a_run_is_corrected_until_its_decoder_falls_in_step() {
        // No warm-up: every run but the first starts out of step and falls in
        // step at its first reset, within 10 frames; the checkpoint after it
        // is at most 16 frames in.
        let frames = stream(400, Some(10));
        let (got, tally) = parallel(&TOY, &frames, 4, (50, 0)).unwrap();
        assert_eq!(got, serial(&frames));
        assert_eq!((tally.runs, tally.out_of_step), (8, 7));
        assert!((7..=7 * 16).contains(&tally.frames_again), "{tally:?}");
    }

    #[test]
    fn a_stream_without_resets_is_decoded_in_order() {
        // Never in step: the collector decodes every run but the first.
        let frames = stream(400, None);
        let (got, tally) = parallel(&TOY, &frames, 4, (7, 4)).unwrap();
        assert_eq!(got, serial(&frames));
        assert_eq!(
            (tally.runs, tally.out_of_step, tally.frames_again),
            (58, 57, 400 - 7)
        );
    }

    #[test]
    fn a_worker_panic_is_an_error() {
        let frames = stream(400, Some(5));
        let codec = Toy {
            panic_at: Some(frames[300][1]),
        };
        let err = parallel(&codec, &frames, 4, (16, 4))
            .err()
            .expect("an error");
        assert!(err.to_string().contains("panicked"), "{err}");
    }

    #[test]
    fn the_decode_stops_when_the_writer_goes() {
        let frames = stream(100_000, Some(5));
        let mut taken = 0;
        let run_size = RunSize {
            frames: 16,
            warmup: 4,
        };
        let forward = |_, _: FrameRef<'_>| {
            taken += 1;
            Ok(taken < 10)
        };
        let tally = run_on(&TOY, 4, run_size, forward, source(&frames)).unwrap();
        assert_eq!(taken, 10);
        assert_eq!(tally.runs, 1);
    }

    #[test]
    fn frames_keep_their_core_and_exss_apart() {
        let mut frames = Frames::default();
        for (core, exss) in [
            (&b"ab"[..], Some(&b"cde"[..])),
            (b"f", None),
            (b"gh", Some(b"i")),
        ] {
            frames.push(FrameRef { core, exss });
        }
        let mut tail = Frames::default();
        tail.push_tail(&frames, 2);
        tail.push_tail(&frames, 9);
        let seen: Vec<_> = tail.iter().map(|f| (f.core, f.exss)).collect();
        let expected: [(&[u8], Option<&[u8]>); 5] = [
            (b"f", None),
            (b"gh", Some(b"i")),
            (b"ab", Some(b"cde")),
            (b"f", None),
            (b"gh", Some(b"i")),
        ];
        assert_eq!(seen, expected);
    }

    /// Every message of a decode, as the bits of the samples it carries.
    fn messages(rx: mpsc::Receiver<Result<DtsFrameMessage>>) -> Vec<Vec<u32>> {
        rx.iter()
            .map(|message| match message.unwrap() {
                DtsFrameMessage::Core(push) => push
                    .pcm
                    .fullband_channels
                    .iter()
                    .chain(&push.pcm.lfe_channel)
                    .flatten()
                    .map(|s| s.to_bits())
                    .collect(),
                DtsFrameMessage::Hd { frame, .. } => frame
                    .samples
                    .iter()
                    .flatten()
                    .chain(&frame.x_samples)
                    .flatten()
                    .map(|s| s.to_bits())
                    .collect(),
                DtsFrameMessage::Auro(_) | DtsFrameMessage::AuroFrame(_) => {
                    panic!("no Auro-3D in the fixtures")
                }
            })
            .collect()
    }

    #[test]
    fn the_fixtures_decode_as_on_one_thread() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        for name in ["dts_core_tone_10f.dts", "dts_core_96k_mono_tone_10f.dts"] {
            let path = root.join(name);
            let (tx, rx) = mpsc::channel();
            spawn_dts_decoder_thread(DtsDecoderThreadConfig {
                input_path: path.clone(),
                strict_mode: false,
                prefix: Vec::new(),
                tx,
                pb_clone: None,
            })
            .join()
            .unwrap()
            .unwrap();
            let expected = messages(rx);
            assert_eq!(expected.len(), 10, "{name}");
            for (threads, frames, warmup) in [(1, 128, 24), (3, 2, 1), (4, 3, 0)] {
                let (tx, rx) = mpsc::sync_channel(4);
                let reader = thread::spawn(move || messages(rx));
                let run_size = RunSize { frames, warmup };
                decode(&path, Vec::new(), threads, run_size, tx, &None).unwrap();
                assert!(
                    reader.join().unwrap() == expected,
                    "{name}: {threads} threads, runs of {frames}, warm-up {warmup}"
                );
            }
        }
    }
}
