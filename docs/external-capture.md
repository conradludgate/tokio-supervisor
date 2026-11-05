# External stack capture experiment

The watchdog can detect a stalled worker even when every worker is blocked,
but an event does not explain where it is stuck. `capture_gdb` tests collecting
that evidence in another process, without invoking an unwinder from a signal
handler in the application. The detector and library API remain unchanged.

This is a Linux-only, one-shot example using GDB with Python support, GNU
`timeout`, and util-linux `prlimit`. Use a debug build to retain useful frames:

```sh
mkdir -p .scratch/capture
cargo run --example capture_gdb -- .scratch/capture/report.txt
```

The report must not already exist. It is created with owner-only permissions
(subject to the umask). Set `TOKIO_SUPERVISOR_GDB` to an absolute GDB path if it
is not on PATH. The example deliberately blocks its single runtime worker,
waits for a stall event, takes a process snapshot and releases the worker.
Debugger errors return a failing exit status after releasing the worker.

## Why the helper is arranged this way

GDB stops **the whole application**, including the watchdog and the thread
waiting for the collector. An application timer cannot enforce the collector's
deadline while stopped. A separate `timeout` process sends TERM after three
seconds and KILL one second later if needed. This is an operational timeout,
not a real-time guarantee under arbitrary OS delays. Killing a debugger
normally detaches and resumes its tracees; this experiment tests that behavior
for its ordinary running process, not every job-control or kernel state.

GDB writes directly to a file. A pipe drained by the application could fill
while the application is stopped and prevent GDB from completing. `prlimit`
disables collector core dumps and gives GDB a hard 256 KiB file-size limit.
The outer timeout does not inherit this limit, so reaching it cannot prevent
timeout enforcement. Timeout diagnostics can append a small amount to the
report. GDB initialization files, automatic script loading and debuginfod
network lookups are disabled. The script never calls inferior functions.
Some GDB builds swallow output write errors, so a successful collector exit
also requires a final completion marker in the report. A truncated report
therefore fails even when GDB returns zero.

On Linux Yama mode 1, a child cannot normally trace its parent. This standalone
example grants `PR_SET_PTRACER` to the unreaped timeout child's PID; its GDB
descendant can then attach. A pipe gates attachment until the grant completes.
The grant is cleared after the collector exits, including setup failures.
Cleanup waits for the external timeout rather than killing the timeout process
and potentially abandoning a stopped application.

This grant replaces the process's **single** designated ptracer. It must not be
silently installed in a library or combined with another crash handler that
owns that setting. It also permits this helper and its descendants to debug
the process, so use trusted local tools. No system ptrace policy or capability
is changed. A stricter policy can reject collection.

## Checks and limits

Run the native checks in a fresh directory:

```sh
cargo build --example capture_gdb --locked
python3 scripts/test-external-capture.py \
  target/debug/examples/capture_gdb "$(command -v gdb)" .scratch/capture-checks
```

The checks require the blocked worker's symbol and a successful detach in a
real report. Fault cases exercise an attached collector ignoring TERM until
KILL, excessive output, a missing GDB executable, and permission denial when
the host policy makes it applicable. Every case requires the application to
resume and complete its blocked task within a separate parent-process deadline.
CI runs these on Linux; this is bounded native evidence, not a proof covering
all debugger, kernel or application states.

This captures up to 32 frames **per process thread**, not a particular task or
only the stalled worker. Rust `ThreadId` is not a native Linux TID. Mapping
workers to native threads, repeated captures, target exit/replacement races,
coexistence with other debuggers and a reusable capture interface remain future
work. There is no macOS/Windows backend or production overhead claim. Stopping
the application itself changes its latency and the watchdog's observations.

Reference contracts: [GDB attach/detach](https://www.sourceware.org/gdb/current/onlinedocs/gdb.html/Attach.html),
[Yama permissions](https://www.kernel.org/doc/html/latest/admin-guide/LSM/Yama.html),
[Linux ptrace tracer exit](https://man7.org/linux/man-pages/man2/ptrace.2.html).
