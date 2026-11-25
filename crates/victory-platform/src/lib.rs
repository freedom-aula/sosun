//! The platform seam: what `victory-loader` and `victory-hle` need from the
//! host OS, behind one API with two implementations.
//!
//! * `win32/` calls the Windows API exactly as the loader and runtime did
//!   before (Wine and Windows hosts);
//! * `macos/` uses Mach VM, pthreads and `os_sync` (native macOS host).
//!
//! `docs/native-host.md` lists every Win32 API still called directly and the
//! order in which they move here. Only memory, threads and synchronisation
//! have landed.

#[cfg(not(any(windows, target_os = "macos")))]
compile_error!("victory-platform supports Windows (and Wine) and macOS only");

pub mod mem;
pub mod sync;
pub mod thread;
