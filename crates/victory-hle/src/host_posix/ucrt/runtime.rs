//! `api-ms-win-crt-runtime` and `-environment`: what a title's C start-up
//! and exit code call. Initializer tables and `atexit` handlers are title
//! functions (called through the bridge); `argv` and the environment come
//! from the host process unless the run path sets them ([`set_command_line`]).

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use super::{EINVAL, Shared, errno, errno_ptr, exports, leak_cstr, leak_wstr, libc, set_errno};
use victory_loader::native::call_win64;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// The invalid-parameter handler.

static INVALID_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Sets `errno` to `e`, tells the title's handler (if it set one), and
/// returns `e`. Windows terminates when there is no handler; here the call
/// just fails.
pub(super) fn bad_param(e: i32) -> i32 {
    set_errno(e);
    let handler = INVALID_HANDLER.load(Ordering::Relaxed);
    if handler != 0 {
        // SAFETY: the handler the title registered, which takes five arguments.
        unsafe { call_win64(handler, &[0; 5]) };
    }
    e
}

pub extern "system" fn _set_invalid_parameter_handler(handler: usize) -> usize {
    INVALID_HANDLER.swap(handler, Ordering::Relaxed)
}

pub extern "system" fn _get_invalid_parameter_handler() -> usize {
    INVALID_HANDLER.load(Ordering::Relaxed)
}

pub extern "system" fn _invalid_parameter_noinfo() {
    bad_param(EINVAL);
}

pub extern "system" fn _invalid_parameter(
    _expr: usize,
    _func: usize,
    _file: usize,
    _line: u32,
    _reserved: usize,
) {
    bad_param(EINVAL);
}

pub extern "system" fn _invalid_parameter_noinfo_noreturn() {
    bad_param(EINVAL);
    std::process::abort();
}

pub extern "system" fn _invoke_watson(
    _expr: usize,
    _func: usize,
    _file: usize,
    _line: u32,
    _reserved: usize,
) {
    std::process::abort();
}

// errno.

pub extern "system" fn _errno() -> *mut i32 {
    errno_ptr()
}

thread_local! {
    static DOS_ERRNO: std::cell::UnsafeCell<u32> = const { std::cell::UnsafeCell::new(0) };
}

pub extern "system" fn __doserrno() -> *mut u32 {
    DOS_ERRNO.with(std::cell::UnsafeCell::get)
}

pub extern "system" fn _get_errno(out: *mut i32) -> i32 {
    if out.is_null() {
        return bad_param(EINVAL);
    }
    // SAFETY: the title's pointer.
    unsafe { out.write(errno()) };
    0
}

pub extern "system" fn _set_errno(e: i32) -> i32 {
    set_errno(e);
    0
}

pub extern "system" fn _get_doserrno(out: *mut u32) -> i32 {
    if out.is_null() {
        return bad_param(EINVAL);
    }
    // SAFETY: the title's pointer.
    unsafe { out.write(*__doserrno()) };
    0
}

pub extern "system" fn _set_doserrno(e: u32) -> i32 {
    // SAFETY: this thread's cell.
    unsafe { *__doserrno() = e };
    0
}

// Initializer tables.

/// Calls each non-null function in `[first, last)`; with `check`, stops at
/// the first one that returns nonzero and returns it.
unsafe fn run_table(first: *const usize, last: *const usize, check: bool) -> i32 {
    let mut p = first;
    while p < last {
        // SAFETY: the title's table of function pointers.
        let f = unsafe { p.read() };
        if f != 0 {
            // SAFETY: a title initializer taking no arguments.
            let r = unsafe { call_win64(f, &[]) } as i32;
            if check && r != 0 {
                return r;
            }
        }
        p = p.wrapping_add(1);
    }
    0
}

pub extern "system" fn _initterm(first: *const usize, last: *const usize) {
    // SAFETY: the title's table.
    unsafe { run_table(first, last, false) };
}

pub extern "system" fn _initterm_e(first: *const usize, last: *const usize) -> i32 {
    // SAFETY: the title's table.
    unsafe { run_table(first, last, true) }
}

/// `_onexit_table_t`: `first`, `last`, `end` of an array of functions.
#[repr(C)]
pub struct OnExitTable {
    first: *mut usize,
    last: *mut usize,
    end: *mut usize,
}

pub extern "system" fn _initialize_onexit_table(table: *mut OnExitTable) -> i32 {
    if table.is_null() {
        return -1;
    }
    // SAFETY: the title's table.
    unsafe {
        if (*table).first.is_null() && (*table).last.is_null() {
            *table = OnExitTable {
                first: std::ptr::null_mut(),
                last: std::ptr::null_mut(),
                end: std::ptr::null_mut(),
            };
        }
    }
    0
}

pub extern "system" fn _register_onexit_function(table: *mut OnExitTable, f: usize) -> i32 {
    if table.is_null() {
        return -1;
    }
    // SAFETY: the title's table, whose array is `malloc`'d here.
    unsafe {
        let t = &mut *table;
        let used = t.last.offset_from(t.first).max(0) as usize;
        let cap = t.end.offset_from(t.first).max(0) as usize;
        if used == cap {
            let new_cap = (cap * 2).max(8);
            let p = libc::realloc(t.first.cast(), new_cap * 8).cast::<usize>();
            if p.is_null() {
                return -1;
            }
            t.first = p;
            t.last = p.add(used);
            t.end = p.add(new_cap);
        }
        t.last.write(f);
        t.last = t.last.add(1);
    }
    0
}

/// Runs the table's functions, last registered first (those registered
/// meanwhile run too), then frees it.
pub extern "system" fn _execute_onexit_table(table: *mut OnExitTable) -> i32 {
    if table.is_null() {
        return -1;
    }
    // SAFETY: the title's table.
    unsafe {
        while !(*table).last.is_null() && (*table).last > (*table).first {
            (*table).last = (*table).last.sub(1);
            let f = (*table).last.read();
            if f != 0 {
                call_win64(f, &[]);
            }
        }
        libc::free((*table).first.cast());
        *table = OnExitTable {
            first: std::ptr::null_mut(),
            last: std::ptr::null_mut(),
            end: std::ptr::null_mut(),
        };
    }
    0
}

/// The process's own `atexit` list.
static AT_EXIT: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static AT_QUICK_EXIT: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static THREAD_LOCAL_EXIT: AtomicUsize = AtomicUsize::new(0);
static APP_TYPE: AtomicU32 = AtomicU32::new(0);

pub extern "system" fn _crt_atexit(f: usize) -> i32 {
    if f == 0 {
        bad_param(EINVAL);
        return -1;
    }
    lock(&AT_EXIT).push(f);
    0
}

pub extern "system" fn _crt_at_quick_exit(f: usize) -> i32 {
    if f == 0 {
        bad_param(EINVAL);
        return -1;
    }
    lock(&AT_QUICK_EXIT).push(f);
    0
}

/// `_onexit` returns the function on success.
pub extern "system" fn _onexit(f: usize) -> usize {
    if _crt_atexit(f) != 0 {
        return 0;
    }
    f
}

pub extern "system" fn _register_thread_local_exe_atexit_callback(f: usize) {
    THREAD_LOCAL_EXIT.store(f, Ordering::Relaxed);
}

pub extern "system" fn _set_app_type(kind: i32) {
    APP_TYPE.store(kind as u32, Ordering::Relaxed);
}

fn run_handlers(list: &Mutex<Vec<usize>>) {
    loop {
        let Some(f) = lock(list).pop() else { break };
        if f != 0 {
            // SAFETY: a function the title registered, taking no arguments.
            unsafe { call_win64(f, &[]) };
        }
    }
}

/// Runs the exit handlers and flushes the streams, without ending the process.
pub extern "system" fn _cexit() {
    run_handlers(&AT_EXIT);
    super::stdio::flush_all();
}

pub extern "system" fn _c_exit() {}

pub extern "system" fn exit(code: i32) {
    _cexit();
    std::process::exit(code);
}

pub extern "system" fn _exit(code: i32) {
    // SAFETY: ends the process.
    unsafe { libc::_exit(code) }
}

pub extern "system" fn quick_exit(code: i32) {
    run_handlers(&AT_QUICK_EXIT);
    // SAFETY: ends the process.
    unsafe { libc::_exit(code) }
}

pub extern "system" fn abort() {
    std::process::abort();
}

pub extern "system" fn terminate() {
    std::process::abort();
}

pub extern "system" fn _set_abort_behavior(_flags: u32, _mask: u32) -> u32 {
    0
}

pub extern "system" fn _set_error_mode(_mode: i32) -> i32 {
    0
}

pub extern "system" fn _set_purecall_handler(_f: usize) -> usize {
    0
}

pub extern "system" fn _set_printf_count_output(_enable: i32) -> i32 {
    0
}

pub extern "system" fn _get_printf_count_output() -> i32 {
    0
}

/// No exception reaches a CRT filter: the unwinder already ran the title's
/// own handlers, so `EXCEPTION_CONTINUE_SEARCH`.
pub extern "system" fn _seh_filter_exe(_code: u32, _pointers: usize) -> i32 {
    0
}

pub extern "system" fn _seh_filter_dll(_code: u32, _pointers: usize) -> i32 {
    0
}

pub extern "system" fn _getpid() -> i32 {
    std::process::id() as i32
}

pub extern "system" fn _wassert(_msg: usize, _file: usize, _line: u32) {
    tracing::error!("title assertion failed");
    std::process::abort();
}

// signal

const SIG_DFL: usize = 0;
const SIG_IGN: usize = 1;
const SIG_ERR: usize = usize::MAX;
const SIGABRT: i32 = 22;
static HANDLERS: Mutex<[usize; 32]> = Mutex::new([0; 32]);

fn signal_ok(sig: i32) -> bool {
    matches!(sig, 2 | 4 | 6 | 8 | 11 | 15 | 21 | 22)
}

pub extern "system" fn signal(sig: i32, handler: usize) -> usize {
    if !signal_ok(sig) {
        set_errno(EINVAL);
        return SIG_ERR;
    }
    std::mem::replace(&mut lock(&HANDLERS)[sig as usize], handler)
}

pub extern "system" fn raise(sig: i32) -> i32 {
    if !signal_ok(sig) {
        set_errno(EINVAL);
        return -1;
    }
    let handler = lock(&HANDLERS)[sig as usize];
    match handler {
        SIG_IGN => 0,
        SIG_DFL => {
            if sig == SIGABRT || sig == 6 {
                std::process::abort();
            }
            std::process::exit(3);
        }
        f => {
            // Back to the default while the handler runs, as the CRT does.
            lock(&HANDLERS)[sig as usize] = SIG_DFL;
            // SAFETY: the title's handler, taking the signal number.
            unsafe { call_win64(f, &[sig as u64]) };
            0
        }
    }
}

// Threads.

/// `_beginthreadex`: a thread that runs `start(arg)` and returns its
/// handle (which the caller closes).
pub extern "system" fn _beginthreadex(
    security: *const c_void,
    stack: u32,
    start: usize,
    arg: *mut c_void,
    flags: u32,
    thread_id: *mut u32,
) -> usize {
    let h = crate::host_posix::CreateThread(security, stack as usize, start, arg, flags, thread_id);
    if h.is_null() {
        set_errno(super::EACCES);
    }
    h as usize
}

pub extern "system" fn _endthreadex(code: u32) -> ! {
    // SAFETY: called through the host stub from a title thread entry point.
    unsafe { victory_loader::native::exit_title_thread(code) }
}

/// `_beginthread` returns `(uintptr_t)-1` on failure.
pub extern "system" fn _beginthread(start: usize, stack: u32, arg: *mut c_void) -> usize {
    let h = crate::host_posix::CreateThread(
        std::ptr::null(),
        stack as usize,
        start,
        arg,
        0,
        std::ptr::null_mut(),
    );
    if h.is_null() {
        return usize::MAX;
    }
    crate::host_posix::CloseHandle(h);
    1
}

// Command line and environment.

struct Process {
    argc: i32,
    argv: *mut *mut u8,
    wargv: *mut *mut u16,
    cmdline: *mut u8,
    wcmdline: *mut u16,
    /// The command line after the program name.
    rest: *mut u8,
    wrest: *mut u16,
    program: *mut u8,
    wprogram: *mut u16,
}

// SAFETY: leaked, read-only memory.
unsafe impl Send for Process {}

static ARGS: Mutex<Option<Vec<String>>> = Mutex::new(None);
static PROCESS: Mutex<Option<Process>> = Mutex::new(None);
static ARGC: Shared<i32> = Shared::new(0);
static ARGV: Shared<usize> = Shared::new(0);
static WARGV: Shared<usize> = Shared::new(0);
static ACMDLN: Shared<usize> = Shared::new(0);
static WCMDLN: Shared<usize> = Shared::new(0);
static PGMPTR: Shared<usize> = Shared::new(0);
static WPGMPTR: Shared<usize> = Shared::new(0);
static ENVIRON: Shared<usize> = Shared::new(0);
static WENVIRON: Shared<usize> = Shared::new(0);
static FMODE: Shared<i32> = Shared::new(0);
static COMMODE: Shared<i32> = Shared::new(0);

/// Gives the title this command line: `args[0]` is the program's path.
/// Without it the host process's own arguments are used.
pub fn set_command_line(args: &[String]) {
    *lock(&ARGS) = Some(args.to_vec());
    *lock(&PROCESS) = None;
}

fn quote(a: &str) -> String {
    if !(a.is_empty() || a.contains([' ', '\t', '"'])) {
        return a.to_string();
    }
    let mut out = String::with_capacity(a.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in a.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..(backslashes * 2 + 1) {
                    out.push('\\');
                }
                out.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                out.push(c);
                backslashes = 0;
            }
        }
    }
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
    out
}

fn pointers<T>(items: Vec<*mut T>) -> *mut *mut T {
    let mut v = items;
    v.push(std::ptr::null_mut());
    Box::leak(v.into_boxed_slice()).as_mut_ptr()
}

/// Builds `argv` and the command line once, and points the CRT variables at them.
fn with_process<R>(f: impl FnOnce(&Process) -> R) -> R {
    let mut slot = lock(&PROCESS);
    if slot.is_none() {
        let args = lock(&ARGS)
            .clone()
            .unwrap_or_else(|| std::env::args().collect());
        let narrow: Vec<*mut u8> = args.iter().map(|a| leak_cstr(a.as_bytes())).collect();
        let wide: Vec<*mut u16> = args
            .iter()
            .map(|a| leak_wstr(&a.encode_utf16().collect::<Vec<_>>()))
            .collect();
        let rest = args
            .iter()
            .skip(1)
            .map(|a| quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        let full = args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
        let p = Process {
            argc: args.len() as i32,
            argv: pointers(narrow),
            wargv: pointers(wide),
            cmdline: leak_cstr(full.as_bytes()),
            wcmdline: leak_wstr(&full.encode_utf16().collect::<Vec<_>>()),
            rest: leak_cstr(rest.as_bytes()),
            wrest: leak_wstr(&rest.encode_utf16().collect::<Vec<_>>()),
            program: leak_cstr(args.first().map_or(&[][..], |a| a.as_bytes())),
            wprogram: leak_wstr(
                &args
                    .first()
                    .map(|a| a.encode_utf16().collect::<Vec<_>>())
                    .unwrap_or_default(),
            ),
        };
        // SAFETY: the CRT variables, written under the process lock.
        unsafe {
            *ARGC.ptr() = p.argc;
            *ARGV.ptr() = p.argv as usize;
            *WARGV.ptr() = p.wargv as usize;
            *ACMDLN.ptr() = p.cmdline as usize;
            *WCMDLN.ptr() = p.wcmdline as usize;
            *PGMPTR.ptr() = p.program as usize;
            *WPGMPTR.ptr() = p.wprogram as usize;
        }
        *slot = Some(p);
    }
    f(slot.as_ref().expect("built above"))
}

pub extern "system" fn __p___argc() -> *mut i32 {
    with_process(|_| ());
    ARGC.ptr()
}

pub extern "system" fn __p___argv() -> *mut usize {
    with_process(|_| ());
    ARGV.ptr()
}

pub extern "system" fn __p___wargv() -> *mut usize {
    with_process(|_| ());
    WARGV.ptr()
}

pub extern "system" fn __p__acmdln() -> *mut usize {
    with_process(|_| ());
    ACMDLN.ptr()
}

pub extern "system" fn __p__wcmdln() -> *mut usize {
    with_process(|_| ());
    WCMDLN.ptr()
}

pub extern "system" fn __p__pgmptr() -> *mut usize {
    with_process(|_| ());
    PGMPTR.ptr()
}

pub extern "system" fn __p__wpgmptr() -> *mut usize {
    with_process(|_| ());
    WPGMPTR.ptr()
}

pub extern "system" fn _get_pgmptr(out: *mut usize) -> i32 {
    if out.is_null() {
        return bad_param(EINVAL);
    }
    // SAFETY: the title's pointer.
    unsafe { out.write(with_process(|p| p.program as usize)) };
    0
}

pub extern "system" fn _get_wpgmptr(out: *mut usize) -> i32 {
    if out.is_null() {
        return bad_param(EINVAL);
    }
    // SAFETY: the title's pointer.
    unsafe { out.write(with_process(|p| p.wprogram as usize)) };
    0
}

pub extern "system" fn _configure_narrow_argv(_mode: i32) -> i32 {
    with_process(|_| ());
    0
}

pub extern "system" fn _configure_wide_argv(_mode: i32) -> i32 {
    with_process(|_| ());
    0
}

pub extern "system" fn _get_narrow_winmain_command_line() -> *mut u8 {
    with_process(|p| p.rest)
}

pub extern "system" fn _get_wide_winmain_command_line() -> *mut u16 {
    with_process(|p| p.wrest)
}

pub extern "system" fn __p__fmode() -> *mut i32 {
    FMODE.ptr()
}

pub extern "system" fn __p__commode() -> *mut i32 {
    COMMODE.ptr()
}

pub extern "system" fn _set_fmode(mode: i32) -> i32 {
    // SAFETY: a word the title shares with the C runtime.
    unsafe { *FMODE.ptr() = mode };
    0
}

pub extern "system" fn _get_fmode(out: *mut i32) -> i32 {
    if out.is_null() {
        return bad_param(EINVAL);
    }
    // SAFETY: the title's pointer; `FMODE` is a plain word.
    unsafe { out.write(*FMODE.ptr()) };
    0
}

/// The environment block at process start: `NAME=value` strings, then null.
fn environment() -> (usize, usize) {
    static BLOCK: std::sync::OnceLock<(usize, usize)> = std::sync::OnceLock::new();
    *BLOCK.get_or_init(|| {
        let mut narrow = Vec::new();
        let mut wide = Vec::new();
        for (k, v) in std::env::vars_os() {
            let (k, v) = (k.to_string_lossy(), v.to_string_lossy());
            let entry = format!("{k}={v}");
            narrow.push(leak_cstr(entry.as_bytes()));
            wide.push(leak_wstr(&entry.encode_utf16().collect::<Vec<_>>()));
        }
        let (n, w) = (pointers(narrow) as usize, pointers(wide) as usize);
        // SAFETY: the CRT variables.
        unsafe {
            *ENVIRON.ptr() = n;
            *WENVIRON.ptr() = w;
        }
        (n, w)
    })
}

pub extern "system" fn _get_initial_narrow_environment() -> usize {
    environment().0
}

pub extern "system" fn _get_initial_wide_environment() -> usize {
    environment().1
}

pub extern "system" fn _initialize_narrow_environment() -> i32 {
    environment();
    0
}

pub extern "system" fn _initialize_wide_environment() -> i32 {
    environment();
    0
}

pub extern "system" fn __p__environ() -> *mut usize {
    environment();
    ENVIRON.ptr()
}

pub extern "system" fn __p__wenviron() -> *mut usize {
    environment();
    WENVIRON.ptr()
}

/// The value of `name`, found as Windows does: ignoring case.
fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().or_else(|| {
        std::env::vars()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    })
}

/// One leaked copy per distinct value, so repeated calls do not grow.
fn leaked_value(v: &str) -> *mut u8 {
    static SEEN: Mutex<Vec<(String, usize)>> = Mutex::new(Vec::new());
    let mut seen = lock(&SEEN);
    if let Some((_, p)) = seen.iter().find(|(s, _)| s == v) {
        return *p as *mut u8;
    }
    let p = leak_cstr(v.as_bytes());
    seen.push((v.to_string(), p as usize));
    p
}

pub extern "system" fn getenv(name: *const u8) -> *mut u8 {
    if name.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: a NUL-terminated name.
    let name = String::from_utf8_lossy(unsafe { super::bytes(name) }).into_owned();
    env_value(&name).map_or(std::ptr::null_mut(), |v| leaked_value(&v))
}

/// One leaked wide copy per distinct value, so repeated calls do not grow.
fn leaked_wide_value(v: &str) -> *mut u16 {
    static SEEN: Mutex<Vec<(String, usize)>> = Mutex::new(Vec::new());
    let mut seen = lock(&SEEN);
    if let Some((_, p)) = seen.iter().find(|(s, _)| s == v) {
        return *p as *mut u16;
    }
    let p = leak_wstr(&v.encode_utf16().collect::<Vec<_>>());
    seen.push((v.to_string(), p as usize));
    p
}

pub extern "system" fn _wgetenv(name: *const u16) -> *mut u16 {
    if name.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: a NUL-terminated name.
    let name = String::from_utf16_lossy(unsafe { super::wide(name) });
    env_value(&name).map_or(std::ptr::null_mut(), |v| leaked_wide_value(&v))
}

pub extern "system" fn getenv_s(
    len: *mut usize,
    buf: *mut u8,
    size: usize,
    name: *const u8,
) -> i32 {
    if len.is_null() || name.is_null() || (buf.is_null() != (size == 0)) {
        return bad_param(EINVAL);
    }
    // SAFETY: a NUL-terminated name.
    let name = String::from_utf8_lossy(unsafe { super::bytes(name) }).into_owned();
    let v = env_value(&name);
    // SAFETY: the title's out parameters and buffer.
    unsafe {
        let Some(v) = v else {
            len.write(0);
            if size > 0 {
                buf.write(0);
            }
            return 0;
        };
        len.write(v.len() + 1);
        if size == 0 {
            return 0;
        }
        if v.len() + 1 > size {
            buf.write(0);
            return bad_param(super::ERANGE);
        }
        std::ptr::copy_nonoverlapping(v.as_ptr(), buf, v.len());
        buf.add(v.len()).write(0);
    }
    0
}

pub extern "system" fn _dupenv_s(buf: *mut *mut u8, len: *mut usize, name: *const u8) -> i32 {
    if buf.is_null() || name.is_null() {
        return bad_param(EINVAL);
    }
    // SAFETY: a NUL-terminated name and the title's out parameters.
    unsafe {
        let name = String::from_utf8_lossy(super::bytes(name)).into_owned();
        let Some(v) = env_value(&name) else {
            buf.write(std::ptr::null_mut());
            if !len.is_null() {
                len.write(0);
            }
            return 0;
        };
        let p = libc::malloc(v.len() + 1).cast::<u8>();
        if p.is_null() {
            return super::fail(super::ENOMEM);
        }
        std::ptr::copy_nonoverlapping(v.as_ptr(), p, v.len());
        p.add(v.len()).write(0);
        buf.write(p);
        if !len.is_null() {
            len.write(v.len() + 1);
        }
    }
    0
}

fn set_env(name: &str, value: &str) -> i32 {
    if name.is_empty() || name.contains('=') {
        return bad_param(EINVAL);
    }
    // Remove every case-insensitive match first so Windows semantics hold on
    // macOS's case-sensitive environment.
    let matching: Vec<std::ffi::OsString> = std::env::vars_os()
        .filter_map(|(k, _)| {
            k.to_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(name))
                .then_some(k)
        })
        .collect();
    // SAFETY: the host process's environment, set the way the title asks.
    unsafe {
        for key in matching {
            std::env::remove_var(key);
        }
        if !value.is_empty() {
            std::env::set_var(name, value);
        }
    }
    0
}

pub extern "system" fn _putenv(entry: *const u8) -> i32 {
    if entry.is_null() {
        bad_param(EINVAL);
        return -1;
    }
    // SAFETY: a NUL-terminated `NAME=value`.
    let text = String::from_utf8_lossy(unsafe { super::bytes(entry) }).into_owned();
    let Some((n, v)) = text.split_once('=') else {
        bad_param(EINVAL);
        return -1;
    };
    if set_env(n, v) == 0 { 0 } else { -1 }
}

pub extern "system" fn _wputenv(entry: *const u16) -> i32 {
    if entry.is_null() {
        bad_param(EINVAL);
        return -1;
    }
    // SAFETY: a NUL-terminated `NAME=value`.
    let text = String::from_utf16_lossy(unsafe { super::wide(entry) });
    let Some((n, v)) = text.split_once('=') else {
        bad_param(EINVAL);
        return -1;
    };
    if set_env(n, v) == 0 { 0 } else { -1 }
}

pub extern "system" fn _putenv_s(name: *const u8, value: *const u8) -> i32 {
    if name.is_null() || value.is_null() {
        return bad_param(EINVAL);
    }
    // SAFETY: NUL-terminated strings.
    let (n, v) = unsafe { (super::bytes(name), super::bytes(value)) };
    set_env(&String::from_utf8_lossy(n), &String::from_utf8_lossy(v))
}

pub extern "system" fn _wputenv_s(name: *const u16, value: *const u16) -> i32 {
    if name.is_null() || value.is_null() {
        return bad_param(EINVAL);
    }
    // SAFETY: NUL-terminated strings.
    let (n, v) = unsafe { (super::wide(name), super::wide(value)) };
    set_env(&String::from_utf16_lossy(n), &String::from_utf16_lossy(v))
}

exports!(pub(super) fn lookup;
    _set_invalid_parameter_handler, _get_invalid_parameter_handler, _invalid_parameter_noinfo,
    _invalid_parameter, _invalid_parameter_noinfo_noreturn, _invoke_watson, _errno, __doserrno,
    _get_errno, _set_errno, _get_doserrno, _set_doserrno, _initterm, _initterm_e,
    _initialize_onexit_table, _register_onexit_function, _execute_onexit_table, _crt_atexit,
    _crt_at_quick_exit, _onexit, _register_thread_local_exe_atexit_callback, _set_app_type,
    _cexit, _c_exit, exit, _exit, quick_exit, abort, terminate, _set_abort_behavior,
    _set_error_mode, _set_purecall_handler, _set_printf_count_output, _get_printf_count_output,
    _seh_filter_exe, _seh_filter_dll, _getpid, _wassert, signal, raise, _beginthreadex, _endthreadex,
    _beginthread, __p___argc, __p___argv, __p___wargv, __p__acmdln, __p__wcmdln, __p__pgmptr,
    __p__wpgmptr, _get_pgmptr, _get_wpgmptr, _configure_narrow_argv, _configure_wide_argv,
    _get_narrow_winmain_command_line, _get_wide_winmain_command_line, __p__fmode, __p__commode,
    _set_fmode, _get_fmode, _get_initial_narrow_environment, _get_initial_wide_environment,
    _initialize_narrow_environment, _initialize_wide_environment, __p__environ, __p__wenviron,
    getenv, _wgetenv, getenv_s, _dupenv_s, _putenv, _wputenv, _putenv_s, _wputenv_s;
    "atexit" => _onexit_atexit, "at_quick_exit" => _crt_at_quick_exit,
    "_set_thread_local_invalid_parameter_handler" => _set_invalid_parameter_handler,
    "_get_thread_local_invalid_parameter_handler" => _get_invalid_parameter_handler,
    "_XcptFilter" => _seh_filter_exe, "_assert" => _wassert);

/// `atexit` returns 0 on success.
extern "system" fn _onexit_atexit(f: usize) -> i32 {
    _crt_atexit(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endthreadex_exits_the_guest_thread_with_its_code() {
        use crate::host_posix::{
            CloseHandle, GetExitCodeThread, WAIT_OBJECT_0, WaitForSingleObject, ucrt,
        };

        #[unsafe(naked)]
        extern "win64" fn start(_end: usize) -> u32 {
            core::arch::naked_asm!(
                "sub rsp, 40",
                "mov rax, rcx",
                "mov ecx, 0xFEDCBA98",
                "call rax",
                "xor eax, eax",
                "add rsp, 40",
                "ret",
            );
        }

        let export = ucrt::lookup("api-ms-win-crt-runtime-l1-1-0.dll", "_endthreadex").unwrap();
        let stub = victory_loader::native::host_callbacks(&[(export, false)]).unwrap()[0];
        let handle = _beginthreadex(
            std::ptr::null(),
            0,
            start as *const () as usize,
            stub as *mut c_void,
            0,
            std::ptr::null_mut(),
        ) as *mut c_void;
        assert!(!handle.is_null());
        assert_eq!(WaitForSingleObject(handle, 5000), WAIT_OBJECT_0);
        let mut code = 0;
        assert_eq!(GetExitCodeThread(handle, &mut code), 1);
        assert_eq!(code, 0xFEDC_BA98);
        assert_eq!(CloseHandle(handle), 1);
    }

    #[test]
    fn the_invalid_parameter_handler_is_swapped_and_errno_set() {
        assert_eq!(_set_invalid_parameter_handler(0), 0);
        assert_eq!(bad_param(EINVAL), EINVAL);
        assert_eq!(errno(), EINVAL);
    }

    #[test]
    fn onexit_tables_grow_and_free() {
        let mut t = OnExitTable {
            first: std::ptr::null_mut(),
            last: std::ptr::null_mut(),
            end: std::ptr::null_mut(),
        };
        assert_eq!(_initialize_onexit_table(&mut t), 0);
        for f in 1..=20 {
            assert_eq!(_register_onexit_function(&mut t, f), 0);
        }
        // SAFETY: the table's array.
        assert_eq!(unsafe { t.last.offset_from(t.first) }, 20);
        // Entries are functions (here null-ish handlers are skipped only for 0); drop them
        // without calling by clearing, then execute an empty table.
        // SAFETY: the table's array.
        unsafe { std::slice::from_raw_parts_mut(t.first, 20) }.fill(0);
        assert_eq!(_execute_onexit_table(&mut t), 0);
        assert!(t.first.is_null());
    }

    #[test]
    fn argv_and_the_command_line_come_from_the_run() {
        set_command_line(&["T:\\game.exe".into(), "-a".into(), "two words".into()]);
        // SAFETY: the pointers are the leaked arrays.
        unsafe {
            assert_eq!(*__p___argc(), 3);
            let argv = *__p___argv() as *const *const u8;
            assert_eq!(super::super::bytes(*argv), b"T:\\game.exe");
            assert_eq!(super::super::bytes(*argv.add(1)), b"-a");
            assert!((*argv.add(3)).is_null());
            assert_eq!(
                super::super::bytes(_get_narrow_winmain_command_line()),
                b"-a \"two words\""
            );
            let wargv = *__p___wargv() as *const *const u16;
            assert_eq!(
                super::super::wide(*wargv.add(2)),
                "two words".encode_utf16().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn the_environment_is_case_insensitive_and_settable() {
        assert_eq!(
            _putenv_s(c"VICTORY_UCRT_T".as_ptr().cast(), c"v1".as_ptr().cast()),
            0
        );
        // SAFETY: NUL-terminated results.
        unsafe {
            assert_eq!(
                super::super::bytes(getenv(c"victory_ucrt_t".as_ptr().cast())),
                b"v1"
            );
        }
        assert!(getenv(c"VICTORY_UCRT_MISSING".as_ptr().cast()).is_null());
        let mut buf = [0u8; 8];
        let mut n = 0;
        assert_eq!(
            getenv_s(
                &mut n,
                buf.as_mut_ptr(),
                8,
                c"VICTORY_UCRT_T".as_ptr().cast()
            ),
            0
        );
        assert_eq!((n, &buf[..3]), (3, &b"v1\0"[..]));
        assert_eq!(
            getenv_s(
                &mut n,
                buf.as_mut_ptr(),
                2,
                c"VICTORY_UCRT_T".as_ptr().cast()
            ),
            super::super::ERANGE
        );
        set_errno(0);
        assert_eq!(_putenv(c"NOEQUALS".as_ptr().cast()), -1);
        assert_eq!(errno(), EINVAL);
        set_errno(0);
        assert_eq!(_wputenv(std::ptr::null()), -1);
        assert_eq!(errno(), EINVAL);
        let no_eq_w: Vec<u16> = "NOEQUALS".encode_utf16().chain([0]).collect();
        set_errno(0);
        assert_eq!(_wputenv(no_eq_w.as_ptr()), -1);
        assert_eq!(errno(), EINVAL);
        let name_w: Vec<u16> = "VICTORY_UCRT_T".encode_utf16().chain([0]).collect();
        let w1 = _wgetenv(name_w.as_ptr());
        let w2 = _wgetenv(name_w.as_ptr());
        assert!(!w1.is_null());
        assert_eq!(w1, w2);
        assert_eq!(
            _putenv_s(c"VICTORY_UCRT_T".as_ptr().cast(), c"".as_ptr().cast()),
            0
        );
        assert!(getenv(c"VICTORY_UCRT_T".as_ptr().cast()).is_null());
        let env = _get_initial_narrow_environment() as *const *const u8;
        assert!(!env.is_null());
    }

    #[test]
    fn signals_run_handlers_through_the_registry() {
        assert_eq!(signal(2, SIG_IGN), SIG_DFL);
        assert_eq!(raise(2), 0);
        assert_eq!(signal(2, SIG_DFL), SIG_IGN);
        assert_eq!(signal(99, SIG_IGN), SIG_ERR);
        assert_eq!(raise(99), -1);
    }

    #[test]
    fn env_updates_and_removals_are_case_insensitive_and_guards_hold() {
        assert_eq!(
            _putenv_s(c"VICTORY_CASE_TEST".as_ptr().cast(), c"one".as_ptr().cast()),
            0
        );
        // Updating with different case must replace the original key.
        assert_eq!(
            _putenv_s(c"victory_case_test".as_ptr().cast(), c"two".as_ptr().cast()),
            0
        );
        // SAFETY: NUL-terminated result.
        assert_eq!(
            unsafe { super::super::bytes(getenv(c"VICTORY_CASE_TEST".as_ptr().cast())) },
            b"two"
        );

        // getenv_s rejects non-null buffer with zero size, and clears buf on ERANGE.
        let mut n = 99usize;
        let mut buf = [b'x'; 2];
        assert_eq!(
            getenv_s(
                &mut n,
                buf.as_mut_ptr(),
                0,
                c"VICTORY_CASE_TEST".as_ptr().cast()
            ),
            EINVAL
        );
        assert_eq!(
            getenv_s(
                &mut n,
                buf.as_mut_ptr(),
                2,
                c"VICTORY_CASE_TEST".as_ptr().cast()
            ),
            super::super::ERANGE
        );
        assert_eq!((n, buf[0]), (4, 0));

        // Removing with different case removes the variable completely.
        assert_eq!(
            _putenv_s(c"ViCtOrY_CaSe_TeSt".as_ptr().cast(), c"".as_ptr().cast()),
            0
        );
        assert!(getenv(c"VICTORY_CASE_TEST".as_ptr().cast()).is_null());
        assert!(getenv(c"victory_case_test".as_ptr().cast()).is_null());

        // Null exit handlers are rejected instead of being queued to jump to 0.
        set_errno(0);
        assert_eq!(_crt_atexit(0), -1);
        assert_eq!(errno(), EINVAL);
        set_errno(0);
        assert_eq!(_crt_at_quick_exit(0), -1);
        assert_eq!(errno(), EINVAL);
        set_errno(0);
        assert_eq!(_onexit(0), 0);
        assert_eq!(errno(), EINVAL);

        // Windows command-line quoting doubles backslashes before quotes and closing quotes.
        assert_eq!(quote(r"C:\Program Files\"), r#""C:\Program Files\\""#);
        assert_eq!(quote(r#"a\"b"#), r#""a\\\"b""#);
    }
}
