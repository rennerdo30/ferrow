import copy
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("gate", Path(__file__).with_name("bench-gate.py"))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


def fixture():
    lines = ["FERROW-MEASURE entries=16 seed=2026 heap_bytes=800 bytes_per_entry=50",
             "FERROW-MEASURE load_ms_median=100"]
    for query in sorted(gate.QUERIES):
        for prefix in ("FERROW-QUERY", "FERROW-SERVICE"):
            lines.append(f'{prefix} query="{query}" first_page_ms=0.2 page_hits=2 full_count_ms=10 total_hits=2')
    return "\n".join(lines)


def without_service(text):
    return "\n".join(line for line in text.splitlines() if not line.startswith("FERROW-SERVICE"))


def metadata_fixture():
    lines = ["FERROW-METADATA entries=16 seed=2026 core_heap_bytes=800 default_heap_bytes=1316 default_bytes_per_entry=82.25 metadata_bytes_per_entry=32.25 load_ms_median=200 precomputed_build_ms=20 lazy_build_ms_median=20 lazy_saved_bytes=256 freeze_ms=50",
             "FERROW-SCAN entries=2101 errors=0 none_ms_median=10 default_ms_median=20"]
    for query in sorted(gate.QUERIES):
        for prefix in ("FERROW-MODIFIED-QUERY", "FERROW-MODIFIED-SERVICE"):
            lines.append(f'{prefix} query="{query}" first_page_ms=1 page_hits=2 full_count_ms=10 total_hits=2')
    return "\n".join(lines)


class GateTests(unittest.TestCase):
    def setUp(self):
        self.base = gate.parse(fixture())
        self.head = copy.deepcopy(self.base)

    def rows(self):
        return gate.compare(self.base, self.head, 16)

    def test_relative_boundary_and_regression(self):
        self.head[0]["load_ms_median"] = 110
        self.assertFalse(any(row[-1] for row in self.rows()))
        self.head[0]["load_ms_median"] = 110.001
        self.assertTrue(any(row[-1] for row in self.rows()))

    def test_submillisecond_absolute_tolerance(self):
        self.head[1]["a"]["first_page_ms"] = 0.29
        self.assertFalse(any(row[-1] for row in self.rows()))
        self.head[1]["a"]["first_page_ms"] = 0.301
        self.assertTrue(any(row[-1] for row in self.rows()))

    def test_every_query_and_operation_is_gated(self):
        for query in gate.QUERIES:
            for metric in ("first_page_ms", "full_count_ms"):
                with self.subTest(query=query, metric=metric):
                    head = copy.deepcopy(self.base)
                    head[1][query][metric] += 2
                    self.assertTrue(any(row[-1] for row in gate.compare(self.base, head, 16)))

    def test_memory_uses_actual_heap_and_relative_tolerance(self):
        self.head[0]["heap_bytes"] = 880
        self.assertFalse(any(row[-1] for row in self.rows()))
        self.head[0]["heap_bytes"] = 881
        self.assertTrue(any(row[-1] for row in self.rows()))

    def test_refuse_mismatched_or_incomplete_evidence(self):
        for key, value in (("entries", 17), ("seed", 2027)):
            head = copy.deepcopy(self.base)
            head[0][key] = value
            with self.assertRaises(ValueError):
                gate.compare(self.base, head, 16)
        self.head[1]["a"]["total_hits"] = 3
        with self.assertRaises(ValueError):
            self.rows()
        for text in ("", fixture().replace('FERROW-QUERY query="a"', 'IGNORE query="a"'),
                     fixture() + '\nFERROW-MEASURE seed=2026', fixture().replace("load_ms_median=100", "load_ms_median=NaN"),
                     fixture().replace("load_ms_median=100", "load_ms_median=-1"),
                     fixture().replace("load_ms_median=100", "load_ms_median=inf")):
            with self.subTest(text=text), self.assertRaises((ValueError, KeyError)):
                gate.parse(text)

    def test_service_uses_the_same_gate(self):
        self.head[2]["a"]["full_count_ms"] = 12
        self.assertTrue(any(row[-1] for row in self.rows()))
        self.assertEqual(len(self.rows()), gate.EXPECTED_METRICS)

    def test_metadata_observation_inventory_keeps_paired_gate(self):
        rows, report = gate.metadata_observations(metadata_fixture(), self.head, 16)
        self.assertEqual(len(rows), 26)
        self.assertEqual(len(self.rows()) + len(rows), gate.EXPECTED_TOTAL_METRICS)
        self.assertIn("head-only", report)
        self.head[1]["a"]["full_count_ms"] = 20
        self.assertTrue(any(row[-1] for row in self.rows()))

    def test_metadata_observations_refuse_missing_malformed_and_changed_counts(self):
        text = metadata_fixture()
        for invalid in ("", text.replace('FERROW-MODIFIED-SERVICE query="a"', 'IGNORE query="a"'),
                        text.replace("entries=16", "entries=17"), text.replace("seed=2026", "seed=2027"),
                        text.replace("default_heap_bytes=1316", "default_heap_bytes=1300"),
                        text.replace("load_ms_median=200", "load_ms_median=nan"),
                        text.replace("page_hits=2", "page_hits=3"), text.replace("total_hits=2", "total_hits=3"),
                        text.replace("errors=0", "errors=1"), text + "\n" + text):
            with self.subTest(invalid=invalid), self.assertRaises((ValueError, KeyError)):
                gate.metadata_observations(invalid, self.head, 16)

    def test_cli_requires_metadata_when_ci_requests_it(self):
        with tempfile.TemporaryDirectory() as folder:
            base, head = [Path(folder) / name for name in ("base.txt", "head.txt")]
            base.write_text(fixture(), encoding="utf-8")
            for observations, expected in (("", 2), (metadata_fixture(), 0)):
                head.write_text(fixture()+"\n"+observations, encoding="utf-8")
                result = subprocess.run([sys.executable, str(Path(__file__).with_name("bench-gate.py")), str(base), str(head),
                                         "--entries", "16", "--require-metadata"], capture_output=True, timeout=5)
                self.assertEqual(result.returncode, expected, result.stderr)

    def test_retry_only_failed_latency_once_paired(self):
        self.head[2]["report 2024"]["first_page_ms"] = 4
        calls = []
        def measure(*args):
            calls.append(args)
            return (1 if args[0] == "base" else 1.05), 2
        rows = gate.retry_failures(self.rows(), self.base, self.head, measure)
        self.assertFalse(any(row[-1] for row in rows))
        self.assertEqual(calls, [(arm, "IPC", "report 2024", "first_page_ms") for arm in ("base", "head")])
        calls.clear()
        self.head[0]["heap_bytes"] = 1000
        self.assertTrue(any(row[-1] for row in gate.retry_failures(self.rows(), self.base, self.head, measure)))
        self.assertEqual(calls, [])

    def test_retry_repeat_breach_fails_and_bad_evidence_refuses(self):
        self.head[0]["load_ms_median"] = 200
        rows = gate.retry_failures(self.rows(), self.base, self.head,
                                   lambda arm, *_: (100 if arm == "base" else 111, 16))
        self.assertTrue(any(row[-1] for row in rows))
        for bad in ((float("nan"), 16), (100, 17), (-1, 16)):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                gate.retry_failures(self.rows(), self.base, self.head, lambda *_: bad)

    def test_retry_pass_does_not_run_producers(self):
        def unexpected(*_):
            self.fail("passing metrics must not be remeasured")
        self.assertEqual(gate.retry_failures(self.rows(), self.base, self.head, unexpected), self.rows())

    def test_service_set_is_required_in_every_arm(self):
        # Missing in one arm, in both arms, or only partly present: refused.
        with self.assertRaises(ValueError):
            gate.parse(without_service(fixture()))
        partial = fixture().replace('FERROW-SERVICE query="a"', 'IGNORE query="a"')
        with self.assertRaises(ValueError):
            gate.parse(partial)

    def test_cli_refuses_when_both_service_arms_are_missing(self):
        with tempfile.TemporaryDirectory() as folder:
            base, head = [Path(folder) / name for name in ("base.txt", "head.txt")]
            base.write_text(without_service(fixture()), encoding="utf-8")
            head.write_text(without_service(fixture()), encoding="utf-8")
            result = subprocess.run([sys.executable, str(Path(__file__).with_name("bench-gate.py")),
                                     str(base), str(head), "--entries", "16"], capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 2, result.stderr)

    def test_compare_refuses_an_incomplete_metric_inventory(self):
        del self.head[2]["a"]
        del self.base[2]["a"]
        with self.assertRaises(ValueError):
            self.rows()

    def test_cli_exit_codes_and_summary_on_trivial_inputs(self):
        with tempfile.TemporaryDirectory() as folder:
            base, head = [Path(folder) / name for name in ("base.txt", "head.txt")]
            base.write_text(fixture(), encoding="utf-8")
            for text, expected in ((fixture(), 0), (fixture().replace("load_ms_median=100", "load_ms_median=200"), 1), ("", 2)):
                head.write_text(text, encoding="utf-8")
                result = subprocess.run([sys.executable, str(Path(__file__).with_name("bench-gate.py")),
                                         str(base), str(head), "--entries", "16"], capture_output=True, timeout=5,
                                        env={**os.environ, "GITHUB_STEP_SUMMARY": str(Path(folder) / "summary.md")})
                self.assertEqual(result.returncode, expected, result.stderr)
            self.assertIn("| Metric | Base | Head |", (Path(folder) / "summary.md").read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
