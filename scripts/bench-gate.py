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
NEW_QUERIES = {"*.log", "report*2024*", "path:docs report", "ext:txt;md", "<a|re> !tmp"}
COMMON_WILDCARDS = {"*.log", "report*2024*"}
# load + memory, then first page and full count per query, in-process and IPC.
PAIRED_METRICS = 2 + 2 * 2 * len(QUERIES)
EXPECTED_METRICS = PAIRED_METRICS + 2 * 2 * len(NEW_QUERIES)
EXPECTED_METADATA_METRICS = 2 + 2 * 2 * len(QUERIES)
EXPECTED_TOTAL_METRICS = EXPECTED_METRICS + EXPECTED_METADATA_METRICS


def number(value):
    value = float(value)
    if not math.isfinite(value) or value < 0:
        raise ValueError("measurements must be finite and nonnegative")
    return value


def parse(text, require_new=True):
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
    expected = QUERIES | NEW_QUERIES if require_new else QUERIES
    if set(queries) != expected:
        raise ValueError("missing or unexpected in-process query set")
    # Every arm must carry the full IPC set: an absent service arm is
    # missing evidence, never a reason to compare fewer metrics.
    if set(service) != expected:
        raise ValueError("missing or unexpected service (IPC) query set")
    return measures, queries, service


def compare(base, head, expected_entries=5_000_000):
    bm, bq, bs = base
    hm, hq, hs = head
    if (set(bq) != QUERIES or set(bs) != QUERIES
            or set(hq) != QUERIES | NEW_QUERIES or set(hs) != QUERIES | NEW_QUERIES):
        raise ValueError("incomplete metric inventory")
    if bm["entries"] != expected_entries or hm["entries"] != expected_entries:
        raise ValueError("wrong fixture size")
    if bm["seed"] != 2026 or hm["seed"] != 2026:
        raise ValueError("wrong fixture seed")
    metrics = [("load total (ms)", bm["load_ms_median"], hm["load_ms_median"], True),
               ("bytes/entry", bm["heap_bytes"] / expected_entries,
                hm["heap_bytes"] / expected_entries, False)]
    for label, before, after in (("in-process", bq, hq), ("IPC", bs, hs)):
        for query in sorted(QUERIES):
            for key in ("page_hits", "total_hits"):
                if before[query][key] != after[query][key]:
                    raise ValueError(f"changed {key}: {label} {query!r}")
            for key in ("first_page_ms", "full_count_ms"):
                metrics.append((f"{label} {query!r} {key}", before[query][key], after[query][key], True))
    if len(metrics) != PAIRED_METRICS:
        raise ValueError(f"expected {PAIRED_METRICS} paired metrics, got {len(metrics)}")
    # 0.1 ms absolute tolerance dominates the relative allowance below 1 ms.
    # Memory has no timing tolerance. Strictly greater fails; the boundary passes.
    rows = [(name, b, h, b + max(b * 0.10, 0.1 if timed else 0),
             h > b + max(b * 0.10, 0.1 if timed else 0))
            for name, b, h, timed in metrics]
    # New syntax has no meaningful baseline on main. Require every reading,
    # verify IPC agrees, and enforce the approved common-wildcard page limit.
    for query in sorted(NEW_QUERIES):
        if any(hq[query][key] != hs[query][key] for key in ("page_hits", "total_hits")):
            raise ValueError(f"inconsistent new-language counts: {query!r}")
        if hq[query]["page_hits"] != min(100, hq[query]["total_hits"]):
            raise ValueError(f"incomplete new-language first page: {query!r}")
        for label, readings in (("in-process", hq), ("IPC", hs)):
            for key in ("first_page_ms", "full_count_ms"):
                limit = 16.0 if key == "first_page_ms" and query in COMMON_WILDCARDS else None
                value = readings[query][key]
                rows.append((f"new-language {label} {query!r} {key}", None, value, limit,
                             limit is not None and value > limit))
    if len(rows) != EXPECTED_METRICS:
        raise ValueError(f"expected {EXPECTED_METRICS} metrics, got {len(rows)}")
    return rows


def render(rows):
    lines = ["## Paired benchmark regression gate", "",
             "Same runner/job; median of 3 loads and 5 queries per arm. "
             "Limit: base + max(10%, 0.1 ms); memory: base + 10%. "
             "All 20 new-syntax head-only observations are required; common wildcard pages must be <= 16 ms. "
             "Section 3 absolute targets remain reported goals.", "",
             "| Metric | Base | Head | Limit | Result |", "|---|---:|---:|---:|---|"]
    def cell(value):
        return "observation" if value is None else f"{value:.6f}"
    lines += [f"| {name.replace('|', chr(92) + '|')} | {cell(b)} | {h:.6f} | {cell(limit)} | {'FAIL' if bad else 'pass'} |"
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
                "metadata_bytes_per_entry", "load_ms_median", "precomputed_build_ms", "lazy_build_ms_median", "lazy_saved_bytes", "freeze_ms"}
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
    expected_extra = 32 + 2 * ((expected_entries + 7) // 8) / expected_entries
    if abs(total-scalars["default_bytes_per_entry"]) > 0.000001 or abs(extra-expected_extra) > 0.000001 or abs(extra-scalars["metadata_bytes_per_entry"]) > 0.000001:
        raise ValueError("inconsistent default-column heap evidence")
    if scalars["lazy_saved_bytes"] != 16*expected_entries:
        raise ValueError("inconsistent lazy-cache saving")
    rows = [("default columns bytes/entry", total), ("default columns load ms", scalars["load_ms_median"])]
    for label, observations, core in (("in-process", engine, head[1]), ("IPC", ipc, head[2])):
        for query in sorted(QUERIES):
            for key in ("page_hits", "total_hits"):
                if observations[query][key] != core[query][key]:
                    raise ValueError(f"modified sort changed {key}: {label} {query!r}")
            rows.extend((f"modified descending {label} {query!r} {key}", observations[query][key]) for key in ("first_page_ms", "full_count_ms"))
    if len(rows) != EXPECTED_METADATA_METRICS:
        raise ValueError("incomplete metadata metric inventory")
    report = "\n## Required head-only metadata observations\n\nMain cannot measure the new sort. These are observations; the existing paired gate remains strict.\n\n| Metric | Head |\n|---|---:|\n"
    report += "".join(f"| {name} | {value:.6f} |\n" for name, value in rows)
    report += f"\nPrecompute: {scalars['precomputed_build_ms']:.6f} ms; deferred build: {scalars['lazy_build_ms_median']:.6f} ms; lazy cache saves {scalars['lazy_saved_bytes']:.0f} bytes before first use.\n"
    report += f"\nSame-tree scan, none/default columns: {scan['none_ms_median']:.6f}/{scan['default_ms_median']:.6f} ms (five alternating samples per arm, 2101 entries).\n"
    return rows, report


def interleaved(measure, kind, query, operation, expected=None, trace=None, observation=None):
    """Warm both arms, then sample ABBA, optionally interleaving head-only C.

    C runs the new-syntax descriptor on head and never participates in the
    paired content comparison. Its own warm-up and five samples must agree.
    """
    samples = {"base": [], "head": []}
    if observation is not None:
        samples["observation"] = []
    signatures, order = {}, []
    trace_index = len(trace) if trace is not None else None
    if trace is not None:
        trace.append("")
    count = 3 if kind == "load" else 5
    forward = tuple(samples)
    for warmup, arms in [(True, forward)] + [
            (False, forward if i % 2 == 0 else forward[::-1]) for i in range(count)]:
        for arm in arms:
            order.append(("warm " if warmup else "") + {"base": "A", "head": "B", "observation": "C"}[arm])
            if trace is not None:
                label = f"{kind} {query!r} {operation}"
                if observation is not None:
                    label += f"; C = head-only {observation[1]!r} {observation[2]}"
                trace[trace_index] = f"| {label.replace('|', chr(92) + '|')} | {' '.join(order)} |"
            sample = measure("head", *observation) if arm == "observation" else measure(arm, kind, query, operation)
            value, hits, digest, heap, entries = sample
            number(value)
            if arm != "observation" and expected is not None and hits != expected:
                raise ValueError(f"remeasure changed hits: {arm} {kind} {query!r}")
            signature = hits, digest, entries
            group = "observation" if arm == "observation" else "paired"
            if group in signatures and signature != signatures[group]:
                raise ValueError("paired sample content/count/fixture mismatch")
            signatures[group] = signature
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
        if not bad or row[1] is None:
            result.append(row)
            continue
        kind, query, operation, expected = descriptors[name]
        samples = interleaved(measure, kind, query, operation, expected, trace)
        b, h = (samples[arm][0] for arm in ("base", "head"))
        limit = b + max(b * 0.10, 0.1)
        result.append((name, b, h, limit, h > limit))
    return result


def probe(args, arm, kind, query, operation):
    if arm == "base" and kind != "load" and query not in QUERIES:
        raise ValueError("base probe accepts only legacy queries")
    command = [str(getattr(args, arm + "_probe")), str(getattr(args, arm + "_binary")),
               str(getattr(args, arm + "_index")), kind, query, operation]
    if arm == "base":
        command.append("--legacy-queries")
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
        new_queries = sorted(NEW_QUERIES)
        for i, query in enumerate(sorted(QUERIES)):
            new_query = new_queries[i] if i < len(new_queries) else None
            page = interleaved(measure, kind, query, "first_page_ms", trace=trace,
                               observation=(kind, new_query, "first_page_ms") if new_query else None)
            full = interleaved(measure, kind, query, "full_count_ms", trace=trace,
                               observation=(kind, new_query, "full_count_ms") if new_query else None)
            for arm in page:
                if page[arm][1] != min(100, full[arm][1]):
                    raise ValueError("page/count evidence disagrees")
                target = "head" if arm == "observation" else arm
                text = new_query if arm == "observation" else query
                output[target].append(f'{prefix} query="{text}" first_page_ms={page[arm][0]:.9f} page_hits={page[arm][1]} full_count_ms={full[arm][0]:.9f} total_hits={full[arm][1]}')
    for arm, lines in output.items():
        getattr(args, arm).write_text("\n".join(lines)+"\n", encoding="utf-8")


def sample_order(trace):
    return ("\n### Sample order (A = base, B = head legacy, C = head-only new syntax; warm samples excluded)\n\n| Metric | Producer order |\n|---|---|\n" + "\n".join(trace) + "\n") if trace else ""


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
    parser.add_argument("--require-metadata", action="store_true")
    parser.add_argument("--head-observations", type=Path)
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
        base = parse(args.base.read_text(encoding="utf-8"), require_new=False)
        head = parse(args.head.read_text(encoding="utf-8"))
        rows = compare(base, head, args.entries)
        report = render(rows)
        if args.require_metadata:
            observations = (args.head_observations or args.head).read_text(encoding="utf-8")
            metadata_rows, metadata_report = metadata_observations(observations, head, args.entries)
            if len(rows) + len(metadata_rows) != EXPECTED_TOTAL_METRICS:
                raise ValueError("incomplete combined metric inventory")
            report += metadata_report
        if any(retry_paths):
            if not all(retry_paths):
                raise ValueError("all paired retry paths are required")
            retryable = any(row[-1] and row[1] is not None and row[0] != "bytes/entry" for row in rows)
            memory_failed = any(row[-1] and row[0] == "bytes/entry" for row in rows)
            if retryable and not memory_failed:
                trace.append("| Single retry phase | Only failing latency metrics |")
            retried = retry_failures(rows, base, head,
                                     lambda *metric: probe(args, *metric), trace)
            if retryable and not memory_failed:
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
