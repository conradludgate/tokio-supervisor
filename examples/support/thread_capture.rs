use std::{
    error::Error,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    ops::Range,
    os::unix::{fs::FileExt, fs::OpenOptionsExt},
    process::{Child, ChildStdout, Command, Stdio},
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Target {
    runtime: usize,
    worker: usize,
    tid: i32,
    start_time: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Response {
    Captured(Sample),
    TargetUnavailable { target: Target },
}

#[derive(Serialize, Deserialize)]
struct Sample {
    target: Target,
    collector_pid: u32,
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
    rejected: Vec<Target>,
    collector_ready_ns: u64,
    images: Vec<Image>,
    samples: Vec<Sample>,
}

pub fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|s| s == "--collector") {
        if args.len() != 3 {
            return Err("invalid collector arguments".into());
        }
        return collector(args[1].parse()?, &args[2]);
    }
    let report = args.first().ok_or(
        "usage: capture_thread REPORT [normal|blocked|hold|timeout|read-error|short-stack|no-permission|multi|multi-hold]",
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
            "multi",
            "multi-hold",
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
    if mode == "multi" || mode == "multi-hold" {
        return multi_runtime(output, mode == "multi-hold");
    }
    let heartbeat = Heartbeat::new();
    let mut worker = Worker::new(0, mode == "blocked")?;
    let result = (|| -> Result<()> {
        let mut client = Client::start(mode)?;
        let mut samples = Vec::new();
        for _ in 0..if mode == "hold" { 1 } else { 20 } {
            samples.push(
                client
                    .capture(worker.target, || check_held(&worker, &heartbeat.progress))?
                    .ok_or("worker exited before capture")?,
            );
            assert_untraced(worker.target.tid)?;
        }
        client.finish()?;
        println!("completed {} captures with one collector", samples.len());
        serde_json::to_writer(
            output,
            &Report {
                version: 2,
                architecture: "x86_64",
                rejected: Vec::new(),
                collector_ready_ns: client.ready_ns,
                images: std::mem::take(&mut client.images),
                samples,
            },
        )?;
        Ok(())
    })();
    worker.stop()?;
    println!("target resumed; blocked worker completed");
    result
}

fn capture_worker(
    client: &mut Client,
    worker: &Worker,
    heartbeat: &AtomicU64,
    other: &Worker,
) -> Result<Sample> {
    let sample = client
        .capture(worker.target, || {
            let before = other.progress.load(Ordering::Relaxed);
            check_held(worker, heartbeat)?;
            assert!(other.progress.load(Ordering::Relaxed) > before);
            Ok(())
        })?
        .ok_or("live worker was rejected")?;
    assert_untraced(worker.target.tid)?;
    Ok(sample)
}

fn multi_runtime(output: File, hold: bool) -> Result<()> {
    let heartbeat = Heartbeat::new();
    let mut a = Some(Worker::new(0, false)?);
    let stale = a.as_ref().unwrap().target;
    let mut client = Client::start(if hold { "hold" } else { "normal" })?;
    // Deliberately create the other runtimes after the helper loaded modules.
    let mut b = Worker::new(1, false)?;
    let mut c = None;
    let mut samples = Vec::new();
    let mut rejected = Vec::new();
    let rounds = if hold { 1 } else { 20 };
    let result = (|| -> Result<()> {
        for round in 0..rounds {
            samples.push(capture_worker(
                &mut client,
                a.as_ref().unwrap(),
                &heartbeat.progress,
                &b,
            )?);
            samples.push(
                client
                    .capture(b.target, || {
                        let before = a.as_ref().unwrap().progress.load(Ordering::Relaxed);
                        check_held(&b, &heartbeat.progress)?;
                        assert!(a.as_ref().unwrap().progress.load(Ordering::Relaxed) > before);
                        if round + 1 == rounds {
                            let mut old = a.take().unwrap();
                            old.stop()?;
                            drop(old);
                            println!("runtime A shut down while runtime B remained held");
                        }
                        Ok(())
                    })?
                    .ok_or("live worker B was rejected")?,
            );
            assert_untraced(b.target.tid)?;
        }
        if let Some(mut old) = a.take() {
            old.stop()?;
        }
        assert!(client
            .capture(stale, || Err("exited target was stopped".into()))?
            .is_none());
        rejected.push(stale);
        let wrong_identity = Target {
            start_time: b.target.start_time.wrapping_add(1),
            ..b.target
        };
        assert!(client
            .capture(wrong_identity, || Err("wrong identity was stopped".into()))?
            .is_none());
        rejected.push(wrong_identity);
        println!("exited and mismatched targets rejected without restarting collector");
        c = Some(Worker::new(2, false)?);
        for _ in 0..rounds {
            samples.push(capture_worker(
                &mut client,
                c.as_ref().unwrap(),
                &heartbeat.progress,
                &b,
            )?);
            samples.push(capture_worker(
                &mut client,
                &b,
                &heartbeat.progress,
                c.as_ref().unwrap(),
            )?);
        }
        Ok(())
    })();
    let finished = client.finish();
    if let Some(mut old) = a.take() {
        old.stop()?;
    }
    b.stop()?;
    if let Some(mut last) = c.take() {
        last.stop()?;
    }
    println!("target resumed; blocked worker completed");
    result?;
    finished?;
    println!(
        "completed {} captures across three runtimes with one collector",
        samples.len()
    );
    serde_json::to_writer(
        output,
        &Report {
            version: 2,
            architecture: "x86_64",
            rejected,
            collector_ready_ns: client.ready_ns,
            images: std::mem::take(&mut client.images),
            samples,
        },
    )?;
    Ok(())
}

// The demo owns these tasks until capture completes. Runtime shutdown releases
// their flag/channel and joins them before dropping their runtime.
struct Worker {
    runtime: tokio::runtime::Runtime,
    monitor: Option<tokio_supervisor::Monitor>,
    task: Option<tokio::task::JoinHandle<()>>,
    running: Stop,
    release: Option<mpsc::Sender<()>>,
    progress: Arc<AtomicU64>,
    target: Target,
}

impl Worker {
    fn new(label: usize, blocked: bool) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()?;
        let running = Stop(Arc::new(AtomicBool::new(true)));
        let progress = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel();
        let (release, wait) = mpsc::channel::<()>();
        let task = {
            let (running, progress) = (running.0.clone(), progress.clone());
            runtime.spawn(async move {
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
            Supervisor::new(runtime.handle().clone()).spawn(Duration::from_millis(25))?;
        let target = Target {
            runtime: label,
            worker: 0,
            tid,
            start_time: thread_start(std::process::id(), tid)?
                .ok_or("worker exited at registration")?,
        };
        let worker = Self {
            runtime,
            monitor: Some(monitor),
            task: Some(task),
            running,
            release: Some(release),
            progress,
            target,
        };
        if !matches!(events.recv_timeout(Duration::from_secs(3))?, Event::Stalled { worker: 0, thread_id: observed, .. } if observed == thread_id)
        {
            return Err("stall did not identify the registered task thread".into());
        }
        Ok(worker)
    }

    fn stop(&mut self) -> Result<()> {
        self.running.0.store(false, Ordering::Relaxed);
        self.release.take();
        if let Some(monitor) = self.monitor.take() {
            monitor.shutdown().map_err(|_| "watchdog panicked")?;
        }
        if let Some(task) = self.task.take() {
            self.runtime.block_on(task)?;
        }
        Ok(())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct Stop(Arc<AtomicBool>);
impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}
struct Heartbeat {
    running: Stop,
    progress: Arc<AtomicU64>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Heartbeat {
    fn new() -> Self {
        let running = Stop(Arc::new(AtomicBool::new(true)));
        let progress = Arc::new(AtomicU64::new(0));
        let thread = {
            let (running, progress) = (running.0.clone(), progress.clone());
            thread::spawn(move || {
                while running.load(Ordering::Relaxed) {
                    progress.fetch_add(1, Ordering::Relaxed);
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };
        Self {
            running,
            progress,
            thread: Some(thread),
        }
    }
}
impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.running.0.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Client {
    child: Child,
    replies: BufReader<ChildStdout>,
    line: String,
    permission: bool,
    resume_probe: bool,
    images: Vec<Image>,
    ready_ns: u64,
}
impl Client {
    fn start(mode: &str) -> Result<Self> {
        let start = Instant::now();
        let mut child = Command::new("timeout")
            .args(["--kill-after=1s", "15s", "prlimit", "--core=0:0", "--"])
            .arg(std::env::current_exe()?)
            .args(["--collector", &std::process::id().to_string(), mode])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let replies = BufReader::new(child.stdout.take().unwrap());
        let mut client = Self {
            child,
            replies,
            line: String::new(),
            permission: false,
            resume_probe: mode == "hold",
            images: Vec::new(),
            ready_ns: 0,
        };
        if mode != "no-permission" {
            // One session owns the process's designated-ptracer slot.
            set_ptracer(PTracer::ProcessID(rustix::process::Pid::from_child(
                &client.child,
            )))?;
            client.permission = true;
        }
        writeln!(client.child.stdin.as_mut().unwrap(), "start")?;
        client.read()?;
        client.images = serde_json::from_str(&client.line)?;
        client.ready_ns = start.elapsed().as_nanos() as u64;
        Ok(client)
    }

    fn read(&mut self) -> Result<()> {
        self.line.clear();
        if self.replies.read_line(&mut self.line)? == 0 {
            return Err("collector exited during capture".into());
        }
        Ok(())
    }

    fn capture(
        &mut self,
        target: Target,
        held: impl FnOnce() -> Result<()>,
    ) -> Result<Option<Sample>> {
        let requested = Instant::now();
        writeln!(
            self.child.stdin.as_mut().unwrap(),
            "{}",
            serde_json::to_string(&target)?
        )?;
        self.read()?;
        if self.line.trim() == "stopped" {
            held()?;
            if self.resume_probe {
                writeln!(self.child.stdin.as_mut().unwrap(), "resume")?;
            }
            self.read()?;
        }
        match serde_json::from_str::<Response>(&self.line)? {
            Response::Captured(mut sample) => {
                if sample.target != target {
                    return Err("collector replied for a different target".into());
                }
                sample.request_ns = requested.elapsed().as_nanos() as u64;
                Ok(Some(sample))
            }
            Response::TargetUnavailable { target: reply } => {
                if reply != target {
                    return Err("collector rejected a different target".into());
                }
                Ok(None)
            }
        }
    }

    fn finish(&mut self) -> Result<()> {
        self.child.stdin.take();
        let status = self.child.wait()?;
        if self.permission {
            set_ptracer(PTracer::None)?;
            self.permission = false;
        }
        if !status.success() {
            return Err("collector failed".into());
        }
        Ok(())
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn assert_untraced(tid: i32) -> Result<()> {
    let status = fs::read_to_string(format!("/proc/self/task/{tid}/status"))?;
    assert!(status.lines().any(|line| line == "TracerPid:\t0"));
    Ok(())
}
fn check_held(worker: &Worker, heartbeat: &AtomicU64) -> Result<()> {
    let before = heartbeat.load(Ordering::Relaxed);
    let progress = worker.progress.load(Ordering::Relaxed);
    for entry in fs::read_dir("/proc/self/task")? {
        let entry = entry?;
        let status = fs::read_to_string(entry.path().join("status"))?;
        let tracer = status
            .lines()
            .find(|line| line.starts_with("TracerPid:"))
            .unwrap();
        assert_eq!(
            !tracer.ends_with("\t0"),
            entry.file_name() == worker.target.tid.to_string().as_str()
        );
    }
    thread::sleep(Duration::from_millis(20));
    assert_eq!(worker.progress.load(Ordering::Relaxed), progress);
    assert!(heartbeat.load(Ordering::Relaxed) > before);
    println!("only target traced; reporter and heartbeat progressed");
    Ok(())
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

fn collector(pid: u32, mode: &str) -> Result<()> {
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
        let target: Target = serde_json::from_str(&request?)?;
        if thread_start(pid, target.tid)? != Some(target.start_time) {
            reply(&Response::TargetUnavailable { target })?;
            continue;
        }
        tx.send(true)?;
        let tid = Pid::from_raw(target.tid);
        // Runtimes created after this collector may allocate new stack mappings.
        // Module code is still fixed in this controlled example.
        let maps = mappings(pid)?;
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
        if thread_start(pid, target.tid)? != Some(target.start_time) {
            ptrace::detach(tid, None)?;
            tx.send(false)?;
            reply(&Response::TargetUnavailable { target })?;
            continue;
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
        // A mapping need not be readable through its end. Retain the readable
        // prefix; the bounded unwind callback reports any missing older frames.
        let count = memory.read_at(&mut stack[..count], address)?;
        if count == 0 {
            return Err("empty stack snapshot".into());
        }
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
            target,
            collector_pid: std::process::id(),
            interrupt_to_detach_ns: elapsed,
            request_ns: 0,
            stack_bytes: count,
            frames,
            termination,
        };
        reply(&Response::Captured(sample))?;
    }
    Ok(())
}

fn reply(response: &Response) -> Result<()> {
    println!("{}", serde_json::to_string(response)?);
    io::stdout().flush()?;
    Ok(())
}

fn thread_start(pid: u32, tid: i32) -> Result<Option<u64>> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat")) {
        Ok(stat) => stat,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(nix::libc::ESRCH) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error.into()),
    };
    // comm can contain spaces and parentheses. Fields after its closing ')' start
    // at state (field 3); starttime is field 22, hence index 19 in this suffix.
    let (_, fields) = stat.rsplit_once(") ").ok_or("invalid thread stat")?;
    Ok(Some(
        fields
            .split_whitespace()
            .nth(19)
            .ok_or("missing starttime")?
            .parse()?,
    ))
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
