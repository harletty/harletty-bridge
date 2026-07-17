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

> Each release has one file per system. Download the one that matches:
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
  mpv --ad=orender --ad-orender-osc film.mkv
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

## Layout

```
src/                 # bridge entry points + transport (raw / IEC61937) glue
truehd/              # TrueHD decoder crate (vendored, Apache-2.0)
eac3/                # E-AC-3 (JOC) decoder crate
docs/                # protocol notes (IEC61937, OAMD shape, …)
EAC3_PATCH_NOTES.md  # upstream patches to the E-AC-3 decoder
OBJECT_SIZE_NOTES.md # notes on OAMD object_size handling
.github/workflows/   # CI: release builds (Linux + Windows) on `v*` tags
```

## Credits

The hard part of this project — actually decoding a TrueHD bitstream —
is **not** our work. The `truehd/` crate is vendored, essentially
unchanged, from [**truehdd**](https://github.com/truehdd/truehdd) by
[**Rainbaby**](https://github.com/truehdd), a clean-room Rust parser and
decoder for Dolby TrueHD. Without that decoder this bridge would have
nothing to hand to the renderer.

So: huge thanks to Rainbaby and the `truehdd` project. All the credit
for the TrueHD decode path belongs there; we just wrap it in the
`bridge_api` ABI and bolt on the OAMD plumbing the renderer needs. If
this plugin is useful to you, go star [`truehdd`](https://github.com/truehdd/truehdd)
too.

## License

Apache-2.0. The vendored TrueHD decoder under `truehd/` ships its own
upstream `LICENSE` and remains © its original author.
