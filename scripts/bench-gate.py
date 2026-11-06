#!/usr/bin/env python3
"""Fail closed on missing measurements or >10% paired CI regressions."""
import argparse
import math
import os
from pathlib import Path
import shlex
import sys

QUERIES = {"a", "re", "report 2024", "!tmp log", "zzzz-no-hit-zzzz", "文書"}
NEW_QUERIES = {"*.log", "report*2024*", "path:docs report", "ext:txt;md", "<a|re> !tmp"}
COMMON_WILDCARDS = {"*.log", "report*2024*"}
# load + memory, then first page and full count per query, in-process and IPC.
PAIRED_METRICS = 2 + 2 * 2 * len(QUERIES)
EXPECTED_METRICS = PAIRED_METRICS + 2 * 2 * len(NEW_QUERIES)


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
    if (not QUERIES <= bq.keys() or not QUERIES <= bs.keys()
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
                limit = 16.0 if key == "first_page_ms" and query in COMMON_WILDCARDS and readings[query]["total_hits"] >= 100 else None
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
             "Section 3 absolute targets remain reported goals.", "",
             "| Metric | Base | Head | Limit | Result |", "|---|---:|---:|---:|---|"]
    def cell(value):
        return "observation" if value is None else f"{value:.6f}"
    lines += [f"| {name.replace('|', chr(92) + '|')} | {cell(b)} | {h:.6f} | {cell(limit)} | {'FAIL' if bad else 'pass'} |"
              for name, b, h, limit, bad in rows]
    return "\n".join(lines) + "\n"


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", type=Path)
    parser.add_argument("head", type=Path)
    parser.add_argument("--entries", type=int, default=5_000_000)
    args = parser.parse_args()
    try:
        rows = compare(parse(args.base.read_text(encoding="utf-8"), require_new=False),
                       parse(args.head.read_text(encoding="utf-8")), args.entries)
    except (ValueError, KeyError) as error:
        print(f"benchmark gate refused incomplete evidence: {error}", file=sys.stderr)
        return 2
    report = render(rows)
    print(report)
    if path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(path, "a", encoding="utf-8") as out:
            out.write(report)
    return int(any(row[-1] for row in rows))


if __name__ == "__main__":
    sys.exit(main())
