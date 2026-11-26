//! The native macOS path: map a title's PE image into a macOS process with
//! no Windows loader and no Wine, bind its imports against `victory-hle`,
//! and run it.
//!
//! * [`image`] maps, relocates and protects an image (any macOS host);
//! * [`teb`], [`bridge`] and [`run`] run x86-64 title code under Rosetta, in
//!   an x86-64 macOS process: the TEB at `gs:`, static TLS, and the
//!   Windows-to-System V calling-convention bridge;
//! * [`bind`] fills an image's IAT through those bridges.
//!
//! `docs/native-host.md` explains the design and the interfaces other
//! components (the Metal backend, the recompiler) code against.

pub mod dll;
mod image;
pub mod seh;

pub use image::{NativeImage, TlsTemplate};

#[cfg(target_arch = "x86_64")]
mod bind;
#[cfg(target_arch = "x86_64")]
pub mod bridge;
#[cfg(target_arch = "x86_64")]
mod run;
#[cfg(target_arch = "x86_64")]
pub mod teb;
#[cfg(target_arch = "x86_64")]
pub mod threadctl;

#[cfg(target_arch = "x86_64")]
pub use bind::bind_imports;
#[cfg(target_arch = "x86_64")]
pub use bridge::{
    bridged_table, call_win64, declare_direct, declare_win64, expose_object, host_callbacks,
    is_bridged, is_title_code,
};
#[cfg(target_arch = "x86_64")]
pub use run::{
    attach, attach_dll, disable_thread_calls, register_dll_thread_notices,
    register_thread_callbacks, run_entry, run_entry_with, spawn_title_thread,
    spawn_title_thread_with,
};
