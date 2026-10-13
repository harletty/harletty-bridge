#!/usr/bin/env python3
import argparse
import numpy as np

BED_NAMES = ["C", "L", "R", "Ls", "Rs", "LFE", "Lb", "Rb"]


def clock(seconds):
    minute, second = divmod(seconds, 60.0)
    hour, minute = divmod(int(minute), 60)
    return f"{hour:02d}:{minute:02d}:{second:06.3f}"


def analyze_side(audio, rate, window, source_cols, source_names, bed_cols, bed_names):
    count = audio.shape[0] // window
    coefficients = np.full((count, len(bed_cols), len(source_cols)), np.nan)
    r2 = np.zeros((count, len(bed_cols)))
    condition = np.full(count, np.inf)
    source_rms = np.zeros((count, len(source_cols)))
    global_xx = np.zeros((len(source_cols), len(source_cols)))
    global_xy = np.zeros((len(source_cols), len(bed_cols)))
    chunk_windows = 128
    for begin in range(0, count, chunk_windows):
        end = min(begin + chunk_windows, count)
        rows = end - begin
        sl = slice(begin * window, end * window)
        x = np.asarray(audio[sl][:, source_cols], dtype=np.float64).reshape(rows, window, len(source_cols))
        y = np.asarray(audio[sl][:, bed_cols], dtype=np.float64).reshape(rows, window, len(bed_cols))
        x -= x.mean(axis=1, keepdims=True)
        y -= y.mean(axis=1, keepdims=True)
        xx = np.einsum("wsi,wsj->wij", x, x)
        xy = np.einsum("wsi,wsk->wik", x, y)
        yy = np.einsum("wsk,wsk->wk", y, y)
        energy = np.diagonal(xx, axis1=1, axis2=2)
        source_rms[begin:end] = np.sqrt(np.maximum(energy, 0.0) / window)
        scale = np.sqrt(np.maximum(energy[:, :, None] * energy[:, None, :], 1e-300))
        correlation = xx / scale
        condition[begin:end] = np.linalg.cond(correlation)
        valid = np.isfinite(condition[begin:end]) & (condition[begin:end] < 1e8) & np.all(energy > 1e-20, axis=1)
        for local in np.flatnonzero(valid):
            beta = np.linalg.solve(xx[local], xy[local])
            coefficients[begin + local] = beta.T
            predicted = np.sum(beta * xy[local], axis=0)
            r2[begin + local] = np.clip(predicted / np.maximum(yy[local], 1e-300), 0.0, 1.0)
        global_xx += xx.sum(axis=0)
        global_xy += xy.sum(axis=0)

    print(f"\n{'+'.join(source_names)} -> {','.join(bed_names)}")
    print("  global least squares:")
    rank = np.linalg.matrix_rank(global_xx)
    if rank < len(source_cols):
        print(
            "    unavailable: extension sources are silent or linearly dependent "
            f"(rank {rank}/{len(source_cols)})"
        )
    else:
        global_beta = np.linalg.solve(global_xx, global_xy).T
        for target, values in zip(bed_names, global_beta):
            print(
                "    "
                + target
                + ": "
                + " ".join(
                    f"{name}={value:+.9f}"
                    for name, value in zip(source_names, values)
                )
            )

    active_floor = np.quantile(source_rms, 0.40, axis=0)
    active = np.all(source_rms >= active_floor, axis=1)
    for cond_limit in [3, 10, 30, 100]:
        mask = active & (condition <= cond_limit) & (np.max(r2, axis=1) >= 0.90)
        print(f"  condition<={cond_limit:3d}, max R2>=.90: n={mask.sum()}")
        if not np.any(mask):
            continue
        for target_index, target in enumerate(bed_names):
            values = coefficients[mask, target_index]
            fields = []
            for source_index, source in enumerate(source_names):
                q = np.nanquantile(values[:, source_index], [0.1, 0.5, 0.9])
                fields.append(f"{source}={q[1]:+.4f}[{q[0]:+.4f},{q[2]:+.4f}]")
            print(f"    {target}: " + " ".join(fields))

    print("  coefficient plateaus (cond<=3, target R2>=.999, rounded 0.001):")
    for target_index, target in enumerate(bed_names):
        mask = active & (condition <= 3) & (r2[:, target_index] >= 0.999)
        print(f"    {target}: n={mask.sum()}")
        for source_index, source in enumerate(source_names):
            values = coefficients[mask, target_index, source_index]
            values = values[np.isfinite(values) & (np.abs(values) <= 4)]
            if values.size == 0:
                continue
            rounded, counts = np.unique(np.round(values, 3), return_counts=True)
            order_bins = np.argsort(counts)[::-1][:8]
            bins = " ".join(f"{rounded[index]:+.3f}:{counts[index]}" for index in order_bins)
            print(f"      {source}: {bins}")

    joint = active & (condition <= 3) & np.all(r2 >= 0.999, axis=1)
    print(f"  joint near-exact matrix windows: n={joint.sum()}")
    if np.any(joint):
        for target_index, target in enumerate(bed_names):
            fields = []
            for source_index, source in enumerate(source_names):
                values = coefficients[joint, target_index, source_index]
                median = np.nanmedian(values)
                mad = np.nanmedian(np.abs(values - median))
                fields.append(f"{source}={median:+.9f} mad={mad:.3e}")
            print(f"    {target}: " + " ".join(fields))

    score = np.max(r2, axis=1) - 0.03 * np.log10(np.maximum(condition, 1.0))
    eligible = active & (condition <= 30)
    order = np.argsort(np.where(eligible, score, -np.inf))[::-1]
    chosen = []
    print("  strongest well-conditioned windows:")
    for index in order:
        if not np.isfinite(score[index]):
            continue
        time = (index + 0.5) * window / rate
        if all(abs(time - previous) >= 2.0 for previous in chosen):
            chosen.append(time)
            print(f"    {clock(time)} cond={condition[index]:.2f} R2=" + ",".join(f"{name}:{r2[index, i]:.6f}" for i, name in enumerate(bed_names)))
            for target_index, target in enumerate(bed_names):
                print("      " + target + ": " + " ".join(
                    f"{source}={coefficients[index, target_index, source_index]:+.6f}"
                    for source_index, source in enumerate(source_names)
                ))
        if len(chosen) == 12:
            break


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("raw")
    parser.add_argument("--extensions", type=int, required=True)
    parser.add_argument("--rate", type=int, default=48000)
    parser.add_argument("--window", type=int, default=4096)
    args = parser.parse_args()
    channels = len(BED_NAMES) + args.extensions
    raw = np.memmap(args.raw, dtype="<f4", mode="r")
    samples = raw.size // channels
    audio = raw[:samples * channels].reshape(samples, channels)
    extension = len(BED_NAMES)
    print(f"duration={clock(samples / args.rate)} samples={samples} channels={channels}")
    if args.extensions == 4:
        left_sources = [extension + 0, extension + 2]
        right_sources = [extension + 1, extension + 3]
        left_names = ["X0", "X2"]
        right_names = ["X1", "X3"]
    elif args.extensions == 6:
        left_sources = [extension + 0, extension + 2, extension + 4]
        right_sources = [extension + 1, extension + 3, extension + 5]
        left_names = ["X0", "X2", "X4"]
        right_names = ["X1", "X3", "X5"]
    else:
        raise SystemExit("expected four or six extension sources")
    analyze_side(audio, args.rate, args.window, left_sources, left_names, [1, 3, 6], ["L", "Ls", "Lb"])
    analyze_side(audio, args.rate, args.window, right_sources, right_names, [2, 4, 7], ["R", "Rs", "Rb"])


if __name__ == "__main__":
    main()
