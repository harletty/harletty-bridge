# Plan: one crate per codec family inside the bridge

Status: implemented on `refactor/codec-family-crates` (see Outcome at the end
for where it departs from the plan). Step 1 of a possible two-step split; step
2 (one plugin library per family) is out of scope here and only becomes
mechanical once this lands.

## Goal

Split the bridge's decode paths into one library crate per codec family, and
let the `harletty-bridge` cdylib pick which families it contains through Cargo
features — so that a bridge can be built from **the IAMF family alone**.

IAMF is an open, royalty-free AOMedia format. Today its pipeline only exists as
an add-on to the full bridge (`--features iamf`), which always contains every
other decoder too. After this change, a build such as

```
cargo build --release -p harletty-bridge --no-default-features --features iamf
```

produces a plugin whose crate graph holds no other decoder (`truehd`, `eac3`,
`dca`, `auro` absent), checked by CI rather than by convention.

### Non-goals

- No change to `bridge_api`, the plugin ABI, or anything in Omniphony. The host
  still loads one library through one `render.bridge_path`.
- No change to what the default build decodes or to the release artifacts:
  default features reproduce today's bridge exactly. Shipping an IAMF-only
  artifact is a follow-up (it needs libopus on each release platform, the
  reason `iamf` is off by default today).
- No change to the offline CLI (`harletty`), which has its own `iamf` feature.
- No behaviour change: decoded output must stay bit-identical and the hot path
  no slower (see Verification).

## Where things stand

One crate, `bridge/`, ~11.7 k lines. `AtmosBridge` (`bridge/src/bridge.rs`)
owns the state of every pipeline as `pub(crate)` fields and is also the codec
router: `sniff_raw_codec` / `configure("input_codec")` for the raw transport,
the IEC 61937 `data_type` for S/PDIF. Every pipeline function takes
`&mut AtmosBridge` and reaches into its fields.

What each pipeline touches today:

| Module | Own fields | Shared fields |
|---|---|---|
| `truehd_pipeline`, `mat` | `mat_stream`, `extractor`, `parser`, `decoder`, `truehd_*`, `current_substream_info`, `recovering_until_major_sync` | `strict`, `total_samples`, `frame_count`, `presentation`, `drc_mode`, `current_dialogue_level`, `declared_object_channels`, `perf` |
| `eac3_pipeline`, `eac3_spdif`, `ac3_native`, E-AC-3 presentation code in `bridge.rs` | `eac3_*`, `ac3_decoder`, `pending_eac3_core` | `strict`, `drc_mode`, `current_dialogue_level`, `declared_object_channels`, `perf` |
| `dts_pipeline`, `dts_spdif`, `auro_pipeline` | `dts_*` (incl. `dts_auro`, `dts_x`, `dts_fold_config`) | `strict`, `total_samples`, `declared_object_channels` |
| `iamf_pipeline` | `iamf: Box<IamfState>` (already self-contained) | `strict`, `total_samples`, `declared_object_channels` |

Cross-module links that decide the family boundaries:

- `metadata.rs` holds the OAMD → events code for TrueHD *and* E-AC-3 JOC, plus
  `declare_object_channels`, which DTS and IAMF use too.
- `labels.rs` mixes Dolby (`channel_label_to_r`, `bed_channel_to_r`,
  `oamd_speaker_to_label`), DTS (`dca_*`, `dts_declared_poses`) and Auro
  (`auro_stream_to_r`, which calls `auro_pipeline::auro_pose`).
- `auro_pipeline` calls `dts_pipeline::speaker_to_label`; the Auro carrier
  rides in DTS-HD lossless. Auro therefore belongs to the DTS family.
- `drc_mode`, `presentation` and `current_dialogue_level` are only read by the
  Dolby paths.
- `bridge/Cargo.toml` declares `spdif`, `sys` and `anyhow`, none of which
  `bridge/src` uses; `serde`/`serde_yaml_ng` serve only the DTS fold config.

## Target layout

```
bridge-common/        rlib — no decoder dependency
  FamilyPipeline trait, SharedState, frame_builders, logging, perf,
  declare_object_channels, the generic RChannelLabel helpers
bridge-family-dolby/  rlib — truehd, eac3
  truehd_pipeline, mat, eac3_pipeline, eac3_spdif, ac3_native,
  OAMD metadata, Dolby labels, the E-AC-3 presentation assembly now in bridge.rs
bridge-family-dts/    rlib — dca, auro, serde, serde_yaml_ng
  dts_pipeline, dts_spdif, auro_pipeline, DTS and Auro labels, fold config
bridge-family-iamf/   rlib — iamf-obu, iamf-dec, iamf-codecs
  iamf_pipeline
bridge/               cdylib + rlib "harletty-bridge" — the router only
  features: dolby, dts, iamf; default = ["dolby", "dts"]
```

Each family crate depends on `bridge_api`, `abi_stable`, `log` and
`bridge-common`, plus its own decoders — never on another family. The router
depends on each family crate optionally, behind the feature of the same name.

### The family interface

Statically dispatched: the router holds concrete types, not trait objects, and
matches on the active family once per packet, as it does today. Sketch:

```rust
// bridge-common
pub struct SharedState {
    pub strict: bool,
    pub total_samples: u64,
    pub declared_object_channels: Option<RVec<RObjectChannel>>,
}

pub trait FamilyPipeline: Default {
    /// Raw-transport detection on the first access unit after a reset.
    fn sniff(data: &[u8]) -> bool;
    /// IEC 61937 data types this family decodes (empty for IAMF).
    fn accepts_data_type(data_type: u8) -> bool;
    fn push_raw(&mut self, shared: &mut SharedState, data: &[u8], out: &mut RPushResult);
    fn push_iec61937(&mut self, shared: &mut SharedState, data: &[u8], data_type: u8, out: &mut RPushResult);
    fn reset(&mut self, shared: &SharedState);
    /// A decoder panicked: drop any state `reset` keeps (IAMF's sequence).
    fn discard_after_panic(&mut self) {}
    fn configure(&mut self, key: &str, value: &str) -> bool { false }
    fn is_ready(&self) -> bool;
    fn has_objects(&self) -> bool;
    fn source_family(&self) -> &'static str;
    fn source_label(&self, label: &mut String);
    fn fixed_channel_poses(&self) -> RVec<RChannelPose> { RVec::new() }
    fn channel_tags(&self) -> RVec<RChannelTag> { RVec::new() }
    fn declared_families(out: &mut RVec<RSourceFamily>);
}
```

`PerfStats` is not shared: under `bridge-perf` it records the TrueHD parser's
own statistics, so it would put `truehd` into `bridge-common`. It stays with
the router until the Dolby family takes it (step 6).

Dolby-only knobs (`presentation`, DRC) stay family state: the router forwards
`configure("presentation")` and `set_drc_mode` to the Dolby pipeline when it is
built, and `supported_drc_modes` is empty when it is not.

### The router

`AtmosBridge` keeps its name and becomes:

```rust
pub(crate) struct AtmosBridge {
    #[cfg(feature = "dolby")] dolby: Box<DolbyPipeline>,
    #[cfg(feature = "dts")]   dts:   Box<DtsPipeline>,
    #[cfg(feature = "iamf")]  iamf:  Box<IamfPipeline>,
    active: Option<Family>,          // replaces eac3_active / dts_active / iamf_active
    forced_raw_codec: Option<Codec>, // input_codec, persists across resets
    raw_codec: Option<Codec>,        // locked until reset
    refused: Option<Codec>,          // a codec this build lacks, reported once
    shared: SharedState,
}
```

- Boxing per family keeps the struct pointer-sized per field;
  `atmos_bridge_stack_footprint_stays_small` and
  `new_fits_on_a_macos_sized_playback_thread` keep guarding it.
- `RawCodec` stays the router's vocabulary and is recognised whatever the
  build: detection order is unchanged (IAMF sequence header, TrueHD major
  sync, DTS, E-AC-3). A detected codec whose family is not built is refused by
  name, once per stream — the existing `iamf_refusal_reported` behaviour,
  generalised to every family.
- The "unknown first packet means TrueHD" fallback only exists when `dolby` is
  built. In an IAMF-only build, an unrecognised packet is the continuation of
  the configured sequence if there is one, and dropped otherwise.
- `source_families()` is the union of the built families' declarations, so an
  IAMF-only plugin declares only `iamf`.
- `vbap_cartesian_defaults` and `preferred_vbap_table_mode` stay router
  constants (unchanged values in every build); making them per family is a
  separate question.
- The `push_packet` panic guard stays in the router and calls
  `discard_after_panic` on the active family before the reset.

## Steps

Each step is one commit (or a few) that builds, passes `cargo test` in the
default, `--features iamf` and `--features bridge-perf` configurations, and
leaves output bit-identical.

1. **Baseline.** Record decoded-output hashes and `bridge_bench` timings on the
   local corpus for every family (TrueHD, E-AC-3 JOC, AC-3, DTS core / HD MA /
   DTS:X, Auro carrier, IAMF) on `main`, to compare against at every step.
2. **Prune dependencies.** Drop the unused `spdif`, `sys` and `anyhow` from
   `bridge/Cargo.toml`.
3. **`SharedState` inside the crate.** Move `strict`, `total_samples` and
   `declared_object_channels` into one struct field; pipelines take it
   explicitly. Mechanical, no file moves yet.
4. **IAMF first** (smallest coupling): `push_iamf` takes
   `(&mut IamfState, &mut SharedState)` instead of `&mut AtmosBridge` and
   returns `AfterPush` (reset the pipeline or not) instead of resetting it;
   then the module moves to `bridge-family-iamf` and `bridge-common` is
   created with what it needs. The conformance tests that only use the host
   API become `bridge/tests/iamf_conformance.rs`; those that compare against
   a direct iamf-rs decode stay in the family crate, behind a harness that
   answers like the router. `bridge-family-iamf` is not a default workspace
   member, so a plain `cargo test` still needs no libopus.
5. **DTS + Auro**: gather `dts_*` fields into `DtsPipeline`, split the DTS and
   Auro parts out of `labels.rs`, move the fold config (and serde) along, then
   move the crate.
6. **Dolby**: gather TrueHD and E-AC-3 fields into `DolbyPipeline`, moving the
   E-AC-3 presentation assembly (`process_eac3_access_unit`,
   `resolve_pending_presentation`, `drain_eac3_raw`, …) out of `bridge.rs` and
   the OAMD half of `metadata.rs` with it; then move the crate. The largest
   step; it can be split TrueHD / E-AC-3 if the diff gets unreviewable.
7. **Features in the router**: `dolby`, `dts`, `iamf`, default
   `["dolby", "dts"]`; cfg-gate the router, generalise the refusal, build the
   family catalogue from what is compiled in. Router tests (sniffing,
   `configure`, panic guard, source family) gate on the features they need.
8. **CI**:
   - build and test matrix: default, `--features iamf`, and
     `--no-default-features --features iamf`;
   - `cargo test -p` on each family crate on its own;
   - `scripts/check-crate-isolation.sh`: assert that
     `cargo tree -p harletty-bridge --no-default-features --features iamf -e normal`
     contains none of `truehd`, `eac3`, `dca`, `auro`, and that no family crate
     depends on another.
9. **Docs**: README build section (feature list, the IAMF-only build and its
   libopus requirement).

## Verification

- **Bit-exactness**: the step 1 hashes, re-run after every step. Any difference
  is a bug in the move, not a tolerance to accept.
- **Hot path**: `bridge_bench` per family, no regression beyond noise. The only
  added work per packet is passing `&mut SharedState`, and the match on the
  active family already exists.
- **Listening**: the default build in the integration stack (mpv and live
  S/PDIF input) on one stream per family, including a mid-stream codec switch
  on the live input, before merging.
- **IAMF-only build**: loads in orender, declares only the `iamf` family,
  decodes the conformance streams, and refuses a TrueHD / E-AC-3 / DTS stream
  by name instead of failing silently.

## Risks and open points

- **Shared OAMD code between TrueHD and E-AC-3** stays inside the Dolby family
  by design; nothing outside it needs it.
- **Hidden coupling in tests**: many tests build an `AtmosBridge` to exercise a
  single pipeline. They move with their family and drive the pipeline directly;
  those that need the router stay in `bridge/` behind features.
- **Artifact naming for the follow-up**: orender's auto-discovery accepts any
  `*_bridge.{so,dll,dylib}`, so an IAMF-only artifact can ship under its own
  name (e.g. `libharletty_iamf_bridge.so`) without a host change.
- **Step 2 (one plugin per family)** would add a router on the host side, a
  multi-bridge config and N libraries to rebuild in lockstep with the host's
  `abi_stable` version. Not justified by the IAMF-only goal, which this step
  meets on its own.

## Outcome

Done as planned, output bit-identical at every step (51 streams, raw and
IEC 61937, `pcm_hash` and `host_hash`, default and IAMF builds). Where the
code departs from the sketch above:

- **No `FamilyPipeline` trait.** Each family exposes the same set of
  inherent methods (`push_raw` / `push_iec61937`, `reset`, `is_ready`,
  `has_objects`, `source_family`, `source_label`, poses, tags) and the router
  calls them directly. A trait only earns its keep with step 2.
- **The router keeps two flags** (`dts_active`, `iamf_active`) rather than
  an `active: Option<Family>`: they record the codec of the last packet,
  decoded or refused, which is what the default build already reported for a
  refused IAMF stream. Dolby is the case where neither is set.
- **Families are held inline**, their decoders boxed inside them as before,
  not boxed per family: the struct stays within the footprint test's budget
  and the hot path gains no pointer hop.
- **`PerfStats` belongs to the Dolby family** from the start of the move
  (step 4), not to `SharedState`, for the reason given above.
- **Tests:** the Dolby tests that pushed packets through the whole bridge
  drive the family through a test harness with the same fields; the router
  keeps what it decides itself, gated on the families each test needs.
- **Also removed:** `spdif`, `sys`, `anyhow` and `libc`, all unused; the
  bundled DTS fold table moved to the DTS crate.

