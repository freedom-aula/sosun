//! The two crossings between our runtime and title code that need care on
//! the native macOS path (title code is Windows x64 with GS = the TEB; ours
//! is System V with GS = the host's TSD). On Windows and Wine both are
//! plain calls and nothing here does anything.
//!
//! * [`expose`]: an object (first word = method table) is being handed to
//!   the title. On macOS x86-64 its table word is replaced by a bridged copy
//!   (every slot a bridge stub). `this` stays the object's address, so our
//!   methods find their state as before; the cost is that Rust code must
//!   not call the object's methods through the table word afterwards (ours
//!   never does outside tests, which never expose).
//! * [`call`]: calls a callback pointer the title gave us. If it points into
//!   a mapped title image it goes through `call_win64`; a function of ours
//!   (a Rust callback, as tests and internal completions use) is called
//!   directly.

/// Gives the object at `this`, whose first word is a method table of `len`
/// slots, a table the title can call. Idempotent.
///
/// # Safety
/// `this` is a live object of ours, writable, with such a first word.
#[inline]
pub unsafe fn expose<T>(this: *const T, len: usize, area: &'static str) -> *const T {
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    // SAFETY: forwarded.
    unsafe {
        victory_loader::native::expose_object(this.cast(), len, area)
    };
    #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
    let _ = (len, area);
    this
}

/// Calls the `extern "system"` function at `f` with integer or pointer
/// `args` (up to 6), returning `rax`. Narrow results (`HRESULT`, `bool`)
/// are the caller's to truncate.
///
/// # Safety
/// `f` is a function of that signature, ours or the title's.
pub unsafe fn call(f: usize, args: &[u64]) -> u64 {
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    if victory_loader::native::is_title_code(f) {
        // SAFETY: forwarded.
        return unsafe { victory_loader::native::call_win64(f, args) };
    }
    let a = |i: usize| args.get(i).copied().unwrap_or(0);
    // SAFETY: forwarded; unused trailing arguments are ignored by the callee.
    unsafe {
        let f: unsafe extern "system" fn(u64, u64, u64, u64, u64, u64) -> u64 = std::mem::transmute::<
            usize,
            unsafe extern "system" fn(u64, u64, u64, u64, u64, u64) -> u64,
        >(f);
        f(a(0), a(1), a(2), a(3), a(4), a(5))
    }
}
