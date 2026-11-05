use std::{
    error::Error,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    ops::Range,
    os::unix::{fs::FileExt, fs::OpenOptionsExt},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

use framehop::{
    x86_64::{CacheX86_64, UnwindRegsX86_64, UnwinderX86_64},
    ExplicitModuleSectionInfo, Module, Unwinder,
};
use nix::{
    sys::{
        ptrace,
        signal::{kill, Signal},
        wait::{waitpid, WaitPidFlag, WaitStatus},
    },
    unistd::{getpid, gettid, Pid},
};
use object::{Object, ObjectSection, ObjectSegment};
use rustix::process::{set_ptracer, PTracer};
use serde::{Deserialize, Serialize};
use tokio_supervisor::{Event, Supervisor};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const STACK_BYTES: usize = 64 * 1024;
const FRAME_LIMIT: usize = 128;

#[derive(Serialize, Deserialize)]
struct Frame {
    pc: u64,
    return_address: bool,
}

#[derive(Serialize, Deserialize)]
struct Sample {
    /// Conservative stop bound: immediately before INTERRUPT to after DETACH.
    interrupt_to_detach_ns: u64,
    request_ns: u64,
    stack_bytes: usize,
    /// Preserve PC vs return-address semantics for offline symbolizers.
    frames: Vec<Frame>,
    termination: String,
}

#[derive(Serialize, Deserialize)]
struct Image {
    file: String,
    load_bias: u64,
    code_start: u64,
    code_end: u64,
    build_id: Option<String>,
}

#[derive(Serialize)]
struct Report {
    version: u32,
    architecture: &'static str,
    tid: i32,
    collector_ready_ns: u64,
    images: Vec<Image>,
    samples: Vec<Sample>,
}

pub fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|s| s == "--collector") {
        if args.len() != 4 {
            return Err("invalid collector arguments".into());
        }
        return collector(args[1].parse()?, args[2].parse()?, &args[3]);
    }
    let report = args.first().ok_or(
        "usage: capture_thread REPORT [normal|blocked|hold|timeout|read-error|short-stack|no-permission]",
    )?;
    let mode = args.get(1).map(String::as_str).unwrap_or("normal");
    if args.len() > 2
        || ![
            "normal",
            "blocked",
            "hold",
            "timeout",
            "read-error",
            "short-stack",
            "no-permission",
        ]
        .contains(&mode)
    {
        return Err("unknown mode or extra argument".into());
    }
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(report)?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()?;
    let running = Arc::new(AtomicBool::new(true));
    let _stop = Stop(running.clone());
    let progress = Arc::new(AtomicU64::new(0));
    let heartbeat = Arc::new(AtomicU64::new(0));
    let beat = {
        let (running, heartbeat) = (running.clone(), heartbeat.clone());
        thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                heartbeat.fetch_add(1, Ordering::Relaxed);
                thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let (tx, rx) = mpsc::channel();
    let (release, wait) = mpsc::channel::<()>();
    let blocked = mode == "blocked";
    let task = {
        let (running, progress) = (running.clone(), progress.clone());
        rt.spawn(async move {
            tx.send((thread::current().id(), gettid().as_raw()))
                .unwrap();
            if blocked {
                blocked_worker(wait);
            } else {
                busy_worker(&running, &progress);
            }
        })
    };
    let (thread_id, tid) = rx.recv_timeout(Duration::from_secs(3))?;
    let (monitor, events) =
        Supervisor::new(rt.handle().clone()).spawn(Duration::from_millis(25))?;
    let result = (|| -> Result<()> {
        let event = events.recv_timeout(Duration::from_secs(3))?;
        if !matches!(event, Event::Stalled { thread_id: observed, .. } if observed == thread_id) {
            return Err("stall did not identify the registered task thread".into());
        }
        let spawning = Instant::now();
        let mut child = Client(
            Command::new("timeout")
                .args(["--kill-after=1s", "15s", "prlimit", "--core=0:0", "--"])
                .arg(std::env::current_exe()?)
                .args([
                    "--collector",
                    &std::process::id().to_string(),
                    &tid.to_string(),
                    mode,
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?,
        );
        if mode != "no-permission" {
            // This standalone example owns the designated-ptracer slot, just
            // like capture_gdb. Do not silently install this in a library.
            set_ptracer(PTracer::ProcessID(rustix::process::Pid::from_child(
                &child.0,
            )))?;
        }
        let result = (|| -> Result<()> {
            let mut input = child.0.stdin.take().unwrap();
            let mut replies = BufReader::new(child.0.stdout.take().unwrap());
            writeln!(input, "start")?;
            let mut line = String::new();
            replies.read_line(&mut line)?;
            if line.is_empty() {
                return Err("collector did not become ready".into());
            }
            let images = serde_json::from_str::<Vec<Image>>(&line)?;
            let collector_ready_ns = spawning.elapsed().as_nanos() as u64;
            let mut samples = Vec::new();
            for _ in 0..if mode == "hold" { 1 } else { 20 } {
                let requested = Instant::now();
                writeln!(input, "capture")?;
                line.clear();
                if replies.read_line(&mut line)? == 0 {
                    return Err("collector exited during capture".into());
                }
                if line.trim() == "stopped" {
                    // Fault probe only: the helper deliberately holds the selected
                    // thread, while this reporter and the heartbeat keep running.
                    let before = heartbeat.load(Ordering::Relaxed);
                    let worker = progress.load(Ordering::Relaxed);
                    for entry in fs::read_dir("/proc/self/task")? {
                        let entry = entry?;
                        let status = fs::read_to_string(entry.path().join("status"))?;
                        let tracer = status
                            .lines()
                            .find(|l| l.starts_with("TracerPid:"))
                            .unwrap();
                        assert_eq!(
                            !tracer.ends_with("\t0"),
                            entry.file_name() == tid.to_string().as_str()
                        );
                    }
                    thread::sleep(Duration::from_millis(20));
                    assert_eq!(progress.load(Ordering::Relaxed), worker);
                    assert!(heartbeat.load(Ordering::Relaxed) > before);
                    println!("only target traced; reporter and heartbeat progressed");
                    if mode == "hold" {
                        writeln!(input, "resume")?;
                    }
                    line.clear();
                    if replies.read_line(&mut line)? == 0 {
                        return Err("collector exited while target stopped".into());
                    }
                }
                let mut sample = serde_json::from_str::<Sample>(&line)?;
                sample.request_ns = requested.elapsed().as_nanos() as u64;
                samples.push(sample);
                let status = fs::read_to_string(format!("/proc/self/task/{tid}/status"))?;
                assert!(status.lines().any(|line| line == "TracerPid:\t0"));
            }
            drop(input);
            if !child.0.wait()?.success() {
                return Err("collector failed".into());
            }
            println!("completed {} captures with one collector", samples.len());
            serde_json::to_writer(
                output,
                &Report {
                    version: 1,
                    architecture: "x86_64",
                    tid,
                    collector_ready_ns,
                    images,
                    samples,
                },
            )?;
            Ok(())
        })();
        // Also reset on helper failure, before propagating that failure.
        set_ptracer(PTracer::None)?;
        result
    })();
    monitor.shutdown().map_err(|_| "watchdog panicked")?;
    running.store(false, Ordering::Relaxed);
    drop(release);
    rt.block_on(task)?;
    beat.join().map_err(|_| "heartbeat panicked")?;
    println!("target resumed; blocked worker completed");
    result
}

struct Stop(Arc<AtomicBool>);
impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}
struct Client(Child);
impl Drop for Client {
    fn drop(&mut self) {
        self.0.stdin.take();
        let _ = self.0.wait();
    }
}

#[inline(never)]
fn busy_worker(running: &AtomicBool, progress: &AtomicU64) {
    while running.load(Ordering::Relaxed) {
        progress.fetch_add(1, Ordering::Relaxed);
    }
}

#[inline(never)]
fn blocked_worker(wait: mpsc::Receiver<()>) {
    let _ = wait.recv();
}

/// Each request has a watchdog outside the application. If snapshotting gets
/// stuck, SIGKILL terminates the tracer and the kernel releases its tracee.
fn deadline(rx: mpsc::Receiver<bool>) {
    while rx.recv() == Ok(true) {
        if rx.recv_timeout(Duration::from_secs(3)) != Ok(false) {
            let _ = kill(getpid(), Signal::SIGKILL);
            std::process::exit(1);
        }
    }
}

fn collector(pid: u32, tid: i32, mode: &str) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || deadline(rx));
    let mut input = io::stdin().lock().lines();
    if input.next().transpose()?.as_deref() != Some("start") {
        return Err("missing permission gate".into());
    }
    let maps = mappings(pid)?;
    let (unwinder, images) = load_modules(&maps)?;
    let mut cache = CacheX86_64::new();
    let mut stack = vec![0; STACK_BYTES];
    println!("{}", serde_json::to_string(&images)?);
    io::stdout().flush()?;
    while let Some(request) = input.next() {
        if request? != "capture" {
            return Err("unknown collector request".into());
        }
        tx.send(true)?;
        let tid = Pid::from_raw(tid);
        // Validate membership immediately before seizing; this controlled task
        // stays alive until all requests finish. General TID reuse needs a lease.
        let _ = fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat"))?;
        let memory = File::open(format!("/proc/{pid}/mem"))?;
        ptrace::seize(tid, ptrace::Options::empty())?;
        let start = Instant::now();
        ptrace::interrupt(tid)?;
        match waitpid(tid, Some(WaitPidFlag::__WALL))? {
            WaitStatus::PtraceEvent(_, Signal::SIGTRAP, event)
                if event == nix::libc::PTRACE_EVENT_STOP => {}
            WaitStatus::Stopped(_, signal) => {
                // A signal-delivery stop raced INTERRUPT. Deliver that signal
                // when detaching and fail this capture instead of suppressing it.
                ptrace::detach(tid, signal)?;
                return Err(format!("signal interrupted capture: {signal}").into());
            }
            // Do not swallow arbitrary application signals or job-control stops.
            // On any unexpected state, leave this helper: tracer death detaches.
            state => return Err(format!("unexpected tracee stop: {state:?}").into()),
        }
        if mode == "hold" || mode == "timeout" {
            println!("stopped");
            io::stdout().flush()?;
            if mode == "hold" {
                if input.next().transpose()?.as_deref() != Some("resume") {
                    return Err("missing probe resume acknowledgement".into());
                }
            } else {
                thread::sleep(Duration::from_secs(60));
            }
        }
        let regs = ptrace::getregs(tid)?;
        let range = maps
            .iter()
            .find(|m| m.range.contains(&regs.rsp))
            .ok_or("stack mapping missing")?;
        let base = regs.rsp.saturating_sub(128).max(range.range.start);
        let budget = if mode == "short-stack" {
            8
        } else {
            STACK_BYTES
        };
        let count = budget.min((range.range.end - base) as usize);
        // The error probe occurs after stop, so kernel detach-on-exit is tested.
        let address = if mode == "read-error" { u64::MAX } else { base };
        memory.read_exact_at(&mut stack[..count], address)?;
        ptrace::detach(tid, None)?;
        let elapsed = start.elapsed().as_nanos() as u64;
        // Everything below uses owned bytes. No remote memory is read or thread
        // kept stopped during unwinding, formatting, or waiting for a new request.
        let mut read_stack = |address: u64| -> std::result::Result<u64, ()> {
            let offset = usize::try_from(address.checked_sub(base).ok_or(())?).map_err(|_| ())?;
            let bytes = stack[..count]
                .get(offset..offset.checked_add(8).ok_or(())?)
                .ok_or(())?;
            Ok(u64::from_ne_bytes(bytes.try_into().unwrap()))
        };
        let mut iter = unwinder.iter_frames(
            regs.rip,
            UnwindRegsX86_64::new(regs.rip, regs.rsp, regs.rbp),
            &mut cache,
            &mut read_stack,
        );
        let mut frames = Vec::new();
        let termination = loop {
            if frames.len() == FRAME_LIMIT {
                break "frame limit".into();
            }
            match iter.next() {
                Ok(Some(frame)) => frames.push(Frame {
                    pc: frame.address(),
                    return_address: frame.is_return_address(),
                }),
                Ok(None) => break "complete".into(),
                Err(error) => break format!("unwind stopped: {error}"),
            }
        };
        tx.send(false)?;
        let sample = Sample {
            interrupt_to_detach_ns: elapsed,
            request_ns: 0,
            stack_bytes: count,
            frames,
            termination,
        };
        println!("{}", serde_json::to_string(&sample)?);
        io::stdout().flush()?;
    }
    Ok(())
}

struct Mapping {
    range: Range<u64>,
    offset: u64,
    path: String,
    executable: bool,
}
fn mappings(pid: u32) -> Result<Vec<Mapping>> {
    fs::read_to_string(format!("/proc/{pid}/maps"))?
        .lines()
        .map(|mut line| {
            let mut fields = Vec::new();
            for _ in 0..5 {
                let (field, rest) = line
                    .trim_start()
                    .split_once(char::is_whitespace)
                    .unwrap_or((line, ""));
                fields.push(field);
                line = rest;
            }
            let (start, end) = fields[0].split_once('-').ok_or("invalid mapping")?;
            Ok(Mapping {
                range: u64::from_str_radix(start, 16)?..u64::from_str_radix(end, 16)?,
                offset: u64::from_str_radix(fields[2], 16)?,
                path: line.trim_start().to_string(),
                executable: fields[1].contains('x'),
            })
        })
        .collect()
}

fn load_modules(maps: &[Mapping]) -> Result<(UnwinderX86_64<Vec<u8>>, Vec<Image>)> {
    let mut unwinder = UnwinderX86_64::new();
    let mut images = Vec::new();
    for map in maps
        .iter()
        .filter(|m| m.executable && m.path.starts_with('/'))
    {
        let data = fs::read(&map.path)?;
        let file = object::File::parse(&*data)?;
        let mut executable = file.segments().filter(|segment| {
            matches!(segment.flags(), object::SegmentFlags::Elf { p_flags } if p_flags & object::elf::PF_X != 0)
        });
        let segment = executable.next().ok_or("executable ELF segment missing")?;
        if executable.next().is_some() {
            return Err(
                "multiple executable ELF segments are not supported by this experiment".into(),
            );
        }
        let bias = map
            .range
            .start
            .checked_sub(map.offset)
            .and_then(|a| a.checked_add(segment.file_range().0))
            .and_then(|a| a.checked_sub(segment.address()))
            .ok_or("invalid load bias")?;
        images.push(Image {
            file: map.path.clone(),
            load_bias: bias,
            code_start: map.range.start,
            code_end: map.range.end,
            build_id: file
                .build_id()?
                .map(|bytes| bytes.iter().map(|b| format!("{b:02x}")).collect()),
        });
        let bytes = |name| {
            file.section_by_name(name)
                .and_then(|s| s.data().ok())
                .map(Vec::from)
        };
        let range = |name| {
            file.section_by_name(name)
                .map(|s| s.address()..s.address() + s.size())
        };
        unwinder.add_module(Module::new(
            map.path.clone(),
            map.range.clone(),
            bias,
            ExplicitModuleSectionInfo {
                base_svma: 0,
                text: bytes(".text"),
                text_svma: range(".text"),
                eh_frame: bytes(".eh_frame"),
                eh_frame_svma: range(".eh_frame"),
                eh_frame_hdr: bytes(".eh_frame_hdr"),
                eh_frame_hdr_svma: range(".eh_frame_hdr"),
                got_svma: range(".got"),
                ..Default::default()
            },
        ));
    }
    Ok((unwinder, images))
}
