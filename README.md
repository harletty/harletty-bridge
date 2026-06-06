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

1. Download **`harletty_bridge.dll`** from the releases page.
2. Put it somewhere permanent. Two options, pick whichever suits you:
   - **Next to `orender.exe`** (simplest — no path to type later).
     To find that folder, right-click your Omniphony / mpv-omniphony
     shortcut → *Open file location*; or search `orender.exe` in the
     Start menu, right-click the result → *Open file location*. Drop
     the `.dll` in that same folder.
   - **In a folder of your choice**, e.g. create `C:\Omniphony\` and
     drop the file in it → `C:\Omniphony\harletty_bridge.dll`.
3. Tell Omniphony where it is. Open (or create) the file
   `%APPDATA%\omniphony\config.yaml` — paste that into the address bar
   of Explorer to find the folder — and make sure it contains the full
   path to the file you just placed:

   ```yaml
   render:
     bridge_path: C:\Omniphony\harletty_bridge.dll
   ```

   (If you dropped it next to `orender.exe`, use that path instead,
   e.g. `bridge_path: C:\Program Files\Omniphony\harletty_bridge.dll`.)

That's it. Start mpv-omniphony or Omniphony Studio and your Atmos
tracks now render in 3D.

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

> **Arch users:** there's nothing to copy by hand — install the
> [`omniphony-bridge`](https://github.com/mgth/Omniphony/tree/main/packaging/arch/omniphony-bridge)
> package and it lands at `/usr/lib/orender/omniphony_bridge.so`,
> which every renderer host picks up automatically.

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

### Check it worked

Play any TrueHD/Atmos file:

- **mpv** — `mpv --ad=orender film.mkv`
- **CLI** — `orender --input film.mlp ...`
- **Studio** — connect it over OSC to either of the above and you'll
  see the objects move in 3D.

If the bridge isn't found, the host falls back to the normal
(non-object) audio and the config save log / Studio status will say so
— double-check the `bridge_path` points at the file you downloaded.

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

## License

Apache-2.0. The vendored TrueHD decoder under `truehd/` ships its own
upstream `LICENSE`.
