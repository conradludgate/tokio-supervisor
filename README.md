# tokio-supervisor

Work in progress

## What is this?

This is a library that hooks into your tokio runtime (using unstable features) to monitor your runtime
threads for suspicious tasks that are blocking the thread. It currently only works on Linux and MacOS.

In the future, it will also attempt to spawn some new worker threads to pick up the slack to try
and improve efficiency and unblock your application in the rare event that all your tasks end up blocking.

## How does it work?

Using `RuntimeMetrics::worker_poll_count`, we can detect when a worker is no longer making progress. If the poll
count does not increase after some duration, it is flagged as blocking (it could also be parked, which we also test for). If the thread
is detected as blocking, we send a signal to that thread.

Additionally, we set up a global channel and signal handler on all threads to respond to this signal.
It handles the signal by capturing a backtrace and sending it over the channel. By capturing the backtrace,
it makes it much easier to debug what is causing the runtime worker to become blocked.

## How will it handle spawning new workers?

Tokio has a mechanism built in to spawn a new temporary worker thread called `block_in_place`. Unfortunately
it will reclaim the worker state once the block_in_place method completes. We would need to move this logic
so that tokio only reclaims the worker state once the task unblocks itself.

## Example

```rust
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    // Create a new supervisor and attach it to the current runtime.
    // Spawn the supervisor thread to sample all workers every 100ms.
    Supervisor::new(&tokio::runtime::Handle::current()).spawn(Duration::from_millis(100));

    // Run some workload in the runtime.
    let svc = axum::Router::new()
        .route("/fast", get(|| async {}))
        // will block the runtime when this API is hit.
        .route(
            "/slow",
            get(|| async { std::thread::sleep(Duration::from_secs(5)) }),
        )
        .with_state(())
        .into_make_service();
    axum::serve(TcpListener::bind("0.0.0.0:8123").await.unwrap(), svc)
        .await
        .unwrap()
}
```

Calling `http://localhost:8123/fast` shows nothing. Calling `http://localhost:8123/slow` prints

```
worker thread 1 is blocked (673097 == 673097)
backtrace::backtrace::libunwind::trace::h99d4c3ef5d4daf63
 -> "/Users/conrad/.cargo/registry/src/index.crates.io-6f17d22bba15001f/backtrace-0.3.73/src/backtrace/libunwind.rs"
backtrace::backtrace::trace_unsynchronized::h9b3887e61be83d18
 -> "/Users/conrad/.cargo/registry/src/index.crates.io-6f17d22bba15001f/backtrace-0.3.73/src/backtrace/mod.rs"
tokio_supervisor::tests::get_backtrace2::hedb14cb7a09bf091
 -> "/Users/conrad/Documents/code/tokio-supervisor/src/lib.rs"
signal_hook_registry::handler::hffda5a27bfe9fd9b
 -> "/Users/conrad/.cargo/registry/src/index.crates.io-6f17d22bba15001f/signal-hook-registry-1.4.2/src/lib.rs"
__simple_esappend
std::sys::pal::unix::thread::Thread::sleep::ha9be65893542756d
 -> "/rustc/3f5fd8dd41153bc5fdca9427e9e05be2c767ba23/library/std/src/sys/pal/unix/thread.rs"
std::thread::sleep::h68e6720733935c56
 -> "/rustc/3f5fd8dd41153bc5fdca9427e9e05be2c767ba23/library/std/src/thread/mod.rs"
tokio_supervisor::tests::it_works::{{closure}}::{{closure}}::{{closure}}::h7baafb1d89b51864
 -> "/Users/conrad/Documents/code/tokio-supervisor/src/lib.rs"
<F as axum::handler::Handler<((),),S>>::call::{{closure}}::hdab8fd20326fa8e7
 -> "/Users/conrad/.cargo/registry/src/index.crates.io-6f17d22bba15001f/axum-0.7.5/src/handler/mod.rs"
<core::pin::Pin<P> as core::future::future::Future>::poll::h236b098ffeea1b99
 -> "/rustc/3f5fd8dd41153bc5fdca9427e9e05be2c767ba23/library/core/src/future/future.rs"
<futures_util::future::future::map::Map<Fut,F> as core::future::future::Future>::poll::hfdd9947fda4e1606
 -> "/Users/conrad/.cargo/registry/src/index.crates.io-6f17d22bba15001f/futures-util-0.3.30/src/future/future/map.rs"
<futures_util::future::future::Map<Fut,F> as core::future::future::Future>::poll::h23cd2e63b5ba6313
 -> "/Users/conrad/.cargo/registry/src/index.crates.io-6f17d22bba15001f/futures-util-0.3.30/src/lib.rs"
<axum::handler::future::IntoServiceFuture<F> as core::future::future::Future>::poll::hf95ed4d49ff0ba63
 -> "/Users/conrad/.cargo/registry/src/index.crates.io-6f17d22bba15001f/axum-0.7.5/src/macros.rs"
<F as futures_core::future::TryFuture>::try_poll::h1ad9def16d43a26e
 -> "/Users/conrad/.cargo/registry/src/index.crates.io-6f17d22bba15001f/futures-core-0.3.30/src/future.rs"
<futures_util::future::try_future::into_future::IntoFuture<Fut> as core::future::future::Future>::poll::h76645dc11ea53258
 -> "/Users/conrad/.cargo/registry/src/index.crates.io-6f17d22bba15001f/futures-util-0.3.30/src/future/try_future/into_future.rs"
<futures_util::future::future::map::Map<Fut,F> as core::future::future::Future>::poll::hf796f107af9e2d56
 -> "/Users/conrad/.cargo/registry/src/index.crates.io-6f17d22bba15001f/futures-util-0.3.30/src/future/future/map.rs
```

There's a lot of noise here, but if we filter down the output, we do see our `std::thread::sleep` inside an axum handler

```
worker thread 1 is blocked (673097 == 673097)
...
std::thread::sleep::h68e6720733935c56
 -> "/rustc/3f5fd8dd41153bc5fdca9427e9e05be2c767ba23/library/std/src/thread/mod.rs"
tokio_supervisor::tests::it_works::{{closure}}::{{closure}}::{{closure}}::h7baafb1d89b51864
 -> "/Users/conrad/Documents/code/tokio-supervisor/src/lib.rs"
<F as axum::handler::Handler<((),),S>>::call::{{closure}}::hdab8fd20326fa8e7
 -> "/Users/conrad/.cargo/registry/src/index.crates.io-6f17d22bba15001f/axum-0.7.5/src/handler/mod.rs"
...
```
