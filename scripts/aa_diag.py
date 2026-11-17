#!/usr/bin/env python3
"""TEMPORARY A/A diagnostic (FR-31): identical head binaries in two locations.

Not part of the gate. Prints raw per-arm samples so the arm bias is visible.
"""
import hashlib
import importlib.util
import os
import statistics
import sys
from argparse import Namespace
from pathlib import Path

spec = importlib.util.spec_from_file_location("gate", Path(__file__).with_name("bench-gate.py"))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()[:12]


def run_config(name, base_dirs, head_dirs, index, rows, pairs):
    # each arm = (probe dir, service-binary dir)
    base_dir, head_dir = base_dirs[0], head_dirs[0]
    args = Namespace(
        base_probe=Path(base_dirs[0]) / "examples/latency_probe", base_binary=Path(base_dirs[1]) / "ferrow", base_index=Path(index),
        head_probe=Path(head_dirs[0]) / "examples/latency_probe", head_binary=Path(head_dirs[1]) / "ferrow", head_index=Path(index),
        entries=5_000_000)
    print(f"\n### {name}\n")
    print(f"base dir `{base_dir}` (len {len(str(base_dir))}), head dir `{head_dir}` (len {len(str(head_dir))})")
    print(f"sha256[:12] base ferrow={sha(args.base_binary)} probe={sha(args.base_probe)}; "
          f"head ferrow={sha(args.head_binary)} probe={sha(args.head_probe)}")
    print("\n| Row | base median | head median | head/base | base samples | head samples | first-in-pair median | second-in-pair median |\n|---|---:|---:|---:|---|---|---:|---:|")
    for kind, query, op in rows:
        # warm both arms (excluded), then balanced AB/BA pairs
        for arm in ("base", "head"):
            gate.probe(args, arm, kind, query, op)
        samples = {"base": [], "head": []}
        first, second = [], []
        for i in range(pairs):
            order = ("base", "head") if i % 2 == 0 else ("head", "base")
            for pos, arm in enumerate(order):
                value = gate.probe(args, arm, kind, query, op)[0]
                samples[arm].append(value)
                (first if pos == 0 else second).append(value)
        b, h = statistics.median(samples["base"]), statistics.median(samples["head"])
        fmt = lambda xs: " ".join(f"{x:.3f}" for x in xs)
        print(f"| {kind} {query!r} {op} | {b:.4f} | {h:.4f} | {h / b:.3f} | {fmt(samples['base'])} | {fmt(samples['head'])} "
              f"| {statistics.median(first):.4f} | {statistics.median(second):.4f} |", flush=True)


def main():
    index, pairs = sys.argv[1], int(sys.argv[2])
    real_base, real_head, copy_a, copy_b, copy_c = sys.argv[3:8]
    rows = [("IPC", "!tmp log", "first_page_ms"), ("IPC", "a", "first_page_ms"),
            ("in-process", "!tmp log", "first_page_ms")]
    d = lambda x: (x, x)
    configs = {
        "C0 real base build vs real head build": (d(real_base), d(real_head)),
        "C5 cross: base arm = base probe + HEAD service binary, head arm = head probe + head service binary":
            ((real_base, real_head), d(real_head)),
        "C4 A/A same path for both arms (pure order/slot bias)": (d(real_head), d(real_head)),
    }
    wanted = os.environ.get("AA_CONFIGS", "C0,C5,C4").split(",")
    for name, (b, h) in configs.items():
        if name.split()[0] in wanted:
            run_config(name, b, h, index, rows, pairs)


if __name__ == "__main__":
    main()
