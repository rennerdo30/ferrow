#!/usr/bin/env python3
"""Memory gate for the service's memory per entry (ADR 0007, step 7).

Reads the output of `examples/memory_breakdown` run with 5 000 000 entries on
the Windows runner (the `FERROW-MEMORY*` lines), prints one table and fails,
naming the phase and the number, when a phase needs more memory than its
limit below. A later change then cannot quietly bring the service back above
the owner's budget: never more than 1 GB, target 512 MB, on a 27.2 M entry PC.

Usage: memory-gate.py <memory_breakdown output> [--summary FILE]

`--summary` (CI passes `$GITHUB_STEP_SUMMARY`) also appends the table as
Markdown. Exit codes: 0 pass, 1 a limit is exceeded or a measurement is
missing, 2 the input is not a 5 M entry run.

What is measured (all private bytes, Windows: `PrivateUsage`; the mapped index
and the temporary files are file cache and not counted):

(a) scan: the growth above the point where a full scan starts, per phase and
    in total (the highest point of parse, join, emit, freeze and the live
    layer's tables). It is a fixed working set, not a cost per entry: the
    tables live in temporary files and the builder spills past 32 MiB, so a
    limit in MiB at 5 M entries also catches a per-entry table coming back.
(b) steady: what the process holds after a scan, once the heap builds are
    dropped, and the live layer's own heap in bytes per entry.
(c) compaction: the growth of `Live::compact_into`, in bytes per entry (its
    own live tables are still in the heap, ADR 0007 decision 6).
(d) restart: the growth of mapping the saved index and tables and restoring
    the live layer, and the private bytes afterwards.

Every limit sits in `CHECKS` and `PC` below, with where its number comes from.
Change a limit only with the measured numbers that justify it, in the commit.
"""

import argparse
import re
import sys

MIB = 1 << 20
ENTRIES = 5_000_000  # the run the limits are calibrated for (FERROW_BENCH_ENTRIES default)

# The owner's PC (ADR 0007): 27.2 M entries, never more than 1 GB, target 512 MB.
PC = {
    "entries": 27_200_000,
    "hard_bytes": 1024 * MIB,
    "target_bytes": 512 * MIB,
}

# Each limit is a number from the hosted `memory breakdown (windows 5M)` job
# (windows-latest, release build, 5 M entries) plus a margin. Run to run, the
# scan phases (1 ms sampled peaks) differ by up to about 12 %: parse 13.9 to
# 16.9 MiB, emit 58.3 to 65.1 MiB over eight runs (six on main, 2026-11-18
# 08:54 to 09:40 UTC, two on this gate's branch); the compaction, steady state
# and restart by about 1 to 5 %. So every limit is at least 20 % above the
# highest value seen, and still well inside what one per-entry table brings back
# (4 B/entry is 19 MiB at 5 M entries).
#
# unit "MiB": growth_bytes of a `FERROW-MEMORY-PEAK` line; "B/entry": the same
# divided by the entries of the run.
#
# (id, phase line, what it is, unit, limit, where the limit comes from)
CHECKS = [
    # (a) the full scan path, from the bounded build (the path the service takes)
    ("scan.parse", "bounded.parse", "parse chunks into the file-backed volume builder",
     "MiB", 22,
     "measured 13.9-16.9 MiB over eight runs (16 MiB chunk buffers, fixed), +30 %"),
    ("scan.join", "bounded.from_parsed", "join of names, records and the grouping by parent",
     "MiB", 2,
     "measured 0-0.2 MiB, everything is written to temporary files; +1.8 MiB"),
    ("scan.emit", "bounded.emit", "emission through the spilling builder and the record map file",
     "MiB", 80,
     "measured 58.3-65.1 MiB = the 32 MiB spill budget x 1.6 (shard heap while entries "
     "are added) plus the working set, +23 %; raising the budget raises this"),
    ("scan.freeze", "bounded.freeze", "freeze into the index file (place, fold, sort, checksum in the map)",
     "MiB", 8,
     "measured 4.0-5.0 MiB (4.9 MB at 2 M entries in ADR 0007 decision 8: fixed, not per entry), +60 %"),
    ("scan.live_new", "bounded.live_new", "the live layer's tables built in temporary files",
     "MiB", 8,
     "measured 4.5 MiB, +76 %"),
    ("scan.children", "bounded.children", "the frozen children table for the saved tables",
     "MiB", 1,
     "measured 0.08 MiB"),
]

# The same peaks, taken together: the highest point of the whole scan above
# where it started. (ADR 0007 budget: about 60 MB working set plus the 16 MiB
# read buffers the example does not have.)
SCAN_PHASES = [c[1] for c in CHECKS if c[0].startswith("scan.")]
SCAN_TOTAL = ("scan.total", "full scan, highest point above its start", "MiB", 84,
              "measured 63.2-67.8 MiB (the emit's shards are still alive in the freeze, so the "
              "highest point is the freeze's: emit + freeze), +24 %")
STEADY = ("steady.private", "steady", "private bytes after the scan, heap builds dropped",
          "MiB", 5,
          "measured 6.95 and 7.32 MiB in two hosted runs (the allocator's free memory, 28 MB held, "
          "and the runtime; 1.4 B/entry), +120 %: 2 B/entry more heap would trip it")
STEADY_HEAP = ("steady.heap", "bounded.live", "the live layer's own heap (record tables are mapped)",
               "B/entry", 1.0,
               "measured 0.36 B/entry (removed bits 0.125, record_base remainder 0.23); "
               "a heap table of 1 B/entry or more is a regression")
COMPACT = ("compact.file", "compact.file", "compaction into the index file (Live::compact_into)",
           "B/entry", 15,
           "measured 18.7-21.2 B/entry over six runs (about 60 MB fixed + the new live tables in "
           "the heap, decision 6, still to do), +23 %")
RESTART = ("restart.load", "restart.load", "restart: map index and tables, restore the live layer",
           "MiB", 6,
           "measured 1.36 MiB in both hosted runs (the tables are mapped, only the maps' "
           "bookkeeping and the live layer's bit sets are in the heap), +340 %")
RESTARTED = ("restart.private", "restarted", "private bytes after a restart from the saved files",
             "MiB", 14,
             "measured 5.83 and 6.95 MiB in two hosted runs, +100 %")


def fields(line):
    return dict(re.findall(r"(\w+)=(\S+)", line))


def parse(text):
    """The measured lines by kind and phase."""
    out = {"PEAK": {}, "STEADY": {}, "TOTAL": {}, "ACTIVE": None, "DONE": False}
    for raw in text.splitlines():
        line = raw.strip()
        for kind in ("PEAK", "STEADY", "TOTAL"):
            if line.startswith(f"FERROW-MEMORY-{kind} "):
                f = fields(line)
                out[kind][f["phase"]] = f
        if line.startswith("FERROW-MEMORY-ACTIVE "):
            out["ACTIVE"] = fields(line)
        if line.startswith("FERROW-MEMORY-DONE"):
            out["DONE"] = True
    return out


def number(f, key):
    return float(f[key])


class Row:
    def __init__(self, ident, phase, what, unit, measured, limit, source, status="ok", extra=""):
        self.ident, self.phase, self.what, self.unit = ident, phase, what, unit
        self.measured, self.limit, self.source = measured, limit, source
        self.status, self.extra = status, extra

    def fmt(self, value):
        if value is None:
            return "missing"
        return f"{value:.2f} {self.unit}"


def check(ident, phase, what, unit, measured, limit, source):
    if measured is None:
        return Row(ident, phase, what, unit, None, limit, source, "FAIL", "no measurement in the output")
    status = "ok" if measured <= limit else "FAIL"
    return Row(ident, phase, what, unit, measured, limit, source, status)


def evaluate(data):
    entries = data["ACTIVE"] and int(data["ACTIVE"]["entries"])
    rows = []
    peaks = data["PEAK"]

    def growth(phase, unit):
        f = peaks.get(phase)
        if f is None:
            return None
        g = number(f, "growth_bytes")
        return g / MIB if unit == "MiB" else number(f, "growth_per_entry")

    for ident, phase, what, unit, limit, source in CHECKS:
        rows.append(check(ident, phase, what, unit, growth(phase, unit), limit, source))

    # The whole scan: highest point of its phases above where the scan began.
    ident, what, unit, limit, source = SCAN_TOTAL
    start = peaks.get("bounded.parse")
    seen = [peaks[p] for p in SCAN_PHASES if p in peaks]
    total = None
    if start is not None and len(seen) == len(SCAN_PHASES):
        total = (max(number(f, "peak_bytes") for f in seen) - number(start, "before_bytes")) / MIB
    scan_total = check(ident, "bounded.*", what, unit, total, limit, source)
    rows.append(scan_total)

    # (b) steady state
    steady = data["STEADY"].get("mapped")
    ident, phase, what, unit, limit, source = STEADY
    steady_mib = steady and number(steady, "growth_bytes") / MIB
    rows.append(check(ident, "steady.mapped", what, unit, steady_mib, limit, source))
    ident, phase, what, unit, limit, source = STEADY_HEAP
    total_line = data["TOTAL"].get("bounded.live")
    heap = total_line and number(total_line, "accounted_per_entry")
    rows.append(check(ident, phase, what, unit, heap, limit, source))

    # (c) compaction
    ident, phase, what, unit, limit, source = COMPACT
    rows.append(check(ident, phase, what, unit, growth(phase, unit), limit, source))

    # (d) restart
    ident, phase, what, unit, limit, source = RESTART
    rows.append(check(ident, phase, what, unit, growth(phase, unit), limit, source))
    restarted = data["STEADY"].get("restarted")
    ident, phase, what, unit, limit, source = RESTARTED
    restarted_mib = restarted and number(restarted, "growth_bytes") / MIB
    rows.append(check(ident, "restarted", what, unit, restarted_mib, limit, source))

    # Extrapolation to the owner's PC. Every byte of growth measured at 5 M
    # entries is scaled by the entry count, although the scan's working set is
    # fixed: an upper bound, never an estimate to quote as the PC's figure.
    scale = PC["entries"] / entries if entries else None
    ext = []

    def per_pc(mib):
        return None if mib is None or scale is None else mib * scale

    s = per_pc(steady_mib)
    scan = per_pc(scan_total.measured)
    comp = per_pc(growth("compact.file", "MiB"))
    rst = per_pc(restarted_mib)
    rl = per_pc(growth("restart.load", "MiB"))

    def add(ident, what, mib, must_meet_target):
        row = Row(ident, "27.2M entries", what, "MiB", mib, PC["target_bytes"] / MIB,
                  "ADR 0007: 1 GB hard, 512 MB target (owner, 2026-11-18); growth at 5 M x 5.44")
        if mib is None:
            row.status, row.extra = "FAIL", "no measurement in the output"
        elif mib > PC["hard_bytes"] / MIB:
            row.status, row.extra = "FAIL", f"over the hard limit of {PC['hard_bytes'] // MIB} MiB"
        elif mib > PC["target_bytes"] / MIB:
            row.status = "FAIL" if must_meet_target else "over target"
        ext.append(row)

    def plus(a, b):
        return None if a is None or b is None else a + b

    add("pc.steady", "steady state", s, True)
    add("pc.scan", "steady + a full scan (rescan)", plus(s, scan), True)
    add("pc.compact", "steady + a compaction", plus(s, comp), False)
    add("pc.restart", "a restart", plus(rst, rl), True)
    return rows, ext


def render(rows, ext, entries):
    lines = [f"Memory gate: {entries:,} entries (windows-latest, private bytes)", ""]
    head = f"{'check':<16}{'phase':<22}{'measured':>16}{'limit':>16}  status"
    for title, group in (("limits at 5M entries", rows), ("extrapolated to the owner's PC (upper bound)", ext)):
        lines += [title, head, "-" * len(head)]
        for r in group:
            lines.append(f"{r.ident:<16}{r.phase:<22}{r.fmt(r.measured):>16}{r.fmt(r.limit):>16}  {r.status}"
                         + (f" ({r.extra})" if r.extra else ""))
            lines.append(f"{'':<16}{r.what}")
        lines.append("")
    return "\n".join(lines)


def markdown(rows, ext, entries):
    out = [f"## Memory gate ({entries:,} entries)", ""]
    for title, group in (("Limits at 5M entries", rows),
                         ("Extrapolated to the owner's PC: 27.2M entries, 1 GB hard, 512 MB target (upper bound)", ext)):
        out += [f"### {title}", "", "| check | phase | what | measured | limit | status |", "|---|---|---|---:|---:|---|"]
        for r in group:
            status = r.status if r.status == "ok" else f"**{r.status}**"
            out.append(f"| {r.ident} | {r.phase} | {r.what} | {r.fmt(r.measured)} | {r.fmt(r.limit)} | {status} |")
        out.append("")
    return "\n".join(out)


def main(argv):
    ap = argparse.ArgumentParser()
    ap.add_argument("output")
    ap.add_argument("--summary")
    args = ap.parse_args(argv)
    with open(args.output, encoding="utf-8", errors="replace") as f:
        data = parse(f.read())
    active = data["ACTIVE"]
    if active is None or not data["DONE"]:
        print("MEMORY GATE FAILED: the output has no FERROW-MEMORY-ACTIVE and FERROW-MEMORY-DONE "
              "lines: the measurement did not run to its end", file=sys.stderr)
        return 1
    if int(active["entries"]) != ENTRIES:
        print(f"MEMORY GATE: the limits are calibrated for {ENTRIES} entries, this run has "
              f"{active['entries']}", file=sys.stderr)
        return 2
    rows, ext = evaluate(data)
    entries = int(active["entries"])
    print(render(rows, ext, entries))
    if args.summary:
        with open(args.summary, "a", encoding="utf-8") as f:
            f.write(markdown(rows, ext, entries) + "\n")
    bad = [r for r in rows + ext if r.status == "FAIL"]
    for r in rows + ext:
        if r.status == "over target":
            print(f"::warning title=Memory gate::{r.phase} {r.what}: {r.fmt(r.measured)} extrapolated "
                  f"is over the 512 MB target (known: compaction keeps its live tables in the heap)")
    for r in bad:
        reason = r.extra or f"{r.fmt(r.measured)} is over the limit of {r.fmt(r.limit)}"
        print(f"::error title=Memory gate::{r.ident} ({r.phase}): {r.what}: {reason}", file=sys.stderr)
    if bad:
        print("MEMORY GATE FAILED: " + "; ".join(
            f"{r.ident} [{r.phase}] {r.fmt(r.measured)} > {r.fmt(r.limit)}" if r.measured is not None
            else f"{r.ident} [{r.phase}] no measurement" for r in bad), file=sys.stderr)
        return 1
    print("MEMORY GATE PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
