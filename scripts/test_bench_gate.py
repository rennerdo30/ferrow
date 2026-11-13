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


def fixture(language=True):
    lines = ["FERROW-MEASURE entries=16 seed=2026 heap_bytes=800 bytes_per_entry=50",
             "FERROW-MEASURE load_ms_median=100"]
    for query in sorted(gate.QUERIES | (gate.NEW_QUERIES if language else set())):
        for prefix in ("FERROW-QUERY", "FERROW-SERVICE"):
            lines.append(f'{prefix} query="{query}" first_page_ms=0.2 page_hits=2 full_count_ms=10 total_hits=2')
    return "\n".join(lines)


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
                return 1,16 if kind=="load" else 2,42,800,16
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
            return (1 if args[0] == "base" else 1.05), 2, 42, 800, 16
        rows = gate.retry_failures(self.rows(), self.base, self.head, measure)
        self.assertFalse(any(row[-1] for row in rows))
        self.assertEqual([call[0] for call in calls], ["base", "head", "base", "head", "head", "base", "base", "head", "head", "base", "base", "head"])
        self.assertTrue(all(call[1:] == ("IPC", "report 2024", "first_page_ms") for call in calls))
        calls.clear()
        self.head[0]["heap_bytes"] = 1000
        self.assertTrue(any(row[-1] for row in gate.retry_failures(self.rows(), self.base, self.head, measure)))
        self.assertEqual(calls, [])

    def test_retry_repeat_breach_fails_and_bad_evidence_refuses(self):
        self.head[0]["load_ms_median"] = 200
        rows = gate.retry_failures(self.rows(), self.base, self.head,
                                   lambda arm, *_: (100 if arm == "base" else 111, 16, 0, 800, 16))
        self.assertTrue(any(row[-1] for row in rows))
        for bad in ((float("nan"), 16), (100, 17), (-1, 16)):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                gate.retry_failures(self.rows(), self.base, self.head, lambda *_: (*bad, 0, 800, 16))

    def test_retry_pass_does_not_run_producers(self):
        def unexpected(*_):
            self.fail("passing metrics must not be remeasured")
        self.assertEqual(gate.retry_failures(self.rows(), self.base, self.head, unexpected), self.rows())

    def test_interleaved_samples_exclude_warmups_and_take_medians(self):
        calls, trace = [], []
        values = {"base": iter([999, 1, 50, 2, 100, 3]), "head": iter([999, 2, 60, 3, 90, 4])}
        def measure(arm, *_):
            calls.append(arm)
            return next(values[arm]), 2, 42, 800, 16
        result = gate.interleaved(measure, "IPC", "a", "first_page_ms", trace=trace)
        self.assertEqual(calls, ["base", "head", "base", "head", "head", "base", "base", "head", "head", "base", "base", "head"])
        self.assertEqual((result["base"][0], result["head"][0]), (3, 4))
        self.assertIn("warm A warm B A B B A A B B A A B", trace[0])

    def test_every_interleaved_sample_must_keep_counts_and_content(self):
        for slot in (1, 2, 4):
            calls = 0
            def measure(*_):
                nonlocal calls
                calls += 1
                sample = [1, 2, 42, 800, 16]
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
                return 1, 16 if kind == "load" else 2, 42, 800, 16
            gate.measure_pair(args, measure, trace)
            rows = gate.compare(gate.parse(args.base.read_text(encoding="utf-8"), require_new=False), gate.parse(args.head.read_text(encoding="utf-8")), 16)
            self.assertEqual(len(rows), 50)
            self.assertFalse(any(row[-1] for row in rows))
            self.assertEqual(len(trace), 25)
            self.assertEqual(len(calls), 8 + 24*12 + 24*6)
            self.assertTrue(all(arm == "head" or kind == "load" or query in gate.QUERIES
                                for arm, kind, query, _ in calls))
            for query in gate.NEW_QUERIES:
                for kind in ("IPC", "in-process"):
                    for operation in ("first_page_ms", "full_count_ms"):
                        self.assertEqual(calls.count(("head", kind, query, operation)), 6)
            args.head_index = Path(folder)/"different-fixture"
            with self.assertRaises(ValueError):
                gate.measure_pair(args, measure, trace)

    def test_head_only_samples_are_interleaved_warmed_and_validated(self):
        calls = []
        values = iter([999, 1, 50, 2, 100, 3])
        def measure(arm, kind, query, operation):
            calls.append((arm, query))
            return (next(values), 3, 99, 800, 16) if query == "*.log" else (1, 2, 42, 800, 16)
        result = gate.interleaved(measure, "IPC", "a", "first_page_ms",
                                  observation=("IPC", "*.log", "first_page_ms"))
        a, b, c = ("base", "a"), ("head", "a"), ("head", "*.log")
        self.assertEqual(calls, [a,b,c, a,b,c, c,b,a, a,b,c, c,b,a, a,b,c])
        self.assertEqual(result["observation"][0], 3)
        for slot in (0, 1, 2, 3, 4):
            observed = 0
            def broken(arm, kind, query, operation):
                nonlocal observed
                sample = [1, 2, 42, 800, 16]
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
            return 1, 2, 42, 800, 16
        rows = gate.retry_failures(self.rows(), self.base, self.head, measure)
        self.assertTrue(any(row[-1] for row in rows))
        self.assertEqual(len(calls), 12)
        self.assertTrue(all(call[2] == "a" for call in calls))
        self.assertEqual([row for row in rows if row[1] is None],
                         [row for row in original if row[1] is None])

    def test_probe_enforces_legacy_only_base(self):
        args = SimpleNamespace(base_probe=Path("probe"), base_binary=Path("ferrow"), base_index=Path("fixture"), entries=16)
        receipt = "FERROW-RETRY value_ms=1 hits=2 digest=42 heap_bytes=800 entries=16\n"
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(stdout=receipt)) as run:
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
                    patch.object(gate, "probe", side_effect=lambda *args: (100, 16, 0, 800, 16)) as probe:
                self.assertEqual(gate.main(), 0)
                self.assertEqual(probe.call_count, 8)
                self.assertIn("One paired latency remeasurement", publish.call_args.args[0])
                self.assertIn("new-language", publish.call_args.args[0])

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
        receipt = "FERROW-RETRY value_ms=1 hits=2 digest=42 heap_bytes=800 entries=16\n"
        for stdout in ("", receipt*2, receipt.replace("value_ms=1", "value_ms=NaN"), "FERROW-RETRY value_ms=1\n"):
            with self.subTest(stdout=stdout), patch.object(gate.subprocess, "run", return_value=SimpleNamespace(stdout=stdout)):
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


if __name__ == "__main__":
    unittest.main()
