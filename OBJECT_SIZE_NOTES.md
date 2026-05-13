# E-AC-3 OAMD `object_size` Findings

## Summary

Investigation of missing `object size` display in Studio showed that the decode and transport path is functioning correctly for the tested E-AC-3 stream.

The stream carries valid OAMD payloads, the object element is parsed correctly, and the `size` field is present in render-info blocks. For the captured timecodes, the parsed value is explicitly:

- `size = [0.0, 0.0, 0.0]`

This means the stream is signalling point sources at those instants, not a non-zero object extent.

## What Was Confirmed

- OAMD payloads are present in EMDF and parse successfully.
- The OAMD object element (`element_index == 1`) is recognized and decoded.
- Render info blocks include the `size` field (`render_blocks = Some(15)` in the captured frames).
- The parsed `size` value is preserved through mapping into `REvent.size`.
- The missing Studio gauge activity for the tested stream was not caused by:
  - Studio UI loss
  - OSC/runtime transport loss
  - bridge mapping loss
  - missing OAMD payloads

## Interpretation

For the investigated stream and timecodes, `object_size` is not absent; it is present and zero on the object set currently exposed by `harletty-bridge`.

The most plausible explanation is not that Dolby Atmos Renderer is showing some unrelated parameter. The working assumption is instead:

- the circle shown by Dolby does correspond to `object size`
- but it is likely rendered from an upstream / internal renderer representation
- that representation appears to expose many more objects than the final object set currently surfaced by this bridge

Observed clue:

- Dolby Atmos Renderer exposes about `103` objects in the relevant view
- the decoded OAMD payload inspected here exposes `16` objects total, with `15` dynamic objects

So the likely mismatch is not "wrong parameter", but "different object domain / stage of the rendering pipeline".

In other words, the `object size` seen in Dolby may belong to intermediate or renderer-internal objects that are not the same 15 dynamic objects currently propagated through this bridge path.

## Permanent Diagnostics Left In Place

Two minimal logs remain in `harletty-bridge`:

- `[harletty][object-size] non-zero detected ...`
  - emitted only when mapped object size is non-zero
- `[harletty][oamd] visual attrs ...`
  - emitted only when OAMD visual-ish attributes are non-default:
    - `distance`
    - `screen_factor`
    - `depth_factor`
    - `anchor != Room`

These are intended as low-noise diagnostics for future streams.
