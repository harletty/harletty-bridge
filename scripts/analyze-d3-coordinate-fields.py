#!/usr/bin/env python3
"""Test whether variable D3 framing fields follow inferred X positions.

The position input is the temporal TSV emitted by
``analyze-dtsx-bed-fold-limits.py``.  The feature inputs are frame TSV files
emitted by the ``xll_d3_frame_features`` research example.  Correlations are
reported both directly and after removing linear dependence on extension
activity and known navigation/layout quantities.

This is a falsification aid, not a metadata decoder.  In particular, the
coverage barycentres are content-derived estimates and may move even when the
authoring coordinates are fixed.
"""

from __future__ import annotations

import argparse
import csv
import math
import re
from collections import defaultdict
from pathlib import Path

import numpy as np


WINDOW_SECONDS = 1.0
NAVIGATION_COLUMNS = [
    "payload_size",
    "x_payload_offset",
    "x_payload_size",
    "prefix_len",
    "outer_len",
    "outer_geometry",
    "first_offset",
    "first_size",
    "inner_len",
    "inner_geometry",
    "second_offset",
    "second_size",
    "consumed_bytes",
    "trailer_len",
]
HEX_FIELDS = ["descriptor", "prefix", "outer", "inner"]
COMPONENTS = ["x", "y", "radius", "azimuth_sin", "azimuth_cos"]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--features-dir", type=Path, required=True)
    parser.add_argument("--positions", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--robust-output",
        type=Path,
        required=True,
        help="cross-programme repeatability TSV",
    )
    return parser.parse_args()


def canonical_title(value: str) -> str:
    return re.sub(r"[^a-z0-9]+", "", value.lower()).replace(
        "larepeater", "lerepeater"
    )


def read_positions(
    path: Path,
) -> dict[str, dict[str, np.ndarray | str]]:
    grouped: dict[str, list[dict[str, str]]] = defaultdict(list)
    with path.open(newline="") as source:
        for row in csv.DictReader(source, dialect="excel-tab"):
            grouped[canonical_title(row["title"])].append(row)

    result = {}
    for key, rows in grouped.items():
        extensions = sorted(
            {row["extension"] for row in rows},
            key=lambda value: int(value[1:]),
        )
        times = sorted({float(row["audio_time"]) for row in rows})
        time_index = {value: index for index, value in enumerate(times)}
        extension_index = {
            value: index for index, value in enumerate(extensions)
        }
        coordinates = np.full((len(times), len(extensions), 2), np.nan)
        activity = np.zeros((len(times), len(extensions)))
        for row in rows:
            time = time_index[float(row["audio_time"])]
            extension = extension_index[row["extension"]]
            activity[time, extension] = float(row["activity"])
            if row["valid"] == "1":
                coordinates[time, extension] = (
                    float(row["x"]),
                    float(row["y"]),
                )
        result[key] = {
            "title": rows[0]["title"],
            "times": np.asarray(times),
            "extensions": np.asarray(extensions),
            "coordinates": coordinates,
            "activity": activity,
        }
    return result


def read_feature_rows(
    path: Path,
) -> tuple[np.ndarray, list[str], np.ndarray, list[bytes]]:
    with path.open(newline="") as source:
        rows = list(csv.DictReader(source, dialect="excel-tab"))
    if not rows:
        raise ValueError(f"no D3 frames in {path}")

    names = list(NAVIGATION_COLUMNS)
    columns = [
        np.asarray([float(row[name]) for row in rows])
        for name in NAVIGATION_COLUMNS
    ]
    for field in HEX_FIELDS:
        values = [bytes.fromhex(row[f"{field}_hex"]) for row in rows]
        width = max(map(len, values))
        for byte in range(width):
            byte_values = np.asarray(
                [value[byte] if byte < len(value) else 0 for value in values],
                dtype=np.float64,
            )
            names.append(f"{field}.byte{byte}")
            columns.append(byte_values)
            for bit in range(8):
                names.append(f"{field}.bit{byte * 8 + bit}")
                columns.append(
                    ((byte_values.astype(np.uint8) >> (7 - bit)) & 1).astype(
                        np.float64
                    )
                )
    return (
        np.asarray([float(row["time"]) for row in rows]),
        names,
        np.column_stack(columns),
        [bytes.fromhex(row["prefix_hex"]) for row in rows],
    )


def window_means(
    frame_times: np.ndarray,
    values: np.ndarray,
    target_times: np.ndarray,
) -> np.ndarray:
    cumulative = np.vstack(
        (np.zeros((1, values.shape[1])), np.cumsum(values, axis=0))
    )
    half = WINDOW_SECONDS / 2.0
    starts = np.searchsorted(frame_times, target_times - half, side="left")
    ends = np.searchsorted(frame_times, target_times + half, side="right")
    counts = np.maximum(ends - starts, 1)
    return (cumulative[ends] - cumulative[starts]) / counts[:, None]


def prefix_bitfield_means(
    prefixes: list[bytes],
    frame_times: np.ndarray,
    target_times: np.ndarray,
) -> tuple[list[str], np.ndarray]:
    states = sorted(set(prefixes))
    state_index = {value: index for index, value in enumerate(states)}
    assignments = np.asarray([state_index[value] for value in prefixes])
    width = max(map(len, states))
    padded = np.zeros((len(states), width), dtype=np.uint8)
    for index, value in enumerate(states):
        padded[index, : len(value)] = np.frombuffer(value, dtype=np.uint8)
    bits = np.unpackbits(padded, axis=1)

    names = []
    state_features = []
    for field_width in [8, 10, 12]:
        windows = np.lib.stride_tricks.sliding_window_view(
            bits, field_width, axis=1
        )
        weights = 1 << np.arange(field_width - 1, -1, -1)
        unsigned = windows @ weights
        phase = 2.0 * np.pi * unsigned / (1 << field_width)
        for offset in range(unsigned.shape[1]):
            names.extend(
                [
                    f"prefix.u{field_width}.bit{offset}",
                    f"prefix.phase_sin.u{field_width}.bit{offset}",
                    f"prefix.phase_cos.u{field_width}.bit{offset}",
                ]
            )
            state_features.extend(
                [
                    unsigned[:, offset],
                    np.sin(phase[:, offset]),
                    np.cos(phase[:, offset]),
                ]
            )
    state_features_array = np.column_stack(state_features)

    half = WINDOW_SECONDS / 2.0
    starts = np.searchsorted(frame_times, target_times - half, side="left")
    ends = np.searchsorted(frame_times, target_times + half, side="right")
    counts = np.zeros((len(target_times), len(states)))
    for row, (start, end) in enumerate(zip(starts, ends)):
        counts[row] = np.bincount(
            assignments[start:end], minlength=len(states)
        )
    totals = np.maximum(np.sum(counts, axis=1), 1.0)
    return names, (counts @ state_features_array) / totals[:, None]


def residualize(values: np.ndarray, controls: np.ndarray) -> np.ndarray:
    finite = np.all(np.isfinite(controls), axis=1)
    result = np.full_like(values, np.nan, dtype=np.float64)
    if np.count_nonzero(finite) <= controls.shape[1]:
        return result
    design = np.column_stack((np.ones(np.count_nonzero(finite)), controls[finite]))
    coefficient, *_ = np.linalg.lstsq(design, values[finite], rcond=None)
    result[finite] = values[finite] - design @ coefficient
    return result


def correlations(features: np.ndarray, target: np.ndarray) -> np.ndarray:
    x = features - np.mean(features, axis=0)
    y = target - np.mean(target)
    x_norm = np.linalg.norm(x, axis=0)
    y_norm = np.linalg.norm(y)
    denominator = x_norm * y_norm
    return np.divide(
        x.T @ y,
        denominator,
        out=np.zeros(features.shape[1]),
        where=(x_norm > 1e-8) & (y_norm > 1e-8),
    )


def target_components(coordinates: np.ndarray) -> np.ndarray:
    x = coordinates[:, 0]
    y = coordinates[:, 1]
    radius = np.hypot(x, y)
    azimuth_sin = np.divide(
        x, radius, out=np.zeros_like(x), where=radius > 1e-12
    )
    azimuth_cos = np.divide(
        y, radius, out=np.zeros_like(y), where=radius > 1e-12
    )
    return np.column_stack((x, y, radius, azimuth_sin, azimuth_cos))


def block_shift_pvalue(
    feature: np.ndarray, target: np.ndarray, observed: float
) -> float:
    count = len(target)
    if count < 20:
        return float("nan")
    shifts = np.unique(np.linspace(count // 10, 9 * count // 10, 32).astype(int))
    null = [
        abs(float(correlations(feature[:, None], np.roll(target, shift))[0]))
        for shift in shifts
    ]
    return (1.0 + sum(value >= observed for value in null)) / (
        1.0 + len(null)
    )


def main() -> None:
    args = parse_args()
    positions = read_positions(args.positions)
    observations = []
    repeated: dict[tuple[str, str, str], list[tuple[str, float]]] = defaultdict(
        list
    )
    seen_titles = set()

    for path in sorted(args.features_dir.glob("*.tsv")):
        key = canonical_title(path.stem)
        if key not in positions or key in seen_titles:
            continue
        seen_titles.add(key)
        position = positions[key]
        (
            frame_times,
            feature_names,
            frame_features,
            prefixes,
        ) = read_feature_rows(path)
        times = position["times"]
        features = window_means(frame_times, frame_features, times)
        bitfield_names, bitfield_features = prefix_bitfield_means(
            prefixes, frame_times, times
        )
        feature_names.extend(bitfield_names)
        features = np.column_stack((features, bitfield_features))
        activities = np.log10(np.asarray(position["activity"]) + 1e-15)
        activities = np.maximum(activities, np.max(activities, axis=0) - 8.0)
        navigation = features[:, : len(NAVIGATION_COLUMNS)]
        controls = np.column_stack((activities, navigation))
        residual_features = residualize(features, controls)

        for extension_index, extension in enumerate(position["extensions"]):
            components = target_components(
                position["coordinates"][:, extension_index]
            )
            for component_index, component in enumerate(COMPONENTS):
                target = components[:, component_index]
                valid = np.isfinite(target)
                if np.count_nonzero(valid) < 40:
                    continue
                raw = correlations(features[valid], target[valid])
                residual_target = residualize(
                    target[valid, None], controls[valid]
                )[:, 0]
                selected_features = residual_features[valid]
                finite = np.isfinite(residual_target)
                if np.count_nonzero(finite) < 40:
                    continue
                partial = correlations(
                    selected_features[finite], residual_target[finite]
                )
                candidate_start = len(NAVIGATION_COLUMNS)
                midpoint = np.count_nonzero(finite) // 2
                training = correlations(
                    selected_features[finite][:midpoint],
                    residual_target[finite][:midpoint],
                )
                candidate = candidate_start + int(
                    np.argmax(np.abs(training[candidate_start:]))
                )
                holdout = float(
                    correlations(
                        selected_features[finite][midpoint:, candidate, None],
                        residual_target[finite][midpoint:],
                    )[0]
                )
                pvalue = block_shift_pvalue(
                    selected_features[finite][midpoint:, candidate],
                    residual_target[finite][midpoint:],
                    abs(holdout),
                )
                observations.append(
                    {
                        "title": position["title"],
                        "extension": extension,
                        "component": component,
                        "valid_windows": int(np.count_nonzero(valid)),
                        "feature": feature_names[candidate],
                        "raw_r": f"{raw[candidate]:.6f}",
                        "partial_r": f"{partial[candidate]:.6f}",
                        "holdout_partial_r": f"{holdout:.6f}",
                        "holdout_block_shift_p": f"{pvalue:.6f}",
                    }
                )
                for feature_index in range(candidate_start, len(feature_names)):
                    repeated[
                        (feature_names[feature_index], str(extension), component)
                    ].append(
                        (str(position["title"]), float(partial[feature_index]))
                    )

    fields = [
        "title",
        "extension",
        "component",
        "valid_windows",
        "feature",
        "raw_r",
        "partial_r",
        "holdout_partial_r",
        "holdout_block_shift_p",
    ]
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("w", newline="") as target:
        writer = csv.DictWriter(target, fields, dialect="excel-tab")
        writer.writeheader()
        writer.writerows(observations)

    robust_rows = []
    for (feature, extension, component), values in repeated.items():
        if len(values) < 5:
            continue
        coefficients = np.asarray([value for _, value in values])
        positive = int(np.count_nonzero(coefficients > 0.15))
        negative = int(np.count_nonzero(coefficients < -0.15))
        robust_rows.append(
            {
                "feature": feature,
                "extension": extension,
                "component": component,
                "programmes": len(values),
                "median_abs_partial_r": float(np.median(np.abs(coefficients))),
                "positive_above_0.15": positive,
                "negative_below_-0.15": negative,
                "maximum_same_sign": max(positive, negative),
                "coefficients": ",".join(
                    f"{title}:{coefficient:+.3f}"
                    for title, coefficient in values
                ),
            }
        )
    robust_rows.sort(
        key=lambda row: (
            row["maximum_same_sign"],
            row["median_abs_partial_r"],
        ),
        reverse=True,
    )
    robust_fields = [
        "feature",
        "extension",
        "component",
        "programmes",
        "median_abs_partial_r",
        "positive_above_0.15",
        "negative_below_-0.15",
        "maximum_same_sign",
        "coefficients",
    ]
    args.robust_output.parent.mkdir(parents=True, exist_ok=True)
    with args.robust_output.open("w", newline="") as target:
        writer = csv.DictWriter(target, robust_fields, dialect="excel-tab")
        writer.writeheader()
        writer.writerows(robust_rows)

    significant = sum(
        math.isfinite(float(row["holdout_block_shift_p"]))
        and float(row["holdout_block_shift_p"]) <= 0.05
        for row in observations
    )
    print(
        f"tests={len(observations)} significant_holdout_block_shift={significant}"
    )
    print(f"details={args.output}")
    print(f"repeatability={args.robust_output}")


if __name__ == "__main__":
    main()
