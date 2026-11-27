//! `api-ms-win-crt-filesystem`: `_stat64`, `_access`, `_mkdir`, `remove`,
//! `rename`, the path splitters, on `host_posix::files` paths (virtual
//! drives). A `_stat` result follows the Windows layout, so `st_mode` has
//! `_S_IFREG`/`_S_IFDIR` and the read, write and execute bits as the CRT
//! sets them, and the times are `time_t` seconds.

use std::ffi::CString;
use std::fs::Metadata;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::runtime::bad_param;
use super::stdio::win_errno;
use super::{Ch, EACCES, EEXIST, EINVAL, ENOENT, ERANGE, exports, fail, heap, set_errno, units};
use crate::host_posix::files;

const S_IFDIR: u16 = 0x4000;
const S_IFREG: u16 = 0x8000;
const S_IREAD: u16 = 0x100;
const S_IWRITE: u16 = 0x80;
const S_IEXEC: u16 = 0x40;

fn secs(t: std::io::Result<SystemTime>) -> i64 {
    t.ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

/// Fills a `struct _stat64` (`wide_size`) or `_stat64i32` (32-bit size).
unsafe fn fill_stat(out: *mut u8, m: &Metadata, drive: u32, wide_size: bool, name: &str) {
    // A folder has no size on Windows.
    let size = if m.is_dir() { 0 } else { m.len() };
    let writable = !m.permissions().readonly();
    let executable = m.is_dir()
        || [".exe", ".cmd", ".bat", ".com"]
            .iter()
            .any(|e| name.to_ascii_lowercase().ends_with(e));
    let mode = if m.is_dir() { S_IFDIR } else { S_IFREG }
        | S_IREAD
        | if writable { S_IWRITE } else { 0 }
        | if executable { S_IEXEC } else { 0 };
    // SAFETY: the title's structure: 56 bytes (`_stat64`) or 48 (`_stat64i32`).
    unsafe {
        std::ptr::write_bytes(out, 0, if wide_size { 56 } else { 48 });
        out.cast::<u32>().write_unaligned(drive);
        out.add(6).cast::<u16>().write_unaligned(mode);
        out.add(8).cast::<i16>().write_unaligned(1);
        out.add(16).cast::<u32>().write_unaligned(drive);
        let times = if wide_size {
            out.add(24).cast::<i64>().write_unaligned(size as i64);
            32
        } else {
            out.add(20)
                .cast::<i32>()
                .write_unaligned(size.min(i32::MAX as u64) as i32);
            24
        };
        out.add(times)
            .cast::<i64>()
            .write_unaligned(secs(m.accessed()));
        out.add(times + 8)
            .cast::<i64>()
            .write_unaligned(secs(m.modified()));
        out.add(times + 16)
            .cast::<i64>()
            .write_unaligned(secs(m.created().or_else(|_| m.modified())));
    }
}

fn drive_index(path: &[u8]) -> u32 {
    match path {
        [d, b':', ..] if d.is_ascii_alphabetic() => u32::from(d.to_ascii_uppercase() - b'A'),
        _ => 2,
    }
}

fn stat_path(path: Option<PathBuf>, name: String, out: *mut u8, wide_size: bool) -> i32 {
    if out.is_null() {
        bad_param(EINVAL);
        return -1;
    }
    let Some(p) = path else {
        set_errno(ENOENT);
        return -1;
    };
    match std::fs::metadata(&p) {
        Ok(m) => {
            // SAFETY: the title's `_stat` structure.
            unsafe { fill_stat(out, &m, drive_index(name.as_bytes()), wide_size, &name) };
            0
        }
        Err(e) => {
            set_errno(win_errno(&e));
            -1
        }
    }
}

fn narrow_name(p: *const u8) -> (Option<PathBuf>, String) {
    if p.is_null() {
        return (None, String::new());
    }
    (
        files::narrow_path(p.cast()),
        String::from_utf8_lossy(
            // SAFETY: a NUL-terminated path.
            unsafe { super::bytes(p) },
        )
        .into_owned(),
    )
}

fn wide_name(p: *const u16) -> (Option<PathBuf>, String) {
    if p.is_null() {
        return (None, String::new());
    }
    // SAFETY: a NUL-terminated path.
    (
        files::wide_path(p),
        String::from_utf16_lossy(unsafe { super::wide(p) }),
    )
}

pub extern "system" fn _stat64(path: *const u8, out: *mut u8) -> i32 {
    let (p, n) = narrow_name(path);
    stat_path(p, n, out, true)
}

pub extern "system" fn _stat64i32(path: *const u8, out: *mut u8) -> i32 {
    let (p, n) = narrow_name(path);
    stat_path(p, n, out, false)
}

pub extern "system" fn _wstat64(path: *const u16, out: *mut u8) -> i32 {
    let (p, n) = wide_name(path);
    stat_path(p, n, out, true)
}

pub extern "system" fn _wstat64i32(path: *const u16, out: *mut u8) -> i32 {
    let (p, n) = wide_name(path);
    stat_path(p, n, out, false)
}

pub extern "system" fn _access(path: *const u8, mode: i32) -> i32 {
    access(narrow_name(path).0, mode)
}

pub extern "system" fn _waccess(path: *const u16, mode: i32) -> i32 {
    access(wide_name(path).0, mode)
}

fn access(path: Option<PathBuf>, mode: i32) -> i32 {
    let Some(m) = path.and_then(|p| std::fs::metadata(p).ok()) else {
        set_errno(ENOENT);
        return -1;
    };
    // Bit 1 is write; read is always allowed.
    if mode & 2 != 0 && m.permissions().readonly() {
        set_errno(EACCES);
        return -1;
    }
    0
}

pub extern "system" fn _access_s(path: *const u8, mode: i32) -> i32 {
    if access(narrow_name(path).0, mode) == 0 {
        0
    } else {
        super::errno()
    }
}

pub extern "system" fn _waccess_s(path: *const u16, mode: i32) -> i32 {
    if access(wide_name(path).0, mode) == 0 {
        0
    } else {
        super::errno()
    }
}

fn done(r: std::io::Result<()>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            set_errno(win_errno(&e));
            -1
        }
    }
}

fn with_path(p: Option<PathBuf>, f: impl FnOnce(PathBuf) -> std::io::Result<()>) -> i32 {
    match p {
        Some(p) => done(f(p)),
        None => {
            set_errno(ENOENT);
            -1
        }
    }
}

pub extern "system" fn _mkdir(path: *const u8) -> i32 {
    with_path(narrow_name(path).0, std::fs::create_dir)
}

pub extern "system" fn _wmkdir(path: *const u16) -> i32 {
    with_path(wide_name(path).0, std::fs::create_dir)
}

pub extern "system" fn _rmdir(path: *const u8) -> i32 {
    with_path(narrow_name(path).0, std::fs::remove_dir)
}

pub extern "system" fn _wrmdir(path: *const u16) -> i32 {
    with_path(wide_name(path).0, std::fs::remove_dir)
}

pub extern "system" fn _chdir(path: *const u8) -> i32 {
    with_path(narrow_name(path).0, std::env::set_current_dir)
}

pub extern "system" fn _wchdir(path: *const u16) -> i32 {
    with_path(wide_name(path).0, std::env::set_current_dir)
}

pub extern "system" fn remove(path: *const u8) -> i32 {
    with_path(narrow_name(path).0, std::fs::remove_file)
}

pub extern "system" fn _wremove(path: *const u16) -> i32 {
    with_path(wide_name(path).0, std::fs::remove_file)
}

fn rename_paths(from: Option<PathBuf>, to: Option<PathBuf>) -> i32 {
    let (Some(from_path), Some(to_path)) = (from, to) else {
        set_errno(ENOENT);
        return -1;
    };
    let [Ok(from), Ok(to)] = [&from_path, &to_path].map(|p| CString::new(p.as_os_str().as_bytes()))
    else {
        set_errno(EINVAL);
        return -1;
    };
    // SAFETY: both paths are live, NUL-terminated C strings.
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) } == 0 {
        0
    } else {
        rename_error(&from_path, &to_path, std::io::Error::last_os_error())
    }
}

fn rename_error(from_path: &Path, to_path: &Path, err: std::io::Error) -> i32 {
    if err.raw_os_error() == Some(libc::ENOTSUP) {
        // This filesystem has no RENAME_EXCL: check, then rename (not atomic).
        if std::fs::symlink_metadata(to_path).is_ok() {
            set_errno(EEXIST);
            return -1;
        }
        return done(std::fs::rename(from_path, to_path));
    }
    set_errno(win_errno(&err));
    -1
}

pub extern "system" fn rename(from: *const u8, to: *const u8) -> i32 {
    rename_paths(narrow_name(from).0, narrow_name(to).0)
}

pub extern "system" fn _wrename(from: *const u16, to: *const u16) -> i32 {
    rename_paths(wide_name(from).0, wide_name(to).0)
}

fn chmod(p: Option<PathBuf>, mode: i32) -> i32 {
    with_path(p, |p| {
        let mut perm = std::fs::metadata(&p)?.permissions();
        perm.set_readonly(mode & i32::from(S_IWRITE) == 0);
        std::fs::set_permissions(p, perm)
    })
}

pub extern "system" fn _chmod(path: *const u8, mode: i32) -> i32 {
    chmod(narrow_name(path).0, mode)
}

pub extern "system" fn _wchmod(path: *const u16, mode: i32) -> i32 {
    chmod(wide_name(path).0, mode)
}

/// `_fstat64` of a descriptor from `stdio`.
pub extern "system" fn _fstat64(fd: i32, out: *mut u8) -> i32 {
    super::stdio::fstat(fd, out, true)
}

pub extern "system" fn _fstat64i32(fd: i32, out: *mut u8) -> i32 {
    super::stdio::fstat(fd, out, false)
}

pub(super) unsafe fn stat_from_metadata(out: *mut u8, m: &Metadata, wide_size: bool) {
    // SAFETY: forwarded.
    unsafe { fill_stat(out, m, 2, wide_size, "") };
}

fn put_str(dst: *mut u8, size: usize, s: &str) -> i32 {
    if s.len() + 1 > size {
        if size > 0 {
            // SAFETY: a buffer of at least one unit.
            unsafe { dst.write(0) };
        }
        return fail(ERANGE);
    }
    // SAFETY: the title's buffer of `size`.
    unsafe {
        std::ptr::copy_nonoverlapping(s.as_ptr(), dst, s.len());
        dst.add(s.len()).write(0);
    }
    0
}

/// `s` and a NUL in a `malloc` block of at least `min` bytes, which the title may `free`.
fn malloc_str(s: &str, min: usize) -> *mut u8 {
    let p: *mut u8 = heap::malloc((s.len() + 1).max(min)).cast();
    if !p.is_null() {
        // SAFETY: a fresh block of at least `s.len() + 1` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(s.as_ptr(), p, s.len());
            p.add(s.len()).write(0);
        }
    }
    p
}

pub extern "system" fn _getcwd(buf: *mut u8, max: i32) -> *mut u8 {
    if !buf.is_null() && max <= 0 {
        bad_param(EINVAL);
        return std::ptr::null_mut();
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            set_errno(win_errno(&e));
            return std::ptr::null_mut();
        }
    };
    let s = cwd.to_string_lossy().into_owned();
    // No buffer: a malloc block of at least `max` bytes, as `_wgetcwd` makes (`heap::malloc`
    // sets ENOMEM when it fails).
    if buf.is_null() {
        return malloc_str(&s, max.max(0) as usize);
    }
    if put_str(buf, max as usize, &s) != 0 {
        std::ptr::null_mut()
    } else {
        buf
    }
}

pub extern "system" fn _wgetcwd(mut buf: *mut u16, max: i32) -> *mut u16 {
    if !buf.is_null() && max <= 0 {
        bad_param(EINVAL);
        return std::ptr::null_mut();
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            set_errno(win_errno(&e));
            return std::ptr::null_mut();
        }
    };
    let path: Vec<u16> = cwd.to_string_lossy().encode_utf16().chain([0]).collect();
    let max = max.max(0) as usize;
    if buf.is_null() {
        buf = heap::malloc(path.len().max(max) * size_of::<u16>()).cast();
        if buf.is_null() {
            return buf;
        }
    } else if path.len() > max {
        // SAFETY: `max` is positive and the caller supplied the buffer.
        unsafe { buf.write(0) };
        set_errno(ERANGE);
        return std::ptr::null_mut();
    }
    // SAFETY: the caller or malloc supplied enough space for the path and NUL.
    unsafe { std::ptr::copy_nonoverlapping(path.as_ptr(), buf, path.len()) };
    buf
}

/// An absolute path: a drive or rooted path stays, others go under the working folder.
fn full(path: &str) -> Option<String> {
    let rooted = path.starts_with(['/', '\\']) || path.as_bytes().get(1) == Some(&b':');
    if rooted {
        return Some(path.to_string());
    }
    let cwd = std::env::current_dir().ok()?;
    Some(
        cwd.join(path.replace('\\', "/"))
            .to_string_lossy()
            .into_owned(),
    )
}

pub extern "system" fn _fullpath(buf: *mut u8, path: *const u8, max: usize) -> *mut u8 {
    if path.is_null() {
        bad_param(EINVAL);
        return std::ptr::null_mut();
    }
    // SAFETY: a NUL-terminated path.
    let Some(f) = full(&String::from_utf8_lossy(unsafe { super::bytes(path) })) else {
        return std::ptr::null_mut();
    };
    // No buffer: a malloc block the title frees, whatever `max` says.
    if buf.is_null() {
        return malloc_str(&f, 0);
    }
    if put_str(buf, max, &f) != 0 {
        std::ptr::null_mut()
    } else {
        buf
    }
}

// _splitpath / _makepath over either width.

fn is_sep<C: Ch>(c: C) -> bool {
    c.get() == u32::from(b'/') || c.get() == u32::from(b'\\')
}

/// `(drive, dir, fname, ext)` as ranges of the path.
fn split<C: Ch>(p: &[C]) -> [std::ops::Range<usize>; 4] {
    let drive = if p.len() >= 2 && p[1].get() == u32::from(b':') {
        2
    } else {
        0
    };
    let dir_end = p
        .iter()
        .rposition(|&c| is_sep(c))
        .map_or(drive, |i| (i + 1).max(drive));
    let ext_start = p[dir_end..]
        .iter()
        .rposition(|c| c.get() == u32::from(b'.'))
        .map_or(p.len(), |i| dir_end + i);
    [
        0..drive,
        drive..dir_end,
        dir_end..ext_start,
        ext_start..p.len(),
    ]
}

unsafe fn copy_out<C: Ch>(dst: *mut C, size: usize, src: &[C], secure: bool) -> i32 {
    if dst.is_null() {
        return 0;
    }
    if secure && src.len() + 1 > size {
        if size > 0 {
            // SAFETY: a buffer of at least one unit.
            unsafe { dst.write(C::default()) };
        }
        return bad_param(ERANGE);
    }
    // SAFETY: the title's buffer.
    unsafe {
        std::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
        dst.add(src.len()).write(C::default());
    }
    0
}

unsafe fn clear_outputs<C: Ch>(out: [(*mut C, usize); 4]) {
    for (dst, size) in out {
        if !dst.is_null() && size > 0 {
            // SAFETY: a buffer of at least one unit.
            unsafe { dst.write(C::default()) };
        }
    }
}

unsafe fn g_splitpath<C: Ch>(path: *const C, out: [(*mut C, usize); 4], secure: bool) -> i32 {
    if secure && out.iter().any(|&(dst, size)| dst.is_null() != (size == 0)) {
        // SAFETY: only non-null buffers with positive size are cleared.
        unsafe { clear_outputs(out) };
        return bad_param(EINVAL);
    }
    if path.is_null() {
        if secure {
            // SAFETY: only non-null buffers with positive size are cleared.
            unsafe { clear_outputs(out) };
        }
        return bad_param(EINVAL);
    }
    // SAFETY: a NUL-terminated path and the title's buffers.
    unsafe {
        let p = units(path);
        let parts = split(p);
        for (range, (dst, size)) in parts.iter().zip(out) {
            let r = copy_out(dst, size, &p[range.clone()], secure);
            if r != 0 {
                if secure {
                    clear_outputs(out);
                }
                return r;
            }
        }
    }
    0
}

pub extern "system" fn _splitpath(
    path: *const u8,
    drive: *mut u8,
    dir: *mut u8,
    fname: *mut u8,
    ext: *mut u8,
) {
    // SAFETY: the title's buffers (`_MAX_*` sized).
    unsafe {
        g_splitpath(
            path,
            [(drive, 3), (dir, 256), (fname, 256), (ext, 256)],
            false,
        )
    };
}

pub extern "system" fn _wsplitpath(
    path: *const u16,
    drive: *mut u16,
    dir: *mut u16,
    fname: *mut u16,
    ext: *mut u16,
) {
    // SAFETY: the title's buffers (`_MAX_*` sized).
    unsafe {
        g_splitpath(
            path,
            [(drive, 3), (dir, 256), (fname, 256), (ext, 256)],
            false,
        )
    };
}

#[allow(clippy::too_many_arguments)]
pub extern "system" fn _splitpath_s(
    path: *const u8,
    drive: *mut u8,
    drive_size: usize,
    dir: *mut u8,
    dir_size: usize,
    fname: *mut u8,
    fname_size: usize,
    ext: *mut u8,
    ext_size: usize,
) -> i32 {
    // SAFETY: the title's buffers.
    unsafe {
        g_splitpath(
            path,
            [
                (drive, drive_size),
                (dir, dir_size),
                (fname, fname_size),
                (ext, ext_size),
            ],
            true,
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub extern "system" fn _wsplitpath_s(
    path: *const u16,
    drive: *mut u16,
    drive_size: usize,
    dir: *mut u16,
    dir_size: usize,
    fname: *mut u16,
    fname_size: usize,
    ext: *mut u16,
    ext_size: usize,
) -> i32 {
    // SAFETY: the title's buffers.
    unsafe {
        g_splitpath(
            path,
            [
                (drive, drive_size),
                (dir, dir_size),
                (fname, fname_size),
                (ext, ext_size),
            ],
            true,
        )
    }
}

unsafe fn g_makepath<C: Ch>(
    out: *mut C,
    size: usize,
    drive: *const C,
    dir: *const C,
    fname: *const C,
    ext: *const C,
    secure: bool,
) -> i32 {
    if out.is_null() || (secure && size == 0) {
        return bad_param(EINVAL);
    }
    let mut v: Vec<C> = Vec::new();
    // SAFETY: NUL-terminated components or null.
    unsafe {
        let part = |p: *const C| if p.is_null() { &[][..] } else { units(p) };
        let (d, dr, f, e) = (part(drive), part(dir), part(fname), part(ext));
        if let Some(&first) = d.first() {
            v.push(first);
            v.push(C::make(u32::from(b':')));
        }
        v.extend_from_slice(dr);
        if dr.last().is_some_and(|&c| !is_sep(c)) {
            v.push(C::make(u32::from(b'\\')));
        }
        v.extend_from_slice(f);
        if e.first().is_some_and(|c| c.get() != u32::from(b'.')) {
            v.push(C::make(u32::from(b'.')));
        }
        v.extend_from_slice(e);
    }
    // SAFETY: the title's buffer.
    unsafe { copy_out(out, if secure { size } else { usize::MAX }, &v, secure) }
}

pub extern "system" fn _makepath(
    out: *mut u8,
    drive: *const u8,
    dir: *const u8,
    fname: *const u8,
    ext: *const u8,
) {
    // SAFETY: the title's buffers.
    unsafe { g_makepath(out, 0, drive, dir, fname, ext, false) };
}

pub extern "system" fn _wmakepath(
    out: *mut u16,
    drive: *const u16,
    dir: *const u16,
    fname: *const u16,
    ext: *const u16,
) {
    // SAFETY: the title's buffers.
    unsafe { g_makepath(out, 0, drive, dir, fname, ext, false) };
}

pub extern "system" fn _makepath_s(
    out: *mut u8,
    size: usize,
    drive: *const u8,
    dir: *const u8,
    fname: *const u8,
    ext: *const u8,
) -> i32 {
    // SAFETY: the title's buffers.
    unsafe { g_makepath(out, size, drive, dir, fname, ext, true) }
}

pub extern "system" fn _wmakepath_s(
    out: *mut u16,
    size: usize,
    drive: *const u16,
    dir: *const u16,
    fname: *const u16,
    ext: *const u16,
) -> i32 {
    // SAFETY: the title's buffers.
    unsafe { g_makepath(out, size, drive, dir, fname, ext, true) }
}

// _findfirst64 and relatives: directory listings by wildcard.

type Entry = (String, Option<Metadata>);

struct Listing {
    entries: Vec<Entry>,
    next: usize,
}

static FINDS: std::sync::Mutex<Vec<Option<Listing>>> = std::sync::Mutex::new(Vec::new());

fn finds() -> std::sync::MutexGuard<'static, Vec<Option<Listing>>> {
    FINDS.lock().unwrap_or_else(|e| e.into_inner())
}

/// What `_findfirst` shows for `pattern` (`dir/*.ext`): the matching names
/// with `.` and `..`, in directory order.
fn list(path: Option<PathBuf>) -> Option<Vec<Entry>> {
    let path = path?;
    let pattern: Vec<char> = path.file_name()?.to_string_lossy().chars().collect();
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let read = std::fs::read_dir(dir).ok()?;
    let mut entries: Vec<Entry> = files::matching_entries(read, &pattern)
        .map(|(name, entry)| (name, entry.and_then(|e| files::find_metadata(e.metadata()))))
        .collect();
    for (name, meta) in entries.iter_mut() {
        if meta.is_none() {
            *meta = files::find_metadata(std::fs::metadata(dir.join(&*name)));
        }
    }
    Some(entries)
}

/// `_finddata64i32_t` and friends: attributes, three times, a size (32 or
/// 64 bits) and the name (260 units of either width).
unsafe fn fill_find(out: *mut u8, e: &Entry, wide_name: bool, wide_size: bool) {
    let (name, meta) = e;
    let (len, attrib) = meta.as_ref().map_or((0, 0), |m| {
        let a = if m.is_dir() { 0x10 } else { 0 } | if m.permissions().readonly() { 1 } else { 0 };
        (if m.is_dir() { 0 } else { m.len() }, a)
    });
    // SAFETY: the title's structure of at least 296 (or 560) bytes.
    unsafe {
        let name_at = if wide_size { 40 } else { 36 };
        std::ptr::write_bytes(out, 0, name_at);
        out.cast::<u32>().write_unaligned(attrib);
        if let Some(m) = meta {
            out.add(8)
                .cast::<i64>()
                .write_unaligned(secs(m.created().or_else(|_| m.modified())));
            out.add(16)
                .cast::<i64>()
                .write_unaligned(secs(m.accessed()));
            out.add(24)
                .cast::<i64>()
                .write_unaligned(secs(m.modified()));
        }
        if wide_size {
            out.add(32).cast::<i64>().write_unaligned(len as i64);
        } else {
            out.add(32)
                .cast::<u32>()
                .write_unaligned(len.min(u64::from(u32::MAX)) as u32);
        };
        if wide_name {
            let w: Vec<u16> = name.encode_utf16().take(259).collect();
            for (i, c) in w.iter().enumerate() {
                out.add(name_at + i * 2).cast::<u16>().write_unaligned(*c);
            }
            out.add(name_at + w.len() * 2)
                .cast::<u16>()
                .write_unaligned(0);
        } else {
            let b = &name.as_bytes()[..name.len().min(259)];
            std::ptr::copy_nonoverlapping(b.as_ptr(), out.add(name_at), b.len());
            out.add(name_at + b.len()).write(0);
        }
    }
}

fn find_first(
    path: Option<Option<PathBuf>>,
    out: *mut u8,
    wide_name: bool,
    wide_size: bool,
) -> isize {
    // `None`: no file specification at all, which is an invalid parameter (EINVAL).
    let Some(path) = path else {
        bad_param(EINVAL);
        return -1;
    };
    if out.is_null() {
        bad_param(EINVAL);
        return -1;
    }
    let Some(entries) = list(path).filter(|e| !e.is_empty()) else {
        set_errno(ENOENT);
        return -1;
    };
    // SAFETY: the title's structure.
    unsafe { fill_find(out, &entries[0], wide_name, wide_size) };
    let mut table = finds();
    let listing = Some(Listing { entries, next: 1 });
    let slot = match table.iter().position(Option::is_none) {
        Some(i) => {
            table[i] = listing;
            i
        }
        None => {
            table.push(listing);
            table.len() - 1
        }
    };
    slot as isize + 1
}

fn find_next(handle: isize, out: *mut u8, wide_name: bool, wide_size: bool) -> i32 {
    if out.is_null() {
        bad_param(EINVAL);
        return -1;
    }
    let mut table = finds();
    let Some(Some(l)) = handle
        .checked_sub(1)
        .and_then(|i| usize::try_from(i).ok())
        .and_then(|i| table.get_mut(i))
    else {
        // FindNextFile fails on a bad handle, an error the CRT reports as EINVAL.
        set_errno(EINVAL);
        return -1;
    };
    let Some(e) = l.entries.get(l.next) else {
        set_errno(ENOENT);
        return -1;
    };
    l.next += 1;
    // SAFETY: the title's structure.
    unsafe { fill_find(out, e, wide_name, wide_size) };
    0
}

/// A find's file specification: `None` when there is none (a null pointer).
fn narrow_spec(path: *const u8) -> Option<Option<PathBuf>> {
    (!path.is_null()).then(|| narrow_name(path).0)
}

fn wide_spec(path: *const u16) -> Option<Option<PathBuf>> {
    (!path.is_null()).then(|| wide_name(path).0)
}

pub extern "system" fn _findfirst64i32(path: *const u8, out: *mut u8) -> isize {
    find_first(narrow_spec(path), out, false, false)
}

pub extern "system" fn _findfirst64(path: *const u8, out: *mut u8) -> isize {
    find_first(narrow_spec(path), out, false, true)
}

pub extern "system" fn _wfindfirst64i32(path: *const u16, out: *mut u8) -> isize {
    find_first(wide_spec(path), out, true, false)
}

pub extern "system" fn _wfindfirst64(path: *const u16, out: *mut u8) -> isize {
    find_first(wide_spec(path), out, true, true)
}

pub extern "system" fn _findnext64i32(handle: isize, out: *mut u8) -> i32 {
    find_next(handle, out, false, false)
}

pub extern "system" fn _findnext64(handle: isize, out: *mut u8) -> i32 {
    find_next(handle, out, false, true)
}

pub extern "system" fn _wfindnext64i32(handle: isize, out: *mut u8) -> i32 {
    find_next(handle, out, true, false)
}

pub extern "system" fn _wfindnext64(handle: isize, out: *mut u8) -> i32 {
    find_next(handle, out, true, true)
}

pub extern "system" fn _findclose(handle: isize) -> i32 {
    let mut table = finds();
    match handle
        .checked_sub(1)
        .and_then(|i| usize::try_from(i).ok())
        .and_then(|i| table.get_mut(i))
    {
        Some(slot @ Some(_)) => {
            *slot = None;
            0
        }
        _ => {
            // As Microsoft documents for any failure of `_findclose`.
            set_errno(ENOENT);
            -1
        }
    }
}

exports!(pub(super) fn lookup;
    _stat64, _stat64i32, _wstat64, _wstat64i32, _access, _waccess, _access_s, _waccess_s, _mkdir,
    _wmkdir, _rmdir, _wrmdir, _chdir, _wchdir, remove, _wremove, rename, _wrename, _chmod,
    _wchmod, _fstat64, _fstat64i32, _getcwd, _wgetcwd, _fullpath, _splitpath, _wsplitpath, _splitpath_s,
    _wsplitpath_s, _makepath, _wmakepath, _makepath_s, _wmakepath_s, _findfirst64i32,
    _findfirst64, _wfindfirst64i32, _wfindfirst64, _findnext64i32, _findnext64,
    _wfindnext64i32, _wfindnext64, _findclose;
    "_unlink" => remove, "_wunlink" => _wremove, "_stati64" => _stat64, "_wstati64" => _wstat64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wgetcwd_uses_the_guest_path_and_a_freeable_buffer() {
        let mut narrow = [0u8; 4096];
        assert_eq!(_getcwd(narrow.as_mut_ptr(), 4096), narrow.as_mut_ptr());
        let expected: Vec<u16> = text(&narrow).encode_utf16().collect();
        let mut wide = vec![0xFFFF; expected.len() + 1];
        let capacity = wide.len() as i32;
        assert_eq!(_wgetcwd(wide.as_mut_ptr(), capacity), wide.as_mut_ptr());
        assert_eq!(wide[..wide.len() - 1], expected);
        assert_eq!(wide[wide.len() - 1], 0);
        assert!(_wgetcwd(wide.as_mut_ptr(), capacity - 1).is_null());
        assert_eq!(super::super::errno(), ERANGE);
        assert!(_wgetcwd(wide.as_mut_ptr(), 0).is_null());
        assert_eq!(super::super::errno(), EINVAL);
        for max in [0, 1, capacity + 8] {
            let allocated = _wgetcwd(std::ptr::null_mut(), max);
            assert!(!allocated.is_null());
            assert_eq!(unsafe { super::super::wide(allocated) }, expected);
            assert!(heap::_msize(allocated.cast()) >= max as usize * size_of::<u16>());
            heap::free(allocated.cast());
        }
    }

    #[test]
    fn getcwd_and_fullpath_malloc_when_given_no_buffer() {
        let mut narrow = [0u8; 4096];
        assert_eq!(_getcwd(narrow.as_mut_ptr(), 4096), narrow.as_mut_ptr());
        let cwd = text(&narrow);
        // `_getcwd(NULL, 0)`, then `free`, is Microsoft's own example.
        for max in [0, 1, 4096] {
            let p = _getcwd(std::ptr::null_mut(), max);
            assert!(!p.is_null());
            assert_eq!(unsafe { super::super::bytes(p) }, cwd.as_bytes());
            assert!(heap::_msize(p.cast()) >= max as usize);
            heap::free(p.cast());
        }
        assert!(_getcwd(narrow.as_mut_ptr(), 0).is_null());
        assert_eq!(super::super::errno(), EINVAL);
        let p = _fullpath(std::ptr::null_mut(), c("x").as_ptr(), 0);
        assert!(!p.is_null());
        assert_eq!(unsafe { super::super::bytes(p) }, format!("{cwd}/x").as_bytes());
        heap::free(p.cast());
    }

    fn c(s: &str) -> Vec<u8> {
        s.bytes().chain([0]).collect()
    }

    fn text(b: &[u8]) -> String {
        String::from_utf8_lossy(&b[..b.iter().position(|&x| x == 0).unwrap()]).into_owned()
    }

    #[test]
    fn stat_access_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("sub");
        let dp = c(d.to_str().unwrap());
        assert_eq!(_mkdir(dp.as_ptr()), 0);
        assert_eq!(_mkdir(dp.as_ptr()), -1);
        assert_eq!(super::super::errno(), EEXIST);
        let f = d.join("a.txt");
        std::fs::write(&f, b"12345").unwrap();
        let fp = c(f.to_str().unwrap());
        let mut st = [0u8; 56];
        assert_eq!(_stat64(fp.as_ptr(), st.as_mut_ptr()), 0);
        let size = i64::from_le_bytes(st[24..32].try_into().unwrap());
        let mode = u16::from_le_bytes(st[6..8].try_into().unwrap());
        assert_eq!(size, 5);
        assert_eq!(mode & 0xF000, S_IFREG);
        assert_eq!(mode & 0x1FF, S_IREAD | S_IWRITE);
        let mtime = i64::from_le_bytes(st[40..48].try_into().unwrap());
        assert!(mtime > 1_600_000_000);
        let mut st32 = [0u8; 48];
        assert_eq!(_stat64i32(fp.as_ptr(), st32.as_mut_ptr()), 0);
        assert_eq!(i32::from_le_bytes(st32[20..24].try_into().unwrap()), 5);
        assert_eq!(_stat64(dp.as_ptr(), st.as_mut_ptr()), 0);
        assert_eq!(
            u16::from_le_bytes(st[6..8].try_into().unwrap()) & 0xF000,
            S_IFDIR
        );
        let missing = c(d.join("nope").to_str().unwrap());
        assert_eq!(_stat64(missing.as_ptr(), st.as_mut_ptr()), -1);
        assert_eq!(super::super::errno(), ENOENT);
        assert_eq!(_access(fp.as_ptr(), 6), 0);
        assert_eq!(_access(missing.as_ptr(), 0), -1);
        assert_eq!(_chmod(fp.as_ptr(), 0x100), 0);
        assert_eq!(_access(fp.as_ptr(), 2), -1);
        assert_eq!(_access_s(fp.as_ptr(), 2), EACCES);
        assert_eq!(_chmod(fp.as_ptr(), 0x180), 0);
        // Windows does not rename over a file.
        let g = d.join("b.txt");
        std::fs::write(&g, b"x").unwrap();
        let gp = c(g.to_str().unwrap());
        assert_eq!(rename(fp.as_ptr(), gp.as_ptr()), -1);
        assert_eq!(super::super::errno(), EEXIST);
        assert_eq!(std::fs::read(&f).unwrap(), b"12345");
        assert_eq!(std::fs::read(&g).unwrap(), b"x");
        assert_eq!(remove(gp.as_ptr()), 0);
        assert_eq!(rename(fp.as_ptr(), gp.as_ptr()), 0);
        assert!(!f.try_exists().unwrap());
        assert_eq!(std::fs::read(&g).unwrap(), b"12345");
        assert_eq!(remove(gp.as_ptr()), 0);
        assert_eq!(remove(gp.as_ptr()), -1);
        assert_eq!(_rmdir(dp.as_ptr()), 0);
    }

    #[test]
    fn rename_falls_back_when_exclusive_rename_is_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("source");
        let to = dir.path().join("destination");
        let fallback =
            || rename_error(&from, &to, std::io::Error::from_raw_os_error(libc::ENOTSUP));

        std::fs::write(&from, b"first").unwrap();
        assert_eq!(fallback(), 0);
        assert!(!from.try_exists().unwrap());
        assert_eq!(std::fs::read(&to).unwrap(), b"first");

        std::fs::write(&from, b"second").unwrap();
        assert_eq!(fallback(), -1);
        assert_eq!(super::super::errno(), EEXIST);
        assert_eq!(std::fs::read(&from).unwrap(), b"second");
        assert_eq!(std::fs::read(&to).unwrap(), b"first");

        std::fs::remove_file(&to).unwrap();
        let target = Path::new("missing");
        std::os::unix::fs::symlink(target, &to).unwrap();
        set_errno(0);
        assert_eq!(fallback(), -1);
        assert_eq!(super::super::errno(), EEXIST);
        assert_eq!(std::fs::read(&from).unwrap(), b"second");
        assert_eq!(std::fs::read_link(&to).unwrap(), target);
    }

    #[test]
    fn wrename_preserves_dangling_destination_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("source");
        let to = dir.path().join("destination");
        let target = std::path::Path::new("missing");
        std::fs::write(&from, b"source bytes").unwrap();
        std::os::unix::fs::symlink(target, &to).unwrap();

        let wide = |p: &std::path::Path| {
            p.to_str()
                .unwrap()
                .encode_utf16()
                .chain([0])
                .collect::<Vec<_>>()
        };
        assert_eq!(_wrename(wide(&from).as_ptr(), wide(&to).as_ptr()), -1);
        assert_eq!(super::super::errno(), EEXIST);
        assert_eq!(std::fs::read(&from).unwrap(), b"source bytes");
        assert_eq!(std::fs::read_link(&to).unwrap(), target);
    }

    #[test]
    fn find_first_only_reads_metadata_for_matching_names() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..999 {
            std::fs::File::create(dir.path().join(format!("{i}.bin"))).unwrap();
        }
        std::fs::write(dir.path().join("slot.sav"), b"saved").unwrap();

        let pattern = c(dir.path().join("*.sav").to_str().unwrap());
        let mut data = [0u8; 304];
        files::FIND_METADATA_LOOKUPS.set(0);
        let handle = _findfirst64i32(pattern.as_ptr(), data.as_mut_ptr());
        assert!(handle > 0);
        assert_eq!(files::FIND_METADATA_LOOKUPS.get(), 1);
        assert_eq!(text(&data[36..]), "slot.sav");
        assert_eq!(u32::from_le_bytes(data[32..36].try_into().unwrap()), 5);
        assert_eq!(_findnext64i32(handle, data.as_mut_ptr()), -1);
        assert_eq!(super::super::errno(), ENOENT);
        assert_eq!(_findclose(handle), 0);
    }

    #[test]
    fn findfirst_lists_by_wildcard() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"12").unwrap();
        std::fs::write(dir.path().join("b.bin"), b"1").unwrap();
        std::fs::create_dir(dir.path().join("sub.txt")).unwrap();
        let pattern = c(&format!("{}/*.txt", dir.path().to_str().unwrap()));
        let mut data = [0u8; 304];
        let h = _findfirst64i32(pattern.as_ptr(), data.as_mut_ptr());
        assert!(h > 0);
        let mut seen = Vec::new();
        loop {
            let name = text(&data[36..]);
            let attrib = u32::from_le_bytes(data[0..4].try_into().unwrap());
            let size = u32::from_le_bytes(data[32..36].try_into().unwrap());
            seen.push((name, attrib & 0x10 != 0, size));
            if _findnext64i32(h, data.as_mut_ptr()) != 0 {
                break;
            }
        }
        seen.sort();
        assert_eq!(
            seen,
            [("a.txt".into(), false, 2), ("sub.txt".into(), true, 0)]
        );
        assert_eq!(super::super::errno(), ENOENT);
        assert_eq!(_findclose(h), 0);
        assert_eq!(_findclose(h), -1);
        let none = c(&format!("{}/*.zzz", dir.path().to_str().unwrap()));
        assert_eq!(_findfirst64i32(none.as_ptr(), data.as_mut_ptr()), -1);
        assert_eq!(super::super::errno(), ENOENT);
        // `.` and `..` match `*`; the wide, 64-bit-size form has its name at 40.
        let all: Vec<u16> = format!("{}/*", dir.path().to_str().unwrap())
            .encode_utf16()
            .chain([0])
            .collect();
        let mut wdata = [0u8; 560];
        let h = _wfindfirst64(all.as_ptr(), wdata.as_mut_ptr());
        assert!(h > 0);
        assert_eq!(wdata[40], b'.');
        assert_eq!(_findclose(h), 0);
    }

    #[test]
    fn find_calls_set_the_errno_microsoft_documents() {
        // `_findfirst`: EINVAL for a null file specification; `_findnext`: EINVAL for an
        // error other than "no more files" (a bad handle); `_findclose`: ENOENT on failure.
        let mut data = [0u8; 304];
        set_errno(0);
        assert_eq!(_findfirst64i32(std::ptr::null(), data.as_mut_ptr()), -1);
        assert_eq!(super::super::errno(), EINVAL);
        set_errno(0);
        assert_eq!(_wfindfirst64(std::ptr::null(), data.as_mut_ptr()), -1);
        assert_eq!(super::super::errno(), EINVAL);
        for handle in [0, -1, 9999] {
            set_errno(0);
            assert_eq!(_findnext64i32(handle, data.as_mut_ptr()), -1, "{handle}");
            assert_eq!(super::super::errno(), EINVAL, "{handle}");
            set_errno(0);
            assert_eq!(_findclose(handle), -1, "{handle}");
            assert_eq!(super::super::errno(), ENOENT, "{handle}");
        }
    }

    #[test]
    fn wide_paths_and_virtual_drives_work() {
        let dir = tempfile::tempdir().unwrap();
        files::mount_at('Q', dir.path());
        std::fs::write(dir.path().join("w.bin"), b"abc").unwrap();
        let w: Vec<u16> = r"Q:\w.bin".encode_utf16().chain([0]).collect();
        let mut st = [0u8; 56];
        assert_eq!(_wstat64(w.as_ptr(), st.as_mut_ptr()), 0);
        assert_eq!(i64::from_le_bytes(st[24..32].try_into().unwrap()), 3);
        // The device is the drive's index.
        assert_eq!(u32::from_le_bytes(st[0..4].try_into().unwrap()), 16);
        let d: Vec<u16> = r"Q:\newdir".encode_utf16().chain([0]).collect();
        assert_eq!(_wmkdir(d.as_ptr()), 0);
        assert!(dir.path().join("newdir").is_dir());
        assert_eq!(_wrmdir(d.as_ptr()), 0);
        assert_eq!(_wremove(w.as_ptr()), 0);
        assert!(!dir.path().join("w.bin").exists());
    }

    #[test]
    fn paths_split_and_join() {
        let p = c(r"C:\dir\sub\file.tar.gz");
        let (mut dr, mut di, mut f, mut e) = ([0u8; 4], [0u8; 64], [0u8; 64], [0u8; 64]);
        _splitpath(
            p.as_ptr(),
            dr.as_mut_ptr(),
            di.as_mut_ptr(),
            f.as_mut_ptr(),
            e.as_mut_ptr(),
        );
        assert_eq!(
            (text(&dr), text(&di), text(&f), text(&e)),
            (
                "C:".into(),
                r"\dir\sub\".into(),
                "file.tar".into(),
                ".gz".into()
            )
        );
        let mut out = [0u8; 64];
        _makepath(
            out.as_mut_ptr(),
            c("C:").as_ptr(),
            c(r"\dir\sub").as_ptr(),
            c("file").as_ptr(),
            c("txt").as_ptr(),
        );
        assert_eq!(text(&out), r"C:\dir\sub\file.txt");
        assert_eq!(
            _makepath_s(
                out.as_mut_ptr(),
                4,
                c("C:").as_ptr(),
                c("d").as_ptr(),
                c("f").as_ptr(),
                c("e").as_ptr()
            ),
            ERANGE
        );
        let mut small = [0u8; 2];
        assert_eq!(
            _splitpath_s(
                c("a/b.c").as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
                small.as_mut_ptr(),
                1,
                std::ptr::null_mut(),
                0
            ),
            ERANGE
        );
        let w: Vec<u16> = r"x\y.z".encode_utf16().chain([0]).collect();
        let mut wf = [0u16; 8];
        _wsplitpath(
            w.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            wf.as_mut_ptr(),
            std::ptr::null_mut(),
        );
        assert_eq!(String::from_utf16_lossy(&wf[..1]), "y");
        let mut cwd = [0u8; 512];
        assert!(!_getcwd(cwd.as_mut_ptr(), 512).is_null());
        let mut fp = [0u8; 512];
        assert!(!_fullpath(fp.as_mut_ptr(), c("rel.txt").as_ptr(), 512).is_null());
        assert!(text(&fp).ends_with("rel.txt"));
        assert_eq!(
            text(&{
                let mut b = [0u8; 16];
                _fullpath(b.as_mut_ptr(), c(r"C:\x").as_ptr(), 16);
                b
            }),
            r"C:\x"
        );
    }

    #[test]
    fn zero_size_path_buffers_and_find_edge_cases() {
        // Zero-sized buffers must never be written out of bounds.
        let mut guard = [0xAAu8; 4];
        set_errno(0);
        assert!(_fullpath(guard.as_mut_ptr(), c(r"C:\x").as_ptr(), 0).is_null());
        assert_eq!(super::super::errno(), ERANGE);
        assert_eq!(guard, [0xAA; 4]);

        assert_eq!(
            _makepath_s(
                guard.as_mut_ptr(),
                0,
                c("C:").as_ptr(),
                c("d").as_ptr(),
                c("f").as_ptr(),
                c("e").as_ptr()
            ),
            EINVAL
        );
        assert_eq!(guard, [0xAA; 4]);

        let mut drive = [b'x'; 4];
        let mut ext = [b'y'; 2];
        assert_eq!(
            _splitpath_s(
                c(r"C:\a\b.txt").as_ptr(),
                drive.as_mut_ptr(),
                drive.len(),
                guard.as_mut_ptr(),
                0,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            ),
            EINVAL
        );
        assert_eq!(guard, [0xAA; 4]);
        assert_eq!(drive[0], 0);

        // Non-zero size with a null pointer is `EINVAL`.
        assert_eq!(
            _splitpath_s(
                c(r"C:\a\b.txt").as_ptr(),
                std::ptr::null_mut(),
                4,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            ),
            EINVAL
        );

        // Overflow on a later component clears earlier outputs too.
        drive = [b'x'; 4];
        assert_eq!(
            _splitpath_s(
                c(r"C:\a\b.txt").as_ptr(),
                drive.as_mut_ptr(),
                drive.len(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
                ext.as_mut_ptr(),
                ext.len(),
            ),
            ERANGE
        );
        assert_eq!(drive[0], 0);
        assert_eq!(ext[0], 0);

        // `_findnext*` rejects null output, and `isize::MIN` handles do not underflow.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"1").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"2").unwrap();
        let pattern = c(&format!("{}/*.txt", dir.path().to_str().unwrap()));
        let mut data = [0u8; 304];
        let h = _findfirst64i32(pattern.as_ptr(), data.as_mut_ptr());
        assert!(h > 0);
        set_errno(0);
        assert_eq!(_findnext64i32(h, std::ptr::null_mut()), -1);
        assert_eq!(super::super::errno(), EINVAL);
        assert_eq!(_findclose(h), 0);

        set_errno(0);
        assert_eq!(_findnext64i32(isize::MIN, data.as_mut_ptr()), -1);
        assert_eq!(super::super::errno(), EINVAL);
        set_errno(0);
        assert_eq!(_findclose(isize::MIN), -1);
        assert_eq!(super::super::errno(), ENOENT);

        // Missing metadata zeroes timestamp fields instead of keeping stale bytes.
        let mut stale = [0xFFu8; 304];
        // SAFETY: `stale` is large enough for `_finddata64i32_t`.
        unsafe { fill_find(stale.as_mut_ptr(), &("ghost".into(), None), false, false) };
        assert_eq!(&stale[..36], &[0u8; 36]);
        assert_eq!(&stale[36..42], b"ghost\0");
    }
}
