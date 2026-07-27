# The `harletty` CLI

Offline decoder for TrueHD, E-AC-3 JOC and DTS bitstreams. It writes
Dolby Atmos master files — a `.atmos` presentation, its
`.atmos.metadata` object automation, and the `.atmos.audio` bed/object
interleave — or plain PCM when you ask for a downmix presentation.

It is a **separate artifact from the bridge**. The bridge is the runtime
plugin that plays audio; this is the batch tool that converts it. They
share the decoder crates and nothing else — the bridge does not link the
CLI, cannot reach it, and does not pay for its dependencies.
`scripts/check-crate-isolation.sh` asserts that in CI.

> Formerly `truehdd`. It is deliberately not called that any more: it
> covers three format families now, and sharing a binary name with its
> still-active-at-the-time upstream meant whichever came first on `PATH`
> won. See [truehdd-fork-retirement.md](truehdd-fork-retirement.md).

## Install

Download `harletty-cli-<version>-<system>.zip` from the
[releases page](https://github.com/harletty/harletty-bridge/releases)
and unzip it anywhere. It is a single self-contained binary.

Or build it:

```sh
cargo build --release -p harletty
./target/release/harletty --version
```

## Commands

```
harletty [GLOBAL OPTIONS] <COMMAND>

  decode   Decode a stream into audio + Atmos master files
  info     Print stream information and exit
  help     Print help for a command
```

### Global options

Accepted before or after the subcommand.

| Option | Values | Default | What it does |
|---|---|---|---|
| `--codec` | `auto`, `truehd`, `eac3`, `dts` | `auto` | Input format. `auto` detects by sync word; override it when the probe guesses wrong on a headerless pipe. |
| `--loglevel` | `off`, `error`, `warn`, `info`, `debug`, `trace` | `info` | `warn` is the useful floor: it still surfaces the timeline warning described under [Damaged streams](#damaged-streams). |
| `--log-format` | `plain`, `json` | `plain` | `json` emits one structured record per line, for scripted callers. |
| `--strict` | flag | off | Treat warnings as fatal and stop at the first one. Use it to *audit* a stream, not to convert it — a normal Blu-ray rip trips warnings that do not affect the output. |
| `--progress` | flag | off | Progress bars on stderr. |

### `decode`

```sh
harletty decode [OPTIONS] <INPUT>
```

`<INPUT>` is a file, or `-` to read stdin.

| Option | Values | Default | What it does |
|---|---|---|---|
| `--output-path <PATH>` | path prefix | *(none)* | Base name for the outputs — extensions are appended, so `--output-path out` writes `out.atmos`, `out.atmos.audio`, … With no `--output-path`, the stream is decoded and validated but nothing is written. |
| `--format` | `caf`, `pcm`, `w64` | `caf` | Audio container. **Ignored for Atmos output**, which is always CAF; see [Output files](#output-files). `pcm` is raw 24-bit little-endian, `w64` is Wave64 with a `.wav` extension. |
| `--no-audio` | flag | off | Skip the audio file; still write `.atmos` and `.atmos.metadata`. Much faster when you only want the object automation. |
| `--presentation <0-3>` | index | `3` | Which TrueHD presentation to decode. `3` is the 16-channel Atmos presentation; `0`–`2` are the stereo/5.1/7.1 downmixes carried in the same stream. **TrueHD only** — silently ignored for E-AC-3 and DTS, which have no equivalent. |
| `--bed-conform` | flag | off | Force the Atmos bed to a conformant 7.1.2 layout. |
| `--warp-mode` | `normal`, `warping`, `prologiciix`, `loro` | *(from stream)* | Downmix warp mode to declare when the metadata does not carry one. |
| `--no-estimate-progress` | flag | off | Skip the pre-pass that counts frames for the progress bar. Automatic for stdin, which cannot be pre-scanned. |

### `info`

```sh
harletty info <INPUT>
```

Prints the stream configuration and exits without decoding audio. The
report is shaped per codec — TrueHD lists its presentations, E-AC-3
reports the frame's own configuration.

TrueHD:

```
TrueHD Stream Information
=========================

Stream Information
  Format Sync               F8726FBA
  Sampling rate             48000 Hz
  Peak data rate            8412 kbps
  Number of substreams      4
  Dolby Atmos               true

Presentation Information
  Presentation 0
    Number of channels      2
    Presentation type       Downmix of presentation 1
  …
```

E-AC-3:

```
Codec        : EAC3 (Dolby Digital Plus)
Bitstream ID : 16
Frame type   : independent
Sample rate  : 48000 Hz
Channel mode : 5 ch + LFE
OAMD         : yes
JOC          : yes
Frames seen  : 47
```

## Output files

What lands on disk depends on whether the result is object audio.

**Atmos** (presentation 3 with objects present, or E-AC-3 JOC, or DTS:X)
— a DAMF master set, which is what Resolve, Pro Tools and the Dolby
Reference Player consume:

| File | Contents |
|---|---|
| `<base>.atmos` | The presentation: channel/object declarations, trims, frame rate, and the names of the other two files. |
| `<base>.atmos.metadata` | Per-event object automation, timestamped in samples. |
| `<base>.atmos.audio` | Interleaved bed + object audio, CAF. |

**Non-Atmos** (presentation 0–2, or a stream with no objects) — a single
audio file named from `--format`: `<base>.caf`, `<base>.pcm` or
`<base>.wav`.

`--format` is ignored for the Atmos case, with an `info` log line saying
so. That is not a limitation of this tool: the DAMF spec pins the audio
member to CAF.

## Recipes

**Straight conversion.**

```sh
harletty --progress decode --output-path "movie" movie.thd
```

**Out of a container, without a temporary file.** `harletty` reads
stdin, so pipe ffmpeg into it:

```sh
ffmpeg -i movie.mkv -map 0:a:0 -c copy -f truehd - \
  | harletty --progress decode - --output-path "movie"
```

**Object automation only**, skipping the (much larger, much slower)
audio member:

```sh
harletty --loglevel error decode --no-audio --output-path "movie" movie.thd
```

**A stereo downmix as a plain wav**, from the same stream:

```sh
harletty decode --presentation 0 --format w64 --output-path "movie-stereo" movie.thd
```

**Machine-readable logs** for a batch runner:

```sh
harletty --log-format json --loglevel warn decode - --output-path "$base" < stream.thd
```

## Damaged streams

Real rips are not always clean — seamless-branching titles in particular
stitch segments together in ways that leave malformed access units at the
joins.

By default `harletty` recovers instead of aborting: on a parse failure it
resets and resumes at the next major sync, and the access units it could
not decode are **replaced with silence of the same duration** rather than
dropped. Dropping them would shorten the output and slide everything
after the damage earlier against the picture — permanently, and
cumulatively at every damaged join.

A run that had to do this says so at `warn`:

```
WARN 17 access units could not be decoded and were replaced with silence
     (680 samples, 14 ms). Output length is preserved; audio stays aligned
     with the source timeline.
```

Read that as: the output is no longer bit-exact to the source across
those 14 ms, but it is still in sync. If you would rather the run failed
loudly than substituted anything, use `--strict`.

Errors at the *extractor* level — where framing itself is lost and the
decoder resynchronises — are not compensated this way, because the number
of access units that went by is unknowable. They remain a possible source
of drift.

## Known limitations

- **DTS:X object positions are not decoded.** Spatial presentations are
  exported with their channel layout and labels, but objects sit at
  static positions; the per-frame trajectories live in a proprietary
  extension block that this decoder does not read. A DTS:X master set is
  therefore fine for layout inspection and useless for anything that
  measures movement.
- **ffmpeg cannot open the 6-channel CAF files this writer produces.**
  12-channel files are fine. This predates the rename and is not a
  regression — the reference implementation emits byte-identical headers
  that ffmpeg rejects the same way.
- **`--presentation` is TrueHD-only** and is accepted-then-ignored for
  the other codecs rather than rejected.
