import copy
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from types import SimpleNamespace

spec = importlib.util.spec_from_file_location("gate", Path(__file__).with_name("bench-gate.py"))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


REPEATS = "FERROW-PROBE-REPEATS n=31 min=0.100000000 median=0.200000000 max=0.300000000\n"


def fixture(language=True):
    lines = ["FERROW-MEASURE entries=16 seed=2026 heap_bytes=800 bytes_per_entry=50",
             "FERROW-MEASURE load_ms_median=100"]
    for query in sorted(gate.QUERIES | (gate.NEW_QUERIES if language else set())):
        for prefix in ("FERROW-QUERY", "FERROW-SERVICE"):
            lines.append(f'{prefix} query="{query}" first_page_ms=0.2 page_hits=2 full_count_ms=10 total_hits=2')
    return "\n".join(lines)


def with_cold(text, special=None, cold_first=0.2, cold_full=10):
    """Give every query row its cold samples; `special` maps a row prefix to (first, full)."""
    out = []
    for line in text.splitlines():
        if line.startswith(("FERROW-QUERY ", "FERROW-SERVICE ")):
            first, full = cold_first, cold_full
            for prefix, values in (special or {}).items():
                if line.startswith(prefix):
                    first, full = values
            line += f" first_page_cold_ms={first} full_count_cold_ms={full}"
        out.append(line)
    return "\n".join(out)


def without_service(text):
    return "\n".join(line for line in text.splitlines() if not line.startswith("FERROW-SERVICE"))


def metadata_fixture():
    lines = ["FERROW-METADATA entries=16 seed=2026 core_heap_bytes=800 default_heap_bytes=1044 cold_default_heap_bytes=948 default_bytes_per_entry=65.25 metadata_bytes_per_entry=15.25 load_ms_median=200 core_load_ms_median=200 sort_during_load=0 lazy_build_ms_median=20 persisted_load_ms_median=240 lazy_saved_bytes=96 freeze_ms=50",
             "FERROW-SCAN entries=2101 errors=0 none_ms_median=10 default_ms_median=20"]
    for query in sorted(gate.QUERIES):
        for prefix in ("FERROW-MODIFIED-QUERY", "FERROW-MODIFIED-SERVICE"):
            lines.append(f'{prefix} query="{query}" first_page_ms=1 page_hits=2 full_count_ms=10 total_hits=2')
    for query in sorted(gate.FILTER_QUERIES):
        lines.append(f'FERROW-FILTER-QUERY query="{query}" first_page_ms=1 page_hits=2 full_count_ms=10 total_hits=2')
    return "\n".join(lines)



class GateTests(unittest.TestCase):
    def test_interleaved_collection_preserves_required_head_observations(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder)
            observations=root/"metadata.txt"
            observations.write_text(metadata_fixture(),encoding="utf-8")
            args=["bench-gate.py",str(root/"base.txt"),str(root/"head.txt"),"--entries","16","--measure","--require-metadata","--head-observations",str(observations)]
            for arm in ("base","head"):
                for field in ("probe","binary","index"):
                    args.extend([f"--{arm}-{field}",str(root/field)])
            def measure(_args,arm,kind,*_):
                return 1,16 if kind=="load" else 2,42,800,16,1
            with patch.object(sys,"argv",args),patch.object(gate,"probe",side_effect=measure),patch.object(gate,"publish") as publish:
                self.assertEqual(gate.main(),0)
                self.assertIn("Required head-only",publish.call_args.args[0])
                self.assertIn("Sample order",publish.call_args.args[0])
            output=(root/"head.txt").read_text(encoding="utf-8")
            self.assertEqual(output.count("FERROW-METADATA "),1)
            self.assertEqual(len(gate.compare(gate.parse((root/"base.txt").read_text(encoding="utf-8"), require_new=False),gate.parse(output),16)),gate.EXPECTED_METRICS)





    def setUp(self):
        self.base = gate.parse(fixture(language=False), require_new=False)
        self.head = gate.parse(fixture())

    def rows(self):
        return gate.compare(self.base, self.head, 16)

    def test_metric_inventory_includes_twenty_four_required_language_observations(self):
        self.assertEqual(gate.PAIRED_METRICS, 26)
        self.assertEqual(gate.EXPECTED_METRICS, 50)
        rows = self.rows()
        self.assertEqual(sum(row[1] is not None for row in rows), 26)
        self.assertEqual(sum(row[1] is None for row in rows), 24)
        # Refuse each missing scalar, query operation, count or entire query,
        # in either arm. A shared omission cannot shrink the paired inventory.
        for language in (False, True):
            lines = fixture(language).splitlines()
            for i, line in enumerate(lines):
                for omitted in [line] + line.split()[1:]:
                    broken = lines.copy()
                    broken[i] = line.replace(omitted, "", 1)
                    with self.subTest(language=language, omitted=omitted), self.assertRaises((ValueError, KeyError)):
                        gate.parse("\n".join(broken), require_new=language)

    def test_legacy_baseline_accepts_only_explicit_legacy_queries(self):
        base = gate.parse(fixture(language=False), require_new=False)
        self.assertEqual(len(gate.compare(base, self.head, 16)), 50)
        with self.assertRaises(ValueError):
            gate.parse(fixture(), require_new=False)

    def test_missing_or_partial_language_readings_fail_closed(self):
        with self.assertRaises(ValueError):
            gate.parse(fixture(language=False))
        for query in gate.NEW_QUERIES:
            for prefix in ("FERROW-QUERY", "FERROW-SERVICE"):
                with self.subTest(query=query, prefix=prefix), self.assertRaises(ValueError):
                    gate.parse(fixture().replace(f'{prefix} query="{query}"', f'IGNORE query="{query}"'))

    def test_common_wildcard_absolute_first_page_limit(self):
        for query in gate.COMMON_WILDCARDS:
            head = copy.deepcopy(self.head)
            for readings in head[1:]:
                readings[query].update(page_hits=100, total_hits=200, first_page_ms=16)
            self.assertFalse(any(row[-1] for row in gate.compare(self.base, head, 16)))
            for arm in (1, 2):
                bad = copy.deepcopy(head)
                bad[arm][query]["first_page_ms"] = 16.001
                self.assertTrue(any(row[-1] for row in gate.compare(self.base, bad, 16)))

    def test_language_counts_and_complete_first_pages_must_agree(self):
        self.head[2]["*.log"]["total_hits"] = 3
        with self.assertRaises(ValueError):
            self.rows()
        self.head = gate.parse(fixture())
        for readings in self.head[1:]:
            readings["*.log"].update(page_hits=0, total_hits=200)
        with self.assertRaises(ValueError):
            self.rows()

    def test_metadata_observation_inventory_keeps_paired_gate(self):
        rows, report = gate.metadata_observations(metadata_fixture(), self.head, 16)
        self.assertEqual(gate.PAIRED_METRICS, 26)
        self.assertEqual(gate.EXPECTED_METRICS, 50)
        self.assertEqual(gate.EXPECTED_TOTAL_METRICS, 86)
        self.assertEqual(len(rows), 36)
        self.assertEqual(len(self.rows()) + len(rows), gate.EXPECTED_TOTAL_METRICS)
        self.assertIn("head-only", report)
        self.head[1]["a"]["full_count_ms"] = 20
        self.assertTrue(any(row[-1] for row in self.rows()))

    def test_every_metadata_field_and_query_is_required(self):
        lines = metadata_fixture().splitlines()
        for i, line in enumerate(lines):
            fields = line.split() if line.startswith(("FERROW-METADATA ", "FERROW-SCAN ")) else [line, "first_page_ms=1", "full_count_ms=10", "page_hits=2", "total_hits=2"]
            for field in fields:
                broken = lines.copy()
                broken[i] = line.replace(field, "", 1)
                with self.subTest(field=field), self.assertRaises((ValueError, KeyError)):
                    gate.metadata_observations("\n".join(broken), self.head, 16)

    def test_metadata_observations_refuse_missing_malformed_and_changed_counts(self):
        text = metadata_fixture()
        for invalid in ("", text.replace('FERROW-MODIFIED-SERVICE query="a"', 'IGNORE query="a"'),
                        text.replace("entries=16", "entries=17"), text.replace("seed=2026", "seed=2027"),
                        text.replace("default_heap_bytes=1044", "default_heap_bytes=1300"),
                        text.replace("load_ms_median=200", "load_ms_median=nan"),
                        text.replace("page_hits=2", "page_hits=3"), text.replace("total_hits=2", "total_hits=3"),
                        text.replace("errors=0", "errors=1"), text + "\n" + text,
                        text.replace('FERROW-FILTER-QUERY query="size:<1mb"', 'IGNORE query="size:<1mb"'),
                        text.replace('FERROW-FILTER-QUERY query="size:<1mb" first_page_ms=1 page_hits=2', 'FERROW-FILTER-QUERY query="size:<1mb" first_page_ms=1 page_hits=1'),
                        text + '\nFERROW-FILTER-QUERY query="size:>2gb" first_page_ms=1 page_hits=2 full_count_ms=10 total_hits=2'):
            with self.subTest(invalid=invalid), self.assertRaises((ValueError, KeyError)):
                gate.metadata_observations(invalid, self.head, 16)

    def test_cli_requires_metadata_when_ci_requests_it(self):
        with tempfile.TemporaryDirectory() as folder:
            base, head = [Path(folder) / name for name in ("base.txt", "head.txt")]
            base.write_text(fixture(language=False), encoding="utf-8")
            for observations, expected in (("", 2), (metadata_fixture(), 0)):
                head.write_text(fixture()+"\n"+observations, encoding="utf-8")
                result = subprocess.run([sys.executable, str(Path(__file__).with_name("bench-gate.py")), str(base), str(head),
                                         "--entries", "16", "--require-metadata"], capture_output=True, timeout=5)
                self.assertEqual(result.returncode, expected, result.stderr)

    def test_metadata_targets_are_enforced_and_rare_pages_remain_observations(self):
        for invalid in (metadata_fixture().replace("core_load_ms_median=200", "core_load_ms_median=100"),
                        metadata_fixture().replace("sort_during_load=0", "sort_during_load=1"),
                        metadata_fixture().replace("first_page_ms=1 ", "first_page_ms=16.001 "),
                        metadata_fixture().replace("default_heap_bytes=1044", "default_heap_bytes=1072")
                        .replace("cold_default_heap_bytes=948", "cold_default_heap_bytes=976")
                        .replace("default_bytes_per_entry=65.25", "default_bytes_per_entry=67")
                        .replace("metadata_bytes_per_entry=15.25", "metadata_bytes_per_entry=17")):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                gate.metadata_observations(invalid, self.head, 16)
        rare = metadata_fixture()
        for query in ("report 2024", "zzzz-no-hit-zzzz"):
            rare = rare.replace(f'query="{query}" first_page_ms=1 ', f'query="{query}" first_page_ms=270 ')
        self.assertEqual(len(gate.metadata_observations(rare, self.head, 16)[0]), gate.EXPECTED_METADATA_METRICS)

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
                    head = copy.deepcopy(self.head)
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

    def test_retry_only_failed_latency_once_paired(self):
        self.head[2]["report 2024"]["first_page_ms"] = 4
        calls = []
        def measure(*args):
            calls.append(args)
            return (1 if args[0] == "base" else 1.05), 2, 42, 800, 16, 1
        rows = gate.retry_failures(self.rows(), self.base, self.head, measure)
        self.assertFalse(any(row[-1] for row in rows))
        # Sub-millisecond first pages collect 10 balanced samples (FR-31).
        expected = ["base", "head"] + [arm for i in range(10) for arm in (("base", "head") if i % 2 == 0 else ("head", "base"))]
        self.assertEqual([call[0] for call in calls], expected)
        self.assertTrue(all(call[1:] == ("IPC", "report 2024", "first_page_ms") for call in calls))
        calls.clear()
        self.head[0]["heap_bytes"] = 1000
        self.assertTrue(any(row[-1] for row in gate.retry_failures(self.rows(), self.base, self.head, measure)))
        self.assertEqual(calls, [])

    def test_retry_repeat_breach_fails_and_bad_evidence_refuses(self):
        self.head[0]["load_ms_median"] = 200
        rows = gate.retry_failures(self.rows(), self.base, self.head,
                                   lambda arm, *_: (100 if arm == "base" else 111, 16, 0, 800, 16, 1))
        self.assertTrue(any(row[-1] for row in rows))
        for bad in ((float("nan"), 16), (100, 17), (-1, 16)):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                gate.retry_failures(self.rows(), self.base, self.head, lambda *_: (*bad, 0, 800, 16, 1))

    def test_retry_pass_does_not_run_producers(self):
        def unexpected(*_):
            self.fail("passing metrics must not be remeasured")
        self.assertEqual(gate.retry_failures(self.rows(), self.base, self.head, unexpected), self.rows())

    def test_interleaved_samples_exclude_warmups_and_take_medians(self):
        calls, trace = [], []
        values = {"base": iter([999, 1, 50, 2, 100, 3]), "head": iter([999, 2, 60, 3, 90, 4])}
        def measure(arm, *_):
            calls.append(arm)
            return next(values[arm]), 2, 42, 800, 16, 1
        result = gate.interleaved(measure, "IPC", "a", "first_page_ms", trace=trace)
        self.assertEqual(calls, ["base", "head", "base", "head", "head", "base", "base", "head", "head", "base", "base", "head"])
        self.assertEqual((result["base"][0], result["head"][0]), (3, 4))
        self.assertIn("warm A warm B A B B A A B B A A B", trace[0])

    def wildcard_head(self, median, cold):
        row = 'query="*.log" first_page_ms=0.2 page_hits=2 full_count_ms=10 total_hits=2'
        text = fixture().replace(row, row.replace("first_page_ms=0.2", f"first_page_ms={median}"))
        if cold is not None:
            text = with_cold(text, {"FERROW-QUERY query=\"*.log\"": (cold, 10), "FERROW-SERVICE query=\"*.log\"": (cold, 10)})
        return gate.compare(gate.parse(fixture(language=False), require_new=False),
                            gate.parse(text, require_cold=cold is not None), 16)

    def test_absolute_limits_are_judged_on_the_cold_sample_not_the_repeated_median(self):
        names = ["new-language in-process '*.log' first_page_ms", "new-language IPC '*.log' first_page_ms"]
        # The repeated median is far under 16 ms, the cold first sample is over: fail.
        failing = [r for r in self.wildcard_head(0.2, 17) if r[0] in names]
        self.assertEqual(len(failing), 2)
        for row in failing:
            self.assertEqual((row[2], row[3], row[4]), (17.0, 16.0, True), row)
        # Cold under the limit passes even when the median is over (the cold sample decides).
        passing = [r for r in self.wildcard_head(20, 15) if r[0] in names]
        self.assertEqual(len(passing), 2)
        for row in passing:
            self.assertEqual((row[2], row[4]), (15.0, False), row)
        # Legacy single-sample files carry no cold field: the single value is the cold sample.
        legacy = [r for r in self.wildcard_head(17, None) if r[0] in names]
        self.assertEqual(len(legacy), 2)
        self.assertTrue(all(r[4] for r in legacy))
        # The paired rows keep using the repeated median.
        paired = gate.compare(gate.parse(fixture(language=False), require_new=False), gate.parse(fixture()), 16)
        self.assertEqual([r[2] for r in paired if r[0] == "in-process 'a' first_page_ms"], [0.2])

    def test_gate_output_reports_cold_and_repeated_numbers(self):
        with tempfile.TemporaryDirectory() as folder:
            args = SimpleNamespace(base=Path(folder)/"base.txt", head=Path(folder)/"head.txt",
                                   base_index=Path(folder)/"fixture", head_index=Path(folder)/"fixture", entries=16)
            def measure(arm, kind, query, operation):
                # repeated median 1 ms, cold sample 7 ms
                return (1, 16, 42, 800, 16, 1) if kind == "load" else (1, 2, 42, 800, 16, 7)
            gate.measure_pair(args, measure, [])
            head_text = args.head.read_text(encoding="utf-8")
            self.assertIn("first_page_cold_ms=7.000000000", head_text)
            self.assertIn("full_count_cold_ms=7.000000000", head_text)
            base = gate.parse(args.base.read_text(encoding="utf-8"), require_new=False)
            head = gate.parse(head_text)
            table = gate.cold_table(base, head)
            self.assertIn("| IPC 'a' first_page_ms | 7.000000 / 1.000000 | 7.000000 / 1.000000 |", table)
            self.assertIn("(limit 16 ms judged on cold)", table)
            self.assertIn("Do not compare a repeated median with an older cold number", table)
            # Legacy files without cold fields produce no table instead of a half-empty one.
            self.assertEqual(gate.cold_table(gate.parse(fixture(language=False), require_new=False), gate.parse(fixture())), "")
            # interleaved reports the median of the cold samples next to the median of the repeats.
            result = gate.interleaved(measure, "IPC", "a", "first_page_ms")
            self.assertEqual((result["base"][0], result["base"][5]), (1, 7))

    def test_cold_samples_are_all_or_none_per_file_and_required_when_producers_ran(self):
        full = with_cold(fixture())
        gate.parse(full, require_cold=True)
        gate.parse(fixture())  # legacy file: none at all is fine
        with self.assertRaises(ValueError):
            gate.parse(fixture(), require_cold=True)
        # One row with a cold field, or one row missing it, in an otherwise complete file.
        one = fixture().replace('FERROW-SERVICE query="a" first_page_ms=0.2', 'FERROW-SERVICE query="a" first_page_cold_ms=1 first_page_ms=0.2', 1)
        with self.assertRaises(ValueError):
            gate.parse(one)
        lines = full.splitlines()
        index = next(i for i, line in enumerate(lines) if line.startswith("FERROW-QUERY") and 'query="re"' in line)
        lines[index] = lines[index].replace(" first_page_cold_ms=0.2", "")
        for require in (False, True):
            with self.assertRaises(ValueError):
                gate.parse("\n".join(lines), require_cold=require)

    def test_a_sample_without_its_cold_value_is_never_substituted(self):
        with self.assertRaisesRegex(ValueError, "cold first sample"):
            gate.interleaved(lambda *_: (1, 2, 42, 800, 16), "IPC", "a", "first_page_ms")

    def test_main_fails_closed_on_missing_cold_samples(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            def run(measure, base_text, head_text, extra):
                (root / "base.txt").write_text(base_text, encoding="utf-8")
                (root / "head.txt").write_text(head_text, encoding="utf-8")
                argv = ["bench-gate.py", str(root / "base.txt"), str(root / "head.txt"), "--entries", "16", *extra]
                for arm in ("base", "head"):
                    for field in ("probe", "binary", "index"):
                        argv += [f"--{arm}-{field}", str(root / "shared") if field == "index" else str(root / field)]
                with patch.object(sys, "argv", argv), patch.object(gate, "publish"), patch.object(gate, "probe", side_effect=measure), patch("sys.stderr"):
                    return gate.main()
            # Producers ran (--measure) but one returns no cold sample: refused.
            five = lambda _a, arm, kind, *_: (1, 16 if kind == "load" else 2, 42, 800, 16)
            self.assertEqual(run(five, "", "", ["--measure"]), 2)
            # Files from producers (no --measure) that carry cold samples on only some rows: refused.
            partial = with_cold(fixture()).replace(" first_page_cold_ms=0.2 full_count_cold_ms=10", "", 1)
            self.assertEqual(run(five, with_cold(fixture(language=False)), partial, []), 2)
            self.assertEqual(run(five, with_cold(fixture(language=False)), with_cold(fixture()), []), 0)

    def test_the_smoke_step_accepts_exactly_exit_codes_0_1_and_3(self):
        import re
        text = (Path(__file__).resolve().parent.parent / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
        step = text.split("Smoke interleaved collection on the shared 16-entry fixture", 1)[1].split("\n      - name:", 1)[0]
        tests = [line.strip() for line in step.splitlines() if line.strip().startswith("test ")]
        self.assertEqual(len(tests), 1, tests)
        self.assertNotIn(" -a ", tests[0])
        terms = re.findall(r'"\$result" -(le|lt|eq|ne|ge|gt) (\d+)', tests[0])
        self.assertEqual(len(terms), tests[0].count('"$result"'))
        compare = {"le": int.__le__, "lt": int.__lt__, "eq": int.__eq__, "ne": int.__ne__, "ge": int.__ge__, "gt": int.__gt__}
        accepted = [code for code in range(0, 8) if any(compare[op](code, int(n)) for op, n in terms)]
        self.assertEqual(accepted, [0, 1, 3])

    def test_submillisecond_first_pages_take_ten_balanced_samples(self):
        def run(value, kind="IPC", operation="first_page_ms", head_value=None):
            calls = []
            def measure(arm, *_):
                calls.append(arm)
                return (head_value if arm == "head" and head_value is not None else value), 2, 42, 800, 16, 1
            result = gate.interleaved(measure, kind, "a", operation)
            return calls, result
        calls, result = run(0.5)
        samples = calls[2:]
        self.assertEqual(len(samples), 20)
        self.assertEqual((samples.count("base"), samples.count("head")), (10, 10))
        # Every arm is first in half of the pairs and second in the other half.
        self.assertEqual([samples[i] for i in range(0, 20, 2)].count("base"), 5)
        self.assertEqual(result["base"][0], 0.5)
        # Above the sub-millisecond regime, for full counts, for loads, or when only one arm
        # is sub-millisecond, the existing 5 (3 for loads) samples stay.
        self.assertEqual(len(run(2.5)[0]), 2 + 10)
        self.assertEqual(len(run(0.5, operation="full_count_ms")[0]), 2 + 10)
        self.assertEqual(len(run(0.5, kind="load", operation="load")[0]), 2 + 6)
        self.assertEqual(len(run(0.5, head_value=2.5)[0]), 2 + 10)

    def test_every_interleaved_sample_must_keep_counts_and_content(self):
        for slot in (1, 2, 4):
            calls = 0
            def measure(*_):
                nonlocal calls
                calls += 1
                sample = [1, 2, 42, 800, 16, 1]
                if calls == 3:
                    sample[slot] += 1
                return tuple(sample)
            with self.subTest(slot=slot), self.assertRaises(ValueError):
                gate.interleaved(measure, "IPC", "a", "first_page_ms")
            self.assertEqual(calls, 3)

    def test_initial_interleaved_measurement_produces_all_50_metrics(self):
        with tempfile.TemporaryDirectory() as folder:
            args = SimpleNamespace(base=Path(folder)/"base.txt", head=Path(folder)/"head.txt",
                                   base_index=Path(folder)/"fixture", head_index=Path(folder)/"fixture", entries=16)
            calls, trace = [], []
            def measure(arm, kind, query, operation):
                calls.append((arm, kind, query, operation))
                return 1, 16 if kind == "load" else 2, 42, 800, 16, 1
            gate.measure_pair(args, measure, trace)
            rows = gate.compare(gate.parse(args.base.read_text(encoding="utf-8"), require_new=False), gate.parse(args.head.read_text(encoding="utf-8")), 16)
            self.assertEqual(len(rows), 50)
            self.assertFalse(any(row[-1] for row in rows))
            self.assertEqual(len(trace), 25)
            # load: 2 warm-ups + 3 samples per arm; sub-ms first pages (12 pairs) take 10 samples
            # per arm, full counts (12 pairs) 5; the 24 head-only observations follow their pair.
            self.assertEqual(len(calls), 8 + 12*(2 + 10*2) + 12*(2 + 5*2) + 12*(1 + 10) + 12*(1 + 5))
            self.assertTrue(all(arm == "head" or kind == "load" or query in gate.QUERIES
                                for arm, kind, query, _ in calls))
            for query in gate.NEW_QUERIES:
                for kind in ("IPC", "in-process"):
                    for operation in ("first_page_ms", "full_count_ms"):
                        self.assertEqual(calls.count(("head", kind, query, operation)),
                                         11 if operation == "first_page_ms" else 6)
            args.head_index = Path(folder)/"different-fixture"
            with self.assertRaises(ValueError):
                gate.measure_pair(args, measure, trace)

    def test_head_only_samples_are_interleaved_warmed_and_validated(self):
        calls = []
        values = iter([999, 1, 50, 2, 100, 3])
        def measure(arm, kind, query, operation):
            calls.append((arm, query))
            return (next(values), 3, 99, 800, 16, 1) if query == "*.log" else (5, 2, 42, 800, 16, 1)
        result = gate.interleaved(measure, "IPC", "a", "first_page_ms",
                                  observation=("IPC", "*.log", "first_page_ms"))
        a, b, c = ("base", "a"), ("head", "a"), ("head", "*.log")
        self.assertEqual(calls, [a,b,c, a,b,c, c,b,a, a,b,c, c,b,a, a,b,c])
        self.assertEqual(result["observation"][0], 3)
        for slot in (0, 1, 2, 3, 4):
            observed = 0
            def broken(arm, kind, query, operation):
                nonlocal observed
                sample = [1, 2, 42, 800, 16, 1]
                if query == "*.log":
                    observed += 1
                    if observed == 3:
                        sample[slot] = float("nan") if slot == 0 else sample[slot] + 1
                return tuple(sample)
            with self.subTest(slot=slot), self.assertRaises(ValueError):
                gate.interleaved(broken, "IPC", "a", "first_page_ms",
                                 observation=("IPC", "*.log", "first_page_ms"))

    def test_observation_failure_never_compares_or_retries_against_base(self):
        # The absolute limit also holds when the fixture has fewer than 100 hits.
        self.head[1]["*.log"]["first_page_ms"] = 16.001
        original = self.rows()
        with patch.object(gate, "interleaved") as retry:
            self.assertEqual(gate.retry_failures(original, self.base, self.head, None), original)
            retry.assert_not_called()
        self.head[2]["a"]["first_page_ms"] = 4
        calls = []
        def measure(arm, kind, query, operation):
            calls.append((arm, kind, query, operation))
            return 1, 2, 42, 800, 16, 1
        rows = gate.retry_failures(self.rows(), self.base, self.head, measure)
        self.assertTrue(any(row[-1] for row in rows))
        self.assertEqual(len(calls), 22)  # 1 ms first page: 2 warm-ups + 10 samples per arm
        self.assertTrue(all(call[2] == "a" for call in calls))
        self.assertEqual([row for row in rows if row[1] is None],
                         [row for row in original if row[1] is None])

    def test_probe_enforces_legacy_only_base(self):
        args = SimpleNamespace(base_probe=Path("probe"), base_binary=Path("ferrow"), base_index=Path("fixture"), entries=16)
        receipt = "FERROW-RETRY value_ms=1 cold_ms=3 hits=2 digest=42 heap_bytes=800 entries=16\n"
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(stdout=receipt, stderr=REPEATS)) as run:
            gate.probe(args, "base", "IPC", "a", "first_page_ms")
            self.assertEqual(run.call_args.args[0][-1], "--legacy-queries")
            for kind in ("IPC", "in-process"):
                for query in gate.NEW_QUERIES:
                    with self.subTest(kind=kind, query=query), self.assertRaises(ValueError):
                        gate.probe(args, "base", kind, query, "first_page_ms")
            self.assertEqual(run.call_count, 1)

    def test_main_retries_with_legacy_only_base_and_preserves_observations(self):
        with tempfile.TemporaryDirectory() as folder:
            base, head = [Path(folder) / name for name in ("base.txt", "head.txt")]
            base.write_text(fixture(language=False), encoding="utf-8")
            head.write_text(fixture().replace("load_ms_median=100", "load_ms_median=200"), encoding="utf-8")
            argv = ["bench-gate.py", str(base), str(head), "--entries", "16"]
            for arm in ("base", "head"):
                for field in ("probe", "binary", "index"):
                    argv += [f"--{arm}-{field}", "dummy"]
            with patch.object(sys, "argv", argv), patch.object(gate, "publish") as publish, \
                    patch.object(gate, "probe", side_effect=lambda *args: (100, 16, 0, 800, 16, 1)) as probe:
                self.assertEqual(gate.main(), 0)
                self.assertEqual(probe.call_count, 8)
                self.assertIn("One paired latency remeasurement", publish.call_args.args[0])
                self.assertIn("new-language", publish.call_args.args[0])

    def test_probe_requests_in_process_repeats_and_proves_they_ran(self):
        args = SimpleNamespace(base_probe=Path("probe"), base_binary=Path("ferrow"), base_index=Path("fixture"), entries=16)
        receipt = "FERROW-RETRY value_ms=1 cold_ms=3 hits=2 digest=42 heap_bytes=800 entries=16\n"
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(stdout=receipt, stderr=REPEATS)) as run:
            gate.probe(args, "base", "IPC", "a", "first_page_ms")
            self.assertEqual(run.call_args.kwargs["env"]["FERROW_PROBE_REPEATS"], str(gate.PROBE_REPEATS))
            # The receipt carries both numbers: repeated median first, cold sample last.
            self.assertEqual(gate.probe(args, "base", "IPC", "a", "first_page_ms"), (1.0, 2, 42, 800, 16, 3.0))
            # Loads time one process start-up and have no repeats to prove.
            gate.probe(args, "base", "load", "", "load")
        # A probe that ignored the request (old driver) or repeated too little is refused.
        for stderr in ("", "FERROW-PROBE-ACTIVE pid=1 timeout_s=120\n", REPEATS * 2,
                       "FERROW-PROBE-REPEATS n=2 min=1 median=1 max=1\n"):
            with self.subTest(stderr=stderr), patch.object(gate.subprocess, "run", return_value=SimpleNamespace(stdout=receipt, stderr=stderr)):
                for kind in ("IPC", "in-process"):
                    with self.assertRaisesRegex(ValueError, "in-process repeats"):
                        gate.probe(args, "base", kind, "a", "first_page_ms")

    def test_retry_producer_failure_and_timeout_are_not_retry_passes(self):
        args = SimpleNamespace(base_probe=Path("probe"), base_binary=Path("ferrow"), base_index=Path("fixture"), entries=16)
        for error in (subprocess.CalledProcessError(42, "probe"), subprocess.TimeoutExpired("probe", 120)):
            with self.subTest(error=error), patch.object(gate.subprocess, "run", side_effect=error) as run:
                with self.assertRaises(subprocess.SubprocessError):
                    gate.probe(args, "base", "IPC", "a", "first_page_ms")
                self.assertEqual(run.call_count, 1)
                self.assertTrue(run.call_args.kwargs["check"])
                self.assertEqual(run.call_args.kwargs["timeout"], 120)

    def test_retry_receipt_must_be_complete_finite_and_unique(self):
        args = SimpleNamespace(base_probe=Path("probe"), base_binary=Path("ferrow"), base_index=Path("fixture"), entries=16)
        receipt = "FERROW-RETRY value_ms=1 cold_ms=3 hits=2 digest=42 heap_bytes=800 entries=16\n"
        for stdout in ("", receipt*2, receipt.replace("value_ms=1", "value_ms=NaN"), "FERROW-RETRY value_ms=1\n",
                       receipt.replace("cold_ms=3 ", ""), receipt.replace("cold_ms=3", "cold_ms=-1")):
            with self.subTest(stdout=stdout), patch.object(gate.subprocess, "run", return_value=SimpleNamespace(stdout=stdout, stderr=REPEATS)):
                with self.assertRaises((ValueError, KeyError)):
                    gate.probe(args, "base", "IPC", "a", "first_page_ms")

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
            base.write_text(fixture(language=False), encoding="utf-8")
            for text, expected in ((fixture(), 0), (fixture().replace("load_ms_median=100", "load_ms_median=200"), 1), ("", 2)):
                head.write_text(text, encoding="utf-8")
                result = subprocess.run([sys.executable, str(Path(__file__).with_name("bench-gate.py")),
                                         str(base), str(head), "--entries", "16"], capture_output=True, timeout=5,
                                        env={**os.environ, "GITHUB_STEP_SUMMARY": str(Path(folder) / "summary.md")})
                self.assertEqual(result.returncode, expected, result.stderr)
            self.assertIn("| Metric | Base | Head |", (Path(folder) / "summary.md").read_text(encoding="utf-8"))


class ConfirmationRoundTests(unittest.TestCase):
    """Timed rows over their limit in round 1 fail only when round 2 agrees."""

    def run_main(self, head_text, producer, extra=()):
        with tempfile.TemporaryDirectory() as folder:
            base, head = [Path(folder) / name for name in ("base.txt", "head.txt")]
            base.write_text(fixture(language=False), encoding="utf-8")
            head.write_text(head_text, encoding="utf-8")
            argv = ["bench-gate.py", str(base), str(head), "--entries", "16", *extra]
            for arm in ("base", "head"):
                for field in ("probe", "binary", "index"):
                    argv += [f"--{arm}-{field}", "dummy"]
            with patch.object(sys, "argv", argv), patch.object(gate, "publish") as publish, \
                    patch.object(gate, "probe", side_effect=producer) as probe, \
                    patch("sys.stderr"):
                self.publish_mock = publish
                code = gate.main()
            return code, probe, "".join(call.args[0] for call in publish.call_args_list)

    def spike(self):
        return fixture().replace('FERROW-SERVICE query="a" first_page_ms=0.2', 'FERROW-SERVICE query="a" first_page_ms=4', 1)

    @staticmethod
    def steady(_args, arm, kind, query, operation):
        return (0.2 if operation == "first_page_ms" else 10), 2, 0, 800, 16, 1

    def test_single_round_spike_passes_and_is_reported(self):
        code, probe, report = self.run_main(self.spike(), self.steady)
        self.assertEqual(code, 0)
        self.assertEqual(probe.call_count, 22)  # only that row again: sub-ms, so 10 samples + 1 warm-up per arm
        self.assertIn("first over limit, confirmed not over", report)
        self.assertNotIn("confirmed regression", report)
        self.assertIn("Round 1 base / head", report)

    def test_persistent_regression_fails_in_both_rounds(self):
        code, probe, report = self.run_main(
            self.spike(), lambda _args, arm, *_: (0.2 if arm == "base" else 4, 2, 0, 800, 16, 1))
        self.assertEqual(code, 1)
        self.assertIn("confirmed regression", report)

    def test_a_a_control_flags_bias_in_either_direction(self):
        base = gate.parse(fixture(language=False), require_new=False)
        name = "IPC 'a' first_page_ms"
        for slow_arm in ("base", "head"):
            def biased(arm, *_):
                return (4 if arm == slow_arm else 0.2), 2, 0, 800, 16, 1
            with self.subTest(slow_arm=slow_arm):
                self.assertTrue(gate.control_rows([name], base, biased)[0][-1])
        control = gate.control_rows([name], base, lambda arm, *_: (0.2, 2, 0, 800, 16, 1))
        self.assertFalse(control[0][-1])
        self.assertIn("identical binaries within the limit", gate.control_table(control))

    def test_confirmed_regression_with_clean_control_is_still_a_regression(self):
        code, probe, report = self.run_main(
            self.spike(), lambda _args, arm, *_: (0.2 if arm == "base" else 4, 2, 0, 800, 16, 1))
        self.assertEqual(code, 1)
        self.assertIn("A/A control", report)
        self.assertIn("identical binaries within the limit", report)
        self.assertNotIn("Harness unresolved: identical", report)
        # The control always runs the head binaries, in both arms.
        self.assertTrue(all(call.args[1] == "head" for call in probe.call_args_list[-12:]))

    def test_biased_harness_is_reported_not_called_a_regression(self):
        producer = lambda _args, arm, *_: (0.2 if arm == "base" else 4, 2, 0, 800, 16, 1)
        def control(names, base, measure, trace=None):
            return [(name, 0.2, 4, True) for name in names]
        with patch.object(gate, "control_rows", side_effect=control):
            code, _, report = self.run_main(self.spike(), producer)
        self.assertEqual(code, 3)
        self.assertIn("Harness unresolved: identical binaries also differ. Rerun; do not merge on this result.", report)
        self.assertNotIn("not evidence of a regression", report)

    def test_one_clean_control_keeps_the_regression_verdict(self):
        two = self.spike().replace('FERROW-SERVICE query="re" first_page_ms=0.2', 'FERROW-SERVICE query="re" first_page_ms=4', 1)
        def control(names, base, measure, trace=None):
            self.assertEqual(len(names), 2)
            return [(name, 0.2, 4, index == 0) for index, name in enumerate(names)]
        with patch.object(gate, "control_rows", side_effect=control):
            code, _, report = self.run_main(two, lambda _args, arm, *_: (0.2 if arm == "base" else 4, 2, 0, 800, 16, 1))
        self.assertEqual(code, 1)
        self.assertNotIn("Harness unresolved: identical", report)

    def test_metadata_refusal_does_not_hide_the_paired_result(self):
        # FR-30 (core + 15% load) is a separate, known failure; the paired rows are still
        # re-measured, controlled and reported before the gate refuses.
        def refuse(*_):
            raise ValueError("default-column load target exceeded (core + 15%)")
        control = lambda names, base, measure, trace=None: [(n, 0.2, 4, False) for n in names]
        regression = lambda _args, arm, *_: (0.2 if arm == "base" else 4, 2, 0, 800, 16, 1)
        with patch.object(gate, "metadata_observations", side_effect=refuse), \
                patch.object(gate, "control_rows", side_effect=control):
            code, probe, report = self.run_main(self.spike(), regression, extra=("--require-metadata",))
        self.assertEqual(code, 2)
        self.assertGreater(probe.call_count, 0)
        self.assertIn("One paired latency remeasurement", report)
        self.assertIn("A/A control", report)
        self.assertIn("default-column load target exceeded", report)
        self.assertNotIn("Harness unresolved: identical", report)

    def test_control_never_runs_for_a_passing_gate_or_a_memory_failure(self):
        with patch.object(gate, "control_rows") as control:
            self.run_main(fixture(), self.steady)
            self.run_main(self.spike(), self.steady)
            self.run_main(self.spike().replace("heap_bytes=800", "heap_bytes=1000"), self.steady)
        control.assert_not_called()

    @staticmethod
    def regression(_args, arm, *_):
        return (0.2 if arm == "base" else 4, 2, 0, 800, 16, 1)

    def test_round_two_report_is_published_before_the_control_runs(self):
        order = []
        def control(names, base, measure, trace=None):
            published = "".join(call.args[0] for call in self.publish_mock.call_args_list)
            order.append((self.publish_mock.call_count, "One paired latency remeasurement" in published, "A/A control" in published))
            return [(name, 0.2, 0.2, False) for name in names]
        with patch.object(gate, "control_rows", side_effect=control):
            code, _, report = self.run_main(self.spike(), self.regression)
        self.assertEqual(code, 1)
        self.assertEqual(order, [(1, True, False)])  # one publish, round 2 in it, no control yet
        self.assertEqual(self.publish_mock.call_count, 2)
        self.assertEqual(report.count("### One paired latency remeasurement"), 1)  # not published twice
        self.assertEqual(report.count("### A/A control"), 1)
        self.assertEqual(report.count("### Sample order"), 1)

    def test_a_failing_control_still_leaves_the_round_two_report_published(self):
        def broken(names, base, measure, trace=None):
            raise subprocess.TimeoutExpired("probe", 120)
        with patch.object(gate, "control_rows", side_effect=broken):
            code, _, report = self.run_main(self.spike(), self.regression)
        self.assertEqual(code, 2)
        self.assertIn("confirmed regression", report)
        self.assertIn("refused", report)

    def spiked_rows(self, count):
        """A head file with `count` paired first-page rows over their limit."""
        text = fixture()
        targets = [("FERROW-QUERY", q) for q in sorted(gate.QUERIES)] + [("FERROW-SERVICE", q) for q in sorted(gate.QUERIES)]
        for prefix, query in targets[:count]:
            text = text.replace(f'{prefix} query="{query}" first_page_ms=0.2', f'{prefix} query="{query}" first_page_ms=4', 1)
        return text

    def test_a_broad_failure_skips_the_control_and_exits_1_directly(self):
        calls = []
        def control(names, base, measure, trace=None):
            calls.append(len(names))
            return [(name, 0.2, 4, True) for name in names]
        with patch.object(gate, "control_rows", side_effect=control):
            code, _, report = self.run_main(self.spiked_rows(gate.MAX_CONTROL_ROWS), self.regression)
            self.assertEqual((code, calls), (3, [gate.MAX_CONTROL_ROWS]))  # 8 rows: the control still runs
            calls.clear()
            code, _, report = self.run_main(self.spiked_rows(gate.MAX_CONTROL_ROWS + 1), self.regression)
        self.assertEqual((code, calls), (1, []))  # 9 rows: not plausibly harness bias
        self.assertIn("no A/A control was run", report)
        self.assertNotIn("Harness unresolved", report)

    def test_the_control_sample_order_is_reported_with_its_own_label(self):
        with patch.object(gate, "control_rows", wraps=gate.control_rows) as control:
            code, _, report = self.run_main(self.spike(), lambda _a, arm, *_: (0.2 if arm == "base" else 4, 2, 0, 800, 16, 1))
        self.assertEqual(code, 1)
        self.assertTrue(control.call_args.args[3][-1].startswith("| A/A control: "))  # the trace list reaches the control
        self.assertIn("| A/A control: IPC 'a' first_page_ms |", report)

    def test_metadata_refusal_wins_over_a_biased_control(self):
        # Mutation guard: a biased control must not turn the refusal (2) into 3.
        def refuse(*_):
            raise ValueError("default-column load target exceeded (core + 15%)")
        biased = lambda names, base, measure, trace=None: [(n, 0.2, 4, True) for n in names]
        with patch.object(gate, "metadata_observations", side_effect=refuse), patch.object(gate, "control_rows", side_effect=biased):
            code, _, report = self.run_main(self.spike(), self.regression, extra=("--require-metadata",))
        self.assertEqual(code, 2)
        self.assertIn("default-column load target exceeded", report)
        self.assertNotIn("Harness unresolved: identical", report)

    def test_an_absolute_limit_failure_beside_biased_paired_rows_is_still_exit_1(self):
        # Mutation guard: only paired timing rows may be excused by the control.
        head = with_cold(self.spike(), {'FERROW-QUERY query="*.log"': (17, 10), 'FERROW-SERVICE query="*.log"': (17, 10)})
        biased = lambda names, base, measure, trace=None: [(n, 0.2, 4, True) for n in names]
        with patch.object(gate, "control_rows", side_effect=biased):
            code, _, report = self.run_main(head, self.regression)
        self.assertEqual(code, 1)
        self.assertNotIn("Harness unresolved: identical", report)

    def test_a_real_control_run_end_to_end_returns_3_when_identical_binaries_differ(self):
        real = gate.control_rows
        phase = {"control": False, "n": 0}
        def wrapped(names, base, measure, trace=None):
            phase["control"] = True
            return real(names, base, measure, trace)
        def producer(_args, arm, *_):
            if not phase["control"]:
                return (0.2 if arm == "base" else 4, 2, 0, 800, 16, 1)
            phase["n"] += 1  # identical binaries, but the second half of the control runs slow
            return (0.2 if phase["n"] <= 11 else 4, 2, 0, 800, 16, 1)
        with patch.object(gate, "control_rows", side_effect=wrapped):
            code, probe, report = self.run_main(self.spike(), producer)
        self.assertEqual(code, 3)
        self.assertIn("Harness unresolved: identical binaries also differ. Rerun; do not merge on this result.", report)
        self.assertIn("harness unresolved", report)  # the control table row
        self.assertIn("confirmed regression", report)  # round 2 is still reported
        # And a quiet control keeps the regression verdict through the same real path.
        phase.update(control=False, n=0)
        with patch.object(gate, "control_rows", side_effect=wrapped):
            code, _, report = self.run_main(self.spike(), lambda _a, arm, *_: (0.2 if arm == "base" or phase["control"] else 4, 2, 0, 800, 16, 1))
        self.assertEqual(code, 1)

    def test_nothing_over_runs_no_second_round(self):
        code, probe, report = self.run_main(fixture(), self.steady)
        self.assertEqual(code, 0)
        self.assertEqual(probe.call_count, 0)
        self.assertNotIn("Confirmation round", report)

    def test_missing_confirmation_data_fails_closed(self):
        for error in (ValueError("retry producer must emit exactly one receipt"),
                      subprocess.TimeoutExpired("probe", 120)):
            with self.subTest(error=error):
                code, probe, report = self.run_main(self.spike(), error)
                self.assertEqual(code, 2)
                self.assertIn("refused", report)
        code, _, _ = self.run_main("", self.steady)
        self.assertEqual(code, 2)

    def test_memory_rows_are_never_remeasured(self):
        code, probe, report = self.run_main(
            fixture().replace("heap_bytes=800", "heap_bytes=1000"), self.steady)
        self.assertEqual(code, 1)
        self.assertEqual(probe.call_count, 0)
        # Latency spikes beside a memory regression are not re-measured either.
        code, probe, _ = self.run_main(self.spike().replace("heap_bytes=800", "heap_bytes=1000"), self.steady)
        self.assertEqual(code, 1)
        self.assertEqual(probe.call_count, 0)

    def test_core_plus_15_percent_follows_the_same_rule(self):
        slow = metadata_fixture().replace("load_ms_median=200 core", "load_ms_median=260 core")
        head = gate.parse(fixture())
        rerun = []
        def second(values):
            def run():
                rerun.append(values)
                return values
            return run
        # Spike: round 2 is within core + 15%. Passes and is reported.
        rows, report = gate.metadata_observations(slow, head, 16, second((200, 210)))
        self.assertEqual(len(rows), gate.EXPECTED_METADATA_METRICS)
        self.assertIn("first round over limit", report)
        self.assertIn("confirmed not over", report)
        # Persistent: over in both rounds fails.
        with self.assertRaisesRegex(ValueError, r"core \+ 15%\), confirmed in a second round"):
            gate.metadata_observations(slow, head, 16, second((200, 260)))
        # Within limit in round 1: no second round.
        rerun.clear()
        gate.metadata_observations(metadata_fixture(), head, 16, second((1, 1000)))
        self.assertEqual(rerun, [])
        # Missing or invalid round-2 data fails closed.
        def broken():
            raise ValueError("invalid default-column load confirmation round")
        with self.assertRaises(ValueError):
            gate.metadata_observations(slow, head, 16, broken)
        with self.assertRaises(ValueError):
            gate.metadata_observations(slow, head, 16, second((float("nan"), 1)))
        # Without a confirmation producer the first round stays final.
        with self.assertRaisesRegex(ValueError, "core \\+ 15%"):
            gate.metadata_observations(slow, head, 16)

    def test_main_wires_the_metadata_rerun_flags(self):
        slow = metadata_fixture().replace("load_ms_median=200 core_load_ms_median=200",
                                          "load_ms_median=240 core_load_ms_median=200")
        rerun_ok = metadata_fixture().replace("load_ms_median=200 core_load_ms_median=200",
                                              "load_ms_median=210 core_load_ms_median=200")
        rerun_over = slow
        with tempfile.TemporaryDirectory() as folder:
            base, head = [Path(folder) / name for name in ("base.txt", "head.txt")]
            base.write_text(fixture(language=False), encoding="utf-8")
            flags = ["--metadata-rerun-bench", "bench", "--metadata-rerun-index", "idx"]
            for extra, rerun, expected, runs in ((flags, rerun_ok, 0, 1), (flags, rerun_over, 2, 1), ([], rerun_ok, 2, 0)):
                with self.subTest(flags=bool(extra), rerun=rerun is rerun_ok):
                    head.write_text(fixture() + "\n" + slow, encoding="utf-8")
                    argv = ["bench-gate.py", str(base), str(head), "--entries", "16", "--require-metadata", *extra]
                    with patch.object(sys, "argv", argv), patch.object(gate, "publish"), patch("sys.stderr"), \
                            patch.object(gate.subprocess, "run", return_value=SimpleNamespace(stdout=rerun)) as run:
                        self.assertEqual(gate.main(), expected)
                        self.assertEqual(run.call_count, runs)

    def test_metadata_rerun_parses_and_validates_the_second_round(self):
        args = SimpleNamespace(metadata_rerun_bench=Path("bench"), metadata_rerun_index=Path("idx"), entries=16)
        good = SimpleNamespace(stdout=metadata_fixture().replace("load_ms_median=200 core_load_ms_median=200",
                                                               "load_ms_median=210 core_load_ms_median=200"))
        with patch.object(gate.subprocess, "run", return_value=good) as run:
            self.assertEqual(gate.metadata_rerun(args), (200, 210))
            self.assertEqual(run.call_args.kwargs["timeout"], 600)
        for bad in ("", metadata_fixture().replace("entries=16", "entries=17")):
            with self.subTest(bad=bad), patch.object(gate.subprocess, "run", return_value=SimpleNamespace(stdout=bad)):
                with self.assertRaises(ValueError):
                    gate.metadata_rerun(args)


if __name__ == "__main__":
    unittest.main()
