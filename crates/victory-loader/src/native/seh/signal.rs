//! The signal handler: faults in title code become exceptions.
//!
//! The handler runs on the faulting thread (on its alternate stack, if
//! it has one, else below the signal frame) and does only this: tell title
//! code (GS = the TEB) from host code, copy the thread state into a
//! `CONTEXT` and an `EXCEPTION_RECORD` on the thread's own stack, and set
//! the saved state to enter `victory_exception_entry` there. Returning from
//! the handler is then the thread's jump into the dispatcher. A fault in
//! host code, or any other signal state, goes to the handler that was
//! installed before ours (Rust's stack-overflow handler, then the default).

use std::sync::Once;

use super::context::*;
use super::dispatch::entry_address;
use crate::native::teb::OFF_SELF;

#[repr(C)]
#[derive(Clone, Copy)]
struct SigAction {
    handler: usize,
    mask: u32,
    flags: i32,
}

unsafe extern "C" {
    fn sigaction(signal: i32, action: *const SigAction, old: *mut SigAction) -> i32;
}

const SIGILL: i32 = 4;
const SIGTRAP: i32 = 5;
const SIGFPE: i32 = 8;
const SIGBUS: i32 = 10;
const SIGSEGV: i32 = 11;
const SA_ONSTACK: i32 = 0x1;
const SA_SIGINFO: i32 = 0x40;
const SIG_DFL: usize = 0;
const SIG_IGN: usize = 1;
const SIGNALS: [i32; 5] = [SIGSEGV, SIGBUS, SIGILL, SIGFPE, SIGTRAP];

/// What each signal was set to before ours, for faults that are not ours.
static mut OLD: [SigAction; 32] = [SigAction {
    handler: 0,
    mask: 0,
    flags: 0,
}; 32];

/// Installs the handler, once per process.
pub fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for signal in SIGNALS {
            let action = SigAction {
                handler: on_signal as *const () as usize,
                mask: 0,
                flags: SA_SIGINFO | SA_ONSTACK,
            };
            // SAFETY: a valid action; the old one is saved for chaining,
            // before any signal can reach the handler.
            unsafe {
                let old = (&raw mut OLD).cast::<SigAction>().add(signal as usize);
                sigaction(signal, &action, old);
            }
        }
    });
}

/// `siginfo_t` fields we read.
const INFO_CODE: usize = 8;
const INFO_ADDR: usize = 24;
/// `ucontext_t`'s pointer to the machine context.
const UC_MCONTEXT: usize = 48;
// The machine context: exception state (16 bytes), thread state (21
// words), float state (an 8-byte header, then the FXSAVE image).
const MC_ERR: usize = 4;
const MC_FAULT_ADDR: usize = 8;
const MC_THREAD: usize = 16;
const MC_FLOAT: usize = 16 + 168;
// Thread state words.
const T_RDI: usize = 4;
const T_RSI: usize = 5;
const T_RSP: usize = 7;
const T_RIP: usize = 16;
const T_RFLAGS: usize = 17;

/// Whether GS is a TEB: slot 6 of the host's thread-specific data (the
/// TEB's `Self` offset) is zero in a host thread.
fn gs_is_teb() -> bool {
    let teb: usize;
    // SAFETY: reads the word at gs:[0x30], which exists in either layout.
    unsafe {
        std::arch::asm!("mov {}, gs:[0x30]", out(reg) teb, options(nostack, readonly, preserves_flags))
    };
    // SAFETY: a non-zero word there is a TEB, whose `Self` points back.
    teb != 0 && unsafe { ((teb + OFF_SELF) as *const usize).read() } == teb
}

/// For `clzero; add rax, 0x40; cmp rax, <reg>; jb clzero` (12 bytes), the
/// register number of the limit.
fn clzero_loop_limit(code: &[u8]) -> Option<usize> {
    let [
        0x0F,
        0x01,
        0xFC,
        0x48,
        0x83,
        0xC0,
        0x40,
        rex,
        0x39,
        modrm,
        0x72,
        0xF4,
        ..,
    ] = *code
    else {
        return None;
    };
    // `cmp r/m64, r64` with rm = rax and a 64-bit operand.
    let reg = usize::from(rex & 4) << 1 | usize::from(modrm >> 3 & 7);
    (rex & 0xFB == 0x48 && modrm >> 6 == 3 && modrm & 7 == 0 && rex & 1 == 0).then_some(reg)
}

/// The thread-state slot (macOS order: rax rbx rcx rdx rdi rsi rbp rsp r8..r15) of a
/// register by its x86 encoding number.
fn slot(reg: usize) -> usize {
    [0, 2, 3, 1, 7, 6, 5, 4].get(reg).copied().unwrap_or(reg)
}

/// For a cache line write-back loop, `clwb [p]` or `clflushopt [p]`; `add p, 0x40`;
/// `cmp p, limit`; `jbe` or `jb` back: the registers (encoding numbers) of `p` and the
/// limit, whether the limit is inclusive (`jbe`), and the loop's length. The title
/// flushes buffers of many megabytes this way, one signal per line without this.
fn cache_hint_loop(code: &[u8]) -> Option<(usize, usize, bool, usize)> {
    let hint = cache_hint_len(code)?;
    let rex_b = usize::from(hint == 5 && code[1] & 1 != 0);
    let modrm = code[hint - 1];
    if modrm >> 6 != 0 || matches!(modrm & 7, 4 | 5) {
        return None;
    }
    let p = modrm as usize & 7 | rex_b << 3;
    let rest = code.get(hint..hint + 9)?;
    let w = 0x48 | rex_b as u8;
    let [add_rex, 0x83, add, 0x40, cmp_rex, 0x39, cmp, jump, back] = *rest else {
        return None;
    };
    let limit = usize::from(cmp >> 3 & 7) | usize::from(cmp_rex & 4) << 1;
    let ok = add_rex == w
        && add == 0xC0 | (p & 7) as u8
        && cmp_rex & 0xFB == w
        && cmp >> 6 == 3
        && usize::from(cmp & 7) == p & 7
        && matches!(jump, 0x72 | 0x76)
        && back as i8 as isize == -((hint + 9) as isize);
    ok.then_some((p, limit, jump == 0x76, hint + 9))
}

/// Instructions emulated so far (`clzero`), for the thread dump.
pub static EMULATED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Runs an SSE4a `insertq` or `extrq` (AMD, not in Rosetta; register forms
/// only, as the instructions have no others) at the start of `code` on the
/// 16 XMM registers, and returns its length. The upper halves of the
/// destination are undefined after them; they are left as they were.
fn sse4a(code: &[u8], xmm: &mut [[u8; 16]; 16]) -> Option<usize> {
    let insert = match code.first()? {
        0xF2 => true,
        0x66 => false,
        _ => return None,
    };
    let mut i = 1;
    let rex = code
        .get(i)
        .filter(|b| *b & 0xF0 == 0x40)
        .copied()
        .unwrap_or(0);
    i += usize::from(rex != 0);
    let op = match code.get(i..i + 2)? {
        [0x0F, op @ (0x78 | 0x79)] => *op,
        _ => return None,
    };
    let modrm = *code.get(i + 2)?;
    if modrm >> 6 != 3 {
        return None;
    }
    let reg = usize::from(modrm >> 3 & 7 | (rex & 4) << 1);
    let rm = usize::from(modrm & 7 | (rex & 1) << 3);
    let low = |r: &[u8; 16]| u64::from_le_bytes(r[..8].try_into().unwrap());
    let (dst, len, idx, value, size) = match (insert, op) {
        (true, 0x78) => (
            reg,
            *code.get(i + 3)?,
            *code.get(i + 4)?,
            low(&xmm[rm]),
            i + 5,
        ),
        (true, _) => (reg, xmm[rm][8], xmm[rm][9], low(&xmm[rm]), i + 3),
        (false, 0x78) if reg & 7 == 0 => (rm, *code.get(i + 3)?, *code.get(i + 4)?, 0, i + 5),
        (false, 0x79) => (reg, xmm[rm][0], xmm[rm][1], 0, i + 3),
        _ => return None,
    };
    let (len, idx) = (u32::from(len & 63), u32::from(idx & 63));
    let mask = if len == 0 {
        u64::MAX
    } else {
        (1u64 << len) - 1
    };
    let old = low(&xmm[dst]);
    let new = if insert {
        old & !mask.wrapping_shl(idx) | (value & mask).wrapping_shl(idx)
    } else {
        old.wrapping_shr(idx) & mask
    };
    xmm[dst][..8].copy_from_slice(&new.to_le_bytes());
    Some(size)
}

/// Length of a `clwb` or `clflushopt` (66 [REX] 0F AE /6 or /7) at the start
/// of `code`. Rosetta does not implement them; they only write a cache line
/// back, so the title (which flushes memory the GPU reads) loses nothing
/// when they do nothing.
fn cache_hint_len(code: &[u8]) -> Option<usize> {
    let mut i = usize::from(code.first() == Some(&0x66));
    if i == 0 {
        return None;
    }
    if code.get(i).is_some_and(|b| b & 0xF0 == 0x40) {
        i += 1;
    }
    if code.get(i..i + 2) != Some(&[0x0F, 0xAE]) {
        return None;
    }
    let modrm = *code.get(i + 2)?;
    let (md, reg, rm) = (modrm >> 6, modrm >> 3 & 7, modrm & 7);
    if reg != 6 && reg != 7 || md == 3 {
        return None;
    }
    let mut len = i + 3;
    let sib_base = (md != 3 && rm == 4)
        .then(|| code.get(len).map(|s| s & 7))
        .flatten();
    if rm == 4 {
        len += 1;
    }
    len += match md {
        0 if rm == 5 || sib_base == Some(5) => 4,
        1 => 1,
        2 => 4,
        _ => 0,
    };
    Some(len)
}

/// # Safety
/// Called by the kernel with a valid `siginfo_t` and `ucontext_t`.
unsafe extern "C" fn on_signal(signal: i32, info: *mut u8, uc: *mut u8) {
    if !gs_is_teb() {
        // SAFETY: the same arguments.
        return unsafe { chain(signal, info, uc) };
    }
    // SAFETY: the kernel's machine context for this signal.
    let mc = unsafe { uc.add(UC_MCONTEXT).cast::<*mut u8>().read() };
    let word = |i: usize| -> u64 {
        // SAFETY: inside the thread state.
        unsafe { mc.add(MC_THREAD + i * 8).cast::<u64>().read() }
    };

    let mut ctx = Context::zeroed();
    ctx.context_flags = CONTEXT_FULL;
    let mut thread_state = [0u64; 21];
    for (i, w) in thread_state.iter_mut().enumerate() {
        *w = word(i);
    }
    ctx.set_from_thread_state(&thread_state);
    let rip = word(T_RIP);
    if signal == SIGILL {
        // SAFETY: rip is in the code that just trapped; 15 bytes is the
        // longest instruction and the page after is mapped or the read faults
        // like the instruction would have.
        let code = unsafe { std::slice::from_raw_parts(rip as *const u8, 15) };
        if code.starts_with(&[0x0F, 0x01, 0xFC]) {
            // clzero (AMD, not in Rosetta): zeroes the cache line at rax.
            EMULATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let set = |i: usize, v: u64| {
                // SAFETY: inside the thread state.
                unsafe { mc.add(MC_THREAD + i * 8).cast::<u64>().write(v) }
            };
            let mut next = rip + 3;
            let mut rax = word(0);
            let line = (rax & !63) as *mut u8;
            // SAFETY: the instruction would have written this line.
            unsafe { line.write_bytes(0, 64) };
            // The title's memory clears are `clzero; add rax, 0x40; cmp rax, r;
            // jb back` over thousands of lines: one signal per line is too
            // slow, so the whole loop is done here.
            if let Some(reg) = clzero_loop_limit(code) {
                let limit = word(slot(reg));
                rax += 64;
                while rax < limit {
                    // SAFETY: the loop would have written each line.
                    unsafe { ((rax & !63) as *mut u8).write_bytes(0, 64) };
                    rax += 64;
                }
                set(0, rax);
                next = rip + 12;
                // The flags of its last `cmp rax, limit`: not below.
                let zero = u64::from(rax == limit) << 6;
                set(17, word(17) & !0x8d5 | zero | 0x2);
            }
            set(16, next);
            return;
        }
        if code.starts_with(&[0x0F, 0x01, 0xFA]) || code.starts_with(&[0x0F, 0x01, 0xFB]) {
            // monitorx and mwaitx (AMD, not in Rosetta) wait for a write to a
            // watched line, and may wake for any reason: the caller rereads
            // its flag, so yielding is a wait.
            if code[2] == 0xFB {
                std::thread::yield_now();
            }
            // SAFETY: the saved rip, past the instruction.
            unsafe { mc.add(MC_THREAD + T_RIP * 8).cast::<u64>().write(rip + 3) };
            return;
        }
        // The XMM registers in the float state's FXSAVE image (after its 8-byte header).
        // SAFETY: 16 registers of 16 bytes at offset 160 of the image.
        let xmm = unsafe { &mut *mc.add(MC_FLOAT + 8 + 160).cast::<[[u8; 16]; 16]>() };
        if let Some(len) = sse4a(code, xmm) {
            EMULATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // SAFETY: the saved rip, past the instruction.
            unsafe {
                mc.add(MC_THREAD + T_RIP * 8)
                    .cast::<u64>()
                    .write(rip + len as u64)
            };
            return;
        }
        if let Some((p, limit, inclusive, len)) = cache_hint_loop(code) {
            // Write-backs do nothing here: the loop ends where it would have.
            let (start, limit) = (word(slot(p)), word(slot(limit)));
            let mut end = start.wrapping_add(64);
            if inclusive && limit >= end {
                end += (limit - end) / 64 * 64 + 64;
            } else if !inclusive && limit > end {
                end += (limit - end).div_ceil(64) * 64;
            }
            // SAFETY: inside the thread state.
            unsafe {
                let set = |i: usize, v: u64| mc.add(MC_THREAD + i * 8).cast::<u64>().write(v);
                set(slot(p), end);
                // The flags of its last `cmp p, limit`: not below, equal or above.
                set(
                    T_RFLAGS,
                    word(T_RFLAGS) & !0x8d5 | u64::from(end == limit) << 6 | 0x2,
                );
                set(T_RIP, rip + len as u64);
            }
            return;
        }
        if let Some(len) = cache_hint_len(code) {
            // SAFETY: the saved rip, past the hint.
            unsafe {
                mc.add(MC_THREAD + T_RIP * 8)
                    .cast::<u64>()
                    .write(rip + len as u64)
            };
            return;
        }
    }
    // The float state's FXSAVE image is `XMM_SAVE_AREA32`.
    // SAFETY: 512 bytes of float state follow the 8-byte header.
    unsafe {
        std::ptr::copy_nonoverlapping(mc.add(MC_FLOAT + 8), ctx.flt.as_mut_ptr().cast::<u8>(), 512)
    };
    let mxcsr = u32::from_le_bytes([ctx.flt[1][8], ctx.flt[1][9], ctx.flt[1][10], ctx.flt[1][11]]);
    ctx.mx_csr = mxcsr;

    // SAFETY: fields of the signal's own records.
    let (code, err, fault_addr, si_addr) = unsafe {
        (
            info.add(INFO_CODE).cast::<i32>().read(),
            mc.add(MC_ERR).cast::<u32>().read(),
            mc.add(MC_FAULT_ADDR).cast::<u64>().read(),
            info.add(INFO_ADDR).cast::<usize>().read(),
        )
    };
    let mut rec = ExceptionRecord::new(0, 0, rip as usize);
    match signal {
        SIGSEGV | SIGBUS => {
            let at = if fault_addr != 0 {
                fault_addr as usize
            } else {
                si_addr
            };
            rec.code = EXCEPTION_ACCESS_VIOLATION;
            rec.number_parameters = 2;
            // 0 read, 1 write (bit 1 of the page-fault error code), 8 execute.
            rec.information[0] = if at == rip as usize {
                8
            } else {
                usize::from(err & 2 != 0)
            };
            rec.information[1] = at;
        }
        SIGILL => rec.code = EXCEPTION_ILLEGAL_INSTRUCTION,
        SIGFPE => {
            // macOS's `si_code` values (not Linux's).
            rec.code = match code {
                7 => EXCEPTION_INT_DIVIDE_BY_ZERO,
                8 => EXCEPTION_INT_OVERFLOW,
                1 => EXCEPTION_FLT_DIVIDE_BY_ZERO,
                2 => EXCEPTION_FLT_OVERFLOW,
                3 => EXCEPTION_FLT_UNDERFLOW,
                4 => EXCEPTION_FLT_INEXACT_RESULT,
                6 => EXCEPTION_ARRAY_BOUNDS_EXCEEDED,
                _ => EXCEPTION_FLT_INVALID_OPERATION,
            };
        }
        _ => {
            // `int3` leaves rip after it; Windows reports its address.
            // SAFETY: rip-1 is in the code that just trapped.
            let int3 = rip != 0 && unsafe { ((rip - 1) as *const u8).read() } == 0xCC;
            if int3 {
                rec.code = EXCEPTION_BREAKPOINT;
                rec.address = rip as usize - 1;
                // Windows x64 reports the int3 itself in the CONTEXT too; a
                // handler that continues must step past it.
                ctx.rip = rip - 1;
                rec.number_parameters = 1;
            } else {
                rec.code = EXCEPTION_SINGLE_STEP;
            }
        }
    }

    // Where to put them: below the interrupted rsp (Windows code has no red
    // zone, but ours might), and below this handler's own frame if it runs
    // on that stack.
    let rsp = word(T_RSP) as usize;
    let marker = 0u8;
    let here = std::ptr::from_ref(&marker) as usize;
    let mut top = rsp.wrapping_sub(0x100);
    if here < rsp && rsp - here < 0x2_0000 {
        top = top.min(here - 0x2000);
    }
    let ctx_at = (top - size_of::<Context>()) & !0xF;
    let rec_at = (ctx_at - size_of::<ExceptionRecord>()) & !0xF;
    let entry_rsp = (rec_at - 0x20) & !0xF;
    // SAFETY: the thread's own stack below its rsp.
    unsafe {
        (ctx_at as *mut Context).write(ctx);
        (rec_at as *mut ExceptionRecord).write(rec);
        let set = |i: usize, v: u64| mc.add(MC_THREAD + i * 8).cast::<u64>().write(v);
        set(T_RIP, entry_address() as u64);
        set(T_RSP, entry_rsp as u64);
        set(T_RDI, ctx_at as u64);
        set(T_RSI, rec_at as u64);
        // Direction flag clear, as the ABI wants at an entry.
        set(T_RFLAGS, word(T_RFLAGS) & !0x400);
    }
}

/// Hands a signal that is not ours to the handler that was there before.
///
/// # Safety
/// Called from `on_signal` with its arguments.
unsafe fn chain(signal: i32, info: *mut u8, uc: *mut u8) {
    // SAFETY: written once at install, before any signal.
    let old = unsafe {
        (&raw const OLD)
            .cast::<SigAction>()
            .add(signal as usize)
            .read()
    };
    match old.handler {
        // Back to the default and return: the fault happens again and ends
        // the process.
        SIG_DFL => {
            let default = SigAction {
                handler: SIG_DFL,
                mask: 0,
                flags: 0,
            };
            // SAFETY: a valid action.
            unsafe { sigaction(signal, &default, std::ptr::null_mut()) };
        }
        SIG_IGN => {}
        handler if old.flags & SA_SIGINFO != 0 => {
            // SAFETY: the previous handler, called as the kernel would.
            let f: unsafe extern "C" fn(i32, *mut u8, *mut u8) =
                unsafe { std::mem::transmute(handler) };
            // SAFETY: as above.
            unsafe { f(signal, info, uc) };
        }
        handler => {
            // SAFETY: a plain handler.
            let f: unsafe extern "C" fn(i32) = unsafe { std::mem::transmute(handler) };
            // SAFETY: as above.
            unsafe { f(signal) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{cache_hint_len, cache_hint_loop, clzero_loop_limit, sse4a};

    #[test]
    fn cache_hint_loops_are_recognised() {
        // clwb [rax]; add rax, 0x40; cmp rax, rcx; jbe back
        let rax = [
            0x66, 0x0F, 0xAE, 0x30, 0x48, 0x83, 0xC0, 0x40, 0x48, 0x39, 0xC8, 0x76, 0xF3,
        ];
        assert_eq!(cache_hint_loop(&rax), Some((0, 1, true, 13)));
        // clwb [r9]; add r9, 0x40; cmp r9, rax; jbe back
        let r9 = [
            0x66, 0x41, 0x0F, 0xAE, 0x31, 0x49, 0x83, 0xC1, 0x40, 0x49, 0x39, 0xC1, 0x76, 0xF2,
        ];
        assert_eq!(cache_hint_loop(&r9), Some((9, 0, true, 14)));
        let mut other = rax;
        other[12] = 0xF0; // jumps elsewhere
        assert_eq!(cache_hint_loop(&other), None);
        assert_eq!(cache_hint_loop(&rax[..12]), None);
    }

    #[test]
    fn sse4a_inserts_and_extracts() {
        let mut xmm = [[0u8; 16]; 16];
        xmm[0] = [0xFF; 16];
        xmm[1][..8].copy_from_slice(&0x1234u64.to_le_bytes());
        // insertq xmm0, xmm1, 0x10, 0x10: bits 16..32 of xmm0 = low 16 of xmm1.
        assert_eq!(
            sse4a(&[0xF2, 0x0F, 0x78, 0xC1, 0x10, 0x10, 0x90], &mut xmm),
            Some(6)
        );
        assert_eq!(xmm[0][..8], 0xFFFF_FFFF_1234_FFFFu64.to_le_bytes());
        assert_eq!(xmm[0][8..], [0xFF; 8]);
        // extrq xmm0, 8, 16 (66 0F 78 /0): bits 16..24 to the bottom.
        assert_eq!(
            sse4a(&[0x66, 0x0F, 0x78, 0xC0, 0x08, 0x10], &mut xmm),
            Some(6)
        );
        assert_eq!(xmm[0][..8], 0x34u64.to_le_bytes());
        // insertq xmm9, xmm2 (F2 44 0F 79 CA): length and index from xmm2[64..80].
        xmm[2] = [0; 16];
        xmm[2][..8].copy_from_slice(&0xABu64.to_le_bytes());
        xmm[2][8] = 8;
        xmm[2][9] = 4;
        assert_eq!(sse4a(&[0xF2, 0x44, 0x0F, 0x79, 0xCA], &mut xmm), Some(5));
        assert_eq!(xmm[9][..8], 0xAB0u64.to_le_bytes());
        // A length of 0 means 64.
        assert_eq!(
            sse4a(&[0xF2, 0x0F, 0x78, 0xD9, 0x00, 0x00], &mut xmm),
            Some(6)
        );
        assert_eq!(xmm[3][..8], 0x1234u64.to_le_bytes());
        assert_eq!(sse4a(&[0xF2, 0x0F, 0x78, 0x01, 0x10, 0x10], &mut xmm), None); // memory form
        assert_eq!(sse4a(&[0x66, 0x0F, 0x78, 0xC8, 0x10, 0x10], &mut xmm), None); // extrq /1
    }

    #[test]
    fn the_clzero_loop_is_recognised() {
        let code = [
            0x0F, 0x01, 0xFC, 0x48, 0x83, 0xC0, 0x40, 0x4C, 0x39, 0xD0, 0x72, 0xF4,
        ];
        assert_eq!(clzero_loop_limit(&code), Some(10)); // r10
        let rdx = [
            0x0F, 0x01, 0xFC, 0x48, 0x83, 0xC0, 0x40, 0x48, 0x39, 0xD0, 0x72, 0xF4,
        ];
        assert_eq!(clzero_loop_limit(&rdx), Some(2));
        assert_eq!(clzero_loop_limit(&code[..3]), None);
    }

    #[test]
    fn cache_hints_are_measured() {
        assert_eq!(
            cache_hint_len(&[0x66, 0x41, 0x0F, 0xAE, 0x31, 0x49]),
            Some(5)
        ); // clwb [r9]
        assert_eq!(
            cache_hint_len(&[0x66, 0x0F, 0xAE, 0x74, 0x24, 0x08]),
            Some(6)
        ); // clwb [rsp+8]
        assert_eq!(
            cache_hint_len(&[0x66, 0x0F, 0xAE, 0xB0, 1, 0, 0, 0]),
            Some(8)
        ); // disp32
        assert_eq!(
            cache_hint_len(&[0x66, 0x0F, 0xAE, 0x35, 1, 0, 0, 0]),
            Some(8)
        ); // rip+disp32
        assert_eq!(cache_hint_len(&[0x0F, 0xAE, 0x31]), None); // no 66: not ours
        assert_eq!(cache_hint_len(&[0x66, 0x0F, 0xAE, 0xF0]), None); // register form
    }
}
