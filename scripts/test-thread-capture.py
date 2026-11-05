"""Native Linux x86_64 checks: example binary, fresh scratch directory.

Requires GNU timeout, util-linux prlimit, and GNU addr2line. No Sentry upload.
"""

import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import sys
import time

binary = Path(sys.argv[1]).resolve()
directory = Path(sys.argv[2]).resolve()
directory.mkdir(parents=True, exist_ok=True)
env = dict(os.environ, TMPDIR=str(directory))
summary = []


def run(mode, success):
    report = directory / f"{mode}.json"
    start = time.monotonic()
    result = subprocess.run([str(binary), str(report), mode], env=env,
                            capture_output=True, text=True, timeout=20)
    elapsed = time.monotonic() - start
    (directory / f"{mode}.log").write_text(result.stdout + result.stderr)
    assert "target resumed; blocked worker completed" in result.stdout, result
    assert (result.returncode == 0) == success, result
    assert report.stat().st_mode & 0o777 == 0o600
    if success:
        data = json.loads(report.read_text())
        samples = data["samples"]
        assert len(samples) == (1 if mode == "hold" else 20)
        assert all(0 < s["stack_bytes"] <= 65536 for s in samples)
        if mode == "short-stack":
            assert all(s["stack_bytes"] == 8 for s in samples)
            assert all(s["frames"] and s["termination"].startswith("unwind stopped:") for s in samples)
        else:
            assert all(len(s["frames"]) > 1 for s in samples)
            assert all(s["termination"] == "complete" for s in samples)
        assert all(s["request_ns"] >= s["interrupt_to_detach_ns"] for s in samples)
        if mode == "hold":
            assert "only target traced; reporter and heartbeat progressed" in result.stdout
        print(f"{mode}: {len(samples)} snapshots; "
              f"ready {data['collector_ready_ns'] / 1e6:.3f} ms; "
              f"stop bound median {statistics.median(s['interrupt_to_detach_ns'] for s in samples) / 1000:.3f} us; "
              f"max {max(s['interrupt_to_detach_ns'] for s in samples) / 1000:.3f} us; "
              f"request median {statistics.median(s['request_ns'] for s in samples) / 1000:.3f} us")
        summary.append({"mode": mode, "elapsed_s": elapsed, "samples": len(samples),
                        "ready_ns": data["collector_ready_ns"],
                        "stop_median_ns": statistics.median(s["interrupt_to_detach_ns"] for s in samples),
                        "stop_max_ns": max(s["interrupt_to_detach_ns"] for s in samples),
                        "request_median_ns": statistics.median(s["request_ns"] for s in samples)})
        return data
    print(f"{mode}: expected failure, target recovered after {elapsed:.3f}s")
    if mode == "timeout":
        assert 3 <= elapsed < 10, elapsed
        assert "only target traced; reporter and heartbeat progressed" in result.stdout
    summary.append({"mode": mode, "elapsed_s": elapsed, "expected_failure": True})


def check_symbols(data, function):
    # Symbolize after the target exits, using the saved module load bias. This
    # verifies that short pauses still yield useful stacks, rather than only PCs.
    image = next(i for i in data["images"] if Path(i["file"]) == binary)
    def lookup(frame):
        return frame["pc"] - int(frame["return_address"]) - image["load_bias"]

    addresses = sorted({lookup(f) for s in data["samples"] for f in s["frames"]
                        if image["code_start"] <= f["pc"] < image["code_end"]})
    assert addresses
    symbols = subprocess.check_output(["addr2line", "-a", "-C", "-f", "-i", "-e", str(binary)] +
                                      [hex(a) for a in addresses], text=True)
    (directory / f"symbols-{function}.txt").write_text(symbols)
    names = {}
    for line in symbols.splitlines():
        if re.fullmatch(r"0x[0-9a-fA-F]+", line):
            current = int(line, 16)
            names[current] = ""
        else:
            names[current] += line + "\n"
    for sample in data["samples"]:
        assert any(function in names.get(lookup(f), "") for f in sample["frames"])
    print(f"all 20 snapshots resolve {function} after target exit")


check_symbols(run("normal", True), "busy_worker")
check_symbols(run("blocked", True), "blocked_worker")

run("hold", True)
run("short-stack", True)
run("read-error", False)
run("timeout", False)
yama = Path("/proc/sys/kernel/yama/ptrace_scope")
caps = int(next(line.split()[1] for line in Path("/proc/self/status").read_text().splitlines()
                if line.startswith("CapEff:")), 16)
if yama.exists() and yama.read_text().strip() == "1" and not caps & (1 << 19):
    run("no-permission", False)
else:
    print("no-permission: not applicable to host ptrace policy/capabilities")
(directory / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
print("thread snapshot checks passed")
