# tokio-supervisor

Work in progress

## What is this?

This is a library that hooks into your tokio runtime (using unstable features) to monitor your runtime
threads for suspicious tasks that are blocking the thread. It currently only works on Linux and MacOS.

In the future, it will also attempt to spawn some new worker threads to pick up the slack to try
and improve efficiency and unblock your application in the rare event that all your tasks end up blocking.

## How does it work

Using `RuntimeMetrics::worker_poll_count`, we can detect when a worker is no longer making progress. If the poll
count does not increase after some duration, it is flagged as blocking (it could also be parked). If the thread
is detected as blocking, we send a signal to that thread.

Additionally, we set up a global channel and signal handler on all threads to respond to this signal.
It handles the signal by capturing a backtrace and sending it over the channel. By capturing the backtrace,
it makes it much easier to debug what is causing the runtime worker to become blocked.

## How will is handle spawning new workers

Tokio has a mechanism built in to spawn a new temporary worker thread called `block_in_place`. Unfortunately
it will reclaim the worker state once the block_in_place method completes. We would need to move this logic
so that tokio only reclaims the worker state once the task unblocks itself.
