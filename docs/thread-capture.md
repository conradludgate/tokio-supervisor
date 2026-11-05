# Repeated snapshots across runtimes

`capture_thread` is a Linux x86_64 experiment for reducing capture pauses. The
GDB example remains a useful whole-process reference. Neither example changes
the detector or exposes a library capture API.

```sh
mkdir -p .scratch/thread-capture
cargo run --example capture_thread -- .scratch/thread-capture/report.json
# One helper, three runtimes, including runtime creation and shutdown:
cargo run --example capture_thread -- .scratch/thread-capture/multi.json multi
```

Requires GNU `timeout` and util-linux `prlimit`; GDB is not needed. The JSON
report must not exist. It is created with owner-only permissions. New dependencies
(`framehop`, `nix`, `object`, serde) are dev-only and target-specific.

The example deliberately keeps a Tokio task inside a CPU-bound poll (or a
channel/futex wait with the optional `blocked` argument). That task
reports its Rust thread ID and native Linux TID before blocking. The watchdog's
stall event must match its Rust ID. The task stays alive until collection ends;
this provides controlled target identity without treating a Rust ThreadId as a
native TID or introducing a general thread-registration API.

The `multi` mode routes 80 requests across three runtimes through **one** helper
and one ptracer permission grant. Runtime and worker labels travel with each
request and reply; worker indices are local to a runtime. A second runtime starts
after the helper is ready. The first runtime then shuts down, its stale target is
rejected, and a third starts without restarting the collector. A deliberately
incorrect thread start time is also rejected without stopping that worker.

## Capture sequence

A single external helper loads ELF unwind sections once, allocates a 64 KiB
stack buffer and a framehop cache, then accepts sequential requests. Each
request:

1. Checks `/proc/PID/task/TID/stat` start time against the registered target,
   refreshes stack mappings, and opens the target's memory. This happens before
   stopping a thread, so stacks allocated by new runtimes are included.
2. Uses `PTRACE_SEIZE` and `PTRACE_INTERRUPT` on **only that TID**. No clone/exit
   tracing or `PTRACE_O_EXITKILL` is enabled.
3. Rechecks thread start time while stopped, reads registers and copies at most
   64 KiB around/above the stack pointer,
   including the x86_64 red zone when it is mapped. A read may return a shorter
   readable prefix even within a listed mapping. Only those copied bytes are
   available to the unwinder; missing older frames produce an incomplete trace.
4. Detaches immediately. No thread remains traced between requests.
5. Unwinds using the copied bytes, then serializes the resulting frame addresses.

Unwinding, allocation of result vectors and reporting happen after detach in the
helper. It does not run an unwinder or signal handler inside the application.
Other application threads continue running, though a lock held by the selected
thread can still indirectly delay them during its stop.

`interrupt_to_detach_ns` measures from just before INTERRUPT to after DETACH:
a conservative bound around the selected thread's pause, including scheduling
and wait overhead. `request_ns` additionally includes request/reply transport
and unwinding. `collector_ready_ns` covers process startup and module loading.
These are different measurements; comparing a stop interval with the GDB
example's total collection time would overstate a speedup.

The report includes ELF file paths/load biases/executable ranges and optional
build IDs for later symbolization. Each frame preserves its raw address and
framehop's instruction-pointer/return-address distinction. Offline lookup uses
the previous byte for return addresses, so lookup stays within the call site.
Version 2 labels each sample with its runtime, worker, native TID and thread start
time, plus the collector PID. Rejected targets are listed separately; there is
no longer one top-level TID. This is still an experimental example format. `termination`
distinguishes a complete walk, frame limit and unwind failure. The 64 KiB stack
budget can omit older frames. No full core or raw stack-memory dump is written.

## Deadlines and failure behavior

The helper has an independent three-second watchdog per request. If capture
hangs or an error exits the active request, that helper dies and the kernel
detaches its tracee. A signal-delivery stop racing INTERRUPT is explicitly
detached with that signal; unsupported stop states fail collection. The example
also wraps the complete helper session in a 15-second timeout with a one-second
KILL escalation and disables helper core dumps. These are operational bounds,
not hard real-time guarantees for all kernel states.

The example explicitly owns the process's designated Yama ptracer slot for this
session, then clears it. This retains the GDB example's crash-reporter coexistence
limitation. It must not be installed silently by a library. The helper is idle
and unattached between requests, but still has its permission grant until exit.

This demo owns each selected task until its outstanding capture completes.
Start time is measured in clock ticks: checking it catches observed stale or
mismatched identities, but is not a unique lifetime token or a general solution
to TID reuse. A target exiting between the initial check and ptrace can still
fail the helper; automatic recovery from that race is not implemented. Requests
are sequential, and the helper is process-wide rather than owned by a runtime.

Loaded modules remain fixed even though stack mappings refresh. A general
collector needs a thread-lifetime/reuse protocol, module refresh/identity checks
for dlopen/dlclose or replaced files, and crash-reporter permission coordination.
Missing native unwind information, signal stacks, alternate stacks, JIT code
and arbitrary job-control states require further coverage. Images with multiple
executable ELF segments are rejected rather than guessed. Failed helpers are
not automatically restarted here. No macOS/Windows or other architecture claim.

## Native checks

```sh
cargo build --example capture_thread --locked
python3 scripts/test-thread-capture.py \
  target/debug/examples/capture_thread .scratch/thread-checks
```

GNU `addr2line` is needed for the checks. They resolve CPU-bound and futex-blocked
workers from 20 saved traces each **after the target exits**. A handshake deliberately holds
the selected thread: `/proc` confirms only it has a tracer, its progress counter
stops, and both the reporter and an unrelated heartbeat continue. Checks
also cover 80 captures across runtime lifetimes and a `multi-hold` handshake:
the other runtime continues progressing, and runtime A can shut down while B's
worker stays held. All four multi-runtime held captures check selective tracing.
Error probes exercise an insufficient stack-copy budget, memory-read failure after stop,
watchdog termination while stopped, and permission denial where applicable.
An insufficient snapshot must report an incomplete unwind. Every case requires the target to
resume and finish its task, within a separate parent-process deadline.

Useful reference contracts: [Linux ptrace](https://man7.org/linux/man-pages/man2/ptrace.2.html),
[framehop](https://github.com/mstange/framehop),
[Linux thread start time](https://man7.org/linux/man-pages/man5/proc_pid_stat.5.html),
[Yama permissions](https://www.kernel.org/doc/html/latest/admin-guide/LSM/Yama.html).
