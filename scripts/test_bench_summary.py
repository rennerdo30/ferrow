"""Profiling workflow/summary probes. Producers are mocked; no benchmark runs."""
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[1]
PROFILE = """FERROW-MEASURE entries=4 seed=7 generate_ms=1 freeze_ms=2 bytes_per_entry=49 file_bytes=200
FERROW-MEASURE load_ms_median=3 load_read_ms=0.1 load_checksum_ms=0.2 load_decode_ms=0.3 load_structure_ms=0.4 load_derived_ms=0.5 load_buffer_ms=0.6 load_cleanup_ms=0.7 load_fold_ms=0.2 load_sort_ms=0.3 load_unattributed_ms=0.2
FERROW-QUERY query="a" first_page_ms=0.1 page_hits=2 full_count_ms=0.2 total_hits=2
FERROW-QUERY-PARTS query="a" operation=page total_ms=0.1 prepare_ms=0.01 scan_ms=0.07 collect_ms=0.01 unattributed_ms=0.01
"""


class SummaryTests(unittest.TestCase):
    def shell(self, command, cwd, env):
        child = subprocess.Popen(
            [shutil.which("bash"), "--noprofile", "--norc", "-eo", "pipefail", "-c", command],
            cwd=cwd, env={**os.environ, **env}, text=True, encoding="utf-8",
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        print(f"profiling probe PID={child.pid} timeout_s=5", flush=True)
        try:
            out, err = child.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.communicate()
            raise
        return child.returncode, out, err

    def summary(self, root, content):
        path = root / "input.txt"
        if content is not None:
            path.write_text(content, encoding="utf-8")
        summary = root / "summary.md"
        summary.write_text("existing evidence\n", encoding="utf-8")
        code, out, err = self.shell(
            'sh "$SUMMARY_SCRIPT" "$PROFILE_INPUT"', ROOT,
            {"SUMMARY_SCRIPT": (ROOT / "scripts/bench-summary.sh").as_posix(),
             "PROFILE_INPUT": path.as_posix(), "GITHUB_STEP_SUMMARY": summary.as_posix()},
        )
        return code, out, err, summary.read_text(encoding="utf-8")

    def test_missing_empty_and_non_measurement_input_refused_without_publishing(self):
        for content in (None, "", " \n", "divan output only\n"):
            with self.subTest(content=content), tempfile.TemporaryDirectory() as folder:
                code, _, err, summary = self.summary(Path(folder), content)
                self.assertNotEqual(code, 0, err)
                self.assertEqual(summary, "existing evidence\n")

    def test_partial_profile_refused_without_publishing(self):
        for content in (PROFILE.replace("freeze_ms=2", ""),
                        "\n".join(PROFILE.splitlines()[:2])):
            with self.subTest(content=content), tempfile.TemporaryDirectory() as folder:
                code, _, err, summary = self.summary(Path(folder), content)
                self.assertNotEqual(code, 0, err)
                self.assertEqual(summary, "existing evidence\n")

    def test_trivial_profile_appends_complete_summary(self):
        with tempfile.TemporaryDirectory() as folder:
            code, _, err, summary = self.summary(Path(folder), PROFILE)
            self.assertEqual(code, 0, err)
            self.assertTrue(summary.startswith("existing evidence\n"))
            self.assertIn("Synthetic fixture: 4 entries, seed 7", summary)
            self.assertIn("| Memory | 49 bytes/entry |", summary)
            self.assertIn("| `a` | 0.1 ms | 0.2 ms | 2 |", summary)
            self.assertIn("| `a` | page | 0.1 |", summary)

    def test_event_selection_runs_exactly_one_producer_and_supplies_summary(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        # Read just the named profiling steps, keeping their actual shell bodies
        # and event conditions. No YAML dependency or replacement workflow.
        steps = [(name, body) for name, body in re.findall(
            r"^      - name: ([^\n]+)\n(.*?)(?=^      - |\Z)", workflow, re.M | re.S
        ) if "fixture" in name and "(profiling only)" in name]
        for event in ("pull_request", "push"):
            with self.subTest(event=event), tempfile.TemporaryDirectory() as folder:
                root = Path(folder)
                selected = []
                for name, body in steps:
                    condition = re.search(r"^        if: github.event_name == '([^']+)'$", body, re.M)
                    self.assertIsNotNone(condition, name)
                    if condition[1] == event:
                        selected.append((name, body))
                self.assertEqual(len(selected), 1, f"{event}: {selected}")
                _, body = selected[0]
                base = "working-directory: .bench-base" in body
                self.assertEqual(base, event == "pull_request")
                cwd = root / ".bench-base" if base else root
                cwd.mkdir(exist_ok=True)
                env = {"GITHUB_WORKSPACE": root.as_posix(), "MOCK_PROFILE": PROFILE,
                       "MOCK_CALLS": (root / "calls.txt").as_posix(),
                       "CARGO_TARGET_DIR": "head-target", "GITHUB_STEP_SUMMARY": ""}
                env_part, command = body.split("        run: |\n", 1)
                for key, value in re.findall(r"^          (\w+): (.+)$", env_part, re.M):
                    env[key] = value.strip('"').replace("${{ runner.temp }}", root.as_posix())
                mock = '''
                git() { echo mock-commit; }
                cargo() {
                    printf 'cwd=%s target=%s index=%s args=%s\\n' "${PWD##*/}" "$CARGO_TARGET_DIR" "$FERROW_BENCH_INDEX_PATH" "$*" >> "$MOCK_CALLS"
                    printf '%s' "$MOCK_PROFILE"
                }
                timeout() { test "$1" = '-k'; shift 3; "$@"; }
                '''
                code, _, err = self.shell(textwrap.dedent(mock) + textwrap.dedent(command), cwd, env)
                self.assertEqual(code, 0, err)
                output = root / "fixture-output.txt"
                self.assertEqual(output.read_text(encoding="utf-8"), PROFILE)
                calls = (root / "calls.txt").read_text(encoding="utf-8")
                self.assertEqual(len(calls.splitlines()), 1)
                self.assertIn("bench -p ferrow-query --bench m1_bench --locked", calls)
                self.assertIn("ferrow-bench.bin", calls)
                self.assertIn("target=" + (root.as_posix() + "/ferrow-base-target" if base else "head-target"), calls)
                code, out, err = self.shell('sh "$SUMMARY_SCRIPT" "$PROFILE_INPUT"', ROOT,
                    {**env, "SUMMARY_SCRIPT": (ROOT / "scripts/bench-summary.sh").as_posix(),
                     "PROFILE_INPUT": output.as_posix()})
                self.assertEqual(code, 0, err)
                self.assertIn("Synthetic fixture: 4 entries, seed 7", out)


if __name__ == "__main__":
    unittest.main()
