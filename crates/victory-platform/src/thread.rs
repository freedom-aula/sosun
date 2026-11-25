//! Threads: start with a chosen stack size, join for an exit code, identify.

#[cfg(windows)]
#[path = "win32/thread.rs"]
mod imp;
#[cfg(target_os = "macos")]
#[path = "macos/thread.rs"]
mod imp;

pub use imp::*;
