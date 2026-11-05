# tokio-supervisor

An experimental watchdog for Tokio runtime workers. It reports an active worker
that has shown no poll progress for a sustained interval, followed by a recovery
event when progress, parking, or a thread handoff clears the observation.

Sampling uses an independent OS thread, so it continues when every monitored
worker is blocked. Events go to a bounded channel; a slow reporter cannot block
sampling. This version does not install signal handlers or capture stack traces.

## Use

```rust
use std::time::Duration;
use tokio_supervisor::Supervisor;

let runtime = tokio::runtime::Builder::new_multi_thread()
    .worker_threads(2)
    .build()?;
let (monitor, events) = Supervisor::new(runtime.handle().clone())
    .spawn(Duration::from_millis(100))?;

// Reporting also runs outside the monitored runtime, so a complete worker stall
// cannot prevent it from printing events.
let reporter = std::thread::spawn(move || {
    while let Ok(event) = events.recv() {
        eprintln!("{event:?}");
    }
});

runtime.block_on(async { /* application work */ });
monitor.shutdown().expect("watchdog thread panicked");
reporter.join().expect("reporter thread panicked");
# Ok::<(), std::io::Error>(())
```

Keep the returned `Monitor` alive. Dropping it stops and joins the watchdog;
`shutdown` does the same and returns any watchdog thread panic. Shutdown wakes
the watchdog without waiting for its sampling interval or application tasks.

Events use the standard-library synchronous receiver. Consume them on an
independent reporting thread; blocking a Tokio worker on this receiver can itself
stall the runtime. A bounded standard-library channel avoids running application
wake callbacks on the watchdog thread. Applications can bridge events to their
async reporting system outside the watchdog.

`interval` controls both the sampling period and the minimum observed no-progress
time. Only state transitions are emitted. The queue holds two events per worker;
when full, new events are dropped and counted by `Monitor::dropped_events`.
Recovery can therefore arrive without its earlier stall event. Receiver
disconnection is noticed when the watchdog next publishes an event. Dropping the
monitor or calling shutdown wakes it immediately, even when workers are idle.

## What an event means

This detects lack of observed worker progress, not proof of deadlock. Long
CPU-bound polls and OS descheduling can also trigger events. Poll counts can be
published in batches and the sampled metrics are not one atomic snapshot.

`Event::Stalled` contains the logical worker index, its sampled Rust thread ID,
and time since progress was last observed. The duration is not an exact task-poll
duration. Idle/parked workers and workers that have not started are skipped.
Changing worker thread identity resets the observation period.

`Event::Recovered` refers to the thread ID of the earlier stall. A worker can
recover by parking or handing its scheduler state to a replacement, such as with
`block_in_place`; this does not prove that the original application task completed.

No per-task callbacks are installed. This version does not identify which task
is running, unwind an interrupted stack, or add replacement runtime workers.
Stack collection needs a separate backend with its own lifetime and safety contract.

The Linux [external GDB capture experiment](docs/external-capture.md) collects a
one-shot process snapshot after a stall event. It pauses the whole application
and is an opt-in example, with an external deadline and bounded collector output.

The Linux x86_64 [single-thread snapshot experiment](docs/thread-capture.md)
reuses an external collector, stops only the selected thread, and resumes it
before unwinding a bounded copy of its stack. It records separate stop, request
and collector-startup timings. It remains an opt-in example with controlled
target lifetime and ptrace ownership.

## Build requirements

Tokio 1.54.1 provides stable park/unpark metrics, but
[`worker_poll_count`](https://docs.rs/tokio/1.54.1/tokio/runtime/struct.RuntimeMetrics.html#method.worker_poll_count)
and [`worker_thread_id`](https://docs.rs/tokio/1.54.1/tokio/runtime/struct.RuntimeMetrics.html#method.worker_thread_id)
still require `tokio_unstable`. The detector uses both to
observe progress and thread handoffs, so the cfg is still needed. This repository
enables it in `.cargo/config.toml`. Applications must enable it for their build
too, including Tokio itself:

```sh
RUSTFLAGS="--cfg tokio_unstable" cargo check
```

Cargo does not apply a dependency's `.cargo/config.toml` to its consumer. Append
the cfg if your application already sets `RUSTFLAGS`.

The checked baseline is Linux x86_64, Rust 1.99.1 and the retained lockfile with
Tokio 1.54.1 (which requires Rust 1.85). Only the standard multi-thread runtime
is supported. Zero intervals and current-thread runtimes are rejected before
creating a watchdog thread. Other platform builds remain unvalidated.

```sh
cargo test --all-targets --locked
cargo test --doc --locked
```

The HTTP and deliberately deadlocking Turso examples are manual demonstrations:

```sh
cargo run --example http
cargo run --example turso
```

The HTTP demo listens on `127.0.0.1:8123`. `/slow` blocks a worker; repeated
concurrent requests can occupy both. `/block_in_place` demonstrates handing the
logical worker to a replacement. These servers are not automated tests.

## Migration from the signal-capture prototype

`Supervisor::new(handle)` remains, but `spawn(interval)` now returns a
`Result<(Monitor, Receiver<Event>)>`. Keep the monitor and consume events instead
of relying on automatic stack printing. The old native-thread registration hooks
and `Backtrace`/`BacktraceSymbol` types are removed. No process-global signal or
native unwinder is used. This is an intentional breaking change to the unpublished
prototype; stack capture will be considered separately.
Applications needing Tokio timers or channels must enable the corresponding
Tokio features themselves; the detector requests only runtime features.
