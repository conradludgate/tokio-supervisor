//! One-shot Linux process snapshot after detecting a stalled worker.
//! Run with: cargo run --example capture_gdb -- .scratch/capture-report.txt
//! Requires GDB with Python, GNU timeout, and util-linux prlimit.
//! See docs/external-capture.md.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the external GDB capture experiment requires Linux");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() -> std::io::Result<()> {
    linux::run()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        fs::OpenOptions,
        io::{self, Write},
        os::unix::fs::OpenOptionsExt,
        path::Path,
        process::{Child, Command, ExitStatus, Stdio},
        sync::mpsc,
        time::{Duration, Instant},
    };

    use rustix::process::{set_ptracer, PTracer, Pid};
    use tokio_supervisor::{Event, Supervisor};

    const OUTPUT_LIMIT: usize = 256 * 1024;
    const COMPLETE: &str = "SUPERVISOR_COLLECTOR_COMPLETE\n";

    #[derive(Clone, Copy)]
    enum Probe {
        Capture,
        Timeout,
        OutputLimit,
        NoPermission,
    }

    pub fn run() -> io::Result<()> {
        let mut args = std::env::args_os().skip(1);
        let report = args.next().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput,
            "usage: capture_gdb REPORT [--probe-timeout|--probe-output-limit|--probe-no-permission]"))?;
        let probe_arg = args.next();
        let probe = match probe_arg.as_deref() {
            None => Probe::Capture,
            Some(s) if s == "--probe-timeout" => Probe::Timeout,
            Some(s) if s == "--probe-output-limit" => Probe::OutputLimit,
            Some(s) if s == "--probe-no-permission" => Probe::NoPermission,
            Some(_) => return Err(io::Error::new(io::ErrorKind::InvalidInput, "unknown probe")),
        };
        if args.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many arguments",
            ));
        }

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()?;
        // A disconnected sender also releases this task on any early error.
        let (release, wait) = mpsc::channel();
        let task = rt.spawn(async move {
            blocked_worker(wait);
        });
        let (monitor, events) =
            Supervisor::new(rt.handle().clone()).spawn(Duration::from_millis(25))?;
        let event = events
            .recv_timeout(Duration::from_secs(3))
            .map_err(io::Error::other)?;
        if !matches!(event, Event::Stalled { .. }) {
            return Err(io::Error::other("expected a stalled worker"));
        }
        println!("{event:?}");

        let start = Instant::now();
        let capture = collect(Path::new(&report), probe);
        println!("collector returned after {:?}", start.elapsed());
        monitor
            .shutdown()
            .map_err(|_| io::Error::other("watchdog panicked"))?;
        drop(release);
        rt.block_on(task).map_err(io::Error::other)?;
        println!("target resumed; blocked worker completed");

        let status = capture?;
        println!("collector status: {status}");
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "collector failed: {status}; see report"
            )))
        }
    }

    #[inline(never)]
    fn blocked_worker(wait: mpsc::Receiver<()>) {
        let _ = wait.recv();
    }

    fn collect(report: &Path, probe: Probe) -> io::Result<ExitStatus> {
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(report)?;
        let gdb = std::env::var_os("TOKIO_SUPERVISOR_GDB").unwrap_or_else(|| "gdb".into());
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=1s", "3s", "prlimit", "--core=0:0"])
            .arg(format!("--fsize={OUTPUT_LIMIT}:{OUTPUT_LIMIT}"))
            .arg("--")
            .arg(gdb)
            .args([
                "--batch",
                "--nx",
                "--nh",
                "--quiet",
                "-iex",
                "set auto-load off",
                "-iex",
                "set debuginfod enabled off",
            ]);
        let capture = match probe {
            Probe::Timeout => "python import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)",
            Probe::OutputLimit => "python import sys; sys.stdout.write('X' * 1048576); sys.stdout.flush()",
            Probe::Capture | Probe::NoPermission => "thread apply all backtrace 32",
        };
        // GDB must include Python support. Wait for the parent's permission
        // grant before attach, and explicitly fail the batch on command errors:
        // later successful commands can otherwise hide earlier failures.
        let script = format!(
            r#"import gdb
try:
    input()
    gdb.execute('set pagination off')
    gdb.execute('set confirm off')
    gdb.execute('set print frame-arguments none')
    try:
        gdb.execute('attach {}')
    except gdb.error:
        gdb.write('SUPERVISOR_COLLECTOR_ATTACH_FAILED\n', gdb.STDERR)
        raise
    gdb.write('SUPERVISOR_COLLECTOR_ATTACHED\n')
    gdb.flush()
    gdb.execute({capture:?})
    gdb.execute('detach')
    gdb.write({COMPLETE:?})
    gdb.flush()
except Exception as error:
    try:
        gdb.write(str(error) + '\n', gdb.STDERR)
    finally:
        gdb.execute('quit 1')
"#,
            std::process::id()
        );
        command.args(["-ex", &format!("python exec({script:?})")]);
        let mut collector = Collector {
            child: command
                .env("DEBUGINFOD_URLS", "")
                .stdin(Stdio::piped())
                .stdout(output.try_clone()?)
                .stderr(output)
                .spawn()?,
            permission: false,
        };
        if !matches!(probe, Probe::NoPermission) {
            // Opt-in for this standalone example only. This changes the process's
            // single Yama ptracer slot, so it must not be installed silently in a
            // library or combined with a different crash handler's ownership.
            set_ptracer(PTracer::ProcessID(Pid::from_child(&collector.child)))?;
            collector.permission = true;
        }
        // Reports go to a file, not pipes that need the stopped target to drain.
        collector.child.stdin.take().unwrap().write_all(b"\n")?;
        let status = collector.child.wait()?;
        if collector.permission {
            set_ptracer(PTracer::None)?;
            collector.permission = false;
        }
        // Some GDB builds swallow write errors when their output reaches
        // RLIMIT_FSIZE. Require the final marker as well as a successful exit.
        if status.success() && !std::fs::read(report)?.ends_with(COMPLETE.as_bytes()) {
            return Err(io::Error::other("incomplete collector report; see report"));
        }
        Ok(status)
    }

    struct Collector {
        child: Child,
        permission: bool,
    }

    impl Drop for Collector {
        fn drop(&mut self) {
            // Close an incomplete script on setup errors. Do not kill the timeout
            // supervisor: it must stay alive to terminate an attached/hung GDB.
            self.child.stdin.take();
            let _ = self.child.wait();
            if self.permission {
                let _ = set_ptracer(PTracer::None);
            }
        }
    }
}
