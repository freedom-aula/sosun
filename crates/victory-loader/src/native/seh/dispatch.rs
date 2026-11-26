//! The exception dispatcher: vectored handlers, structured handling over
//! the mapped images' unwind data, the unhandled-exception filter, and the
//! jump back into title code with a `CONTEXT`.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::context::*;
use super::unwind::{self, Bounds, RuntimeFunction, UNW_FLAG_EHANDLER, UNW_FLAG_UHANDLER};
use crate::native::bridge::{SET_GS_BASE, call_win64};
use crate::native::teb::{self, OFF_HOST_TSD, OFF_STACK_BASE, OFF_STACK_LIMIT};

/// A handler: a title's function (Windows ABI, run with the TEB) or ours.
#[derive(Clone, Copy)]
pub enum Handler {
    Title(usize),
    Host(extern "C" fn(*mut ExceptionPointers) -> i32),
}

impl Handler {
    /// # Safety
    /// `ptrs` must be valid for the call.
    unsafe fn call(self, ptrs: *mut ExceptionPointers) -> i32 {
        match self {
            // SAFETY: a title function of one pointer argument.
            Handler::Title(f) => unsafe { call_win64(f, &[ptrs as u64]) as u32 as i32 },
            Handler::Host(f) => f(ptrs),
        }
    }
}

struct Vectored {
    id: usize,
    handler: Handler,
}

static VECTORED: Mutex<Vec<Vectored>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
static UNHANDLED: Mutex<Option<Handler>> = Mutex::new(None);

/// `AddVectoredExceptionHandler`: `first` puts it ahead of the others.
/// Returns the handle to remove it with.
pub fn add_vectored_handler(first: bool, handler: Handler) -> usize {
    super::install();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed) << 4;
    let mut list = VECTORED.lock().unwrap_or_else(|e| e.into_inner());
    let entry = Vectored { id, handler };
    if first {
        list.insert(0, entry);
    } else {
        list.push(entry);
    }
    id
}

pub fn remove_vectored_handler(id: usize) -> bool {
    let mut list = VECTORED.lock().unwrap_or_else(|e| e.into_inner());
    let before = list.len();
    list.retain(|v| v.id != id);
    list.len() != before
}

/// `SetUnhandledExceptionFilter`; returns the previous filter.
pub fn set_unhandled_filter(handler: Option<Handler>) -> Option<Handler> {
    std::mem::replace(
        &mut *UNHANDLED.lock().unwrap_or_else(|e| e.into_inner()),
        handler,
    )
}

/// One exception being dispatched on this thread. The dispatcher's own
/// frame address tells when the dispatch has been unwound past.
struct Active {
    ctx: *mut Context,
    frame_addr: usize,
}

thread_local! {
    static ACTIVE: RefCell<Vec<Active>> = const { RefCell::new(Vec::new()) };
}

core::arch::global_asm!(
    // Where a faulting thread is sent: rdi = CONTEXT, rsi = EXCEPTION_RECORD,
    // GS is the TEB, rsp is 16-byte aligned.
    ".p2align 4",
    ".globl _victory_exception_entry",
    "_victory_exception_entry:",
    "mov rbx, rdi",
    "mov r12, rsi",
    "mov r13, gs:[0x30]",
    "mov rdi, [r13 + {host_tsd}]",
    "mov eax, {set_gs}",
    "syscall",
    "mov rdi, rbx",
    "mov rsi, r12",
    "mov rdx, r13",
    "call {dispatch}",
    "ud2",
    // victory_restore_context(ctx: rdi, teb: rsi) -> !: GS to the TEB, then
    // every register of the CONTEXT, and on to its rip.
    ".p2align 4",
    ".globl _victory_restore_context",
    "_victory_restore_context:",
    "mov rbx, rdi",
    "mov rdi, rsi",
    "mov eax, {set_gs}",
    "syscall",
    "ldmxcsr dword ptr [rbx + 0x34]",
    "movaps xmm0, [rbx + 0x1A0]",
    "movaps xmm1, [rbx + 0x1B0]",
    "movaps xmm2, [rbx + 0x1C0]",
    "movaps xmm3, [rbx + 0x1D0]",
    "movaps xmm4, [rbx + 0x1E0]",
    "movaps xmm5, [rbx + 0x1F0]",
    "movaps xmm6, [rbx + 0x200]",
    "movaps xmm7, [rbx + 0x210]",
    "movaps xmm8, [rbx + 0x220]",
    "movaps xmm9, [rbx + 0x230]",
    "movaps xmm10, [rbx + 0x240]",
    "movaps xmm11, [rbx + 0x250]",
    "movaps xmm12, [rbx + 0x260]",
    "movaps xmm13, [rbx + 0x270]",
    "movaps xmm14, [rbx + 0x280]",
    "movaps xmm15, [rbx + 0x290]",
    // The target's rip and flags go on its stack, below its rsp.
    "mov rax, [rbx + 0x98]",
    "mov r10, [rbx + 0xF8]",
    "mov r11d, [rbx + 0x44]",
    "mov [rax - 8], r10",
    "mov [rax - 16], r11",
    "lea rsp, [rax - 16]",
    "mov rax, [rbx + 0x78]",
    "mov rcx, [rbx + 0x80]",
    "mov rdx, [rbx + 0x88]",
    "mov rbp, [rbx + 0xA0]",
    "mov rsi, [rbx + 0xA8]",
    "mov rdi, [rbx + 0xB0]",
    "mov r8, [rbx + 0xB8]",
    "mov r9, [rbx + 0xC0]",
    "mov r12, [rbx + 0xD8]",
    "mov r13, [rbx + 0xE0]",
    "mov r14, [rbx + 0xE8]",
    "mov r15, [rbx + 0xF0]",
    "mov r10, [rbx + 0xC8]",
    "mov r11, [rbx + 0xD0]",
    "mov rbx, [rbx + 0x90]",
    "popfq",
    "ret",
    // RtlCaptureContext(rcx: CONTEXT*), Windows ABI, called by title code.
    ".p2align 4",
    ".globl _victory_RtlCaptureContext",
    "_victory_RtlCaptureContext:",
    "mov [rcx + 0x78], rax",
    "mov [rcx + 0x80], rcx",
    "mov [rcx + 0x88], rdx",
    "mov [rcx + 0x90], rbx",
    "lea rax, [rsp + 8]",
    "mov [rcx + 0x98], rax",
    "mov [rcx + 0xA0], rbp",
    "mov [rcx + 0xA8], rsi",
    "mov [rcx + 0xB0], rdi",
    "mov [rcx + 0xB8], r8",
    "mov [rcx + 0xC0], r9",
    "mov [rcx + 0xC8], r10",
    "mov [rcx + 0xD0], r11",
    "mov [rcx + 0xD8], r12",
    "mov [rcx + 0xE0], r13",
    "mov [rcx + 0xE8], r14",
    "mov [rcx + 0xF0], r15",
    "mov rax, [rsp]",
    "mov [rcx + 0xF8], rax",
    "pushfq",
    "pop rax",
    "mov [rcx + 0x44], eax",
    "stmxcsr dword ptr [rcx + 0x34]",
    "stmxcsr dword ptr [rcx + 0x118]",
    "movaps [rcx + 0x1A0], xmm0",
    "movaps [rcx + 0x1B0], xmm1",
    "movaps [rcx + 0x1C0], xmm2",
    "movaps [rcx + 0x1D0], xmm3",
    "movaps [rcx + 0x1E0], xmm4",
    "movaps [rcx + 0x1F0], xmm5",
    "movaps [rcx + 0x200], xmm6",
    "movaps [rcx + 0x210], xmm7",
    "movaps [rcx + 0x220], xmm8",
    "movaps [rcx + 0x230], xmm9",
    "movaps [rcx + 0x240], xmm10",
    "movaps [rcx + 0x250], xmm11",
    "movaps [rcx + 0x260], xmm12",
    "movaps [rcx + 0x270], xmm13",
    "movaps [rcx + 0x280], xmm14",
    "movaps [rcx + 0x290], xmm15",
    "mov dword ptr [rcx + 0x30], {flags}",
    "mov word ptr [rcx + 0x38], 0x33",
    "mov word ptr [rcx + 0x3A], 0x2B",
    "mov word ptr [rcx + 0x3C], 0x2B",
    "mov word ptr [rcx + 0x3E], 0x53",
    "mov word ptr [rcx + 0x40], 0x2B",
    "mov word ptr [rcx + 0x42], 0x2B",
    "ret",
    // RaiseException(rcx: code, edx: flags, r8d: count, r9: arguments),
    // Windows ABI, called by title code: captures the caller's context and
    // dispatches it.
    ".p2align 4",
    ".globl _victory_RaiseException",
    "_victory_RaiseException:",
    "sub rsp, 0x4F8",
    "mov [rsp], rcx",
    "mov [rsp + 8], rdx",
    "mov [rsp + 0x10], r8",
    "mov [rsp + 0x18], r9",
    "lea rcx, [rsp + 0x20]",
    "call _victory_RtlCaptureContext",
    "lea rax, [rsp + 0x500]",
    "mov [rsp + 0xB8], rax",
    "mov rax, [rsp + 0x4F8]",
    "mov [rsp + 0x118], rax",
    "mov r13, gs:[0x30]",
    "mov rdi, [r13 + {host_tsd}]",
    "mov eax, {set_gs}",
    "syscall",
    "lea rdi, [rsp + 0x20]",
    "mov rsi, [rsp]",
    "mov rdx, [rsp + 8]",
    "mov rcx, [rsp + 0x10]",
    "mov r8, [rsp + 0x18]",
    "mov r9, r13",
    "call {raise}",
    "ud2",
    // DebugBreak(), Windows ABI: RaiseException(EXCEPTION_BREAKPOINT, 0, 0, NULL), a tail call
    // so the context captured is the caller's, at the return address.
    ".p2align 4",
    ".globl _victory_DebugBreak",
    "_victory_DebugBreak:",
    "mov ecx, 0x80000003",
    "xor edx, edx",
    "xor r8d, r8d",
    "xor r9d, r9d",
    "jmp _victory_RaiseException",
    host_tsd = const OFF_HOST_TSD,
    set_gs = const SET_GS_BASE,
    flags = const CONTEXT_FULL,
    dispatch = sym dispatch_exception,
    raise = sym raise_exception,
);

unsafe extern "C" {
    fn victory_exception_entry();
    fn victory_restore_context(ctx: *const Context, teb: *mut u8) -> !;
    fn victory_RtlCaptureContext();
    fn victory_RaiseException();
    fn victory_DebugBreak();
}

/// Where a faulting thread is redirected.
pub(super) fn entry_address() -> usize {
    victory_exception_entry as *const () as usize
}

/// `RtlCaptureContext` and `RaiseException` for the title: the functions
/// that read the caller's registers, in the Windows ABI, bound with no
/// bridge.
pub fn capture_context_address() -> usize {
    victory_RtlCaptureContext as *const () as usize
}

pub fn raise_exception_address() -> usize {
    victory_RaiseException as *const () as usize
}

/// `DebugBreak` for the title: `EXCEPTION_BREAKPOINT` through `RaiseException`, so a vectored
/// handler or `__except` takes it and, with none, it is unhandled, as on Windows.
///
/// Guess (`docs/native-host.md`): Windows runs an `int3` inside `DbgBreakPoint`, so its record
/// names that address and has one parameter (0); here the address is the caller's return
/// address, there are none, and a handler that continues resumes after the call.
pub fn debug_break_address() -> usize {
    victory_DebugBreak as *const () as usize
}

fn stack_bounds() -> Bounds {
    let teb = teb::current();
    // SAFETY: inside this thread's TEB block.
    unsafe {
        Some((
            teb.add(OFF_STACK_LIMIT).cast::<usize>().read(),
            teb.add(OFF_STACK_BASE).cast::<usize>().read(),
        ))
    }
}

/// The return addresses up the stack from `start` by the unwind tables (the
/// first entry is its `rip`), ending at the first frame they cannot follow.
pub fn backtrace(start: Context, bounds: Bounds) -> Vec<usize> {
    let mut u = start;
    let mut chain = vec![u.rip as usize];
    for _ in 0..24 {
        let pc = if chain.len() == 1 { u.rip } else { u.rip.wrapping_sub(1) } as usize;
        if !stack_ok(&u, bounds) {
            break;
        }
        match unwind::lookup(pc) {
            Some((b, entry)) => {
                // SAFETY: an entry of an image we mapped.
                if unsafe { unwind::virtual_unwind(0, b, pc, entry, &mut u, bounds) }.is_none() {
                    break;
                }
            }
            None => {
                if unwind::image_base_of(pc).is_none() {
                    break;
                }
                let rsp = u.gpr[RSP];
                u.rip = read_stack(rsp, bounds);
                u.gpr[RSP] = rsp + 8;
            }
        }
        if u.rip == 0 {
            break;
        }
        chain.push(u.rip as usize);
    }
    chain
}

/// Continues at `ctx`: sets the TEB as GS and every register, and leaves
/// the dispatch (and any nested above it) behind.
///
/// # Safety
/// `ctx` must describe code to run on this thread's stack.
unsafe fn resume(ctx: *const Context) -> ! {
    // SAFETY: a valid context, per the caller.
    let rsp = unsafe { (*ctx).gpr[RSP] } as usize;
    ACTIVE.with(|a| a.borrow_mut().retain(|x| x.frame_addr > rsp));
    // SAFETY: as above.
    unsafe { victory_restore_context(ctx, teb::current()) }
}

/// Runs on the host's GS base, entered from `victory_exception_entry`.
extern "C" fn dispatch_exception(ctx: *mut Context, rec: *mut ExceptionRecord, _teb: usize) -> ! {
    let marker = 0u8;
    let frame_addr = std::ptr::from_ref(&marker) as usize;
    ACTIVE.with(|a| a.borrow_mut().push(Active { ctx, frame_addr }));
    // SAFETY: a context and record the entry stub was given.
    unsafe { dispatch(ctx, rec) }
}

/// `RaiseException`'s dispatch, with the caller's captured `ctx`.
extern "C" fn raise_exception(
    ctx: *mut Context,
    code: u64,
    flags: u64,
    count: u64,
    args: *const usize,
    _teb: usize,
) -> ! {
    // SAFETY: the context was just captured on this thread's stack.
    let rip = unsafe { (*ctx).rip } as usize;
    let mut record = ExceptionRecord::new(code as u32, flags as u32, rip);
    let count = (count as usize).min(15);
    record.number_parameters = count as u32;
    if !args.is_null() {
        // SAFETY: the title gives `count` arguments.
        unsafe { std::ptr::copy_nonoverlapping(args, record.information.as_mut_ptr(), count) };
    }
    dispatch_exception(ctx, &mut record, 0)
}

/// # Safety
/// `ctx` and `rec` stay valid until the thread is resumed.
unsafe fn dispatch(ctx: *mut Context, rec: *mut ExceptionRecord) -> ! {
    // SAFETY: both are ours for this dispatch.
    if unsafe { super::super::dump::write_trap(&*ctx, &*rec) } {
        // SAFETY: forwarded from the caller.
        unsafe { resume(ctx) };
    }
    // First-chance log: the title's own handlers may hide where a fault started.
    // SAFETY: both are ours for this dispatch.
    unsafe {
        let (c, r) = (&*ctx, &*rec);
        if r.code != 0x406d_1388 && r.code != 0xe06d_7363 {
            let g = &c.gpr;
            tracing::warn!(
                code = format_args!("{:#010x}", r.code),
                at = format_args!("{:#x}", c.rip),
                image = format_args!("{:#x?}", unwind::image_base_of(c.rip as usize).map(|b| c.rip as usize - b)),
                info = format_args!("{:x?}", &r.information[..2]),
                rax = format_args!("{:#x}", g[0]), rcx = format_args!("{:#x}", g[1]),
                rdx = format_args!("{:#x}", g[2]), rsp = format_args!("{:#x}", g[4]),
                r8 = format_args!("{:#x}", g[8]), r10 = format_args!("{:#x}", g[10]),
                "first-chance exception"
            );
            // VICTORY_DUMP_THREADS=0: at a breakpoint (the title's asserts). An access violation
            // waits for `unhandled`: the title's handlers take some and go on.
            super::super::dump::report_at_fault(r.code, false);
            // Debugging aid: VICTORY_PEEK=rva,rva,... prints those qwords of
            // the faulting image.
            if let (Ok(list), Some(base)) = (
                std::env::var("VICTORY_PEEK"),
                unwind::image_base_of(c.rip as usize),
            ) {
                for rva in list.split(',').filter_map(|v| usize::from_str_radix(v.trim(), 16).ok()) {
                    // Beyond the image: an absolute address.
                    let at = if rva > 0x1000_0000 { rva } else { base + rva };
                    let v = (at as *const u64).read_unaligned();
                    tracing::warn!("peek {at:#x} = {v:#x}");
                }
                // The qwords rcx points at, when it looks like a pointer.
                if (0x10000..0x7fff_ffff_0000).contains(&c.gpr[1]) {
                    let words: Vec<u64> = (0..8)
                        .map(|i| ((c.gpr[1] + 8 * i) as *const u64).read_unaligned())
                        .collect();
                    tracing::warn!("[rcx] {words:x?}");
                }
                let chain: Vec<usize> = backtrace(*c, stack_bounds())
                    .into_iter()
                    .map(|pc| pc.wrapping_sub(base))
                    .collect();
                tracing::warn!("call chain image+ {chain:x?}");
            }
        }
    }
    let mut ptrs = ExceptionPointers {
        record: rec,
        context: ctx,
    };
    let list: Vec<Handler> = VECTORED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|v| v.handler)
        .collect();
    for handler in list {
        // SAFETY: `ptrs` is ours and valid for the call.
        if unsafe { handler.call(&mut ptrs) } == EXCEPTION_CONTINUE_EXECUTION {
            // SAFETY: forwarded from the caller.
            unsafe { resume(ctx) };
        }
    }
    // SAFETY: forwarded from the caller.
    unsafe { walk_frames(ctx, rec) };
    // SAFETY: forwarded from the caller.
    unsafe { unhandled(ctx, rec) }
}

/// The first pass: asks each frame's language handler, from the faulting
/// frame out, to take the exception. Returns if nobody does.
unsafe fn walk_frames(ctx: *mut Context, rec: *mut ExceptionRecord) {
    let bounds = stack_bounds();
    // SAFETY: a valid context.
    let mut c = unsafe { *ctx };
    let mut first = true;
    for _ in 0..4096 {
        let pc = if first { c.rip } else { c.rip.wrapping_sub(1) } as usize;
        if !stack_ok(&c, bounds) {
            return;
        }
        let mut frame_ctx = c;
        let found = unwind::lookup(pc);
        let handler = match found {
            Some((base, entry)) => {
                // SAFETY: an entry of an image we mapped.
                let Some(u) = (unsafe {
                    unwind::virtual_unwind(UNW_FLAG_EHANDLER, base, pc, entry, &mut c, bounds)
                }) else {
                    return;
                };
                Some((base, entry, u))
            }
            None => {
                // A leaf function has no table entry: its return address is
                // at rsp. Anywhere but in one of our images (host code,
                // a thunk) the walk ends.
                if unwind::image_base_of(pc).is_none() {
                    return;
                }
                let rsp = c.gpr[RSP];
                c.rip = read_stack(rsp, bounds);
                c.gpr[RSP] = rsp + 8;
                None
            }
        };
        if let Some((base, entry, u)) = handler
            && u.handler != 0
        {
            let mut dc = dispatcher_context(base, pc, entry, u, std::ptr::null_mut());
            let pass: *mut Context = if first { ctx } else { &mut frame_ctx };
            dc.context_record = pass;
            // SAFETY: a title language handler: four pointer arguments.
            let disposition = unsafe {
                call_win64(
                    u.handler,
                    &[rec as u64, u.frame, pass as u64, &raw mut dc as u64],
                ) as u32
            };
            if disposition == EXCEPTION_CONTINUE_EXECUTION_DISPOSITION {
                // SAFETY: valid for the whole dispatch.
                if unsafe { (*rec).flags } & EXCEPTION_NONCONTINUABLE != 0 {
                    // SAFETY: as above.
                    unsafe { noncontinuable(ctx, rec) };
                }
                // SAFETY: as above.
                unsafe { resume(ctx) };
            }
        }
        c.context_flags |= CONTEXT_UNWOUND_TO_CALL;
        first = false;
        if c.rip == 0 {
            return;
        }
    }
}

fn dispatcher_context(
    base: usize,
    pc: usize,
    entry: *const RuntimeFunction,
    u: unwind::Unwound,
    context: *mut Context,
) -> DispatcherContext {
    DispatcherContext {
        control_pc: pc as u64,
        image_base: base as u64,
        function_entry: entry,
        establisher_frame: u.frame,
        target_ip: 0,
        context_record: context,
        language_handler: u.handler,
        handler_data: u.handler_data,
        history_table: std::ptr::null_mut(),
        scope_index: 0,
        fill: 0,
    }
}

fn stack_ok(c: &Context, bounds: Bounds) -> bool {
    let rsp = c.gpr[RSP] as usize;
    bounds.is_none_or(|(low, high)| rsp >= low && rsp + 8 <= high)
}

fn read_stack(addr: u64, _bounds: Bounds) -> u64 {
    // SAFETY: the caller checked `addr` against the stack bounds.
    unsafe { (addr as *const u64).read_unaligned() }
}

/// A handler asked to continue an exception that cannot be.
unsafe fn noncontinuable(ctx: *mut Context, rec: *mut ExceptionRecord) -> ! {
    let mut record = ExceptionRecord::new(
        EXCEPTION_NONCONTINUABLE_EXCEPTION,
        EXCEPTION_NONCONTINUABLE,
        // SAFETY: valid for the whole dispatch.
        unsafe { (*ctx).rip } as usize,
    );
    record.record = rec;
    dispatch_exception(ctx, &mut record, 0)
}

/// Nobody took the exception: the unhandled-exception filter, then the end.
unsafe fn unhandled(ctx: *mut Context, rec: *mut ExceptionRecord) -> ! {
    let filter = *UNHANDLED.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: valid for the whole dispatch.
    let (code, address) = unsafe { ((*rec).code, (*rec).address) };
    // Before the filter, which may end the process.
    super::super::dump::report_at_fault(code, true);
    if let Some(handler) = filter {
        let mut ptrs = ExceptionPointers {
            record: rec,
            context: ctx,
        };
        // SAFETY: `ptrs` is ours.
        match unsafe { handler.call(&mut ptrs) } {
            EXCEPTION_CONTINUE_EXECUTION => {
                // SAFETY: as above.
                unsafe { resume(ctx) }
            }
            EXCEPTION_EXECUTE_HANDLER => std::process::exit(code as i32),
            _ => {}
        }
    }
    let place = match unwind::image_base_of(address) {
        Some(base) => format!("image+{:#x}", address - base),
        None => format!("{address:#x}"),
    };
    // SAFETY: valid for the whole dispatch.
    let information = unsafe { (*rec).information };
    eprintln!("VIctory: unhandled exception {code:#010x} at {place} {information:x?}");
    std::process::abort();
}

/// What a language handler calls to unwind: `RtlUnwindEx`. Runs the
/// termination (`__finally`) handlers of the frames between the exception
/// and `target_frame`, then continues at `target_ip` in that frame with
/// `retval` in `rax`. Unwinds from the exception being dispatched on this
/// thread (a call outside a dispatch has no stack to start from and
/// returns).
unsafe extern "C" fn unwind_ex(
    target_frame: u64,
    target_ip: usize,
    rec: *mut ExceptionRecord,
    retval: u64,
    _orig: *mut Context,
    _history: *mut c_void,
) {
    let Some(start) = ACTIVE.with(|a| a.borrow().last().map(|x| x.ctx)) else {
        return;
    };
    let mut local = ExceptionRecord::new(STATUS_UNWIND, 0, 0);
    let rec = if rec.is_null() { &raw mut local } else { rec };
    // SAFETY: a record the caller or this function owns.
    unsafe {
        (*rec).flags |= EXCEPTION_UNWINDING;
        if target_frame == 0 {
            (*rec).flags |= EXCEPTION_EXIT_UNWIND;
        }
    }
    let bounds = stack_bounds();
    // SAFETY: the context of the dispatch in progress.
    let mut c = unsafe { *start };
    let mut first = true;
    for _ in 0..4096 {
        let pc = if first { c.rip } else { c.rip.wrapping_sub(1) } as usize;
        if !stack_ok(&c, bounds) {
            break;
        }
        let mut frame_ctx = c;
        let mut frame = 0;
        match unwind::lookup(pc) {
            Some((base, entry)) => {
                // SAFETY: an entry of an image we mapped.
                let Some(u) = (unsafe {
                    unwind::virtual_unwind(UNW_FLAG_UHANDLER, base, pc, entry, &mut c, bounds)
                }) else {
                    break;
                };
                frame = u.frame;
                if target_frame != 0 && frame > target_frame {
                    break;
                }
                if u.handler != 0 {
                    // SAFETY: valid, per above.
                    unsafe {
                        if frame == target_frame {
                            (*rec).flags |= EXCEPTION_TARGET_UNWIND;
                        }
                    }
                    let mut dc = dispatcher_context(base, pc, entry, u, &raw mut frame_ctx);
                    dc.target_ip = target_ip as u64;
                    // SAFETY: a title language handler.
                    unsafe {
                        call_win64(
                            u.handler,
                            &[rec as u64, u.frame, &raw mut frame_ctx as u64, &raw mut dc as u64],
                        );
                        (*rec).flags &= !(EXCEPTION_COLLIDED_UNWIND | EXCEPTION_TARGET_UNWIND);
                    }
                }
            }
            None => {
                if unwind::image_base_of(pc).is_none() {
                    break;
                }
                let rsp = c.gpr[RSP];
                c.rip = read_stack(rsp, bounds);
                c.gpr[RSP] = rsp + 8;
            }
        }
        if frame == target_frame && target_frame != 0 {
            frame_ctx.gpr[RAX] = retval;
            frame_ctx.rip = target_ip as u64;
            frame_ctx.context_flags &= !CONTEXT_UNWOUND_TO_CALL;
            // SAFETY: the context of a live frame of this thread's stack.
            unsafe { resume(&frame_ctx) };
        }
        c.context_flags |= CONTEXT_UNWOUND_TO_CALL;
        first = false;
        if c.rip == 0 {
            break;
        }
    }
    eprintln!("VIctory: RtlUnwindEx found no frame {target_frame:#x}");
    std::process::abort();
}

const RAX: usize = super::context::RAX;

/// The functions title code calls, as the host's System V ABI sees them
/// (through the bridge), except the two that read the caller's registers.
pub mod api {
    use super::*;

    pub use super::{capture_context_address, debug_break_address, raise_exception_address};

    pub extern "C" fn lookup_function_entry(
        pc: usize,
        image_base: *mut usize,
        _history: *mut c_void,
    ) -> *const RuntimeFunction {
        let Some((base, entry)) = unwind::lookup(pc) else {
            if !image_base.is_null() {
                // SAFETY: the title's out-pointer.
                unsafe { image_base.write(0) };
            }
            return std::ptr::null();
        };
        if !image_base.is_null() {
            // SAFETY: the title's out-pointer.
            unsafe { image_base.write(base) };
        }
        entry
    }

    /// `RtlVirtualUnwind`.
    ///
    /// # Safety
    /// `ctx` and the out-pointers are the title's own.
    pub unsafe extern "C" fn virtual_unwind(
        kind: u32,
        base: usize,
        pc: usize,
        entry: *const RuntimeFunction,
        ctx: *mut Context,
        handler_data: *mut usize,
        frame: *mut u64,
        _pointers: *mut c_void,
    ) -> usize {
        // SAFETY: the title's context for this thread's stack.
        let ctx = unsafe { &mut *ctx };
        // SAFETY: an entry the title got from the lookup.
        let Some(u) = (unsafe { unwind::virtual_unwind(kind, base, pc, entry, ctx, None) }) else {
            return 0;
        };
        // SAFETY: the title's out-pointers, or null.
        unsafe {
            if !handler_data.is_null() {
                handler_data.write(u.handler_data);
            }
            if !frame.is_null() {
                frame.write(u.frame);
            }
        }
        u.handler
    }

    pub extern "C" fn add_function_table(entries: usize, count: u32, base: usize) -> i32 {
        unwind::add_function_table(entries, count, base);
        1
    }

    pub extern "C" fn delete_function_table(entries: usize) -> i32 {
        i32::from(unwind::delete_function_table(entries))
    }

    pub extern "C" fn pc_to_file_header(pc: usize, base: *mut usize) -> usize {
        let found = unwind::image_base_of(pc).unwrap_or(0);
        if !base.is_null() {
            // SAFETY: the title's out-pointer.
            unsafe { base.write(found) };
        }
        found
    }

    pub extern "C" fn add_vectored(first: u32, handler: usize) -> usize {
        add_vectored_handler(first != 0, Handler::Title(handler))
    }

    pub extern "C" fn remove_vectored(handle: usize) -> u32 {
        u32::from(remove_vectored_handler(handle))
    }

    /// Returns the previous filter's address (0 if none or ours).
    pub extern "C" fn set_unhandled(filter: usize) -> usize {
        let new = (filter != 0).then_some(Handler::Title(filter));
        match set_unhandled_filter(new) {
            Some(Handler::Title(f)) => f,
            _ => 0,
        }
    }

    /// `RtlRestoreContext`: continues at the context.
    ///
    /// # Safety
    /// `ctx` must describe code to run on this thread's stack.
    pub unsafe extern "C" fn restore_context(ctx: *const Context, _rec: *mut ExceptionRecord) {
        // SAFETY: forwarded from the caller.
        unsafe { resume(ctx) }
    }

    /// `RtlUnwindEx`.
    ///
    /// # Safety
    /// Called with a record and context of the title's.
    pub unsafe extern "C" fn unwind_ex(
        target_frame: u64,
        target_ip: usize,
        rec: *mut ExceptionRecord,
        retval: u64,
        orig: *mut Context,
        history: *mut c_void,
    ) {
        // SAFETY: forwarded from the caller.
        unsafe { super::unwind_ex(target_frame, target_ip, rec, retval, orig, history) }
    }

    /// `__C_specific_handler`: the language handler of `__try`/`__except`
    /// and `__try`/`__finally` (the scope table is the handler data).
    ///
    /// # Safety
    /// Called by the dispatcher (or the unwinder) with their records.
    pub unsafe extern "C" fn c_specific_handler(
        rec: *mut ExceptionRecord,
        frame: u64,
        ctx: *mut Context,
        dc: *mut DispatcherContext,
    ) -> u32 {
        // SAFETY: records the dispatcher built.
        let (flags, dc) = unsafe { ((*rec).flags, &mut *dc) };
        let table = dc.handler_data as *const u32;
        // SAFETY: the scope table: a count, then 16 bytes per scope.
        let count = unsafe { table.read_unaligned() } as usize;
        let scope = |i: usize| -> [u32; 4] {
            // SAFETY: `i < count`.
            unsafe { (table.add(1 + i * 4) as *const [u32; 4]).read_unaligned() }
        };
        let base = dc.image_base;
        let pc = dc.control_pc;
        if flags & (EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND) != 0 {
            for i in dc.scope_index as usize..count {
                let [begin, end, handler, jump] = scope(i);
                if pc < base + u64::from(begin) || pc >= base + u64::from(end) || jump != 0 {
                    continue;
                }
                if flags & EXCEPTION_TARGET_UNWIND != 0
                    && dc.target_ip >= base + u64::from(begin)
                    && dc.target_ip < base + u64::from(end)
                {
                    break;
                }
                dc.scope_index = i as u32 + 1;
                // The termination handler: `__finally`.
                // SAFETY: title code, (abnormal termination, frame).
                unsafe { call_win64((base + u64::from(handler)) as usize, &[1, frame]) };
            }
            return EXCEPTION_CONTINUE_SEARCH_DISPOSITION;
        }
        for i in dc.scope_index as usize..count {
            let [begin, end, handler, jump] = scope(i);
            if pc < base + u64::from(begin) || pc >= base + u64::from(end) || jump == 0 {
                continue;
            }
            if handler != 1 {
                let mut ptrs = ExceptionPointers {
                    record: rec,
                    context: ctx,
                };
                // SAFETY: title filter (EXCEPTION_POINTERS*, frame).
                let verdict = unsafe {
                    call_win64(
                        (base + u64::from(handler)) as usize,
                        &[&raw mut ptrs as u64, frame],
                    ) as u32 as i32
                };
                match verdict {
                    EXCEPTION_CONTINUE_SEARCH => continue,
                    EXCEPTION_CONTINUE_EXECUTION => return EXCEPTION_CONTINUE_EXECUTION_DISPOSITION,
                    _ => {}
                }
            }
            // SAFETY: the target is the scope's landing pad; this unwinds
            // and does not return.
            unsafe {
                super::unwind_ex(
                    frame,
                    (base + u64::from(jump)) as usize,
                    rec,
                    0,
                    dc.context_record,
                    dc.history_table,
                )
            };
        }
        EXCEPTION_CONTINUE_SEARCH_DISPOSITION
    }
}
