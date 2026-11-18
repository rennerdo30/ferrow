"""Tests for scripts/memory-gate.py.

The reference is the real output of the hosted `memory breakdown (windows 5M)`
job, a 5 M and a 1 M entry run, trimmed to the lines the gate reads
(`scripts/testdata/memory-gate-5m.txt` and `memory-gate-1m.txt`, see the README
line at their top). The tests replay copies of it with a regression injected
(`mutate`) through the gate, and must see it fail.
"""

import contextlib
import importlib.util
import io
import os
import re
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("memory_gate", os.path.join(HERE, "memory-gate.py"))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)

MIB = 1 << 20


def fixture(name):
    with open(os.path.join(HERE, "testdata", name), encoding="utf-8") as f:
        return f.read()


LARGE = fixture("memory-gate-5m.txt")
SMALL = fixture("memory-gate-1m.txt")


def entries_of(text):
    """The index's entry count, from the steady line (what the gate divides by)."""
    return int(re.search(r"FERROW-MEMORY-STEADY phase=mapped entries=(\d+)", text).group(1))


def edit_line(text, prefix, edit):
    """`text` with the one line that starts with `prefix` passed through `edit`."""
    lines, hit = text.split("\n"), 0
    for i, line in enumerate(lines):
        if line.startswith(prefix):
            lines[i], hit = edit(line), hit + 1
    assert hit == 1, (prefix, hit)
    return "\n".join(lines)


def set_field(line, key, value):
    new, n = re.subn(rf"(\s{key}=)\S+", lambda m: f"{m.group(1)}{value}", line, count=1)
    assert n == 1, (key, line)
    return new


def bump(line, key, extra):
    return set_field(line, key, int(re.search(rf"\s{key}=(\d+)", line).group(1)) + extra)


def mutate(text, phase, bytes_per_entry, alive_in=()):
    """`text` as it would read if `phase` held `bytes_per_entry` more per entry.

    A phase's memory is still alive in the phases after it when the next
    phase starts from it (the emit's shards are alive in the freeze): those
    start `extra` higher and peak `extra` higher, so their own growth stays
    the same and the scan's total rises, as a real change would show it.
    """
    extra = int(bytes_per_entry * entries_of(text))

    def grow(line):
        return bump(bump(line, "growth_bytes", extra), "peak_bytes", extra)

    text = edit_line(text, f"FERROW-MEMORY-PEAK phase={phase} ", grow)
    for later in alive_in:
        text = edit_line(text, f"FERROW-MEMORY-PEAK phase={later} ",
                         lambda line: bump(bump(line, "before_bytes", extra), "peak_bytes", extra))
    return text


def run(large, small, summary=False):
    with tempfile.TemporaryDirectory() as d:
        paths = []
        for name, text in (("large.txt", large), ("small.txt", small)):
            paths.append(os.path.join(d, name))
            with open(paths[-1], "w", encoding="utf-8", newline="") as f:
                f.write(text)
        args = paths + (["--summary", os.path.join(d, "summary.md")] if summary else [])
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = gate.main(args)
        written = ""
        if summary:
            with open(os.path.join(d, "summary.md"), encoding="utf-8") as f:
                written = f.read()
        return code, out.getvalue(), err.getvalue(), written


def rows(large=LARGE, small=SMALL):
    """The gate's rows by id: absolute checks, slopes and the fit to the PC."""
    first, second, third = gate.evaluate(gate.Run("5M", gate.parse(large)), gate.Run("1M", gate.parse(small)))
    return {r.ident: r for r in first + second + third}


def regress(phase, bytes_per_entry, alive_in=(), both=True):
    """Both outputs (or only the 5 M one) with a per-entry regression in `phase`."""
    return (mutate(LARGE, phase, bytes_per_entry, alive_in),
            mutate(SMALL, phase, bytes_per_entry, alive_in) if both else SMALL)


class RealOutput(unittest.TestCase):
    def test_real_output_passes(self):
        code, out, err, _ = run(LARGE, SMALL)
        self.assertEqual(code, 0, err)
        self.assertIn("MEMORY GATE PASSED", out)
        for ident in ("scan.emit", "slope.emit", "slope.compact", "pc.compact", "restart.sort"):
            self.assertIn(ident, out)  # the tables name every check

    def test_every_check_has_a_measurement_in_the_real_output(self):
        by = rows()
        for ident, r in by.items():
            self.assertIsNotNone(r.measured, ident)
            self.assertIn(r.status, ("ok", "info", "over target"), ident)


class SlopeCatchesPerEntryGrowth(unittest.TestCase):
    """The reviewer's finding: +4 B/entry in the emit or the compaction passed.

    The old limits (emit 80 MiB, compaction 26 B/entry) sat 3 to 5 B/entry above
    the measured values, because of a fixed 51 MiB part and the runner's noise.
    Each of these replays the real output with +2 B/entry in a phase and needs
    the gate to fail on the slope, which sees the growth per entry between the
    two sizes whatever the fixed part is.
    """

    def assert_caught(self, large, small, ident):
        by = rows(large, small)
        self.assertEqual(by[ident].status, "FAIL", f"{ident}: {by[ident].measured}")
        code, _, err, _ = run(large, small)
        self.assertEqual(code, 1)
        self.assertIn(ident, err)
        self.assertIn("MEMORY GATE FAILED", err)
        return by

    def test_two_bytes_per_entry_in_the_emit(self):
        large, small = regress("bounded.emit", 2.0, alive_in=("bounded.freeze",))
        by = self.assert_caught(large, small, "slope.emit")
        self.assertEqual(by["slope.total"].status, "FAIL")  # the scan's total follows

    def test_two_bytes_per_entry_in_the_compaction_is_below_every_absolute_limit(self):
        # The compaction's absolute limit alone lets +2 B/entry through (measured
        # 18.5 to 21.2, limit 26); only the slope sees it.
        large, small = regress("compact.file", 2.0)
        by = self.assert_caught(large, small, "slope.compact")
        absolute = [r for i, r in by.items() if not i.startswith(("slope.", "pc."))]
        self.assertTrue(all(r.status in ("ok", "info") for r in absolute),
                        [(r.ident, r.status) for r in absolute if r.status not in ("ok", "info")])

    def test_two_bytes_per_entry_in_the_parse(self):
        large, small = regress("bounded.parse", 2.0)
        self.assert_caught(large, small, "slope.parse")

    def test_without_the_slope_checks_the_compaction_regression_passes(self):
        # The test can fail: take the slope checks away and the same replay is green.
        large, small = regress("compact.file", 2.0)
        saved = gate.SLOPES
        try:
            gate.SLOPES = []
            code, out, err, _ = run(large, small)
        finally:
            gate.SLOPES = saved
        self.assertEqual(code, 0, err)
        self.assertIn("MEMORY GATE PASSED", out)

    def test_one_and_a_half_bytes_per_entry_in_the_emit(self):
        large, small = regress("bounded.emit", 1.5, alive_in=("bounded.freeze",))
        self.assert_caught(large, small, "slope.emit")

    def test_the_reviewers_replay_only_the_large_output_is_mutated(self):
        # +2 B/entry at 5 M entries, the small run untouched: the slope grows by
        # more than 2, so it fails too.
        for phase, alive, ident in (("bounded.emit", ("bounded.freeze",), "slope.emit"),
                                    ("compact.file", (), "slope.compact")):
            large, small = regress(phase, 2.0, alive_in=alive, both=False)
            self.assertEqual(rows(large, small)[ident].status, "FAIL", ident)

    def test_the_reviewers_replay_only_the_large_output_is_mutated(self):
        # +2 B/entry at 5 M entries, the small run untouched: the slope grows by
        # more than 2, so it fails too.
        large, small = regress("bounded.emit", 2.0, alive_in=("bounded.freeze",), both=False)
        self.assertEqual(rows(large, small)["slope.emit"].status, "FAIL")
        large, small = regress("compact.file", 2.0, both=False)
        self.assertEqual(rows(large, small)["slope.compact"].status, "FAIL")

    def test_every_slope_limit_is_crossed_by_two_bytes_per_entry_on_the_real_output(self):
        by = rows()
        for ident, _measure, _what, limit, _source in gate.SLOPES:
            if ident != "slope.sort":  # the sort's own limit is 0.5 B/entry, tighter than +2
                self.assertGreater(by[ident].measured + 2.0, limit, ident)
            self.assertLess(by[ident].measured, limit, ident)


class AbsoluteLimits(unittest.TestCase):
    def test_each_absolute_limit_fails_and_names_phase_and_number(self):
        for ident, phase in (("scan.parse", "bounded.parse"), ("scan.emit", "bounded.emit"),
                             ("scan.freeze", "bounded.freeze"), ("restart.load", "restart.load"),
                             ("restart.save_tables", "restart.save_tables"),
                             ("restart.sort", "restart.sort_orders"), ("compact.file", "compact.file")):
            with self.subTest(ident):
                spec = next(c for c in gate.CHECKS + [gate.COMPACT, gate.SAVE_TABLES, gate.RESTART, gate.SORT]
                            if c[0] == ident)
                unit, limit = spec[3], spec[4]
                over = limit * 1.2 * (MIB if unit == "MiB" else entries_of(LARGE))
                current = float(re.search(r"phase=%s .*?growth_bytes=(\d+)" % re.escape(phase), LARGE).group(1))
                extra = int(over - current)
                text = edit_line(LARGE, f"FERROW-MEMORY-PEAK phase={phase} ",
                                 lambda line: bump(bump(line, "growth_bytes", extra), "peak_bytes", extra))
                code, _, err, _ = run(text, SMALL)
                self.assertEqual(code, 1)
                self.assertIn(ident, err)

    def test_steady_state_is_the_uncompacted_number(self):
        # The compacted figure is a diagnostic: it cannot rescue or fail the check.
        big = edit_line(LARGE, "FERROW-MEMORY-STEADY phase=mapped ",
                        lambda line: set_field(line, "compacted_growth_bytes", 1))
        self.assertEqual(rows(big, SMALL)["steady.private"].status, "ok")
        self.assertEqual(rows(big, SMALL)["steady.compacted"].status, "info")
        over = int(gate.STEADY[4] * 1.2 * MIB)
        text = edit_line(LARGE, "FERROW-MEMORY-STEADY phase=mapped ",
                         lambda line: set_field(set_field(line, "growth_bytes", over), "compacted_growth_bytes", 1))
        code, _, err, _ = run(text, SMALL)
        self.assertEqual(code, 1)
        self.assertIn("steady.private", err)

    def test_live_heap_per_entry(self):
        text = edit_line(LARGE, "FERROW-MEMORY-TOTAL phase=bounded.live ",
                         lambda line: set_field(line, "accounted_per_entry", gate.STEADY_HEAP[4] * 1.5))
        code, _, err, _ = run(text, SMALL)
        self.assertEqual(code, 1)
        self.assertIn("steady.heap", err)

    def test_total_scan_peak_fails_when_every_phase_is_inside_its_own_limit(self):
        # emit and freeze each just inside their limits, shards of the emit alive in
        # the freeze: the sum is over the total's limit.
        emit, freeze = 0.99 * gate.CHECKS[2][4], 0.99 * gate.CHECKS[3][4]
        self.assertGreater(emit + freeze, gate.SCAN_TOTAL[3], "limits must allow the total to trip")
        text = LARGE
        for phase, mib in (("bounded.emit", emit), ("bounded.freeze", freeze)):
            current = float(re.search(r"phase=%s .*?growth_bytes=(\d+)" % re.escape(phase), text).group(1))
            extra = int(mib * MIB - current)
            before_shift = int(emit * MIB - current) if phase == "bounded.freeze" else 0
            text = edit_line(text, f"FERROW-MEMORY-PEAK phase={phase} ",
                             lambda line: bump(bump(bump(line, "growth_bytes", extra), "peak_bytes", extra + before_shift),
                                                "before_bytes", before_shift))
        code, _, err, _ = run(text, SMALL)
        self.assertEqual(code, 1)
        self.assertIn("scan.total", err)


class DeadSampler(unittest.TestCase):
    """A sampler that produced no value reads as no growth (the example once did `unwrap_or(0)`)."""

    def assert_fails(self, large, small, *needles):
        code, _, err, _ = run(large, small)
        self.assertEqual(code, 1)
        for needle in needles:
            self.assertIn(needle, err)

    def test_peak_zero(self):
        text = edit_line(LARGE, "FERROW-MEMORY-PEAK phase=bounded.emit ",
                         lambda line: set_field(set_field(line, "peak_bytes", 0), "growth_bytes", 0))
        self.assert_fails(text, SMALL, "bounded.emit", "peak_bytes=0")

    def test_peak_below_its_start(self):
        text = edit_line(LARGE, "FERROW-MEMORY-PEAK phase=compact.file ",
                         lambda line: set_field(set_field(line, "peak_bytes", 1000), "growth_bytes", 0))
        self.assert_fails(text, SMALL, "compact.file", "below the start")

    def test_the_example_says_the_platform_gave_no_peak(self):
        text = edit_line(SMALL, "FERROW-MEMORY-PEAK phase=bounded.parse ",
                         lambda line: set_field(set_field(set_field(line, "sampler", "missing"), "peak_bytes", 0),
                                                "growth_bytes", 0))
        self.assert_fails(LARGE, text, "bounded.parse", "no peak")

    def test_sampler_self_check_failed(self):
        text = edit_line(LARGE, "FERROW-MEMORY-SAMPLER ", lambda line: set_field(line, "ok", "false"))
        self.assert_fails(text, SMALL, "sampler")

    def test_sampler_self_check_missing(self):
        text = "\n".join(line for line in SMALL.split("\n") if not line.startswith("FERROW-MEMORY-SAMPLER"))
        self.assert_fails(LARGE, text, "FERROW-MEMORY-SAMPLER")

    def test_no_growth_is_below_the_floor(self):
        # A dead sampler that still reports its start value: growth near zero for a phase that must grow.
        for phase, ident in (("bounded.emit", "scan.emit"), ("bounded.parse", "scan.parse"),
                             ("compact.file", "compact.file")):
            with self.subTest(phase):
                text = edit_line(LARGE, f"FERROW-MEMORY-PEAK phase={phase} ",
                                 lambda line: set_field(set_field(line, "growth_bytes", 1000), "peak_bytes",
                                                        int(re.search(r"before_bytes=(\d+)", line).group(1)) + 1000))
                code, _, err, _ = run(text, SMALL)
                self.assertEqual(code, 1)
                self.assertIn(ident, err)
                self.assertIn("floor", err)

    def test_floors_are_below_what_the_real_runs_measure(self):
        by = rows()
        for ident, r in by.items():
            if r.floor is not None:
                self.assertGreater(r.measured, r.floor, ident)
                self.assertGreater(r.small, r.floor, ident)


class MissingOrBrokenInput(unittest.TestCase):
    def test_missing_measurement_fails(self):
        text = "\n".join(line for line in LARGE.split("\n") if "phase=restart.load " not in line)
        code, _, err, _ = run(text, SMALL)
        self.assertEqual(code, 1)
        self.assertIn("restart.load", err)
        self.assertIn("no measurement", err)

    def test_unfinished_run_fails(self):
        for large, small in ((LARGE.replace("FERROW-MEMORY-DONE", ""), SMALL),
                             (LARGE, SMALL.replace("FERROW-MEMORY-DONE", ""))):
            code, _, err, _ = run(large, small)
            self.assertEqual(code, 1)
            self.assertIn("did not run to its end", err)

    def test_other_entry_count_or_budget_is_not_judged(self):
        text = edit_line(LARGE, "FERROW-MEMORY-ACTIVE ", lambda line: set_field(line, "entries", 20_000))
        code, _, err, _ = run(text, SMALL)
        self.assertEqual(code, 2)
        self.assertIn("calibrated", err)
        text = edit_line(SMALL, "FERROW-MEMORY-CONFIG round=bounded spill_budget_bytes",
                         lambda line: set_field(line, "spill_budget_bytes", 33554432))
        code, _, err, _ = run(LARGE, text)
        self.assertEqual(code, 2)
        self.assertIn("spill budget", err)

    def test_a_line_without_a_field_names_the_line(self):
        text = edit_line(LARGE, "FERROW-MEMORY-PEAK phase=bounded.emit ",
                         lambda line: line.replace(" growth_bytes=", " growth_bits="))
        code, _, err, _ = run(text, SMALL)
        self.assertEqual(code, 1)
        self.assertIn("::error", err)
        self.assertIn("no number growth_bytes=", err)
        self.assertIn("phase=bounded.emit", err)  # the offending line itself
        self.assertRegex(err, r"line \d+:")

    def test_a_line_without_a_phase_names_the_line(self):
        text = edit_line(LARGE, "FERROW-MEMORY-STEADY phase=restarted ",
                         lambda line: line.replace(" phase=restarted", ""))
        code, _, err, _ = run(text, SMALL)
        self.assertEqual(code, 1)
        self.assertIn("::error", err)
        self.assertIn("without phase=", err)

    def test_a_number_that_is_not_a_number_names_the_line(self):
        text = edit_line(SMALL, "FERROW-MEMORY-PEAK phase=compact.file ", lambda line: set_field(line, "growth_bytes", "n/a"))
        code, _, err, _ = run(LARGE, text)
        self.assertEqual(code, 1)
        self.assertIn("no number growth_bytes=", err)

    def test_fields_stop_at_the_note(self):
        f = gate.fields("FERROW-MEMORY-PEAK phase=x growth_bytes=5 note=a note, growth_bytes=999 phase=y")
        self.assertEqual(f, {"phase": "x", "growth_bytes": "5"})

    def test_a_note_cannot_override_a_figure(self):
        text = edit_line(LARGE, "FERROW-MEMORY-PEAK phase=bounded.emit ",
                         lambda line: line + " and growth_bytes=0 phase=other")
        self.assertEqual(rows(text, SMALL)["scan.emit"].measured, rows()["scan.emit"].measured)


class Extrapolation(unittest.TestCase):
    def test_fit_is_fixed_part_plus_slope_times_entries_and_uses_the_real_entry_count(self):
        large, small = gate.Run("5M", gate.parse(LARGE)), gate.Run("1M", gate.parse(SMALL))
        el, es = large.entries, small.entries
        self.assertNotEqual(el, gate.ENTRIES_LARGE)  # the real count differs from the 5 M asked for
        v5, v1 = large.measure("compact"), small.measure("compact")
        s = (v5 - v1) / (el - es)
        fitted_compact = (v5 + s * (gate.PC["entries"] - el)) / MIB
        steady5, steady1 = large.measure("steady"), small.measure("steady")
        steady_fit = (steady5 + max((steady5 - steady1) / (el - es), 0) * (gate.PC["entries"] - el)) / MIB
        by = rows()
        self.assertAlmostEqual(by["pc.compact"].measured, steady_fit + fitted_compact, places=6)
        # Not the old scaling of everything by 27.2 M / 5 M, fixed part included.
        scaled = (v5 / MIB + steady5 / MIB) * gate.PC["entries"] / el
        self.assertLess(by["pc.compact"].measured, scaled)

    def test_a_per_entry_regression_moves_the_fit_by_its_size(self):
        base = rows()["pc.compact"].measured
        large, small = regress("compact.file", 2.0)
        moved = rows(large, small)["pc.compact"].measured - base
        # +2 B/entry over 27.2 M entries is 52 MiB.
        self.assertAlmostEqual(moved, 2.0 * gate.PC["entries"] / MIB, delta=1.0)

    def test_warns_only_on_the_fitted_figure_and_fails_over_the_hard_limit(self):
        saved = dict(gate.PC)
        try:
            by = rows()
            self.assertNotIn("::warning", run(LARGE, SMALL)[1])  # the real fit is under the target
            gate.PC["target_bytes"] = int(by["pc.compact"].measured * 0.9 * MIB)
            gate.PC["hard_bytes"] = int(by["pc.compact"].measured * 3 * MIB)
            by = rows()
            self.assertEqual(by["pc.compact"].status, "over target")
            code, out, err, _ = run(LARGE, SMALL)
            self.assertIn("::warning", out)
            self.assertEqual(code, 1 if by["pc.steady"].status == "FAIL" or by["pc.scan"].status == "FAIL"
                             or by["pc.restart"].status == "FAIL" else 0)
            gate.PC["hard_bytes"] = gate.PC["target_bytes"]
            self.assertEqual(rows()["pc.compact"].status, "FAIL")
        finally:
            gate.PC.clear()
            gate.PC.update(saved)


class Report(unittest.TestCase):
    def test_summary_markdown(self):
        code, _, _, md = run(LARGE, SMALL, summary=True)
        self.assertEqual(code, 0)
        self.assertIn("| scan.emit |", md)
        self.assertIn("| slope.compact |", md)
        self.assertIn("| pc.scan |", md)


if __name__ == "__main__":
    unittest.main()
