use std::{
    cell::OnceCell,
    hint::spin_loop,
    sync::{atomic::AtomicUsize, mpsc, Arc, Mutex, OnceLock, Weak},
    time::Duration,
};

use nix::libc::{pthread_kill, pthread_self, pthread_t, SIGINFO};
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
    workers: Mutex<Vec<WorkerState>>,
    metrics: RuntimeMetrics,
}

impl State {
    pub fn new(f: impl for<'a> FnOnce(&'a mut Builder)) -> (Arc<State>, Runtime) {
        let worker_idx = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::sync_channel(1);

        let mut rt = None;
        let state = Arc::<State>::new_cyclic(|state| {
            let state = state.clone();
            let state2 = state.clone();

            let mut builder = tokio::runtime::Builder::new_multi_thread();

            f(&mut builder);

            let rt1 = builder
                .on_thread_start(move || {
                    let id = worker_idx.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    WORKER_THREAD_IDX.with(|c| c.set(id).unwrap());
                    let _ = tx.send(WorkerState {
                        thread: unsafe { pthread_self() },
                        parked: true,
                        poll_count: 0,
                        blocked: false,
                    });
                })
                .on_thread_park(move || State::park(&state))
                .on_thread_unpark(move || State::unpark(&state2))
                .build()
                .unwrap();

            let metrics = rt1.metrics();
            let workers: Vec<WorkerState> = std::iter::from_fn(|| rx.recv().ok())
                .take(metrics.num_workers())
                .collect();

            rt = Some(rt1);

            State {
                workers: Mutex::new(workers),
                metrics,
            }
        });

        let rt = rt.unwrap();
        (state, rt)
    }

    fn get_worker(state: &Weak<Self>, f: impl for<'a> FnOnce(&'a mut WorkerState)) {
        let Some(id) = WORKER_THREAD_IDX.with(|c| c.get().copied()) else {
            return;
        };
        let Some(state) = state.upgrade() else { return };
        let mut workers = state.workers.lock().unwrap();
        let Some(worker_state) = workers.get_mut(id) else {
            return;
        };
        f(worker_state)
    }

    fn park(state: &Weak<Self>) {
        Self::get_worker(state, |w| {
            w.parked = true;
            w.blocked = false;
        });
    }

    fn unpark(state: &Weak<Self>) {
        Self::get_worker(state, |w| {
            w.parked = false;
            w.blocked = false;
        });
    }

    fn supervisor(&self, interval: Duration, channel: &BtChan) {
        loop {
            std::thread::sleep(interval);
            let mut workers = self.workers.lock().unwrap().clone();

            for (i, worker_state) in workers.iter_mut().enumerate() {
                let new_count = self.metrics.worker_poll_count(i);

                // thread is blocked.
                if worker_state.parked {
                    if worker_state.blocked {
                        println!("worker thread {i} is parked")
                    }
                } else if worker_state.poll_count == new_count {
                    if !worker_state.blocked {
                        println!(
                            "worker thread {i} is blocked ({} == {new_count})",
                            worker_state.poll_count
                        );
                        request_backtrace(worker_state.thread, channel);
                    }
                    worker_state.blocked = true;
                } else {
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

            let mut workers2 = self.workers.lock().unwrap();
            std::iter::zip(&mut *workers2, workers).for_each(|(w2, w)| {
                w2.poll_count = w.poll_count;
                w2.blocked = w.blocked;
            });
        }
    }

    pub fn spawn_supervisor(self: Arc<Self>, interval: Duration) {
        let channel = register_backtrace();
        std::thread::spawn(move || {
            self.supervisor(interval, &channel);
        });
    }
}

#[derive(Clone, Copy)]
struct WorkerState {
    thread: pthread_t,
    poll_count: u64,
    parked: bool,
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
        state.spawn_supervisor(Duration::from_millis(50));

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
