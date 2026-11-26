//! The calling-convention bridge between title code (Windows x64 ABI, GS =
//! the TEB) and host code (this process's own ABI, GS = pthread TSD).
//!
//! **Title to host.** Every import bound to a host function goes through a
//! 32-byte stub that loads the target (and a context word) and jumps to
//! `victory_bridge_to_host`. That routine saves what the Windows ABI makes
//! the callee keep but the System V ABI does not (`rdi`, `rsi`, `xmm6` to
//! `xmm15`), spills the four register arguments, switches GS to the host's
//! TSD base, loads System V argument registers and the stack arguments
//! (up to 12 integer or pointer arguments), calls, switches GS back and
//! returns. The result is in `rax`, or `xmm0` for a float, in both ABIs, so
//! it passes through. Host functions with float or double arguments cannot
//! be remapped without their signature, so they are written
//! `extern "win64"` and bound through `victory_bridge_win64`, which swaps
//! GS and nothing else. See [`declare_win64`].
//!
//! **Host to title.** [`call_win64`] puts arguments where the Windows ABI
//! wants them (four registers, 32 bytes of shadow space, the rest on the
//! stack), switches GS to the thread's TEB, calls and switches back.
//!
//! Both directions switch GS to an explicit base rather than toggling, so
//! they nest freely: title, host, title again, and so on.

use std::cell::Cell;
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, bail};
use victory_platform::mem::{self, Protect};

use super::teb::{self, OFF_HOST_TSD};

/// The macOS machdep call that sets the calling thread's GS base. Rosetta
/// implements it; native x86-64 macOS does too.
pub(crate) const SET_GS_BASE: u32 = 0x300_0003;

core::arch::global_asm!(
    ".p2align 4",
    ".globl _victory_bridge_to_host",
    "_victory_bridge_to_host:",
    "push rbp",
    "mov rbp, rsp",
    "push rbx",
    "push rdi",
    "push rsi",
    "push r12",
    "push r13",
    "push r14",
    "sub rsp, 0x100",
    // The four register arguments, then the registers Windows keeps for us.
    "mov [rsp + 0x30], rcx",
    "mov [rsp + 0x38], rdx",
    "mov [rsp + 0x40], r8",
    "mov [rsp + 0x48], r9",
    "movaps [rsp + 0x50], xmm6",
    "movaps [rsp + 0x60], xmm7",
    "movaps [rsp + 0x70], xmm8",
    "movaps [rsp + 0x80], xmm9",
    "movaps [rsp + 0x90], xmm10",
    "movaps [rsp + 0xA0], xmm11",
    "movaps [rsp + 0xB0], xmm12",
    "movaps [rsp + 0xC0], xmm13",
    "movaps [rsp + 0xD0], xmm14",
    "movaps [rsp + 0xE0], xmm15",
    "mov r12, gs:[0x30]",
    "mov r13, r10",
    "mov r14, r11",
    // Where this thread is, for the thread dump.
    "mov rax, [rbp + 8]",
    "mov [r12 + {in_host_ret}], rax",
    "mov [r12 + {in_host_ctx}], r14",
    "mov [r12 + {in_host_rbp}], rbp",
    "mov [r12 + {in_host}], r13",
    // VICTORY_PROFILE: the time stamp counter at entry, for traced stubs.
    "test r14, 1",
    "jz 9f",
    "rdtsc",
    "shl rdx, 32",
    "or rax, rdx",
    "mov [rsp + 0xF0], rax",
    "9:",
    "mov rbx, [r12 + {host_tsd}]",
    "mov rdi, rbx",
    "mov eax, {set_gs}",
    "syscall",
    "mov rdi, [rsp + 0x30]",
    "mov rsi, [rsp + 0x38]",
    "mov rdx, [rsp + 0x40]",
    "mov rcx, [rsp + 0x48]",
    "mov r8, [rbp + 0x30]",
    "mov r9, [rbp + 0x38]",
    "mov rax, [rbp + 0x40]",
    "mov [rsp], rax",
    "mov rax, [rbp + 0x48]",
    "mov [rsp + 0x08], rax",
    "mov rax, [rbp + 0x50]",
    "mov [rsp + 0x10], rax",
    "mov rax, [rbp + 0x58]",
    "mov [rsp + 0x18], rax",
    "mov rax, [rbp + 0x60]",
    "mov [rsp + 0x20], rax",
    "mov rax, [rbp + 0x68]",
    "mov [rsp + 0x28], rax",
    // A stub with a context word (an unimplemented import) takes it as the
    // first argument.
    // Bit 0 of the context marks a traced method: `this` stays the first
    // argument and the result goes to `trace_ret` (VICTORY_TRACE_FAILS).
    "test r14, r14",
    "jz 7f",
    "test r14, 1",
    "jnz 7f",
    "mov rdi, r14",
    "7:",
    "xor eax, eax",
    "call r13",
    "mov r13, rax",
    "mov qword ptr [r12 + {in_host}], 0",
    "test r14, 1",
    "jz 8f",
    "lea rdi, [r14 - 1]",
    "mov esi, r13d",
    "mov rdx, [rsp + 0x30]",
    "mov rcx, [rbp + 8]",
    "mov r8, [rsp + 0xF0]",
    "lea r9, [rsp + 0x30]",
    "call {trace_ret}",
    "8:",
    "mov rdi, r12",
    "mov eax, {set_gs}",
    "syscall",
    "mov rax, r13",
    "movaps xmm6, [rsp + 0x50]",
    "movaps xmm7, [rsp + 0x60]",
    "movaps xmm8, [rsp + 0x70]",
    "movaps xmm9, [rsp + 0x80]",
    "movaps xmm10, [rsp + 0x90]",
    "movaps xmm11, [rsp + 0xA0]",
    "movaps xmm12, [rsp + 0xB0]",
    "movaps xmm13, [rsp + 0xC0]",
    "movaps xmm14, [rsp + 0xD0]",
    "movaps xmm15, [rsp + 0xE0]",
    "add rsp, 0x100",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rsi",
    "pop rdi",
    "pop rbx",
    "pop rbp",
    "ret",
    // The same crossing for a host function written in the Windows ABI:
    // arguments stay where they are (registers and the caller's stack).
    ".p2align 4",
    ".globl _victory_bridge_win64",
    "_victory_bridge_win64:",
    "push rbp",
    "mov rbp, rsp",
    "push rbx",
    "push rdi",
    "push rsi",
    "push r12",
    "push r13",
    "push r14",
    "sub rsp, 0x140",
    "mov [rsp + 0x60], rcx",
    "mov [rsp + 0x68], rdx",
    "mov [rsp + 0x70], r8",
    "mov [rsp + 0x78], r9",
    "movaps [rsp + 0x80], xmm6",
    "movaps [rsp + 0x90], xmm7",
    "movaps [rsp + 0xA0], xmm8",
    "movaps [rsp + 0xB0], xmm9",
    "movaps [rsp + 0xC0], xmm10",
    "movaps [rsp + 0xD0], xmm11",
    "movaps [rsp + 0xE0], xmm12",
    "movaps [rsp + 0xF0], xmm13",
    "movaps [rsp + 0x100], xmm14",
    "movaps [rsp + 0x110], xmm15",
    "mov r12, gs:[0x30]",
    "mov r13, r10",
    "mov rbx, [r12 + {host_tsd}]",
    "movaps [rsp + 0x40], xmm0",
    "movaps [rsp + 0x50], xmm1",
    "movaps [rsp + 0x120], xmm2",
    "movaps [rsp + 0x130], xmm3",
    "mov rdi, rbx",
    "mov eax, {set_gs}",
    "syscall",
    // Stack arguments 5 to 10, above the callee's 32 bytes of shadow space.
    "mov rax, [rbp + 0x30]",
    "mov [rsp + 0x20], rax",
    "mov rax, [rbp + 0x38]",
    "mov [rsp + 0x28], rax",
    "mov rax, [rbp + 0x40]",
    "mov [rsp + 0x30], rax",
    "mov rax, [rbp + 0x48]",
    "mov [rsp + 0x38], rax",
    "mov rcx, [rsp + 0x60]",
    "mov rdx, [rsp + 0x68]",
    "mov r8, [rsp + 0x70]",
    "mov r9, [rsp + 0x78]",
    "movaps xmm0, [rsp + 0x40]",
    "movaps xmm1, [rsp + 0x50]",
    "movaps xmm2, [rsp + 0x120]",
    "movaps xmm3, [rsp + 0x130]",
    "call r13",
    "mov r13, rax",
    "movaps [rsp + 0x40], xmm0",
    "mov rdi, r12",
    "mov eax, {set_gs}",
    "syscall",
    "mov rax, r13",
    "movaps xmm0, [rsp + 0x40]",
    "movaps xmm6, [rsp + 0x80]",
    "movaps xmm7, [rsp + 0x90]",
    "movaps xmm8, [rsp + 0xA0]",
    "movaps xmm9, [rsp + 0xB0]",
    "movaps xmm10, [rsp + 0xC0]",
    "movaps xmm11, [rsp + 0xD0]",
    "movaps xmm12, [rsp + 0xE0]",
    "movaps xmm13, [rsp + 0xF0]",
    "movaps xmm14, [rsp + 0x100]",
    "movaps xmm15, [rsp + 0x110]",
    "add rsp, 0x140",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rsi",
    "pop rdi",
    "pop rbx",
    "pop rbp",
    "ret",
    // victory_call_win64(f: rdi, args: rsi, nargs: rdx, teb: rcx, exit_frame: r8) -> rax
    ".p2align 4",
    ".globl _victory_call_win64",
    "_victory_call_win64:",
    "push rbp",
    "mov rbp, rsp",
    "push rbx",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "sub rsp, 8",
    "test r8, r8",
    "jz 5f",
    "mov [r8], rbp",
    "5:",
    "mov r12, rdi",
    "mov r13, rsi",
    "mov r14, rdx",
    "mov r15, rcx",
    "mov rbx, [r15 + {host_tsd}]",
    "mov rax, r14",
    "sub rax, 4",
    "jg 2f",
    "xor eax, eax",
    "2:",
    "lea rax, [rax * 8 + 47]",
    "and rax, -16",
    "sub rsp, rax",
    "mov rcx, 4",
    "3:",
    "cmp rcx, r14",
    "jae 4f",
    "mov rax, [r13 + rcx * 8]",
    "mov [rsp + rcx * 8], rax",
    "inc rcx",
    "jmp 3b",
    "4:",
    "mov rdi, r15",
    "mov eax, {set_gs}",
    "syscall",
    "mov rcx, [r13]",
    "mov rdx, [r13 + 8]",
    "mov r8, [r13 + 16]",
    "mov r9, [r13 + 24]",
    "call r12",
    "mov r12, rax",
    "mov rdi, rbx",
    "mov eax, {set_gs}",
    "syscall",
    "mov rax, r12",
    "jmp 6f",
    // Return directly from the thread's outer bridge call. GS is already
    // the host's because _endthreadex entered through a host stub.
    ".globl _victory_exit_title_thread",
    "_victory_exit_title_thread:",
    "mov rbp, rdi",
    "mov eax, esi",
    "6:",
    "lea rsp, [rbp - 40]",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
    host_tsd = const OFF_HOST_TSD,
    set_gs = const SET_GS_BASE,
    trace_ret = sym trace_ret,
    in_host = const super::teb::OFF_IN_HOST,
    in_host_ret = const super::teb::OFF_IN_HOST_RET,
    in_host_ctx = const super::teb::OFF_IN_HOST_CTX,
    in_host_rbp = const super::teb::OFF_IN_HOST_RBP,
);

/// Which method of which object a traced stub stands for.
struct Traced {
    area: &'static str,
    slot: usize,
    /// An import's symbol (the area is its DLL); those log every call.
    name: &'static str,
    /// VICTORY_PROFILE: calls, total and longest time in the call (TSC ticks).
    calls: AtomicU64,
    ticks: AtomicU64,
    longest: AtomicU64,
}

/// Every `Traced` record, for the profile dump.
static TRACED: Mutex<Vec<&'static Traced>> = Mutex::new(Vec::new());

fn new_traced(area: &'static str, slot: usize, name: &'static str) -> &'static Traced {
    let t: &'static Traced = Box::leak(Box::new(Traced {
        area,
        slot,
        name,
        calls: Default::default(),
        ticks: Default::default(),
        longest: Default::default(),
    }));
    TRACED.lock().unwrap_or_else(|e| e.into_inner()).push(t);
    t
}

/// `VICTORY_PROFILE=<seconds>`: every title-to-host call is timed (count, total, longest) and the
/// table is logged, longest total first, every that many seconds.
pub(crate) fn profile_seconds() -> Option<u64> {
    static SECS: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *SECS.get_or_init(|| std::env::var("VICTORY_PROFILE").ok()?.parse().ok())
}

fn tsc() -> u64 {
    // SAFETY: rdtsc has no preconditions.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Starts the thread that logs the profile.
pub(crate) fn start_profile() {
    let Some(secs) = profile_seconds() else { return };
    std::thread::spawn(move || {
        let (t0, c0) = (std::time::Instant::now(), tsc());
        std::thread::sleep(std::time::Duration::from_millis(100));
        let per_ms = (tsc() - c0) as f64 / t0.elapsed().as_secs_f64() / 1000.0;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            let mut rows: Vec<_> = TRACED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|t| (t.ticks.load(Ordering::Relaxed), *t))
                .filter(|(_, t)| t.calls.load(Ordering::Relaxed) > 0)
                .collect();
            rows.sort_by_key(|r| std::cmp::Reverse(r.0));
            tracing::warn!("profile at {:.1}s: {} methods", t0.elapsed().as_secs_f64(), rows.len());
            for (ticks, t) in rows.iter().take(40) {
                tracing::warn!(
                    "profile {}!{}{:#x} calls {} total {:.1} ms max {:.2} ms",
                    t.area,
                    t.name,
                    t.slot * 8,
                    t.calls.load(Ordering::Relaxed),
                    *ticks as f64 / per_ms,
                    t.longest.load(Ordering::Relaxed) as f64 / per_ms
                );
            }
        }
    });
}

/// The context word that makes a stub log its results: `area!name` imports
/// when `VICTORY_TRACE_IMPORTS` is set (see [`trace_imports`]).
pub(crate) fn traced_import(dll: &str, symbol: &str) -> usize {
    let leak = |s: &str| &*Box::leak(s.to_string().into_boxed_str());
    let info = new_traced(leak(dll), 0, leak(symbol));
    info as *const Traced as usize | 1
}

/// Whether to log every call of every import (`VICTORY_TRACE_IMPORTS=1`).
pub(crate) fn trace_imports() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TRACE_IMPORTS").is_some_and(|v| v != "0"))
}

/// `(dll, symbol)` of an import stub's context word, as [`traced_import`] made it.
pub(crate) fn traced_name(context: usize) -> Option<(&'static str, &'static str, usize)> {
    if context & 1 == 0 {
        return None;
    }
    // SAFETY: odd context words are leaked `Traced` records plus one.
    let info = unsafe { &*((context - 1) as *const Traced) };
    Some((info.area, info.name, info.slot * 8))
}

extern "C" fn trace_ret(info: &Traced, hr: i32, this: usize, ret: usize, start: u64, args: *const u64) {
    if callbacks_enabled() {
        note_callbacks(info, args);
    }
    if profile_seconds().is_some() {
        let d = tsc().wrapping_sub(start);
        info.calls.fetch_add(1, Ordering::Relaxed);
        info.ticks.fetch_add(d, Ordering::Relaxed);
        info.longest.fetch_max(d, Ordering::Relaxed);
    }
    if if info.name.is_empty() { trace_level() > 0 && (hr < 0 || trace_level() == 2) } else { trace_imports() } {
        tracing::warn!(
            area = info.area,
            name = info.name,
            slot = format_args!("{:#x}", info.slot * 8),
            this = format_args!("{this:#x}"),
            hr = format_args!("{:#010x}", hr as u32),
            from = format_args!(
                "{:#x?}",
                super::seh::unwind::image_base_of(ret).map(|b| ret - b)
            ),
            "title -> host method returned"
        );
    }
}

/// 0 off, 1 failing results (negative `eax`; a method returning a pointer
/// or a count shows up as noise), 2 every call.
fn trace_level() -> u8 {
    static LEVEL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| match std::env::var("VICTORY_TRACE_FAILS").as_deref() {
        Ok("0") | Err(_) => 0,
        Ok("2") => 2,
        Ok(_) => 1,
    })
}

unsafe extern "C" {
    fn victory_bridge_to_host();
    fn victory_bridge_win64();
    fn victory_call_win64(
        f: usize,
        args: *const u64,
        nargs: usize,
        teb: *mut u8,
        exit_frame: *mut usize,
    ) -> u64;
    fn victory_exit_title_thread(frame: usize, code: u32) -> !;
}

/// Most arguments [`call_win64`] and the System V bridge pass.
pub const MAX_ARGS: usize = 12;

/// Calls the Windows x64 function at `f` with integer or pointer `args` on
/// the calling thread (which gets a TEB if it has none), and returns `rax`.
///
/// # Safety
/// `f` must be a Windows x64 function taking `args.len()` such arguments,
/// in an image whose imports are bound.
pub unsafe fn call_win64(f: usize, args: &[u64]) -> u64 {
    if callbacks_enabled() {
        CALLED.lock().unwrap_or_else(|e| e.into_inner()).insert(f);
    }
    assert!(args.len() <= MAX_ARGS, "too many arguments");
    let mut padded = [0u64; MAX_ARGS];
    padded[..args.len()].copy_from_slice(args);
    // SAFETY: forwarded from the caller; `padded` holds at least 4 words.
    unsafe {
        victory_call_win64(
            f,
            padded.as_ptr(),
            args.len(),
            teb::current(),
            std::ptr::null_mut(),
        )
    }
}

thread_local! {
    static THREAD_EXIT_FRAME: Cell<usize> = const { Cell::new(0) };
}

/// Calls a thread entry point, which can return or exit through
/// [`exit_title_thread`]. Nested `call_win64` calls leave this frame in place.
///
/// # Safety
/// As for [`call_win64`], with one argument and no active thread entry frame.
pub(super) unsafe fn call_thread_entry(f: usize, param: usize) -> u32 {
    THREAD_EXIT_FRAME.with(|frame| {
        debug_assert_eq!(frame.get(), 0);
        let args = [param as u64, 0, 0, 0];
        // SAFETY: forwarded from the caller; assembly records the frame for this call.
        let code =
            unsafe { victory_call_win64(f, args.as_ptr(), 1, teb::current(), frame.as_ptr()) };
        frame.set(0);
        code as u32
    })
}

/// Returns from the active thread entry point with `code`, leaving the
/// loader to run its thread-detach notifications and finish the thread.
///
/// # Safety
/// Must be called from a guest thread entry via a host stub with host GS.
/// This abandons the intervening guest and host callback frames.
pub unsafe fn exit_title_thread(code: u32) -> ! {
    let frame = THREAD_EXIT_FRAME.get();
    if frame == 0 {
        tracing::error!("thread exit outside a title thread entry point");
        std::process::abort();
    }
    // SAFETY: this thread owns the TEB and the live bridge frame.
    unsafe {
        teb::current()
            .add(teb::OFF_IN_HOST)
            .cast::<usize>()
            .write(0);
        victory_exit_title_thread(frame, code);
    }
}

/// Host functions written `extern "win64"` (arguments in the Windows ABI,
/// including floats), by address. Binding one skips the argument remapping.
static WIN64: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

/// Declares that the host function at `address` uses the Windows x64 ABI.
pub fn declare_win64(address: usize) {
    WIN64
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_default()
        .insert(address);
}

/// Functions of ours that follow the Windows ABI and switch GS themselves
/// (`RtlCaptureContext`, `RaiseException`): bound with no stub at all.
static DIRECT: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

/// Declares that the function at `address` needs no bridge stub.
pub fn declare_direct(address: usize) {
    DIRECT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_default()
        .insert(address);
}

pub(crate) fn is_direct(address: usize) -> bool {
    DIRECT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&address))
}

pub(crate) fn is_win64(address: usize) -> bool {
    WIN64
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&address))
}

const STUB_SIZE: usize = 32;

/// Bridge stubs for host functions the title calls through pointers it was
/// handed (the methods of a COM object), not through its import table, one
/// per `(target, win64)` in the same order; `win64` says the function is
/// `extern "win64"` (floats in their Windows registers). The stubs live for
/// the life of the process.
pub fn host_callbacks(targets: &[(usize, bool)]) -> Result<Vec<usize>> {
    let mut block = StubBlock::new(targets.len())?;
    let stubs = targets
        .iter()
        .map(|&(target, win64)| block.emit(target, 0, win64))
        .collect::<Result<Vec<_>>>()?;
    block.seal()?;
    Ok(stubs)
}

/// Whether `address` is code of a mapped title image (or DLL), as opposed to
/// a function of ours. A host function pointer may be handed back to us in
/// place of a title callback; only title code needs [`call_win64`].
pub fn is_title_code(address: usize) -> bool {
    super::seh::unwind::image_base_of(address).is_some()
}

/// Extra slots after the last real one in a bridged table. A title that
/// calls a method we never listed would read past the table and jump to
/// whatever follows it (often zero); these answer `E_NOTIMPL` and log.
const SPARE_SLOTS: usize = 64;

struct Spare {
    area: &'static str,
    slot: usize,
}

extern "C" fn spare_slot(spare: &Spare) -> i32 {
    tracing::warn!(
        area = spare.area,
        slot = format_args!("{:#x}", spare.slot * 8),
        "title called a method past the end of our table"
    );
    0x8000_4001_u32 as i32 // E_NOTIMPL
}

/// Raw table address to its bridged copy, and the reverse set of copies.
static TABLES: Mutex<Option<(std::collections::HashMap<usize, usize>, HashSet<usize>)>> =
    Mutex::new(None);

/// A copy of the method table at `raw` (`len` slots) that title code can
/// call: each slot is a bridge stub for the host function in it (null stays
/// null), followed by [`SPARE_SLOTS`] logging stubs. Cached per `raw`, so
/// every object sharing a table shares the copy. Leaked.
///
/// # Safety
/// `raw` points at `len` function addresses of the host.
pub unsafe fn bridged_table(
    raw: *const *const (),
    len: usize,
    area: &'static str,
) -> Result<*const *const ()> {
    let mut guard = TABLES.lock().unwrap_or_else(|e| e.into_inner());
    let (by_raw, copies) = guard.get_or_insert_default();
    if copies.contains(&(raw as usize)) {
        return Ok(raw);
    }
    if let Some(&copy) = by_raw.get(&(raw as usize)) {
        return Ok(copy as *const *const ());
    }
    // SAFETY: the caller's table.
    let slots = unsafe { std::slice::from_raw_parts(raw, len) };
    let targets: Vec<(usize, bool)> = slots
        .iter()
        .filter(|s| !s.is_null())
        .map(|&s| (s as usize, is_win64(s as usize)))
        .collect();
    // One stub per distinct target keeps AddRef, Release, `missing` shared.
    let mut unique: Vec<(usize, bool)> = targets.clone();
    unique.sort_unstable();
    unique.dedup();
    let tracing_on = trace_level() > 0 || profile_seconds().is_some() || callbacks_enabled();
    let mut block = StubBlock::new(unique.len() + len + SPARE_SLOTS)?;
    let mut stub_of = std::collections::HashMap::new();
    let mut copy: Vec<usize> = Vec::with_capacity(len + SPARE_SLOTS);
    if tracing_on {
        // One stub per slot, so the log can name the slot.
        for (slot, &s) in slots.iter().enumerate() {
            if s.is_null() {
                copy.push(0);
                continue;
            }
            let info = new_traced(area, slot, "");
            let ctx = info as *const Traced as usize | 1;
            let win64 = is_win64(s as usize);
            // win64 targets skip the remapping and cannot be traced.
            copy.push(block.emit(s as usize, if win64 { 0 } else { ctx }, win64)?);
        }
    } else {
        for &(target, win64) in &unique {
            stub_of.insert(target, block.emit(target, 0, win64)?);
        }
        copy.extend(
            slots
                .iter()
                .map(|&s| stub_of.get(&(s as usize)).copied().unwrap_or(0)),
        );
    }
    for i in 0..SPARE_SLOTS {
        let spare = Box::leak(Box::new(Spare {
            area,
            slot: len + i,
        }));
        copy.push(block.emit(
            spare_slot as *const () as usize,
            spare as *const Spare as usize,
            false,
        )?);
    }
    block.seal()?;
    let copy = Box::leak(copy.into_boxed_slice())
        .as_ptr()
        .cast::<*const ()>();
    by_raw.insert(raw as usize, copy as usize);
    copies.insert(copy as usize);
    Ok(copy)
}

/// The bridged copy of the method table at `raw`, if there is one.
pub fn bridged_copy_of(raw: *const *const ()) -> Option<*const *const ()> {
    let guard = TABLES.lock().unwrap_or_else(|e| e.into_inner());
    let (by_raw, _) = guard.as_ref()?;
    by_raw.get(&(raw as usize)).map(|&c| c as *const *const ())
}

/// Whether the method table at `table` is one of our bridged copies.
pub fn is_bridged(table: *const *const ()) -> bool {
    let guard = TABLES.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .is_some_and(|(_, copies)| copies.contains(&(table as usize)))
}

/// Gives the COM-style object at `this` (first word: its method table of
/// `len` slots) a table title code can call, in place. `this` stays the
/// object's address, so our methods find their state as before; only the
/// table word changes. Idempotent; does nothing before an image is mapped.
///
/// # Safety
/// `this` is a live object of ours whose first word is such a table
/// pointer, in writable memory.
pub unsafe fn expose_object(this: *const (), len: usize, area: &'static str) {
    // Nothing to hand it to (unit tests of the runtime call methods directly).
    if !super::seh::unwind::any_image() {
        return;
    }
    use std::sync::atomic::{AtomicPtr, Ordering};
    // SAFETY: the first word is a pointer-sized table pointer.
    let word = unsafe { &*this.cast::<AtomicPtr<*const ()>>() };
    let raw = word.load(Ordering::Acquire).cast_const();
    // SAFETY: forwarded.
    match unsafe { bridged_table(raw, len, area) } {
        Ok(copy) => word.store(copy.cast_mut(), Ordering::Release),
        Err(e) => tracing::error!(area, "cannot bridge a method table: {e:#}"),
    }
}

/// Executable memory holding one stub per host function bound to the title.
pub struct StubBlock {
    base: *mut u8,
    len: usize,
    used: usize,
}

impl StubBlock {
    pub fn new(count: usize) -> Result<Self> {
        let len = (count.max(1) * STUB_SIZE).next_multiple_of(mem::page_size());
        // SAFETY: a fresh anonymous allocation.
        let base = unsafe { mem::alloc(None, len, Protect::READ_WRITE) }?;
        Ok(StubBlock { base, len, used: 0 })
    }

    /// `mov r10, target; mov r11, context; mov rax, bridge; jmp rax`.
    pub fn emit(&mut self, target: usize, context: usize, win64: bool) -> Result<usize> {
        if self.used + STUB_SIZE > self.len {
            bail!("stub block is full");
        }
        let bridge = if win64 {
            victory_bridge_win64 as *const () as u64
        } else {
            victory_bridge_to_host as *const () as u64
        };
        let mut code = [0xCC_u8; STUB_SIZE]; // int3 padding
        code[0..2].copy_from_slice(&[0x49, 0xBA]);
        code[2..10].copy_from_slice(&(target as u64).to_le_bytes());
        code[10..12].copy_from_slice(&[0x49, 0xBB]);
        code[12..20].copy_from_slice(&(context as u64).to_le_bytes());
        code[20..22].copy_from_slice(&[0x48, 0xB8]);
        code[22..30].copy_from_slice(&bridge.to_le_bytes());
        code[30..32].copy_from_slice(&[0xFF, 0xE0]);
        // SAFETY: `used + STUB_SIZE <= len` and the block is still writable.
        let at = unsafe {
            let at = self.base.add(self.used);
            std::ptr::copy_nonoverlapping(code.as_ptr(), at, STUB_SIZE);
            at
        };
        self.used += STUB_SIZE;
        Ok(at as usize)
    }

    /// Makes the block executable. Leaked on purpose: the title keeps the
    /// addresses for the life of the process.
    pub fn seal(self) -> Result<()> {
        // SAFETY: the allocation made in `new`.
        unsafe {
            mem::protect(self.base, self.len, Protect::READ_EXECUTE)?;
            mem::flush_icache(self.base, self.len);
        }
        Ok(())
    }
}

/// A callable Windows x64 entry for the host function at `address`, as the
/// binder would put in an IAT slot: a fresh bridge stub, or `address` itself
/// if it needs none. For `GetProcAddress` on modules the host serves.
pub fn stub_for(address: usize) -> Result<usize> {
    if is_direct(address) {
        return Ok(address);
    }
    let mut block = StubBlock::new(1)?;
    let stub = block.emit(address, 0, is_win64(address))?;
    block.seal()?;
    Ok(stub)
}

// `VICTORY_TRACE_CALLBACKS=1`: every title code address passed to a host import or method (as an
// argument, or in the first 16 qwords an argument points at) is remembered with the import's
// name, and the host's calls into the title are remembered too; `report_callbacks` lists what was
// registered and never called.

/// Whether callbacks are being recorded.
pub(crate) fn callbacks_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TRACE_CALLBACKS").is_some())
}

/// Title code addresses the host has called.
static CALLED: Mutex<std::collections::BTreeSet<usize>> = Mutex::new(std::collections::BTreeSet::new());

/// Registered title code addresses: (import or method, argument, offset behind it, times seen).
static REGISTERED: Mutex<std::collections::BTreeMap<usize, (String, usize, usize, u64)>> =
    Mutex::new(std::collections::BTreeMap::new());

fn in_text(v: usize) -> bool {
    (0x1_4000_1000..0x1_45f0_0000).contains(&v)
}

fn note_callbacks(info: &Traced, args: *const u64) {
    // SAFETY: the bridge's saved register arguments, four words.
    let args = unsafe { std::slice::from_raw_parts(args, 4) };
    let name = if info.name.is_empty() {
        format!("{} method {:#x}", info.area, info.slot * 8)
    } else {
        format!("{}!{}", info.area, info.name)
    };
    let mut found = Vec::new();
    for (i, &a) in args.iter().enumerate() {
        let a = a as usize;
        if in_text(a) {
            found.push((a, i, 0));
        } else if a >= 0x10000 && a < 0x7fff_ffff_0000 && a & 7 == 0 && !info.area.contains("-crt-") {
            for k in 0..16 {
                match super::dump::read_checked(a + 8 * k) {
                    Some(v) if in_text(v) => found.push((v, i, 8 * k)),
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }
    if found.is_empty() {
        return;
    }
    let mut map = REGISTERED.lock().unwrap_or_else(|e| e.into_inner());
    for (target, arg, off) in found {
        let e = map.entry(target).or_insert((name.clone(), arg, off, 0));
        e.3 += 1;
    }
}

/// Logs what was registered and never called by the host.
pub fn report_callbacks() {
    if !callbacks_enabled() {
        return;
    }
    let called = CALLED.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let map = REGISTERED.lock().unwrap_or_else(|e| e.into_inner());
    let (mut never, mut yes) = (Vec::new(), 0);
    for (&t, (name, arg, off, n)) in map.iter() {
        if called.contains(&t) {
            yes += 1;
        } else {
            never.push(format!("{name} arg{arg}+{off:#x} x{n} -> {:#x}", t - 0x1_4000_0000));
        }
    }
    tracing::warn!("callbacks: {} registered, {yes} called by the host, {} never", map.len(), never.len());
    for line in never {
        tracing::warn!("never called: {line}");
    }
}
