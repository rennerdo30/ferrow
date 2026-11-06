#!/usr/bin/env python3
"""Fail closed on missing measurements or >10% paired CI regressions."""
import argparse
import math
import os
from pathlib import Path
import shlex
import sys
import subprocess
import statistics

QUERIES = {"a", "re", "report 2024", "!tmp log", "zzzz-no-hit-zzzz", "文書"}
# load + memory, then first page and full count per query, in-process and IPC.
EXPECTED_METRICS = 2 + 2 * 2 * len(QUERIES)


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


def interleaved(measure, kind, query, operation, expected=None, trace=None):
    """Warm both arms, then take 3 load or 5 query samples in ABBA order."""
    samples = {"base": [], "head": []}
    signatures, order = {}, []
    trace_index = len(trace) if trace is not None else None
    if trace is not None:
        trace.append("")
    count = 3 if kind == "load" else 5
    for warmup, arms in [(True, ("base", "head"))] + [
            (False, ("base", "head") if i % 2 == 0 else ("head", "base")) for i in range(count)]:
        for arm in arms:
            order.append(("warm " if warmup else "") + ("A" if arm == "base" else "B"))
            if trace is not None:
                trace[trace_index] = f"| {kind} {query!r} {operation} | {' '.join(order)} |"
            sample = measure(arm, kind, query, operation)
            value, hits, digest, heap, entries = sample
            number(value)
            if expected is not None and hits != expected:
                raise ValueError(f"remeasure changed hits: {arm} {kind} {query!r}")
            signature = hits, digest, entries
            if signatures and signature != next(iter(signatures.values())):
                raise ValueError("paired sample content/count/fixture mismatch")
            signatures[arm] = signature
            if samples[arm] and heap != samples[arm][0][3]:
                raise ValueError("heap changed within one arm")
            if not warmup:
                samples[arm].append(sample)
    return {arm: (statistics.median(s[0] for s in values), *values[0][1:]) for arm, values in samples.items()}


def retry_failures(rows, base, head, measure, trace=None):
    """Validate first, then retry each failing latency exactly once per arm.

    The callback runs one bounded producer sample; both arms are interleaved.
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
        samples = interleaved(measure, kind, query, operation, expected, trace)
        b, h = (samples[arm][0] for arm in ("base", "head"))
        limit = b + max(b * 0.10, 0.1)
        result.append((name, b, h, limit, h > limit))
    return result


def probe(args, arm, kind, query, operation):
    command = [str(getattr(args, arm + "_probe")), str(getattr(args, arm + "_binary")),
               str(getattr(args, arm + "_index")), kind, query, operation]
    try:
        run = subprocess.run(command, capture_output=True, text=True, encoding="utf-8", timeout=120, check=True)
    except subprocess.CalledProcessError as error:
        if error.stderr:
            print(error.stderr, file=sys.stderr, end="")
        raise
    if getattr(run, "stderr", ""):
        print(run.stderr, file=sys.stderr, end="")
    records = [line for line in run.stdout.splitlines() if line.startswith("FERROW-RETRY ")]
    if len(records) != 1:
        raise ValueError("retry producer must emit exactly one receipt")
    fields = dict(word.split("=", 1) for word in records[0].split()[1:])
    integers = tuple(int(fields[key]) for key in ("hits", "digest", "heap_bytes", "entries"))
    if any(v < 0 for v in integers) or integers[-1] != args.entries or integers[-2] <= 0:
        raise ValueError("invalid probe count, heap or fixture")
    return number(fields["value_ms"]), *integers


def measure_pair(args, measure, trace):
    if args.base_index.resolve() != args.head_index.resolve():
        raise ValueError("interleaved arms require the same fixture file")
    output = {"base": [], "head": []}
    load = interleaved(measure, "load", "", "load", args.entries, trace)
    for arm, (elapsed, _, _, heap, entries) in load.items():
        output[arm].append(f"FERROW-MEASURE entries={entries} seed=2026 heap_bytes={heap} bytes_per_entry={heap/entries:.9f} load_ms_median={elapsed:.9f}")
    for kind, prefix in (("in-process", "FERROW-QUERY"), ("IPC", "FERROW-SERVICE")):
        for query in sorted(QUERIES):
            page = interleaved(measure, kind, query, "first_page_ms", trace=trace)
            full = interleaved(measure, kind, query, "full_count_ms", trace=trace)
            for arm in output:
                if page[arm][1] != min(100, full[arm][1]):
                    raise ValueError("page/count evidence disagrees")
                output[arm].append(f'{prefix} query="{query}" first_page_ms={page[arm][0]:.9f} page_hits={page[arm][1]} full_count_ms={full[arm][0]:.9f} total_hits={full[arm][1]}')
    for arm, lines in output.items():
        getattr(args, arm).write_text("\n".join(lines)+"\n", encoding="utf-8")


def sample_order(trace):
    return ("\n### Sample order (A = base, B = head; warm samples excluded)\n\n| Metric | Producer order |\n|---|---|\n" + "\n".join(trace) + "\n") if trace else ""


def publish(report):
    print(report)
    if path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(path, "a", encoding="utf-8") as out:
            out.write(report)


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", type=Path)
    parser.add_argument("head", type=Path)
    parser.add_argument("--entries", type=int, default=5_000_000)
    parser.add_argument("--measure", action="store_true", help="collect initial interleaved medians before comparison")
    for arm in ("base", "head"):
        for field in ("probe", "binary", "index"):
            parser.add_argument(f"--{arm}-{field}", type=Path)
    args = parser.parse_args()
    trace = []
    report = ""
    try:
        retry_paths = [getattr(args, arm + "_" + field) for arm in ("base", "head") for field in ("probe", "binary", "index")]
        if (any(retry_paths) or args.measure) and not all(retry_paths):
            raise ValueError("all paired producer paths are required")
        if args.measure:
            trace.append("| Initial phase | Warm both arms, then interleave samples |")
            measure_pair(args, lambda *metric: probe(args, *metric), trace)
        rows = compare(parse(args.base.read_text(encoding="utf-8")),
                       parse(args.head.read_text(encoding="utf-8")), args.entries)
        report = render(rows)
        if any(retry_paths):
            if not all(retry_paths):
                raise ValueError("all paired retry paths are required")
            if any(row[-1] for row in rows):
                trace.append("| Single retry phase | Only failing latency metrics |")
            retried = retry_failures(rows, parse(args.base.read_text(encoding="utf-8")),
                                     parse(args.head.read_text(encoding="utf-8")),
                                     lambda *metric: probe(args, *metric), trace)
            if any(row[-1] for row in rows) and not any(row[-1] and row[0] == "bytes/entry" for row in rows):
                report += "\n### One paired latency remeasurement (final decision)\n\n" + render(retried)
            rows = retried
    except (ValueError, KeyError, OSError, subprocess.SubprocessError) as error:
        print(f"benchmark gate refused incomplete evidence: {error}", file=sys.stderr)
        publish(report + f"\nBenchmark evidence refused: {error}\n" + sample_order(trace))
        return 2
    publish(report + sample_order(trace))
    return int(any(row[-1] for row in rows))


if __name__ == "__main__":
    sys.exit(main())
