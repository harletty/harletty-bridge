#!/usr/bin/env python3
"""Compare two host-baseline runs, stream by stream.

    compare.py <reference.jsonl> <run.jsonl> [--per-push]

Fails (exit 1) when a stream's output differs: frames, samples, errors,
pcm_hash, frame_hash, description_hash, and host_hash with --per-push (only
meaningful when both runs cut the pushes alike).
"""
import json, sys

args = [a for a in sys.argv[1:] if not a.startswith("--")]
keys = ["frames", "samples", "errors", "pcm_hash", "frame_hash", "description_hash"]
if "--per-push" in sys.argv:
    keys.append("host_hash")
load = lambda p: {(r["transport"], r["input"]): r for r in map(json.loads, open(p))}
ref, new = load(args[0]), load(args[1])
bad = 0
for k in sorted(ref.keys() | new.keys()):
    if k not in ref or k not in new:
        print("MISSING", k); bad += 1; continue
    diff = [f for f in keys if ref[k][f] != new[k][f]]
    if diff:
        bad += 1
        print("DIFF", k[0], k[1], ", ".join(f"{f}: {ref[k][f]} -> {new[k][f]}" for f in diff))
print(f"{len(ref)} streams, " + ("IDENTICAL" if bad == 0 else f"{bad} DIFFERENT"))
sys.exit(1 if bad else 0)
