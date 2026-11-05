//! Detect sustained lack of progress on Tokio runtime workers.
//!
//! Sampling runs on its own OS thread. It continues when every monitored worker
//! is blocked and never waits for event consumers. This crate does not install
//! signal handlers or capture stacks.

use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, TrySendError},
        Arc,
    },
    thread::{self, JoinHandle, ThreadId},
    time::{Duration, Instant},
};

use tokio::runtime::{Handle, RuntimeFlavor, RuntimeMetrics};

/// A change in a logical runtime worker's observed progress.
///
/// These are diagnostic observations, not proof of a deadlock. A long CPU-bound
/// poll or OS descheduling can also cause a stall. Metrics are sampled separately
/// and Tokio can publish poll counts in batches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// An active worker has shown no poll progress for at least one interval.
    Stalled {
        worker: usize,
        thread_id: ThreadId,
        /// Time since progress was last observed, not an exact poll duration.
        duration: Duration,
    },
    /// A previously stalled worker made progress, parked, or changed identity.
    ///
    /// The thread ID identifies the earlier stall. A handoff can restore worker
    /// capacity without completing the task that originally occupied that thread.
    Recovered {
        worker: usize,
        thread_id: ThreadId,
        /// Time since progress was last observed before this stall.
        duration: Duration,
    },
}

/// Configures observation of one multi-thread Tokio runtime.
///
/// ```
/// use std::time::Duration;
/// use tokio_supervisor::Supervisor;
///
/// let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).build()?;
/// let (monitor, events) = Supervisor::new(runtime.handle().clone())
///     .spawn(Duration::from_millis(100))?;
/// // An independent reporter can receive events without using the runtime.
/// monitor.shutdown().expect("watchdog panicked");
/// assert!(events.recv().is_err());
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct Supervisor {
    metrics: RuntimeMetrics,
    flavor: RuntimeFlavor,
}

impl Supervisor {
    pub fn new(handle: Handle) -> Self {
        Self {
            metrics: handle.metrics(),
            flavor: handle.runtime_flavor(),
        }
    }

    /// Start an independent watchdog and return its lifecycle guard and events.
    ///
    /// `interval` is both the sampling period and the minimum observed
    /// no-progress time before reporting a stall. Detection can take an additional
    /// interval depending on sampling alignment, plus OS scheduling delays.
    /// Only transitions are reported, rather than repeating a stall every tick.
    ///
    /// The event queue holds two events per runtime worker. If it fills, new
    /// events are dropped and counted by [`Monitor::dropped_events`]; sampling
    /// never waits for a consumer. Recovery does not promise that the corresponding
    /// stall event was delivered. Receiver disconnection is noticed on the next
    /// event publication. Use the monitor to stop promptly even when workers are idle.
    ///
    /// Keep the monitor alive while observing the runtime. Receive events on a
    /// separate reporting thread using [`Receiver::recv`]. Do not block a runtime
    /// worker waiting on this synchronous receiver. Event delivery does not invoke
    /// application wakers or reporting callbacks on the watchdog thread.
    ///
    /// Returns an error for zero intervals, unsupported runtime flavors, or an
    /// OS thread creation failure. The supported flavor is `MultiThread`.
    pub fn spawn(self, interval: Duration) -> io::Result<(Monitor, Receiver<Event>)> {
        if interval.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sampling interval must be nonzero",
            ));
        }
        if !matches!(self.flavor, RuntimeFlavor::MultiThread) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "supervision requires a multi-thread runtime",
            ));
        }

        let (events, receiver) = mpsc::sync_channel(self.metrics.num_workers() * 2);
        let (shutdown, stopped) = mpsc::channel();
        let dropped = Arc::new(AtomicUsize::new(0));
        let lost = dropped.clone();
        let thread = thread::Builder::new()
            .name("tokio-supervisor".into())
            .spawn(move || {
                let now = Instant::now();
                let mut workers: Vec<_> = (0..self.metrics.num_workers())
                    .map(|i| Worker::new(Observation::read(&self.metrics, i), now))
                    .collect();

                while matches!(
                    stopped.recv_timeout(interval),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    let now = Instant::now();
                    for (i, worker) in workers.iter_mut().enumerate() {
                        let observation = Observation::read(&self.metrics, i);
                        if let Some(event) = worker.observe(i, observation, now, interval) {
                            match events.try_send(event) {
                                Ok(()) => {}
                                Err(TrySendError::Full(_)) => {
                                    lost.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(TrySendError::Disconnected(_)) => return,
                            }
                        }
                    }
                }
            })?;
        Ok((
            Monitor {
                shutdown: Some(shutdown),
                thread: Some(thread),
                dropped,
            },
            receiver,
        ))
    }
}

/// Owns the watchdog thread. Drop stops and joins it independently of Tokio.
///
/// Shutdown does not wait for application tasks, event consumption, or callbacks.
/// OS scheduling and the currently executing metrics sample can still delay it.
#[must_use = "dropping the monitor stops supervision"]
pub struct Monitor {
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    dropped: Arc<AtomicUsize>,
}

impl Monitor {
    /// Number of events dropped because the bounded event queue was full.
    pub fn dropped_events(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Stop and join the watchdog, returning any watchdog thread panic.
    pub fn shutdown(mut self) -> thread::Result<()> {
        self.stop()
    }

    fn stop(&mut self) -> thread::Result<()> {
        // Disconnecting wakes recv_timeout even when the interval is very long.
        self.shutdown.take();
        match self.thread.take() {
            Some(thread) => thread.join(),
            None => Ok(()),
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[derive(Clone, Copy)]
struct Observation {
    poll_count: u64,
    parked: bool,
    thread_id: Option<ThreadId>,
}

impl Observation {
    fn read(metrics: &RuntimeMetrics, worker: usize) -> Self {
        Self {
            poll_count: metrics.worker_poll_count(worker),
            parked: metrics.worker_park_unpark_count(worker) % 2 == 1,
            thread_id: metrics.worker_thread_id(worker),
        }
    }
}

struct Worker {
    previous: Observation,
    progress: Instant,
    stalled: bool,
}

impl Worker {
    fn new(previous: Observation, now: Instant) -> Self {
        Self {
            previous,
            progress: now,
            stalled: false,
        }
    }

    fn observe(
        &mut self,
        worker: usize,
        current: Observation,
        now: Instant,
        interval: Duration,
    ) -> Option<Event> {
        let duration = now.saturating_duration_since(self.progress);
        let progress = current.parked
            || self.previous.parked
            || current.thread_id.is_none()
            || current.thread_id != self.previous.thread_id
            || current.poll_count != self.previous.poll_count;
        let event = if progress {
            let event = if self.stalled {
                self.previous.thread_id.map(|thread_id| Event::Recovered {
                    worker,
                    thread_id,
                    duration,
                })
            } else {
                None
            };
            self.progress = now;
            self.stalled = false;
            event
        } else if !self.stalled && duration >= interval {
            current.thread_id.map(|thread_id| {
                self.stalled = true;
                Event::Stalled {
                    worker,
                    thread_id,
                    duration,
                }
            })
        } else {
            None
        };
        self.previous = current;
        event
    }
}

#[cfg(test)]
mod tests;
