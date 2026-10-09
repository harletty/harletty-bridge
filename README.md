# harletty-bridge

**The Dolby TrueHD / E-AC-3 (Atmos) decoder for the
[Omniphony](https://github.com/mgth/Omniphony) renderer.**

In plain terms: this is the piece that lets Omniphony (and
[mpv-omniphony](https://github.com/mgth/mpv-omniphony)) actually *play*
a TrueHD or Atmos soundtrack with real 3D object positioning, instead
of the flat stereo/5.1 downmix you normally get. It turns the encoded
audio in your movie file into sound + the spatial information the
renderer needs to place each object in space.

It comes as **plugins**, one per codec family: you don't run them on
their own. You install Omniphony or mpv-omniphony, drop the three files
next to it (or tell the config where they are), and that's the whole
job.

> **Looking for the command-line converter instead?** This repo also
> ships **`harletty`** — a fork of
> [`truehdd`](https://github.com/truehdd/truehdd) by
> [Rainbaby](https://github.com/truehdd), extended with E-AC-3 JOC, DTS
> and Auro-3D input — which turns those bitstreams into Dolby Atmos
> master files on disk. That one you *do* run yourself; see
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

> Each release has one bridge archive per system,
> `harletty-bridge-<version>-<system>.zip`. It holds three plugins, one
> per codec family; install all three side by side (the
> `harletty-cli-*` archives alongside are the separate
> [offline tool](docs/harletty-cli.md) — not needed for playback):
>
> | Your system | Files in the archive |
> |---|---|
> | **Windows** | `harletty_dolby_bridge.dll`, `harletty_dts_bridge.dll`, `harletty_iamf_bridge.dll` |
> | **Linux** | `libharletty_dolby_bridge.so`, `libharletty_dts_bridge.so`, `libharletty_iamf_bridge.so` |
> | **macOS** | `libharletty_dolby_bridge.dylib`, `libharletty_dts_bridge.dylib`, `libharletty_iamf_bridge.dylib` |
>
> `dolby` decodes TrueHD and E-AC-3 / AC-3 (with Atmos objects), `dts`
> decodes DTS, DTS-HD, DTS:X and Auro-3D, `iamf` decodes IAMF (Eclipsa
> Audio). The host loads them together and hands each stream to the one
> that recognises it. On Linux the IAMF plugin uses the system's libopus
> (`libopus0` on Debian/Ubuntu, `opus` on Arch and Fedora); on Windows
> and macOS it is built in.
>
> **Upgrading from a single `harletty_bridge` file?** Delete it and put
> the three files in its place. A config whose `bridge_path` still names
> `libharletty_bridge.so` / `harletty_bridge.dll` /
> `libharletty_bridge.dylib` keeps working: the host loads the three
> family plugins from that folder instead, and the next Save writes them.

> **Match the bridges to your Omniphony.** A bridge loads only in an
> Omniphony built against the same `bridge_api` minor version, so each
> release says at the top of its notes which Omniphony it requires
> ("Requires Omniphony ≥ …"). The current `main` requires **Omniphony
> with `bridge_api` 0.6 that loads several bridges**: Omniphony `main`
> since [mgth/Omniphony#785](https://github.com/mgth/Omniphony/pull/785),
> and the first Omniphony release after 0.6.0. Older hosts refuse the
> plugins. When a host loads a plugin, its log shows what it was built
> from, e.g.
> `harletty-dolby-bridge 0.8.0 (bridge_api 0.6.0, Omniphony 25369b9f…)`.

### 🪟 Windows

Download **`harletty-bridge-<version>-windows-x86_64.zip`** from the
releases page and extract the three `.dll` files, then pick **one** of
the two options below.

**Option A — drop them next to `orender.exe` (recommended, no config).**
The host automatically loads every `*_bridge.dll` sitting in its own
folder, so there's nothing else to set up.

1. Find that folder: right-click your Omniphony / mpv-omniphony
   shortcut → *Open file location*; or search `orender.exe` in the
   Start menu, right-click the result → *Open file location*.
2. Drop the three `harletty_*_bridge.dll` files into that folder. Keep
   the file names as they are (auto-detection needs the `_bridge.dll`
   ending).

Done — skip to "Check it worked" below.

**Option B — keep them in a folder of your choice (needs a config entry).**

1. Put the files somewhere permanent, e.g. create `C:\Omniphony\` and
   drop them in → `C:\Omniphony\harletty_dolby_bridge.dll` and so on.
2. Tell Omniphony where they are. Open (or create) the file
   `%APPDATA%\omniphony\config.yaml` — paste that into the address bar
   of Explorer to find the folder — and make sure it lists the full
   path to each file:

   ```yaml
   render:
     bridge_paths:
       - C:\Omniphony\harletty_dolby_bridge.dll
       - C:\Omniphony\harletty_dts_bridge.dll
       - C:\Omniphony\harletty_iamf_bridge.dll
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

If a bridge isn't found, the host falls back to the normal
(non-object) audio and the config save log / Studio status will say so
— double-check that `bridge_paths` points at the files you downloaded.

### 🐧 Linux

1. Download **`harletty-bridge-<version>-linux-x86_64.zip`** from the
   releases page, and install libopus if it isn't already
   (`sudo apt install libopus0`, `sudo pacman -S opus`,
   `sudo dnf install opus`): the IAMF plugin uses it.
2. Extract the three `.so` files somewhere permanent, e.g.
   `~/.local/lib/harletty/`.
3. Edit `~/.config/omniphony/config.yaml` (create it if missing) so it
   contains:

   ```yaml
   render:
     bridge_paths:
       - /home/you/.local/lib/harletty/libharletty_dolby_bridge.so
       - /home/you/.local/lib/harletty/libharletty_dts_bridge.so
       - /home/you/.local/lib/harletty/libharletty_iamf_bridge.so
   ```

   (Or drop the files next to the `orender` binary and skip this step —
   the host auto-loads every `*_bridge.so` in its own folder.)

Then verify it with the [Check it worked](#check-it-worked) steps above.

> **Arch users:** there's nothing to download by hand — the bridge is on
> the AUR as [`harletty-bridge`](https://aur.archlinux.org/packages/harletty-bridge):
>
> ```sh
> paru -S harletty-bridge
> ```
>
> It builds from this repo's release and installs the plugins in
> `/usr/lib/orender/`, the system plugin folder the host searches last
> when no bridge is configured. A config that names them explicitly:
>
> ```yaml
> render:
>   bridge_paths:
>     - /usr/lib/orender/libharletty_dolby_bridge.so
>     - /usr/lib/orender/libharletty_dts_bridge.so
>     - /usr/lib/orender/libharletty_iamf_bridge.so
> ```

### 🍎 macOS

1. Download **`harletty-bridge-<version>-macos-arm64.zip`** from the
   releases page.
2. Extract the three `.dylib` files somewhere permanent, e.g.
   `~/Library/Application Support/omniphony/`.
3. Edit `~/.config/omniphony/config.yaml` (create it if missing) so it
   contains:

   ```yaml
   render:
     bridge_paths:
       - /Users/you/Library/Application Support/omniphony/libharletty_dolby_bridge.dylib
       - /Users/you/Library/Application Support/omniphony/libharletty_dts_bridge.dylib
       - /Users/you/Library/Application Support/omniphony/libharletty_iamf_bridge.dylib
   ```

   (Or drop the files next to the `orender` binary and skip this step —
   the host auto-loads every `*_bridge.dylib` in its own folder.)

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

Architecturally each bridge is a runtime `dlopen` plugin — the exact
same loading pattern Omniphony uses for any format-specific bridge
(`*_bridge.so` / `.dll` / `.dylib`). harletty ships one per codec
family; the host loads them together, asks each one's `probe` where a
stream of its family starts, and routes each stream to the one that
claims it
([docs/multi-bridge.md](https://github.com/mgth/Omniphony/blob/main/docs/multi-bridge.md)
in Omniphony).

### Related projects

| Repo | Role |
|---|---|
| [`Omniphony`](https://github.com/mgth/Omniphony) | The renderer (`liborender` C library, `orender` CLI, `omniphony-studio` 3D supervision UI). Loads this bridge at runtime. |
| [`mpv-omniphony`](https://github.com/mgth/mpv-omniphony) | mpv patched with the `ad_orender` audio decoder; embeds `liborender` so mpv plays Atmos with full object rendering instead of FFmpeg's downmix. |

## Build from source

Only needed if you want to hack on the bridge or there's no prebuilt
artifact for your platform. Requires a Rust toolchain **and a checkout of
[Omniphony](https://github.com/mgth/Omniphony) next to this repository, as
`../Omniphony`**: the bridge crates take `bridge_api` (and, for the
bench, `spdif`) from it by path. [`.omniphony-ref`](.omniphony-ref) names the
Omniphony commit CI and the releases build against; check that one out to
build what a release would:

```sh
git clone https://github.com/harletty/harletty-bridge
git clone https://github.com/mgth/Omniphony
git -C Omniphony checkout "$(harletty-bridge/scripts/omniphony-ref.sh)"
cd harletty-bridge
```

Then:

```sh
./build_bridge.sh       # Linux / macOS / MSYS — builds the three plugins
```

```cmd
build_bridge.bat        :: Windows native
```

The build produces `target/release/libharletty_{dolby,dts,iamf}_bridge.{so,dylib}`
on unix and `target\release\harletty_{dolby,dts,iamf}_bridge.dll` on
Windows. Point `bridge_paths` at those files exactly as in the install
steps above.

The IAMF plugin links libopus. On Linux it takes the system's through
pkg-config (install `libopus-dev` or your distribution's equivalent). On
Windows and macOS, `build_bridge` builds libopus from source first and
links it in statically (`scripts/build-static-opus.sh`, CMake needed), as
the release does; set `OPUS_LIB_DIR` (and `OPUS_STATIC=1`) to use one of
your own instead.

Each library records the Omniphony commit it was built against (`git
rev-parse HEAD` in `../Omniphony`, or `HARLETTY_OMNIPHONY_COMMIT` when set,
`unknown` when neither is available) and logs it with its own version and
the `bridge_api` version when a host loads it.

### One plugin per codec family

Each codec family is a crate of its own, and each has a plugin crate that
makes it a bridge library:

| Plugin (package) | Library | Decodes | IEC 61937 burst types |
|---|---|---|---|
| `harletty-dolby-bridge` | `harletty_dolby_bridge` | TrueHD (raw and MAT), E-AC-3 / AC-3 with JOC objects | 0x01, 0x15, 0x16 |
| `harletty-dts-bridge` | `harletty_dts_bridge` | DTS, DTS-HD, DTS:X, Auro-3D over DTS-HD MA | 0x0B–0x0D, 0x11 |
| `harletty-iamf-bridge` | `harletty_iamf_bridge` | IAMF (Eclipsa Audio) | none |

Build any of them on its own, e.g. `cargo build --release -p
harletty-iamf-bridge`. A plugin's crate graph holds its own family's
decoders and no other's, which CI checks
(`scripts/check-crate-isolation.sh`).

`bridge/` is the combined bridge, every family behind one router, kept as
a library for the fuzz target, `bridge_bench` and the bit-exactness kit;
it is no longer shipped. Its Cargo features (`dolby`, `dts`, `iamf`) pick
the families it holds.

## The offline `harletty` CLI

**`harletty` is a fork of [`truehdd`](https://github.com/truehdd/truehdd)
by [Rainbaby](https://github.com/truehdd)** — 85% of the shared-lineage
code is byte-identical to upstream, including the whole DAMF master-set
writer and the CAF/Wave64 writers. What was added here is E-AC-3 JOC and
DTS/DTS:X input, the latter exported as ADM — on a DTS-HD MA carrier and on
a lossy DTS-HD HRA one ([docs/dtsx-lossy-carrier.md](docs/dtsx-lossy-carrier.md)) —
the unfolding of Auro-3D carriers (a DTS-HD MA track whose low bits
hold a folded 9.1 to 13.1 layout) into their bed and height layer, and
IAMF input (v2.0 objects and v1.1 beds, in builds with the `iamf` feature).

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
or build it with `cargo build --release -p harletty` (add `--features iamf`
for IAMF input, which links the system libopus).

The rename is not a claim of authorship — it exists because the binary
accepts a superset of upstream's inputs and writes labels upstream does
not, so two binaries called `truehdd` on one `PATH` would be a miserable
thing to debug. See
[docs/truehdd-fork-retirement.md](docs/truehdd-fork-retirement.md) for
the audit of what was ported, superseded or imported.

It is also a *separate artifact*: the bridges do not link it, do not
pay for it, and cannot reach it. That isolation is by crate graph rather
than feature flags, and `scripts/check-crate-isolation.sh` asserts it in
CI — if you find yourself wanting `use damf::…` inside a bridge crate,
the mapping you want belongs on the CLI side instead.

## Layout

The repo is a virtual cargo workspace — no package at the root. It
builds the bridge plugins and the CLI from one decoder lineage.

```
plugin-dolby/        # the Dolby bridge plugin (harletty_dolby_bridge)
plugin-dts/          # the DTS bridge plugin (harletty_dts_bridge)
plugin-iamf/         # the IAMF bridge plugin (harletty_iamf_bridge)
bridge-common/       # what the families share: FamilyPipeline, PluginBridge, probe helpers, root module
bridge/              # the combined bridge (every family behind one router), for fuzz / bench / kit
bridge-family-dolby/ # TrueHD (raw / MAT) and E-AC-3 / AC-3 JOC paths
bridge-family-dts/   # DTS, DTS-HD, DTS:X and Auro-3D paths
bridge-family-iamf/  # IAMF path (iamf-rs)
harletty/            # offline CLI: decode/info, codec probing, codec->OAMD
damf/                # DAMF metadata + CAF/WAV writers (CLI-side only)
truehdd-macros/      # proc macros used by the CAF writer and `info`
truehd/              # TrueHD decoder crate (vendored, Apache-2.0)
eac3/                # E-AC-3 (JOC) decoder crate
dca/                 # DTS (core / XXCH / DTS-HD MA / XLL / DTS:X) decoder crate
auro/                # Auro-Codec side channel: detection and unfold
docs/                # protocol notes (IEC61937, OAMD shape, …)
tools/host-baseline/ # bit-exactness through Omniphony's own bridge router
EAC3_PATCH_NOTES.md  # upstream patches to the E-AC-3 decoder
OBJECT_SIZE_NOTES.md # notes on OAMD object_size handling
.omniphony-ref       # the Omniphony commit CI and releases build against
.github/workflows/   # CI on pushes and pull requests; release builds on `v*` tags
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
renderer needs; the `eac3`, `dca` and `auro` crates; the E-AC-3 JOC,
DTS/DTS:X and Auro-3D routing in the CLI; and decoder robustness fixes. Upstream is Apache-2.0,
as is this repo.

So: huge thanks to Rainbaby and the `truehdd` project. **If any of this
is useful to you, go star [`truehdd`](https://github.com/truehdd/truehdd).**
That is where the hard part was done.

## License

The sources in this repository are **Apache-2.0**. The two things it builds
are not distributable under the same terms, so they are worth separating:

- **`harletty`, the CLI** — Apache-2.0. It depends only on this workspace, on
  crates.io and on our fork of `truehd` (`truehd` and `truehdd-macros` are
  Apache-2.0 too), so the binary carries no copyleft.
- **`libharletty_{dolby,dts,iamf}_bridge.so` / `.dll`, the decoder
  bridges** — **GPL-3.0-or-later**. It links `bridge_api` from
  [Omniphony](https://github.com/mgth/Omniphony), which is GPL-3.0-or-later,
  and the resulting library is a combined work. Apache-2.0 code may be
  combined into a GPLv3 work, so there is no licence conflict — but what you
  receive is governed by the GPL, and linking it into a proprietary program is
  not permitted. Source for both halves is public.

Note that this is one-way: Apache-2.0 is incompatible with GPLv2 because of
its patent clause, so anything built on `truehd` can be GPLv3 but never GPLv2.

The TrueHD decoder itself is the `truehd` crate (Apache-2.0), © its original
author; it used to be vendored here and no longer is. It is built from
[our fork](https://github.com/harletty/truehdd), pinned in the root
`Cargo.toml`, which carries a decode-speed series on top of upstream.
