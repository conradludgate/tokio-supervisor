use super::*;

const INTERVAL: Duration = Duration::from_millis(10);

fn active() -> Observation {
    Observation {
        poll_count: 0,
        parked: false,
        thread_id: Some(thread::current().id()),
    }
}

#[test]
fn classifier_reports_first_poll_once_after_elapsed_threshold() {
    let now = Instant::now();
    let observation = active();
    let mut worker = Worker::new(observation, now);
    // Repeated samples alone must not advance the no-progress timer.
    for _ in 0..10 {
        assert_eq!(worker.observe(0, observation, now, INTERVAL), None);
    }
    assert_eq!(
        worker.observe(
            0,
            observation,
            now + INTERVAL - Duration::from_nanos(1),
            INTERVAL
        ),
        None
    );
    assert_eq!(
        worker.observe(0, observation, now + INTERVAL, INTERVAL),
        Some(Event::Stalled {
            worker: 0,
            thread_id: thread::current().id(),
            duration: INTERVAL,
        })
    );
    assert_eq!(
        worker.observe(0, observation, now + INTERVAL * 10, INTERVAL),
        None
    );
}

#[test]
fn classifier_reports_recovery_and_a_later_stall() {
    let now = Instant::now();
    let mut observation = active();
    let mut worker = Worker::new(observation, now);
    assert!(matches!(
        worker.observe(0, observation, now + INTERVAL, INTERVAL),
        Some(Event::Stalled { .. })
    ));
    observation.poll_count += 1;
    assert_eq!(
        worker.observe(0, observation, now + INTERVAL * 2, INTERVAL),
        Some(Event::Recovered {
            worker: 0,
            thread_id: thread::current().id(),
            duration: INTERVAL * 2,
        })
    );
    assert_eq!(
        worker.observe(0, observation, now + INTERVAL * 2, INTERVAL),
        None
    );
    assert!(matches!(
        worker.observe(0, observation, now + INTERVAL * 3, INTERVAL),
        Some(Event::Stalled { .. })
    ));
}

#[test]
fn classifier_parking_clears_a_stall_and_resets_the_clock() {
    let now = Instant::now();
    let mut observation = active();
    let mut worker = Worker::new(observation, now);
    worker
        .observe(0, observation, now + INTERVAL, INTERVAL)
        .unwrap();
    observation.parked = true;
    assert!(matches!(
        worker.observe(0, observation, now + INTERVAL * 2, INTERVAL),
        Some(Event::Recovered { .. })
    ));
    assert_eq!(
        worker.observe(0, observation, now + INTERVAL * 10, INTERVAL),
        None
    );
    observation.parked = false;
    assert_eq!(
        worker.observe(0, observation, now + INTERVAL * 10, INTERVAL),
        None
    );
    assert!(matches!(
        worker.observe(0, observation, now + INTERVAL * 11, INTERVAL),
        Some(Event::Stalled { .. })
    ));
}

#[test]
fn classifier_waits_for_worker_start() {
    let now = Instant::now();
    let unknown = Observation {
        thread_id: None,
        ..active()
    };
    let mut worker = Worker::new(unknown, now);
    assert_eq!(
        worker.observe(0, unknown, now + INTERVAL * 10, INTERVAL),
        None
    );
    assert_eq!(
        worker.observe(0, active(), now + INTERVAL * 20, INTERVAL),
        None
    );
    assert!(matches!(
        worker.observe(0, active(), now + INTERVAL * 21, INTERVAL),
        Some(Event::Stalled { .. })
    ));
}

#[test]
fn classifier_does_not_count_idle_time_towards_a_new_stall() {
    let now = Instant::now();
    let parked = Observation {
        parked: true,
        ..active()
    };
    let mut worker = Worker::new(parked, now);
    // The active poll may have started just before this observation, even when
    // the previous observation was a long time ago.
    let unparked = now + INTERVAL * 10;
    assert_eq!(worker.observe(0, active(), unparked, INTERVAL), None);
    assert!(matches!(
        worker.observe(0, active(), unparked + INTERVAL, INTERVAL),
        Some(Event::Stalled { .. })
    ));
}

#[test]
fn classifier_handoff_recovers_old_identity_before_timing_the_replacement() {
    let now = Instant::now();
    let original = active();
    let replacement = Observation {
        thread_id: Some(thread::spawn(|| thread::current().id()).join().unwrap()),
        ..original
    };
    let mut worker = Worker::new(original, now);
    worker
        .observe(0, original, now + INTERVAL, INTERVAL)
        .unwrap();
    assert_eq!(
        worker.observe(0, replacement, now + INTERVAL * 2, INTERVAL),
        Some(Event::Recovered {
            worker: 0,
            thread_id: original.thread_id.unwrap(),
            duration: INTERVAL * 2,
        })
    );
    assert_eq!(
        worker.observe(0, replacement, now + INTERVAL * 2, INTERVAL),
        None
    );
    assert_eq!(
        worker.observe(0, replacement, now + INTERVAL * 3, INTERVAL),
        Some(Event::Stalled {
            worker: 0,
            thread_id: replacement.thread_id.unwrap(),
            duration: INTERVAL,
        })
    );
}
