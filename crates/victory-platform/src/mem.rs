//! Address space: reserve, commit, protect, and shared page pools.
//!
//! A [`PagePool`] is one OS memory object that can be mapped at several
//! addresses at once (title memory seen by the GPU and the CPU, aliases of a
//! texture, a framebuffer). On macOS one Mach memory entry backs the whole
//! pool, so mapping or re-protecting a range costs one VM call however many
//! pages it holds. On Windows the same API sits on a file mapping and
//! placeholders, with Windows' 64 KiB view granularity.
//!
//! Every function that takes an address is `unsafe`: the caller owns the
//! range it names.

#[cfg(windows)]
#[path = "win32/mem.rs"]
mod imp;
#[cfg(target_os = "macos")]
#[path = "macos/mem.rs"]
mod imp;

pub use imp::*;
