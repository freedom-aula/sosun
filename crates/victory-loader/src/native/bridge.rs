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

use std::collections::HashSet;
use std::sync::Mutex;

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
    "sub rsp, 0xF0",
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
    "test r14, r14",
    "cmovnz rdi, r14",
    "xor eax, eax",
    "call r13",
    "mov r13, rax",
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
    "add rsp, 0xF0",
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
    "sub rsp, 0x120",
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
    "add rsp, 0x120",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rsi",
    "pop rdi",
    "pop rbx",
    "pop rbp",
    "ret",
    // victory_call_win64(f: rdi, args: rsi, nargs: rdx, teb: rcx) -> rax
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
);

unsafe extern "C" {
    fn victory_bridge_to_host();
    fn victory_bridge_win64();
    fn victory_call_win64(f: usize, args: *const u64, nargs: usize, teb: *mut u8) -> u64;
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
    assert!(args.len() <= MAX_ARGS, "too many arguments");
    let mut padded = [0u64; MAX_ARGS];
    padded[..args.len()].copy_from_slice(args);
    // SAFETY: forwarded from the caller; `padded` holds at least 4 words.
    unsafe { victory_call_win64(f, padded.as_ptr(), args.len(), teb::current()) }
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
