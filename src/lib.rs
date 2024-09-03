use std::{
    cell::OnceCell,
    hint::spin_loop,
    sync::{Arc, OnceLock},
    time::Duration,
};

use nix::libc::{pthread_kill, SIGINFO};
use signal_hook::low_level::channel::Channel;
use tokio::runtime::{Builder, Runtime, RuntimeMetrics};

const FRAMES: usize = 8;

type BtChan = Channel<arrayvec::ArrayVec<backtrace::Frame, FRAMES>>;
static BACKTRACE_HOOK: OnceLock<Arc<BtChan>> = OnceLock::new();

fn register_backtrace() -> Arc<BtChan> {
    BACKTRACE_HOOK
        .get_or_init(|| {
            let channel = Arc::new(Channel::new());
            let channel2 = channel.clone();

            unsafe {
                signal_hook::low_level::register(SIGINFO, move || fulfill_backtrace(&channel2))
                    .unwrap()
            };

            channel
        })
        .clone()
}

fn request_backtrace(thread: usize, channel: &BtChan) {
    unsafe { pthread_kill(thread, SIGINFO) };

    loop {
        let Some(frames) = channel.recv() else {
            spin_loop();
            continue;
        };

        for frame in frames {
            backtrace::resolve_frame(&frame, |symbol| {
                if let Some(name) = symbol.name() {
                    println!("{name}");
                    if let Some(file) = symbol.filename() {
                        println!(" -> {file:?}");
                    }
                }
            });
        }
        break;
    }
}

fn fulfill_backtrace(channel: &BtChan) {
    let mut frames = arrayvec::ArrayVec::new();
    unsafe {
        backtrace::trace_unsynchronized(|f| frames.try_push(f.clone()).map_or(false, |_| true))
    }
    channel.send(frames);
}

pub struct State {
    workers: Vec<WorkerState>,
    metrics: RuntimeMetrics,
}

impl State {
    pub fn new(f: impl for<'a> FnOnce(&'a mut Builder)) -> (State, Runtime) {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        f(&mut builder);
        let rt = builder.build().unwrap();

        let metrics = rt.metrics();
        let workers: Vec<WorkerState> = std::iter::repeat(WorkerState {
            poll_count: 0,
            blocked: false,
        })
        .take(metrics.num_workers())
        .collect();

        let state = State { workers, metrics };

        (state, rt)
    }

    fn supervisor(&mut self, interval: Duration, channel: &BtChan) {
        loop {
            std::thread::sleep(interval);

            for (i, worker_state) in self.workers.iter_mut().enumerate() {
                let new_count = self.metrics.worker_poll_count(i);

                // An odd count means that the worker is currently parked. An even count means that the worker is currently active.
                let unpark_count = self.metrics.worker_park_unpark_count(i);

                if unpark_count % 2 == 1 {
                    // we are parked. mark as unblocked.
                    if worker_state.blocked {
                        println!("worker thread {i} is parked")
                    }
                    worker_state.blocked = false;
                } else if worker_state.poll_count == new_count {
                    // no poll progress was made since last loop.
                    if !worker_state.blocked {
                        // we were not blocked before. get the backtrace from the registered thread.
                        println!(
                            "worker thread {i} is blocked ({} == {new_count})",
                            worker_state.poll_count
                        );
                        if let Some(thread) = self.metrics.worker_pthread_id(i) {
                            request_backtrace(thread, channel);
                        }
                    }
                    worker_state.blocked = true;
                } else {
                    // progress was made since last loop. mark as unblocked.
                    if worker_state.blocked {
                        println!(
                            "worker thread {i} is not blocked ({} < {new_count})",
                            worker_state.poll_count
                        )
                    }
                    worker_state.blocked = false;
                }

                worker_state.poll_count = new_count;
            }
        }
    }

    pub fn spawn_supervisor(mut self, interval: Duration) {
        let channel = register_backtrace();
        std::thread::spawn(move || {
            self.supervisor(interval, &channel);
        });
    }
}

#[derive(Clone, Copy)]
struct WorkerState {
    poll_count: u64,
    blocked: bool,
}

thread_local! {
    static WORKER_THREAD_IDX: OnceCell<usize>  = const { OnceCell::new()};
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::routing::get;
    use tokio::net::TcpListener;

    use crate::State;

    #[test]
    fn it_works() {
        let (state, rt) = State::new(|b| {
            b.worker_threads(2).enable_all();
        });
        state.spawn_supervisor(Duration::from_millis(100));

        rt.block_on(async move {
            let svc = axum::Router::new()
                .route("/fast", get(|| async {}))
                .route(
                    "/slow",
                    get(|| async { std::thread::sleep(Duration::from_secs(5)) }),
                )
                .with_state(())
                .into_make_service();
            axum::serve(TcpListener::bind("0.0.0.0:8123").await.unwrap(), svc)
                .await
                .unwrap()
        });
    }
}
