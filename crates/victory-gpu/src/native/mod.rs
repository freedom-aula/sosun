//! The native host: victory-gpu's work for a host GPU API reached through `victory_backend`'s
//! traits instead of `windows::Win32::Graphics::Direct3D12`. Platform neutral and generic over the
//! backend, so it builds (and is linted) on the Windows target too; on macOS it is instantiated with
//! `victory_metal::metal::Metal` ([`Host`]). The D3D12 host (`host.rs`, `draw.rs`, ...) is untouched.
//!
//! Module selection is by `cfg`, not at run time: `crate::host` and everything under it exist only
//! with `cfg(windows)`, and this module is what a macOS build links the title-facing objects
//! (`list.rs`, `queue.rs`, `fence.rs`) to.
//!
//! [`memory`] title memory as zero-copy pools (no shadow, watch or copy-back), [`descriptors`]
//! the title's views for the backend, [`list`] a command list, [`host`] queues, pipelines, heaps and
//! submission.

pub mod descriptors;
pub mod host;
pub mod list;
pub mod memory;

#[cfg(target_os = "macos")]
pub type Host = host::NativeHost<victory_metal::metal::Metal>;

#[cfg(all(test, target_os = "macos"))]
mod tests;
