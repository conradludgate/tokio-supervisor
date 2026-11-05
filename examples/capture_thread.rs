//! Repeated single-thread snapshots on Linux x86_64. See docs/thread-capture.md.

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    eprintln!("this experiment requires Linux x86_64");
    std::process::exit(1);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[path = "support/thread_capture.rs"]
mod capture;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    capture::run()
}
