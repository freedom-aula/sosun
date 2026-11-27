//! Files for the native host: the calls of `modules::files` that reach the
//! host's own kernel32 on Windows, on POSIX instead.
//!
//! The title's storage is drives (`G:\..`, `T:\..`, the persistent
//! storage's): a drive is a mounted host folder, and a path on one is
//! rewritten to the host path, as `modules::files` does for Windows (there
//! the rewritten path goes to the host's `CreateFileW`; here it goes to
//! `open`). A path on a drive that is not mounted is not found.
//!
//! Done: `CreateFile(W|A)`, `ReadFile`, `WriteFile`, `SetFilePointer(Ex)`,
//! `GetFileSize(Ex)`, `SetEndOfFile`, `FlushFileBuffers`,
//! `GetFileAttributes(W|A|ExW)`, `FindFirstFile(W|ExW)`, `FindNextFileW`,
//! `FindClose`, `DeleteFileW`, `CreateDirectoryW`, `RemoveDirectoryW`,
//! `GetOverlappedResult`. Overlapped reads and writes complete before the call
//! returns, which is a legal outcome.
//!
//! Not done: the C runtime's file functions (`fopen`, `_wopen`, `_stat64`,
//! ...), `MoveFileEx`, `GetDiskFreeSpace`, `CreateFile2`, file mappings,
//! share modes and locking, and `dstorage.rs` (see `docs/native-host.md`).

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CStr, c_char, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use super::{HANDLES, Object, lock, new_handle, set_error};

const INVALID_HANDLE_VALUE: usize = usize::MAX;
const INVALID_FILE_SIZE: u32 = 0xFFFF_FFFF;
const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_NO_MORE_FILES: u32 = 18;
const ERROR_HANDLE_EOF: u32 = 38;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_ALREADY_EXISTS: u32 = 183;
const STATUS_END_OF_FILE: usize = 0xC000_0011;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const FILE_READ_DATA: u32 = 1;
const FILE_WRITE_DATA: u32 = 2;
const FILE_APPEND_DATA: u32 = 4;
const CREATE_NEW: u32 = 1;
const CREATE_ALWAYS: u32 = 2;
const OPEN_EXISTING: u32 = 3;
const OPEN_ALWAYS: u32 = 4;
const TRUNCATE_EXISTING: u32 = 5;
const FILE_ATTRIBUTE_READONLY: u32 = 1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
/// Seconds from 1601 (FILETIME's epoch) to 1970.
const EPOCH_DIFFERENCE: u64 = 11_644_473_600;

// Drives.

static DRIVES: Mutex<Vec<(u8, PathBuf)>> = Mutex::new(Vec::new());

fn drives() -> MutexGuard<'static, Vec<(u8, PathBuf)>> {
    DRIVES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Gives the host folder `root` a drive letter (the same one if it has one):
/// the highest free letter.
pub fn mount(root: &Path) -> Option<char> {
    let root = std::path::absolute(root).ok()?;
    let mut drives = drives();
    if let Some((letter, _)) = drives.iter().find(|(_, r)| *r == root) {
        return Some(char::from(*letter));
    }
    let letter = (b'D'..=b'Z')
        .rev()
        .find(|l| !drives.iter().any(|(m, _)| m == l))?;
    drives.push((letter, root));
    Some(char::from(letter))
}

/// Gives `root` the letter the console fixes (`G`, `T`).
pub fn mount_at(letter: char, root: &Path) {
    let Ok(letter) = u8::try_from(letter.to_ascii_uppercase()) else {
        return;
    };
    let Ok(root) = std::path::absolute(root) else {
        return;
    };
    let mut drives = drives();
    drives.retain(|(l, _)| *l != letter);
    drives.push((letter, root));
}

/// The host path for a title's path: a drive path goes under its folder,
/// with `.` and `..` resolved here so that `..` stops at the root; any
/// other path is taken as it is, its backslashes made slashes.
pub(crate) fn translate(path: &[u16]) -> Option<PathBuf> {
    let text = String::from_utf16_lossy(path);
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let letter = bytes[0].to_ascii_uppercase();
        let root = drives().iter().find(|(l, _)| *l == letter)?.1.clone();
        let mut parts: Vec<&str> = Vec::new();
        for part in text[2..].split(['\\', '/']) {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
        return Some(parts.iter().fold(root, |p, c| p.join(c)));
    }
    Some(PathBuf::from(text.replace('\\', "/")))
}

/// A NUL-terminated wide string.
pub(super) fn wide_str(p: *const u16) -> Vec<u16> {
    // SAFETY: the title passes a NUL-terminated path.
    unsafe { std::slice::from_raw_parts(p, (0..).take_while(|&i| *p.add(i) != 0).count()) }.to_vec()
}

pub(crate) fn wide_path(p: *const u16) -> Option<PathBuf> {
    if p.is_null() {
        return None;
    }
    translate(&wide_str(p))
}

pub(super) fn narrow_path(p: *const c_char) -> Option<PathBuf> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the title passes a NUL-terminated path.
    let text = String::from_utf8_lossy(unsafe { CStr::from_ptr(p) }.to_bytes()).into_owned();
    translate(&text.encode_utf16().collect::<Vec<_>>())
}

/// The Win32 error for an I/O failure ([`crate::win32_errors`], `ERROR_ACCESS_DENIED` for the
/// rest). A missing file whose parent folder is missing too is `ERROR_PATH_NOT_FOUND`, as
/// `CreateFileW` says.
pub(crate) fn win32_error(e: &io::Error, path: &Path) -> u32 {
    match crate::win32_errors::code(e) {
        Some(ERROR_FILE_NOT_FOUND)
            if path
                .parent()
                .is_some_and(|p| !p.as_os_str().is_empty() && !p.exists()) =>
        {
            ERROR_PATH_NOT_FOUND
        }
        Some(code) => code,
        None => ERROR_ACCESS_DENIED,
    }
}

// File handles.

/// An open file and its file pointer.
pub(crate) struct FileObject {
    file: File,
    position: Mutex<u64>,
    writable: bool,
    /// Opened for `FILE_APPEND_DATA` without `FILE_WRITE_DATA`: every write goes to the end.
    append: bool,
}

/// The host's metadata of the open file `handle`.
pub(crate) fn metadata_of(handle: *mut c_void) -> Option<std::fs::Metadata> {
    file_of(handle)?.file.metadata().ok()
}

fn file_of(handle: *mut c_void) -> Option<Arc<FileObject>> {
    match lock(&HANDLES).get(&(handle as usize)) {
        Some(Object::File(f)) => Some(f.clone()),
        _ => None,
    }
}

fn invalid_handle() -> *mut c_void {
    INVALID_HANDLE_VALUE as *mut c_void
}

fn create(path: Option<PathBuf>, access: u32, disposition: u32, flags: u32) -> *mut c_void {
    let Some(path) = path else {
        set_error(ERROR_PATH_NOT_FOUND);
        return invalid_handle();
    };
    if std::env::var_os("VICTORY_TRACE_FILES").is_some() {
        tracing::info!(path = %path.display(), access = format_args!("{access:#x}"), "CreateFile");
    }
    let all = access & GENERIC_ALL != 0;
    let read = all || access & (GENERIC_READ | FILE_READ_DATA) != 0;
    let write = all || access & (GENERIC_WRITE | FILE_WRITE_DATA | FILE_APPEND_DATA) != 0;
    let append =
        !all && access & (GENERIC_WRITE | FILE_WRITE_DATA) == 0 && access & FILE_APPEND_DATA != 0;
    let existed = path.exists();
    let mut options = OpenOptions::new();
    options.read(read || !write).write(write);
    match disposition {
        CREATE_NEW => {
            options.create_new(true).write(true);
        }
        CREATE_ALWAYS => {
            options.create(true).truncate(true).write(true);
        }
        OPEN_EXISTING => {}
        OPEN_ALWAYS => {
            // Darwin permits O_RDONLY | O_CREAT, including when the file already exists.
            const O_CREAT: i32 = 0x0200;
            options.custom_flags(O_CREAT);
        }
        TRUNCATE_EXISTING if access & (GENERIC_WRITE | GENERIC_ALL) != 0 => {
            options.truncate(true);
        }
        _ => {
            set_error(ERROR_INVALID_PARAMETER);
            return invalid_handle();
        }
    }
    match options.open(&path) {
        Ok(file) => {
            set_error(
                if existed && matches!(disposition, CREATE_ALWAYS | OPEN_ALWAYS) {
                    ERROR_ALREADY_EXISTS
                } else {
                    0
                },
            );
            let _ = flags;
            new_handle(Object::File(Arc::new(FileObject {
                file,
                position: Mutex::new(0),
                writable: write,
                append,
            })))
        }
        Err(e) => {
            set_error(win32_error(&e, &path));
            invalid_handle()
        }
    }
}

pub extern "system" fn CreateFileW(
    path: *const u16,
    access: u32,
    _share: u32,
    _security: *const c_void,
    disposition: u32,
    flags: u32,
    _template: *mut c_void,
) -> *mut c_void {
    create(wide_path(path), access, disposition, flags)
}

pub extern "system" fn CreateFileA(
    path: *const c_char,
    access: u32,
    _share: u32,
    _security: *const c_void,
    disposition: u32,
    flags: u32,
    _template: *mut c_void,
) -> *mut c_void {
    create(narrow_path(path), access, disposition, flags)
}

/// The part of `OVERLAPPED` a synchronous completion fills in.
struct Overlapped(*mut u8);

impl Overlapped {
    fn offset(&self) -> u64 {
        // SAFETY: the title's OVERLAPPED: Offset, OffsetHigh at 16 and 20.
        unsafe { self.0.add(16).cast::<u64>().read_unaligned() }
    }

    fn complete(&self, status: usize, bytes: usize) {
        // SAFETY: the title's OVERLAPPED: Internal and InternalHigh at 0 and 8.
        unsafe {
            self.0.add(8).cast::<usize>().write(bytes);
            self.0.cast::<usize>().write(status);
            let event = self.0.add(24).cast::<usize>().read() & !1;
            if let Some(Object::Event(e)) = lock(&HANDLES).get(&event) {
                e.set();
                super::sync::signalled();
            }
        }
    }
}

/// Synchronous I/O holds the file position lock from offset selection through completion.
enum IoPosition<'a> {
    Synchronous(MutexGuard<'a, u64>),
    Overlapped(Overlapped),
}

impl<'a> IoPosition<'a> {
    fn new(file: &'a FileObject, overlapped: *mut u8) -> Self {
        if overlapped.is_null() {
            Self::Synchronous(lock(&file.position))
        } else {
            Self::Overlapped(Overlapped(overlapped))
        }
    }

    fn offset(&self) -> u64 {
        match self {
            Self::Synchronous(position) => **position,
            Self::Overlapped(ov) => ov.offset(),
        }
    }

    fn complete(self, offset: u64, bytes: usize) {
        match self {
            Self::Synchronous(mut position) => *position = offset + bytes as u64,
            Self::Overlapped(ov) => ov.complete(0, bytes),
        }
    }
}

pub extern "system" fn ReadFile(
    handle: *mut c_void,
    buffer: *mut u8,
    len: u32,
    read: *mut u32,
    overlapped: *mut u8,
) -> i32 {
    let Some(f) = file_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    if len > 0 && buffer.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if std::env::var_os("VICTORY_TRACE_FILES").is_some() {
        trace_read(&f.file, len);
    }
    let out = if len == 0 {
        &mut []
    } else {
        // SAFETY: the title's buffer of `len` bytes.
        unsafe { std::slice::from_raw_parts_mut(buffer, len as usize) }
    };
    let position = IoPosition::new(&f, overlapped);
    let at = position.offset();
    let mut done = 0;
    while done < out.len() {
        match f.file.read_at(&mut out[done..], at + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                set_error(ERROR_ACCESS_DENIED);
                return 0;
            }
        }
    }
    if !read.is_null() {
        // SAFETY: the title's out-pointer.
        unsafe { read.write(done as u32) };
    }
    if len > 0
        && done == 0
        && let IoPosition::Overlapped(ov) = &position
    {
        ov.complete(STATUS_END_OF_FILE, 0);
        set_error(ERROR_HANDLE_EOF);
        return 0;
    }
    position.complete(at, done);
    1
}

pub extern "system" fn WriteFile(
    handle: *mut c_void,
    buffer: *const u8,
    len: u32,
    written: *mut u32,
    overlapped: *mut u8,
) -> i32 {
    if !written.is_null() {
        // SAFETY: the title's byte-count out-pointer.
        unsafe { written.write(0) };
    }
    let Some(f) = file_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    if !f.writable {
        set_error(ERROR_ACCESS_DENIED);
        return 0;
    }
    if len > 0 && buffer.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let data = if len == 0 {
        &[]
    } else {
        // SAFETY: the title's buffer of `len` bytes.
        unsafe { std::slice::from_raw_parts(buffer, len as usize) }
    };
    let position = IoPosition::new(&f, overlapped);
    let at = if f.append {
        // Append-only access writes at the end whatever the offset, as Windows does.
        match f.file.metadata() {
            Ok(m) => m.len(),
            Err(_) => {
                set_error(ERROR_ACCESS_DENIED);
                return 0;
            }
        }
    } else {
        position.offset()
    };
    if f.file.write_all_at(data, at).is_err() {
        set_error(ERROR_ACCESS_DENIED);
        return 0;
    }
    position.complete(at, data.len());
    if !written.is_null() {
        // SAFETY: the title's out-pointer.
        unsafe { written.write(len) };
    }
    1
}

pub extern "system" fn GetOverlappedResult(
    _handle: *mut c_void,
    overlapped: *mut u8,
    bytes: *mut u32,
    _wait: i32,
) -> i32 {
    if !bytes.is_null() {
        // SAFETY: the title's OVERLAPPED and out-pointer.
        unsafe { bytes.write(overlapped.add(8).cast::<usize>().read() as u32) };
    }
    // SAFETY: Internal is the first pointer-sized field of the title's OVERLAPPED.
    if !overlapped.is_null() && unsafe { overlapped.cast::<usize>().read() } == STATUS_END_OF_FILE {
        set_error(ERROR_HANDLE_EOF);
        return 0;
    }
    1
}

fn seek(f: &FileObject, distance: i64, method: u32) -> Option<u64> {
    let mut position = lock(&f.position);
    let base = match method {
        0 => 0,
        1 => *position as i64,
        2 => f.file.metadata().ok()?.len() as i64,
        _ => return None,
    };
    let to = u64::try_from(base.checked_add(distance)?).ok()?;
    *position = to;
    Some(to)
}

pub extern "system" fn SetFilePointer(
    handle: *mut c_void,
    low: i32,
    high: *mut i32,
    method: u32,
) -> u32 {
    let Some(f) = file_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return INVALID_FILE_SIZE;
    };
    let distance = if high.is_null() {
        i64::from(low)
    } else {
        // SAFETY: the title's high word.
        i64::from(unsafe { high.read() }) << 32 | i64::from(low as u32)
    };
    let Some(to) = seek(&f, distance, method) else {
        set_error(ERROR_INVALID_PARAMETER);
        return INVALID_FILE_SIZE;
    };
    set_error(0);
    if !high.is_null() {
        // SAFETY: as above.
        unsafe { high.write((to >> 32) as i32) };
    }
    to as u32
}

pub extern "system" fn SetFilePointerEx(
    handle: *mut c_void,
    distance: i64,
    new_position: *mut i64,
    method: u32,
) -> i32 {
    let Some(f) = file_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    let Some(to) = seek(&f, distance, method) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    if !new_position.is_null() {
        // SAFETY: the title's out-pointer.
        unsafe { new_position.write(to as i64) };
    }
    1
}

pub extern "system" fn GetFileSizeEx(handle: *mut c_void, size: *mut i64) -> i32 {
    let Some(f) = file_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    let Ok(m) = f.file.metadata() else {
        set_error(ERROR_ACCESS_DENIED);
        return 0;
    };
    // SAFETY: the title's out-pointer.
    unsafe { size.write(m.len() as i64) };
    1
}

pub extern "system" fn GetFileSize(handle: *mut c_void, high: *mut u32) -> u32 {
    let mut size = 0;
    if GetFileSizeEx(handle, &mut size) == 0 {
        return INVALID_FILE_SIZE;
    }
    set_error(0);
    if !high.is_null() {
        // SAFETY: the title's out-pointer.
        unsafe { high.write((size >> 32) as u32) };
    }
    size as u32
}

pub extern "system" fn SetEndOfFile(handle: *mut c_void) -> i32 {
    let Some(f) = file_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    if !f.writable || f.append {
        set_error(ERROR_ACCESS_DENIED);
        return 0;
    }
    let position = lock(&f.position);
    i32::from(f.file.set_len(*position).is_ok())
}

pub extern "system" fn FlushFileBuffers(handle: *mut c_void) -> i32 {
    let Some(f) = file_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    i32::from(f.file.sync_all().is_ok())
}

// Attributes and directories.

pub(crate) fn attributes_of(m: &std::fs::Metadata) -> u32 {
    let mut a = if m.is_dir() {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    if m.permissions().readonly() {
        a |= FILE_ATTRIBUTE_READONLY;
        a &= !FILE_ATTRIBUTE_NORMAL;
    }
    a
}

pub(crate) fn filetime(t: io::Result<SystemTime>) -> u64 {
    let since = t
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .unwrap_or_default();
    (since.as_secs() + EPOCH_DIFFERENCE) * 10_000_000 + u64::from(since.subsec_nanos()) / 100
}

fn attributes(path: Option<PathBuf>) -> u32 {
    let Some(path) = path else {
        set_error(ERROR_PATH_NOT_FOUND);
        return INVALID_FILE_ATTRIBUTES;
    };
    match std::fs::metadata(&path) {
        Ok(m) => attributes_of(&m),
        Err(e) => {
            set_error(win32_error(&e, &path));
            INVALID_FILE_ATTRIBUTES
        }
    }
}

pub extern "system" fn GetFileAttributesW(path: *const u16) -> u32 {
    attributes(wide_path(path))
}

pub extern "system" fn GetFileAttributesA(path: *const c_char) -> u32 {
    attributes(narrow_path(path))
}

/// `WIN32_FILE_ATTRIBUTE_DATA`: attributes, three times, the size.
pub extern "system" fn GetFileAttributesExW(path: *const u16, _level: u32, data: *mut u8) -> i32 {
    let Some(path) = wide_path(path) else {
        set_error(ERROR_PATH_NOT_FOUND);
        return 0;
    };
    match std::fs::metadata(&path) {
        Ok(m) => {
            // SAFETY: the title's 36-byte structure.
            unsafe {
                data.cast::<u32>().write_unaligned(attributes_of(&m));
                data.add(4)
                    .cast::<u64>()
                    .write_unaligned(filetime(m.created()));
                data.add(12)
                    .cast::<u64>()
                    .write_unaligned(filetime(m.accessed()));
                data.add(20)
                    .cast::<u64>()
                    .write_unaligned(filetime(m.modified()));
                data.add(28)
                    .cast::<u32>()
                    .write_unaligned((m.len() >> 32) as u32);
                data.add(32).cast::<u32>().write_unaligned(m.len() as u32);
            }
            1
        }
        Err(e) => {
            set_error(win32_error(&e, &path));
            0
        }
    }
}

pub extern "system" fn DeleteFileW(path: *const u16) -> i32 {
    let Some(path) = wide_path(path) else {
        set_error(ERROR_PATH_NOT_FOUND);
        return 0;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => 1,
        Err(e) => {
            set_error(win32_error(&e, &path));
            0
        }
    }
}

pub extern "system" fn CreateDirectoryW(path: *const u16, _security: *const c_void) -> i32 {
    let Some(path) = wide_path(path) else {
        set_error(ERROR_PATH_NOT_FOUND);
        return 0;
    };
    match std::fs::create_dir(&path) {
        Ok(()) => 1,
        Err(e) => {
            set_error(if e.kind() == io::ErrorKind::AlreadyExists {
                ERROR_ALREADY_EXISTS
            } else {
                win32_error(&e, &path)
            });
            0
        }
    }
}

pub extern "system" fn RemoveDirectoryW(path: *const u16) -> i32 {
    let Some(path) = wide_path(path) else {
        set_error(ERROR_PATH_NOT_FOUND);
        return 0;
    };
    match std::fs::remove_dir(&path) {
        Ok(()) => 1,
        Err(e) => {
            set_error(win32_error(&e, &path));
            0
        }
    }
}

// Directory searches.

/// What `FindFirstFile` found: its entries, and the next to give out.
pub(crate) struct Find {
    entries: Vec<(String, Option<std::fs::Metadata>)>,
    next: usize,
}

/// Whether `name` matches a `*` and `?` pattern, ignoring case.
pub(super) fn wildcard(pattern: &[char], name: &[char]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some(('*', rest)) => (0..=name.len()).any(|i| wildcard(rest, &name[i..])),
        Some((p, rest)) => name.split_first().is_some_and(|(n, tail)| {
            (*p == '?' || p.to_lowercase().eq(n.to_lowercase())) && wildcard(rest, tail)
        }),
    }
}

fn fill_find_data(entry: &(String, Option<std::fs::Metadata>), data: *mut u8) {
    let (name, meta) = entry;
    // SAFETY: the title's 592-byte WIN32_FIND_DATAW.
    unsafe {
        std::ptr::write_bytes(data, 0, 592);
        if let Some(m) = meta {
            data.cast::<u32>().write_unaligned(attributes_of(m));
            data.add(4)
                .cast::<u64>()
                .write_unaligned(filetime(m.created()));
            data.add(12)
                .cast::<u64>()
                .write_unaligned(filetime(m.accessed()));
            data.add(20)
                .cast::<u64>()
                .write_unaligned(filetime(m.modified()));
            data.add(28)
                .cast::<u32>()
                .write_unaligned((m.len() >> 32) as u32);
            data.add(32).cast::<u32>().write_unaligned(m.len() as u32);
        } else {
            data.cast::<u32>().write_unaligned(FILE_ATTRIBUTE_DIRECTORY);
        }
        let out = data.add(44).cast::<u16>();
        for (i, unit) in name.encode_utf16().enumerate().take(259) {
            out.add(i).write_unaligned(unit);
        }
    }
}

fn find_first(path: Option<PathBuf>, data: *mut u8) -> *mut c_void {
    let Some(path) = path else {
        set_error(ERROR_PATH_NOT_FOUND);
        return invalid_handle();
    };
    let (dir, pattern) = match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => (dir.to_owned(), name.to_string_lossy().into_owned()),
        _ => {
            set_error(ERROR_PATH_NOT_FOUND);
            return invalid_handle();
        }
    };
    let dir = if dir.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        dir
    };
    let pattern: Vec<char> = pattern.chars().collect();
    let read = match std::fs::read_dir(&dir) {
        Ok(read) => read,
        Err(e) => {
            set_error(win32_error(&e, &path));
            return invalid_handle();
        }
    };
    let mut entries: Vec<(String, Option<std::fs::Metadata>)> = vec![
        (".".to_owned(), std::fs::metadata(&dir).ok()),
        ("..".to_owned(), std::fs::metadata(dir.join("..")).ok()),
    ];
    entries.extend(read.flatten().map(|e| {
        (
            e.file_name().to_string_lossy().into_owned(),
            e.metadata().ok(),
        )
    }));
    entries.retain(|(name, _)| wildcard(&pattern, &name.chars().collect::<Vec<_>>()));
    if entries.is_empty() {
        set_error(ERROR_FILE_NOT_FOUND);
        return invalid_handle();
    }
    fill_find_data(&entries[0], data);
    new_handle(Object::Find(Arc::new(Mutex::new(Find {
        entries,
        next: 1,
    }))))
}

pub extern "system" fn FindFirstFileW(path: *const u16, data: *mut u8) -> *mut c_void {
    find_first(wide_path(path), data)
}

pub extern "system" fn FindFirstFileExW(
    path: *const u16,
    _level: u32,
    data: *mut u8,
    _operation: u32,
    _filter: *const c_void,
    _flags: u32,
) -> *mut c_void {
    find_first(wide_path(path), data)
}

pub extern "system" fn FindNextFileW(handle: *mut c_void, data: *mut u8) -> i32 {
    let find = match lock(&HANDLES).get(&(handle as usize)) {
        Some(Object::Find(f)) => Some(f.clone()),
        _ => None,
    };
    let Some(find) = find else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    let mut find = lock(&find);
    let Some(entry) = find.entries.get(find.next) else {
        set_error(ERROR_NO_MORE_FILES);
        return 0;
    };
    fill_find_data(entry, data);
    find.next += 1;
    1
}

pub extern "system" fn FindClose(handle: *mut c_void) -> i32 {
    let mut handles = lock(&HANDLES);
    if matches!(handles.get(&(handle as usize)), Some(Object::Find(_))) {
        handles.remove(&(handle as usize));
        1
    } else {
        set_error(ERROR_INVALID_HANDLE);
        0
    }
}

/// Names this module answers for.
pub(super) fn lookup(name: &str) -> Option<usize> {
    macro_rules! table {
        ($($f:ident),* $(,)?) => {
            match name {
                $(stringify!($f) => Some($f as *const () as usize),)*
                _ => None,
            }
        };
    }
    table!(
        CreateFileW,
        CreateFileA,
        ReadFile,
        WriteFile,
        GetOverlappedResult,
        SetFilePointer,
        SetFilePointerEx,
        GetFileSize,
        GetFileSizeEx,
        SetEndOfFile,
        FlushFileBuffers,
        GetFileAttributesW,
        GetFileAttributesA,
        GetFileAttributesExW,
        DeleteFileW,
        CreateDirectoryW,
        RemoveDirectoryW,
        FindFirstFileW,
        FindFirstFileExW,
        FindNextFileW,
        FindClose,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// File calls report what Windows would: a missing folder apart from a missing file, and
    /// running out of descriptors apart from a refusal (`ERROR_ACCESS_DENIED` is the rest).
    #[test]
    fn host_errors_reach_file_calls_as_windows_reports_them() {
        let missing = io::Error::from_raw_os_error(2);
        let here = std::env::temp_dir().join("victory-no-such-file");
        assert_eq!(win32_error(&missing, &here), ERROR_FILE_NOT_FOUND);
        let nowhere = Path::new("/victory-no-such-folder/file");
        assert_eq!(win32_error(&missing, nowhere), ERROR_PATH_NOT_FOUND);
        let code = |errno| win32_error(&io::Error::from_raw_os_error(errno), &here);
        assert_eq!(code(24), 4, "EMFILE: ERROR_TOO_MANY_OPEN_FILES");
        assert_eq!(code(66), 145, "ENOTEMPTY: ERROR_DIR_NOT_EMPTY, as before");
        assert_eq!(code(17), 80, "EEXIST: ERROR_FILE_EXISTS, as before");
        assert_eq!(code(5), ERROR_ACCESS_DENIED, "EIO: the rest, as before");
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn a_drive_path_goes_under_its_folder_and_dotdot_stops_at_the_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("save.dat"), [7u8; 100]).unwrap();
        let letter = mount(dir.path()).unwrap();
        assert_eq!(mount(dir.path()), Some(letter));
        let root = std::path::absolute(dir.path()).unwrap();
        let path = |s: &str| translate(&s.encode_utf16().collect::<Vec<_>>()).unwrap();
        assert_eq!(
            path(&format!("{letter}:\\a\\b.txt")),
            root.join("a").join("b.txt")
        );
        assert_eq!(path(&format!("{letter}:/../../x")), root.join("x"));
        assert_eq!(path(&format!("\\\\?\\{letter}:\\")), root);
        assert_eq!(path("/tmp\\x"), PathBuf::from("/tmp/x"));

        let mut attributes = [0u8; 36];
        let p = wide(&format!("{letter}:\\save.dat"));
        assert_ne!(
            GetFileAttributesExW(p.as_ptr(), 0, attributes.as_mut_ptr()),
            0
        );
        let size = u32::from_le_bytes(attributes[32..36].try_into().unwrap());
        assert_eq!(size, 100);
    }

    #[test]
    fn wildcards_match_like_find_first_file() {
        let m = |p: &str, n: &str| {
            wildcard(
                &p.chars().collect::<Vec<_>>(),
                &n.chars().collect::<Vec<_>>(),
            )
        };
        assert!(m("*", "anything"));
        assert!(m("*.TXT", "a.txt"));
        assert!(m("a?c", "abc"));
        assert!(!m("a?c", "ac"));
        assert!(!m("*.bin", "a.txt"));
    }

    #[test]
    fn creating_a_read_only_handle_does_not_grant_write_access() {
        for disposition in [OPEN_ALWAYS, CREATE_NEW, CREATE_ALWAYS] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("save.dat");
            let name = wide(path.to_str().unwrap());
            let null = std::ptr::null_mut();
            let h = CreateFileW(name.as_ptr(), GENERIC_READ, 0, null, disposition, 0, null);
            assert_ne!(h, invalid_handle());
            assert_eq!(super::super::GetLastError(), 0);
            let mut count = 99;
            assert_eq!(WriteFile(h, b"x".as_ptr(), 1, &mut count, null.cast()), 0);
            assert_eq!(super::super::GetLastError(), ERROR_ACCESS_DENIED);
            assert_eq!(count, 0);
            assert_eq!(SetFilePointerEx(h, 3, std::ptr::null_mut(), 0), 1);
            set_error(0);
            assert_eq!(SetEndOfFile(h), 0);
            assert_eq!(super::super::GetLastError(), ERROR_ACCESS_DENIED);
            assert_eq!(super::super::CloseHandle(h), 1);
            assert!(std::fs::read(path).unwrap().is_empty());
        }
    }

    #[test]
    fn open_always_respects_access_when_reopening_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("save.dat");
        let name = wide(path.to_str().unwrap());
        let null = std::ptr::null_mut();
        let open = |access| CreateFileW(name.as_ptr(), access, 0, null, OPEN_ALWAYS, 0, null);

        let h = open(GENERIC_READ | GENERIC_WRITE);
        assert_ne!(h, invalid_handle());
        let mut count = 0;
        assert_eq!(WriteFile(h, b"save".as_ptr(), 4, &mut count, null.cast()), 1);
        assert_eq!(count, 4);
        assert_eq!(super::super::CloseHandle(h), 1);

        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).unwrap();
        let h = open(GENERIC_READ);
        assert_ne!(h, invalid_handle());
        assert_eq!(super::super::GetLastError(), ERROR_ALREADY_EXISTS);
        let mut data = [0; 4];
        assert_eq!(ReadFile(h, data.as_mut_ptr(), 4, &mut count, null.cast()), 1);
        assert_eq!(count, 4);
        assert_eq!(&data, b"save");
        assert_eq!(WriteFile(h, b"!".as_ptr(), 1, &mut count, null.cast()), 0);
        assert_eq!(super::super::GetLastError(), ERROR_ACCESS_DENIED);
        assert_eq!(count, 0);
        assert_eq!(super::super::CloseHandle(h), 1);
        assert_eq!(std::fs::read(path).unwrap(), b"save");
    }

    #[test]
    fn truncate_existing_requires_generic_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("save.dat");
        std::fs::write(&path, b"save").unwrap();
        let name = wide(path.to_str().unwrap());
        let truncate = |access| {
            let null = std::ptr::null_mut();
            CreateFileW(name.as_ptr(), access, 0, null, TRUNCATE_EXISTING, 0, null)
        };

        for access in [GENERIC_READ, 0, FILE_WRITE_DATA, FILE_APPEND_DATA] {
            let handle = truncate(access);
            assert_eq!(std::fs::read(&path).unwrap(), b"save");
            assert_eq!(handle, invalid_handle());
            assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
        }

        for access in [GENERIC_WRITE, GENERIC_ALL] {
            std::fs::write(&path, b"save").unwrap();
            let handle = truncate(access);
            assert_ne!(handle, invalid_handle(), "access {access:#x}");
            assert_eq!(super::super::CloseHandle(handle), 1);
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        }
    }

    /// `FILE_APPEND_DATA` alone writes at the end of the file, whatever the file pointer says.
    #[test]
    fn append_only_access_writes_at_the_end() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("log.txt"), b"abc").unwrap();
        let letter = mount(dir.path()).unwrap();
        let path = wide(&format!("{letter}:\\log.txt"));
        let open = |access| {
            let null = std::ptr::null_mut();
            CreateFileW(path.as_ptr(), access, 0, null, OPEN_EXISTING, 0, null)
        };
        let h = open(FILE_APPEND_DATA);
        assert_ne!(h, invalid_handle());
        let mut count = 0u32;
        let null = std::ptr::null_mut();
        assert_eq!(SetEndOfFile(h), 0);
        assert_eq!(super::super::GetLastError(), ERROR_ACCESS_DENIED);
        assert_eq!(WriteFile(h, b"de".as_ptr(), 2, &mut count, null), 1);
        assert_eq!(WriteFile(h, b"f".as_ptr(), 1, &mut count, null), 1);
        assert_eq!(super::super::CloseHandle(h), 1);
        assert_eq!(std::fs::read(dir.path().join("log.txt")).unwrap(), b"abcdef");
        // Write access as well: at the file pointer, as before.
        let h = open(FILE_WRITE_DATA | FILE_APPEND_DATA);
        assert_eq!(WriteFile(h, b"X".as_ptr(), 1, &mut count, null), 1);
        assert_eq!(super::super::CloseHandle(h), 1);
        assert_eq!(std::fs::read(dir.path().join("log.txt")).unwrap(), b"Xbcdef");
    }

    #[test]
    fn concurrent_synchronous_writes_keep_both_records() {
        const RECORD_LEN: usize = 4096;
        const ROUNDS: usize = 64;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.bin");
        let handle = create(Some(path.clone()), GENERIC_WRITE, CREATE_ALWAYS, 0);
        assert_ne!(handle, invalid_handle());
        let handle_id = handle as usize;
        let start = std::sync::Barrier::new(2);
        let first = [b'A'; RECORD_LEN];
        let second = [b'B'; RECORD_LEN];

        let write_records = |record: &[u8; RECORD_LEN]| {
            let mut all_written = true;
            for _ in 0..ROUNDS {
                start.wait();
                let mut count = 0;
                let result = WriteFile(
                    handle_id as *mut c_void,
                    record.as_ptr(),
                    RECORD_LEN as u32,
                    &mut count,
                    std::ptr::null_mut(),
                );
                all_written &= result == 1 && count == RECORD_LEN as u32;
            }
            all_written
        };
        std::thread::scope(|scope| {
            let writer_a = scope.spawn(|| write_records(&first));
            let writer_b = scope.spawn(|| write_records(&second));
            assert!(writer_a.join().unwrap());
            assert!(writer_b.join().unwrap());
        });

        assert_eq!(super::super::CloseHandle(handle), 1);
        let data = std::fs::read(path).unwrap();
        assert_eq!(data.len(), ROUNDS * 2 * RECORD_LEN);
        for (round, pair) in data.chunks_exact(2 * RECORD_LEN).enumerate() {
            let (left, right) = pair.split_at(RECORD_LEN);
            assert!(
                (left == first && right == second) || (left == second && right == first),
                "round {round} lost or corrupted a record"
            );
        }
    }

    #[test]
    fn overlapped_reads_report_eof_only_when_a_nonempty_request_reads_nothing() {
        use super::super::{
            CloseHandle, CreateEventW, GetLastError, ResetEvent, WAIT_OBJECT_0, WaitForSingleObject,
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("read.bin");
        std::fs::write(&path, b"1234tail").unwrap();
        let handle = create(Some(path), GENERIC_READ, OPEN_EXISTING, 0);
        assert_ne!(handle, invalid_handle());
        let event = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
        assert!(!event.is_null());
        // OVERLAPPED: Internal, InternalHigh, Offset/OffsetHigh, hEvent.
        let mut ov = [0, 0, 4, event as usize];
        let ov_ptr = (&raw mut ov).cast();
        let mut buffer = [0; 16];
        let mut count = 99;
        assert_eq!(
            ReadFile(handle, buffer.as_mut_ptr(), 16, &mut count, ov_ptr),
            1
        );
        assert_eq!(count, 4);
        assert_eq!(&buffer[..4], b"tail");
        count = 99;
        assert_eq!(GetOverlappedResult(handle, ov_ptr, &mut count, 0), 1);
        assert_eq!(count, 4);

        for offset in [8, 9] {
            assert_eq!(ResetEvent(event), 1);
            ov[2] = offset;
            count = 99;
            assert_eq!(
                ReadFile(handle, buffer.as_mut_ptr(), 16, &mut count, ov_ptr),
                0,
                "offset {offset}"
            );
            assert_eq!(GetLastError(), 38);
            assert_eq!(count, 0);
            assert_eq!(&ov[..2], &[0xC000_0011, 0]);
            assert_eq!(WaitForSingleObject(event, 0), WAIT_OBJECT_0);

            set_error(0);
            count = 99;
            assert_eq!(GetOverlappedResult(handle, ov_ptr, &mut count, 0), 0);
            assert_eq!(GetLastError(), 38);
            assert_eq!(count, 0);
        }

        ov[2] = 8;
        count = 99;
        assert_eq!(
            ReadFile(handle, std::ptr::null_mut(), 0, &mut count, ov_ptr),
            1
        );
        assert_eq!(count, 0);
        assert_eq!(&ov[..2], &[0, 0]);
        assert_eq!(CloseHandle(event), 1);
        assert_eq!(CloseHandle(handle), 1);
    }

    #[test]
    fn synchronous_read_at_eof_succeeds_with_zero_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("read.bin");
        std::fs::write(&path, b"data").unwrap();
        let handle = create(Some(path), GENERIC_READ, OPEN_EXISTING, 0);
        assert_ne!(handle, invalid_handle());
        assert_eq!(SetFilePointer(handle, 0, std::ptr::null_mut(), 2), 4);
        let mut buffer = [0; 4];
        let mut count = 99;
        assert_eq!(
            ReadFile(
                handle,
                buffer.as_mut_ptr(),
                4,
                &mut count,
                std::ptr::null_mut()
            ),
            1
        );
        assert_eq!(count, 0);
        assert_eq!(super::super::CloseHandle(handle), 1);
    }

    #[test]
    fn zero_length_null_io_and_find_close_handle_checks() {
        let dir = tempfile::tempdir().unwrap();
        let letter = mount(dir.path()).unwrap();
        let path = wide(&format!("{letter}:\\io.bin"));
        let h = CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            CREATE_ALWAYS,
            0,
            std::ptr::null_mut(),
        );
        assert_ne!(h, invalid_handle());

        // Zero-length reads and writes with a null buffer succeed without UB.
        let mut count = 99u32;
        assert_eq!(WriteFile(h, std::ptr::null(), 0, &mut count, std::ptr::null_mut()), 1);
        assert_eq!(count, 0);
        count = 99;
        assert_eq!(ReadFile(h, std::ptr::null_mut(), 0, &mut count, std::ptr::null_mut()), 1);
        assert_eq!(count, 0);

        // Nonzero-length reads and writes with a null buffer fail with ERROR_INVALID_PARAMETER.
        assert_eq!(WriteFile(h, std::ptr::null(), 4, &mut count, std::ptr::null_mut()), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
        assert_eq!(ReadFile(h, std::ptr::null_mut(), 4, &mut count, std::ptr::null_mut()), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);

        // FindClose rejects non-Find handles without closing them.
        assert_eq!(FindClose(h), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_HANDLE);
        let payload = b"ok";
        assert_eq!(WriteFile(h, payload.as_ptr(), 2, &mut count, std::ptr::null_mut()), 1);
        assert_eq!(count, 2);
        assert_eq!(super::super::CloseHandle(h), 1);

        // FindClose on an invalid handle sets ERROR_INVALID_HANDLE.
        assert_eq!(FindClose(invalid_handle()), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_HANDLE);
    }
}

/// `VICTORY_TRACE_FILES=1`: reads per file (first and every power of two).
fn trace_read(file: &File, len: u32) {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    static SEEN: Mutex<std::collections::BTreeMap<String, (u64, u64)>> =
        Mutex::new(std::collections::BTreeMap::new());
    let mut buf = [0u8; 1024];
    // SAFETY: F_GETPATH (50) fills a PATH_MAX buffer.
    if unsafe { fcntl(file.as_raw_fd(), 50, buf.as_mut_ptr()) } < 0 {
        return;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
    let path = String::from_utf8_lossy(&buf[..end]).into_owned();
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let e = seen.entry(path.clone()).or_default();
    e.0 += 1;
    e.1 += u64::from(len);
    if e.0.is_power_of_two() {
        tracing::info!(path, reads = e.0, bytes = e.1, "ReadFile of a file so far");
    }
}
