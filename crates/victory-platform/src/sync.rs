//! Kernel-style synchronisation objects: events, semaphores and
//! wait-on-address, the three things title threads block on.

#[cfg(windows)]
#[path = "win32/sync.rs"]
mod imp;
#[cfg(target_os = "macos")]
#[path = "macos/sync.rs"]
mod imp;

pub use imp::*;
