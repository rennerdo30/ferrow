#!/usr/bin/env python3
"""Fail closed on missing measurements or >10% paired CI regressions."""
import argparse
import math
import os
from pathlib import Path
import shlex
import sys
import subprocess

QUERIES = {"a", "re", "report 2024", "!tmp log", "zzzz-no-hit-zzzz", "文書"}
# load + memory, then first page and full count per query, in-process and IPC.
EXPECTED_METRICS = 2 + 2 * 2 * len(QUERIES)
# New sorts have no comparable main arm yet. Require all observations, while
# preserving the complete strict paired inventory above (and other extensions).
EXPECTED_METADATA_METRICS = 2 + 2 * 2 * len(QUERIES)
EXPECTED_TOTAL_METRICS = EXPECTED_METRICS + EXPECTED_METADATA_METRICS


def number(value):
    value = float(value)
    if not math.isfinite(value) or value < 0:
        raise ValueError("measurements must be finite and nonnegative")
    return value


def parse(text):
    measures, queries, service = {}, {}, {}
    for line in text.splitlines():
        prefix, _, body = line.partition(" ")
        if prefix not in {"FERROW-MEASURE", "FERROW-QUERY", "FERROW-SERVICE"}:
            continue
        fields = dict(word.split("=", 1) for word in shlex.split(body))
        if prefix == "FERROW-MEASURE":
            if measures.keys() & fields.keys():
                raise ValueError("duplicate scalar measurement")
            measures.update({key: number(value) for key, value in fields.items()})
        else:
            query = fields.pop("query")
            target = service if prefix == "FERROW-SERVICE" else queries
            if query in target:
                raise ValueError("duplicate query measurement")
            target[query] = {key: number(fields[key]) for key in (
                "first_page_ms", "full_count_ms", "page_hits", "total_hits")}
    for key in ("entries", "seed", "heap_bytes", "bytes_per_entry", "load_ms_median"):
        if key not in measures:
            raise ValueError(f"missing measurement: {key}")
    if measures["entries"] <= 0 or measures["heap_bytes"] <= 0:
        raise ValueError("empty fixture")
    if set(queries) != QUERIES:
        raise ValueError("missing or unexpected in-process query set")
    # Every arm must carry the full IPC set: an absent service arm is
    # missing evidence, never a reason to compare fewer metrics.
    if set(service) != QUERIES:
        raise ValueError("missing or unexpected service (IPC) query set")
    return measures, queries, service


def compare(base, head, expected_entries=5_000_000):
    bm, bq, bs = base
    hm, hq, hs = head
    if bm["entries"] != expected_entries or hm["entries"] != expected_entries:
        raise ValueError("wrong fixture size")
    if bm["seed"] != 2026 or hm["seed"] != 2026:
        raise ValueError("wrong fixture seed")
    metrics = [("load total (ms)", bm["load_ms_median"], hm["load_ms_median"], True),
               ("bytes/entry", bm["heap_bytes"] / expected_entries,
                hm["heap_bytes"] / expected_entries, False)]
    for label, before, after in (("in-process", bq, hq), ("IPC", bs, hs)):
        for query in sorted(before):
            for key in ("page_hits", "total_hits"):
                if before[query][key] != after[query][key]:
                    raise ValueError(f"changed {key}: {label} {query!r}")
            for key in ("first_page_ms", "full_count_ms"):
                metrics.append((f"{label} {query!r} {key}", before[query][key], after[query][key], True))
    if len(metrics) != EXPECTED_METRICS:
        raise ValueError(f"expected {EXPECTED_METRICS} metrics, got {len(metrics)}")
    # 0.1 ms absolute tolerance dominates the relative allowance below 1 ms.
    # Memory has no timing tolerance. Strictly greater fails; the boundary passes.
    return [(name, b, h, b + max(b * 0.10, 0.1 if timed else 0),
             h > b + max(b * 0.10, 0.1 if timed else 0))
            for name, b, h, timed in metrics]


def render(rows):
    lines = ["## Paired benchmark regression gate", "",
             "Same runner/job; median of 3 loads and 5 queries per arm. "
             "Limit: base + max(10%, 0.1 ms); memory: base + 10%. "
             "Section 3 absolute targets remain reported goals.", "",
             "| Metric | Base | Head | Limit | Result |", "|---|---:|---:|---:|---|"]
    lines += [f"| {name} | {b:.6f} | {h:.6f} | {limit:.6f} | {'FAIL' if bad else 'pass'} |"
              for name, b, h, limit, bad in rows]
    return "\n".join(lines) + "\n"


def metadata_observations(text, head, expected_entries=5_000_000):
    scalars, engine, ipc, scan = {}, {}, {}, {}
    for line in text.splitlines():
        prefix, _, body = line.partition(" ")
        if prefix not in {"FERROW-METADATA", "FERROW-MODIFIED-QUERY", "FERROW-MODIFIED-SERVICE", "FERROW-SCAN"}:
            continue
        fields = dict(word.split("=", 1) for word in shlex.split(body))
        if prefix in {"FERROW-METADATA", "FERROW-SCAN"}:
            target = scalars if prefix == "FERROW-METADATA" else scan
            if target.keys() & fields.keys():
                raise ValueError("duplicate metadata/scan scalar")
            target.update({key: number(value) for key, value in fields.items()})
        else:
            query = fields.pop("query")
            target = engine if prefix == "FERROW-MODIFIED-QUERY" else ipc
            if query in target:
                raise ValueError("duplicate modified-sort query")
            target[query] = {key: number(fields[key]) for key in ("first_page_ms", "full_count_ms", "page_hits", "total_hits")}
    required = {"entries", "seed", "core_heap_bytes", "default_heap_bytes", "default_bytes_per_entry",
                "metadata_bytes_per_entry", "load_ms_median", "core_load_ms_median", "cold_default_heap_bytes", "sort_during_load", "lazy_build_ms_median", "lazy_saved_bytes", "freeze_ms"}
    if not required <= scalars.keys() or set(engine) != QUERIES or set(ipc) != QUERIES:
        raise ValueError("missing metadata observations")
    if not {"entries", "errors", "none_ms_median", "default_ms_median"} <= scan.keys() or scan["entries"] != 2101 or scan["errors"] != 0:
        raise ValueError("invalid scan-cost observations")
    if scalars["entries"] != expected_entries or scalars["seed"] != 2026:
        raise ValueError("wrong metadata fixture")
    if scalars["core_heap_bytes"] != head[0]["heap_bytes"]:
        raise ValueError("metadata core heap changed")
    total = scalars["default_heap_bytes"] / expected_entries
    extra = (scalars["default_heap_bytes"] - scalars["core_heap_bytes"]) / expected_entries
    if abs(total-scalars["default_bytes_per_entry"]) > 0.000001 or abs(extra-scalars["metadata_bytes_per_entry"]) > 0.000001:
        raise ValueError("inconsistent default-column heap evidence")
    cold = scalars["cold_default_heap_bytes"]
    if not scalars["core_heap_bytes"] <= cold <= scalars["default_heap_bytes"]:
        raise ValueError("invalid cold metadata heap")
    cached = (6 if expected_entries <= 2**24 else 8)*expected_entries
    if scalars["lazy_saved_bytes"] != cached or scalars["default_heap_bytes"]-cold != cached:
        raise ValueError("inconsistent lazy-cache saving")
    if total > 66 or extra > 16.5:
        raise ValueError("default-column memory target exceeded")
    if scalars["core_load_ms_median"] <= 0 or scalars["load_ms_median"] > scalars["core_load_ms_median"]*1.15:
        raise ValueError("default-column load target exceeded (core + 15%)")
    if scalars["sort_during_load"] != 0:
        raise ValueError("metadata sort built during load")
    rows = [("default columns bytes/entry", total), ("default columns load ms", scalars["load_ms_median"])]
    for label, observations, core in (("in-process", engine, head[1]), ("IPC", ipc, head[2])):
        for query in sorted(QUERIES):
            for key in ("page_hits", "total_hits"):
                if observations[query][key] != core[query][key]:
                    raise ValueError(f"modified sort changed {key}: {label} {query!r}")
            if query in {"a", "re", "!tmp log", "文書"} and observations[query]["first_page_ms"] > 16:
                raise ValueError(f"modified-sort first-page target exceeded: {label} {query!r}")
            rows.extend((f"modified descending {label} {query!r} {key}", observations[query][key]) for key in ("first_page_ms", "full_count_ms"))
    if len(rows) != EXPECTED_METADATA_METRICS:
        raise ValueError("incomplete metadata metric inventory")
    report = "\n## Required head-only metadata observations\n\nMain cannot measure the new sort. These are observations; the existing paired gate remains strict.\n\n| Metric | Head |\n|---|---:|\n"
    report += "".join(f"| {name} | {value:.6f} |\n" for name, value in rows)
    report += f"\nDeferred build: {scalars['lazy_build_ms_median']:.6f} ms; lazy cache saves {scalars['lazy_saved_bytes']:.0f} bytes before first use.\n"
    report += f"\nSame-tree scan, none/default columns: {scan['none_ms_median']:.6f}/{scan['default_ms_median']:.6f} ms (five alternating samples per arm, 2101 entries).\n"
    return rows, report


def retry_failures(rows, base, head, measure):
    """Validate first, then retry each failing latency exactly once per arm.

    The callback runs a bounded producer and returns (median_ms, hit_count).
    Memory regressions cannot be recovered by remeasurement.
    """
    if any(bad and name == "bytes/entry" for name, _, _, _, bad in rows):
        return rows
    descriptors = {"load total (ms)": ("load", "", "load", base[0]["entries"])}
    for label, queries in (("in-process", base[1]), ("IPC", base[2])):
        for query, values in queries.items():
            for operation, hits in (("first_page_ms", "page_hits"), ("full_count_ms", "total_hits")):
                descriptors[f"{label} {query!r} {operation}"] = (label, query, operation, values[hits])
    result = []
    for row in rows:
        name, _, _, _, bad = row
        if not bad:
            result.append(row)
            continue
        kind, query, operation, expected = descriptors[name]
        samples = []
        for arm in ("base", "head"):
            value, hits = measure(arm, kind, query, operation)
            samples.append(number(value))
            if number(hits) != expected:
                raise ValueError(f"retry changed hits: {arm} {name}")
        b, h = samples
        limit = b + max(b * 0.10, 0.1)
        result.append((name, b, h, limit, h > limit))
    return result


def probe(args, arm, kind, query, operation):
    command = [str(getattr(args, arm + "_probe")), str(getattr(args, arm + "_binary")),
               str(getattr(args, arm + "_index")), kind, query, operation]
    run = subprocess.run(command, capture_output=True, text=True, encoding="utf-8", timeout=120, check=True)
    records = [line for line in run.stdout.splitlines() if line.startswith("FERROW-RETRY ")]
    if len(records) != 1:
        raise ValueError("retry producer must emit exactly one receipt")
    fields = dict(word.split("=", 1) for word in records[0].split()[1:])
    return number(fields["value_ms"]), number(fields["hits"])


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", type=Path)
    parser.add_argument("head", type=Path)
    parser.add_argument("--entries", type=int, default=5_000_000)
    parser.add_argument("--require-metadata", action="store_true")
    for arm in ("base", "head"):
        for field in ("probe", "binary", "index"):
            parser.add_argument(f"--{arm}-{field}", type=Path)
    args = parser.parse_args()
    try:
        rows = compare(parse(args.base.read_text(encoding="utf-8")),
                       parse(args.head.read_text(encoding="utf-8")), args.entries)
        report = render(rows)
        if args.require_metadata:
            _, metadata_report = metadata_observations(args.head.read_text(encoding="utf-8"),
                                                       parse(args.head.read_text(encoding="utf-8")), args.entries)
            report += metadata_report
        retry_paths = [getattr(args, arm + "_" + field) for arm in ("base", "head")
                       for field in ("probe", "binary", "index")]
        if any(retry_paths):
            if not all(retry_paths):
                raise ValueError("all paired retry paths are required")
            retried = retry_failures(rows, parse(args.base.read_text(encoding="utf-8")),
                                     parse(args.head.read_text(encoding="utf-8")),
                                     lambda *metric: probe(args, *metric))
            if any(row[-1] for row in rows) and not any(row[-1] and row[0] == "bytes/entry" for row in rows):
                report += "\n### One paired latency remeasurement (final decision)\n\n" + render(retried)
            rows = retried
    except (ValueError, KeyError, OSError, subprocess.SubprocessError) as error:
        print(f"benchmark gate refused incomplete evidence: {error}", file=sys.stderr)
        return 2
    print(report)
    if path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(path, "a", encoding="utf-8") as out:
            out.write(report)
    return int(any(row[-1] for row in rows))


if __name__ == "__main__":
    sys.exit(main())
