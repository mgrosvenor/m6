#!/usr/bin/env python3
"""Append one JSON record per performance run to tools/perf-history.jsonl.

A number without the conditions it was taken under cannot be compared with
anything later. tools/perfcheck.sh measures and gates; this records what the
measurement was taken on, so a later reader can tell a real regression from a
busy afternoon.

It is called once, at the end of a run, from tools/perfcheck.sh:

    perf_history.py <history-file> <raw-file> <baseline-file> \\
                    <margin> <runs> <load-before> <load-after>

`raw-file` is one line per target: "<key> <ns> <ns> ...", every reading kept.

WHAT IS DELIBERATELY ABSENT. No hostname, no address, no domain, no account
name. What class of machine a number came from, and how busy it was, is the
question a later reader has. Which machine it was is not, and m6 is a generic
web engine whose repository names no deployment.
"""

import json
import os
import platform
import subprocess
import sys
import time


def sh(*cmd):
    """Run a command and return its stripped stdout, or None."""
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=10)
    except (OSError, subprocess.SubprocessError):
        return None
    if out.returncode != 0:
        return None
    return out.stdout.strip() or None


def cpu_model():
    """The CPU model, on Linux and on macOS. A hardware fact, not an identity."""
    try:
        with open("/proc/cpuinfo", encoding="utf-8") as fh:
            for line in fh:
                if line.startswith("model name"):
                    return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return sh("sysctl", "-n", "machdep.cpu.brand_string")


def physical_cores():
    """Physical cores, which is what a wall-clock number is sensitive to.

    Hyperthreads do not add throughput to a latency measurement, so the
    logical count would overstate the machine.
    """
    lscpu = sh("lscpu", "-p=Core,Socket")
    if lscpu:
        pairs = {ln for ln in lscpu.splitlines() if not ln.startswith("#")}
        if pairs:
            return len(pairs)
    macos = sh("sysctl", "-n", "hw.physicalcpu")
    if macos and macos.isdigit():
        return int(macos)
    return os.cpu_count()


def total_memory_kb():
    try:
        with open("/proc/meminfo", encoding="utf-8") as fh:
            for line in fh:
                if line.startswith("MemTotal:"):
                    return int(line.split()[1])
    except (OSError, ValueError):
        pass
    macos = sh("sysctl", "-n", "hw.memsize")
    if macos and macos.isdigit():
        return int(macos) // 1024
    return None


def git(*args):
    return sh("git", *args)


def baselines(path):
    """The recorded numbers, so a record says what it was compared against."""
    found = {}
    try:
        with open(path, encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if not line or line.startswith("#"):
                    continue
                parts = line.split()
                if len(parts) >= 2 and parts[1].isdigit():
                    found[parts[0]] = int(parts[1])
    except OSError:
        pass
    return found


def read_targets(raw_path, recorded):
    targets = {}
    try:
        with open(raw_path, encoding="utf-8") as fh:
            lines = fh.readlines()
    except OSError:
        return targets
    for line in lines:
        parts = line.split()
        if not parts:
            continue
        key, values = parts[0], [int(v) for v in parts[1:] if v.isdigit()]
        if not values:
            # A target that measured nothing is recorded as such. A gap in the
            # history is worse than a record saying the run failed here.
            targets[key] = {"readings": [], "min_ns": None, "baseline_ns": recorded.get(key)}
            continue
        targets[key] = {
            "readings": values,
            "min_ns": min(values),
            "max_ns": max(values),
            "spread_pct": round((max(values) - min(values)) * 100 / min(values), 1),
            "baseline_ns": recorded.get(key),
        }
    return targets


def main():
    if len(sys.argv) != 8:
        print(__doc__, file=sys.stderr)
        return 2
    history, raw, baseline, margin, runs, load_before, load_after = sys.argv[1:]

    recorded = baselines(baseline)
    record = {
        "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "commit": git("rev-parse", "HEAD"),
        "dirty": bool(git("status", "--porcelain")),
        "toolchain": sh("rustc", "--version"),
        "profile": "release",
        "machine": {
            "cpu": cpu_model(),
            "physical_cores": physical_cores(),
            "os": f"{platform.system()} {platform.release()}",
            "arch": platform.machine(),
            "memory_kb": total_memory_kb(),
        },
        "load_before": _num(load_before),
        "load_after": _num(load_after),
        "runs": int(runs) if runs.isdigit() else None,
        "margin_pct": int(margin) if margin.isdigit() else None,
        "targets": read_targets(raw, recorded),
    }

    with open(history, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(record, sort_keys=True) + "\n")
    return 0


def _num(text):
    try:
        return float(text)
    except (TypeError, ValueError):
        return None


if __name__ == "__main__":
    sys.exit(main())
