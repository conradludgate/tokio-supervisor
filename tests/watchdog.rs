use std::{
    collections::HashSet,
    io,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use tokio::{
    runtime::{Builder, Runtime},
    task::JoinHandle,
};
use tokio_supervisor::{Event, Supervisor};

const INTERVAL: Duration = Duration::from_millis(10);
const DEADLINE: Duration = Duration::from_secs(3);

fn receive(events: &Receiver<Event>) -> Event {
    events
        .recv_timeout(DEADLINE)
        .expect("watchdog did not deliver an event")
}

// Dropping these senders also releases tasks when an assertion fails, so a
// failed observation cannot strand an application task during runtime teardown.
struct BlockedWorkers {
    releases: Vec<mpsc::Sender<()>>,
    tasks: Vec<JoinHandle<()>>,
}

impl BlockedWorkers {
    fn new(rt: &Runtime, count: usize) -> Self {
        let (started, ready) = mpsc::channel();
        let mut workers = Self {
            releases: vec![],
            tasks: vec![],
        };
        for _ in 0..count {
            let (release, wait) = mpsc::channel();
            let started = started.clone();
            workers.releases.push(release);
            workers.tasks.push(rt.spawn(async move {
                let _ = started.send(());
                let _ = wait.recv();
            }));
        }
        for _ in 0..count {
            ready.recv_timeout(DEADLINE).unwrap();
        }
        workers
    }

    fn release(&mut self, rt: &Runtime) {
        self.releases.clear();
        for task in self.tasks.drain(..) {
            rt.block_on(task).unwrap();
        }
    }
}

#[test]
fn detects_every_worker_in_its_first_blocked_poll_and_reports_recovery() {
    let rt = Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .unwrap();
    let mut blocked = BlockedWorkers::new(&rt, 2);
    for worker in 0..2 {
        assert_eq!(rt.metrics().worker_poll_count(worker), 0);
    }
    let (monitor, events) = Supervisor::new(rt.handle().clone())
        .spawn(INTERVAL)
        .unwrap();

    let mut stalled = HashSet::new();
    for _ in 0..2 {
        let Event::Stalled {
            worker,
            thread_id,
            duration,
        } = receive(&events)
        else {
            panic!("expected stall");
        };
        assert_eq!(Some(thread_id), rt.metrics().worker_thread_id(worker));
        assert!(duration >= INTERVAL);
        assert!(stalled.insert(worker));
    }
    assert_eq!(stalled, HashSet::from([0, 1]));
    assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
    blocked.release(&rt);

    let mut recovered = HashSet::new();
    for _ in 0..2 {
        let Event::Recovered { worker, .. } = receive(&events) else {
            panic!("expected recovery");
        };
        assert!(recovered.insert(worker));
    }
    assert_eq!(recovered, stalled);
    monitor.shutdown().unwrap();
    assert_eq!(events.recv(), Err(mpsc::RecvError));
}

#[test]
fn idle_workers_and_async_sleep_are_not_stalls() {
    let rt = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (monitor, events) = Supervisor::new(rt.handle().clone())
        .spawn(INTERVAL)
        .unwrap();
    let task = rt.spawn(async {
        tokio::time::sleep(Duration::from_millis(150)).await;
    });
    rt.block_on(task).unwrap();
    monitor.shutdown().unwrap();
    assert_eq!(events.recv(), Err(mpsc::RecvError));
}

#[test]
fn block_in_place_keeps_the_logical_worker_available() {
    let rt = Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    let (started, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel::<()>();
    let task = rt.spawn(async move {
        tokio::task::block_in_place(|| {
            let _ = started.send(());
            let _ = wait.recv();
        });
    });
    ready.recv_timeout(DEADLINE).unwrap();
    // Prove the replacement can run a task before observing its idle period.
    rt.block_on(rt.spawn(async {})).unwrap();
    let (monitor, events) = Supervisor::new(rt.handle().clone())
        .spawn(INTERVAL)
        .unwrap();
    thread::sleep(Duration::from_millis(150));
    drop(release);
    rt.block_on(task).unwrap();
    monitor.shutdown().unwrap();
    assert_eq!(events.recv(), Err(mpsc::RecvError));
}

#[test]
fn full_event_queue_does_not_stop_observing_later_transitions() {
    let rt = Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    let mut first = BlockedWorkers::new(&rt, 1);
    let (monitor, events) = Supervisor::new(rt.handle().clone())
        .spawn(INTERVAL)
        .unwrap();
    let deadline = Instant::now() + DEADLINE;
    while monitor.dropped_events() < 2 {
        assert!(
            Instant::now() < deadline,
            "full event queue stopped the watchdog"
        );
        thread::sleep(INTERVAL * 3);
        first.release(&rt);
        thread::sleep(INTERVAL * 3);
        if monitor.dropped_events() < 2 {
            first = BlockedWorkers::new(&rt, 1);
        }
    }
    // Keep the full receiver alive through shutdown.
    assert_eq!(events.try_iter().count(), 2);
    monitor.shutdown().unwrap();
}

#[test]
fn dropping_monitor_wakes_a_long_interval_while_all_workers_are_blocked() {
    let rt = Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    let _blocked = BlockedWorkers::new(&rt, 1);
    let (monitor, events) = Supervisor::new(rt.handle().clone())
        .spawn(Duration::from_secs(60))
        .unwrap();
    let (stopped, ready) = mpsc::channel();
    let shutdown = thread::spawn(move || {
        drop(monitor);
        stopped.send(()).unwrap();
    });
    ready.recv_timeout(DEADLINE).unwrap();
    shutdown.join().unwrap();
    assert_eq!(events.recv(), Err(mpsc::RecvError));
}

#[test]
fn explicit_shutdown_does_not_need_a_live_runtime() {
    let rt = Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    let (monitor, events) = Supervisor::new(rt.handle().clone())
        .spawn(Duration::from_secs(60))
        .unwrap();
    drop(rt);
    let (stopped, ready) = mpsc::channel();
    let shutdown = thread::spawn(move || {
        monitor.shutdown().unwrap();
        stopped.send(()).unwrap();
    });
    ready.recv_timeout(DEADLINE).unwrap();
    shutdown.join().unwrap();
    assert_eq!(events.recv(), Err(mpsc::RecvError));
}

#[test]
fn rejects_zero_intervals_and_current_thread_runtimes() {
    let rt = Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    let error = Supervisor::new(rt.handle().clone())
        .spawn(Duration::ZERO)
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    let current = Builder::new_current_thread().build().unwrap();
    let error = Supervisor::new(current.handle().clone())
        .spawn(INTERVAL)
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}
