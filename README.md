# harletty-bridge

`harletty-bridge` is the Dolby TrueHD / E-AC-3 (JOC, Atmos) decoder
plugin for the [Omniphony](https://github.com/mgth/Omniphony) renderer
and its [mpv-omniphony](https://github.com/mgth/mpv-omniphony)
integration. It decodes raw or IEC61937-wrapped access units into PCM
plus OAMD spatial metadata and hands them to `liborender` over a
stable `abi_stable` ABI ([`bridge_api`](https://github.com/mgth/Omniphony/tree/main/omniphony-renderer/bridge_api)).

The renderer side is format-agnostic on purpose: `harletty-bridge` is
the only piece in the stack that names the specific input formats.
Plug in a different `*_bridge.so` and `liborender` will happily render
whatever else feeds it OAMD-shaped metadata.

[![mpv-omniphony — mpv playing a TrueHD Atmos stream rendered by liborender, supervised by Omniphony Studio](https://github.com/mgth/mpv-omniphony/raw/main/mpv-omniphony-1200.png)](https://github.com/mgth/mpv-omniphony)

*mpv-omniphony decoding a TrueHD Atmos track through this bridge, with
Omniphony Studio attached over OSC for live object visualization.*

## Related projects

| Repo | Role |
|---|---|
| [`Omniphony`](https://github.com/mgth/Omniphony) | The renderer (`liborender` C library, `orender` CLI, `omniphony-studio` 3D supervision UI). Loads this bridge at runtime. |
| [`mpv-omniphony`](https://github.com/mgth/mpv-omniphony) | mpv patched with the `ad_orender` audio decoder; embeds `liborender` so mpv plays Atmos with full object rendering instead of FFmpeg's downmix. |

## What it does

1. Receives raw TrueHD or E-AC-3 (JOC) access units from the host
   (the `orender` CLI, or `ad_orender` inside mpv), either as raw
   payload or as IEC61937 frames (PipeWire encoded sinks).
2. Decodes them with the bundled `truehd` / `eac3` crates.
3. Parses OAMD metadata into the renderer-neutral shape described by
   `bridge_api` (per-object positions, gains, sizes, channel labels,
   …) and emits PCM in parallel.
4. The renderer takes care of VBAP, distance / spread modeling and
   the speaker-side output.

Architecturally the bridge is a runtime `dlopen` plugin — exact same
loading pattern Omniphony uses for any future format-specific bridge
(`*_bridge.so` / `.dll` / `.dylib`).

## Build

```sh
./build_bridge.sh       # Linux / macOS / MSYS — runs `cargo build --release`
```

```cmd
build_bridge.bat        :: Windows native
```

The build produces `target/release/libharletty_bridge.{so,dylib}` on
unix and `harletty_bridge.dll` on Windows.

Prebuilt artifacts for the matching tag are also published on the
[releases page](https://github.com/harletty/harletty-bridge/releases)
(Linux `.so` + Windows `.dll`).

## Use

`harletty-bridge` is not a standalone program — it's loaded by
`liborender`. Point the renderer at the built artifact via the shared
config:

```yaml
# ~/.config/omniphony/config.yaml
render:
  bridge_path: /path/to/libharletty_bridge.so
```

… or, packaged via [`omniphony-bridge`](https://github.com/mgth/Omniphony/tree/main/packaging/arch/omniphony-bridge),
installed at the conventional location:

```
/usr/lib/orender/omniphony_bridge.so
```

Then every renderer host picks it up automatically:

- **CLI** — `orender --input file.mlp ...`
- **mpv** — `mpv --ad=orender film.mkv`
- **Studio** — connects over OSC to either of the above.

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
