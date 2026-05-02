# MAT stream reconstruction

This document describes the algorithm implemented in `src/mat.rs` to turn an IEC 61937 payload of type `0x16` into a sequence of usable MAT chunks.

Strictly speaking, "decoding" is not the best word here. This stage does not decompress audio. It mainly does three things:

- verify that the input really is a valid MAT frame
- remove MAT markers that are not part of the useful payload
- reassemble useful chunks, even when a chunk crosses a MAT marker

The goal is to produce the same binary stream that the next stage would see if the MAT markers were not present.

## Overview

Each IEC 61937 TrueHD burst contains one MAT frame with a fixed size:

- total size: `61424` bytes
- MAT start code: `20` bytes at the beginning
- MAT middle code: `12` bytes at offset `30708`
- MAT end code: `16` bytes at offset `61408`

Between these codes, the useful content is a sequence of chunks. Each chunk:

- starts with a `2` byte header
- uses that header to describe the chunk size
- may contain any byte value, including `00 00`
- may cross the middle code or the end code

So the parser must track two positions at the same time:

- the absolute position inside the MAT frame
- the current position inside the working buffer

These two positions must never drift apart.

## Input and output

Input:

- one complete IEC 61937 payload already extracted from the SPDIF burst
- this payload normally starts with the MAT start code

Output:

- zero, one, or more chunk fragments
- each fragment is already reordered in 16-bit words

The parser is incremental in the sense that `next_chunk()` is called multiple times after one `push_payload()`.

## General idea

The algorithm behaves like a small state machine:

1. wait for a payload
2. verify the MAT start code
3. read the rest of the frame as a sequence of chunks
4. skip MAT codes when the read position reaches them exactly
5. if a chunk crosses a MAT code, emit the part before the code, then resume the same chunk immediately after the code

## States

`WaitingForPayload`

- no current payload
- `next_chunk()` returns `None`

`VerifyingMatStart`

- a payload has just been received
- the first `20` bytes are checked against the MAT start code

`ReadingPayload`

- the parser is walking through the useful content
- it keeps:
  - `bytes_remaining`: how much is left in the frame
  - `mat_position`: absolute position of the next useful byte in the MAT frame
  - `middle_code_skipped`: whether the middle code has already been skipped
  - `end_code_skipped`: whether the end code has already been skipped

In addition to the main state, the parser keeps `pending_chunk_bytes`:

- `None` if no chunk is currently being continued
- `Some(n)` if a chunk was cut by a MAT code and `n` bytes still need to be emitted

## Step 1: verify the start code

When a new payload is received:

1. copy the payload into the internal buffer
2. verify that its first `20` bytes exactly match the MAT start code
3. if they do not match:
   - clear the current state
   - report an error
4. otherwise:
   - advance the cursor by `20` bytes
   - initialize `mat_position = 20`
   - switch to `ReadingPayload`

This check matters because all later offsets assume the frame starts correctly.

## Step 2: remove internal MAT codes

While reading the payload:

- if `mat_position == 30708`, check for the middle code
- if `mat_position == 61408`, check for the end code

If the expected code is present:

- advance the cursor by the code length
- advance `mat_position` by the same amount
- subtract the same amount from `bytes_remaining`
- mark that code as already skipped

Critical rule:

- a code is skipped only if `mat_position` is exactly at that code position
- the parser must never drain a code that is only visible farther ahead in the buffer

In other words:

- the parser never removes bytes from the middle of the logical stream
- it only removes bytes that are exactly at the current read head

This invariant is what keeps all later offset calculations correct.

## Step 3: continue a chunk that was split by a MAT code

This is the most sensitive part.

If `pending_chunk_bytes` is set, it means:

- a chunk started earlier
- part of it has already been emitted
- the rest of that same chunk begins immediately after a MAT code

In that case, the continuation must be handled before any other logic.

More precisely:

1. compute how many bytes of that continuation can be read now
2. if another MAT code still lies inside that continuation, stop at that boundary
3. emit this fragment
4. update `pending_chunk_bytes`
5. if the chunk is still not complete, resume later after the next MAT code

Critical rule:

- the "strip leading `00 00` padding" logic must never run before a pending continuation is handled

Why:

- at the start of a continuation, `00 00` may be real chunk payload
- if those bytes are removed, the chunk becomes shifted and corrupted

This is not theoretical. It already caused a real regression.

## Step 4: ignore padding between chunks

When there is no pending continuation, the parser may encounter `00 00` words between chunks.

In that case:

- while the current bytes are `00 00`
- and the parser is not resuming a chunk continuation
- advance by `2` bytes

This padding is ignored only between chunks.

It must not be confused with:

- zeros that belong to a chunk
- zeros at the beginning of a continuation after a MAT code

## Step 5: read a new chunk header

When the parser is not in a continuation:

1. read `2` bytes
2. interpret them as a little-endian `u16`
3. keep the low `12` bits
4. multiply by `2`

Formula:

```text
chunk_size = (raw & 0x0FFF) << 1
```

If `chunk_size == 0`:

- treat the header as invalid
- advance by `2` bytes
- continue scanning

## Step 6: emit the chunk, possibly in several pieces

Once `chunk_size` is known:

1. compute how many bytes are available before the next MAT code
2. emit only that part
3. if the chunk is not complete:
   - store the remainder in `pending_chunk_bytes`
4. otherwise:
   - move on to the next chunk

This naturally handles:

- a chunk entirely before the middle code
- a chunk crossing the middle code
- a chunk crossing the end code
- a chunk that is split more than once

## Byte reordering

When the parser emits a chunk fragment, it swaps bytes in 2-byte pairs:

```text
[a, b, c, d] -> [b, a, d, c]
```

This is done by `copy_swapped_words()`.

The purpose is not to change the logical content of the chunk, but to restore the 16-bit word order expected by the next stage.

## Invariants that must hold

A correct reimplementation must preserve these rules:

1. `mat_position` must always represent the MAT position of the first unread byte.
2. Any real advance in the buffer must advance `mat_position` by the same amount, unless the whole state is reset.
3. A MAT code may be skipped only when the read position is exactly at its location.
4. A chunk continuation has priority over detecting a new chunk header.
5. `00 00` padding may be ignored only between chunks, never at the start of a continuation.
6. The chunk size always describes the full logical chunk, even if output is split into several fragments.

## Pseudocode

```text
push_payload(payload):
  verify the start code
  position = 20
  bytes_remaining = payload_size - 20
  pending_chunk_bytes = none

next_chunk():
  while data remains:
    if exactly at a MAT code:
      skip that code
      continue

    if pending_chunk_bytes exists:
      emit the chunk continuation immediately
      if chunk is still incomplete:
        remember the remaining bytes
      return that fragment

    ignore 00 00 padding words between chunks

    read the next chunk header
    compute chunk_size
    emit at most chunk_size bytes, without crossing a MAT code
    if chunk is incomplete:
      pending_chunk_bytes = remaining_size
    return that fragment

  return None
```

## Typical failure modes

When the algorithm is wrong, the usual symptoms are:

- zero-sized or invalid chunks
- a chunk starts correctly but becomes corrupted after the middle code
- expected `00 00` sequences disappear from the payload
- a continuation is read as if it were a new header
- the stream becomes shifted after a MAT code

In practice, this quickly leads to:

- very few or no decoded frames
- audio throughput below real time
- output buffers gradually draining

## How to validate a reimplementation

At minimum:

- one test where a chunk stays entirely before the middle code
- one test where a chunk crosses the middle code
- one test where the continuation begins with `00 00`
- one test where the end code is skipped correctly
- one test where a payload without a valid start code is rejected

It is also useful to compare:

- raw bytes captured from the pipe
- the reconstructed MAT stream
- and then verify that Linux and Windows produce the same binary output for the same capture length

## Summary

This algorithm does not really decode MAT. It reconstructs a clean binary stream from a MAT-encapsulated frame.

The core of the problem is simple:

- remove MAT codes without losing alignment
- never confuse inter-chunk padding with real payload
- resume a chunk correctly after a MAT code

If those three rules hold, the reconstructed stream stays stable and the next stage can process it normally.
