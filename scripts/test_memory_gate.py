"""Tests for scripts/memory-gate.py on small synthetic outputs (no measurement)."""

import contextlib
import importlib.util
import io
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("memory_gate", os.path.join(HERE, "memory-gate.py"))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)

MIB = 1 << 20
N = 5_000_000


def peak(phase, before, growth_mib):
    growth = int(growth_mib * MIB)
    return (f"FERROW-MEMORY-PEAK phase={phase} before_bytes={before} peak_bytes={before + growth} "
            f"growth_bytes={growth} growth_per_entry={growth / N:.3f} note=a note with words")


def output(entries=N, overrides=None, drop=None, done=True):
    """What a passing run prints, with some values replaced or lines removed."""
    o = {
        "bounded.parse": 15, "bounded.from_parsed": 0.1, "bounded.emit": 59, "bounded.freeze": 4.5,
        "bounded.live_new": 4.5, "bounded.children": 0.1, "compact.file": 100, "restart.load": 0.2,
        "steady": 20, "restarted": 20, "heap_per_entry": 0.36,
    }
    o.update(overrides or {})
    lines = [f"FERROW-MEMORY-ACTIVE example=memory_breakdown entries={entries} changes=20000 seed=2026"]
    start = 418_795_520
    for phase in ("bounded.parse", "bounded.from_parsed", "bounded.emit", "bounded.freeze",
                  "bounded.live_new", "bounded.children"):
        # The emit's and the freeze's shards are alive together: peaks above the start add up.
        before = start + (int(o["bounded.emit"] * MIB) if phase == "bounded.freeze" else 0)
        lines.append(peak(phase, before, o[phase]))
    lines.append(peak("compact.file", start, o["compact.file"]))
    lines.append(peak("restart.load", start, o["restart.load"]))
    for phase, key in (("mapped", "steady"), ("restarted", "restarted")):
        lines.append(f"FERROW-MEMORY-STEADY phase={phase} entries={N} private_bytes=1 baseline_bytes=1 "
                     f"growth_bytes={int(o[key] * MIB)} growth_per_entry=1.0 mapped_bytes=1 note=x")
    lines.append(f"FERROW-MEMORY-TOTAL phase=bounded.live entries={N} accounted_bytes=1 "
                 f"accounted_per_entry={o['heap_per_entry']} process_bytes=1 process_per_entry=1 "
                 f"unattributed_bytes=1 unattributed_per_entry=1")
    lines = [line for line in lines if not any(d in line for d in (drop or []))]
    if done:
        lines.append("FERROW-MEMORY-DONE")
    return "\r\n".join(lines) + "\r\n"


def run(text, summary=None):
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "out.txt")
        with open(path, "w", encoding="utf-8") as f:
            f.write(text)
        args = [path] + (["--summary", os.path.join(d, "summary.md")] if summary else [])
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = gate.main(args)
        written = ""
        if summary:
            with open(os.path.join(d, "summary.md"), encoding="utf-8") as f:
                written = f.read()
        return code, out.getvalue(), err.getvalue(), written


class MemoryGate(unittest.TestCase):
    def test_passing_run(self):
        code, out, err, _ = run(output())
        self.assertEqual(code, 0, err)
        self.assertIn("MEMORY GATE PASSED", out)
        self.assertIn("scan.emit", out)  # the table names every phase

    def test_each_limit_fails_and_names_phase_and_number(self):
        cases = {
            "bounded.parse": ("scan.parse", 30),
            "bounded.emit": ("scan.emit", 90),
            "bounded.freeze": ("scan.freeze", 12),
            "compact.file": ("compact.file", 5_000_000 * 30 / MIB),
            "restart.load": ("restart.load", 40),
            "steady": ("steady.private", 200),
            "restarted": ("restart.private", 200),
            "heap_per_entry": ("steady.heap", 4.0),
        }
        for key, (ident, value) in cases.items():
            with self.subTest(key):
                code, _, err, _ = run(output(overrides={key: value}))
                self.assertEqual(code, 1)
                self.assertIn("MEMORY GATE FAILED", err)
                self.assertIn(ident, err)

    def test_total_scan_peak_fails_when_every_phase_is_inside_its_own_limit(self):
        # emit and freeze each inside their limits, but the shards of the emit
        # are still alive in the freeze: the sum is over the total's limit.
        code, _, err, _ = run(output(overrides={"bounded.emit": 75, "bounded.freeze": 7}))
        self.assertEqual(code, 1)
        self.assertIn("scan.total", err)
        self.assertNotIn("scan.emit", err.split("MEMORY GATE FAILED")[1])

    def test_extrapolation_over_hard_limit_fails_over_target_warns_for_compaction(self):
        # A steady state that is inside its own limit but, scaled to 27.2 M
        # entries and with a rescan on top, is over the 512 MB target.
        code, _, err, _ = run(output(overrides={"steady": 40}))
        self.assertEqual(code, 1)
        self.assertIn("pc.scan", err)
        # A compaction over the target only warns (its tables are in the heap, decision 6).
        code, out, err, _ = run(output(overrides={"compact.file": 120}))
        self.assertEqual(code, 0, err)
        self.assertIn("over target", out)
        self.assertIn("::warning", out)

    def test_missing_measurement_fails(self):
        code, _, err, _ = run(output(drop=["restart.load"]))
        self.assertEqual(code, 1)
        self.assertIn("restart.load", err)
        self.assertIn("no measurement", err)

    def test_unfinished_run_fails(self):
        code, _, err, _ = run(output(done=False))
        self.assertEqual(code, 1)
        self.assertIn("did not run to its end", err)

    def test_other_entry_count_is_not_judged(self):
        code, _, err, _ = run(output(entries=20_000))
        self.assertEqual(code, 2)
        self.assertIn("calibrated", err)

    def test_summary_markdown(self):
        code, _, _, md = run(output(), summary=True)
        self.assertEqual(code, 0)
        self.assertIn("| scan.emit |", md)
        self.assertIn("| pc.scan |", md)


if __name__ == "__main__":
    unittest.main()
