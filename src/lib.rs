use core::fmt;
use std::{
    hint::spin_loop,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

use backtrace::{BacktraceFmt, BytesOrWideString, PrintFmt, SymbolName};
use libc::{pthread_kill, SIGPROF};
use signal_hook::low_level::channel::Channel;
use tokio::runtime::RuntimeMetrics;

const FRAMES: usize = 16;

type BtChan = Channel<arrayvec::ArrayVec<backtrace::Frame, FRAMES>>;
static BACKTRACE_HOOK: OnceLock<Arc<BtChan>> = OnceLock::new();

pub struct Backtrace {
    // Frames here are listed from top-to-bottom of the stack
    frames: Vec<BacktraceFrame>,
}

struct BacktraceFrame {
    // frame: backtrace::Frame,
    ip: *mut libc::c_void,
    symbols: Vec<BacktraceSymbol>,
}

pub struct BacktraceSymbol {
    name: Option<Vec<u8>>,
    filename: Option<PathBuf>,
    lineno: Option<u32>,
    colno: Option<u32>,
}

impl fmt::Debug for Backtrace {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let style = if fmt.alternate() {
            PrintFmt::Full
        } else {
            PrintFmt::Short
        };

        // When printing paths we try to strip the cwd if it exists, otherwise
        // we just print the path as-is. Note that we also only do this for the
        // short format, because if it's full we presumably want to print
        // everything.
        let cwd = std::env::current_dir();
        let mut print_path = move |fmt: &mut fmt::Formatter<'_>, path: BytesOrWideString<'_>| {
            let path = path.into_path_buf();
            if style == PrintFmt::Full {
                if let Ok(cwd) = &cwd {
                    if let Ok(suffix) = path.strip_prefix(cwd) {
                        return fmt::Display::fmt(&suffix.display(), fmt);
                    }
                }
            }
            fmt::Display::fmt(&path.display(), fmt)
        };

        let mut f = BacktraceFmt::new(fmt, style, &mut print_path);
        f.add_context()?;
        for frame in &self.frames {
            let mut fr = f.frame();
            for symbol in &frame.symbols {
                fr.print_raw_with_column(
                    frame.ip,
                    symbol.name.as_ref().map(|s| SymbolName::new(s)),
                    // TODO: this isn't great that we don't end up printing anything
                    // with non-utf8 filenames. Thankfully almost everything is utf8 so
                    // this shouldn't be too bad.
                    symbol
                        .filename
                        .as_ref()
                        .and_then(|p| Some(BytesOrWideString::Bytes(p.to_str()?.as_bytes()))),
                    symbol.lineno,
                    symbol.colno,
                )?;
            }
            if frame.symbols.is_empty() {
                fr.print_raw(frame.ip, None, None, None)?;
            }
        }
        f.finish()?;
        Ok(())
    }
}

fn register_backtrace() -> Arc<BtChan> {
    BACKTRACE_HOOK
        .get_or_init(|| {
            let channel = Arc::new(Channel::new());
            let channel2 = channel.clone();

            unsafe {
                signal_hook::low_level::register(SIGPROF, move || fulfill_backtrace(&channel2))
                    .unwrap()
            };

            channel
        })
        .clone()
}

fn request_backtrace(thread: usize, channel: &BtChan) {
    unsafe { pthread_kill(thread, SIGPROF) };

    loop {
        let Some(frames) = channel.recv() else {
            spin_loop();
            continue;
        };

        let mut bt = Backtrace { frames: vec![] };

        for frame in frames {
            let mut f = BacktraceFrame {
                ip: frame.ip(),
                symbols: vec![],
            };
            backtrace::resolve_frame(&frame, |symbol| {
                let symbol = BacktraceSymbol {
                    name: symbol.name().map(|s| s.as_bytes().to_vec()),
                    filename: symbol.filename().map(|p| p.to_path_buf()),
                    lineno: symbol.lineno(),
                    colno: symbol.colno(),
                };

                f.symbols.push(symbol);
            });
            bt.frames.push(f);
        }

        println!("{bt:?}");

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

pub struct Supervisor {
    workers: Vec<WorkerState>,
    metrics: RuntimeMetrics,
}

impl Supervisor {
    pub fn new(rt_handle: &tokio::runtime::Handle) -> Self {
        match rt_handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => {}
            tokio::runtime::RuntimeFlavor::MultiThreadAlt => {}
            tokio::runtime::RuntimeFlavor::CurrentThread => {
                panic!("does not work with current thread runtimes. only works with multithreaded runtimes")
            }
            _ => panic!("unknown runtime flavor. only works with multithreaded runtimes"),
        }

        let metrics = rt_handle.metrics();
        let workers: Vec<WorkerState> = std::iter::repeat(WorkerState {
            poll_count: 0,
            blocked: false,
        })
        .take(metrics.num_workers())
        .collect();

        Supervisor { workers, metrics }
    }

    fn sample(&mut self, channel: &BtChan) {
        for (i, worker_state) in self.workers.iter_mut().enumerate() {
            let new_count = self.metrics.worker_poll_count(i);

            // An odd count means that the worker is currently parked. An even count means that the worker is currently active.
            let unpark_count = self.metrics.worker_park_unpark_count(i);

            if unpark_count % 2 == 1 {
                // we are parked. mark as unblocked.
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
                worker_state.blocked = false;
            }

            worker_state.poll_count = new_count;
        }
    }

    fn run(&mut self, interval: Duration, channel: &BtChan) {
        loop {
            std::thread::sleep(interval);
            self.sample(channel);
        }
    }

    pub fn spawn(mut self, interval: Duration) {
        let channel = register_backtrace();
        std::thread::spawn(move || {
            self.run(interval, &channel);
        });
    }
}

#[derive(Clone, Copy)]
struct WorkerState {
    poll_count: u64,
    blocked: bool,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::routing::get;
    use tokio::{net::TcpListener, task::block_in_place};

    use crate::Supervisor;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn it_works() {
        Supervisor::new(&tokio::runtime::Handle::current()).spawn(Duration::from_millis(100));

        let svc = axum::Router::new()
            .route("/fast", get(|| async {}))
            .route(
                "/slow",
                get(|| async { std::thread::sleep(Duration::from_secs(5)) }),
            )
            .route(
                "/block_in_place",
                get(|| async { block_in_place(|| std::thread::sleep(Duration::from_secs(5))) }),
            )
            .with_state(())
            .into_make_service();
        axum::serve(TcpListener::bind("0.0.0.0:8123").await.unwrap(), svc)
            .await
            .unwrap()
    }
}
