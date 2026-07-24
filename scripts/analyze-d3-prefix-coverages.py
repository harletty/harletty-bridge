#!/usr/bin/env python3
"""Test whether D3 prefix states or fields predict temporal bed-fold coverage.

The prefix TSV inputs come from ``xll_d3_frame_features``.  The coverage TSV
comes from ``analyze-dtsx-bed-fold-limits.py --temporal-coverage-output``.
The final two prefix bytes are verified CRC-16/IBM-3740 values and are removed
before scanning possible 8-, 10- and 12-bit fields.
"""

from __future__ import annotations

import argparse
import csv
import math
import re
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

import numpy as np


WINDOW_SECONDS = 1.0
BED_NAMES = ("C", "L", "R", "Ls", "Rs", "LFE", "Lb", "Rb")
NAVIGATION_COLUMNS = (
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
)


@dataclass
class CoverageTrack:
    title: str
    times: np.ndarray
    coverages: np.ndarray
    active_blocks: np.ndarray


@dataclass
class PrefixTrack:
    times: np.ndarray
    navigation: np.ndarray
    prefixes: list[bytes]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--features-dir", type=Path, required=True)
    parser.add_argument("--coverages", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument(
        "--discovery-title",
        dest="discovery_titles",
        action="append",
        default=[],
        help=(
            "programme title assigned to the discovery subset; repeat for "
            "multiple titles (default: first three titles in lexical order)"
        ),
    )
    return parser.parse_args()


def canonical_title(value: str) -> str:
    return re.sub(r"[^a-z0-9]+", "", value.lower()).replace(
        "larepeater", "lerepeater"
    )


def read_coverages(path: Path) -> dict[str, CoverageTrack]:
    grouped: dict[str, list[dict[str, str]]] = defaultdict(list)
    with path.open(newline="") as source:
        for row in csv.DictReader(source, dialect="excel-tab"):
            grouped[canonical_title(row["title"])].append(row)

    tracks = {}
    for key, rows in grouped.items():
        times = sorted({float(row["audio_time"]) for row in rows})
        extensions = sorted(
            {row["extension"] for row in rows},
            key=lambda value: int(value[1:]),
        )
        time_index = {value: index for index, value in enumerate(times)}
        extension_index = {
            value: index for index, value in enumerate(extensions)
        }
        bed_index = {value: index for index, value in enumerate(BED_NAMES)}
        coverages = np.full(
            (len(times), len(extensions), len(BED_NAMES)), np.nan
        )
        active_blocks = np.zeros((len(times), len(extensions)))
        for row in rows:
            time = time_index[float(row["audio_time"])]
            extension = extension_index[row["extension"]]
            active_blocks[time, extension] = float(row["active_blocks"])
            if row["valid"] == "1":
                coverages[time, extension, bed_index[row["bed"]]] = float(
                    row["coverage"]
                )
        tracks[key] = CoverageTrack(
            title=rows[0]["title"],
            times=np.asarray(times),
            coverages=coverages,
            active_blocks=active_blocks,
        )
    return tracks


def read_prefix_track(path: Path) -> PrefixTrack:
    with path.open(newline="") as source:
        rows = list(csv.DictReader(source, dialect="excel-tab"))
    return PrefixTrack(
        times=np.asarray([float(row["time"]) for row in rows]),
        navigation=np.column_stack(
            [
                np.asarray([float(row[name]) for row in rows])
                for name in NAVIGATION_COLUMNS
            ]
        ),
        prefixes=[bytes.fromhex(row["prefix_hex"]) for row in rows],
    )


def window_bounds(
    frame_times: np.ndarray, target_times: np.ndarray
) -> tuple[np.ndarray, np.ndarray]:
    half = WINDOW_SECONDS / 2.0
    return (
        np.searchsorted(frame_times, target_times - half, side="left"),
        np.searchsorted(frame_times, target_times + half, side="right"),
    )


def window_means(
    frame_times: np.ndarray,
    values: np.ndarray,
    target_times: np.ndarray,
) -> np.ndarray:
    cumulative = np.vstack(
        (np.zeros((1, values.shape[1])), np.cumsum(values, axis=0))
    )
    starts, ends = window_bounds(frame_times, target_times)
    counts = np.maximum(ends - starts, 1)
    return (cumulative[ends] - cumulative[starts]) / counts[:, None]


def prefix_window_data(
    track: PrefixTrack, target_times: np.ndarray
) -> tuple[list[bytes], np.ndarray, list[str], np.ndarray]:
    states = sorted(set(track.prefixes))
    state_index = {value: index for index, value in enumerate(states)}
    assignments = np.asarray(
        [state_index[value] for value in track.prefixes]
    )
    starts, ends = window_bounds(track.times, target_times)
    proportions = np.zeros((len(target_times), len(states)))
    for row, (start, end) in enumerate(zip(starts, ends)):
        proportions[row] = np.bincount(
            assignments[start:end], minlength=len(states)
        )
    totals = np.maximum(np.sum(proportions, axis=1), 1.0)
    proportions /= totals[:, None]
    dominant = np.argmax(proportions, axis=1)

    bodies = [value[:-2] for value in states]
    width = max(map(len, bodies))
    padded = np.zeros((len(states), width), dtype=np.uint8)
    for index, value in enumerate(bodies):
        padded[index, : len(value)] = np.frombuffer(value, dtype=np.uint8)
    bits = np.unpackbits(padded, axis=1)

    names = []
    state_features = []
    for field_width in (8, 10, 12):
        windows = np.lib.stride_tricks.sliding_window_view(
            bits, field_width, axis=1
        )
        weights = 1 << np.arange(field_width - 1, -1, -1)
        unsigned = windows @ weights
        phase = 2.0 * np.pi * unsigned / (1 << field_width)
        for offset in range(unsigned.shape[1]):
            names.extend(
                (
                    f"u{field_width}.bit{offset}",
                    f"phase_sin.u{field_width}.bit{offset}",
                    f"phase_cos.u{field_width}.bit{offset}",
                )
            )
            state_features.extend(
                (
                    unsigned[:, offset],
                    np.sin(phase[:, offset]),
                    np.cos(phase[:, offset]),
                )
            )
    features = proportions @ np.column_stack(state_features)
    return states, dominant, names, features


def residualize(values: np.ndarray, controls: np.ndarray) -> np.ndarray:
    result = np.full_like(values, np.nan, dtype=np.float64)
    finite = np.all(np.isfinite(controls), axis=1)
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


def categorical_metrics(
    labels: np.ndarray, target: np.ndarray
) -> tuple[float, float, float, float]:
    valid = np.isfinite(target)
    if np.count_nonzero(valid) < 40:
        return (float("nan"),) * 4
    values = target[valid]
    variance = float(np.var(values))
    if variance <= 1e-12:
        return 0.0, 0.0, 1.0, 1.0
    valid_labels = labels[valid]
    means = {
        state: float(np.mean(values[valid_labels == state]))
        for state in np.unique(valid_labels)
    }
    prediction = np.asarray([means[state] for state in valid_labels])
    eta_squared = 1.0 - float(
        np.mean((values - prediction) ** 2) / variance
    )
    null = []
    for shift in np.unique(
        np.linspace(
            len(valid_labels) // 10,
            9 * len(valid_labels) // 10,
            32,
        ).astype(int)
    ):
        shifted = np.roll(valid_labels, shift)
        shifted_means = {
            state: float(np.mean(values[shifted == state]))
            for state in np.unique(shifted)
        }
        shifted_prediction = np.asarray(
            [shifted_means[state] for state in shifted]
        )
        null.append(
            1.0
            - float(
                np.mean((values - shifted_prediction) ** 2) / variance
            )
        )
    pvalue = (1.0 + sum(value >= eta_squared for value in null)) / (
        1.0 + len(null)
    )

    midpoint = len(target) // 2
    train = valid & (np.arange(len(target)) < midpoint)
    test = valid & (np.arange(len(target)) >= midpoint)
    if np.count_nonzero(train) == 0 or np.count_nonzero(test) == 0:
        return eta_squared, float("nan"), 0.0, pvalue
    baseline = float(np.mean(target[train]))
    state_means = {
        state: float(np.mean(target[train & (labels == state)]))
        for state in np.unique(labels[train])
    }
    seen = test & np.isin(labels, list(state_means))
    seen_fraction = np.count_nonzero(seen) / np.count_nonzero(test)
    if np.count_nonzero(seen) < 20:
        return eta_squared, float("nan"), seen_fraction, pvalue
    predicted = np.asarray([state_means[state] for state in labels[seen]])
    model_error = float(np.mean((target[seen] - predicted) ** 2))
    baseline_error = float(np.mean((target[seen] - baseline) ** 2))
    skill = (
        1.0 - model_error / baseline_error
        if baseline_error > 1e-12
        else 0.0
    )

    return eta_squared, skill, seen_fraction, pvalue


def transition_metrics(
    labels: np.ndarray, coverages: np.ndarray
) -> tuple[int, float, float, float]:
    flattened = coverages.reshape(len(coverages), -1)
    transition = np.flatnonzero(labels[1:] != labels[:-1]) + 1
    candidates = np.arange(1, len(labels) - 1)
    scores = np.full(len(labels), np.nan)
    for index in candidates:
        a = flattened[index - 1]
        b = flattened[index + 1]
        valid = np.isfinite(a) & np.isfinite(b)
        if np.count_nonzero(valid) >= 8:
            scores[index] = float(
                np.sqrt(np.mean((b[valid] - a[valid]) ** 2))
            )
    actual = scores[transition]
    actual = actual[np.isfinite(actual)]
    other = scores[np.setdiff1d(candidates, transition)]
    other = other[np.isfinite(other)]
    if actual.size == 0 or other.size == 0:
        return len(transition), float("nan"), float("nan"), float("nan")
    actual_median = float(np.median(actual))
    other_median = float(np.median(other))
    random = np.random.default_rng(0)
    null = [
        float(
            np.median(
                random.choice(
                    other,
                    size=min(len(actual), len(other)),
                    replace=False,
                )
            )
        )
        for _ in range(1000)
    ]
    pvalue = (1.0 + sum(value >= actual_median for value in null)) / 1001.0
    return len(transition), actual_median, other_median, pvalue


def vector_correlation(a: np.ndarray, b: np.ndarray) -> float:
    valid = np.isfinite(a) & np.isfinite(b)
    if np.count_nonzero(valid) < 8:
        return float("nan")
    x = a[valid] - np.mean(a[valid])
    y = b[valid] - np.mean(b[valid])
    denominator = np.linalg.norm(x) * np.linalg.norm(y)
    return float(np.dot(x, y) / denominator) if denominator > 1e-12 else float(
        "nan"
    )


def write_rows(path: Path, fields: list[str], rows: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="") as target:
        writer = csv.DictWriter(target, fields, dialect="excel-tab")
        writer.writeheader()
        writer.writerows(rows)


def main() -> None:
    args = parse_args()
    coverage_tracks = read_coverages(args.coverages)
    categorical_rows = []
    field_rows = []
    summary_rows = []
    bit299_programme_rows = []
    correlations_by_title = {}
    state_vectors_by_title: dict[str, dict[bytes, np.ndarray]] = {}

    seen_titles = set()
    for path in sorted(args.features_dir.glob("*.tsv")):
        key = canonical_title(path.stem)
        if key not in coverage_tracks or key in seen_titles:
            continue
        seen_titles.add(key)
        coverage = coverage_tracks[key]
        prefix = read_prefix_track(path)
        states, labels, feature_names, features = prefix_window_data(
            prefix, coverage.times
        )
        bit299_index = feature_names.index("phase_cos.u10.bit299")
        for extension in range(coverage.coverages.shape[1]):
            for bed, bed_name in enumerate(BED_NAMES):
                target = coverage.coverages[:, extension, bed]
                valid = np.isfinite(target)
                if not np.any(valid):
                    continue
                bit299_programme_rows.append(
                    {
                        "title": coverage.title,
                        "extension": f"X{extension}",
                        "bed": bed_name,
                        "field_mean": f"{np.mean(features[valid, bit299_index]):.9f}",
                        "coverage_mean": f"{np.mean(target[valid]):.9f}",
                    }
                )
        navigation = window_means(
            prefix.times, prefix.navigation, coverage.times
        )
        activity = np.log1p(coverage.active_blocks)
        controls = np.column_stack((activity, navigation))
        title_correlations = np.zeros(
            (len(feature_names), coverage.coverages.shape[1], len(BED_NAMES))
        )
        title_holdout_correlations = np.zeros_like(title_correlations)
        flattened_coverages = coverage.coverages.reshape(
            len(coverage.times), -1
        )
        state_vectors = {}
        for state_index, state in enumerate(states):
            selected = labels == state_index
            if np.count_nonzero(selected) < 2:
                continue
            values = flattened_coverages[selected]
            finite = np.isfinite(values)
            counts = np.sum(finite, axis=0)
            vector = np.divide(
                np.nansum(values, axis=0),
                counts,
                out=np.full(values.shape[1], np.nan),
                where=counts > 0,
            )
            if np.count_nonzero(np.isfinite(vector)) >= 8:
                state_vectors[state] = vector
        state_vectors_by_title[coverage.title] = state_vectors

        eta_values = []
        skill_values = []
        seen_values = []
        significant_categories = 0
        for extension in range(coverage.coverages.shape[1]):
            extension_valid = np.all(
                np.isfinite(coverage.coverages[:, extension]), axis=1
            )
            valid_count = np.count_nonzero(extension_valid)
            extension_features = features[extension_valid]
            extension_controls = controls[extension_valid]
            midpoint = valid_count // 2
            if valid_count >= 40:
                full_residual_features = residualize(
                    extension_features, extension_controls
                )
                training_residual_features = residualize(
                    extension_features[:midpoint],
                    extension_controls[:midpoint],
                )
                holdout_residual_features = residualize(
                    extension_features[midpoint:],
                    extension_controls[midpoint:],
                )
            for bed, bed_name in enumerate(BED_NAMES):
                target = coverage.coverages[:, extension, bed]
                eta, skill, seen, category_p = categorical_metrics(
                    labels, target
                )
                categorical_rows.append(
                    {
                        "title": coverage.title,
                        "extension": f"X{extension}",
                        "bed": bed_name,
                        "prefix_states": len(set(prefix.prefixes)),
                        "eta_squared": f"{eta:.6f}",
                        "holdout_skill": f"{skill:.6f}",
                        "holdout_seen_fraction": f"{seen:.6f}",
                        "block_shift_p": f"{category_p:.6f}",
                    }
                )
                if math.isfinite(eta):
                    eta_values.append(eta)
                if math.isfinite(skill):
                    skill_values.append(skill)
                if math.isfinite(seen):
                    seen_values.append(seen)
                if math.isfinite(category_p) and category_p <= 0.05:
                    significant_categories += 1

                if valid_count < 40:
                    continue
                extension_target = target[extension_valid]
                full_residual_target = residualize(
                    extension_target[:, None], extension_controls
                )[:, 0]
                if np.count_nonzero(np.isfinite(full_residual_target)) < 40:
                    continue
                full_correlation = correlations(
                    full_residual_features, full_residual_target
                )
                title_correlations[:, extension, bed] = full_correlation
                training_residual_target = residualize(
                    extension_target[:midpoint, None],
                    extension_controls[:midpoint],
                )[:, 0]
                holdout_residual_target = residualize(
                    extension_target[midpoint:, None],
                    extension_controls[midpoint:],
                )[:, 0]
                training = correlations(
                    training_residual_features,
                    training_residual_target,
                )
                candidate = int(np.argmax(np.abs(training)))
                holdout_correlations = correlations(
                    holdout_residual_features,
                    holdout_residual_target,
                )
                title_holdout_correlations[:, extension, bed] = (
                    holdout_correlations
                )
                holdout = float(holdout_correlations[candidate])
                pvalue = block_shift_pvalue(
                    holdout_residual_features[:, candidate],
                    holdout_residual_target,
                    abs(holdout),
                )
                field_rows.append(
                    {
                        "title": coverage.title,
                        "extension": f"X{extension}",
                        "bed": bed_name,
                        "feature": feature_names[candidate],
                        "partial_r": f"{full_correlation[candidate]:.6f}",
                        "holdout_partial_r": f"{holdout:.6f}",
                        "holdout_block_shift_p": f"{pvalue:.6f}",
                    }
                )

        (
            transition_count,
            transition_median,
            ordinary_median,
            transition_p,
        ) = transition_metrics(labels, coverage.coverages)
        summary_rows.append(
            {
                "title": coverage.title,
                "prefix_states": len(set(prefix.prefixes)),
                "window_transitions": transition_count,
                "median_eta_squared": f"{np.nanmedian(eta_values):.6f}",
                "maximum_eta_squared": f"{np.nanmax(eta_values):.6f}",
                "median_holdout_skill": (
                    f"{np.nanmedian(skill_values):.6f}"
                    if skill_values
                    else ""
                ),
                "positive_holdout_targets": sum(
                    value > 0.0 for value in skill_values
                ),
                "median_seen_fraction": f"{np.nanmedian(seen_values):.6f}",
                "significant_categorical_targets": significant_categories,
                "transition_coverage_rms": f"{transition_median:.6f}",
                "ordinary_coverage_rms": f"{ordinary_median:.6f}",
                "transition_p": f"{transition_p:.6f}",
            }
        )
        correlations_by_title[coverage.title] = (
            {
                feature: index
                for index, feature in enumerate(feature_names)
            },
            title_correlations,
            title_holdout_correlations,
        )

    common_features = set.intersection(
        *(
            set(feature_indices)
            for feature_indices, _, _ in correlations_by_title.values()
        )
    )
    common_features = sorted(common_features)
    repeatability_rows = []
    for feature in common_features:
        values = []
        for title, (
            feature_indices,
            title_correlations,
            title_holdout_correlations,
        ) in (
            correlations_by_title.items()
        ):
            index = feature_indices[feature]
            values.append(
                (
                    title,
                    title_correlations[index],
                    title_holdout_correlations[index],
                )
            )
        for extension in range(8):
            for bed, bed_name in enumerate(BED_NAMES):
                coefficients = np.asarray(
                    [matrix[extension, bed] for _, matrix, _ in values]
                )
                holdout_coefficients = np.asarray(
                    [matrix[extension, bed] for _, _, matrix in values]
                )
                varying = np.abs(coefficients) > 1e-12
                positive = int(np.count_nonzero(coefficients > 0.15))
                negative = int(np.count_nonzero(coefficients < -0.15))
                same_sign = max(positive, negative)
                median = (
                    float(np.median(np.abs(coefficients[varying])))
                    if np.any(varying)
                    else 0.0
                )
                if same_sign < 3 and median < 0.15:
                    continue
                repeatability_rows.append(
                    {
                        "feature": feature,
                        "extension": f"X{extension}",
                        "bed": bed_name,
                        "programmes": len(values),
                        "varying_programmes": int(np.count_nonzero(varying)),
                        "median_abs_partial_r": f"{median:.6f}",
                        "positive_above_0.15": positive,
                        "negative_below_-0.15": negative,
                        "maximum_same_sign": same_sign,
                        "coefficients": ",".join(
                            f"{title}:{matrix[extension, bed]:+.3f}"
                            for title, matrix, _ in values
                        ),
                        "holdout_coefficients": ",".join(
                            f"{title}:{matrix[extension, bed]:+.3f}"
                            for title, _, matrix in values
                        ),
                    }
                )
    repeatability_rows.sort(
        key=lambda row: (
            row["maximum_same_sign"],
            float(row["median_abs_partial_r"]),
        ),
        reverse=True,
    )

    shared_state_rows = []
    titles = sorted(state_vectors_by_title)
    for left_index, left_title in enumerate(titles):
        left = state_vectors_by_title[left_title]
        for right_title in titles[left_index + 1 :]:
            right = state_vectors_by_title[right_title]
            for state in set(left) & set(right):
                observed = vector_correlation(left[state], right[state])
                if not math.isfinite(observed):
                    continue
                null = [
                    vector_correlation(left_vector, right_vector)
                    for left_state, left_vector in left.items()
                    for right_state, right_vector in right.items()
                    if left_state != right_state
                ]
                null = [value for value in null if math.isfinite(value)]
                percentile = (
                    float(np.mean(np.asarray(null) <= observed))
                    if null
                    else float("nan")
                )
                shared_state_rows.append(
                    {
                        "left_title": left_title,
                        "right_title": right_title,
                        "prefix_len": len(state),
                        "prefix_hex": state.hex(),
                        "coverage_vector_r": f"{observed:.6f}",
                        "different_prefix_median_r": (
                            f"{np.median(null):.6f}" if null else ""
                        ),
                        "within_pair_percentile": f"{percentile:.6f}",
                        "different_prefix_pairs": len(null),
                    }
                )

    bit299_summary_rows = []
    discovery_titles = set(args.discovery_titles)
    if not discovery_titles:
        discovery_titles = set(titles[:3])
    for group, selected_titles in (
        ("all", set(titles)),
        ("discovery", discovery_titles),
        ("validation", set(titles) - discovery_titles),
    ):
        for extension in range(8):
            for bed_name in BED_NAMES:
                selected = [
                    row
                    for row in bit299_programme_rows
                    if row["title"] in selected_titles
                    and row["extension"] == f"X{extension}"
                    and row["bed"] == bed_name
                ]
                x = np.asarray(
                    [float(row["field_mean"]) for row in selected]
                )
                y = np.asarray(
                    [float(row["coverage_mean"]) for row in selected]
                )
                correlation = (
                    float(np.corrcoef(x, y)[0, 1])
                    if len(selected) >= 3
                    and np.std(x) > 1e-12
                    and np.std(y) > 1e-12
                    else float("nan")
                )
                bit299_summary_rows.append(
                    {
                        "group": group,
                        "programmes": len(selected),
                        "extension": f"X{extension}",
                        "bed": bed_name,
                        "programme_mean_r": f"{correlation:.6f}",
                    }
                )

    write_rows(
        args.output_dir / "categorical.tsv",
        [
            "title",
            "extension",
            "bed",
            "prefix_states",
            "eta_squared",
            "holdout_skill",
            "holdout_seen_fraction",
            "block_shift_p",
        ],
        categorical_rows,
    )
    write_rows(
        args.output_dir / "field-tests.tsv",
        [
            "title",
            "extension",
            "bed",
            "feature",
            "partial_r",
            "holdout_partial_r",
            "holdout_block_shift_p",
        ],
        field_rows,
    )
    write_rows(
        args.output_dir / "field-repeatability.tsv",
        [
            "feature",
            "extension",
            "bed",
            "programmes",
            "varying_programmes",
            "median_abs_partial_r",
            "positive_above_0.15",
            "negative_below_-0.15",
            "maximum_same_sign",
            "coefficients",
            "holdout_coefficients",
        ],
        repeatability_rows,
    )
    write_rows(
        args.output_dir / "summary.tsv",
        [
            "title",
            "prefix_states",
            "window_transitions",
            "median_eta_squared",
            "maximum_eta_squared",
            "median_holdout_skill",
            "positive_holdout_targets",
            "median_seen_fraction",
            "significant_categorical_targets",
            "transition_coverage_rms",
            "ordinary_coverage_rms",
            "transition_p",
        ],
        summary_rows,
    )
    write_rows(
        args.output_dir / "shared-prefixes.tsv",
        [
            "left_title",
            "right_title",
            "prefix_len",
            "prefix_hex",
            "coverage_vector_r",
            "different_prefix_median_r",
            "within_pair_percentile",
            "different_prefix_pairs",
        ],
        shared_state_rows,
    )
    write_rows(
        args.output_dir / "bit299-programmes.tsv",
        [
            "title",
            "extension",
            "bed",
            "field_mean",
            "coverage_mean",
        ],
        bit299_programme_rows,
    )
    write_rows(
        args.output_dir / "bit299-summary.tsv",
        [
            "group",
            "programmes",
            "extension",
            "bed",
            "programme_mean_r",
        ],
        bit299_summary_rows,
    )
    print(f"titles={len(summary_rows)}")
    print(f"summary={args.output_dir / 'summary.tsv'}")
    print(f"categorical={args.output_dir / 'categorical.tsv'}")
    print(f"field_tests={args.output_dir / 'field-tests.tsv'}")
    print(
        f"field_repeatability="
        f"{args.output_dir / 'field-repeatability.tsv'}"
    )
    print(f"shared_prefixes={args.output_dir / 'shared-prefixes.tsv'}")
    print(f"bit299_summary={args.output_dir / 'bit299-summary.tsv'}")


if __name__ == "__main__":
    main()
