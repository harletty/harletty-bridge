#!/usr/bin/env python3
import argparse
import math
import numpy as np

BED_NAMES = ["C", "L", "R", "Ls", "Rs", "LFE", "Lb", "Rb"]


def clock(seconds):
    minutes, second = divmod(seconds, 60.0)
    hours, minute = divmod(int(minutes), 60)
    return f"{hours:02d}:{minute:02d}:{second:06.3f}"


def lag_fit(x, y, max_lag):
    best = None
    for lag in range(-max_lag, max_lag + 1):
        if lag < 0:
            xx, yy = x[-lag:], y[:lag]
        elif lag > 0:
            xx, yy = x[:-lag], y[lag:]
        else:
            xx, yy = x, y
        xx = xx.astype(np.float64)
        yy = yy.astype(np.float64)
        xx -= xx.mean()
        yy -= yy.mean()
        energy_x = np.dot(xx, xx)
        energy_y = np.dot(yy, yy)
        cross = np.dot(xx, yy)
        corr = cross / math.sqrt(max(energy_x * energy_y, 1e-300))
        beta = cross / max(energy_x, 1e-300)
        score = abs(corr)
        if best is None or score > best[0]:
            best = (score, lag, corr, beta)
    return best


def analyze_link(audio, rate, window, source_col, bed_col, label, subtract=(), expected=None):
    samples = audio.shape[0]
    windows = samples // window
    print(f"\n{label}")
    if windows == 0:
        print("  unavailable: input is shorter than one analysis window")
        return None
    beta = np.empty(windows)
    corr = np.empty(windows)
    residual = np.empty(windows)
    x_rms = np.empty(windows)
    chunk_windows = 256
    for begin in range(0, windows, chunk_windows):
        end = min(begin + chunk_windows, windows)
        sl = slice(begin * window, end * window)
        count = end - begin
        x = np.asarray(audio[sl, source_col], dtype=np.float64).reshape(count, window)
        y = np.asarray(audio[sl, bed_col], dtype=np.float64).reshape(count, window)
        for subtract_col, gain in subtract:
            y -= gain * np.asarray(audio[sl, subtract_col], dtype=np.float64).reshape(count, window)
        x -= x.mean(axis=1, keepdims=True)
        y -= y.mean(axis=1, keepdims=True)
        xx = np.sum(x * x, axis=1)
        yy = np.sum(y * y, axis=1)
        xy = np.sum(x * y, axis=1)
        local_beta = np.divide(xy, xx, out=np.zeros_like(xy), where=xx > 1e-30)
        local_corr = np.divide(xy, np.sqrt(xx * yy), out=np.zeros_like(xy), where=xx * yy > 1e-30)
        err = yy - np.divide(xy * xy, xx, out=np.zeros_like(xy), where=xx > 1e-30)
        beta[begin:end] = local_beta
        corr[begin:end] = local_corr
        residual[begin:end] = np.sqrt(np.maximum(err, 0.0) / np.maximum(yy, 1e-30))
        x_rms[begin:end] = np.sqrt(xx / window)

    active = x_rms >= max(np.quantile(x_rms, 0.70), 1e-12)
    if not np.any(active):
        print("  unavailable: source is silent in this range")
        return None
    order = np.argsort(np.where(active, residual, np.inf))
    selected = [
        int(index)
        for index in order[:64]
        if active[index] and np.isfinite(residual[index])
    ]
    for threshold in [0.95, 0.99, 0.999, 0.9999]:
        mask = active & (np.abs(corr) >= threshold)
        if np.any(mask):
            q = np.quantile(beta[mask], [0.05, 0.5, 0.95])
            print(f"  |r|>={threshold:.4f}: n={mask.sum():5d} beta={q[1]:+.9f} [{q[0]:+.9f},{q[2]:+.9f}]")
    print("  best scalar windows:")
    for index in selected[:8]:
        time = (index + 0.5) * window / rate
        print(f"    {clock(time)} r={corr[index]:+.9f} beta={beta[index]:+.9f} resid={residual[index]:.6e} xrms={x_rms[index]:.3e}")

    if expected is not None:
        fixed_residual = np.empty(windows)
        for begin in range(0, windows, chunk_windows):
            end = min(begin + chunk_windows, windows)
            sl = slice(begin * window, end * window)
            count = end - begin
            x = np.asarray(audio[sl, source_col], dtype=np.float64).reshape(count, window)
            y = np.asarray(audio[sl, bed_col], dtype=np.float64).reshape(count, window)
            for subtract_col, gain in subtract:
                y -= gain * np.asarray(audio[sl, subtract_col], dtype=np.float64).reshape(count, window)
            x -= x.mean(axis=1, keepdims=True)
            y -= y.mean(axis=1, keepdims=True)
            error = y - expected * x
            fixed_residual[begin:end] = np.sqrt(
                np.sum(error * error, axis=1) / np.maximum(np.sum(y * y, axis=1), 1e-30)
            )
        fixed_order = np.argsort(np.where(active, fixed_residual, np.inf))
        print(f"  best windows for expected gain {expected:.9f}:")
        for index in fixed_order[:8]:
            time = (index + 0.5) * window / rate
            print(f"    {clock(time)} fixed_resid={fixed_residual[index]:.6e} r={corr[index]:+.9f} beta={beta[index]:+.9f}")

    lag_rows = []
    for index in selected[:32]:
        start = index * window
        x = np.asarray(audio[start:start + window, source_col])
        y = np.asarray(audio[start:start + window, bed_col])
        for subtract_col, gain in subtract:
            y = y - gain * np.asarray(audio[start:start + window, subtract_col])
        lag_rows.append(lag_fit(x, y, 32))
    lag_counts = {}
    for _, lag, _, _ in lag_rows:
        lag_counts[lag] = lag_counts.get(lag, 0) + 1
    print(f"  best lags (32 isolated windows): {dict(sorted(lag_counts.items()))}")

    # Cross-spectral transfer estimate over the best scalar windows. A scalar
    # fold should have nearly constant real gain and near-zero phase.
    fft_size = window
    taper = np.hanning(fft_size)
    cross = np.zeros(fft_size // 2 + 1, dtype=np.complex128)
    power = np.zeros(fft_size // 2 + 1, dtype=np.float64)
    for index in selected:
        start = index * window
        x = np.asarray(audio[start:start + window, source_col], dtype=np.float64)
        y = np.asarray(audio[start:start + window, bed_col], dtype=np.float64)
        for subtract_col, gain in subtract:
            y -= gain * np.asarray(audio[start:start + window, subtract_col], dtype=np.float64)
        x = (x - x.mean()) * taper
        y = (y - y.mean()) * taper
        xf = np.fft.rfft(x)
        yf = np.fft.rfft(y)
        cross += np.conj(xf) * yf
        power += np.abs(xf) ** 2
    transfer = np.divide(cross, power, out=np.zeros_like(cross), where=power > np.max(power) * 1e-12)
    frequencies = np.fft.rfftfreq(fft_size, 1.0 / rate)
    print("  transfer by band (weighted complex H):")
    for low, high in [(20, 80), (80, 250), (250, 1000), (1000, 4000), (4000, 10000), (10000, 20000)]:
        mask = (frequencies >= low) & (frequencies < high) & (power > np.max(power) * 1e-10)
        if not np.any(mask):
            continue
        h = np.sum(cross[mask]) / np.sum(power[mask])
        print(f"    {low:5d}-{high:5d} Hz: real={h.real:+.6f} imag={h.imag:+.6f} mag={abs(h):.6f} phase={np.angle(h, deg=True):+.2f}°")
    return beta, corr, residual, x_rms


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("raw")
    parser.add_argument("--extensions", type=int, default=6)
    parser.add_argument("--rate", type=int, default=48000)
    parser.add_argument("--window", type=int, default=4096)
    args = parser.parse_args()
    channels = len(BED_NAMES) + args.extensions
    raw = np.memmap(args.raw, dtype="<f4", mode="r")
    samples = raw.size // channels
    audio = raw[:samples * channels].reshape(samples, channels)
    print(f"samples={samples} duration={clock(samples / args.rate)} channels={channels} window={args.window}")
    extension = len(BED_NAMES)
    gain = 23170.0 / 32768.0
    if args.extensions == 4:
        links = [
            (extension + 0, 1, "calibration X0 -> L (front top fold)", (), gain),
            (extension + 1, 2, "calibration X1 -> R (front top fold)", (), gain),
            (extension + 2, 6, "calibration X2 -> Lb (rear top fold)", (), gain),
            (extension + 3, 7, "calibration X3 -> Rb (rear top fold)", (), gain),
        ]
    else:
        links = [
            (extension + 0, 1, "D1 X0 -> L", (), gain),
            (extension + 1, 2, "D1 X1 -> R", (), gain),
            (extension + 4, 6, "D1 X4 -> Lb", (), gain),
            (extension + 5, 7, "D1 X5 -> Rb", (), gain),
            (extension + 2, 1, "candidate X2 -> L after X0 top removal", ((extension + 0, gain),), None),
            (extension + 2, 3, "candidate X2 -> Ls", (), None),
            (extension + 3, 2, "candidate X3 -> R after X1 top removal", ((extension + 1, gain),), None),
            (extension + 3, 4, "candidate X3 -> Rs", (), None),
        ]
    for source, bed, label, subtract, expected in links:
        analyze_link(audio, args.rate, args.window, source, bed, label, subtract, expected)


if __name__ == "__main__":
    main()
