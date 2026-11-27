//! `api-ms-win-crt-string`, `-utility` and the C-locale half of `-locale`:
//! the `mem*`/`str*`/`wcs*` functions (wide strings are UTF-16), the secure
//! `_s` variants, `ctype`, `qsort`/`bsearch` (callbacks run in title code),
//! `rand` (the CRT's own LCG, so a seeded sequence matches Windows) and
//! `setlocale`/`localeconv` for the "C" locale.

use std::cell::Cell as StdCell;
use std::ffi::{c_char, c_void};

use super::runtime::bad_param;
use super::{
    Ch, EINVAL, ERANGE, STRUNCATE, TRUNCATE, exports, leak_cstr, leak_wstr, libc, set_errno, units,
};
use victory_loader::native::call_win64;

// memory

pub extern "system" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if n == 0 {
        return dst;
    }
    // SAFETY: the title's buffers; they do not overlap by contract.
    unsafe { std::ptr::copy_nonoverlapping(src, dst, n) };
    dst
}

pub extern "system" fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if n == 0 {
        return dst;
    }
    // SAFETY: the title's buffers.
    unsafe { std::ptr::copy(src, dst, n) };
    dst
}

pub extern "system" fn memset(dst: *mut u8, c: i32, n: usize) -> *mut u8 {
    if n == 0 {
        return dst;
    }
    // SAFETY: the title's buffer.
    unsafe { dst.write_bytes(c as u8, n) };
    dst
}

fn sign(o: std::cmp::Ordering) -> i32 {
    o as i32
}

pub extern "system" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    if n == 0 {
        return 0;
    }
    // SAFETY: the title's buffers.
    let (a, b) = unsafe {
        (
            std::slice::from_raw_parts(a, n),
            std::slice::from_raw_parts(b, n),
        )
    };
    sign(a.cmp(b))
}

pub extern "system" fn memchr(p: *const u8, c: i32, n: usize) -> *const u8 {
    if n == 0 {
        return std::ptr::null();
    }
    // SAFETY: the title's buffer.
    let s = unsafe { std::slice::from_raw_parts(p, n) };
    s.iter()
        .position(|&b| b == c as u8)
        .map_or(std::ptr::null(), |i| p.wrapping_add(i))
}

pub extern "system" fn _memicmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    if n == 0 {
        return 0;
    }
    // SAFETY: the title's buffers.
    let (a, b) = unsafe {
        (
            std::slice::from_raw_parts(a, n),
            std::slice::from_raw_parts(b, n),
        )
    };
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (x.to_ascii_lowercase(), y.to_ascii_lowercase());
        if x != y {
            return if x < y { -1 } else { 1 };
        }
    }
    0
}

pub extern "system" fn memcpy_s(dst: *mut u8, dst_size: usize, src: *const u8, n: usize) -> i32 {
    if n == 0 {
        return 0;
    }
    if dst.is_null() {
        return bad_param(EINVAL);
    }
    if src.is_null() {
        // SAFETY: the title's buffer.
        unsafe { dst.write_bytes(0, dst_size) };
        return bad_param(EINVAL);
    }
    if dst_size < n {
        // SAFETY: the title's buffer.
        unsafe { dst.write_bytes(0, dst_size) };
        return bad_param(ERANGE);
    }
    memmove(dst, src, n);
    0
}

pub extern "system" fn memmove_s(dst: *mut u8, dst_size: usize, src: *const u8, n: usize) -> i32 {
    if n == 0 {
        return 0;
    }
    // Unlike memcpy_s, a failed memmove_s leaves the destination as it was.
    if dst.is_null() || src.is_null() {
        return bad_param(EINVAL);
    }
    if dst_size < n {
        return bad_param(ERANGE);
    }
    memmove(dst, src, n);
    0
}

pub extern "system" fn _memccpy(dst: *mut u8, src: *const u8, c: i32, n: usize) -> *mut u8 {
    for i in 0..n {
        // SAFETY: within the title's buffers.
        unsafe {
            let b = src.add(i).read();
            dst.add(i).write(b);
            if b == c as u8 {
                return dst.add(i + 1);
            }
        }
    }
    std::ptr::null_mut()
}

// Case: the "C" locale folds ASCII for bytes, and Unicode (one-to-one) for wide units.

fn lower<C: Ch>(c: C) -> u32 {
    let v = c.get();
    if v < 128 {
        (v as u8).to_ascii_lowercase().into()
    } else if std::mem::size_of::<C>() == 2 {
        char::from_u32(v).map_or(v, |ch| {
            let mut it = ch.to_lowercase();
            match (it.next(), it.next()) {
                (Some(l), None) if (l as u32) < 0x10000 => l as u32,
                _ => v,
            }
        })
    } else {
        v
    }
}

fn upper<C: Ch>(c: C) -> u32 {
    let v = c.get();
    if v < 128 {
        (v as u8).to_ascii_uppercase().into()
    } else if std::mem::size_of::<C>() == 2 {
        char::from_u32(v).map_or(v, |ch| {
            let mut it = ch.to_uppercase();
            match (it.next(), it.next()) {
                (Some(u), None) if (u as u32) < 0x10000 => u as u32,
                _ => v,
            }
        })
    } else {
        v
    }
}

// Generic string functions, wrapped per width below.

unsafe fn g_len<C: Ch>(s: *const C) -> usize {
    // SAFETY: a NUL-terminated string.
    unsafe { units(s).len() }
}

/// `-1`, `0`, `1` comparing at most `n` units; `fold` ignores case.
unsafe fn g_cmp<C: Ch>(a: *const C, b: *const C, n: usize, fold: bool) -> i32 {
    for i in 0..n {
        // SAFETY: strings the title passes, read up to their NULs.
        let (x, y) = unsafe { (*a.add(i), *b.add(i)) };
        let (u, v) = if fold {
            (lower(x), lower(y))
        } else {
            (x.get(), y.get())
        };
        if u != v {
            return if u < v { -1 } else { 1 };
        }
        if x.get() == 0 {
            break;
        }
    }
    0
}

unsafe fn g_cpy<C: Ch>(dst: *mut C, src: *const C) -> *mut C {
    // SAFETY: the title's buffers.
    unsafe {
        let n = g_len(src) + 1;
        std::ptr::copy(src, dst, n);
    }
    dst
}

unsafe fn g_ncpy<C: Ch>(dst: *mut C, src: *const C, n: usize) -> *mut C {
    // SAFETY: the title's buffers; pads with NULs as `strncpy` does.
    unsafe {
        let len = len_max(src, n);
        if len > 0 {
            std::ptr::copy(src, dst, len);
        }
        for i in len..n {
            dst.add(i).write(C::default());
        }
    }
    dst
}

unsafe fn g_cat<C: Ch>(dst: *mut C, src: *const C) -> *mut C {
    // SAFETY: the title's buffers.
    unsafe { g_cpy(dst.add(g_len(dst)), src) };
    dst
}

unsafe fn g_ncat<C: Ch>(dst: *mut C, src: *const C, n: usize) -> *mut C {
    // SAFETY: the title's buffers.
    unsafe {
        let end = dst.add(g_len(dst));
        let len = len_max(src, n);
        if len > 0 {
            std::ptr::copy(src, end, len);
        }
        end.add(len).write(C::default());
    }
    dst
}

unsafe fn g_chr<C: Ch>(s: *const C, c: i32) -> *const C {
    // SAFETY: a NUL-terminated string; the terminator matches `c == 0`.
    let u = unsafe { units(s) };
    let c = C::make(c as u32);
    if c.get() == 0 {
        return s.wrapping_add(u.len());
    }
    u.iter()
        .position(|&x| x == c)
        .map_or(std::ptr::null(), |i| s.wrapping_add(i))
}

unsafe fn g_rchr<C: Ch>(s: *const C, c: i32) -> *const C {
    // SAFETY: a NUL-terminated string.
    let u = unsafe { units(s) };
    let c = C::make(c as u32);
    if c.get() == 0 {
        return s.wrapping_add(u.len());
    }
    u.iter()
        .rposition(|&x| x == c)
        .map_or(std::ptr::null(), |i| s.wrapping_add(i))
}

unsafe fn g_str<C: Ch>(h: *const C, n: *const C) -> *const C {
    // SAFETY: NUL-terminated strings.
    let (h_u, n_u) = unsafe { (units(h), units(n)) };
    if n_u.is_empty() {
        return h;
    }
    h_u.windows(n_u.len())
        .position(|w| w == n_u)
        .map_or(std::ptr::null(), |i| h.wrapping_add(i))
}

unsafe fn g_spn<C: Ch>(s: *const C, set: *const C, inside: bool) -> usize {
    // SAFETY: NUL-terminated strings.
    let (s, set) = unsafe { (units(s), units(set)) };
    s.iter().take_while(|c| set.contains(c) == inside).count()
}

unsafe fn g_pbrk<C: Ch>(s: *const C, set: *const C) -> *const C {
    // SAFETY: NUL-terminated strings.
    let (u, set_u) = unsafe { (units(s), units(set)) };
    u.iter()
        .position(|c| set_u.contains(c))
        .map_or(std::ptr::null(), |i| s.wrapping_add(i))
}

/// `strtok_s` over either width: `ctx` holds the rest of the string.
unsafe fn g_tok<C: Ch>(s: *mut C, delim: *const C, ctx: *mut *mut C) -> *mut C {
    // SAFETY: the title's string and context.
    unsafe {
        let mut p = if s.is_null() { *ctx } else { s };
        if p.is_null() {
            return std::ptr::null_mut();
        }
        let d = units(delim);
        while p.read().get() != 0 && d.contains(&p.read()) {
            p = p.add(1);
        }
        if p.read().get() == 0 {
            *ctx = p;
            return std::ptr::null_mut();
        }
        let start = p;
        while p.read().get() != 0 && !d.contains(&p.read()) {
            p = p.add(1);
        }
        if p.read().get() != 0 {
            p.write(C::default());
            p = p.add(1);
        }
        *ctx = p;
        start
    }
}

/// The `_s` copy: `count` units of `src` (all of it for `None` and `_TRUNCATE`'s
/// sentinel), into a buffer of `size` units.
unsafe fn g_cpy_s<C: Ch>(dst: *mut C, size: usize, src: *const C, count: Option<usize>) -> i32 {
    if dst.is_null() || size == 0 {
        return bad_param(EINVAL);
    }
    // SAFETY: the title's buffers.
    unsafe {
        if src.is_null() {
            dst.write(C::default());
            return bad_param(EINVAL);
        }
        g_put_s(dst, dst, size, src, count)
    }
}

unsafe fn g_cat_s<C: Ch>(dst: *mut C, size: usize, src: *const C, count: Option<usize>) -> i32 {
    if dst.is_null() || size == 0 {
        return bad_param(EINVAL);
    }
    // SAFETY: the title's buffers.
    unsafe {
        let used = units_in(dst, size);
        let Some(used) = used else {
            dst.write(C::default());
            return bad_param(EINVAL);
        };
        if src.is_null() {
            dst.write(C::default());
            return bad_param(EINVAL);
        }
        g_put_s(dst, dst.add(used), size - used, src, count)
    }
}

/// Writes `count` units of `src` and a NUL at `at`, which has `room` (at least one) units
/// left of the destination `dst`. Reads no more than `room` units: that many without a NUL
/// already mean `ERANGE` (one more than fits tells a `_TRUNCATE`). An error empties the
/// whole destination, as the CRT does, so a failed append leaves no old string behind.
unsafe fn g_put_s<C: Ch>(
    dst: *mut C,
    at: *mut C,
    room: usize,
    src: *const C,
    count: Option<usize>,
) -> i32 {
    // SAFETY: the title's buffers, `room` units from `at`.
    unsafe {
        let (take, truncating) = match count {
            None => (len_max(src, room), false),
            Some(TRUNCATE) => {
                let len = len_max(src, room);
                (len.min(room - 1), len > room - 1)
            }
            Some(n) => (len_max(src, n.min(room)), false),
        };
        if take >= room {
            dst.write(C::default());
            return bad_param(ERANGE);
        }
        if take > 0 {
            std::ptr::copy(src, at, take);
        }
        at.add(take).write(C::default());
        if truncating {
            set_errno(STRUNCATE);
            return STRUNCATE;
        }
    }
    0
}

/// Length of the string in a buffer of `size` units, or `None` if it has no NUL.
unsafe fn units_in<C: Ch>(p: *const C, size: usize) -> Option<usize> {
    // SAFETY: a buffer of `size` units.
    (0..size).find(|&i| unsafe { p.add(i).read() }.get() == 0)
}

/// Length of `s`, reading no more than `max` units: the counted functions
/// take sources that need no NUL within their count (a fixed-size field).
unsafe fn len_max<C: Ch>(s: *const C, max: usize) -> usize {
    // SAFETY: at least `max` units or a NUL before them.
    unsafe { units_in(s, max) }.unwrap_or(max)
}

// Narrow.

pub extern "system" fn strlen(s: *const u8) -> usize {
    // SAFETY: a NUL-terminated string.
    unsafe { g_len(s) }
}

pub extern "system" fn strnlen(s: *const u8, max: usize) -> usize {
    if s.is_null() {
        return 0;
    }
    // SAFETY: reads up to the NUL or `max`.
    (0..max).take_while(|&i| unsafe { *s.add(i) } != 0).count()
}

pub extern "system" fn strcpy(d: *mut u8, s: *const u8) -> *mut u8 {
    // SAFETY: the title's buffers.
    unsafe { g_cpy(d, s) }
}

pub extern "system" fn strncpy(d: *mut u8, s: *const u8, n: usize) -> *mut u8 {
    // SAFETY: the title's buffers.
    unsafe { g_ncpy(d, s, n) }
}

pub extern "system" fn strcat(d: *mut u8, s: *const u8) -> *mut u8 {
    // SAFETY: the title's buffers.
    unsafe { g_cat(d, s) }
}

pub extern "system" fn strncat(d: *mut u8, s: *const u8, n: usize) -> *mut u8 {
    // SAFETY: the title's buffers.
    unsafe { g_ncat(d, s, n) }
}

pub extern "system" fn strcmp(a: *const u8, b: *const u8) -> i32 {
    // SAFETY: NUL-terminated strings.
    unsafe { g_cmp(a, b, usize::MAX, false) }
}

pub extern "system" fn strncmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    // SAFETY: NUL-terminated strings.
    unsafe { g_cmp(a, b, n, false) }
}

pub extern "system" fn _stricmp(a: *const u8, b: *const u8) -> i32 {
    // SAFETY: NUL-terminated strings.
    unsafe { g_cmp(a, b, usize::MAX, true) }
}

pub extern "system" fn _strnicmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    // SAFETY: NUL-terminated strings.
    unsafe { g_cmp(a, b, n, true) }
}

pub extern "system" fn strchr(s: *const u8, c: i32) -> *const u8 {
    // SAFETY: a NUL-terminated string.
    unsafe { g_chr(s, c) }
}

pub extern "system" fn strrchr(s: *const u8, c: i32) -> *const u8 {
    // SAFETY: a NUL-terminated string.
    unsafe { g_rchr(s, c) }
}

pub extern "system" fn strstr(h: *const u8, n: *const u8) -> *const u8 {
    // SAFETY: NUL-terminated strings.
    unsafe { g_str(h, n) }
}

pub extern "system" fn strspn(s: *const u8, set: *const u8) -> usize {
    // SAFETY: NUL-terminated strings.
    unsafe { g_spn(s, set, true) }
}

pub extern "system" fn strcspn(s: *const u8, set: *const u8) -> usize {
    // SAFETY: NUL-terminated strings.
    unsafe { g_spn(s, set, false) }
}

pub extern "system" fn strpbrk(s: *const u8, set: *const u8) -> *const u8 {
    // SAFETY: NUL-terminated strings.
    unsafe { g_pbrk(s, set) }
}

thread_local! {
    static STRTOK: StdCell<*mut u8> = const { StdCell::new(std::ptr::null_mut()) };
    static WCSTOK: StdCell<*mut u16> = const { StdCell::new(std::ptr::null_mut()) };
}

pub extern "system" fn strtok_s(s: *mut u8, delim: *const u8, ctx: *mut *mut u8) -> *mut u8 {
    // SAFETY: the title's context pointer, checked non-null first.
    if delim.is_null() || ctx.is_null() || (s.is_null() && unsafe { (*ctx).is_null() }) {
        set_errno(EINVAL);
        return std::ptr::null_mut();
    }
    // SAFETY: the title's strings and context.
    unsafe { g_tok(s, delim, ctx) }
}

pub extern "system" fn strtok(s: *mut u8, delim: *const u8) -> *mut u8 {
    let mut rest = STRTOK.get();
    // SAFETY: the title's string; the context is this thread's.
    let r = unsafe { g_tok(s, delim, &mut rest) };
    STRTOK.set(rest);
    r
}

pub extern "system" fn strcpy_s(d: *mut u8, size: usize, s: *const u8) -> i32 {
    // SAFETY: the title's buffers.
    unsafe { g_cpy_s(d, size, s, None) }
}

pub extern "system" fn strncpy_s(d: *mut u8, size: usize, s: *const u8, n: usize) -> i32 {
    // SAFETY: the title's buffers.
    unsafe { g_cpy_s(d, size, s, Some(n)) }
}

pub extern "system" fn strcat_s(d: *mut u8, size: usize, s: *const u8) -> i32 {
    // SAFETY: the title's buffers.
    unsafe { g_cat_s(d, size, s, None) }
}

pub extern "system" fn strncat_s(d: *mut u8, size: usize, s: *const u8, n: usize) -> i32 {
    // SAFETY: the title's buffers.
    unsafe { g_cat_s(d, size, s, Some(n)) }
}

pub extern "system" fn _strdup(s: *const u8) -> *mut u8 {
    if s.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: a NUL-terminated string; the copy is `malloc`'d for `free`.
    unsafe {
        let n = g_len(s) + 1;
        let p = libc::malloc(n).cast::<u8>();
        if !p.is_null() {
            std::ptr::copy_nonoverlapping(s, p, n);
        }
        p
    }
}

fn map_in_place<C: Ch>(s: *mut C, f: impl Fn(C) -> u32) -> *mut C {
    // SAFETY: a NUL-terminated string.
    unsafe {
        for i in 0..g_len(s) {
            s.add(i).write(C::make(f(s.add(i).read())));
        }
    }
    s
}

pub extern "system" fn _strlwr(s: *mut u8) -> *mut u8 {
    map_in_place(s, lower)
}

pub extern "system" fn _strupr(s: *mut u8) -> *mut u8 {
    map_in_place(s, upper)
}

pub extern "system" fn _strlwr_s(s: *mut u8, size: usize) -> i32 {
    // SAFETY: a buffer of `size`.
    if s.is_null() || unsafe { units_in(s, size) }.is_none() {
        return bad_param(EINVAL);
    }
    map_in_place(s, lower);
    0
}

pub extern "system" fn _strupr_s(s: *mut u8, size: usize) -> i32 {
    // SAFETY: a buffer of `size`.
    if s.is_null() || unsafe { units_in(s, size) }.is_none() {
        return bad_param(EINVAL);
    }
    map_in_place(s, upper);
    0
}

fn rev<C: Ch>(s: *mut C) -> *mut C {
    // SAFETY: a NUL-terminated string.
    unsafe { std::slice::from_raw_parts_mut(s, g_len(s)) }.reverse();
    s
}

pub extern "system" fn _strrev(s: *mut u8) -> *mut u8 {
    rev(s)
}

pub extern "system" fn _strset(s: *mut u8, c: i32) -> *mut u8 {
    map_in_place(s, |_| c as u32 & 0xFF)
}

pub extern "system" fn _strnset(s: *mut u8, c: i32, n: usize) -> *mut u8 {
    // SAFETY: a NUL-terminated string.
    unsafe {
        for i in 0..g_len(s).min(n) {
            s.add(i).write(c as u8);
        }
    }
    s
}

/// Byte order is collation order in the "C" locale.
pub extern "system" fn strxfrm(d: *mut u8, s: *const u8, n: usize) -> usize {
    // SAFETY: the title's buffers.
    unsafe {
        let len = g_len(s);
        if n > len {
            std::ptr::copy(s, d, len + 1);
        }
        len
    }
}

const ERRORS: [&str; 43] = [
    "No error",
    "Operation not permitted",
    "No such file or directory",
    "No such process",
    "Interrupted function call",
    "Input/output error",
    "No such device or address",
    "Arg list too long",
    "Exec format error",
    "Bad file descriptor",
    "No child processes",
    "Resource temporarily unavailable",
    "Not enough space",
    "Permission denied",
    "Bad address",
    "Unknown error",
    "Resource device",
    "File exists",
    "Improper link",
    "No such device",
    "Not a directory",
    "Is a directory",
    "Invalid argument",
    "Too many open files in system",
    "Too many open files",
    "Inappropriate I/O control operation",
    "Unknown error",
    "File too large",
    "No space left on device",
    "Invalid seek",
    "Read-only file system",
    "Too many links",
    "Broken pipe",
    "Numerical argument out of domain",
    "Result too large",
    "Unknown error",
    "Resource deadlock avoided",
    "Unknown error",
    "Filename too long",
    "No locks available",
    "Function not implemented",
    "Directory not empty",
    "Illegal byte sequence",
];

fn error_text(e: i32) -> &'static str {
    usize::try_from(e)
        .ok()
        .and_then(|i| ERRORS.get(i))
        .copied()
        .unwrap_or("Unknown error")
}

pub extern "system" fn strerror(e: i32) -> *const u8 {
    // One leaked string per distinct message.
    thread_local! { static TEXTS: std::cell::RefCell<Vec<(i32, *const u8)>> = const { std::cell::RefCell::new(Vec::new()) }; }
    TEXTS.with(|t| {
        let mut t = t.borrow_mut();
        if let Some((_, p)) = t.iter().find(|(k, _)| *k == e) {
            return *p;
        }
        let p = leak_cstr(error_text(e).as_bytes()).cast_const();
        t.push((e, p));
        p
    })
}

pub extern "system" fn _strerror(msg: *const u8) -> *const u8 {
    if msg.is_null() {
        return strerror(super::errno());
    }
    // SAFETY: a NUL-terminated message.
    let m = String::from_utf8_lossy(unsafe { super::bytes(msg) }).into_owned();
    leak_cstr(format!("{m}: {}\n", error_text(super::errno())).as_bytes()).cast_const()
}

pub extern "system" fn strerror_s(buf: *mut u8, size: usize, e: i32) -> i32 {
    // SAFETY: the title's buffer.
    unsafe { g_cpy_s(buf, size, strerror(e), Some(TRUNCATE)) }
}

// Wide.

pub extern "system" fn wcslen(s: *const u16) -> usize {
    // SAFETY: a NUL-terminated string.
    unsafe { g_len(s) }
}

pub extern "system" fn wcsnlen(s: *const u16, max: usize) -> usize {
    if s.is_null() {
        return 0;
    }
    // SAFETY: reads up to the NUL or `max`.
    (0..max).take_while(|&i| unsafe { *s.add(i) } != 0).count()
}

pub extern "system" fn wcscpy(d: *mut u16, s: *const u16) -> *mut u16 {
    // SAFETY: the title's buffers.
    unsafe { g_cpy(d, s) }
}

pub extern "system" fn wcsncpy(d: *mut u16, s: *const u16, n: usize) -> *mut u16 {
    // SAFETY: the title's buffers.
    unsafe { g_ncpy(d, s, n) }
}

pub extern "system" fn wcscat(d: *mut u16, s: *const u16) -> *mut u16 {
    // SAFETY: the title's buffers.
    unsafe { g_cat(d, s) }
}

pub extern "system" fn wcsncat(d: *mut u16, s: *const u16, n: usize) -> *mut u16 {
    // SAFETY: the title's buffers.
    unsafe { g_ncat(d, s, n) }
}

pub extern "system" fn wcscmp(a: *const u16, b: *const u16) -> i32 {
    // SAFETY: NUL-terminated strings.
    unsafe { g_cmp(a, b, usize::MAX, false) }
}

pub extern "system" fn wcsncmp(a: *const u16, b: *const u16, n: usize) -> i32 {
    // SAFETY: NUL-terminated strings.
    unsafe { g_cmp(a, b, n, false) }
}

pub extern "system" fn _wcsicmp(a: *const u16, b: *const u16) -> i32 {
    // SAFETY: NUL-terminated strings.
    unsafe { g_cmp(a, b, usize::MAX, true) }
}

pub extern "system" fn _wcsnicmp(a: *const u16, b: *const u16, n: usize) -> i32 {
    // SAFETY: NUL-terminated strings.
    unsafe { g_cmp(a, b, n, true) }
}

pub extern "system" fn wcschr(s: *const u16, c: u16) -> *const u16 {
    // SAFETY: a NUL-terminated string.
    unsafe { g_chr(s, i32::from(c)) }
}

pub extern "system" fn wcsrchr(s: *const u16, c: u16) -> *const u16 {
    // SAFETY: a NUL-terminated string.
    unsafe { g_rchr(s, i32::from(c)) }
}

pub extern "system" fn wcsstr(h: *const u16, n: *const u16) -> *const u16 {
    // SAFETY: NUL-terminated strings.
    unsafe { g_str(h, n) }
}

pub extern "system" fn wcsspn(s: *const u16, set: *const u16) -> usize {
    // SAFETY: NUL-terminated strings.
    unsafe { g_spn(s, set, true) }
}

pub extern "system" fn wcscspn(s: *const u16, set: *const u16) -> usize {
    // SAFETY: NUL-terminated strings.
    unsafe { g_spn(s, set, false) }
}

pub extern "system" fn wcspbrk(s: *const u16, set: *const u16) -> *const u16 {
    // SAFETY: NUL-terminated strings.
    unsafe { g_pbrk(s, set) }
}

pub extern "system" fn wcstok_s(s: *mut u16, delim: *const u16, ctx: *mut *mut u16) -> *mut u16 {
    // SAFETY: the title's context pointer, checked non-null first.
    if delim.is_null() || ctx.is_null() || (s.is_null() && unsafe { (*ctx).is_null() }) {
        set_errno(EINVAL);
        return std::ptr::null_mut();
    }
    // SAFETY: the title's strings and context.
    unsafe { g_tok(s, delim, ctx) }
}

/// The three-argument `wcstok`; a null context is the thread's own.
pub extern "system" fn wcstok(s: *mut u16, delim: *const u16, ctx: *mut *mut u16) -> *mut u16 {
    if !ctx.is_null() {
        return wcstok_s(s, delim, ctx);
    }
    let mut rest = WCSTOK.get();
    // SAFETY: the title's string; the context is this thread's.
    let r = unsafe { g_tok(s, delim, &mut rest) };
    WCSTOK.set(rest);
    r
}

pub extern "system" fn wcscpy_s(d: *mut u16, size: usize, s: *const u16) -> i32 {
    // SAFETY: the title's buffers.
    unsafe { g_cpy_s(d, size, s, None) }
}

pub extern "system" fn wcsncpy_s(d: *mut u16, size: usize, s: *const u16, n: usize) -> i32 {
    // SAFETY: the title's buffers.
    unsafe { g_cpy_s(d, size, s, Some(n)) }
}

pub extern "system" fn wcscat_s(d: *mut u16, size: usize, s: *const u16) -> i32 {
    // SAFETY: the title's buffers.
    unsafe { g_cat_s(d, size, s, None) }
}

pub extern "system" fn wcsncat_s(d: *mut u16, size: usize, s: *const u16, n: usize) -> i32 {
    // SAFETY: the title's buffers.
    unsafe { g_cat_s(d, size, s, Some(n)) }
}

pub extern "system" fn _wcsdup(s: *const u16) -> *mut u16 {
    if s.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: a NUL-terminated string; the copy is `malloc`'d for `free`.
    unsafe {
        let n = g_len(s) + 1;
        let p = libc::malloc(n * 2).cast::<u16>();
        if !p.is_null() {
            std::ptr::copy_nonoverlapping(s, p, n);
        }
        p
    }
}

pub extern "system" fn _wcslwr(s: *mut u16) -> *mut u16 {
    map_in_place(s, lower)
}

pub extern "system" fn _wcsupr(s: *mut u16) -> *mut u16 {
    map_in_place(s, upper)
}

pub extern "system" fn _wcslwr_s(s: *mut u16, size: usize) -> i32 {
    // SAFETY: a buffer of `size`.
    if s.is_null() || unsafe { units_in(s, size) }.is_none() {
        return bad_param(EINVAL);
    }
    map_in_place(s, lower);
    0
}

pub extern "system" fn _wcsupr_s(s: *mut u16, size: usize) -> i32 {
    // SAFETY: a buffer of `size`.
    if s.is_null() || unsafe { units_in(s, size) }.is_none() {
        return bad_param(EINVAL);
    }
    map_in_place(s, upper);
    0
}

pub extern "system" fn _wcsrev(s: *mut u16) -> *mut u16 {
    rev(s)
}

pub extern "system" fn wcsxfrm(d: *mut u16, s: *const u16, n: usize) -> usize {
    // SAFETY: the title's buffers.
    unsafe {
        let len = g_len(s);
        if n > len {
            std::ptr::copy(s, d, len + 1);
        }
        len
    }
}

pub extern "system" fn _wcserror(e: i32) -> *const u16 {
    thread_local! { static TEXTS: std::cell::RefCell<Vec<(i32, *const u16)>> = const { std::cell::RefCell::new(Vec::new()) }; }
    TEXTS.with(|t| {
        let mut t = t.borrow_mut();
        if let Some((_, p)) = t.iter().find(|(k, _)| *k == e) {
            return *p;
        }
        let w: Vec<u16> = error_text(e).encode_utf16().collect();
        let p = leak_wstr(&w).cast_const();
        t.push((e, p));
        p
    })
}

pub extern "system" fn wmemcpy(d: *mut u16, s: *const u16, n: usize) -> *mut u16 {
    if n == 0 {
        return d;
    }
    // SAFETY: the title's buffers.
    unsafe { std::ptr::copy_nonoverlapping(s, d, n) };
    d
}

pub extern "system" fn wmemmove(d: *mut u16, s: *const u16, n: usize) -> *mut u16 {
    if n == 0 {
        return d;
    }
    // SAFETY: the title's buffers.
    unsafe { std::ptr::copy(s, d, n) };
    d
}

pub extern "system" fn wmemset(d: *mut u16, c: u16, n: usize) -> *mut u16 {
    if n == 0 {
        return d;
    }
    // SAFETY: the title's buffer.
    unsafe { std::slice::from_raw_parts_mut(d, n) }.fill(c);
    d
}

pub extern "system" fn wmemcmp(a: *const u16, b: *const u16, n: usize) -> i32 {
    if n == 0 {
        return 0;
    }
    // SAFETY: the title's buffers.
    let (a, b) = unsafe {
        (
            std::slice::from_raw_parts(a, n),
            std::slice::from_raw_parts(b, n),
        )
    };
    sign(a.cmp(b))
}

pub extern "system" fn wmemchr(p: *const u16, c: u16, n: usize) -> *const u16 {
    if n == 0 {
        return std::ptr::null();
    }
    // SAFETY: the title's buffer.
    let s = unsafe { std::slice::from_raw_parts(p, n) };
    s.iter()
        .position(|&x| x == c)
        .map_or(std::ptr::null(), |i| p.wrapping_add(i))
}

// ctype

const UPPER: u16 = 0x1;
const LOWER: u16 = 0x2;
const DIGIT: u16 = 0x4;
const SPACE: u16 = 0x8;
const PUNCT: u16 = 0x10;
const CONTROL: u16 = 0x20;
const BLANK: u16 = 0x40;
const HEX: u16 = 0x80;
const ALPHA: u16 = 0x100;

fn class_bits(c: u8) -> u16 {
    match c {
        b'A'..=b'F' => UPPER | HEX | ALPHA,
        b'G'..=b'Z' => UPPER | ALPHA,
        b'a'..=b'f' => LOWER | HEX | ALPHA,
        b'g'..=b'z' => LOWER | ALPHA,
        b'0'..=b'9' => DIGIT | HEX,
        b' ' => SPACE | BLANK,
        b'\t'..=b'\r' => SPACE | CONTROL,
        0..=31 | 127 => CONTROL,
        33..=126 => PUNCT,
        _ => 0,
    }
}

/// `_pctype`: entry 0 is EOF, then the 256 characters.
fn ctype_table() -> *const u16 {
    static TABLE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *TABLE.get_or_init(|| {
        let mut t = vec![0u16; 257];
        for c in 0..=255u8 {
            t[usize::from(c) + 1] = class_bits(c);
        }
        Box::leak(t.into_boxed_slice()).as_ptr() as usize
    }) as *const u16
}

pub extern "system" fn __pctype_func() -> *const u16 {
    ctype_table().wrapping_add(1)
}

pub extern "system" fn __pwctype_func() -> *const u16 {
    ctype_table().wrapping_add(1)
}

pub extern "system" fn ___mb_cur_max_func() -> i32 {
    1
}

pub extern "system" fn ___lc_codepage_func() -> u32 {
    0
}

pub extern "system" fn ___lc_collate_cp_func() -> u32 {
    0
}

pub extern "system" fn ___lc_locale_name_func() -> *const u16 {
    static NAMES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    // Six categories' locale names, all empty ("C").
    let p = *NAMES.get_or_init(|| {
        let empty = leak_wstr(&[]) as usize;
        Box::leak(Box::new([empty; 6])).as_ptr() as usize
    });
    p as *const u16
}

pub extern "system" fn ___lc_handle_func() -> *const u32 {
    static HANDLES: [u32; 6] = [0; 6];
    HANDLES.as_ptr()
}

fn has(c: i32, mask: u16) -> bool {
    // SAFETY: the table has 257 entries and `c + 1` is in 0..=256.
    (-1..=255).contains(&c) && unsafe { *ctype_table().add((c + 1) as usize) } & mask != 0
}

macro_rules! is {
    ($($name:ident => $mask:expr;)*) => {$(
        pub extern "system" fn $name(c: i32) -> i32 {
            i32::from(has(c, $mask))
        }
    )*};
}

is! {
    isalpha => ALPHA;
    isupper => UPPER;
    islower => LOWER;
    isdigit => DIGIT;
    isxdigit => HEX;
    isspace => SPACE;
    ispunct => PUNCT;
    iscntrl => CONTROL;
    isalnum => ALPHA | DIGIT;
    isgraph => ALPHA | DIGIT | PUNCT;
    isprint => ALPHA | DIGIT | PUNCT | BLANK;
}

pub extern "system" fn isblank(c: i32) -> i32 {
    i32::from(c == 32 || c == 9)
}

pub extern "system" fn __isascii(c: i32) -> i32 {
    i32::from((0..128).contains(&c))
}

pub extern "system" fn __toascii(c: i32) -> i32 {
    c & 0x7F
}

pub extern "system" fn __iscsymf(c: i32) -> i32 {
    i32::from(isalpha(c) != 0 || c == i32::from(b'_'))
}

pub extern "system" fn __iscsym(c: i32) -> i32 {
    i32::from(isalnum(c) != 0 || c == i32::from(b'_'))
}

pub extern "system" fn isleadbyte(_c: i32) -> i32 {
    0
}

pub extern "system" fn _isctype(c: i32, mask: i32) -> i32 {
    i32::from(has(c, mask as u16))
}

pub extern "system" fn toupper(c: i32) -> i32 {
    if (97..=122).contains(&c) { c - 32 } else { c }
}

pub extern "system" fn tolower(c: i32) -> i32 {
    if (65..=90).contains(&c) { c + 32 } else { c }
}

pub extern "system" fn _toupper(c: i32) -> i32 {
    c - 32
}

pub extern "system" fn _tolower(c: i32) -> i32 {
    c + 32
}

pub extern "system" fn towupper(c: u32) -> u32 {
    if c > 0xFFFF { c } else { upper(c as u16) }
}

pub extern "system" fn towlower(c: u32) -> u32 {
    if c > 0xFFFF { c } else { lower(c as u16) }
}

fn wide_is(c: u32, mask: u16) -> bool {
    if c < 256 {
        return has(c as i32, mask);
    }
    let Some(ch) = char::from_u32(c) else {
        return false;
    };
    (mask & ALPHA != 0 && ch.is_alphabetic())
        || (mask & UPPER != 0 && ch.is_uppercase())
        || (mask & LOWER != 0 && ch.is_lowercase())
        || (mask & SPACE != 0 && ch.is_whitespace())
        || (mask & CONTROL != 0 && ch.is_control())
}

macro_rules! isw {
    ($($name:ident => $mask:expr;)*) => {$(
        pub extern "system" fn $name(c: u32) -> i32 {
            i32::from(wide_is(c, $mask))
        }
    )*};
}

isw! {
    iswalpha => ALPHA;
    iswupper => UPPER;
    iswlower => LOWER;
    iswdigit => DIGIT;
    iswxdigit => HEX;
    iswspace => SPACE;
    iswpunct => PUNCT;
    iswcntrl => CONTROL;
    iswalnum => ALPHA | DIGIT;
    iswgraph => ALPHA | DIGIT | PUNCT;
    iswprint => ALPHA | DIGIT | PUNCT | BLANK;
}

pub extern "system" fn iswblank(c: u32) -> i32 {
    i32::from(c == 32 || c == 9)
}

pub extern "system" fn iswascii(c: u32) -> i32 {
    i32::from(c < 128)
}

pub extern "system" fn iswctype(c: u32, mask: u32) -> i32 {
    i32::from(wide_is(c, mask as u16))
}

// utility

/// Bottom-up merge sort of `n` elements by index: `less_eq(i, j)` is
/// "element i does not come after element j". Tolerates an inconsistent
/// comparator (the result is then some order, never a panic).
fn merge_sort(n: usize, mut le: impl FnMut(usize, usize) -> bool) -> Vec<usize> {
    let mut a: Vec<usize> = (0..n).collect();
    let mut b = a.clone();
    let mut width = 1;
    while width < n {
        let mut i = 0;
        while i < n {
            let (mid, end) = ((i + width).min(n), (i + 2 * width).min(n));
            let (mut l, mut r, mut k) = (i, mid, i);
            while l < mid && r < end {
                if le(a[l], a[r]) {
                    b[k] = a[l];
                    l += 1;
                } else {
                    b[k] = a[r];
                    r += 1;
                }
                k += 1;
            }
            b[k..k + (mid - l)].copy_from_slice(&a[l..mid]);
            k += mid - l;
            b[k..k + (end - r)].copy_from_slice(&a[r..end]);
            i += 2 * width;
        }
        std::mem::swap(&mut a, &mut b);
        width *= 2;
    }
    a
}

unsafe fn sort(base: *mut u8, n: usize, width: usize, cmp: impl Fn(*const u8, *const u8) -> i32) {
    if n < 2 || width == 0 {
        return;
    }
    // SAFETY: the title's array of `n` elements of `width` bytes. The
    // comparator sees a copy, so it may not touch the array being moved.
    unsafe {
        let copy = std::slice::from_raw_parts(base, n * width).to_vec();
        let at = |i: usize| copy.as_ptr().add(i * width);
        let order = merge_sort(n, |i, j| cmp(at(i), at(j)) <= 0);
        for (dst, &src) in order.iter().enumerate() {
            std::ptr::copy_nonoverlapping(at(src), base.add(dst * width), width);
        }
    }
}

fn call_cmp(f: usize, args: &[u64]) -> i32 {
    // SAFETY: the title's own comparison function.
    unsafe { call_win64(f, args) as i32 }
}

pub extern "system" fn qsort(base: *mut u8, n: usize, width: usize, cmp: usize) {
    // SAFETY: the title's array and comparator.
    unsafe { sort(base, n, width, |a, b| call_cmp(cmp, &[a as u64, b as u64])) }
}

pub extern "system" fn qsort_s(base: *mut u8, n: usize, width: usize, cmp: usize, ctx: usize) {
    // SAFETY: the title's array and comparator.
    unsafe {
        sort(base, n, width, |a, b| {
            call_cmp(cmp, &[ctx as u64, a as u64, b as u64])
        })
    }
}

unsafe fn search(
    key: *const u8,
    base: *const u8,
    n: usize,
    width: usize,
    cmp: impl Fn(*const u8, *const u8) -> i32,
) -> *const u8 {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        // SAFETY: inside the title's array.
        let p = unsafe { base.add(mid * width) };
        match cmp(key, p) {
            0 => return p,
            c if c < 0 => hi = mid,
            _ => lo = mid + 1,
        }
    }
    std::ptr::null()
}

pub extern "system" fn bsearch(
    key: *const u8,
    base: *const u8,
    n: usize,
    width: usize,
    cmp: usize,
) -> *const u8 {
    // SAFETY: the title's array and comparator.
    unsafe {
        search(key, base, n, width, |a, b| {
            call_cmp(cmp, &[a as u64, b as u64])
        })
    }
}

pub extern "system" fn bsearch_s(
    key: *const u8,
    base: *const u8,
    n: usize,
    width: usize,
    cmp: usize,
    ctx: usize,
) -> *const u8 {
    // SAFETY: the title's array and comparator.
    unsafe {
        search(key, base, n, width, |a, b| {
            call_cmp(cmp, &[ctx as u64, a as u64, b as u64])
        })
    }
}

thread_local! {
    static SEED: StdCell<u32> = const { StdCell::new(1) };
}

pub extern "system" fn srand(seed: u32) {
    SEED.set(seed);
}

/// The CRT's generator: `seed = seed * 214013 + 2531011`, bits 16..30.
pub extern "system" fn rand() -> i32 {
    let s = SEED.get().wrapping_mul(214_013).wrapping_add(2_531_011);
    SEED.set(s);
    ((s >> 16) & 0x7FFF) as i32
}

pub extern "system" fn rand_s(out: *mut u32) -> i32 {
    if out.is_null() {
        return bad_param(EINVAL);
    }
    // SAFETY: the title passes a valid pointer.
    unsafe { libc::arc4random_buf(out.cast(), 4) };
    0
}

pub extern "system" fn _lfind(
    key: *const u8,
    base: *const u8,
    num: *mut u32,
    width: u32,
    cmp: usize,
) -> *const u8 {
    // SAFETY: the title's array of `*num` elements.
    unsafe {
        for i in 0..*num as usize {
            let p = base.add(i * width as usize);
            if call_cmp(cmp, &[key as u64, p as u64]) == 0 {
                return p;
            }
        }
    }
    std::ptr::null()
}

// locale: "C" only

static LOCALE_LOCK: std::sync::LazyLock<crate::host_posix::system::MutexObject> =
    std::sync::LazyLock::new(crate::host_posix::system::MutexObject::new);

pub extern "system" fn _lock_locales() {
    LOCALE_LOCK.acquire(None);
}

pub extern "system" fn _unlock_locales() {
    LOCALE_LOCK.release();
}

pub extern "system" fn setlocale(_category: i32, locale: *const c_char) -> *const u8 {
    static C: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let name = *C.get_or_init(|| leak_cstr(b"C") as usize) as *const u8;
    if locale.is_null() {
        return name;
    }
    // SAFETY: a NUL-terminated name.
    match unsafe { super::bytes(locale.cast()) } {
        b"" | b"C" | b"POSIX" => name,
        _ => std::ptr::null(),
    }
}

pub extern "system" fn _wsetlocale(_category: i32, locale: *const u16) -> *const u16 {
    static C: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let name = *C.get_or_init(|| leak_wstr(&[u16::from(b'C')]) as usize) as *const u16;
    if locale.is_null() {
        return name;
    }
    // SAFETY: a NUL-terminated name.
    match unsafe { super::wide(locale) } {
        [] => name,
        [c] if *c == u16::from(b'C') => name,
        _ => std::ptr::null(),
    }
}

/// `struct lconv`: ten strings, eight `char`s, eight wide strings.
pub extern "system" fn localeconv() -> *const c_void {
    static LCONV: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let p = *LCONV.get_or_init(|| {
        let mut block = vec![0u8; 152];
        let put = |block: &mut Vec<u8>, at: usize, v: usize| {
            block[at..at + 8].copy_from_slice(&v.to_le_bytes())
        };
        let dot = leak_cstr(b".") as usize;
        let empty = leak_cstr(b"") as usize;
        // decimal_point, thousands_sep, grouping, int_curr_symbol, currency_symbol,
        // mon_decimal_point, mon_thousands_sep, mon_grouping, positive_sign, negative_sign
        for (i, s) in [
            dot, empty, empty, empty, empty, empty, empty, empty, empty, empty,
        ]
        .iter()
        .enumerate()
        {
            put(&mut block, i * 8, *s);
        }
        block[80..88].fill(127);
        let wdot = leak_wstr(&[u16::from(b'.')]) as usize;
        let wempty = leak_wstr(&[]) as usize;
        for (i, s) in [wdot, wempty, wempty, wempty, wempty, wempty, wempty, wempty]
            .iter()
            .enumerate()
        {
            put(&mut block, 88 + i * 8, *s);
        }
        Box::leak(block.into_boxed_slice()).as_ptr() as usize
    });
    p as *const c_void
}

/// Always 0: the per-thread locale setting is off, and there is one locale.
pub extern "system" fn _configthreadlocale(_mode: i32) -> i32 {
    0
}

/// A locale handle is never dereferenced by the functions that take one
/// here; `_get_current_locale` and `_create_locale` hand out one token.
pub extern "system" fn _get_current_locale() -> usize {
    1
}

pub extern "system" fn _create_locale(_category: i32, locale: *const c_char) -> usize {
    if setlocale(0, locale).is_null() { 0 } else { 1 }
}

pub extern "system" fn _free_locale(_locale: usize) {}

exports!(pub(super) fn lookup;
    memcpy, memmove, memset, memcmp, memchr, _memicmp, memcpy_s, memmove_s, _memccpy, strlen,
    strnlen, strcpy, strncpy, strcat, strncat, strcmp, strncmp, _stricmp, _strnicmp, strchr,
    strrchr, strstr, strspn, strcspn, strpbrk, strtok_s, strtok, strcpy_s, strncpy_s, strcat_s,
    strncat_s, _strdup, _strlwr, _strupr, _strlwr_s, _strupr_s, _strrev, _strset, _strnset,
    strxfrm, strerror, _strerror, strerror_s, wcslen, wcsnlen, wcscpy, wcsncpy, wcscat, wcsncat,
    wcscmp, wcsncmp, _wcsicmp, _wcsnicmp, wcschr, wcsrchr, wcsstr, wcsspn, wcscspn, wcspbrk,
    wcstok_s, wcstok, wcscpy_s, wcsncpy_s, wcscat_s, wcsncat_s, _wcsdup, _wcslwr, _wcsupr,
    _wcslwr_s, _wcsupr_s, _wcsrev, wcsxfrm, _wcserror, wmemcpy, wmemmove, wmemset, wmemcmp,
    wmemchr, __pctype_func, __pwctype_func, ___mb_cur_max_func, ___lc_codepage_func,
    ___lc_collate_cp_func, ___lc_locale_name_func, ___lc_handle_func, isalpha, isupper, islower,
    isdigit, isxdigit, isspace, ispunct, iscntrl, isalnum, isgraph, isprint, isblank, __isascii,
    __toascii, __iscsymf, __iscsym, isleadbyte, _isctype, toupper, tolower, _toupper, _tolower,
    towupper, towlower, iswalpha, iswupper, iswlower, iswdigit, iswxdigit, iswspace, iswpunct,
    iswcntrl, iswalnum, iswgraph, iswprint, iswblank, iswascii, iswctype, qsort, qsort_s,
    bsearch, bsearch_s, srand, rand, rand_s, _lfind, setlocale, _wsetlocale, localeconv,
    _configthreadlocale, _get_current_locale, _create_locale, _free_locale,
    _lock_locales, _unlock_locales;
    "__strncnt" => strnlen,
    // `_l` variants take the locale last, which is ignored.
    "_stricmp_l" => _stricmp, "_strnicmp_l" => _strnicmp, "_strlwr_l" => _strlwr,
    "_strupr_l" => _strupr, "_wcsicmp_l" => _wcsicmp, "_wcsnicmp_l" => _wcsnicmp,
    "_wcslwr_l" => _wcslwr, "_wcsupr_l" => _wcsupr, "strcoll" => strcmp, "_stricoll" => _stricmp,
    "_strcoll_l" => strcmp, "_stricoll_l" => _stricmp, "_strncoll" => strncmp,
    "_strnicoll" => _strnicmp, "wcscoll" => wcscmp, "_wcsicoll" => _wcsicmp,
    "_wcscoll_l" => wcscmp, "_wcsicoll_l" => _wcsicmp, "_wcsncoll" => wcsncmp,
    "_wcsnicoll" => _wcsnicmp, "strdup" => _strdup, "_isalpha_l" => isalpha,
    "_isupper_l" => isupper, "_islower_l" => islower, "_isdigit_l" => isdigit,
    "_isxdigit_l" => isxdigit, "_isspace_l" => isspace, "_ispunct_l" => ispunct,
    "_iscntrl_l" => iscntrl, "_isalnum_l" => isalnum, "_isgraph_l" => isgraph,
    "_isprint_l" => isprint, "_isblank_l" => isblank, "_toupper_l" => toupper,
    "_tolower_l" => tolower, "_iswalpha_l" => iswalpha, "_iswupper_l" => iswupper,
    "_iswlower_l" => iswlower, "_iswdigit_l" => iswdigit, "_iswxdigit_l" => iswxdigit,
    "_iswspace_l" => iswspace, "_iswpunct_l" => iswpunct, "_iswcntrl_l" => iswcntrl,
    "_iswalnum_l" => iswalnum, "_towupper_l" => towupper, "_towlower_l" => towlower,
    "_isctype_l" => _isctype, "_iswctype_l" => iswctype, "___mb_cur_max_l_func" => ___mb_cur_max_func,
    "_memicmp_l" => _memicmp, "_strxfrm_l" => strxfrm, "_wcsxfrm_l" => wcsxfrm,
    "_strerror_s" => strerror_s);

#[cfg(test)]
mod tests {
    use super::*;
    use victory_platform::mem::{self, Protect};

    #[test]
    fn memmove_s_leaves_the_destination_alone_on_error_and_memcpy_s_zeroes_it() {
        let (src, null) = (*b"abcd", std::ptr::null::<u8>());
        let mut dst = *b"xxxx";
        assert_eq!(memmove_s(dst.as_mut_ptr(), 4, null, 2), EINVAL);
        assert_eq!(super::super::errno(), EINVAL);
        assert_eq!(memmove_s(dst.as_mut_ptr(), 2, src.as_ptr(), 3), ERANGE);
        assert_eq!(super::super::errno(), ERANGE);
        assert_eq!(dst, *b"xxxx");
        assert_eq!(memmove_s(std::ptr::null_mut(), 4, src.as_ptr(), 2), EINVAL);
        assert_eq!(memmove_s(std::ptr::null_mut(), 0, null, 0), 0);
        // Overlapping, as memmove.
        let mut both = *b"abcdef";
        assert_eq!(memmove_s(both.as_mut_ptr().wrapping_add(2), 4, both.as_ptr(), 4), 0);
        assert_eq!(both, *b"ababcd");
        assert_eq!(memcpy_s(dst.as_mut_ptr(), 2, src.as_ptr(), 3), ERANGE);
        assert_eq!(dst, *b"\0\0xx");
    }

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// `n` units of `A` with no NUL after them: the last one ends where a
    /// page nobody may read begins. With the mapping to release.
    fn before_a_guard<C: Ch>(n: usize) -> (*const C, *mut u8, usize) {
        const HALF: usize = 64 << 10;
        // SAFETY: a fresh mapping of our own, written below its guard.
        unsafe {
            let base = mem::alloc(None, 2 * HALF, Protect::READ_WRITE).unwrap();
            mem::protect(base.add(HALF), HALF, Protect::NO_ACCESS).unwrap();
            let src = base.add(HALF).cast::<C>().sub(n);
            for i in 0..n {
                src.add(i).write(C::make(u32::from(b'A')));
            }
            (src, base, 2 * HALF)
        }
    }

    #[test]
    fn counted_copies_read_no_further_than_their_count() {
        // A counted source need not hold a NUL within its count, so nothing past the count
        // (for the `_s` forms, past the room left) may be read: here that is a guard page.
        // 0xEE marks units nothing should write.
        let fresh = || [0xEEu8; 16];
        let (s, base, len) = before_a_guard::<u8>(8);
        let mut d = fresh();
        strncpy(d.as_mut_ptr(), s, 8);
        assert_eq!(&d[..9], b"AAAAAAAA\xEE", "a full count writes no NUL");
        let mut d = fresh();
        d[..3].copy_from_slice(b"xy\0");
        strncat(d.as_mut_ptr(), s, 8);
        assert_eq!(&d[..12], b"xyAAAAAAAA\0\xEE");
        let mut d = fresh();
        assert_eq!(strncpy_s(d.as_mut_ptr(), d.len(), s, 8), 0);
        assert_eq!(&d[..9], b"AAAAAAAA\0");
        let mut d = fresh();
        d[..3].copy_from_slice(b"xy\0");
        assert_eq!(strncat_s(d.as_mut_ptr(), d.len(), s, 8), 0);
        assert_eq!(&d[..11], b"xyAAAAAAAA\0");
        // _TRUNCATE reads no more than fits.
        let mut d = fresh();
        assert_eq!(strncpy_s(d.as_mut_ptr(), 4, s, TRUNCATE), STRUNCATE);
        assert_eq!(&d[..5], b"AAA\0\xEE");
        let mut d = fresh();
        d[..2].copy_from_slice(b"x\0");
        assert_eq!(strncat_s(d.as_mut_ptr(), 4, s, TRUNCATE), STRUNCATE);
        assert_eq!(&d[..5], b"xAA\0\xEE");
        // SAFETY: our own mapping.
        unsafe { mem::release(base, len).unwrap() };

        // A count past the room: the room's worth without a NUL is already ERANGE (this source
        // has four units before the guard), and the error empties the destination.
        let (s, base, len) = before_a_guard::<u8>(4);
        let mut d = fresh();
        assert_eq!(strncpy_s(d.as_mut_ptr(), 4, s, 100), ERANGE);
        assert_eq!(d[0], 0);
        let mut d = fresh();
        d[..3].copy_from_slice(b"xy\0");
        assert_eq!(strncat_s(d.as_mut_ptr(), 5, s, 100), ERANGE);
        assert_eq!(d[0], 0, "a failed append leaves no old string");
        let mut d = fresh();
        d[..3].copy_from_slice(b"xy\0");
        assert_eq!(strcat_s(d.as_mut_ptr(), 5, b"abc\0".as_ptr()), ERANGE);
        assert_eq!(d[0], 0);
        // SAFETY: our own mapping.
        unsafe { mem::release(base, len).unwrap() };

        let a = u16::from(b'A');
        let fresh = || [0xEEEEu16; 16];
        let (s, base, len) = before_a_guard::<u16>(8);
        let mut d = fresh();
        wcsncpy(d.as_mut_ptr(), s, 8);
        assert_eq!(&d[..9], &[a, a, a, a, a, a, a, a, 0xEEEE]);
        let mut d = fresh();
        d[..3].copy_from_slice(&w("xy"));
        wcsncat(d.as_mut_ptr(), s, 8);
        assert_eq!(&d[..12], &[&w("xyAAAAAAAA")[..], &[0xEEEE]].concat()[..]);
        let mut d = fresh();
        assert_eq!(wcsncpy_s(d.as_mut_ptr(), d.len(), s, 8), 0);
        assert_eq!(&d[..9], &w("AAAAAAAA")[..]);
        let mut d = fresh();
        d[..3].copy_from_slice(&w("xy"));
        assert_eq!(wcsncat_s(d.as_mut_ptr(), d.len(), s, 8), 0);
        assert_eq!(&d[..11], &w("xyAAAAAAAA")[..]);
        let mut d = fresh();
        assert_eq!(wcsncpy_s(d.as_mut_ptr(), 4, s, TRUNCATE), STRUNCATE);
        assert_eq!(&d[..5], &[a, a, a, 0, 0xEEEE]);
        // SAFETY: our own mapping.
        unsafe { mem::release(base, len).unwrap() };
        let (s, base, len) = before_a_guard::<u16>(4);
        let mut d = fresh();
        assert_eq!(wcsncpy_s(d.as_mut_ptr(), 4, s, 100), ERANGE);
        assert_eq!(d[0], 0);
        let mut d = fresh();
        d[..3].copy_from_slice(&w("xy"));
        assert_eq!(wcsncat_s(d.as_mut_ptr(), 5, s, 100), ERANGE);
        assert_eq!(d[0], 0);
        // SAFETY: our own mapping.
        unsafe { mem::release(base, len).unwrap() };
    }

    #[test]
    fn narrow_strings() {
        let a = b"hello\0";
        assert_eq!(strlen(a.as_ptr()), 5);
        assert_eq!(strcmp(b"abc\0".as_ptr(), b"abd\0".as_ptr()), -1);
        assert_eq!(strcmp(b"b\0".as_ptr(), b"a\0".as_ptr()), 1);
        assert_eq!(_stricmp(b"HeLLo\0".as_ptr(), a.as_ptr()), 0);
        assert_eq!(_strnicmp(b"HELP\0".as_ptr(), a.as_ptr(), 3), 0);
        assert_eq!(strncmp(b"abcX\0".as_ptr(), b"abcY\0".as_ptr(), 3), 0);
        // SAFETY: inside `a`.
        assert_eq!(
            unsafe { strchr(a.as_ptr(), i32::from(b'l')).offset_from(a.as_ptr()) },
            2
        );
        assert_eq!(
            unsafe { strrchr(a.as_ptr(), i32::from(b'l')).offset_from(a.as_ptr()) },
            3
        );
        assert_eq!(
            unsafe { strstr(a.as_ptr(), b"lo\0".as_ptr()).offset_from(a.as_ptr()) },
            3
        );
        assert!(strstr(a.as_ptr(), b"xyz\0".as_ptr()).is_null());
        assert_eq!(strspn(b"aabbc\0".as_ptr(), b"ab\0".as_ptr()), 4);
        assert_eq!(strcspn(b"hello\0".as_ptr(), b"l\0".as_ptr()), 2);
        let mut buf = [0u8; 16];
        strcpy(buf.as_mut_ptr(), a.as_ptr());
        strcat(buf.as_mut_ptr(), b" you\0".as_ptr());
        assert_eq!(&buf[..10], b"hello you\0");
        strncpy(buf.as_mut_ptr(), b"ab\0".as_ptr(), 4);
        assert_eq!(&buf[..5], b"ab\0\0o");
        let mut up = *b"MiXed\0";
        _strlwr(up.as_mut_ptr());
        assert_eq!(&up, b"mixed\0");
    }

    #[test]
    fn secure_copies_report_errors() {
        let mut buf = [b'x'; 4];
        assert_eq!(strcpy_s(buf.as_mut_ptr(), 4, b"abc\0".as_ptr()), 0);
        assert_eq!(&buf, b"abc\0");
        assert_eq!(strcpy_s(buf.as_mut_ptr(), 4, b"abcd\0".as_ptr()), ERANGE);
        assert_eq!(buf[0], 0);
        assert_eq!(strcpy_s(std::ptr::null_mut(), 4, b"a\0".as_ptr()), EINVAL);
        let mut buf = [b'x'; 4];
        assert_eq!(
            strncpy_s(buf.as_mut_ptr(), 4, b"abcdef\0".as_ptr(), TRUNCATE),
            STRUNCATE
        );
        assert_eq!(&buf, b"abc\0");
        assert_eq!(strncpy_s(buf.as_mut_ptr(), 4, b"abcdef\0".as_ptr(), 2), 0);
        assert_eq!(&buf[..3], b"ab\0");
        assert_eq!(strcat_s(buf.as_mut_ptr(), 4, b"c\0".as_ptr()), 0);
        assert_eq!(strcat_s(buf.as_mut_ptr(), 4, b"d\0".as_ptr()), ERANGE);
        let mut dst = [1u8; 4];
        assert_eq!(memcpy_s(dst.as_mut_ptr(), 4, b"abcdef".as_ptr(), 6), ERANGE);
        assert_eq!(dst, [0; 4]);
        assert_eq!(memcpy_s(dst.as_mut_ptr(), 4, b"abcdef".as_ptr(), 2), 0);
    }

    #[test]
    fn wide_strings_are_utf16() {
        let a = w("Héllo");
        assert_eq!(wcslen(a.as_ptr()), 5);
        assert_eq!(_wcsicmp(w("HÉLLO").as_ptr(), a.as_ptr()), 0);
        assert_eq!(wcscmp(w("a").as_ptr(), w("b").as_ptr()), -1);
        assert_eq!(wcsncmp(w("abcX").as_ptr(), w("abcY").as_ptr(), 3), 0);
        // SAFETY: inside `a`.
        assert_eq!(
            unsafe { wcschr(a.as_ptr(), u16::from(b'l')).offset_from(a.as_ptr()) },
            2
        );
        assert_eq!(
            unsafe { wcsstr(a.as_ptr(), w("llo").as_ptr()).offset_from(a.as_ptr()) },
            2
        );
        let mut buf = [0u16; 8];
        assert_eq!(wcscpy_s(buf.as_mut_ptr(), 8, a.as_ptr()), 0);
        assert_eq!(wcscat_s(buf.as_mut_ptr(), 8, w("!!").as_ptr()), 0);
        assert_eq!(wcslen(buf.as_ptr()), 7);
        assert_eq!(wcscat_s(buf.as_mut_ptr(), 8, w("!!").as_ptr()), ERANGE);
        let mut up = w("abc");
        _wcsupr(up.as_mut_ptr());
        assert_eq!(up, w("ABC"));
    }

    #[test]
    fn tokenizing() {
        let mut s = *b"  a,b;;c \0";
        let mut ctx = std::ptr::null_mut();
        let mut out = Vec::new();
        let mut t = strtok_s(s.as_mut_ptr(), b" ,;\0".as_ptr(), &mut ctx);
        while !t.is_null() {
            // SAFETY: a token inside `s`.
            out.push(unsafe { super::super::bytes(t) }.to_vec());
            t = strtok_s(std::ptr::null_mut(), b" ,;\0".as_ptr(), &mut ctx);
        }
        assert_eq!(out, [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
        let mut s = w("x y");
        let t = wcstok(s.as_mut_ptr(), w(" ").as_ptr(), std::ptr::null_mut());
        assert_eq!(unsafe { super::super::wide(t) }, &w("x")[..1]);
        let t = wcstok(std::ptr::null_mut(), w(" ").as_ptr(), std::ptr::null_mut());
        assert_eq!(unsafe { super::super::wide(t) }, &w("y")[..1]);
    }

    #[test]
    fn ctype_in_the_c_locale() {
        assert_eq!(isalpha(i32::from(b'a')), 1);
        assert_eq!(isalpha(0xE9), 0);
        assert_eq!(isdigit(i32::from(b'7')), 1);
        assert_eq!(isxdigit(i32::from(b'F')), 1);
        assert_eq!(isspace(i32::from(b'\n')), 1);
        assert_eq!(isspace(-1), 0);
        assert_eq!(ispunct(i32::from(b'!')), 1);
        assert_eq!(toupper(i32::from(b'q')), i32::from(b'Q'));
        assert_eq!(tolower(0xC9), 0xC9);
        assert_eq!(towlower(0xC9), 0xE9);
        assert_eq!(iswalpha(0x3b1), 1);
        // SAFETY: the table has an entry for each character, and EOF before.
        let t = __pctype_func();
        assert_eq!(
            unsafe { *t.add(usize::from(b'A')) } & (UPPER | ALPHA),
            UPPER | ALPHA
        );
        assert_eq!(unsafe { *t.offset(-1) }, 0);
    }

    #[test]
    fn qsort_and_bsearch_call_back_through_a_comparator() {
        // The comparator is host code here: `call_win64` is for title
        // functions, so the sort helpers are exercised directly.
        let mut v = [5i32, 3, 9, 1, 3, 7, 0, 8];
        // SAFETY: `v` is the array.
        unsafe {
            sort(v.as_mut_ptr().cast(), v.len(), 4, |a, b| {
                (*a.cast::<i32>()).cmp(&*b.cast::<i32>()) as i32
            });
        }
        assert_eq!(v, [0, 1, 3, 3, 5, 7, 8, 9]);
        let key = 7i32;
        // SAFETY: `v` is sorted.
        let found = unsafe {
            search(
                (&key as *const i32).cast(),
                v.as_ptr().cast(),
                v.len(),
                4,
                |a, b| (*a.cast::<i32>()).cmp(&*b.cast::<i32>()) as i32,
            )
        };
        assert_eq!(unsafe { *found.cast::<i32>() }, 7);
        // An inconsistent comparator ends, in some order.
        let mut v = [3i32, 1, 2, 5, 4];
        unsafe { sort(v.as_mut_ptr().cast(), v.len(), 4, |_, _| 1) };
        let mut s = v;
        s.sort();
        assert_eq!(s, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn rand_is_the_crt_generator() {
        srand(1);
        assert_eq!([rand(), rand(), rand()], [41, 18467, 6334]);
        srand(42);
        assert_eq!(rand(), 175);
    }

    #[test]
    fn locale_is_c() {
        // SAFETY: the returned strings are NUL-terminated.
        unsafe {
            assert_eq!(super::super::bytes(setlocale(0, std::ptr::null())), b"C");
            assert_eq!(super::super::bytes(setlocale(0, c"".as_ptr())), b"C");
        }
        assert!(setlocale(0, c"fr_FR".as_ptr()).is_null());
        let lc = localeconv().cast::<*const u8>();
        // SAFETY: the structure's first member is the decimal point.
        assert_eq!(unsafe { super::super::bytes(*lc) }, b".");
        assert_eq!(unsafe { *lc.cast::<u8>().add(80) }, 127);
        assert_eq!(
            unsafe { super::super::wide(*lc.add(11).cast::<*const u16>()) },
            [u16::from(b'.')]
        );
    }

    #[test]
    fn lock_and_unlock_locales_are_recursive_and_process_wide() {
        let available_elsewhere = || {
            std::thread::spawn(|| {
                let acquired = LOCALE_LOCK.acquire(Some(std::time::Duration::ZERO));
                if acquired {
                    _unlock_locales();
                }
                acquired
            })
            .join()
            .unwrap()
        };
        _lock_locales();
        _lock_locales();
        let held_twice = !available_elsewhere();
        _unlock_locales();
        let held_once = !available_elsewhere();
        _unlock_locales();
        assert!(held_twice && held_once && available_elsewhere());
    }

    #[test]
    fn strncnt_stops_at_nul_or_the_bound() {
        let address = lookup("__strncnt").unwrap();
        // SAFETY: the registered symbol has this signature.
        let count: extern "system" fn(*const u8, usize) -> usize =
            unsafe { std::mem::transmute(address) };
        assert_eq!(count(b"abc\0xyz".as_ptr(), 7), 3);
        assert_eq!(count(b"abc".as_ptr(), 3), 3);
        assert_eq!(count(b"abc".as_ptr(), 2), 2);
        assert_eq!(count(std::ptr::null(), 0), 0);
    }

    #[test]
    fn error_messages() {
        // SAFETY: NUL-terminated.
        assert_eq!(
            unsafe { super::super::bytes(strerror(2)) },
            b"No such file or directory"
        );
        assert_eq!(
            unsafe { super::super::bytes(strerror(9999)) },
            b"Unknown error"
        );
    }

    #[test]
    fn zero_length_memory_and_copy_ops_accept_null_pointers() {
        assert!(memcpy(std::ptr::null_mut(), std::ptr::null(), 0).is_null());
        assert!(memmove(std::ptr::null_mut(), std::ptr::null(), 0).is_null());
        assert!(memset(std::ptr::null_mut(), 0xAA, 0).is_null());
        assert_eq!(memcmp(std::ptr::null(), std::ptr::null(), 0), 0);
        assert!(memchr(std::ptr::null(), i32::from(b'a'), 0).is_null());
        assert_eq!(_memicmp(std::ptr::null(), std::ptr::null(), 0), 0);

        assert!(wmemcpy(std::ptr::null_mut(), std::ptr::null(), 0).is_null());
        assert!(wmemmove(std::ptr::null_mut(), std::ptr::null(), 0).is_null());
        assert!(wmemset(std::ptr::null_mut(), 0x1234, 0).is_null());
        assert_eq!(wmemcmp(std::ptr::null(), std::ptr::null(), 0), 0);
        assert!(wmemchr(std::ptr::null(), u16::from(b'a'), 0).is_null());

        assert!(strncpy(std::ptr::null_mut(), std::ptr::null(), 0).is_null());
        assert!(wcsncpy(std::ptr::null_mut(), std::ptr::null(), 0).is_null());
        let mut narrow = *b"hi\0x";
        assert_eq!(
            strncat(narrow.as_mut_ptr(), std::ptr::null(), 0),
            narrow.as_mut_ptr()
        );
        assert_eq!(&narrow[..3], b"hi\0");
        let mut wide = w("hi");
        assert_eq!(
            wcsncat(wide.as_mut_ptr(), std::ptr::null(), 0),
            wide.as_mut_ptr()
        );
        assert_eq!(wide, w("hi"));
    }
}
