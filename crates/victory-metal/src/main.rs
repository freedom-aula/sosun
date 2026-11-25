//! Probe for the native Metal backend: zero-copy title memory and submission latency.
//! Run: `cargo run --release -p victory-metal`. Does nothing on non-macOS targets.

#[cfg(target_os = "macos")]
mod mem;
#[cfg(target_os = "macos")]
mod probe;

fn main() {
    #[cfg(target_os = "macos")]
    probe::run();
}
