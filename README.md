# harletty-bridge

**The Dolby TrueHD / E-AC-3 (Atmos) decoder for the
[Omniphony](https://github.com/mgth/Omniphony) renderer.**

In plain terms: this is the piece that lets Omniphony (and
[mpv-omniphony](https://github.com/mgth/mpv-omniphony)) actually *play*
a TrueHD or Atmos soundtrack with real 3D object positioning, instead
of the flat stereo/5.1 downmix you normally get. It turns the encoded
audio in your movie file into sound + the spatial information the
renderer needs to place each object in space.

It is a **plugin**: you don't run it on its own. You install
Omniphony or mpv-omniphony, drop this file next to it, and tell the
config where it is. That's the whole job.

> **Looking for the command-line converter instead?** This repo also
> ships **`harletty`** — a fork of
> [`truehdd`](https://github.com/truehdd/truehdd) by
> [Rainbaby](https://github.com/truehdd), extended with E-AC-3 JOC and
> DTS input — which turns those bitstreams into Dolby Atmos master files
> on disk. That one you *do* run yourself; see
> **[docs/harletty-cli.md](docs/harletty-cli.md)** for the command
> reference and the provenance in detail.

[![mpv-omniphony — mpv playing a TrueHD Atmos stream rendered by liborender, supervised by Omniphony Studio](https://github.com/mgth/mpv-omniphony/raw/main/mpv-omniphony-1200.png)](https://github.com/mgth/mpv-omniphony)

*mpv-omniphony decoding a TrueHD Atmos track through this bridge, with
Omniphony Studio attached for live object visualization.*

---

## Install

You need **two minutes** and a working Omniphony or mpv-omniphony
install. You do **not** need to compile anything — grab the prebuilt
file for your system from the
[**releases page**](https://github.com/harletty/harletty-bridge/releases)
and follow the three steps for your OS below.

> Each release has one bridge file per system. Download the one that
> matches (the `harletty-cli-*` archives alongside them are the separate
> [offline tool](docs/harletty-cli.md) — not needed for playback):
>
> | Your system | File to download |
> |---|---|
> | **Windows** | `harletty_bridge.dll` |
> | **Linux** | `libharletty_bridge.so` |
> | **macOS** | `libharletty_bridge.dylib` |

### 🪟 Windows

Download **`harletty_bridge.dll`** from the releases page, then pick
**one** of the two options below.

**Option A — drop it next to `orender.exe` (recommended, no config).**
The host automatically loads any `*_bridge.dll` sitting in its own
folder, so there's nothing else to set up.

1. Find that folder: right-click your Omniphony / mpv-omniphony
   shortcut → *Open file location*; or search `orender.exe` in the
   Start menu, right-click the result → *Open file location*.
2. Drop `harletty_bridge.dll` into that folder. Keep the file name as
   is (auto-detection needs the `_bridge.dll` ending).

Done — skip to "Check it worked" below.

**Option B — keep it in a folder of your choice (needs one config line).**

1. Put the file somewhere permanent, e.g. create `C:\Omniphony\` and
   drop it in → `C:\Omniphony\harletty_bridge.dll`.
2. Tell Omniphony where it is. Open (or create) the file
   `%APPDATA%\omniphony\config.yaml` — paste that into the address bar
   of Explorer to find the folder — and make sure it contains the full
   path to the file:

   ```yaml
   render:
     bridge_path: C:\Omniphony\harletty_bridge.dll
   ```

That's it. Start mpv-omniphony or Omniphony Studio and your Atmos
tracks now render in 3D.

### Check it worked

Play any TrueHD/Atmos file:

- **mpv** —

  ```sh
  mpv --ad=orender --ad-orender-osc input.mkv
  ```

  `--ad=orender` is what switches mpv over to object rendering for this
  file. It's **opt-in**: without it, mpv plays the track normally
  (FFmpeg downmix) and the bridge is never used — so if you hear sound
  but it's flat, you forgot this flag.

  `--ad-orender-osc` forces the OSC broadcast on for this run so
  Studio can attach (otherwise OSC follows `render.osc` in the config).
  It's optional for plain playback, but **required the first time you
  set things up through Studio** — that's how Studio sees the stream and
  the live 3D view.

- **CLI** — `orender --input film.mlp ...`
- **Studio** — start it and it shows the objects move in 3D as soon as
  an OSC-enabled host (mpv with `--ad-orender-osc`, or the CLI) is
  playing.

If the bridge isn't found, the host falls back to the normal
(non-object) audio and the config save log / Studio status will say so
— double-check the `bridge_path` points at the file you downloaded.

### 🐧 Linux

1. Download **`libharletty_bridge.so`** from the releases page.
2. Put it somewhere permanent, e.g.
   `~/.local/lib/harletty/libharletty_bridge.so`.
3. Edit `~/.config/omniphony/config.yaml` (create it if missing) so it
   contains:

   ```yaml
   render:
     bridge_path: /home/you/.local/lib/harletty/libharletty_bridge.so
   ```

   (Or drop the file next to the `orender` binary and skip this step —
   the host auto-loads any `*_bridge.so` in its own folder.)

Then verify it with the [Check it worked](#check-it-worked) steps above.

> **Arch users:** there's nothing to download by hand — the bridge is on
> the AUR as [`harletty-bridge`](https://aur.archlinux.org/packages/harletty-bridge):
>
> ```sh
> paru -S harletty-bridge
> ```
>
> It builds from this repo's release and lands at
> `/usr/lib/orender/libharletty_bridge.so`. Hosts installed system-wide
> (`/usr/bin/orender`, the AUR `mpv-omniphony`/`omniphony-studio`) don't
> scan that directory, so point them at it once in
> `~/.config/omniphony/config.yaml`:
>
> ```yaml
> render:
>   bridge_path: /usr/lib/orender/libharletty_bridge.so
> ```

### 🍎 macOS

1. Download **`libharletty_bridge.dylib`** from the releases page.
2. Put it somewhere permanent, e.g.
   `~/Library/Application Support/omniphony/libharletty_bridge.dylib`.
3. Edit `~/.config/omniphony/config.yaml` (create it if missing) so it
   contains:

   ```yaml
   render:
     bridge_path: /Users/you/Library/Application Support/omniphony/libharletty_bridge.dylib
   ```

   (Or drop the file next to the `orender` binary and skip this step —
   the host auto-loads any `*_bridge.dylib` in its own folder.)

Then verify it with the [Check it worked](#check-it-worked) steps above.

---

## How it works (the technical bit)

`harletty-bridge` decodes raw or IEC61937-wrapped access units into PCM
plus OAMD spatial metadata and hands them to `liborender` over a stable
`abi_stable` ABI
([`bridge_api`](https://github.com/mgth/Omniphony/tree/main/omniphony-renderer/bridge_api)).

The renderer side is format-agnostic on purpose: `harletty-bridge` is
the only piece in the stack that names the specific input formats. Plug
in a different bridge and `liborender` will happily render whatever else
feeds it OAMD-shaped metadata.

Step by step:

1. Receives raw TrueHD or E-AC-3 (JOC) access units from the host
   (the `orender` CLI, or `ad_orender` inside mpv), either as raw
   payload or as IEC61937 frames (PipeWire encoded sinks).
2. Decodes them with the bundled `truehd` / `eac3` crates.
3. Parses OAMD metadata into the renderer-neutral shape described by
   `bridge_api` (per-object positions, gains, sizes, channel labels,
   …) and emits PCM in parallel.
4. The renderer takes care of VBAP, distance / spread modeling and the
   speaker-side output.

Architecturally the bridge is a runtime `dlopen` plugin — the exact
same loading pattern Omniphony uses for any future format-specific
bridge (`*_bridge.so` / `.dll` / `.dylib`).

### Related projects

| Repo | Role |
|---|---|
| [`Omniphony`](https://github.com/mgth/Omniphony) | The renderer (`liborender` C library, `orender` CLI, `omniphony-studio` 3D supervision UI). Loads this bridge at runtime. |
| [`mpv-omniphony`](https://github.com/mgth/mpv-omniphony) | mpv patched with the `ad_orender` audio decoder; embeds `liborender` so mpv plays Atmos with full object rendering instead of FFmpeg's downmix. |

## Build from source

Only needed if you want to hack on the bridge or there's no prebuilt
artifact for your platform. Requires a Rust toolchain.

```sh
./build_bridge.sh       # Linux / macOS / MSYS — runs `cargo build --release`
```

```cmd
build_bridge.bat        :: Windows native
```

The build produces `target/release/libharletty_bridge.{so,dylib}` on
unix and `target\release\harletty_bridge.dll` on Windows. Point
`bridge_path` at that file exactly as in the install steps above.

## The offline `harletty` CLI

**`harletty` is a fork of [`truehdd`](https://github.com/truehdd/truehdd)
by [Rainbaby](https://github.com/truehdd)** — 85% of the shared-lineage
code is byte-identical to upstream, including the whole DAMF master-set
writer and the CAF/Wave64 writers. What was added here is E-AC-3 JOC and
DTS/DTS:X input, the latter exported as ADM.

It turns those bitstreams into Dolby Atmos master files (`.atmos`,
`.atmos.metadata`, plus CAF/WAV audio), reading a file or stdin, so it
pipes straight out of ffmpeg:

```sh
ffmpeg -i movie.mkv -map 0:a:0 -c copy -f truehd - \
  | harletty --progress decode - --output-path "movie"
```

📖 **[docs/harletty-cli.md](docs/harletty-cli.md) — commands, every
option, output files, recipes and limitations.**

Grab `harletty-cli-<version>-<system>.zip` from the
[releases page](https://github.com/harletty/harletty-bridge/releases),
or build it with `cargo build --release -p harletty`.

The rename is not a claim of authorship — it exists because the binary
accepts a superset of upstream's inputs and writes labels upstream does
not, so two binaries called `truehdd` on one `PATH` would be a miserable
thing to debug. See
[docs/truehdd-fork-retirement.md](docs/truehdd-fork-retirement.md) for
the audit of what was ported, superseded or imported.

It is also a *separate artifact*: the bridge does not link it, does not
pay for it, and cannot reach it. That isolation is by crate graph rather
than feature flags, and `scripts/check-crate-isolation.sh` asserts it in
CI — if you find yourself wanting `use damf::…` inside `bridge/`, the
mapping you want belongs on the CLI side instead.

## Layout

The repo is a virtual cargo workspace — no package at the root. It
builds two artifacts from one decoder lineage.

```
bridge/              # bridge entry points + transport (raw / IEC61937) glue
harletty/            # offline CLI: decode/info, codec probing, codec->OAMD
damf/                # DAMF metadata + CAF/WAV writers (CLI-side only)
truehdd-macros/      # proc macros used by the CAF writer and `info`
truehd/              # TrueHD decoder crate (vendored, Apache-2.0)
eac3/                # E-AC-3 (JOC) decoder crate
dca/                 # DTS (core / DTS-HD MA / XLL) decoder crate
docs/                # protocol notes (IEC61937, OAMD shape, …)
EAC3_PATCH_NOTES.md  # upstream patches to the E-AC-3 decoder
OBJECT_SIZE_NOTES.md # notes on OAMD object_size handling
.github/workflows/   # CI: release builds (Linux + Windows) on `v*` tags
```

## Credits

The hard part of this project — actually decoding a TrueHD bitstream —
is **not** our work, and neither is most of the offline CLI. Both come
from [**truehdd**](https://github.com/truehdd/truehdd) by
[**Rainbaby**](https://github.com/truehdd), a clean-room Rust parser and
decoder for Dolby TrueHD.

Concretely, what is Rainbaby's:

- **`truehd/`** — the TrueHD parser and decoder, vendored essentially
  unchanged. Without it this bridge would have nothing to hand to the
  renderer.
- **The `harletty` CLI**, which is a fork of upstream's own binary rather
  than something built alongside it. Of the 5470 lines with a shared
  lineage, **4655 (85%) are byte-identical to upstream**: the command
  structure, the DAMF master-set writer, the CAF and Wave64 writers, the
  `info` report and `truehdd-macros/` are all upstream work.

What is ours: the `bridge_api` ABI wrapper and the OAMD plumbing the
renderer needs; the `eac3` and `dca` crates; the E-AC-3 JOC and DTS/DTS:X
routing in the CLI; and decoder robustness fixes. Upstream is Apache-2.0,
as is this repo.

So: huge thanks to Rainbaby and the `truehdd` project. **If any of this
is useful to you, go star [`truehdd`](https://github.com/truehdd/truehdd).**
That is where the hard part was done.

## License

Apache-2.0. The vendored TrueHD decoder under `truehd/` ships its own
upstream `LICENSE` and remains © its original author.
