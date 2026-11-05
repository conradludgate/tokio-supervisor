//! Manual blocked-worker demonstration: cargo run --example http.
//! Visit /fast, /slow or /block_in_place on http://127.0.0.1:8123.

use std::time::Duration;

use axum::routing::get;
use tokio::{net::TcpListener, task::block_in_place};
use tokio_supervisor::Supervisor;

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    let (monitor, events) = Supervisor::new(rt.handle().clone())
        .spawn(Duration::from_millis(100))
        .unwrap();
    let reporter = std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            eprintln!("{event:?}");
        }
    });

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

    rt.block_on(async {
        axum::serve(TcpListener::bind("127.0.0.1:8123").await.unwrap(), svc)
            .await
            .unwrap()
    });
    monitor.shutdown().unwrap();
    reporter.join().unwrap();
}
