#!/usr/bin/env python3
"""Compare two headless cbar performance records and reject material regressions."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import pathlib
import sys
from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True)
class Limit:
    relative: float
    absolute: float


LIMITS = {
    "startup_to_layout_ms": Limit(relative=0.20, absolute=25.0),
    "resident_rss_kib": Limit(relative=0.10, absolute=4096.0),
    "idle_cpu_ms_per_s": Limit(relative=0.25, absolute=10.0),
    "graph_redraws_per_s": Limit(relative=0.20, absolute=0.5),
}
SAMPLE_RATE_LIMIT = Limit(relative=0.10, absolute=0.2)


def load(path: pathlib.Path) -> dict[str, Any]:
    try:
        record = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read {path}: {error}") from error
    if (
        record.get("schema") != 1
        or not isinstance(record.get("metrics"), dict)
        or not isinstance(record.get("context"), dict)
        or not isinstance(record.get("context_fingerprint"), str)
    ):
        raise ValueError(f"{path}: expected performance schema 1")
    expected = context_fingerprint(record["context"])
    if record["context_fingerprint"] != expected:
        raise ValueError(f"{path}: benchmark context fingerprint is invalid")
    return record


def context_fingerprint(context: dict[str, Any]) -> str:
    encoded = json.dumps(context, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def number(metrics: dict[str, Any], name: str) -> float:
    value = metrics.get(name)
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"metric {name!r} is not numeric")
    value = float(value)
    if not math.isfinite(value) or value < 0:
        raise ValueError(f"metric {name!r} must be finite and non-negative")
    return value


def normalized(record: dict[str, Any]) -> dict[str, float]:
    metrics = record["metrics"]
    window_ms = number(metrics, "idle_window_ms")
    if window_ms <= 0:
        raise ValueError("metric 'idle_window_ms' must be positive")
    window_seconds = window_ms / 1000.0
    return {
        "startup_to_layout_ms": number(metrics, "startup_to_layout_ms"),
        "resident_rss_kib": number(metrics, "resident_rss_kib"),
        "idle_cpu_ms_per_s": number(metrics, "idle_cpu_ms") / window_seconds,
        "graph_samples_per_s": number(metrics, "graph_samples") / window_seconds,
        "graph_redraws_per_s": number(metrics, "graph_redraws") / window_seconds,
    }


def compare(
    baseline: dict[str, Any], current: dict[str, Any], *, emit: bool = True
) -> list[str]:
    if baseline.get("context_fingerprint") != current.get("context_fingerprint"):
        raise ValueError(
            "benchmark contexts differ; record a new baseline before comparing releases"
        )
    old = normalized(baseline)
    new = normalized(current)
    failures = []
    for name, limit in LIMITS.items():
        allowance = max(old[name] * limit.relative, limit.absolute)
        ceiling = old[name] + allowance
        result = "PASS" if new[name] <= ceiling else "REGRESSION"
        if emit:
            print(
                f"performance-compare metric={name} baseline={old[name]:.3f} "
                f"current={new[name]:.3f} ceiling={ceiling:.3f} result={result}"
            )
        if result != "PASS":
            failures.append(name)

    sample_allowance = max(
        old["graph_samples_per_s"] * SAMPLE_RATE_LIMIT.relative,
        SAMPLE_RATE_LIMIT.absolute,
    )
    sample_delta = abs(new["graph_samples_per_s"] - old["graph_samples_per_s"])
    sample_result = "PASS" if sample_delta <= sample_allowance else "REGRESSION"
    if emit:
        print(
            "performance-compare "
            f"metric=graph_samples_per_s baseline={old['graph_samples_per_s']:.3f} "
            f"current={new['graph_samples_per_s']:.3f} "
            f"allowance={sample_allowance:.3f} result={sample_result}"
        )
    if sample_result != "PASS":
        failures.append("graph_samples_per_s")
    return failures


def self_test() -> None:
    context = {
        "architecture": "fixture",
        "binary_profile": "release",
        "graph_sample_interval_ms": 500,
    }
    baseline = {
        "schema": 1,
        "context": context,
        "context_fingerprint": context_fingerprint(context),
        "metrics": {
            "startup_to_layout_ms": 100,
            "resident_rss_kib": 50_000,
            "idle_window_ms": 2_000,
            "idle_cpu_ms": 20,
            "graph_samples": 4,
            "graph_redraws": 4,
        },
    }
    within = json.loads(json.dumps(baseline))
    within["metrics"]["resident_rss_kib"] += 4_096
    assert compare(baseline, within, emit=False) == []

    regression = json.loads(json.dumps(baseline))
    regression["metrics"]["resident_rss_kib"] += 5_001
    assert compare(baseline, regression, emit=False) == ["resident_rss_kib"]

    slow_sampler = json.loads(json.dumps(baseline))
    slow_sampler["metrics"]["graph_samples"] = 2
    assert compare(baseline, slow_sampler, emit=False) == ["graph_samples_per_s"]

    mismatched = json.loads(json.dumps(baseline))
    mismatched_context = {**context, "architecture": "different"}
    mismatched["context"] = mismatched_context
    mismatched["context_fingerprint"] = context_fingerprint(mismatched_context)
    try:
        compare(baseline, mismatched, emit=False)
    except ValueError as error:
        assert "contexts differ" in str(error)
    else:
        raise AssertionError("mismatched benchmark contexts must not compare")
    print("performance-compare self-test=PASS")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("baseline", nargs="?", type=pathlib.Path)
    parser.add_argument("current", nargs="?", type=pathlib.Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        if args.baseline is not None or args.current is not None:
            parser.error("--self-test does not accept records")
        self_test()
        return 0
    if args.baseline is None or args.current is None:
        parser.error("baseline and current records are required")

    try:
        failures = compare(load(args.baseline), load(args.current))
    except ValueError as error:
        print(f"performance-compare error={error}", file=sys.stderr)
        return 2
    if failures:
        print(
            f"performance-compare=FAIL regressions={','.join(failures)}", file=sys.stderr
        )
        return 1
    print("performance-compare=PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
