use std::env;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
#[path = "../mat.rs"]
mod mat;
use mat::MatStream;
use spdif::SpdifParser;
use sys::{InputReader, ShutdownHandle};

#[derive(Debug)]
struct Args {
    input: PathBuf,
    output: PathBuf,
    stats_output: Option<PathBuf>,
    chunk_size: usize,
    max_bytes: Option<u64>,
    max_packets: Option<u64>,
    drain_pipe: bool,
}

#[derive(Debug, Default)]
struct StageStat {
    total: Duration,
    max: Duration,
    calls: u64,
}

impl StageStat {
    fn record(&mut self, elapsed: Duration) {
        self.total += elapsed;
        self.max = self.max.max(elapsed);
        self.calls += 1;
    }

    fn avg_us(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.total.as_secs_f64() * 1_000_000.0 / self.calls as f64
        }
    }

    fn total_ms(&self) -> f64 {
        self.total.as_secs_f64() * 1_000.0
    }

    fn max_us(&self) -> f64 {
        self.max.as_secs_f64() * 1_000_000.0
    }
}

#[derive(Debug, Default)]
struct PerfStats {
    wall: Duration,
    input_read: StageStat,
    spdif_push: StageStat,
    spdif_packet: StageStat,
    mat_push_payload: StageStat,
    mat_next_chunk: StageStat,
    mat_swap: StageStat,
    mat_padding: StageStat,
    write_output: StageStat,
    input_chunks: u64,
    input_bytes: u64,
    iec_packets: u64,
    truehd_packets: u64,
    mat_chunks: u64,
    mat_swap_bytes: u64,
    mat_padding_words: u64,
    output_bytes: u64,
}

impl PerfStats {
    fn write_report(&self, path: &std::path::Path, args: &Args) -> Result<()> {
        let file = File::create(path)
            .with_context(|| format!("failed to create stats file {}", path.display()))?;
        let mut writer = BufWriter::new(file);
        let wall_s = self.wall.as_secs_f64();
        let input_mib = self.input_bytes as f64 / (1024.0 * 1024.0);
        let output_mib = self.output_bytes as f64 / (1024.0 * 1024.0);
        let input_mib_s = if wall_s > 0.0 {
            input_mib / wall_s
        } else {
            0.0
        };
        let output_mib_s = if wall_s > 0.0 {
            output_mib / wall_s
        } else {
            0.0
        };

        writeln!(writer, "dump_mat instrumentation")?;
        writeln!(writer, "input={}", args.input.display())?;
        writeln!(writer, "output={}", args.output.display())?;
        writeln!(writer, "stats_output={}", path.display())?;
        writeln!(writer, "chunk_size={}", args.chunk_size)?;
        writeln!(
            writer,
            "max_bytes={}",
            args.max_bytes
                .map_or_else(|| "none".to_string(), |v| v.to_string())
        )?;
        writeln!(
            writer,
            "max_packets={}",
            args.max_packets
                .map_or_else(|| "none".to_string(), |v| v.to_string())
        )?;
        writeln!(writer, "drain_pipe={}", args.drain_pipe)?;
        writeln!(writer, "wall_ms={:.3}", self.wall.as_secs_f64() * 1_000.0)?;
        writeln!(writer, "input_chunks={}", self.input_chunks)?;
        writeln!(writer, "input_bytes={}", self.input_bytes)?;
        writeln!(writer, "iec_packets={}", self.iec_packets)?;
        writeln!(writer, "truehd_packets={}", self.truehd_packets)?;
        writeln!(writer, "mat_chunks={}", self.mat_chunks)?;
        writeln!(writer, "output_bytes={}", self.output_bytes)?;
        writeln!(writer, "input_mib_per_s={:.3}", input_mib_s)?;
        writeln!(writer, "output_mib_per_s={:.3}", output_mib_s)?;
        writeln!(
            writer,
            "stage,input_read,total_ms={:.3},avg_us={:.3},max_us={:.3},calls={}",
            self.input_read.total_ms(),
            self.input_read.avg_us(),
            self.input_read.max_us(),
            self.input_read.calls
        )?;
        writeln!(
            writer,
            "stage,spdif_push,total_ms={:.3},avg_us={:.3},max_us={:.3},calls={}",
            self.spdif_push.total_ms(),
            self.spdif_push.avg_us(),
            self.spdif_push.max_us(),
            self.spdif_push.calls
        )?;
        writeln!(
            writer,
            "stage,spdif_packet,total_ms={:.3},avg_us={:.3},max_us={:.3},calls={}",
            self.spdif_packet.total_ms(),
            self.spdif_packet.avg_us(),
            self.spdif_packet.max_us(),
            self.spdif_packet.calls
        )?;
        writeln!(
            writer,
            "stage,mat_push_payload,total_ms={:.3},avg_us={:.3},max_us={:.3},calls={}",
            self.mat_push_payload.total_ms(),
            self.mat_push_payload.avg_us(),
            self.mat_push_payload.max_us(),
            self.mat_push_payload.calls
        )?;
        writeln!(
            writer,
            "stage,mat_next_chunk,total_ms={:.3},avg_us={:.3},max_us={:.3},calls={}",
            self.mat_next_chunk.total_ms(),
            self.mat_next_chunk.avg_us(),
            self.mat_next_chunk.max_us(),
            self.mat_next_chunk.calls
        )?;
        writeln!(
            writer,
            "stage,mat_swap,total_ms={:.3},avg_us={:.3},max_us={:.3},calls={},bytes={}",
            self.mat_swap.total_ms(),
            self.mat_swap.avg_us(),
            self.mat_swap.max_us(),
            self.mat_swap.calls,
            self.mat_swap_bytes
        )?;
        writeln!(
            writer,
            "stage,mat_padding,total_ms={:.3},avg_us={:.3},max_us={:.3},calls={},words={}",
            self.mat_padding.total_ms(),
            self.mat_padding.avg_us(),
            self.mat_padding.max_us(),
            self.mat_padding.calls,
            self.mat_padding_words
        )?;
        writeln!(
            writer,
            "stage,write_output,total_ms={:.3},avg_us={:.3},max_us={:.3},calls={}",
            self.write_output.total_ms(),
            self.write_output.avg_us(),
            self.write_output.max_us(),
            self.write_output.calls
        )?;
        writer.flush()?;
        Ok(())
    }
}

#[cfg(unix)]
fn ensure_input_pipe(path: &std::path::Path) -> Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    if path.exists() {
        return Ok(false);
    }

    let path_cstr = CString::new(path.as_os_str().as_bytes())?;
    let result = unsafe { libc::mkfifo(path_cstr.as_ptr(), 0o666) };
    if result != 0 {
        bail!(
            "failed to create FIFO {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }

    let chmod_result = unsafe { libc::chmod(path_cstr.as_ptr(), 0o666) };
    if chmod_result != 0 {
        bail!(
            "failed to chmod FIFO {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }

    Ok(true)
}

#[cfg(not(unix))]
fn ensure_input_pipe(_path: &std::path::Path) -> Result<bool> {
    Ok(false)
}

fn cleanup_input_pipe(path: &std::path::Path, created: bool) {
    if created {
        let _ = std::fs::remove_file(path);
    }
}

fn parse_args() -> Result<Args> {
    let mut args = env::args_os().skip(1);
    let mut input = None;
    let mut output = None;
    let mut stats_output = None;
    let mut chunk_size = 64 * 1024usize;
    let mut max_bytes = None;
    let mut max_packets = None;
    let mut drain_pipe = false;

    while let Some(arg) = args.next() {
        match arg.to_string_lossy().as_ref() {
            "--output" => {
                output = Some(PathBuf::from(
                    args.next().context("missing value for --output")?,
                ))
            }
            "--stats-output" => {
                stats_output = Some(PathBuf::from(
                    args.next().context("missing value for --stats-output")?,
                ))
            }
            "--chunk-size" => {
                chunk_size = args
                    .next()
                    .context("missing value for --chunk-size")?
                    .to_string_lossy()
                    .parse()
                    .context("invalid --chunk-size")?;
            }
            "--max-bytes" => {
                max_bytes = Some(
                    args.next()
                        .context("missing value for --max-bytes")?
                        .to_string_lossy()
                        .parse()
                        .context("invalid --max-bytes")?,
                );
            }
            "--max-packets" => {
                max_packets = Some(
                    args.next()
                        .context("missing value for --max-packets")?
                        .to_string_lossy()
                        .parse()
                        .context("invalid --max-packets")?,
                );
            }
            "--drain-pipe" => drain_pipe = true,
            "--help" | "-h" => {
                println!(
                    "Usage: dump_mat INPUT --output FILE [--stats-output FILE] [--chunk-size BYTES] [--max-bytes BYTES] [--max-packets COUNT] [--drain-pipe]"
                );
                std::process::exit(0);
            }
            value if value.starts_with('-') => bail!("unknown flag: {}", value),
            _ => {
                if input.is_some() {
                    bail!("unexpected extra positional argument");
                }
                input = Some(PathBuf::from(arg));
            }
        }
    }

    Ok(Args {
        input: input.context("missing INPUT path")?,
        output: output.context("missing --output FILE")?,
        stats_output,
        chunk_size: chunk_size.max(1),
        max_bytes,
        max_packets,
        drain_pipe,
    })
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let created_pipe = ensure_input_pipe(&args.input)?;
    let shutdown = ShutdownHandle::install()?;
    let shutdown_signal = shutdown.shutdown_signal();
    let output = File::create(&args.output)
        .with_context(|| format!("failed to create {}", args.output.display()))?;
    let mut writer = BufWriter::new(output);
    let mut spdif_parser = SpdifParser::new();
    let mut mat_stream = MatStream::default();
    mat_stream.enable_perf_stats();
    let mut perf = PerfStats::default();
    let wall_started = Instant::now();
    let mut input_chunks = 0u64;
    let mut iec_packets = 0u64;
    let mut truehd_packets = 0u64;
    let mut mat_chunks = 0u64;
    let mut output_bytes = 0u64;

    loop {
        let mut input = InputReader::new(&args.input, args.drain_pipe)?;
        let chunks_before = input_chunks;
        let is_pipe = input.is_pipe();

        input.process_chunks_with_shutdown(args.chunk_size, &shutdown_signal, |chunk| {
            let input_read_started = Instant::now();
            input_chunks += 1;
            perf.input_chunks += 1;
            perf.input_bytes += chunk.len() as u64;
            let spdif_push_started = Instant::now();
            spdif_parser.push_bytes(chunk);
            perf.spdif_push.record(spdif_push_started.elapsed());
            perf.input_read.record(input_read_started.elapsed());

            while let Some(packet) = spdif_parser.get_next_packet() {
                let spdif_packet_started = Instant::now();
                iec_packets += 1;
                perf.iec_packets += 1;
                if let Some(limit) = args.max_packets {
                    if iec_packets > limit {
                        perf.spdif_packet.record(spdif_packet_started.elapsed());
                        return Ok(false);
                    }
                }

                if !MatStream::accepts_data_type(packet.data_type) {
                    perf.spdif_packet.record(spdif_packet_started.elapsed());
                    continue;
                }

                truehd_packets += 1;
                perf.truehd_packets += 1;
                let mat_push_started = Instant::now();
                mat_stream.push_payload(&packet.payload);
                perf.mat_push_payload.record(mat_push_started.elapsed());
                loop {
                    let mat_next_started = Instant::now();
                    let Some(mat_chunk) = mat_stream.next_chunk().map_err(anyhow::Error::msg)?
                    else {
                        perf.mat_next_chunk.record(mat_next_started.elapsed());
                        break;
                    };
                    perf.mat_next_chunk.record(mat_next_started.elapsed());

                    let remaining = args
                        .max_bytes
                        .map(|limit: u64| limit.saturating_sub(output_bytes))
                        .unwrap_or(u64::MAX) as usize;
                    if remaining == 0 {
                        perf.spdif_packet.record(spdif_packet_started.elapsed());
                        return Ok(false);
                    }
                    let to_write = mat_chunk.len().min(remaining);
                    let write_started = Instant::now();
                    writer.write_all(&mat_chunk[..to_write])?;
                    perf.write_output.record(write_started.elapsed());
                    output_bytes += to_write as u64;
                    mat_chunks += 1;
                    perf.mat_chunks += 1;
                    perf.output_bytes += to_write as u64;
                    if to_write < mat_chunk.len() {
                        perf.spdif_packet.record(spdif_packet_started.elapsed());
                        return Ok(false);
                    }
                }
                perf.spdif_packet.record(spdif_packet_started.elapsed());
            }

            Ok(true)
        })?;

        if !is_pipe || input_chunks > chunks_before || ShutdownHandle::is_requested() {
            break;
        }

        eprintln!("dump_mat: waiting for writer on {}", args.input.display());
        thread::sleep(Duration::from_millis(100));
    }

    writer.flush()?;
    perf.wall = wall_started.elapsed();
    if let Some(mat_perf) = mat_stream.perf_stats() {
        perf.mat_swap.total = mat_perf.swap_total;
        perf.mat_swap.max = mat_perf.swap_max;
        perf.mat_swap.calls = mat_perf.swap_calls;
        perf.mat_swap_bytes = mat_perf.swap_bytes;
        perf.mat_padding.total = mat_perf.padding_total;
        perf.mat_padding.max = mat_perf.padding_max;
        perf.mat_padding.calls = mat_perf.padding_calls;
        perf.mat_padding_words = mat_perf.padding_words;
    }
    if let Some(stats_path) = &args.stats_output {
        perf.write_report(stats_path, &args)?;
    }
    eprintln!(
        "dump_mat: input={} output={} input_chunks={} iec_packets={} truehd_packets={} mat_chunks={} output_bytes={}",
        args.input.display(),
        args.output.display(),
        input_chunks,
        iec_packets,
        truehd_packets,
        mat_chunks,
        output_bytes
    );
    cleanup_input_pipe(&args.input, created_pipe);
    drop(shutdown);
    Ok(())
}
