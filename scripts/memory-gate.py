#!/usr/bin/env python3
"""Memory gate for the service's memory per entry (ADR 0007, step 7).

Reads the output of `examples/memory_breakdown` run twice on the Windows
runner, with 5 000 000 and with 1 000 000 entries (the `FERROW-MEMORY*`
lines), prints the tables and fails, naming the phase and the number, when a
phase needs more memory than its limit below. A later change then cannot
quietly bring the service back above the owner's budget: never more than 1 GB,
target 512 MB, on a 27.2 M entry PC.

Usage: memory-gate.py <5M output> <1M output> [--summary FILE]

`--summary` (CI passes `$GITHUB_STEP_SUMMARY`) also appends the tables as
Markdown. Exit codes: 0 pass, 1 a limit is exceeded or a measurement is
missing or not credible, 2 the input is not the two runs the limits are
calibrated for.

What is measured (all private bytes, Windows: `PrivateUsage`; the mapped index
and the temporary files are file cache and not counted):

(a) scan: the growth above the point where a full scan starts, per phase and
    in total (the highest point of parse, join, emit, freeze and the live
    layer's tables). Its working set is fixed, not a cost per entry: the
    tables live in temporary files and the builder spills past its budget.
(b) steady: what the process holds after a scan, once the heap builds are
    dropped and without giving the allocator's free memory back (the service
    never calls `HeapCompact`), and the live layer's own heap in bytes per
    entry. The same figure after a `HeapCompact` is shown as a diagnostic.
(c) compaction: the growth of `Live::compact_into`.
(d) restart: the growth of saving the live tables, of mapping the saved index
    and tables and restoring the live layer, and the private bytes afterwards;
    then the first sort orders built on the mapped index.

Two lines of defence, both here:

1. Absolute limits at 5 M entries. A fixed working set (the emit's shards
   alone are about the spill budget x 1.6) and the runner's noise leave a few
   bytes per entry of room under them, so a small per-entry table could hide.
2. The SLOPE: (value at 5 M - value at 1 M) / (entries at 5 M - entries at
   1 M), in bytes per entry. The fixed part and most of the noise cancel in
   the difference, so a table of 2 bytes per entry brought back into the
   scan's emit, parse or total or the compaction moves the slope by 2 and
   trips it. The run uses an 8 MiB spill budget (`SPILL_BUDGET`) so that the
   emit's fixed part is small and the same at both sizes: a 1 M entry run
   writes about 32 MiB of shards, which would not fill the production budget.

The extrapolation to the owner's PC is a fit from the same two sizes, fixed
part + slope x 27.2 M, not the 5 M figure scaled by the entry count.

Every limit sits below, with where its number comes from. Change a limit only
with the measured numbers that justify it, in the commit.
"""

import argparse
import re
import sys

MIB = 1 << 20
ENTRIES_LARGE = 5_000_000  # FERROW_BENCH_ENTRIES of the two runs the limits are calibrated for
ENTRIES_SMALL = 1_000_000
SPILL_BUDGET = 8 * MIB  # FERROW_MEMORY_SPILL_BUDGET of both runs
# What the service's own 32 MiB budget adds to a scan's highest point over the 8 MiB
# of the run: the same example at 32 MiB measured 62.8 to 67.8 MiB for the scan total
# (eight runs before this change), 15.4 MiB at 8 MiB: about 47 to 52 MiB more, so 52.
PRODUCTION_BUDGET_MIB = 52

# The owner's PC (ADR 0007): 27.2 M entries, never more than 1 GB, target 512 MB.
PC = {
    "entries": 27_200_000,
    "hard_bytes": 1024 * MIB,
    "target_bytes": 512 * MIB,
}

# PROVISIONAL (calibration run): every limit below is replaced with the CI numbers.
# unit "MiB": the growth of a `FERROW-MEMORY-PEAK` line; "B/entry": the same
# divided by the entries of the run.
#
# (id, phase line, what it is, unit, limit, floor, where the limit comes from)
# A floor is the least a phase must grow: below it the sampler or the phase did
# not run (a dead sampler reads as no growth and would pass every limit).
CHECKS = [
    # (a) the full scan path, from the bounded build (the path the service takes)
    ("scan.parse", "bounded.parse", "parse chunks into the file-backed volume builder",
     "MiB", 22, 0, "provisional"),
    ("scan.join", "bounded.from_parsed", "join of names, records and the grouping by parent",
     "MiB", 2, None, "provisional"),
    ("scan.emit", "bounded.emit", "emission through the spilling builder and the record map file",
     "MiB", 80, 0, "provisional"),
    ("scan.freeze", "bounded.freeze", "freeze into the index file (place, fold, sort, checksum in the map)",
     "MiB", 8, None, "provisional"),
    ("scan.live_new", "bounded.live_new", "the live layer's tables built in temporary files",
     "MiB", 8, None, "provisional"),
    ("scan.children", "bounded.children", "the frozen children table for the saved tables",
     "MiB", 1, None, "provisional"),
]

SCAN_PHASES = [c[1] for c in CHECKS if c[0].startswith("scan.")]
SCAN_TOTAL = ("scan.total", "full scan, highest point above its start", "MiB", 84, None, "provisional")
STEADY = ("steady.private", "steady", "private bytes after the scan, heap builds dropped, nothing given back",
          "MiB", 16, None, "provisional")
STEADY_HEAP = ("steady.heap", "bounded.live", "the live layer's own heap (record tables are mapped)",
               "B/entry", 1.0, None, "provisional")
COMPACT = ("compact.file", "compact.file", "compaction into the index file (Live::compact_into)",
           "B/entry", 26, None, "provisional")
SAVE_TABLES = ("restart.save_tables", "restart.save_tables", "restart: stream the live tables to live.bin",
               "MiB", 4, None, "provisional")
RESTART = ("restart.load", "restart.load", "restart: map index and tables, restore the live layer",
           "MiB", 6, None, "provisional")
RESTARTED = ("restart.private", "restarted", "private bytes after a restart from the saved files",
             "MiB", 14, None, "provisional")
SORT = ("restart.sort", "restart.sort_orders", "first sort orders (size, date) on the mapped index",
        "MiB", 16, None, "provisional")

# The slope checks: (id, measure, what, limit in B/entry, where the limit comes from)
SLOPES = [
    ("slope.parse", "parse", "scan parse, growth per entry between 1 M and 5 M", 100.0, "provisional"),
    ("slope.emit", "emit", "scan emit, growth per entry between 1 M and 5 M", 100.0, "provisional"),
    ("slope.total", "total", "scan total, growth per entry between 1 M and 5 M", 100.0, "provisional"),
    ("slope.compact", "compact", "compaction, growth per entry between 1 M and 5 M", 100.0, "provisional"),
]


class BadLine(Exception):
    """A measurement line the gate cannot read; the message names it."""


def fields(line):
    """The key=value pairs of a measurement line, up to its free text (` note=`)."""
    return dict(re.findall(r"(\w+)=(\S+)", line.split(" note=", 1)[0]))


def parse(text):
    """The measured lines by kind and phase."""
    out = {"PEAK": {}, "STEADY": {}, "TOTAL": {}, "BUDGET": None, "ACTIVE": None, "SAMPLER": None, "DONE": False}
    for n, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        for kind in ("PEAK", "STEADY", "TOTAL"):
            if line.startswith(f"FERROW-MEMORY-{kind} "):
                f = fields(line)
                f["_line"] = (n, line)
                if "phase" not in f:
                    raise BadLine(f"line {n}: a {kind} line without phase=: {line[:240]}")
                out[kind][f["phase"]] = f
        if line.startswith("FERROW-MEMORY-ACTIVE "):
            out["ACTIVE"] = fields(line)
        if line.startswith("FERROW-MEMORY-SAMPLER "):
            out["SAMPLER"] = fields(line)
        if line.startswith("FERROW-MEMORY-CONFIG ") and "spill_budget_bytes=" in line:
            out["BUDGET"] = fields(line).get("spill_budget_bytes")
        if line.startswith("FERROW-MEMORY-DONE"):
            out["DONE"] = True
    return out


def number(f, key):
    try:
        return float(f[key])
    except (KeyError, ValueError):
        n, line = f.get("_line", (0, "?"))
        raise BadLine(f"line {n}: no number {key}= in: {line[:240]}") from None


class Run:
    """One run of the example: its lines and the figures the gate reads from them."""

    def __init__(self, label, data):
        self.label, self.data = label, data
        self.peaks, self.steady_lines = data["PEAK"], data["STEADY"]

    @property
    def entries(self):
        """The index's entry count (a run asks for n names and gets a few more, hard links)."""
        return number(self.steady_lines["mapped"], "entries") if "mapped" in self.steady_lines else None

    def growth(self, phase):
        """Growth of a phase's peak above where it started, in bytes (None: no such line)."""
        f = self.peaks.get(phase)
        return None if f is None else number(f, "growth_bytes")

    def scan_total(self):
        """The highest point of the scan's phases above where the scan began, in bytes."""
        start = self.peaks.get("bounded.parse")
        seen = [self.peaks[p] for p in SCAN_PHASES if p in self.peaks]
        if start is None or len(seen) != len(SCAN_PHASES):
            return None
        return max(number(f, "peak_bytes") for f in seen) - number(start, "before_bytes")

    def steady(self, phase, key="growth_bytes"):
        f = self.steady_lines.get(phase)
        return None if f is None else number(f, key)

    def restart_peak(self):
        """The restart's highest private bytes above the process's baseline, in bytes."""
        f, s = self.peaks.get("restart.load"), self.steady_lines.get("restarted")
        if f is None or s is None:
            return None
        return number(f, "peak_bytes") - number(s, "baseline_bytes")

    def measure(self, name):
        return {
            "parse": lambda: self.growth("bounded.parse"),
            "emit": lambda: self.growth("bounded.emit"),
            "total": self.scan_total,
            "compact": lambda: self.growth("compact.file"),
            "steady": lambda: self.steady("mapped"),
            "restart": self.restart_peak,
        }[name]()

    def credibility(self):
        """Why the run's peaks cannot be believed (a dead sampler reads as no growth), or []."""
        out = []
        s = self.data["SAMPLER"]
        if s is None:
            out.append("no FERROW-MEMORY-SAMPLER line: the peak sampler was never proven")
        elif s.get("ok") != "true":
            out.append(f"the peak sampler self-check failed (a 48 MiB block held for 30 ms showed as "
                       f"{s.get('seen_bytes', '?')} bytes): the sampler is dead, so every peak reads as no growth")
        for phase in sorted(set(SCAN_PHASES + [c[1] for c in (COMPACT, SAVE_TABLES, RESTART, SORT)])):
            f = self.peaks.get(phase)
            if f is None:
                continue
            if f.get("sampler") != "ok":
                out.append(f"{phase}: the platform gave no peak (sampler={f.get('sampler', 'absent')})")
            elif number(f, "peak_bytes") == 0:
                out.append(f"{phase}: peak_bytes=0 (no sampled value)")
            elif number(f, "peak_bytes") < number(f, "before_bytes"):
                out.append(f"{phase}: peak {number(f, 'peak_bytes'):.0f} is below the start "
                           f"{number(f, 'before_bytes'):.0f} (the sampler lost its window)")
        return out


class Row:
    def __init__(self, ident, phase, what, unit, measured, limit, status="ok", extra="", small=None, floor=None):
        self.ident, self.phase, self.what, self.unit = ident, phase, what, unit
        self.measured, self.limit, self.status, self.extra = measured, limit, status, extra
        self.small, self.floor = small, floor

    def fmt(self, value):
        if value is None:
            return "missing"
        return f"{value:.2f} {self.unit}"


def check(ident, phase, what, unit, measured, limit, floor=None, small=None):
    if measured is None:
        return Row(ident, phase, what, unit, None, limit, "FAIL", "no measurement in the output", small, floor)
    if measured > limit:
        return Row(ident, phase, what, unit, measured, limit, "FAIL", "", small, floor)
    if floor is not None and (measured < floor or (small is not None and small < floor)):
        low = measured if measured < floor else small
        return Row(ident, phase, what, unit, measured, limit, "FAIL",
                   f"{low:.2f} {unit} is below the floor of {floor} {unit}: the phase or the "
                   f"peak sampler did not run", small, floor)
    return Row(ident, phase, what, unit, measured, limit, "ok", "", small, floor)


def per_unit(value, unit, entries):
    if value is None:
        return None
    return value / MIB if unit == "MiB" else value / entries


def slope(big, small, e_big, e_small):
    """Bytes per entry between the two runs."""
    if big is None or small is None:
        return None
    return (big - small) / (e_big - e_small)


def evaluate(large, small):
    """The rows of the three tables: absolute limits, slopes, extrapolation."""
    el, es = large.entries, small.entries
    rows, slopes = [], []

    def absolute(spec, measured_of):
        ident, phase, what, unit, limit, floor, _source = spec
        m_large, m_small = measured_of(large), measured_of(small)
        rows.append(check(ident, phase, what, unit, per_unit(m_large, unit, el), limit, floor,
                          per_unit(m_small, unit, es)))

    for ident, phase, what, unit, limit, floor, source in CHECKS:
        absolute((ident, phase, what, unit, limit, floor, source), lambda r, p=phase: r.growth(p))
    ident, what, unit, limit, floor, source = SCAN_TOTAL
    absolute((ident, "bounded.*", what, unit, limit, floor, source), lambda r: r.scan_total())
    ident, phase, what, unit, limit, floor, source = STEADY
    absolute((ident, "steady.mapped", what, unit, limit, floor, source), lambda r: r.steady("mapped"))
    ident, phase, what, unit, limit, floor, source = STEADY_HEAP
    heap = lambda r: (number(r.data["TOTAL"][phase], "accounted_per_entry") if phase in r.data["TOTAL"] else None)  # noqa: E731
    rows.append(check(ident, phase, what, unit, heap(large), limit, floor, heap(small)))
    for spec in (COMPACT, SAVE_TABLES, RESTART, SORT):
        ident, phase, what, unit, limit, floor, source = spec
        absolute(spec, lambda r, p=phase: r.growth(p))
    ident, phase, what, unit, limit, floor, source = RESTARTED
    absolute((ident, "restarted", what, unit, limit, floor, source), lambda r: r.steady("restarted"))

    # The compacted steady state is a diagnostic only (the service never compacts the heap).
    comp_large, comp_small = large.steady("mapped", "compacted_growth_bytes"), small.steady("mapped", "compacted_growth_bytes")
    rows.append(Row("steady.compacted", "steady.mapped", "the same after HeapCompact (diagnostic, no limit)", "MiB",
                    per_unit(comp_large, "MiB", el), None, "info" if comp_large is not None else "FAIL",
                    "" if comp_large is not None else "no measurement in the output", per_unit(comp_small, "MiB", es)))

    # The slope: growth per entry between the two sizes.
    for ident, name, what, limit, _source in SLOPES:
        s = slope(large.measure(name), small.measure(name), el, es)
        row = Row(ident, name, what, "B/entry", s, limit, small=None)
        if s is None:
            row.status, row.extra = "FAIL", "no measurement in the output"
        elif s > limit:
            row.status = "FAIL"
        slopes.append(row)

    # Extrapolation: a fit, fixed part + slope x entries, from the same two sizes.
    ext = []

    def fit(name):
        v_large, v_small = large.measure(name), small.measure(name)
        if v_large is None or v_small is None:
            return None
        s = max(slope(v_large, v_small, el, es), 0.0)  # a negative slope is noise
        return (v_large + s * (PC["entries"] - el)) / MIB

    def plus(a, b):
        return None if a is None or b is None else a + b

    def add(ident, what, mib, must_meet_target):
        row = Row(ident, "27.2M entries", what, "MiB", mib, PC["target_bytes"] / MIB)
        if mib is None:
            row.status, row.extra = "FAIL", "no measurement in the output"
        elif mib > PC["hard_bytes"] / MIB:
            row.status, row.extra = "FAIL", f"over the hard limit of {PC['hard_bytes'] // MIB} MiB"
        elif mib > PC["target_bytes"] / MIB:
            row.status = "FAIL" if must_meet_target else "over target"
        ext.append(row)

    steady = fit("steady")
    add("pc.steady", "steady state", steady, True)
    add("pc.scan", "steady + a full scan (rescan, with the production spill budget)",
        plus(steady, plus(fit("total"), PRODUCTION_BUDGET_MIB)), True)
    add("pc.compact", "steady + a compaction", plus(steady, fit("compact")), False)
    add("pc.restart", "a restart (peak)", fit("restart"), True)
    return rows, slopes, ext


def render(groups, entries):
    lines = [f"Memory gate: {entries[0]:,} and {entries[1]:,} entries (windows-latest, private bytes)", ""]
    head = f"{'check':<20}{'phase':<22}{'1M':>14}{'5M':>16}{'limit':>16}  status"
    for title, group in groups:
        lines += [title, head, "-" * len(head)]
        for r in group:
            small = r.fmt(r.small) if r.small is not None else ""
            lines.append(f"{r.ident:<20}{r.phase:<22}{small:>14}{r.fmt(r.measured):>16}"
                         f"{(r.fmt(r.limit) if r.limit is not None else ''):>16}  {r.status}"
                         + (f" ({r.extra})" if r.extra else ""))
            lines.append(f"{'':<20}{r.what}")
        lines.append("")
    return "\n".join(lines)


def markdown(groups, entries):
    out = [f"## Memory gate ({entries[0]:,} and {entries[1]:,} entries)", ""]
    for title, group in groups:
        out += [f"### {title}", "", "| check | phase | what | 1M | 5M | limit | status |", "|---|---|---|---:|---:|---:|---|"]
        for r in group:
            status = r.status if r.status in ("ok", "info") else f"**{r.status}**"
            small = r.fmt(r.small) if r.small is not None else ""
            limit = r.fmt(r.limit) if r.limit is not None else ""
            out.append(f"| {r.ident} | {r.phase} | {r.what} | {small} | {r.fmt(r.measured)} | {limit} | {status} |")
        out.append("")
    return "\n".join(out)


def load(path, label, expected_entries):
    """The run in `path`, or an exit code and the reason."""
    with open(path, encoding="utf-8", errors="replace") as f:
        data = parse(f.read())
    if data["ACTIVE"] is None or not data["DONE"]:
        return None, 1, (f"the {label} output has no FERROW-MEMORY-ACTIVE and FERROW-MEMORY-DONE "
                         f"lines: the measurement did not run to its end")
    asked = int(data["ACTIVE"]["entries"])
    if asked != expected_entries:
        return None, 2, f"the limits are calibrated for {expected_entries} entries in the {label} run, it has {asked}"
    if data["BUDGET"] is None or int(data["BUDGET"]) != SPILL_BUDGET:
        return None, 2, (f"the limits are calibrated for a spill budget of {SPILL_BUDGET} bytes, the {label} run has "
                         f"{data['BUDGET']} (set FERROW_MEMORY_SPILL_BUDGET)")
    return Run(label, data), 0, ""


def main(argv):
    ap = argparse.ArgumentParser()
    ap.add_argument("large", help="output of the 5 M entry run")
    ap.add_argument("small", help="output of the 1 M entry run")
    ap.add_argument("--summary")
    args = ap.parse_args(argv)
    try:
        runs = []
        for path, label, n in ((args.large, "5M", ENTRIES_LARGE), (args.small, "1M", ENTRIES_SMALL)):
            run, code, why = load(path, label, n)
            if run is None:
                print(f"{'MEMORY GATE FAILED' if code == 1 else 'MEMORY GATE'}: {why}", file=sys.stderr)
                return code
            runs.append(run)
        large, small = runs
        bad_runs = [f"{r.label}: {why}" for r in runs for why in r.credibility()]
        rows, slopes, ext = evaluate(large, small)
    except BadLine as e:
        print(f"::error title=Memory gate::{e}", file=sys.stderr)
        print(f"MEMORY GATE FAILED: {e}", file=sys.stderr)
        return 1
    entries = (int(large.entries), int(small.entries))
    groups = [("Limits at 5M entries (and the 1M run)", rows),
              ("Growth per entry between the 1M and the 5M run (the slope)", slopes),
              ("Fitted to the owner's PC: 27.2M entries, 1 GB hard, 512 MB target (fixed part + slope x entries)", ext)]
    print(render(groups, entries))
    if args.summary:
        with open(args.summary, "a", encoding="utf-8") as f:
            f.write(markdown(groups, entries) + "\n")
    for r in ext:
        if r.status == "over target":
            print(f"::warning title=Memory gate::{r.phase} {r.what}: {r.fmt(r.measured)} fitted is over the "
                  f"512 MB target (known: compaction keeps its live tables in the heap)")
    bad = [r for r in rows + slopes + ext if r.status == "FAIL"]
    for why in bad_runs:
        print(f"::error title=Memory gate::peak measurement not credible, {why}", file=sys.stderr)
    for r in bad:
        reason = r.extra or f"{r.fmt(r.measured)} is over the limit of {r.fmt(r.limit)}"
        print(f"::error title=Memory gate::{r.ident} ({r.phase}): {r.what}: {reason}", file=sys.stderr)
    if bad or bad_runs:
        print("MEMORY GATE FAILED: " + "; ".join(
            [f"{r.ident} [{r.phase}] {r.fmt(r.measured)} > {r.fmt(r.limit)}" if r.measured is not None
             and not r.extra else f"{r.ident} [{r.phase}] {r.extra or 'no measurement'}" for r in bad]
            + [f"peaks not credible in the {why}" for why in bad_runs]), file=sys.stderr)
        return 1
    print("MEMORY GATE PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
