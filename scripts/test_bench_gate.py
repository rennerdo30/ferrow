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
        lines.append(f'FERROW-QUERY query="{query}" first_page_ms=0.2 page_hits=2 full_count_ms=10 total_hits=2')
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

    def test_service_uses_the_same_gate_and_is_required_in_both_arms(self):
        service_text = fixture() + "\n" + "\n".join(line.replace("FERROW-QUERY", "FERROW-SERVICE")
                                                       for line in fixture().splitlines() if line.startswith("FERROW-QUERY"))
        base = gate.parse(service_text)
        head = copy.deepcopy(base)
        head[2]["a"]["full_count_ms"] = 12
        self.assertTrue(any(row[-1] for row in gate.compare(base, head, 16)))
        with self.assertRaises(ValueError):
            gate.compare(self.base, head, 16)

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
