//! Exceptions for title code on macOS: what `ntdll` does on Windows.
//!
//! A fault in title code (SIGSEGV, SIGBUS, SIGILL, SIGFPE, SIGTRAP) is
//! turned into a Windows `EXCEPTION_RECORD` and `CONTEXT` on the thread's own
//! stack, and the thread is sent to an entry point that runs the same
//! sequence Windows' `KiUserExceptionDispatcher` does: the vectored
//! handlers, then structured exception handling over the mapped image's
//! `.pdata` and `.xdata` (language handlers such as `__C_specific_handler`,
//! `__try/__except` filters and `__finally` blocks), then the unhandled
//! exception filter. A handler that continues or an `__except` that is
//! taken ends in a jump to a `CONTEXT`.
//!
//! * [`context`]: the `CONTEXT` and `EXCEPTION_RECORD` layouts;
//! * [`unwind`]: function tables and the unwinder (any host);
//! * `dispatch` and `signal`: the dispatcher, `RaiseException` and the
//!   signal handler (x86-64 hosts, under Rosetta).
//!
//! **Signals, not a Mach exception port.** The handler runs on the faulting
//! thread with its exact state; it only has to move the state to the stack
//! and redirect the thread, so nothing is called in signal context, and no
//! second thread, port or `thread_get_state` round trip is involved. A
//! Mach exception port would be the way to deal with a debugger or with
//! faults in host code (a fault there is not dispatched, and ends the
//! process); it is the next step for those.
//!
//! **GS.** The handler only reads GS (to tell title code, GS = TEB, from
//! host code, GS = pthread TSD) and leaves it alone. The dispatch entry
//! switches to the host base before any Rust runs; title handlers are
//! called through [`super::call_win64`], which sets the TEB for them and
//! restores the host base after, and a jump to a `CONTEXT` sets the TEB
//! last.

pub mod context;
pub mod unwind;

#[cfg(target_arch = "x86_64")]
mod dispatch;
#[cfg(target_arch = "x86_64")]
pub mod signal;

#[cfg(target_arch = "x86_64")]
pub use dispatch::{
    backtrace,
    Handler, add_vectored_handler, remove_vectored_handler, set_unhandled_filter,
};
#[cfg(target_arch = "x86_64")]
pub use signal::install;

/// The functions title code calls (bound by name in `victory-hle`).
#[cfg(target_arch = "x86_64")]
pub mod api {
    pub use super::dispatch::api::*;
}
