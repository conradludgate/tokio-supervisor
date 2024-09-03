//! <https://turso.tech/blog/how-to-deadlock-tokio-application-in-rust-with-just-a-single-mutex>

use std::time::Duration;

use tokio_supervisor::Supervisor;

async fn sleepy_task() {
    tokio::time::sleep(Duration::from_millis(100)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test() {
    Supervisor::new(&tokio::runtime::Handle::current()).spawn(Duration::from_millis(100));

    let mutex = std::sync::Arc::new(std::sync::Mutex::new(()));
    let async_task = tokio::spawn({
        let mutex = mutex.clone();
        async move {
            loop {
                eprintln!("async thread start");
                tokio::time::sleep(Duration::from_millis(100)).await;
                let guard = mutex.lock().unwrap();
                drop(guard);
                eprintln!("async thread end");
            }
        }
    });
    let blocking_task = tokio::task::spawn_blocking({
        let mutex = mutex.clone();
        move || loop {
            eprintln!("blocking thread start");
            let guard = mutex.lock().unwrap();
            tokio::runtime::Handle::current().block_on(sleepy_task());
            drop(guard);
            eprintln!("blocking thread end");
        }
    });

    for future in [async_task, blocking_task] {
        future.await.unwrap();
    }
}
