# E-AC3 bridge patch notes

Date: 2026-05-08

Context:
- Live PipeWire IEC61937 extraction was producing many E-AC3 packets, but playback was mostly noise.
- Some captured reject dumps were 1792-byte E-AC3 syncframes (`bsid=16`, 48 kHz, 5.1 core).
- A subset of dumps contained JOC/OAMD metadata and could expose 15 active objects.
- Existing E-AC3 tracks that previously worked later regressed with the same noisy output, so the patch set was treated as too intrusive and reverted.

## Corrections Tried

### Rejected-frame dumps

Added temporary diagnostics to dump rejected E-AC3 frames:
- `HARLETTY_DUMP_EAC3_REJECTS=N`
- `HARLETTY_DUMP_EAC3_SHORT_PACKET=1`

Why:
- Needed real failing access units rather than logs only.
- The logs showed many packets queued but few useful decoded frames.

Result:
- Useful. The dumps confirmed the IEC61937 packet size was usually coherent (`1792` bytes), so the main issue was not only SPDIF extraction.
- The dump path should be kept as a diagnostic tool, but not mixed with broad decode changes.

### Aux/JOC extraction fallback

Tried making aux skip-field walking more tolerant and falling back to frame scanning for EMDF/JOC blocks.

Why:
- Some frames showed JOC/OAMD only through a broad scan, while regular aux extraction missed them.

Result:
- Helped specific dumps: JOC/OAMD was detected in several frames and object decode could report 15 active objects.
- Risk: broad frame scanning can create false positives or hide parser desynchronization. This needs regression coverage across known-good E-AC3 tracks before landing.

### E-AC3 SPX parsing

Tried aligning spectral extension parsing with FFmpeg:
- Parse `dst_start_freq`.
- Use E-AC3 default SPX band structure.
- Decode SPX coordinates as blend + master + per-band gains.
- Avoid applying coupling-coordinate math directly to SPX.

Why:
- The old SPX path was suspicious and could produce high-frequency garbage on E-AC3/JOC material.

Result:
- Some dump amplitudes improved, but this area is too risky without a corpus. SPX is common in working E-AC3 streams, so changes here must be isolated and tested against both failing and known-good tracks.

### Coupling coordinate scaling

Tried changing the `exp == 15` coupling-coordinate case to match FFmpeg-style scaling.

Why:
- Several dumps had full-block coupling and implausibly large decoded PCM.

Result:
- Plausible as a bug fix, but not enough alone to solve the issue. Should be retested independently.

### SNR / fast-gain parsing

Tried adjusting E-AC3 SNR and fast-gain syntax handling.

Why:
- A one-bit desync before mantissas can turn the whole frame into noise.

Result:
- This is high risk. One attempted change improved aux alignment for some dumps but also changed behavior for other frames. Do not reland without side-by-side tests on known-good E-AC3.

### IMDCT output scaling

Changed IMDCT overlap output scaling from `2.0` to `1.0 / 32.0`.

Why:
- The Rust FFT backend is unnormalized, and decoded dump amplitudes were roughly tens of times larger than FFmpeg output.

Result:
- Strong signal on the failing dumps: max amplitude dropped from values like `20..65` to below `1.0`.
- Regression report says existing E-AC3 tracks now also produce bad sound, so this cannot be accepted as-is. It may be compensating for another scale mismatch or exposing assumptions elsewhere.

### PCM plausibility guard

Tried normalizing overrange E-AC3 PCM instead of substituting silence.

Why:
- Silence made playback appear to stop after a few frames.

Result:
- Bad tradeoff. If the PCM is parser garbage, normalization makes the garbage audible. For future work, overrange PCM should be treated as a decoder failure unless the underlying decode bug is understood.

## Revert Decision

The patch set touched too many sensitive parts at once:
- Bitstream block parsing.
- SPX.
- Coupling.
- SNR / fast gain.
- IMDCT scaling.
- Runtime fallback behavior.

Because existing E-AC3 tracks regressed, revert the code changes and keep only this note. Future patches should be small, independently testable, and validated against both:
- The failing E-AC3/JOC dumps.
- A known-good E-AC3 regression corpus.

## Required Future Test Discipline

Before landing any E-AC3 decode patch:
- Run unit tests: `cargo test -p eac3`.
- Run bridge tests: `cargo test`.
- Decode at least one known-good E-AC3 track and verify it is not noisy.
- Decode at least one failing dump and compare PCM max amplitude against FFmpeg.
- Verify JOC/OAMD detection separately from PCM audio quality.
- Avoid normalizing implausible PCM as a substitute for fixing parser or transform bugs.

Recommended next step:
- Build a small fixture corpus from known-good E-AC3 frames plus failing dumps.
- Add tests that check parser alignment, JOC payload presence, and conservative PCM amplitude bounds.
- Reapply candidates one at a time, starting with diagnostics and non-invasive fixtures.
