#!/usr/bin/env python3
"""TEMPORARY A/A diagnostic (FR-31). Not part of the gate.

Prints raw per-arm samples so arm bias and variance are visible.
FERROW_PROBE_REPEATS (set here) makes each probe process repeat the timed
operation and report its median; the min/max of those repeats are printed so
within-process and between-process variance can be told apart.
"""
import contextlib
import hashlib
import importlib.util
import io
import os
import re
import statistics
import sys
from argparse import Namespace
from pathlib import Path

spec = importlib.util.spec_from_file_location("gate", Path(__file__).with_name("bench-gate.py"))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()[:12]


def one(args, arm, kind, query, op):
    err = io.StringIO()
    with contextlib.redirect_stderr(err):
        value = gate.probe(args, arm, kind, query, op)[0]
    spread = re.search(r"FERROW-PROBE-REPEATS n=(\d+) min=([\d.]+) median=([\d.]+) max=([\d.]+)", err.getvalue())
    return value, (tuple(float(x) for x in spread.groups()) if spread else None)


def run_config(name, base_dir, head_dir, index, rows, pairs):
    args = Namespace(
        base_probe=Path(base_dir) / "examples/latency_probe", base_binary=Path(base_dir) / "ferrow", base_index=Path(index),
        head_probe=Path(head_dir) / "examples/latency_probe", head_binary=Path(head_dir) / "ferrow", head_index=Path(index),
        entries=5_000_000)
    print(f"\n### {name} (FERROW_PROBE_REPEATS={os.environ.get('FERROW_PROBE_REPEATS', '1')})\n")
    print(f"base dir `{base_dir}`, head dir `{head_dir}`")
    print(f"sha256[:12] base ferrow={sha(args.base_binary)} probe={sha(args.base_probe)}; "
          f"head ferrow={sha(args.head_binary)} probe={sha(args.head_probe)}")
    print("\n| Row | base median | head median | head/base | base per-process medians | head per-process medians | typical within-process min..max (n) |\n|---|---:|---:|---:|---|---|---|")
    for kind, query, op in rows:
        for arm in ("base", "head"):  # warm-up, excluded
            gate.probe(args, arm, kind, query, op)
        samples, spreads = {"base": [], "head": []}, []
        for i in range(pairs):
            for arm in (("base", "head") if i % 2 == 0 else ("head", "base")):
                value, spread = one(args, arm, kind, query, op)
                samples[arm].append(value)
                if spread:
                    spreads.append(spread)
        b, h = statistics.median(samples["base"]), statistics.median(samples["head"])
        fmt = lambda xs: " ".join(f"{x:.3f}" for x in xs)
        typical = (f"{statistics.median(s[1] for s in spreads):.3f}..{statistics.median(s[3] for s in spreads):.3f} (n={spreads[0][0]:.0f})"
                   if spreads else "-")
        print(f"| {kind} {query!r} {op} | {b:.4f} | {h:.4f} | {h / b:.3f} | {fmt(samples['base'])} | {fmt(samples['head'])} | {typical} |", flush=True)


def main():
    index, pairs, repeats = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    real_base, real_head = sys.argv[4:6]
    rows = [("IPC", "!tmp log", "first_page_ms"), ("IPC", "re", "first_page_ms"), ("IPC", "a", "first_page_ms"),
            ("in-process", "!tmp log", "first_page_ms")]
    configs = {
        "C0 real base build vs real head build": (real_base, real_head),
        "C4 A/A same path for both arms (pure order/slot bias)": (real_head, real_head),
    }
    for reps in repeats.split(","):
        os.environ["FERROW_PROBE_REPEATS"] = reps
        for name, (b, h) in configs.items():
            run_config(name, b, h, index, rows, pairs)


if __name__ == "__main__":
    main()
