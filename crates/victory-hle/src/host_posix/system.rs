//! The rest of what title code (and the DLLs it ships) calls in kernel32 and
//! its API sets, advapi32, bcrypt, crypt32 and the window and message
//! calls of user32, on POSIX: time and time zones, strings and locales,
//! system information, processes and threads, mutexes, waitable timers,
//! the thread pool, FLS, interlocked lists, and stubs where the console
//! has nothing to answer (registry, ETW, COM, named pipes, window messages).
//!
//! Stubs say so in their failure: the registry has no keys, a COM class is
//! not registered, no window ever receives a message. Real work is only
//! where a title cannot go on without it.
//!
//! The C runtime (`api-ms-win-crt-*`) is not here.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::Duration;

use victory_gpu::clock;
use victory_loader::native::{self, seh, teb};
use victory_platform::mem::{self, Protect};
use victory_platform::sync::{Event, Semaphore};
use victory_platform::thread;

use super::{Object, files, lock, new_handle, set_error};

const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_NOT_SUPPORTED: u32 = 50;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_NOT_OWNER: u32 = 288;
const ERROR_INVALID_FLAGS: u32 = 1004;
const ERROR_MOD_NOT_FOUND: u32 = 126;

/// 100 ns ticks from 1601 to 1970.
const EPOCH_DIFFERENCE: u64 = 116_444_736_000_000_000;
const TICKS_PER_SECOND: u64 = 10_000_000;
const TICKS_PER_DAY: u64 = 86_400 * TICKS_PER_SECOND;
/// Days from 1601-01-01 to 1970-01-01.
const DAYS_1601_TO_1970: i64 = 134_774;

unsafe extern "C" {
    fn arc4random_buf(buf: *mut c_void, len: usize);
    fn localtime_r(time: *const i64, out: *mut Tm) -> *mut Tm;
}

/// libc's `struct tm` on macOS.
#[repr(C)]
struct Tm {
    sec: i32,
    min: i32,
    hour: i32,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
    isdst: i32,
    gmtoff: i64,
    zone: *const u8,
}

// SAFETY (every function below): the title passes the pointers its Windows
// call takes, valid for the structures named in each signature.

// Strings.

/// `n` units at `p`, or up to the NUL if `n` is negative.
unsafe fn units<'a, T: Copy + Default + PartialEq>(p: *const T, n: i32) -> &'a [T] {
    // SAFETY: as above.
    unsafe {
        let len = if n < 0 {
            (0..).take_while(|&i| *p.add(i) != T::default()).count()
        } else {
            n as usize
        };
        std::slice::from_raw_parts(p, len)
    }
}

/// Copies `text` out as `WideCharToMultiByte` and friends do: the count
/// needed if `size` is 0, else the count written, 0 with
/// `ERROR_INSUFFICIENT_BUFFER` if it does not fit.
fn copy_out<T: Copy>(text: &[T], out: *mut T, size: i32) -> i32 {
    if size == 0 {
        return text.len() as i32;
    }
    if text.len() > size as usize {
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    // SAFETY: the title's buffer of `size` elements.
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len()) };
    text.len() as i32
}

// shortcut: every code page is read and written as UTF-8 (or Latin-1 for
// 28591 and 1252); the title's own text is ASCII or UTF-8.
pub extern "system" fn MultiByteToWideChar(
    code_page: u32,
    _flags: u32,
    src: *const u8,
    src_len: i32,
    out: *mut u16,
    size: i32,
) -> i32 {
    if src.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: see above.
    let bytes = unsafe { units(src, if src_len < 0 { -1 } else { src_len }) };
    let mut text: Vec<u16> = if matches!(code_page, 1252 | 28591) {
        bytes.iter().map(|&b| u16::from(b)).collect()
    } else {
        String::from_utf8_lossy(bytes).encode_utf16().collect()
    };
    if src_len < 0 {
        text.push(0);
    }
    copy_out(&text, out, size)
}

pub extern "system" fn WideCharToMultiByte(
    code_page: u32,
    _flags: u32,
    src: *const u16,
    src_len: i32,
    out: *mut u8,
    size: i32,
    _default: *const u8,
    used_default: *mut i32,
) -> i32 {
    if src.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if !used_default.is_null() {
        // SAFETY: see above.
        unsafe { used_default.write(0) };
    }
    // SAFETY: see above.
    let text =
        String::from_utf16_lossy(unsafe { units(src, if src_len < 0 { -1 } else { src_len }) });
    let mut bytes: Vec<u8> = if matches!(code_page, 1252 | 28591) {
        text.chars()
            .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
            .collect()
    } else {
        text.into_bytes()
    };
    if src_len < 0 {
        bytes.push(0);
    }
    copy_out(&bytes, out, size)
}

pub extern "system" fn lstrcmpW(a: *const u16, b: *const u16) -> i32 {
    // SAFETY: see above.
    let (a, b) = unsafe { (units(a, -1), units(b, -1)) };
    a.cmp(b) as i32
}

const NORM_IGNORECASE: u32 = 1;
const LINGUISTIC_IGNORECASE: u32 = 0x10;

fn fold(unit: u16) -> u16 {
    char::from_u32(u32::from(unit))
        .and_then(|c| c.to_lowercase().next())
        .map_or(unit, |c| c as u32 as u16)
}

// shortcut: code unit order, with case folded when asked; no linguistic
// collation.
pub extern "system" fn CompareStringEx(
    _locale: *const u16,
    flags: u32,
    a: *const u16,
    a_len: i32,
    b: *const u16,
    b_len: i32,
    _version: *const c_void,
    _reserved: *const c_void,
    _param: isize,
) -> i32 {
    // SAFETY: see above.
    let (a, b) = unsafe { (units(a, a_len), units(b, b_len)) };
    let ignore = flags & (NORM_IGNORECASE | LINGUISTIC_IGNORECASE) != 0;
    let key = |s: &[u16]| -> Vec<u16> {
        s.iter()
            .map(|&u| if ignore { fold(u) } else { u })
            .collect()
    };
    match key(a).cmp(&key(b)) {
        std::cmp::Ordering::Less => 1,
        std::cmp::Ordering::Equal => 2,
        std::cmp::Ordering::Greater => 3,
    }
}

const CT_CTYPE1: u32 = 1;

pub extern "system" fn GetStringTypeW(kind: u32, src: *const u16, len: i32, out: *mut u16) -> i32 {
    if kind != CT_CTYPE1 || src.is_null() || out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: see above.
    for (i, &unit) in unsafe { units(src, len) }.iter().enumerate() {
        let c = char::from_u32(u32::from(unit)).unwrap_or('\0');
        let mut t = 0u16;
        if c.is_uppercase() {
            t |= 1;
        }
        if c.is_lowercase() {
            t |= 2;
        }
        if c.is_ascii_digit() {
            t |= 4;
        }
        if c.is_whitespace() {
            t |= 8;
        }
        if c.is_ascii_punctuation() {
            t |= 0x10;
        }
        if c.is_control() {
            t |= 0x20;
        }
        if c == ' ' || c == '\t' {
            t |= 0x40;
        }
        if c.is_ascii_hexdigit() {
            t |= 0x80;
        }
        if c.is_alphabetic() {
            t |= 0x100;
        }
        // SAFETY: the title's output array has one word per unit.
        unsafe { out.add(i).write(t) };
    }
    1
}

pub extern "system" fn GetCPInfo(code_page: u32, info: *mut u8) -> i32 {
    if info.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // CPINFO: MaxCharSize u32, DefaultChar[2], LeadByte[12].
    // SAFETY: a 18-byte structure.
    unsafe {
        std::ptr::write_bytes(info, 0, 18);
        info.cast::<u32>()
            .write_unaligned(if code_page == 65001 { 4 } else { 1 });
        info.add(4).write(b'?');
    }
    1
}

const LCMAP_LOWERCASE: u32 = 0x100;
const LCMAP_UPPERCASE: u32 = 0x200;

pub extern "system" fn LCMapStringEx(
    _locale: *const u16,
    flags: u32,
    src: *const u16,
    len: i32,
    out: *mut u16,
    size: i32,
    _version: *const c_void,
    _reserved: *const c_void,
    _param: isize,
) -> i32 {
    // SAFETY: see above.
    let mut text = unsafe { units(src, len) }.to_vec();
    for unit in &mut text {
        let Some(c) = char::from_u32(u32::from(*unit)) else {
            continue;
        };
        let mapped = if flags & LCMAP_LOWERCASE != 0 {
            c.to_lowercase().next()
        } else if flags & LCMAP_UPPERCASE != 0 {
            c.to_uppercase().next()
        } else {
            None
        };
        if let Some(m) = mapped.filter(|m| (*m as u32) < 0x1_0000) {
            *unit = m as u32 as u16;
        }
    }
    copy_out(&text, out, size)
}

fn write_wide(text: &str, out: *mut u16, size: i32) -> i32 {
    let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
    copy_out(&units, out, size)
}

pub extern "system" fn GetUserDefaultLocaleName(out: *mut u16, size: i32) -> i32 {
    write_wide("en-US", out, size)
}

pub extern "system" fn GetUserDefaultGeoName(out: *mut u16, size: i32) -> i32 {
    write_wide("US", out, size)
}

pub extern "system" fn LocaleNameToLCID(_name: *const u16, _flags: u32) -> u32 {
    0x0409
}

const LOCALE_RETURN_NUMBER: u32 = 0x2000_0000;

// shortcut: en-US; only the separators and names titles format with.
pub extern "system" fn GetLocaleInfoEx(
    _name: *const u16,
    kind: u32,
    out: *mut u16,
    size: i32,
) -> i32 {
    let text = match kind & !LOCALE_RETURN_NUMBER {
        0x0E => ".",
        0x0F => ",",
        0x10 => "3;0",
        0x5C | 0x59 => "en-US",
        0x02 | 0x04 => "English (United States)",
        _ => {
            set_error(ERROR_INVALID_FLAGS);
            return 0;
        }
    };
    write_wide(text, out, size)
}

/// `FormatMessage` with no message table: the id as text.
pub extern "system" fn FormatMessageA(
    flags: u32,
    _source: *const c_void,
    id: u32,
    _language: u32,
    out: *mut u8,
    size: u32,
    _args: *const c_void,
) -> u32 {
    const ALLOCATE_BUFFER: u32 = 0x100;
    let text = format!("Error {id}\0");
    if flags & ALLOCATE_BUFFER != 0 {
        // The title frees it with `LocalFree`, which keeps nothing to free.
        let leaked: &'static mut [u8] = Box::leak(text.into_bytes().into_boxed_slice());
        // SAFETY: with ALLOCATE_BUFFER `out` is a `char **`.
        unsafe { out.cast::<*mut u8>().write(leaked.as_mut_ptr()) };
        return (leaked.len() - 1) as u32;
    }
    if text.len() > size as usize {
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    // SAFETY: the title's buffer of `size` bytes.
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len()) };
    (text.len() - 1) as u32
}

pub extern "system" fn LocalFree(memory: *mut c_void) -> *mut c_void {
    let _ = memory;
    std::ptr::null_mut()
}

pub extern "system" fn GlobalFree(memory: *mut c_void) -> *mut c_void {
    LocalFree(memory)
}

pub extern "system" fn EncodePointer(pointer: *mut c_void) -> *mut c_void {
    pointer
}

pub extern "system" fn DecodePointer(pointer: *mut c_void) -> *mut c_void {
    pointer
}

// Time.

pub(super) fn now_ticks() -> u64 {
    EPOCH_DIFFERENCE + (clock::unix_time().as_nanos() / 100) as u64
}

/// Seconds east of UTC at the current time.
fn local_offset() -> i64 {
    let now = (now_ticks() - EPOCH_DIFFERENCE) / TICKS_PER_SECOND;
    let mut tm = std::mem::MaybeUninit::<Tm>::zeroed();
    // SAFETY: `localtime_r` fills the structure it is given.
    unsafe {
        localtime_r(&raw const now as *const i64, tm.as_mut_ptr());
        tm.assume_init().gmtoff
    }
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `SYSTEMTIME`: year, month, day of week, day, hour, minute, second, ms.
type SystemTimeFields = [u16; 8];

fn to_system_time(ticks: u64) -> SystemTimeFields {
    let days = (ticks / TICKS_PER_DAY) as i64;
    let rest = ticks % TICKS_PER_DAY;
    let (y, m, d) = civil_from_days(days - DAYS_1601_TO_1970);
    let seconds = rest / TICKS_PER_SECOND;
    [
        y as u16,
        m as u16,
        ((days + 1) % 7) as u16,
        d as u16,
        (seconds / 3600) as u16,
        (seconds / 60 % 60) as u16,
        (seconds % 60) as u16,
        (rest % TICKS_PER_SECOND / 10_000) as u16,
    ]
}

fn from_system_time(t: &SystemTimeFields) -> Option<u64> {
    let [y, m, _, d, h, mi, s, ms] = *t;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 59 || ms > 999 {
        return None;
    }
    let days = days_from_civil(i64::from(y), u32::from(m), u32::from(d)) + DAYS_1601_TO_1970;
    let seconds = u64::from(h) * 3600 + u64::from(mi) * 60 + u64::from(s);
    Some(days as u64 * TICKS_PER_DAY + seconds * TICKS_PER_SECOND + u64::from(ms) * 10_000)
}

pub extern "system" fn GetSystemTimeAsFileTime(out: *mut u64) {
    // SAFETY: a FILETIME (two words, 4-byte aligned).
    unsafe { out.write_unaligned(now_ticks()) };
}

pub extern "system" fn GetSystemTimePreciseAsFileTime(out: *mut u64) {
    GetSystemTimeAsFileTime(out);
}

pub extern "system" fn GetSystemTime(out: *mut SystemTimeFields) {
    // SAFETY: a SYSTEMTIME.
    unsafe { out.write_unaligned(to_system_time(now_ticks())) };
}

pub extern "system" fn GetLocalTime(out: *mut SystemTimeFields) {
    let ticks = now_ticks().wrapping_add_signed(local_offset() * TICKS_PER_SECOND as i64);
    // SAFETY: a SYSTEMTIME.
    unsafe { out.write_unaligned(to_system_time(ticks)) };
}

pub extern "system" fn FileTimeToSystemTime(ft: *const u64, out: *mut SystemTimeFields) -> i32 {
    // SAFETY: a FILETIME and a SYSTEMTIME.
    unsafe { out.write_unaligned(to_system_time(ft.read_unaligned())) };
    1
}

pub extern "system" fn SystemTimeToFileTime(st: *const SystemTimeFields, out: *mut u64) -> i32 {
    // SAFETY: a SYSTEMTIME and a FILETIME.
    let fields = unsafe { st.read_unaligned() };
    match from_system_time(&fields) {
        Some(ticks) => {
            // SAFETY: as above.
            unsafe { out.write_unaligned(ticks) };
            1
        }
        None => {
            set_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

pub extern "system" fn FileTimeToLocalFileTime(ft: *const u64, out: *mut u64) -> i32 {
    // SAFETY: two FILETIMEs.
    unsafe {
        out.write_unaligned(
            ft.read_unaligned()
                .wrapping_add_signed(local_offset() * TICKS_PER_SECOND as i64),
        )
    };
    1
}

pub extern "system" fn LocalFileTimeToFileTime(ft: *const u64, out: *mut u64) -> i32 {
    // SAFETY: two FILETIMEs.
    unsafe {
        out.write_unaligned(
            ft.read_unaligned()
                .wrapping_add_signed(-local_offset() * TICKS_PER_SECOND as i64),
        )
    };
    1
}

/// `GetDateFormatEx` and `GetTimeFormatEx`: ISO 8601, whatever the format
/// asked for.
fn format_time(time: *const SystemTimeFields, date: bool, out: *mut u16, size: i32) -> i32 {
    let t = if time.is_null() {
        to_system_time(now_ticks().wrapping_add_signed(local_offset() * TICKS_PER_SECOND as i64))
    } else {
        // SAFETY: a SYSTEMTIME.
        unsafe { time.read_unaligned() }
    };
    let text = if date {
        format!("{:04}-{:02}-{:02}", t[0], t[1], t[3])
    } else {
        format!("{:02}:{:02}:{:02}", t[4], t[5], t[6])
    };
    write_wide(&text, out, size)
}

pub extern "system" fn GetDateFormatEx(
    _locale: *const u16,
    _flags: u32,
    time: *const SystemTimeFields,
    _format: *const u16,
    out: *mut u16,
    size: i32,
    _calendar: *const u16,
) -> i32 {
    format_time(time, true, out, size)
}

pub extern "system" fn GetTimeFormatEx(
    _locale: *const u16,
    _flags: u32,
    time: *const SystemTimeFields,
    _format: *const u16,
    out: *mut u16,
    size: i32,
) -> i32 {
    format_time(time, false, out, size)
}

// System information.

fn cpus() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}

pub extern "system" fn GetNativeSystemInfo(out: *mut u8) {
    // SYSTEM_INFO, 48 bytes.
    let n = cpus();
    let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    // SAFETY: the title's SYSTEM_INFO.
    unsafe {
        std::ptr::write_bytes(out, 0, 48);
        out.cast::<u16>().write_unaligned(9); // PROCESSOR_ARCHITECTURE_AMD64
        out.add(4).cast::<u32>().write_unaligned(4096);
        out.add(8).cast::<u64>().write_unaligned(0x1_0000);
        out.add(16).cast::<u64>().write_unaligned(0x7FFF_FFFE_FFFF);
        out.add(24).cast::<u64>().write_unaligned(mask);
        out.add(32).cast::<u32>().write_unaligned(n.min(64));
        out.add(36).cast::<u32>().write_unaligned(8664);
        out.add(40).cast::<u32>().write_unaligned(0x1_0000);
        out.add(44).cast::<u16>().write_unaligned(0x17);
    }
}

pub extern "system" fn GetSystemInfo(out: *mut u8) {
    GetNativeSystemInfo(out);
}

pub extern "system" fn GlobalMemoryStatusEx(out: *mut u8) -> i32 {
    let (fast, standard) = victory_gpu::console::current().title_memory();
    let total = (fast + standard) as u64;
    // MEMORYSTATUSEX, 64 bytes.
    // SAFETY: the title's structure.
    unsafe {
        out.add(4).cast::<u32>().write_unaligned(25);
        for (at, value) in [
            (8, total),
            (16, total * 3 / 4),
            (24, total),
            (32, total * 3 / 4),
            (40, 0x7FFF_FFFE_0000),
            (48, 0x7FFF_0000_0000),
            (56, 0),
        ] {
            out.add(at).cast::<u64>().write_unaligned(value);
        }
    }
    1
}

pub extern "system" fn GetLogicalProcessorInformation(out: *mut u8, length: *mut u32) -> i32 {
    // SYSTEM_LOGICAL_PROCESSOR_INFORMATION is 32 bytes: one entry per
    // core, and one for the package.
    let n = cpus().min(64);
    let needed = (n + 1) * 32;
    // SAFETY: the title's length word and buffer.
    unsafe {
        if length.read_unaligned() < needed {
            length.write_unaligned(needed);
            set_error(ERROR_INSUFFICIENT_BUFFER);
            return 0;
        }
        std::ptr::write_bytes(out, 0, needed as usize);
        for i in 0..n {
            out.add(i as usize * 32)
                .cast::<u64>()
                .write_unaligned(1 << i);
        }
        let package = out.add(n as usize * 32);
        package
            .cast::<u64>()
            .write_unaligned(if n == 64 { u64::MAX } else { (1 << n) - 1 });
        package.add(8).cast::<u32>().write_unaligned(3);
        length.write_unaligned(needed);
    }
    1
}

pub extern "system" fn IsProcessorFeaturePresent(feature: u32) -> i32 {
    // SSE and later, RDTSC, NX, cmpxchg16b, XSAVE, AVX2, RDRAND: the console's CPU has them.
    // Not `__fastfail` (23): a failure then goes through `abort`, which we log.
    i32::from(matches!(
        feature,
        6 | 8 | 10 | 12 | 13 | 14 | 17 | 28 | 36 | 37 | 38
    ))
}

pub extern "system" fn IsDebuggerPresent() -> i32 {
    0
}

pub extern "system" fn UnhandledExceptionFilter(_info: *const c_void) -> i32 {
    0 // EXCEPTION_CONTINUE_SEARCH
}

pub extern "system" fn RtlCaptureStackBackTrace(
    _skip: u32,
    _count: u32,
    _frames: *mut *mut c_void,
    hash: *mut u32,
) -> u16 {
    if !hash.is_null() {
        // SAFETY: the title's hash word.
        unsafe { hash.write(0) };
    }
    0
}

/// The old form of `RtlUnwindEx`, with no context to start from.
pub extern "system" fn RtlUnwind(
    target_frame: u64,
    target_ip: usize,
    record: *mut seh::context::ExceptionRecord,
    value: u64,
) {
    // SAFETY: the title's own call; the unwinder checks what it walks.
    unsafe {
        seh::api::unwind_ex(
            target_frame,
            target_ip,
            record,
            value,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    }
}

pub extern "system" fn WerRegisterFile(_file: *const u16, _kind: u32, _flags: u32) -> i32 {
    0
}

// Processes and threads.

pub extern "system" fn GetCurrentProcess() -> isize {
    -1
}

pub extern "system" fn GetCurrentThread() -> isize {
    -2
}

pub extern "system" fn GetProcessId(_process: isize) -> u32 {
    std::process::id()
}

pub extern "system" fn OpenThread(_access: u32, _inherit: i32, _id: u32) -> *mut c_void {
    set_error(ERROR_ACCESS_DENIED);
    std::ptr::null_mut()
}

pub extern "system" fn GetThreadPriority(_thread: *mut c_void) -> i32 {
    0
}

pub extern "system" fn SetThreadPriority(_thread: *mut c_void, _priority: i32) -> i32 {
    1
}

pub extern "system" fn SetProcessPriorityBoost(_process: isize, _disable: i32) -> i32 {
    1
}

pub extern "system" fn SetThreadIdealProcessorEx(
    _thread: *mut c_void,
    _ideal: *const u8,
    previous: *mut u8,
) -> i32 {
    if !previous.is_null() {
        // SAFETY: a PROCESSOR_NUMBER (group u16, number u8, reserved u8).
        unsafe { std::ptr::write_bytes(previous, 0, 4) };
    }
    1
}

pub extern "system" fn GetThreadGroupAffinity(_thread: *mut c_void, out: *mut u8) -> i32 {
    let n = cpus().min(64);
    // GROUP_AFFINITY: Mask u64, Group u16, 6 reserved bytes.
    // SAFETY: the title's structure.
    unsafe {
        std::ptr::write_bytes(out, 0, 16);
        out.cast::<u64>()
            .write_unaligned(if n == 64 { u64::MAX } else { (1 << n) - 1 });
    }
    1
}

pub extern "system" fn SetThreadGroupAffinity(
    _thread: *mut c_void,
    _affinity: *const u8,
    previous: *mut u8,
) -> i32 {
    if !previous.is_null() {
        GetThreadGroupAffinity(_thread, previous);
    }
    1
}

pub extern "system" fn SetThreadAffinityMask(_thread: *mut c_void, _mask: usize) -> usize {
    usize::MAX >> (64 - cpus().min(64))
}

pub extern "system" fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize) {
    // SAFETY: the calling thread's own TEB, and the title's two words.
    unsafe {
        let teb = teb::current();
        high.write(teb.add(teb::OFF_STACK_BASE).cast::<usize>().read());
        low.write(teb.add(teb::OFF_STACK_LIMIT).cast::<usize>().read());
    }
}

pub extern "system" fn GetStartupInfoW(out: *mut u8) {
    // STARTUPINFOW is 104 bytes, `cb` first.
    // SAFETY: the title's structure.
    unsafe {
        std::ptr::write_bytes(out, 0, 104);
        out.cast::<u32>().write_unaligned(104);
    }
}

fn command_line() -> &'static [u16] {
    static LINE: LazyLock<Vec<u16>> = LazyLock::new(|| {
        let name = crate::runtime::main_image()
            .and_then(crate::runtime::image_path)
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "title.exe".into());
        format!("\"{name}\"").encode_utf16().chain([0]).collect()
    });
    &LINE
}

pub extern "system" fn GetCommandLineW() -> *const u16 {
    command_line().as_ptr()
}

pub extern "system" fn GetCommandLineA() -> *const u8 {
    static LINE: LazyLock<Vec<u8>> = LazyLock::new(|| {
        let text = String::from_utf16_lossy(&command_line()[..command_line().len() - 1]);
        text.into_bytes().into_iter().chain([0]).collect()
    });
    LINE.as_ptr()
}

pub extern "system" fn GetModuleHandleExW(
    flags: u32,
    name: *const u16,
    out: *mut *mut c_void,
) -> i32 {
    const FROM_ADDRESS: u32 = 4;
    let module = if flags & FROM_ADDRESS != 0 {
        seh::unwind::image_base_of(name as usize).unwrap_or(0) as *mut c_void
    } else {
        crate::modules::kernel::GetModuleHandleW(name)
    };
    if module.is_null() {
        set_error(ERROR_MOD_NOT_FOUND);
        return 0;
    }
    if !out.is_null() {
        // SAFETY: the title's output word.
        unsafe { out.write(module) };
    }
    1
}

/// `MEMORY_BASIC_INFORMATION` (48 bytes) of the page at `address`.
pub extern "system" fn VirtualQuery(address: *const c_void, out: *mut u8, length: usize) -> usize {
    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    const MEM_FREE: u32 = 0x10000;
    const MEM_PRIVATE: u32 = 0x20000;
    if length < 48 || out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let page = mem::page_size();
    let at = address as usize & !(page - 1);
    let (base, size, state, protect) = match mem::next_region(at) {
        Some(r) if r.base <= at => {
            let flags = match r.protect {
                Protect::NO_ACCESS => 0x01,
                Protect::READ => 0x02,
                Protect::READ_WRITE => 0x04,
                Protect::READ_EXECUTE => 0x20,
                _ => 0x40,
            };
            let state = if r.protect == Protect::NO_ACCESS {
                MEM_RESERVE
            } else {
                MEM_COMMIT
            };
            (r.base, r.base + r.len - at, state, flags)
        }
        next => (
            at,
            next.map_or(1 << 47, |r| r.base).saturating_sub(at),
            MEM_FREE,
            0x01,
        ),
    };
    // SAFETY: the title's 48-byte structure.
    unsafe {
        std::ptr::write_bytes(out, 0, 48);
        out.cast::<usize>().write_unaligned(at);
        out.add(8)
            .cast::<usize>()
            .write_unaligned(if state == MEM_FREE { 0 } else { base });
        out.add(16)
            .cast::<u32>()
            .write_unaligned(if state == MEM_FREE { 0 } else { protect });
        out.add(24).cast::<usize>().write_unaligned(size);
        out.add(32).cast::<u32>().write_unaligned(state);
        out.add(36)
            .cast::<u32>()
            .write_unaligned(if state == MEM_FREE { 1 } else { protect });
        out.add(40)
            .cast::<u32>()
            .write_unaligned(if state == MEM_FREE { 0 } else { MEM_PRIVATE });
    }
    48
}

pub extern "system" fn VirtualQueryEx(
    _process: isize,
    address: *const c_void,
    out: *mut u8,
    length: usize,
) -> usize {
    VirtualQuery(address, out, length)
}

// Synchronisation objects.

/// A mutex object: owned by a thread, recursive, released by that thread.
pub(crate) struct MutexObject {
    free: Semaphore,
    owner: AtomicU32,
    count: AtomicU32,
}

impl MutexObject {
    fn new() -> MutexObject {
        MutexObject {
            free: Semaphore::new(1, 1).expect("semaphore"),
            owner: AtomicU32::new(0),
            count: AtomicU32::new(0),
        }
    }

    /// Takes it for the calling thread (again, if it has it).
    pub(crate) fn acquire(&self, timeout: Option<Duration>) -> bool {
        let me = thread::current_id();
        if self.owner.load(Ordering::Acquire) == me {
            self.count.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        if !self.free.wait(timeout) {
            return false;
        }
        self.owner.store(me, Ordering::Release);
        self.count.store(1, Ordering::Relaxed);
        true
    }

    /// One release by the owner; false if the caller is not.
    pub(crate) fn release(&self) -> bool {
        if self.owner.load(Ordering::Acquire) != thread::current_id() {
            return false;
        }
        if self.count.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.owner.store(0, Ordering::Release);
            self.free.release(1);
        }
        true
    }
}

pub extern "system" fn CreateMutexW(
    _attributes: *const c_void,
    initial_owner: i32,
    _name: *const u16,
) -> *mut c_void {
    let mutex = Arc::new(MutexObject::new());
    if initial_owner != 0 {
        mutex.acquire(Some(Duration::ZERO));
    }
    new_handle(Object::Mutex(mutex))
}

pub extern "system" fn CreateMutexA(
    attributes: *const c_void,
    initial_owner: i32,
    _name: *const u8,
) -> *mut c_void {
    CreateMutexW(attributes, initial_owner, std::ptr::null())
}

pub extern "system" fn CreateMutexExW(
    attributes: *const c_void,
    name: *const u16,
    flags: u32,
    _access: u32,
) -> *mut c_void {
    CreateMutexW(attributes, i32::from(flags & 1 != 0), name)
}

pub extern "system" fn ReleaseMutex(handle: *mut c_void) -> i32 {
    let mutex = match lock(&super::HANDLES).get(&(handle as usize)) {
        Some(Object::Mutex(m)) => Some(m.clone()),
        _ => None,
    };
    let Some(mutex) = mutex else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    if !mutex.release() {
        set_error(ERROR_NOT_OWNER);
        return 0;
    }
    super::sync::signalled();
    1
}

pub extern "system" fn CreateEventA(
    attributes: *const c_void,
    manual_reset: i32,
    initial: i32,
    _name: *const u8,
) -> *mut c_void {
    super::CreateEventW(attributes, manual_reset, initial, std::ptr::null())
}

pub extern "system" fn CreateEventExW(
    attributes: *const c_void,
    _name: *const u16,
    flags: u32,
    _access: u32,
) -> *mut c_void {
    // CREATE_EVENT_MANUAL_RESET 1, CREATE_EVENT_INITIAL_SET 2.
    super::CreateEventW(
        attributes,
        i32::from(flags & 1 != 0),
        i32::from(flags & 2 != 0),
        std::ptr::null(),
    )
}

pub extern "system" fn CreateSemaphoreExW(
    attributes: *const c_void,
    initial: i32,
    max: i32,
    _name: *const u16,
    _flags: u32,
    _access: u32,
) -> *mut c_void {
    super::CreateSemaphoreW(attributes, initial, max, std::ptr::null())
}

/// A waitable timer: an event a thread of ours sets when it is due.
/// `generation` retires the timer thread of an earlier `SetWaitableTimer`.
pub(crate) struct TimerObject {
    pub(crate) event: Arc<Event>,
    generation: AtomicU64,
}

pub extern "system" fn CreateWaitableTimerW(
    _attributes: *const c_void,
    manual_reset: i32,
    _name: *const u16,
) -> *mut c_void {
    let Ok(event) = Event::new(manual_reset != 0, false) else {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    };
    new_handle(Object::Timer(Arc::new(TimerObject {
        event: Arc::new(event),
        generation: AtomicU64::new(0),
    })))
}

pub extern "system" fn CreateWaitableTimerExW(
    attributes: *const c_void,
    name: *const u16,
    flags: u32,
    _access: u32,
) -> *mut c_void {
    CreateWaitableTimerW(attributes, i32::from(flags & 1 != 0), name)
}

/// How long until `due` (negative: relative, 100 ns; positive: an absolute FILETIME).
fn until(due: i64) -> Duration {
    let ticks = if due < 0 {
        due.unsigned_abs()
    } else {
        // shortcut: waits use host time at debug rates, scale wait durations if virtual deadlines must match them.
        (due as u64).saturating_sub(now_ticks())
    };
    Duration::from_nanos(ticks.saturating_mul(100))
}

fn timer_of(handle: *mut c_void) -> Option<Arc<TimerObject>> {
    match lock(&super::HANDLES).get(&(handle as usize)) {
        Some(Object::Timer(t)) => Some(t.clone()),
        _ => None,
    }
}

// shortcut: the completion routine of `SetWaitableTimer` never runs (it
// needs an alertable wait); the event is what titles wait on.
pub extern "system" fn SetWaitableTimer(
    handle: *mut c_void,
    due: *const i64,
    period_ms: i32,
    _routine: *const c_void,
    _arg: *const c_void,
    _resume: i32,
) -> i32 {
    let Some(timer) = timer_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    let generation = timer.generation.fetch_add(1, Ordering::AcqRel) + 1;
    timer.event.reset();
    // SAFETY: the title's due time.
    let first = until(unsafe { due.read_unaligned() });
    let period = Duration::from_millis(period_ms.max(0) as u64);
    std::thread::spawn(move || {
        let live = || timer.generation.load(Ordering::Acquire) == generation;
        std::thread::sleep(first);
        while live() {
            timer.event.set();
            super::sync::signalled();
            if period.is_zero() {
                break;
            }
            std::thread::sleep(period);
        }
    });
    1
}

pub extern "system" fn SetWaitableTimerEx(
    handle: *mut c_void,
    due: *const i64,
    period_ms: i32,
    routine: *const c_void,
    arg: *const c_void,
    _context: *const c_void,
    _tolerable: u32,
) -> i32 {
    SetWaitableTimer(handle, due, period_ms, routine, arg, 0)
}

pub extern "system" fn CancelWaitableTimer(handle: *mut c_void) -> i32 {
    let Some(timer) = timer_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    timer.generation.fetch_add(1, Ordering::AcqRel);
    1
}

// Interlocked singly linked lists: the header is ours (head pointer, depth),
// every access goes through one lock.

static SLISTS: Mutex<()> = Mutex::new(());

pub extern "system" fn InitializeSListHead(head: *mut usize) {
    // SAFETY: a 16-byte SLIST_HEADER.
    unsafe {
        head.write(0);
        head.add(1).write(0);
    }
}

pub extern "system" fn InterlockedPushEntrySList(
    head: *mut usize,
    entry: *mut usize,
) -> *mut usize {
    let _held = lock(&SLISTS);
    // SAFETY: the header and the entry (whose first word links to the next).
    unsafe {
        let first = head.read();
        entry.write(first);
        head.write(entry as usize);
        head.add(1).write(head.add(1).read() + 1);
        first as *mut usize
    }
}

pub extern "system" fn InterlockedPopEntrySList(head: *mut usize) -> *mut usize {
    let _held = lock(&SLISTS);
    // SAFETY: as above.
    unsafe {
        let first = head.read() as *mut usize;
        if !first.is_null() {
            head.write(first.read());
            head.add(1).write(head.add(1).read() - 1);
        }
        first
    }
}

pub extern "system" fn InterlockedFlushSList(head: *mut usize) -> *mut usize {
    let _held = lock(&SLISTS);
    // SAFETY: as above.
    unsafe {
        let first = head.read();
        head.write(0);
        head.add(1).write(0);
        first as *mut usize
    }
}

pub extern "system" fn QueryDepthSList(head: *const usize) -> u16 {
    let _held = lock(&SLISTS);
    // SAFETY: as above.
    unsafe { head.add(1).read() as u16 }
}

/// `InitOnceExecuteOnce`: the word holds 0 (new), 1 (running) or the
/// callback's context with 2 set (done).
pub extern "system" fn InitOnceExecuteOnce(
    once: *mut AtomicU64,
    init: usize,
    parameter: *mut c_void,
    context: *mut *mut c_void,
) -> i32 {
    // SAFETY: the title's INIT_ONCE, a pointer-sized word.
    let word = unsafe { &*once };
    loop {
        match word.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(state) if state & 3 == 2 => {
                if !context.is_null() {
                    // SAFETY: the title's output word.
                    unsafe { context.write((state & !3) as *mut c_void) };
                }
                return 1;
            }
            Err(_) => std::thread::sleep(Duration::from_micros(200)),
        }
    }
    let mut result: *mut c_void = std::ptr::null_mut();
    // SAFETY: the title's own init routine, `BOOL (INIT_ONCE *, PVOID, PVOID *)`.
    let ok = unsafe {
        native::call_win64(
            init,
            &[once as u64, parameter as u64, &raw mut result as u64],
        )
    } as u32
        != 0;
    if ok {
        word.store(result as u64 | 2, Ordering::Release);
        if !context.is_null() {
            // SAFETY: the title's output word.
            unsafe { context.write(result) };
        }
        1
    } else {
        word.store(0, Ordering::Release);
        0
    }
}

// Fiber local storage, per thread (a fiber is a thread here).

const FLS_OUT_OF_INDEXES: u32 = 0xFFFF_FFFF;
const FLS_SLOTS: usize = 4096;

static FLS_USED: Mutex<Vec<bool>> = Mutex::new(Vec::new());

thread_local! {
    static FLS_VALUES: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}

// shortcut: the destructor callback is not called (at thread end or `FlsFree`).
pub extern "system" fn FlsAlloc(_callback: usize) -> u32 {
    let mut used = lock(&FLS_USED);
    let free = used.iter().position(|u| !u).unwrap_or(used.len());
    if free >= FLS_SLOTS {
        set_error(ERROR_INVALID_PARAMETER);
        return FLS_OUT_OF_INDEXES;
    }
    if free == used.len() {
        used.push(true);
    }
    used[free] = true;
    FLS_VALUES.with(|v| {
        if let Some(slot) = v.borrow_mut().get_mut(free) {
            *slot = 0;
        }
    });
    free as u32
}

pub extern "system" fn FlsFree(index: u32) -> i32 {
    match lock(&FLS_USED).get_mut(index as usize) {
        Some(used) if *used => {
            *used = false;
            1
        }
        _ => {
            set_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

pub extern "system" fn FlsGetValue(index: u32) -> usize {
    set_error(0);
    FLS_VALUES.with(|v| v.borrow().get(index as usize).copied().unwrap_or(0))
}

pub extern "system" fn FlsSetValue(index: u32, value: usize) -> i32 {
    if !lock(&FLS_USED).get(index as usize).is_some_and(|u| *u) {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    FLS_VALUES.with(|v| {
        let mut v = v.borrow_mut();
        if v.len() <= index as usize {
            v.resize(index as usize + 1, 0);
        }
        v[index as usize] = value;
    });
    1
}

// The thread pool: one thread per submission or timer firing (the real one
// reuses threads; titles cannot tell). Callbacks are title code, called with
// the Windows ABI as `(instance, context, object)`.

pub(crate) struct PoolObject {
    callback: usize,
    context: usize,
    /// Callbacks queued or running, for the waits.
    pending: Mutex<u32>,
    idle: Condvar,
    /// Retires the timer thread of an earlier `SetThreadpoolTimer`.
    generation: AtomicU64,
}

/// What a callback gets as its instance: nothing it can use, but not null.
static INSTANCE: u64 = 0;

fn new_pool_object(callback: usize, context: usize) -> *mut PoolObject {
    Box::into_raw(Box::new(PoolObject {
        callback,
        context,
        pending: Mutex::new(0),
        idle: Condvar::new(),
        generation: AtomicU64::new(0),
    }))
}

/// Runs the callback of `object` on a thread of its own after `delay`.
fn run_callback(object: usize, delay: Duration, period: Duration, generation: Option<u64>) {
    // SAFETY: pool objects are leaked: valid for the life of the process.
    let o = unsafe { &*(object as *const PoolObject) };
    *lock(&o.pending) += 1;
    std::thread::spawn(move || {
        // SAFETY: as above.
        let o = unsafe { &*(object as *const PoolObject) };
        std::thread::sleep(delay);
        loop {
            if generation.is_some_and(|g| o.generation.load(Ordering::Acquire) != g) {
                break;
            }
            // SAFETY: the title's `TP_WORK_CALLBACK` or `TP_TIMER_CALLBACK`.
            unsafe {
                native::call_win64(
                    o.callback,
                    &[&raw const INSTANCE as u64, o.context as u64, object as u64],
                )
            };
            if period.is_zero() {
                break;
            }
            std::thread::sleep(period);
        }
        *lock(&o.pending) -= 1;
        o.idle.notify_all();
    });
}

fn wait_idle(object: *const PoolObject) {
    // SAFETY: as above.
    let o = unsafe { &*object };
    let mut pending = lock(&o.pending);
    while *pending > 0 {
        pending = o.idle.wait(pending).unwrap_or_else(|e| e.into_inner());
    }
}

pub extern "system" fn CreateThreadpoolWork(
    callback: usize,
    context: usize,
    _environment: *const c_void,
) -> *mut PoolObject {
    new_pool_object(callback, context)
}

pub extern "system" fn SubmitThreadpoolWork(work: *mut PoolObject) {
    run_callback(work as usize, Duration::ZERO, Duration::ZERO, None);
}

pub extern "system" fn WaitForThreadpoolWorkCallbacks(work: *mut PoolObject, _cancel: i32) {
    wait_idle(work);
}

pub extern "system" fn CloseThreadpoolWork(work: *mut PoolObject) {
    wait_idle(work);
}

pub extern "system" fn CreateThreadpoolTimer(
    callback: usize,
    context: usize,
    _environment: *const c_void,
) -> *mut PoolObject {
    new_pool_object(callback, context)
}

pub extern "system" fn SetThreadpoolTimer(
    timer: *mut PoolObject,
    due: *const i64,
    period_ms: u32,
    _window_ms: u32,
) {
    // SAFETY: as above.
    let o = unsafe { &*timer };
    let generation = o.generation.fetch_add(1, Ordering::AcqRel) + 1;
    if due.is_null() {
        return;
    }
    // SAFETY: the title's due time.
    let delay = until(unsafe { due.read_unaligned() });
    run_callback(
        timer as usize,
        delay,
        Duration::from_millis(u64::from(period_ms)),
        Some(generation),
    );
}

pub extern "system" fn WaitForThreadpoolTimerCallbacks(timer: *mut PoolObject, cancel: i32) {
    if cancel != 0 {
        // SAFETY: as above.
        unsafe { &*timer }.generation.fetch_add(1, Ordering::AcqRel);
    }
    wait_idle(timer);
}

pub extern "system" fn CloseThreadpoolTimer(timer: *mut PoolObject) {
    WaitForThreadpoolTimerCallbacks(timer, 1);
}

pub extern "system" fn CallbackMayRunLong(_instance: *const c_void) -> i32 {
    0
}

pub extern "system" fn DisassociateCurrentThreadFromCallback(_instance: *const c_void) {}

pub extern "system" fn FreeLibraryWhenCallbackReturns(
    _instance: *const c_void,
    _module: *mut c_void,
) {
}

// Files the file module has no answer for.

pub extern "system" fn GetFileInformationByHandle(handle: *mut c_void, out: *mut u8) -> i32 {
    let Some(m) = files::metadata_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    use std::os::unix::fs::MetadataExt;
    // BY_HANDLE_FILE_INFORMATION, 52 bytes.
    // SAFETY: the title's structure.
    unsafe {
        let put = |at: usize, v: u32| out.add(at).cast::<u32>().write_unaligned(v);
        let put64 = |at: usize, v: u64| out.add(at).cast::<u64>().write_unaligned(v);
        put(0, files::attributes_of(&m));
        put64(4, files::filetime(m.created()));
        put64(12, files::filetime(m.accessed()));
        put64(20, files::filetime(m.modified()));
        put(28, 1);
        put(32, (m.len() >> 32) as u32);
        put(36, m.len() as u32);
        put(40, m.nlink() as u32);
        put(44, (m.ino() >> 32) as u32);
        put(48, m.ino() as u32);
    }
    1
}

pub extern "system" fn GetFileTime(
    handle: *mut c_void,
    created: *mut u64,
    accessed: *mut u64,
    written: *mut u64,
) -> i32 {
    let Some(m) = files::metadata_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    for (out, time) in [
        (created, m.created()),
        (accessed, m.accessed()),
        (written, m.modified()),
    ] {
        if !out.is_null() {
            // SAFETY: the title's FILETIME or null.
            unsafe { out.write_unaligned(files::filetime(time)) };
        }
    }
    1
}

// shortcut: times and attributes are not changed (the host's own stay).
pub extern "system" fn SetFileTime(
    _handle: *mut c_void,
    _created: *const u64,
    _accessed: *const u64,
    _written: *const u64,
) -> i32 {
    1
}

pub extern "system" fn SetFileAttributesW(_path: *const u16, _attributes: u32) -> i32 {
    1
}

pub extern "system" fn GetDiskFreeSpaceExW(
    _path: *const u16,
    available: *mut u64,
    total: *mut u64,
    free: *mut u64,
) -> i32 {
    const TB: u64 = 1 << 40;
    for (out, value) in [(available, TB / 2), (total, TB), (free, TB / 2)] {
        if !out.is_null() {
            // SAFETY: the title's output word or null.
            unsafe { out.write_unaligned(value) };
        }
    }
    1
}

pub extern "system" fn GetTempPathW(size: u32, out: *mut u16) -> u32 {
    // The console's temporary files drive.
    let text: Vec<u16> = r"T:\".encode_utf16().chain([0]).collect();
    if (size as usize) < text.len() {
        return text.len() as u32;
    }
    // SAFETY: the title's buffer of `size` units.
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len()) };
    (text.len() - 1) as u32
}

pub extern "system" fn MoveFileExW(from: *const u16, to: *const u16, _flags: u32) -> i32 {
    let (Some(from), Some(to)) = (files::wide_path(from), files::wide_path(to)) else {
        set_error(ERROR_FILE_NOT_FOUND);
        return 0;
    };
    match std::fs::rename(&from, &to) {
        Ok(()) => 1,
        Err(e) => {
            set_error(files::win32_error(&e, &from));
            0
        }
    }
}

pub extern "system" fn CreateFile2(
    path: *const u16,
    access: u32,
    share: u32,
    disposition: u32,
    _parameters: *const c_void,
) -> *mut c_void {
    files::CreateFileW(
        path,
        access,
        share,
        std::ptr::null(),
        disposition,
        0,
        std::ptr::null_mut(),
    )
}

pub extern "system" fn CopyFile2(
    from: *const u16,
    to: *const u16,
    _parameters: *const c_void,
) -> i32 {
    let (Some(from), Some(to)) = (files::wide_path(from), files::wide_path(to)) else {
        return 0x8007_0002_u32 as i32; // HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)
    };
    match std::fs::copy(&from, &to) {
        Ok(_) => 0,
        Err(e) => 0x8007_0000_u32 as i32 | files::win32_error(&e, &from) as i32,
    }
}

pub extern "system" fn CreateHardLinkW(
    _link: *const u16,
    _target: *const u16,
    _security: *const c_void,
) -> i32 {
    set_error(ERROR_NOT_SUPPORTED);
    0
}

pub extern "system" fn CreateSymbolicLinkW(
    _link: *const u16,
    _target: *const u16,
    _flags: u32,
) -> u8 {
    set_error(ERROR_NOT_SUPPORTED);
    0
}

// Named pipes: the console has none to connect to.

pub extern "system" fn SetNamedPipeHandleState(
    _pipe: *mut c_void,
    _mode: *const u32,
    _max_collection: *const u32,
    _timeout: *const u32,
) -> i32 {
    set_error(ERROR_INVALID_HANDLE);
    0
}

pub extern "system" fn WaitNamedPipeW(_name: *const u16, _timeout: u32) -> i32 {
    set_error(ERROR_FILE_NOT_FOUND);
    0
}

// Process state notifications: the title is told of nothing.

pub extern "system" fn RegisterAppStateChangeNotification(
    _routine: usize,
    _context: *const c_void,
    registration: *mut *mut c_void,
) -> u32 {
    if !registration.is_null() {
        // SAFETY: the title's output word.
        unsafe { registration.write(0x10 as *mut c_void) };
    }
    0
}

pub extern "system" fn UnregisterAppStateChangeNotification(_registration: *mut c_void) {}

pub extern "system" fn RegisterAppConstrainedChangeNotification(
    routine: usize,
    context: *const c_void,
    registration: *mut *mut c_void,
) -> u32 {
    RegisterAppStateChangeNotification(routine, context, registration)
}

pub extern "system" fn UnregisterAppConstrainedChangeNotification(_registration: *mut c_void) {}

// COM: nothing is registered.

pub extern "system" fn CoInitializeEx(_reserved: *const c_void, _model: u32) -> i32 {
    0
}

pub extern "system" fn CoUninitialize() {}

pub extern "system" fn CoCreateInstance(
    _class: *const c_void,
    _outer: *const c_void,
    _context: u32,
    _interface: *const c_void,
    out: *mut *mut c_void,
) -> i32 {
    if !out.is_null() {
        // SAFETY: the title's output pointer.
        unsafe { out.write(std::ptr::null_mut()) };
    }
    0x8004_0154_u32 as i32 // REGDB_E_CLASSNOTREG
}

pub extern "system" fn CoTaskMemFree(_memory: *mut c_void) {}

pub extern "system" fn CoCreateGuid(out: *mut u8) -> i32 {
    // SAFETY: a 16-byte GUID.
    unsafe {
        arc4random_buf(out.cast(), 16);
        // Version 4, variant 1.
        *out.add(7) = *out.add(7) & 0x0F | 0x40;
        *out.add(8) = *out.add(8) & 0x3F | 0x80;
    }
    0
}

pub extern "system" fn StringFromGUID2(guid: *const u8, out: *mut u16, size: i32) -> i32 {
    // SAFETY: a 16-byte GUID.
    let g = unsafe { std::slice::from_raw_parts(guid, 16) };
    let text = format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        u32::from_le_bytes([g[0], g[1], g[2], g[3]]),
        u16::from_le_bytes([g[4], g[5]]),
        u16::from_le_bytes([g[6], g[7]]),
        g[8],
        g[9],
        g[10],
        g[11],
        g[12],
        g[13],
        g[14],
        g[15]
    );
    let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
    if (size as usize) < units.len() {
        return 0;
    }
    // SAFETY: the title's buffer of `size` units.
    unsafe { std::ptr::copy_nonoverlapping(units.as_ptr(), out, units.len()) };
    units.len() as i32
}

pub extern "system" fn PropVariantClear(variant: *mut u8) -> i32 {
    // SAFETY: a 24-byte PROPVARIANT; clearing leaves VT_EMPTY.
    unsafe { std::ptr::write_bytes(variant, 0, 24) };
    0
}

// Registry: no keys, but the one the title reads its device type from (see
// `modules::xsystem`).

const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
const OEM_KEY: &str = r"Software\Microsoft\Windows NT\CurrentVersion\OEM";
/// Stands for the opened OEM key.
static OEM: u8 = 0;

fn wide_is(text: *const u16, expected: &str) -> bool {
    if text.is_null() {
        return false;
    }
    // SAFETY: the title passes a NUL-terminated string.
    let text = unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    };
    text.eq_ignore_ascii_case(expected)
}

fn open_key(key: isize, sub: *const u16, result: *mut *mut c_void) -> i32 {
    if key == HKEY_LOCAL_MACHINE && wide_is(sub, OEM_KEY) && !result.is_null() {
        // SAFETY: the title passes room for a key.
        unsafe { result.write((&raw const OEM).cast_mut().cast()) };
        return 0;
    }
    ERROR_FILE_NOT_FOUND as i32
}

pub extern "system" fn RegOpenKeyExW(
    key: isize,
    sub_key: *const u16,
    _options: u32,
    _access: u32,
    result: *mut *mut c_void,
) -> i32 {
    open_key(key, sub_key, result)
}

pub extern "system" fn RegOpenKeyW(
    key: isize,
    sub_key: *const u16,
    result: *mut *mut c_void,
) -> i32 {
    open_key(key, sub_key, result)
}

pub extern "system" fn RegQueryValueExW(
    key: isize,
    name: *const u16,
    _reserved: *const u32,
    kind: *mut u32,
    data: *mut u8,
    size: *mut u32,
) -> i32 {
    if key != (&raw const OEM) as isize || !wide_is(name, "ConsoleMode") {
        return ERROR_FILE_NOT_FOUND as i32;
    }
    const ERROR_MORE_DATA: i32 = 234;
    // SAFETY: the title passes valid or null pointers, sized by `size`.
    unsafe {
        if !kind.is_null() {
            kind.write(4); // REG_DWORD
        }
        if size.is_null() {
            return 0;
        }
        let room = size.read();
        size.write(4);
        if data.is_null() {
            return 0;
        }
        if room < 4 {
            return ERROR_MORE_DATA;
        }
        data.cast::<u32>().write_unaligned(crate::modules::xsystem::console_mode(
            victory_gpu::console::current(),
        ));
    }
    0
}

pub extern "system" fn RegCloseKey(_key: isize) -> i32 {
    0
}

// ETW: no listeners, so every event is dropped.

pub extern "system" fn EventRegister(
    _provider: *const c_void,
    _callback: usize,
    _context: *const c_void,
    handle: *mut u64,
) -> u32 {
    if !handle.is_null() {
        // SAFETY: the title's REGHANDLE.
        unsafe { handle.write(0) };
    }
    0
}

pub extern "system" fn EventUnregister(_handle: u64) -> u32 {
    0
}

pub extern "system" fn EventSetInformation(
    _handle: u64,
    _class: u32,
    _info: *const c_void,
    _len: u32,
) -> u32 {
    0
}

pub extern "system" fn EventWriteTransfer(
    _handle: u64,
    _descriptor: *const c_void,
    _activity: *const c_void,
    _related: *const c_void,
    _count: u32,
    _data: *const c_void,
) -> u32 {
    0
}

// bcrypt.

pub extern "system" fn BCryptOpenAlgorithmProvider(
    handle: *mut *mut c_void,
    _algorithm: *const u16,
    _implementation: *const u16,
    _flags: u32,
) -> i32 {
    // SAFETY: the title's output word. Only the RNG is real: the handle
    // is an opaque non-null value.
    unsafe { handle.write(0x20 as *mut c_void) };
    0
}

pub extern "system" fn BCryptCloseAlgorithmProvider(_handle: *mut c_void, _flags: u32) -> i32 {
    0
}

pub extern "system" fn BCryptGenRandom(
    _algorithm: *mut c_void,
    buffer: *mut u8,
    len: u32,
    _flags: u32,
) -> i32 {
    // SAFETY: the title's buffer of `len` bytes.
    unsafe { arc4random_buf(buffer.cast(), len as usize) };
    0
}

// crypt32: no certificates.

pub extern "system" fn CertOpenSystemStoreW(_provider: usize, _store: *const u16) -> *mut c_void {
    set_error(ERROR_FILE_NOT_FOUND);
    std::ptr::null_mut()
}

pub extern "system" fn CertCloseStore(_store: *mut c_void, _flags: u32) -> i32 {
    1
}

pub extern "system" fn CertFindChainInStore(
    _store: *mut c_void,
    _encoding: u32,
    _flags: u32,
    _kind: u32,
    _parameter: *const c_void,
    _previous: *const c_void,
) -> *const c_void {
    set_error(ERROR_FILE_NOT_FOUND);
    std::ptr::null()
}

pub extern "system" fn CertFreeCertificateChain(_chain: *const c_void) {}

// user32: the console has one window of the title's own; these are for
// libraries that make a message window and pump it.

static WINDOWS: AtomicU64 = AtomicU64::new(1);

pub extern "system" fn RegisterClassExA(_class: *const c_void) -> u16 {
    0xC001
}

pub extern "system" fn CreateWindowExA(
    _ex_style: u32,
    _class: *const u8,
    _name: *const u8,
    _style: u32,
    _x: i32,
    _y: i32,
    _width: i32,
    _height: i32,
    _parent: *mut c_void,
    _menu: *mut c_void,
    _instance: *mut c_void,
    _param: *mut c_void,
) -> *mut c_void {
    ((WINDOWS.fetch_add(1, Ordering::Relaxed) as usize) << 4) as *mut c_void
}

pub extern "system" fn DefWindowProcA(
    _window: *mut c_void,
    _message: u32,
    _w: usize,
    _l: isize,
) -> isize {
    0
}

pub extern "system" fn ShowWindow(_window: *mut c_void, _command: i32) -> i32 {
    0
}

pub extern "system" fn PeekMessageA(
    _message: *mut c_void,
    _window: *mut c_void,
    _min: u32,
    _max: u32,
    _remove: u32,
) -> i32 {
    0
}

pub extern "system" fn TranslateMessage(_message: *const c_void) -> i32 {
    0
}

pub extern "system" fn DispatchMessageA(_message: *const c_void) -> isize {
    0
}

/// The function table of the DLLs this module answers for.
pub(super) fn lookup(dll: &str, name: &str) -> Option<usize> {
    macro_rules! table {
        ($($f:ident),* $(,)?) => {
            match name {
                $(stringify!($f) => Some($f as *const () as usize),)*
                _ => None,
            }
        };
    }
    let kernel = dll == "kernel32.dll"
        || dll == "kernelbase.dll"
        || dll.starts_with("api-ms-win-core-")
        || dll.starts_with("api-ms-win-eventing-");
    if kernel {
        return table!(
            MultiByteToWideChar,
            WideCharToMultiByte,
            lstrcmpW,
            CompareStringEx,
            GetStringTypeW,
            GetCPInfo,
            LCMapStringEx,
            GetUserDefaultLocaleName,
            GetUserDefaultGeoName,
            LocaleNameToLCID,
            GetLocaleInfoEx,
            FormatMessageA,
            LocalFree,
            GlobalFree,
            EncodePointer,
            DecodePointer,
            GetSystemTimeAsFileTime,
            GetSystemTimePreciseAsFileTime,
            GetSystemTime,
            GetLocalTime,
            FileTimeToSystemTime,
            SystemTimeToFileTime,
            FileTimeToLocalFileTime,
            LocalFileTimeToFileTime,
            GetDateFormatEx,
            GetTimeFormatEx,
            GetNativeSystemInfo,
            GetSystemInfo,
            GlobalMemoryStatusEx,
            GetLogicalProcessorInformation,
            IsProcessorFeaturePresent,
            IsDebuggerPresent,
            UnhandledExceptionFilter,
            RtlCaptureStackBackTrace,
            RtlUnwind,
            WerRegisterFile,
            GetCurrentProcess,
            GetCurrentThread,
            GetProcessId,
            OpenThread,
            GetThreadPriority,
            SetThreadPriority,
            SetProcessPriorityBoost,
            SetThreadIdealProcessorEx,
            GetThreadGroupAffinity,
            SetThreadGroupAffinity,
            SetThreadAffinityMask,
            GetCurrentThreadStackLimits,
            GetStartupInfoW,
            GetCommandLineW,
            GetCommandLineA,
            GetModuleHandleExW,
            VirtualQuery,
            VirtualQueryEx,
            CreateMutexW,
            CreateMutexA,
            CreateMutexExW,
            ReleaseMutex,
            CreateEventA,
            CreateEventExW,
            CreateSemaphoreExW,
            CreateWaitableTimerW,
            CreateWaitableTimerExW,
            SetWaitableTimer,
            SetWaitableTimerEx,
            CancelWaitableTimer,
            InitializeSListHead,
            InterlockedPushEntrySList,
            InterlockedPopEntrySList,
            InterlockedFlushSList,
            QueryDepthSList,
            InitOnceExecuteOnce,
            FlsAlloc,
            FlsFree,
            FlsGetValue,
            FlsSetValue,
            CreateThreadpoolWork,
            SubmitThreadpoolWork,
            WaitForThreadpoolWorkCallbacks,
            CloseThreadpoolWork,
            CreateThreadpoolTimer,
            SetThreadpoolTimer,
            WaitForThreadpoolTimerCallbacks,
            CloseThreadpoolTimer,
            CallbackMayRunLong,
            DisassociateCurrentThreadFromCallback,
            FreeLibraryWhenCallbackReturns,
            GetFileInformationByHandle,
            GetFileTime,
            SetFileTime,
            SetFileAttributesW,
            GetDiskFreeSpaceExW,
            GetTempPathW,
            MoveFileExW,
            CreateFile2,
            CopyFile2,
            CreateHardLinkW,
            CreateSymbolicLinkW,
            SetNamedPipeHandleState,
            WaitNamedPipeW,
            RegisterAppStateChangeNotification,
            UnregisterAppStateChangeNotification,
            RegisterAppConstrainedChangeNotification,
            UnregisterAppConstrainedChangeNotification,
            CoInitializeEx,
            CoUninitialize,
            CoCreateInstance,
            CoTaskMemFree,
            CoCreateGuid,
            StringFromGUID2,
            PropVariantClear,
            EventRegister,
            EventUnregister,
            EventSetInformation,
            EventWriteTransfer,
        );
    }
    if dll == "advapi32.dll" || dll == "api-ms-win-eventing-provider-l1-1-0.dll" {
        return table!(
            RegOpenKeyExW,
            RegOpenKeyW,
            RegQueryValueExW,
            RegCloseKey,
            EventRegister,
            EventUnregister,
            EventSetInformation,
            EventWriteTransfer,
        );
    }
    if dll == "bcrypt.dll" {
        return table!(
            BCryptOpenAlgorithmProvider,
            BCryptCloseAlgorithmProvider,
            BCryptGenRandom
        );
    }
    if dll == "crypt32.dll" {
        return table!(
            CertOpenSystemStoreW,
            CertCloseStore,
            CertFindChainInStore,
            CertFreeCertificateChain
        );
    }
    if dll.starts_with("ext-ms-win-rtcore-ntuser-") || dll == "user32.dll" {
        return table!(
            RegisterClassExA,
            CreateWindowExA,
            DefWindowProcA,
            ShowWindow,
            PeekMessageA,
            TranslateMessage,
            DispatchMessageA,
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_times_and_system_times_round_trip() {
        // 2024-02-29 13:45:07.250, a Thursday.
        let st: SystemTimeFields = [2024, 2, 4, 29, 13, 45, 7, 250];
        let ticks = from_system_time(&st).unwrap();
        assert_eq!(to_system_time(ticks), st);
        // The Unix epoch is 116444736000000000 ticks after 1601.
        assert_eq!(
            from_system_time(&[1970, 1, 4, 1, 0, 0, 0, 0]),
            Some(EPOCH_DIFFERENCE)
        );
        assert_eq!(from_system_time(&[2024, 13, 0, 1, 0, 0, 0, 0]), None);
    }

    #[test]
    fn strings_convert_and_report_the_size_needed() {
        let text = b"h\xC3\xA9llo\0";
        let n = MultiByteToWideChar(65001, 0, text.as_ptr(), -1, std::ptr::null_mut(), 0);
        assert_eq!(n, 6);
        let mut wide = [0u16; 6];
        assert_eq!(
            MultiByteToWideChar(65001, 0, text.as_ptr(), -1, wide.as_mut_ptr(), 6),
            6
        );
        assert_eq!(wide[1], 0xE9);
        let mut small = [0u16; 2];
        assert_eq!(
            MultiByteToWideChar(65001, 0, text.as_ptr(), -1, small.as_mut_ptr(), 2),
            0
        );
        let mut back = [0u8; 8];
        assert_eq!(
            WideCharToMultiByte(
                65001,
                0,
                wide.as_ptr(),
                6,
                back.as_mut_ptr(),
                8,
                std::ptr::null(),
                std::ptr::null_mut()
            ),
            7
        );
        assert_eq!(&back[..7], b"h\xC3\xA9llo\0");
    }

    #[test]
    fn a_mutex_is_recursive_and_only_its_owner_releases_it() {
        let m = Arc::new(MutexObject::new());
        assert!(m.acquire(None));
        assert!(m.acquire(None));
        let other = m.clone();
        let blocked = std::thread::spawn(move || other.acquire(Some(Duration::from_millis(20))));
        assert!(!blocked.join().unwrap());
        assert!(m.release());
        assert!(m.release());
        assert!(!m.release());
        let other = m.clone();
        assert!(
            std::thread::spawn(move || other.acquire(Some(Duration::ZERO)))
                .join()
                .unwrap()
        );
    }

    #[test]
    fn interlocked_lists_are_last_in_first_out() {
        let mut head = [0usize; 2];
        let mut a = [0usize; 2];
        let mut b = [0usize; 2];
        InterlockedPushEntrySList(head.as_mut_ptr(), a.as_mut_ptr());
        InterlockedPushEntrySList(head.as_mut_ptr(), b.as_mut_ptr());
        assert_eq!(QueryDepthSList(head.as_ptr()), 2);
        assert_eq!(InterlockedPopEntrySList(head.as_mut_ptr()), b.as_mut_ptr());
        assert_eq!(InterlockedFlushSList(head.as_mut_ptr()), a.as_mut_ptr());
        assert!(InterlockedPopEntrySList(head.as_mut_ptr()).is_null());
    }

    #[test]
    fn fiber_local_storage_is_per_thread() {
        let slot = FlsAlloc(0);
        assert_ne!(slot, FLS_OUT_OF_INDEXES);
        assert_eq!(FlsSetValue(slot, 7), 1);
        let other = std::thread::spawn(move || FlsGetValue(slot))
            .join()
            .unwrap();
        assert_eq!((FlsGetValue(slot), other), (7, 0));
        assert_eq!(FlsFree(slot), 1);
        assert_eq!(FlsSetValue(slot, 1), 0);
    }

    #[test]
    fn a_waitable_timer_signals_when_due() {
        let timer = CreateWaitableTimerW(std::ptr::null(), 1, std::ptr::null());
        let due = -(TICKS_PER_SECOND as i64) / 100; // 10 ms
        assert_eq!(
            SetWaitableTimer(timer, &due, 0, std::ptr::null(), std::ptr::null(), 0),
            1
        );
        assert_eq!(super::super::WaitForSingleObject(timer, 5000), 0);
        let start = std::time::Instant::now();
        assert_eq!(CancelWaitableTimer(timer), 1);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}

#[cfg(test)]
mod console_mode_tests {
    use super::*;

    #[test]
    fn device_type_key_answers_console_mode() {
        let w = |s: &str| -> Vec<u16> { s.encode_utf16().chain([0]).collect() };
        let mut key = std::ptr::null_mut();
        assert_eq!(RegOpenKeyW(HKEY_LOCAL_MACHINE, w("software\\other").as_ptr(), &mut key), 2);
        assert_eq!(RegOpenKeyW(HKEY_LOCAL_MACHINE, w(OEM_KEY).as_ptr(), &mut key), 0);
        let (mut kind, mut value, mut size) = (0u32, 0u32, 4u32);
        let got = RegQueryValueExW(
            key as isize,
            w("ConsoleMode").as_ptr(),
            std::ptr::null(),
            &mut kind,
            (&raw mut value).cast(),
            &mut size,
        );
        assert_eq!((got, kind, size), (0, 4, 4));
        assert_eq!(value, crate::modules::xsystem::console_mode(victory_gpu::console::current()));
    }
}
