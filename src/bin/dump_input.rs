use std::env;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sys::{InputReader, ShutdownHandle};

#[derive(Debug)]
struct Args {
    input: PathBuf,
    output: PathBuf,
    chunk_size: usize,
    max_bytes: Option<u64>,
    drain_pipe: bool,
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
    let mut chunk_size = 64 * 1024usize;
    let mut max_bytes = None;
    let mut drain_pipe = false;

    while let Some(arg) = args.next() {
        match arg.to_string_lossy().as_ref() {
            "--output" => output = Some(PathBuf::from(args.next().context("missing value for --output")?)),
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
            "--drain-pipe" => drain_pipe = true,
            "--help" | "-h" => {
                println!("Usage: dump_input INPUT --output FILE [--chunk-size BYTES] [--max-bytes BYTES] [--drain-pipe]");
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
        chunk_size: chunk_size.max(1),
        max_bytes,
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
    let mut total_bytes = 0u64;
    let mut total_chunks = 0u64;

    loop {
        let mut input = InputReader::new(&args.input, args.drain_pipe)?;
        let chunks_before = total_chunks;
        let is_pipe = input.is_pipe();

        input.process_chunks_with_shutdown(args.chunk_size, &shutdown_signal, |chunk| {
            let remaining = args
                .max_bytes
                .map(|limit: u64| limit.saturating_sub(total_bytes))
                .unwrap_or(u64::MAX) as usize;
            if remaining == 0 {
                return Ok(false);
            }
            let to_write = chunk.len().min(remaining);
            writer.write_all(&chunk[..to_write])?;
            total_bytes += to_write as u64;
            total_chunks += 1;
            Ok(to_write == chunk.len())
        })?;

        if !is_pipe || total_chunks > chunks_before || ShutdownHandle::is_requested() {
            break;
        }

        eprintln!("dump_input: waiting for writer on {}", args.input.display());
        thread::sleep(Duration::from_millis(100));
    }

    writer.flush()?;
    eprintln!(
        "dump_input: input={} output={} bytes={} chunks={}",
        args.input.display(),
        args.output.display(),
        total_bytes,
        total_chunks
    );
    cleanup_input_pipe(&args.input, created_pipe);
    drop(shutdown);
    Ok(())
}
