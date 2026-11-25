//! The platform seam: what `victory-loader` and `victory-hle` need from the
//! host OS, behind one API with two implementations.
//!
//! * `win32/` calls the Windows API exactly as the loader and runtime did
//!   before (Wine and Windows hosts);
//! * `macos/` uses Mach VM, pthreads and `os_sync` (native macOS host).
//!
//! `docs/native-host.md` lists every Win32 API still called directly and the
//! order in which they move here. Only memory, threads and synchronisation
//! have landed. On any other OS the crate is empty.

#[cfg(any(windows, target_os = "macos"))]
pub mod mem;
#[cfg(any(windows, target_os = "macos"))]
pub mod sync;
#[cfg(any(windows, target_os = "macos"))]
pub mod thread;
