//! Probe for the native Metal backend: zero-copy title memory and submission latency.
//!
//! `cargo run --release -p victory-metal` runs the memory/latency probe.
//! `victory-metal <file.dxil>...` converts DXIL through Apple's Metal Shader Converter
//! (`VICTORY_MSC_DYLIB` names `libmetalirconverter.dylib`, which is x86_64-only in GPTK:
//! build with `--target x86_64-apple-darwin` and run under Rosetta).
//! Does nothing on non-macOS targets.

#[cfg(target_os = "macos")]
mod convert;
#[cfg(target_os = "macos")]
mod corpus;
#[cfg(target_os = "macos")]
mod demo;
#[cfg(target_os = "macos")]
mod dxc;
#[cfg(target_os = "macos")]
mod probe;

fn main() {
    #[cfg(target_os = "macos")]
    {
        let files: Vec<String> = std::env::args().skip(1).collect();
        if files.first().map(String::as_str) == Some("--corpus") {
            corpus::run(&files[1..]);
        } else if files.first().map(String::as_str) == Some("--demo") {
            demo::run();
        } else if files.is_empty() {
            probe::run();
        } else {
            convert::run(&files);
        }
    }
}
