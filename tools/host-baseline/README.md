# host-baseline

Decodes the baseline corpus through the host's own bridge loader and router
(`orender_engine::bridge_set::BridgeSet`, what `orender` and liborender
decode with), so that the family plugins can be checked against the combined
bridge they replace, bit for bit, including when a stream arrives in small
reads.

```
HARLETTY_BASELINE_CORPUS=<manifest> host-baseline [--read N] <bridge library>...
```

- The manifest lists one stream per line, `<family> <transport> <path>`,
  with `transport` `raw` or `iec61937` (an `ffmpeg -f spdif` capture, pushed
  one burst per call with its data type). Lines starting with `#` are
  comments. The streams are not part of the repository.
- The libraries are loaded in the order given, as `--bridge-path` does: one
  library is never probed, several are routed by their probes.
- `--read N` pushes raw streams in reads of N bytes (64 KiB, as the CLI
  reads, by default).

It prints one JSON line per stream: `frames`, `samples`, `pcm_hash` (samples
and object positions, as `bridge_bench`), `frame_hash` (every field of every
frame, whatever push it came out of), `description_hash` (what the host reads
of the stream at the end), `host_hash` (frames, errors and the
description after every push, which depends on how pushes are cut), and
`decode_ms`, the wall time of the pushes (IEC 61937 parsing included).
`compare.py` checks two runs against each other.

Build it next to the bridge, against the same Omniphony checkout:

```
cargo build --release --manifest-path tools/host-baseline/Cargo.toml \
    --target-dir target/host-baseline
```
