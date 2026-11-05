"""Native Linux checks. Run after cargo build --example capture_gdb.

Arguments: example binary, GDB binary, an empty scratch directory.
Requires a GDB built with Python, GNU timeout, and util-linux prlimit.
"""

import os
from pathlib import Path
import stat
import subprocess
import sys
import time


binary, gdb, directory = sys.argv[1:]
binary, gdb = str(Path(binary).resolve()), str(Path(gdb).resolve())
directory = Path(directory).resolve()
directory.mkdir(parents=True, exist_ok=True)
env = dict(os.environ, TOKIO_SUPERVISOR_GDB=gdb, TMPDIR=str(directory))
resumed = "target resumed; blocked worker completed"
attached = "SUPERVISOR_COLLECTOR_ATTACHED"


def run(name, probe=None, *, missing=False):
    report = directory / f"{name}.txt"
    case_env = dict(env)
    if missing:
        case_env["TOKIO_SUPERVISOR_GDB"] = str(directory / "missing-gdb")
    start = time.monotonic()
    result = subprocess.run(
        [binary, str(report)] + ([probe] if probe else []),
        env=case_env, capture_output=True, text=True, timeout=15,
    )
    elapsed = time.monotonic() - start
    (directory / f"{name}.log").write_text(result.stdout + result.stderr)
    assert resumed in result.stdout, (name, result.stdout, result.stderr)
    assert stat.S_IMODE(report.stat().st_mode) == 0o600
    # GDB has a hard 256 KiB file limit. The outer timeout can append a short
    # diagnostic; allow 1 KiB for it without giving it GDB's output limit.
    assert report.stat().st_size <= 256 * 1024 + 1024
    text = report.read_text(errors="replace")
    print(f"{name}: exit={result.returncode}, {elapsed:.3f}s, {report.stat().st_size} bytes")
    return result, text, elapsed


result, text, _ = run("capture")
assert result.returncode == 0, result.stderr
assert attached in text and "capture_gdb::linux::blocked_worker" in text
assert "detached" in text
assert text.endswith("SUPERVISOR_COLLECTOR_COMPLETE\n")

result, text, elapsed = run("timeout", "--probe-timeout")
assert result.returncode != 0
assert attached in text
assert "signal: 9" in result.stdout, result.stdout
assert 3 <= elapsed < 10, elapsed

result, text, _ = run("output-limit", "--probe-output-limit")
assert result.returncode != 0
assert attached in text and len(text) >= 250 * 1024

result, text, _ = run("missing-tool", missing=True)
assert result.returncode != 0
assert "missing-gdb" in text

# Denial is only expected for an unprivileged caller under Yama mode 1.
# Other policies still run every capture/fault case above; no sysctl is changed.
yama = Path("/proc/sys/kernel/yama/ptrace_scope")
status = Path("/proc/self/status").read_text()
caps = int(next(line.split()[1] for line in status.splitlines()
                if line.startswith("CapEff:")), 16)
if yama.exists() and yama.read_text().strip() == "1" and not caps & (1 << 19):
    result, text, _ = run("permission-denied", "--probe-no-permission")
    assert result.returncode != 0
    assert attached not in text, text
    assert "SUPERVISOR_COLLECTOR_ATTACH_FAILED" in text, text
    print(text.strip())
else:
    print("permission-denied: not applicable to this ptrace policy/capability set")

print("external capture checks passed")
