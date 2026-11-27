//! `api-ms-win-crt-stdio`: `FILE*` and file descriptors over our own
//! stream table, on `host_posix::files` paths (so `G:\` and `T:\` work),
//! and the `__stdio_common_v*` entry points every `printf` and `scanf` of a
//! title's headers lands in.
//!
//! A `FILE*` is the address of a 16-byte block this module allocates and
//! never lets the title dereference; the stream behind it is shared with the
//! descriptor `_fileno` returns. Streams are not buffered for writing
//! (`setvbuf` is accepted and ignored); reads are buffered 4 KiB. Text mode
//! strips `\r` before `\n` on reads and does not add it on writes.

use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use super::format::{self, Input, VaList};
use super::runtime::bad_param;
use super::{
    EACCES, EBADF, EEXIST, EINVAL, EMFILE, ENOENT, ENOSPC, ERANGE, TRUNCATE, errno, exports, fail,
    set_errno,
};
use crate::host_posix::files;

const EOF: i32 = -1;
const WEOF: u32 = 0xFFFF;
const SEEK_SET: i32 = 0;
const SEEK_CUR: i32 = 1;
const SEEK_END: i32 = 2;

/// `errno = e` and the `-1` of a failed call.
fn err(e: i32) -> i32 {
    set_errno(e);
    -1
}

/// An invalid parameter and the `-1` of a failed call.
fn bad(e: i32) -> i32 {
    bad_param(e);
    -1
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

enum Io {
    File(File),
    Stdin,
    Stdout,
    Stderr,
}

pub(super) struct Stream {
    io: Io,
    /// Bytes read from the OS and not yet consumed: `rbuf[rpos..]`.
    rbuf: Vec<u8>,
    rpos: usize,
    eof: bool,
    err: bool,
    text: bool,
    readable: bool,
    writable: bool,
    append: bool,
}

impl Stream {
    fn new(io: Io, text: bool, readable: bool, writable: bool, append: bool) -> Self {
        Stream {
            io,
            rbuf: Vec::new(),
            rpos: 0,
            eof: false,
            err: false,
            text,
            readable,
            writable,
            append,
        }
    }

    fn unconsumed(&self) -> usize {
        self.rbuf.len() - self.rpos
    }

    /// Hands the read-ahead back to the OS file, so its position is the stream's.
    fn drop_readahead(&mut self) {
        let back = self.unconsumed();
        if let Io::File(f) = &mut self.io
            && back > 0
        {
            let _ = f.seek(SeekFrom::Current(-(back as i64)));
        }
        self.rbuf.clear();
        self.rpos = 0;
    }

    fn fill(&mut self) -> bool {
        if self.rpos < self.rbuf.len() {
            return true;
        }
        self.rbuf.resize(4096, 0);
        self.rpos = 0;
        let n = match &mut self.io {
            Io::File(f) => f.read(&mut self.rbuf),
            Io::Stdin => io::stdin().read(&mut self.rbuf),
            _ => Ok(0),
        };
        match n {
            Ok(n) => {
                self.rbuf.truncate(n);
                if n == 0 {
                    self.eof = true;
                }
                n > 0
            }
            Err(_) => {
                self.rbuf.clear();
                self.err = true;
                false
            }
        }
    }

    /// One byte, with text mode's `\r\n` as `\n`.
    fn get(&mut self) -> Option<u8> {
        if !self.readable {
            self.err = true;
            set_errno(EBADF);
            return None;
        }
        if !self.fill() {
            return None;
        }
        let b = self.rbuf[self.rpos];
        self.rpos += 1;
        if self.text && b == b'\r' && self.fill() && self.rbuf[self.rpos] == b'\n' {
            self.rpos += 1;
            return Some(b'\n');
        }
        Some(b)
    }

    fn unget(&mut self, b: u8) {
        self.unget_bytes(&[b]);
    }

    fn unget_bytes(&mut self, bytes: &[u8]) {
        if self.rbuf[..self.rpos].ends_with(bytes) {
            self.rpos -= bytes.len();
        } else {
            self.rbuf
                .splice(self.rpos..self.rpos, bytes.iter().copied());
        }
        self.eof = false;
    }

    fn put(&mut self, data: &[u8]) -> bool {
        if !self.writable {
            self.err = true;
            set_errno(EBADF);
            return false;
        }
        self.drop_readahead();
        let r = match &mut self.io {
            Io::File(f) => {
                if self.append {
                    let _ = f.seek(SeekFrom::End(0));
                }
                f.write_all(data)
            }
            Io::Stdout => {
                let mut o = io::stdout().lock();
                o.write_all(data).and_then(|()| o.flush())
            }
            Io::Stderr => io::stderr().lock().write_all(data),
            Io::Stdin => Err(io::Error::from_raw_os_error(9)),
        };
        if let Err(e) = r {
            self.err = true;
            set_errno(win_errno(&e));
            return false;
        }
        true
    }

    fn seek(&mut self, to: SeekFrom) -> Option<u64> {
        self.drop_readahead();
        self.eof = false;
        match &mut self.io {
            // A position before the start is `InvalidInput`, so `EINVAL`.
            Io::File(f) => f.seek(to).map_err(|e| set_errno(win_errno(&e))).ok(),
            _ => None,
        }
    }

    fn tell(&mut self) -> Option<u64> {
        let back = self.unconsumed() as u64;
        match &mut self.io {
            Io::File(f) => f.stream_position().ok().map(|p| p.saturating_sub(back)),
            _ => None,
        }
    }
}

/// The CRT's `errno` for a host I/O error (the numbers differ from macOS's).
pub(super) fn win_errno(e: &io::Error) -> i32 {
    match e.kind() {
        io::ErrorKind::NotFound => ENOENT,
        io::ErrorKind::PermissionDenied => EACCES,
        io::ErrorKind::AlreadyExists => EEXIST,
        io::ErrorKind::InvalidInput => EINVAL,
        _ => match e.raw_os_error() {
            Some(20) => 20,
            Some(21) => 21,
            Some(24) => EMFILE,
            Some(28) => ENOSPC,
            Some(30) => 30,
            Some(63) => 38,
            Some(66) => 41,
            Some(78) => 40,
            Some(35) => 11,
            Some(32) => 32,
            Some(29) => 29,
            _ => EACCES,
        },
    }
}

type Shared = Arc<Mutex<Stream>>;

struct State {
    /// `FILE*` to stream.
    files: HashMap<usize, Shared>,
    /// Descriptor to stream (0..=2 are the standard streams).
    fds: Vec<Option<Shared>>,
    std_files: [usize; 3],
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    let std = [
        Arc::new(Mutex::new(Stream::new(
            Io::Stdin,
            false,
            true,
            false,
            false,
        ))),
        Arc::new(Mutex::new(Stream::new(
            Io::Stdout,
            false,
            false,
            true,
            false,
        ))),
        Arc::new(Mutex::new(Stream::new(
            Io::Stderr,
            false,
            false,
            true,
            false,
        ))),
    ];
    let mut files = HashMap::new();
    let mut std_files = [0; 3];
    for (i, s) in std.iter().enumerate() {
        let block = Box::leak(Box::new([0u64; 2])).as_mut_ptr() as usize;
        files.insert(block, s.clone());
        std_files[i] = block;
    }
    Mutex::new(State {
        files,
        fds: std.into_iter().map(Some).collect(),
        std_files,
    })
});

fn state() -> MutexGuard<'static, State> {
    lock(&STATE)
}

fn stream_of(file: *const c_void) -> Option<Shared> {
    state().files.get(&(file as usize)).cloned()
}

fn fd_of(fd: i32) -> Option<Shared> {
    let fd = usize::try_from(fd).ok()?;
    state().fds.get(fd).cloned().flatten()
}

fn alloc_fd(s: &Shared) -> Option<i32> {
    let mut st = state();
    let slot = st
        .fds
        .iter()
        .skip(3)
        .position(Option::is_none)
        .map(|i| i + 3);
    let fd = match slot {
        Some(i) => {
            st.fds[i] = Some(s.clone());
            i
        }
        None if st.fds.len() < 8192 => {
            st.fds.push(Some(s.clone()));
            st.fds.len() - 1
        }
        None => return None,
    };
    Some(fd as i32)
}

fn new_file(s: Shared) -> *mut c_void {
    let block = Box::leak(Box::new([0u64; 2])).as_mut_ptr() as usize;
    state().files.insert(block, s);
    block as *mut c_void
}

/// Flushes every stream (writes are not buffered, so this is `fsync`-free).
pub(super) fn flush_all() {
    let all: Vec<Shared> = state().files.values().cloned().collect();
    for s in all {
        if let Io::Stdout = lock(&s).io {
            let _ = io::stdout().flush();
        }
    }
}

/// Parsed `fopen` mode.
struct Mode {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    exclusive: bool,
    text: bool,
}

fn parse_mode(mode: &[u8]) -> Option<Mode> {
    let mut m = Mode {
        read: false,
        write: false,
        append: false,
        truncate: false,
        create: false,
        exclusive: false,
        text: true,
    };
    let mut it = mode.iter().copied().skip_while(|c| *c == b' ');
    match it.next()? {
        b'r' => m.read = true,
        b'w' => {
            m.write = true;
            m.truncate = true;
            m.create = true;
        }
        b'a' => {
            m.write = true;
            m.append = true;
            m.create = true;
        }
        _ => return None,
    }
    for c in it {
        match c {
            b'+' => {
                m.read = true;
                m.write = true;
            }
            b'b' => m.text = false,
            b't' => m.text = true,
            b'x' => m.exclusive = true,
            b',' => break,
            _ => {}
        }
    }
    Some(m)
}

fn open_path(path: Option<PathBuf>, mode: &[u8]) -> *mut c_void {
    let (Some(path), Some(m)) = (path, parse_mode(mode)) else {
        set_errno(EINVAL);
        return std::ptr::null_mut();
    };
    let Some(s) = open_stream(&path, &m) else {
        return std::ptr::null_mut();
    };
    new_file(s)
}

fn open_stream(path: &PathBuf, m: &Mode) -> Option<Shared> {
    let mut o = OpenOptions::new();
    o.read(m.read || !m.write)
        .write(m.write && !m.append)
        .append(m.append);
    if m.create {
        if m.exclusive {
            o.create_new(true);
        } else {
            o.create(true);
        }
    }
    if m.truncate {
        o.truncate(true);
    }
    if path.is_dir() {
        set_errno(EACCES);
        return None;
    }
    match o.open(path) {
        Ok(f) => {
            let s = Arc::new(Mutex::new(Stream::new(
                Io::File(f),
                m.text,
                m.read,
                m.write,
                m.append,
            )));
            if alloc_fd(&s).is_none() {
                set_errno(EMFILE);
                return None;
            }
            Some(s)
        }
        Err(e) => {
            set_errno(win_errno(&e));
            None
        }
    }
}

pub extern "system" fn fopen(path: *const u8, mode: *const u8) -> *mut c_void {
    if path.is_null() || mode.is_null() {
        bad_param(EINVAL);
        return std::ptr::null_mut();
    }
    // SAFETY: NUL-terminated strings.
    open_path(files::narrow_path(path.cast()), unsafe {
        super::bytes(mode)
    })
}

pub extern "system" fn _wfopen(path: *const u16, mode: *const u16) -> *mut c_void {
    if path.is_null() || mode.is_null() {
        bad_param(EINVAL);
        return std::ptr::null_mut();
    }
    // SAFETY: NUL-terminated strings.
    let m: Vec<u8> = unsafe { super::wide(mode) }
        .iter()
        .map(|&c| c as u8)
        .collect();
    open_path(files::wide_path(path), &m)
}

pub extern "system" fn fopen_s(out: *mut *mut c_void, path: *const u8, mode: *const u8) -> i32 {
    if out.is_null() {
        return bad_param(EINVAL);
    }
    let f = fopen(path, mode);
    // SAFETY: the title's out pointer.
    unsafe { out.write(f) };
    if f.is_null() { errno() } else { 0 }
}

pub extern "system" fn _wfopen_s(out: *mut *mut c_void, path: *const u16, mode: *const u16) -> i32 {
    if out.is_null() {
        return bad_param(EINVAL);
    }
    let f = _wfopen(path, mode);
    // SAFETY: the title's out pointer.
    unsafe { out.write(f) };
    if f.is_null() { errno() } else { 0 }
}

pub extern "system" fn _fdopen(fd: i32, mode: *const u8) -> *mut c_void {
    if mode.is_null() {
        bad_param(EINVAL);
        return std::ptr::null_mut();
    }
    // SAFETY: checked non-null, NUL-terminated mode string.
    if parse_mode(unsafe { super::bytes(mode) }).is_none() {
        bad_param(EINVAL);
        return std::ptr::null_mut();
    }
    let Some(s) = fd_of(fd) else {
        set_errno(EBADF);
        return std::ptr::null_mut();
    };
    new_file(s)
}

pub extern "system" fn fclose(file: *mut c_void) -> i32 {
    let mut st = state();
    let Some(s) = st.files.remove(&(file as usize)) else {
        drop(st);
        return err(EINVAL);
    };
    // Descriptors of this stream go with it, unless it is a standard one.
    if !st.std_files.contains(&(file as usize)) {
        for slot in st.fds.iter_mut().skip(3) {
            if slot.as_ref().is_some_and(|f| Arc::ptr_eq(f, &s)) {
                *slot = None;
            }
        }
    }
    // SAFETY: the block this module allocated for `fopen`.
    if !st.std_files.contains(&(file as usize)) {
        // SAFETY: the block `new_file` leaked.
        drop(unsafe { Box::from_raw(file.cast::<[u64; 2]>()) });
    }
    0
}

pub extern "system" fn _fcloseall() -> i32 {
    let all: Vec<usize> = {
        let st = state();
        st.files
            .keys()
            .copied()
            .filter(|k| !st.std_files.contains(k))
            .collect()
    };
    for f in &all {
        fclose(*f as *mut c_void);
    }
    all.len() as i32
}

pub extern "system" fn fflush(file: *mut c_void) -> i32 {
    if file.is_null() {
        flush_all();
        return 0;
    }
    match stream_of(file) {
        Some(s) => {
            if let Io::Stdout = lock(&s).io {
                let _ = io::stdout().flush();
            }
            0
        }
        None => err(EINVAL),
    }
}

pub extern "system" fn _flushall() -> i32 {
    flush_all();
    0
}

/// Runs `f` on the stream, or fails like a bad `FILE*`.
fn with<R>(file: *const c_void, bad: R, f: impl FnOnce(&mut Stream) -> R) -> R {
    match stream_of(file) {
        Some(s) => f(&mut lock(&s)),
        None => {
            bad_param(EINVAL);
            bad
        }
    }
}

pub extern "system" fn fread(buf: *mut u8, size: usize, count: usize, file: *mut c_void) -> usize {
    if size == 0 || count == 0 {
        return 0;
    }
    if buf.is_null() {
        bad_param(EINVAL);
        return 0;
    }
    let Some(total) = size.checked_mul(count) else {
        bad_param(EINVAL);
        return 0;
    };
    with(file, 0, |s| {
        if !s.readable {
            s.err = true;
            set_errno(EBADF);
            return 0;
        }
        let mut done = 0;
        while done < total {
            if !s.text && s.rpos == s.rbuf.len() && total - done >= 4096 {
                // Large binary reads skip the buffer.
                if let Io::File(f) = &mut s.io {
                    // SAFETY: the title's buffer of `total` bytes.
                    let dst =
                        unsafe { std::slice::from_raw_parts_mut(buf.add(done), total - done) };
                    match f.read(dst) {
                        Ok(0) => {
                            s.eof = true;
                            break;
                        }
                        Ok(n) => {
                            done += n;
                            continue;
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            s.err = true;
                            set_errno(win_errno(&e));
                            break;
                        }
                    }
                }
            }
            match s.get() {
                // SAFETY: inside the title's buffer.
                Some(b) => unsafe { buf.add(done).write(b) },
                None => break,
            }
            done += 1;
        }
        done / size
    })
}

pub extern "system" fn fwrite(
    buf: *const u8,
    size: usize,
    count: usize,
    file: *mut c_void,
) -> usize {
    if size == 0 || count == 0 {
        return 0;
    }
    if buf.is_null() {
        bad_param(EINVAL);
        return 0;
    }
    let Some(total) = size.checked_mul(count) else {
        bad_param(EINVAL);
        return 0;
    };
    // SAFETY: the title's buffer.
    let data = unsafe { std::slice::from_raw_parts(buf, total) };
    with(file, 0, |s| if s.put(data) { count } else { 0 })
}

pub extern "system" fn fgetc(file: *mut c_void) -> i32 {
    with(file, EOF, |s| s.get().map_or(EOF, i32::from))
}

pub extern "system" fn fputc(c: i32, file: *mut c_void) -> i32 {
    with(file, EOF, |s| {
        if s.put(&[c as u8]) {
            i32::from(c as u8)
        } else {
            EOF
        }
    })
}

pub extern "system" fn ungetc(c: i32, file: *mut c_void) -> i32 {
    if c == EOF {
        return EOF;
    }
    with(file, EOF, |s| {
        if !s.readable {
            return EOF;
        }
        s.unget(c as u8);
        i32::from(c as u8)
    })
}

pub extern "system" fn fgets(buf: *mut u8, n: i32, file: *mut c_void) -> *mut u8 {
    if buf.is_null() || n <= 0 {
        bad_param(EINVAL);
        return std::ptr::null_mut();
    }
    with(file, std::ptr::null_mut(), |s| {
        let (mut i, mut ended) = (0, false);
        while i + 1 < n as usize {
            let Some(b) = s.get() else {
                ended = true;
                break;
            };
            // SAFETY: inside the title's buffer of `n`.
            unsafe { buf.add(i).write(b) };
            i += 1;
            if b == b'\n' {
                break;
            }
        }
        // NULL only for an end of file (or error) before any character: with n = 1 nothing
        // is read and the buffer gets "".
        if i == 0 && ended {
            return std::ptr::null_mut();
        }
        // SAFETY: `i < n`.
        unsafe { buf.add(i).write(0) };
        buf
    })
}

pub extern "system" fn fputs(text: *const u8, file: *mut c_void) -> i32 {
    if text.is_null() {
        return bad(EINVAL);
    }
    // SAFETY: a NUL-terminated string.
    let b = unsafe { super::bytes(text) };
    with(file, EOF, |s| if s.put(b) { 0 } else { EOF })
}

fn std_file(i: usize) -> *mut c_void {
    state().std_files[i] as *mut c_void
}

pub extern "system" fn puts(text: *const u8) -> i32 {
    if text.is_null() {
        return bad(EINVAL);
    }
    // SAFETY: a NUL-terminated string.
    let mut b = unsafe { super::bytes(text) }.to_vec();
    b.push(b'\n');
    with(std_file(1), EOF, |s| if s.put(&b) { 0 } else { EOF })
}

pub extern "system" fn getchar() -> i32 {
    fgetc(std_file(0))
}

pub extern "system" fn putchar(c: i32) -> i32 {
    fputc(c, std_file(1))
}

/// UTF-16 unit as UTF-8 onto the stream (a lone half of a pair is `?`).
pub extern "system" fn fputwc(c: u16, file: *mut c_void) -> u32 {
    let ch = char::from_u32(u32::from(c)).unwrap_or('?');
    let mut b = [0u8; 4];
    let s = ch.encode_utf8(&mut b);
    with(file, WEOF, |st| {
        if st.put(s.as_bytes()) {
            u32::from(c)
        } else {
            WEOF
        }
    })
}

pub extern "system" fn ungetwc(c: u16, file: *mut c_void) -> u16 {
    with(file, WEOF as u16, |s| {
        if !s.readable || u32::from(c) == WEOF {
            return WEOF as u16;
        }
        let Some(ch) = char::from_u32(u32::from(c)) else {
            return WEOF as u16;
        };
        s.unget_bytes(ch.encode_utf8(&mut [0; 4]).as_bytes());
        c
    })
}

pub extern "system" fn fgetwc(file: *mut c_void) -> u32 {
    with(file, WEOF, |s| {
        let Some(first) = s.get() else { return WEOF };
        let extra = match first {
            0xC0..=0xDF => 1,
            0xE0..=0xEF => 2,
            0xF0..=0xF7 => 3,
            _ => 0,
        };
        let mut bytes = vec![first];
        for _ in 0..extra {
            match s.get() {
                Some(b) => bytes.push(b),
                None => break,
            }
        }
        std::str::from_utf8(&bytes)
            .ok()
            .and_then(|t| t.chars().next())
            .map_or(0xFFFD, |c| {
                let mut u = [0u16; 2];
                u32::from(c.encode_utf16(&mut u)[0])
            })
    })
}

pub extern "system" fn fputws(text: *const u16, file: *mut c_void) -> i32 {
    if text.is_null() {
        return bad(EINVAL);
    }
    // SAFETY: a NUL-terminated string.
    let b = String::from_utf16_lossy(unsafe { super::wide(text) });
    with(file, EOF, |s| if s.put(b.as_bytes()) { 0 } else { EOF })
}

pub extern "system" fn feof(file: *mut c_void) -> i32 {
    with(file, 0, |s| i32::from(s.eof))
}

pub extern "system" fn ferror(file: *mut c_void) -> i32 {
    with(file, 0, |s| i32::from(s.err))
}

pub extern "system" fn clearerr(file: *mut c_void) {
    with(file, (), |s| {
        s.eof = false;
        s.err = false;
    });
}

fn seek_from(offset: i64, origin: i32) -> Option<SeekFrom> {
    Some(match origin {
        SEEK_SET => SeekFrom::Start(u64::try_from(offset).ok()?),
        SEEK_CUR => SeekFrom::Current(offset),
        SEEK_END => SeekFrom::End(offset),
        _ => return None,
    })
}

pub extern "system" fn _fseeki64(file: *mut c_void, offset: i64, origin: i32) -> i32 {
    let Some(to) = seek_from(offset, origin) else {
        return err(EINVAL);
    };
    with(file, -1, |s| if s.seek(to).is_some() { 0 } else { -1 })
}

pub extern "system" fn fseek(file: *mut c_void, offset: i32, origin: i32) -> i32 {
    _fseeki64(file, i64::from(offset), origin)
}

pub extern "system" fn _ftelli64(file: *mut c_void) -> i64 {
    with(file, -1, |s| s.tell().map_or(-1, |p| p as i64))
}

pub extern "system" fn ftell(file: *mut c_void) -> i32 {
    i32::try_from(_ftelli64(file)).unwrap_or(-1)
}

pub extern "system" fn rewind(file: *mut c_void) {
    _fseeki64(file, 0, SEEK_SET);
    clearerr(file);
}

pub extern "system" fn fgetpos(file: *mut c_void, pos: *mut i64) -> i32 {
    if pos.is_null() {
        return bad(EINVAL);
    }
    let p = _ftelli64(file);
    if p < 0 {
        return -1;
    }
    // SAFETY: the title's pointer.
    unsafe { pos.write(p) };
    0
}

pub extern "system" fn fsetpos(file: *mut c_void, pos: *const i64) -> i32 {
    if pos.is_null() {
        return bad(EINVAL);
    }
    // SAFETY: the title's pointer.
    _fseeki64(file, unsafe { pos.read() }, SEEK_SET)
}

pub extern "system" fn setvbuf(file: *mut c_void, _buf: *mut u8, _mode: i32, _size: usize) -> i32 {
    with(file, -1, |_| 0)
}

pub extern "system" fn setbuf(_file: *mut c_void, _buf: *mut u8) {}

pub extern "system" fn _fileno(file: *mut c_void) -> i32 {
    let Some(s) = stream_of(file) else {
        bad_param(EINVAL);
        return -1;
    };
    let st = state();
    st.fds
        .iter()
        .position(|f| f.as_ref().is_some_and(|f| Arc::ptr_eq(f, &s)))
        .map_or(-1, |i| i as i32)
}

pub extern "system" fn __acrt_iob_func(i: u32) -> *mut c_void {
    if i > 2 {
        return std::ptr::null_mut();
    }
    std_file(i as usize)
}

pub extern "system" fn _lock_file(_file: *mut c_void) {}

pub extern "system" fn _unlock_file(_file: *mut c_void) {}

/// The CRT's buffer internals, which nothing here has.
pub extern "system" fn _get_stream_buffer_pointers(
    _file: *mut c_void,
    base: *mut usize,
    ptr: *mut usize,
    count: *mut i32,
) -> i32 {
    // SAFETY: the title's out pointers, each possibly null.
    unsafe {
        for p in [base, ptr] {
            if !p.is_null() {
                p.write(0);
            }
        }
        if !count.is_null() {
            count.write(0);
        }
    }
    0
}

pub extern "system" fn tmpfile() -> *mut c_void {
    let path =
        std::env::temp_dir().join(format!("victory-tmp-{}-{}", std::process::id(), next_tmp()));
    let f = open_path(Some(path.clone()), b"w+b");
    // Gone when the process is; the open handle keeps it usable.
    let _ = std::fs::remove_file(path);
    f
}

fn next_tmp() -> u64 {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

// Descriptors.

const O_WRONLY: i32 = 1;
const O_RDWR: i32 = 2;
const O_APPEND: i32 = 8;
const O_CREAT: i32 = 0x100;
const O_TRUNC: i32 = 0x200;
const O_EXCL: i32 = 0x400;
const O_BINARY: i32 = 0x8000;

fn open_fd(path: Option<PathBuf>, oflag: i32) -> i32 {
    let Some(path) = path else {
        set_errno(ENOENT);
        return -1;
    };
    let access = oflag & 3;
    let m = Mode {
        read: access != O_WRONLY,
        write: access == O_WRONLY || access == O_RDWR,
        append: oflag & O_APPEND != 0,
        truncate: oflag & O_TRUNC != 0,
        create: oflag & O_CREAT != 0,
        exclusive: oflag & O_EXCL != 0,
        text: oflag & O_BINARY == 0,
    };
    let Some(s) = open_stream(&path, &m) else {
        return -1;
    };
    // `open_stream` made the descriptor.
    state()
        .fds
        .iter()
        .position(|f| f.as_ref().is_some_and(|f| Arc::ptr_eq(f, &s)))
        .map_or(-1, |i| i as i32)
}

pub extern "system" fn _open(path: *const u8, oflag: i32, _pmode: i32) -> i32 {
    if path.is_null() {
        return bad(EINVAL);
    }
    open_fd(files::narrow_path(path.cast()), oflag)
}

pub extern "system" fn _wopen(path: *const u16, oflag: i32, _pmode: i32) -> i32 {
    if path.is_null() {
        return bad(EINVAL);
    }
    open_fd(files::wide_path(path), oflag)
}

pub extern "system" fn _sopen_s(
    fd: *mut i32,
    path: *const u8,
    oflag: i32,
    _share: i32,
    pmode: i32,
) -> i32 {
    if fd.is_null() {
        return bad_param(EINVAL);
    }
    let r = _open(path, oflag, pmode);
    // SAFETY: the title's out pointer.
    unsafe { fd.write(r) };
    if r < 0 { errno() } else { 0 }
}

pub extern "system" fn _wsopen_s(
    fd: *mut i32,
    path: *const u16,
    oflag: i32,
    _share: i32,
    pmode: i32,
) -> i32 {
    if fd.is_null() {
        return bad_param(EINVAL);
    }
    let r = _wopen(path, oflag, pmode);
    // SAFETY: the title's out pointer.
    unsafe { fd.write(r) };
    if r < 0 { errno() } else { 0 }
}

pub extern "system" fn _close(fd: i32) -> i32 {
    let Ok(i) = usize::try_from(fd) else {
        return err(EBADF);
    };
    let mut st = state();
    if i < 3 || i >= st.fds.len() || st.fds[i].is_none() {
        drop(st);
        return err(EBADF);
    }
    st.fds[i] = None;
    0
}

pub extern "system" fn _read(fd: i32, buf: *mut u8, count: u32) -> i32 {
    let Some(s) = fd_of(fd) else {
        return err(EBADF);
    };
    let mut s = lock(&s);
    if !s.readable {
        // Not open for reading: -1, not the 0 of an end of file.
        return err(EBADF);
    }
    if count == 0 {
        return 0;
    }
    if buf.is_null() {
        return bad(EINVAL);
    }
    let mut n = 0;
    while n < count as usize {
        match s.get() {
            // SAFETY: inside the title's buffer.
            Some(b) => unsafe { buf.add(n).write(b) },
            None => break,
        }
        n += 1;
    }
    n as i32
}

pub extern "system" fn _write(fd: i32, buf: *const u8, count: u32) -> i32 {
    let Some(s) = fd_of(fd) else {
        return err(EBADF);
    };
    if count == 0 {
        return if lock(&s).writable { 0 } else { err(EBADF) };
    }
    if buf.is_null() {
        return bad(EINVAL);
    }
    // SAFETY: the title's buffer.
    let data = unsafe { std::slice::from_raw_parts(buf, count as usize) };
    if lock(&s).put(data) { count as i32 } else { -1 }
}

pub extern "system" fn _lseeki64(fd: i32, offset: i64, origin: i32) -> i64 {
    let Some(s) = fd_of(fd) else {
        return i64::from(err(EBADF));
    };
    let Some(to) = seek_from(offset, origin) else {
        return i64::from(err(EINVAL));
    };
    let mut s = lock(&s);
    s.seek(to).map_or(-1, |p| p as i64)
}

pub extern "system" fn _lseek(fd: i32, offset: i32, origin: i32) -> i32 {
    i32::try_from(_lseeki64(fd, i64::from(offset), origin)).unwrap_or(-1)
}

pub extern "system" fn _telli64(fd: i32) -> i64 {
    let Some(s) = fd_of(fd) else {
        return i64::from(err(EBADF));
    };
    lock(&s).tell().map_or(-1, |p| p as i64)
}

pub extern "system" fn _tell(fd: i32) -> i32 {
    i32::try_from(_telli64(fd)).unwrap_or(-1)
}

pub extern "system" fn _filelengthi64(fd: i32) -> i64 {
    let Some(s) = fd_of(fd) else {
        return i64::from(err(EBADF));
    };
    match &lock(&s).io {
        Io::File(f) => f.metadata().map_or(-1, |m| m.len() as i64),
        _ => -1,
    }
}

pub extern "system" fn _filelength(fd: i32) -> i32 {
    i32::try_from(_filelengthi64(fd)).unwrap_or(-1)
}

pub extern "system" fn _eof(fd: i32) -> i32 {
    let len = _filelengthi64(fd);
    let pos = _telli64(fd);
    if len < 0 || pos < 0 {
        -1
    } else {
        i32::from(pos >= len)
    }
}

pub extern "system" fn _isatty(fd: i32) -> i32 {
    i32::from(fd_of(fd).is_some_and(|s| !matches!(lock(&s).io, Io::File(_))))
}

pub extern "system" fn _setmode(fd: i32, mode: i32) -> i32 {
    let Some(s) = fd_of(fd) else {
        return err(EBADF);
    };
    let mut s = lock(&s);
    let old = if s.text { 0x4000 } else { O_BINARY };
    s.text = mode & O_BINARY == 0;
    old
}

pub extern "system" fn _commit(fd: i32) -> i32 {
    match fd_of(fd) {
        Some(s) => {
            if let Io::File(f) = &lock(&s).io {
                let _ = f.sync_all();
            }
            0
        }
        None => err(EBADF),
    }
}

pub extern "system" fn _chsize_s(fd: i32, size: i64) -> i32 {
    let Some(s) = fd_of(fd) else {
        return fail(EBADF);
    };
    if size < 0 {
        return bad_param(EINVAL);
    }
    let mut s = lock(&s);
    if !s.writable {
        return fail(EACCES);
    }
    s.drop_readahead();
    match &s.io {
        Io::File(f) => match f.set_len(size as u64) {
            Ok(()) => 0,
            Err(e) => fail(win_errno(&e)),
        },
        _ => fail(EBADF),
    }
}

pub extern "system" fn _dup(fd: i32) -> i32 {
    match fd_of(fd) {
        Some(s) => alloc_fd(&s).unwrap_or_else(|| err(EMFILE)),
        None => err(EBADF),
    }
}

/// `_fstat64` and `_fstat64i32`.
pub(super) fn fstat(fd: i32, out: *mut u8, wide_size: bool) -> i32 {
    if out.is_null() {
        return bad(EINVAL);
    }
    let Some(s) = fd_of(fd) else {
        return err(EBADF);
    };
    match &lock(&s).io {
        Io::File(f) => match f.metadata() {
            // SAFETY: the title's `_stat` structure.
            Ok(m) => unsafe {
                super::fs::stat_from_metadata(out, &m, wide_size);
                0
            },
            Err(e) => err(win_errno(&e)),
        },
        // A console: a character device (`_S_IFCHR`).
        _ => {
            // SAFETY: the title's `_stat` structure.
            unsafe {
                std::ptr::write_bytes(out, 0, if wide_size { 56 } else { 48 });
                out.add(6).cast::<u16>().write_unaligned(0x2000);
            }
            0
        }
    }
}

pub extern "system" fn _dup2(fd: i32, to: i32) -> i32 {
    let (Some(s), Ok(to_i)) = (fd_of(fd), usize::try_from(to)) else {
        return err(EBADF);
    };
    let mut st = state();
    if !(3..8192).contains(&to_i) {
        drop(st);
        return err(EBADF);
    }
    if st.fds.len() <= to_i {
        st.fds.resize(to_i + 1, None);
    }
    st.fds[to_i] = Some(s);
    0
}

// printf and scanf.

/// Delivers formatted output to a buffer by `__stdio_common_vsprintf`'s rules.
unsafe fn deliver<C: super::Ch>(out: &[u32], buf: *mut C, count: usize, options: u64) -> i32 {
    let len = out.len();
    let standard = options & format::PRINTF_STANDARD_SNPRINTF_BEHAVIOR != 0;
    // No buffer and no room asks for the length (the sizing idiom), whatever the options.
    if buf.is_null() {
        return if count == 0 { len as i32 } else { err(EINVAL) };
    }
    if count == 0 {
        return if standard { len as i32 } else { -1 };
    }
    // SAFETY: the title's buffer of `count` units.
    unsafe {
        let put = |n: usize| {
            for (i, &u) in out.iter().take(n).enumerate() {
                buf.add(i).write(C::make(u));
            }
        };
        if standard {
            let n = len.min(count - 1);
            put(n);
            buf.add(n).write(C::default());
            return len as i32;
        }
        if len < count {
            put(len);
            buf.add(len).write(C::default());
            return len as i32;
        }
        // `_vsnprintf` and `_vsnwprintf`: what fits, terminated only when there is room, so a text
        // that fills the buffer exactly is returned whole.
        if options & format::PRINTF_LEGACY_VSPRINTF_NULL_TERMINATION != 0 {
            put(count);
            return if len == count { len as i32 } else { -1 };
        }
        // ISO `vswprintf` (`_vswprintf_c_l`, no option): truncated, always terminated, negative.
        put(count - 1);
        buf.add(count - 1).write(C::default());
        -1
    }
}

fn format_units(fmt: *const u8, wide: bool, options: u64, va: *const u64) -> Option<Vec<u32>> {
    if fmt.is_null() {
        bad_param(EINVAL);
        return None;
    }
    let f: Vec<u32> = if wide {
        // SAFETY: a NUL-terminated format.
        unsafe { super::wide(fmt.cast()) }
            .iter()
            .map(|&c| u32::from(c))
            .collect()
    } else {
        // SAFETY: a NUL-terminated format.
        unsafe { super::bytes(fmt) }
            .iter()
            .map(|&c| u32::from(c))
            .collect()
    };
    let r = format::printf(&f, wide, options, &mut VaList(va));
    if r.is_none() {
        bad_param(EINVAL);
    }
    r
}

pub extern "system" fn __stdio_common_vsprintf(
    options: u64,
    buf: *mut u8,
    count: usize,
    fmt: *const u8,
    _locale: usize,
    va: *const u64,
) -> i32 {
    let Some(out) = format_units(fmt, false, options, va) else {
        return -1;
    };
    // SAFETY: the title's buffer.
    unsafe { deliver(&out, buf, count, options) }
}

pub extern "system" fn __stdio_common_vswprintf(
    options: u64,
    buf: *mut u16,
    count: usize,
    fmt: *const u16,
    _locale: usize,
    va: *const u64,
) -> i32 {
    let Some(out) = format_units(fmt.cast(), true, options, va) else {
        return -1;
    };
    // SAFETY: the title's buffer.
    unsafe { deliver(&out, buf, count, options) }
}

/// The `_s` rule: no room for the text and a NUL empties the buffer.
unsafe fn deliver_s<C: super::Ch>(
    out: &[u32],
    buf: *mut C,
    bufcount: usize,
    maxcount: usize,
) -> i32 {
    if buf.is_null() || bufcount == 0 {
        return bad(EINVAL);
    }
    let len = out.len();
    let (n, result) = if maxcount == TRUNCATE {
        (
            len.min(bufcount - 1),
            if len > bufcount - 1 { -1 } else { len as i32 },
        )
    } else {
        (
            len.min(maxcount),
            if len > maxcount { -1 } else { len as i32 },
        )
    };
    if n >= bufcount {
        // SAFETY: the title's buffer.
        unsafe { buf.write(C::default()) };
        set_errno(ERANGE);
        bad_param(ERANGE);
        return -1;
    }
    // SAFETY: the title's buffer of `bufcount`.
    unsafe {
        for (i, &u) in out.iter().take(n).enumerate() {
            buf.add(i).write(C::make(u));
        }
        buf.add(n).write(C::default());
    }
    result
}

pub extern "system" fn __stdio_common_vsprintf_s(
    options: u64,
    buf: *mut u8,
    count: usize,
    fmt: *const u8,
    _locale: usize,
    va: *const u64,
) -> i32 {
    let Some(out) = format_units(fmt, false, options, va) else {
        return -1;
    };
    // SAFETY: the title's buffer.
    unsafe { deliver_s(&out, buf, count, usize::MAX - 1) }
}

pub extern "system" fn __stdio_common_vswprintf_s(
    options: u64,
    buf: *mut u16,
    count: usize,
    fmt: *const u16,
    _locale: usize,
    va: *const u64,
) -> i32 {
    let Some(out) = format_units(fmt.cast(), true, options, va) else {
        return -1;
    };
    // SAFETY: the title's buffer.
    unsafe { deliver_s(&out, buf, count, usize::MAX - 1) }
}

pub extern "system" fn __stdio_common_vsnprintf_s(
    options: u64,
    buf: *mut u8,
    count: usize,
    max: usize,
    fmt: *const u8,
    _locale: usize,
    va: *const u64,
) -> i32 {
    let Some(out) = format_units(fmt, false, options, va) else {
        return -1;
    };
    // SAFETY: the title's buffer.
    unsafe { deliver_s(&out, buf, count, max) }
}

pub extern "system" fn __stdio_common_vsnwprintf_s(
    options: u64,
    buf: *mut u16,
    count: usize,
    max: usize,
    fmt: *const u16,
    _locale: usize,
    va: *const u64,
) -> i32 {
    let Some(out) = format_units(fmt.cast(), true, options, va) else {
        return -1;
    };
    // SAFETY: the title's buffer.
    unsafe { deliver_s(&out, buf, count, max) }
}

/// Wide output onto a narrow stream is UTF-8.
fn to_stream_bytes(out: &[u32], wide: bool) -> Vec<u8> {
    if wide {
        let w: Vec<u16> = out.iter().map(|&u| u as u16).collect();
        String::from_utf16_lossy(&w).into_bytes()
    } else {
        out.iter().map(|&u| u as u8).collect()
    }
}

fn vfprintf(options: u64, file: *mut c_void, fmt: *const u8, va: *const u64, wide: bool) -> i32 {
    let Some(out) = format_units(fmt, wide, options, va) else {
        return -1;
    };
    let bytes = to_stream_bytes(&out, wide);
    with(
        file,
        -1,
        |s| if s.put(&bytes) { out.len() as i32 } else { -1 },
    )
}

pub extern "system" fn __stdio_common_vfprintf(
    options: u64,
    file: *mut c_void,
    fmt: *const u8,
    _locale: usize,
    va: *const u64,
) -> i32 {
    vfprintf(options, file, fmt, va, false)
}

pub extern "system" fn __stdio_common_vfwprintf(
    options: u64,
    file: *mut c_void,
    fmt: *const u16,
    _locale: usize,
    va: *const u64,
) -> i32 {
    vfprintf(options, file, fmt.cast(), va, true)
}

/// Scanning a stream, a unit at a time.
struct StreamInput<'a> {
    s: &'a mut Stream,
    wide: bool,
    pushed: Vec<u32>,
}

impl Input for StreamInput<'_> {
    fn get(&mut self) -> Option<u32> {
        if let Some(c) = self.pushed.pop() {
            return Some(c);
        }
        let first = self.s.get()?;
        if !self.wide || first < 0x80 {
            return Some(u32::from(first));
        }
        let extra = match first {
            0xC0..=0xDF => 1,
            0xE0..=0xEF => 2,
            _ => 3,
        };
        let mut b = vec![first];
        for _ in 0..extra {
            b.extend(self.s.get());
        }
        let c = std::str::from_utf8(&b)
            .ok()
            .and_then(|t| t.chars().next())
            .unwrap_or('\u{FFFD}');
        Some(c as u32 & 0xFFFF)
    }
    fn unget(&mut self, c: u32) {
        self.pushed.push(c);
    }
}

impl Drop for StreamInput<'_> {
    fn drop(&mut self) {
        // Units read ahead and not used go back to the stream, last first.
        for &c in self.pushed.iter() {
            match char::from_u32(c) {
                Some(ch) if self.wide && c >= 0x80 => {
                    let mut b = [0u8; 4];
                    for &x in ch.encode_utf8(&mut b).as_bytes().iter().rev() {
                        self.s.unget(x);
                    }
                }
                _ => self.s.unget(c as u8),
            }
        }
    }
}

fn scan_units(options: u64, input: &[u32], fmt: *const u8, wide: bool, va: *const u64) -> i32 {
    if fmt.is_null() {
        return bad(EINVAL);
    }
    let f: Vec<u32> = if wide {
        // SAFETY: a NUL-terminated format.
        unsafe { super::wide(fmt.cast()) }
            .iter()
            .map(|&c| u32::from(c))
            .collect()
    } else {
        // SAFETY: a NUL-terminated format.
        unsafe { super::bytes(fmt) }
            .iter()
            .map(|&c| u32::from(c))
            .collect()
    };
    let mut inp = format::UnitInput {
        units: input.to_vec(),
        pos: 0,
    };
    format::scanf(&f, wide, options, &mut inp, &mut VaList(va))
}

pub extern "system" fn __stdio_common_vsscanf(
    options: u64,
    input: *const u8,
    length: usize,
    fmt: *const u8,
    _locale: usize,
    va: *const u64,
) -> i32 {
    if input.is_null() || fmt.is_null() {
        return bad(EINVAL);
    }
    // SAFETY: the title's input, NUL-terminated or `length` long.
    let u: Vec<u32> = unsafe {
        (0..length)
            .map_while(|i| Some(u32::from(*input.add(i))).filter(|&c| c != 0))
            .collect()
    };
    scan_units(options, &u, fmt, false, va)
}

pub extern "system" fn __stdio_common_vswscanf(
    options: u64,
    input: *const u16,
    length: usize,
    fmt: *const u16,
    _locale: usize,
    va: *const u64,
) -> i32 {
    if input.is_null() || fmt.is_null() {
        return bad(EINVAL);
    }
    // SAFETY: the title's input, NUL-terminated or `length` long.
    let u: Vec<u32> = unsafe {
        (0..length)
            .map_while(|i| Some(u32::from(*input.add(i))).filter(|&c| c != 0))
            .collect()
    };
    scan_units(options, &u, fmt.cast(), true, va)
}

fn vfscanf(options: u64, file: *mut c_void, fmt: *const u8, wide: bool, va: *const u64) -> i32 {
    if fmt.is_null() {
        return bad(EINVAL);
    }
    let f: Vec<u32> = if wide {
        // SAFETY: a NUL-terminated format.
        unsafe { super::wide(fmt.cast()) }
            .iter()
            .map(|&c| u32::from(c))
            .collect()
    } else {
        // SAFETY: a NUL-terminated format.
        unsafe { super::bytes(fmt) }
            .iter()
            .map(|&c| u32::from(c))
            .collect()
    };
    with(file, -1, |s| {
        let mut inp = StreamInput {
            s,
            wide,
            pushed: Vec::new(),
        };
        format::scanf(&f, wide, options, &mut inp, &mut VaList(va))
    })
}

pub extern "system" fn __stdio_common_vfscanf(
    options: u64,
    file: *mut c_void,
    fmt: *const u8,
    _locale: usize,
    va: *const u64,
) -> i32 {
    vfscanf(options, file, fmt, false, va)
}

pub extern "system" fn __stdio_common_vfwscanf(
    options: u64,
    file: *mut c_void,
    fmt: *const u16,
    _locale: usize,
    va: *const u64,
) -> i32 {
    vfscanf(options, file, fmt.cast(), true, va)
}

exports!(pub(super) fn lookup;
    fopen, _wfopen, fopen_s, _wfopen_s, _fdopen, fclose, _fcloseall, fflush, _flushall, fread,
    fwrite, fgetc, fputc, ungetc, fgets, fputs, puts, getchar, putchar, fputwc, fgetwc, ungetwc, fputws,
    feof, ferror, clearerr, _fseeki64, fseek, _ftelli64, ftell, rewind, fgetpos, fsetpos,
    setvbuf, setbuf, _fileno, __acrt_iob_func, _lock_file, _unlock_file,
    _get_stream_buffer_pointers, tmpfile, _open, _wopen, _sopen_s, _wsopen_s, _close, _read,
    _write, _lseeki64, _lseek, _telli64, _tell, _filelengthi64, _filelength, _eof, _isatty,
    _setmode, _commit, _chsize_s, _dup, _dup2, __stdio_common_vsprintf, __stdio_common_vswprintf,
    __stdio_common_vsprintf_s, __stdio_common_vswprintf_s, __stdio_common_vsnprintf_s,
    __stdio_common_vsnwprintf_s, __stdio_common_vfprintf, __stdio_common_vfwprintf,
    __stdio_common_vsscanf, __stdio_common_vswscanf, __stdio_common_vfscanf,
    __stdio_common_vfwscanf;
    "_fsopen" => fopen, "_wfsopen" => _wfopen, "getwc" => fgetwc, "putwc" => fputwc,
    "getc" => fgetc, "putc" => fputc, "_getc_nolock" => fgetc, "_fgetc_nolock" => fgetc,
    "_putc_nolock" => fputc, "_fputc_nolock" => fputc, "_fread_nolock" => fread,
    "_fwrite_nolock" => fwrite, "_fseeki64_nolock" => _fseeki64, "_ftelli64_nolock" => _ftelli64,
    "_fflush_nolock" => fflush, "_ungetc_nolock" => ungetc, "_fclose_nolock" => fclose,
    "_getchar_nolock" => getchar, "_putchar_nolock" => putchar, "_sopen" => _wsopen_compat,
    "_creat" => _creat, "__stdio_common_vsprintf_p" => __stdio_common_vsprintf,
    "__stdio_common_vswprintf_p" => __stdio_common_vswprintf,
    "__stdio_common_vfprintf_p" => __stdio_common_vfprintf,
    "__stdio_common_vfwprintf_p" => __stdio_common_vfwprintf,
    "__stdio_common_vfprintf_s" => __stdio_common_vfprintf,
    "__stdio_common_vfwprintf_s" => __stdio_common_vfwprintf,
    "__stdio_common_vsscanf_s" => __stdio_common_vsscanf,
    "_fseek_nolock" => fseek, "_ftell_nolock" => ftell);

/// `_sopen(path, oflag, shflag, pmode)`: the share mode is ignored.
extern "system" fn _wsopen_compat(path: *const u8, oflag: i32, _share: i32, pmode: i32) -> i32 {
    _open(path, oflag, pmode)
}

/// `_creat(path, pmode)`: create or truncate for writing.
extern "system" fn _creat(path: *const u8, pmode: i32) -> i32 {
    _open(path, O_CREAT | O_TRUNC | O_RDWR, pmode)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cstr(s: &str) -> Vec<u8> {
        s.bytes().chain([0]).collect()
    }

    fn open(path: &std::path::Path, mode: &str) -> *mut c_void {
        fopen(cstr(path.to_str().unwrap()).as_ptr(), cstr(mode).as_ptr())
    }

    #[test]
    fn ungetwc_pushes_utf16_units_through_the_stream_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.txt");
        std::fs::write(&path, "éx").unwrap();
        let f = open(&path, "r");
        assert_eq!(fgetwc(f), u32::from('é'));
        assert_eq!(ungetwc('©' as u16, f), '©' as u16);
        assert_eq!(fgetwc(f), u32::from('©'));
        assert_eq!(fgetwc(f), u32::from('x'));
        assert_eq!(fgetwc(f), WEOF);
        assert_eq!(feof(f), 1);
        assert_eq!(ungetwc(WEOF as u16, f), WEOF as u16);
        assert_eq!(feof(f), 1);
        assert_eq!(ungetwc('雪' as u16, f), '雪' as u16);
        assert_eq!(feof(f), 0);
        assert_eq!(fgetwc(f), u32::from('雪'));
        assert_eq!(ungetwc('q' as u16, f), 'q' as u16);
        rewind(f);
        assert_eq!(fgetwc(f), u32::from('é'));
        assert_eq!(fclose(f), 0);

        let f = open(&path, "w");
        assert_eq!(ungetwc('q' as u16, f), WEOF as u16);
        assert_eq!(fclose(f), 0);
        assert_eq!(ungetwc('q' as u16, std::ptr::null_mut()), WEOF as u16);
        assert_eq!(errno(), EINVAL);
    }

    #[test]
    fn files_read_write_seek_and_close() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        let f = open(&path, "wb");
        assert!(!f.is_null());
        assert_eq!(fwrite(b"hello world".as_ptr(), 1, 11, f), 11);
        assert_eq!(_ftelli64(f), 11);
        assert_eq!(fclose(f), 0);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");

        let f = open(&path, "rb");
        let mut buf = [0u8; 5];
        assert_eq!(fread(buf.as_mut_ptr(), 1, 5, f), 5);
        assert_eq!(&buf, b"hello");
        assert_eq!(ftell(f), 5);
        assert_eq!(fseek(f, 6, SEEK_SET), 0);
        assert_eq!(fgetc(f), i32::from(b'w'));
        assert_eq!(ungetc(i32::from(b'W'), f), i32::from(b'W'));
        assert_eq!(fgetc(f), i32::from(b'W'));
        assert_eq!(_fseeki64(f, -2, SEEK_END), 0);
        let mut tail = [0u8; 8];
        assert_eq!(fread(tail.as_mut_ptr(), 1, 8, f), 2);
        assert_eq!(&tail[..2], b"ld");
        assert_eq!(feof(f), 1);
        rewind(f);
        assert_eq!(feof(f), 0);
        assert_eq!(fgetc(f), i32::from(b'h'));
        assert_eq!(fclose(f), 0);

        // Append, and read-write.
        let f = open(&path, "ab");
        fputs(c"!\n".as_ptr().cast(), f);
        fclose(f);
        let f = open(&path, "r+b");
        fseek(f, 0, SEEK_SET);
        fputc(i32::from(b'H'), f);
        fseek(f, 0, SEEK_SET);
        assert_eq!(fgetc(f), i32::from(b'H'));
        fclose(f);
        assert_eq!(std::fs::read(&path).unwrap(), b"Hello world!\n");
        assert!(open(&dir.path().join("missing"), "rb").is_null());
        assert_eq!(errno(), ENOENT);
        assert!(open(&path, "q").is_null());
    }

    #[test]
    fn text_mode_and_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.txt");
        std::fs::write(&path, b"one\r\ntwo\r\nthree").unwrap();
        let f = open(&path, "r");
        let mut line = [0u8; 16];
        assert!(!fgets(line.as_mut_ptr(), 16, f).is_null());
        assert_eq!(&line[..5], b"one\n\0");
        assert!(!fgets(line.as_mut_ptr(), 4, f).is_null());
        assert_eq!(&line[..4], b"two\0");
        assert!(!fgets(line.as_mut_ptr(), 16, f).is_null());
        assert_eq!(&line[..2], b"\n\0");
        assert!(!fgets(line.as_mut_ptr(), 16, f).is_null());
        assert_eq!(&line[..6], b"three\0");
        assert!(fgets(line.as_mut_ptr(), 16, f).is_null());
        assert_eq!(feof(f), 1);
        fclose(f);
    }

    #[test]
    fn fgets_with_room_for_only_the_nul_stores_an_empty_string() {
        // n = 1 reads nothing: no character was read, but no end of file was met either, so
        // it stores "" and returns the buffer (C 7.21.7.2), and the stream is untouched.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("one.txt");
        std::fs::write(&path, b"ab").unwrap();
        let f = open(&path, "rb");
        let mut line = [0xEEu8; 4];
        assert_eq!(fgets(line.as_mut_ptr(), 1, f), line.as_mut_ptr());
        assert_eq!(line, [0, 0xEE, 0xEE, 0xEE]);
        assert_eq!(fgetc(f), i32::from(b'a'));
        assert_eq!(feof(f), 0);
        // With room for more, an end of file before any character is still NULL.
        assert_eq!(fgetc(f), i32::from(b'b'));
        assert!(fgets(line.as_mut_ptr(), 4, f).is_null());
        assert_eq!(feof(f), 1);
        fclose(f);
    }

    #[test]
    fn descriptors_share_the_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.bin");
        let fd = _open(
            cstr(path.to_str().unwrap()).as_ptr(),
            O_CREAT | O_RDWR | O_BINARY,
            0,
        );
        assert!(fd >= 3);
        assert_eq!(_write(fd, b"abcdef".as_ptr(), 6), 6);
        assert_eq!(_lseeki64(fd, 2, SEEK_SET), 2);
        let mut b = [0u8; 4];
        assert_eq!(_read(fd, b.as_mut_ptr(), 4), 4);
        assert_eq!(&b, b"cdef");
        assert_eq!(_filelengthi64(fd), 6);
        assert_eq!(_eof(fd), 1);
        let f = _fdopen(fd, c"rb".as_ptr().cast());
        assert_eq!(_fileno(f), fd);
        assert_eq!(_ftelli64(f), 6);
        assert_eq!(_chsize_s(fd, 3), 0);
        assert_eq!(_close(fd), 0);
        // The descriptor number can be reused immediately, so check the original stream.
        assert_eq!(_fileno(f), -1);
        assert_eq!(fclose(f), 0);
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
    }

    #[test]
    fn a_bad_descriptor_sets_ebadf_for_its_length_and_position() {
        // Microsoft documents -1 with EBADF for an invalid descriptor in `_filelength`,
        // `_tell` and `_eof` (and their 64-bit forms).
        let fd = 9999;
        for name in ["_filelengthi64", "_filelength", "_telli64", "_tell", "_eof"] {
            set_errno(0);
            let r = match name {
                "_filelengthi64" => _filelengthi64(fd),
                "_filelength" => i64::from(_filelength(fd)),
                "_telli64" => _telli64(fd),
                "_tell" => i64::from(_tell(fd)),
                _ => i64::from(_eof(fd)),
            };
            assert_eq!(r, -1, "{name}");
            assert_eq!(errno(), EBADF, "{name}");
        }
    }

    #[test]
    fn reading_a_write_only_descriptor_fails_with_ebadf() {
        // Microsoft documents `_read` on a descriptor not open for reading as -1 with EBADF;
        // 0 would tell the title it is at the end of the file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.bin");
        std::fs::write(&path, b"abc").unwrap();
        let fd = _open(
            cstr(path.to_str().unwrap()).as_ptr(),
            O_WRONLY | O_BINARY,
            0,
        );
        assert!(fd >= 3);
        let mut b = [0xEEu8; 4];
        set_errno(0);
        assert_eq!(_read(fd, b.as_mut_ptr(), 4), -1);
        assert_eq!(errno(), EBADF);
        assert_eq!(b, [0xEE; 4]);
        // An empty read of a readable descriptor is still 0.
        assert_eq!(_close(fd), 0);
        let fd = _open(cstr(path.to_str().unwrap()).as_ptr(), O_BINARY, 0);
        assert_eq!(_read(fd, b.as_mut_ptr(), 0), 0);
        assert_eq!(_read(fd, b.as_mut_ptr(), 4), 3);
        assert_eq!(_read(fd, b.as_mut_ptr(), 4), 0);
        assert_eq!(_close(fd), 0);
    }

    #[test]
    fn a_bad_seek_sets_einval_and_keeps_the_position() {
        // Microsoft documents `_lseek`'s errno as EBADF for a bad descriptor and EINVAL for a
        // bad origin or a position before the start of the file; `fseek` fails the same way.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.bin");
        std::fs::write(&path, b"abcdef").unwrap();
        let fd = _open(cstr(path.to_str().unwrap()).as_ptr(), O_BINARY, 0);
        assert!(fd >= 3);
        assert_eq!(_lseeki64(fd, 2, SEEK_SET), 2);
        for (offset, origin) in [(-1, SEEK_SET), (-3, SEEK_CUR), (-7, SEEK_END), (0, 3)] {
            set_errno(0);
            assert_eq!(_lseeki64(fd, offset, origin), -1, "{offset} {origin}");
            assert_eq!(errno(), EINVAL, "{offset} {origin}");
            set_errno(0);
            assert_eq!(_lseek(fd, offset as i32, origin), -1, "{offset} {origin}");
            assert_eq!(errno(), EINVAL, "{offset} {origin}");
        }
        assert_eq!(_telli64(fd), 2);
        set_errno(0);
        assert_eq!(_lseeki64(9999, 0, SEEK_SET), -1);
        assert_eq!(errno(), EBADF);
        assert_eq!(_close(fd), 0);

        let f = open(&path, "rb");
        assert_eq!(fseek(f, 4, SEEK_SET), 0);
        for (offset, origin) in [(-5, SEEK_CUR), (-7, SEEK_END)] {
            set_errno(0);
            assert_eq!(fseek(f, offset, origin), -1, "{offset} {origin}");
            assert_eq!(errno(), EINVAL, "{offset} {origin}");
        }
        assert_eq!(ftell(f), 4);
        assert_eq!(fclose(f), 0);
    }

    #[test]
    fn standard_streams_exist() {
        let (i, o, e) = (__acrt_iob_func(0), __acrt_iob_func(1), __acrt_iob_func(2));
        assert!(!i.is_null() && !o.is_null() && !e.is_null() && i != o && o != e);
        assert!(__acrt_iob_func(3).is_null());
        assert_eq!(_fileno(o), 1);
        assert_eq!(_fileno(e), 2);
        assert_eq!(fflush(o), 0);
    }

    fn sprintf(fmt: &str, args: &[u64]) -> (i32, String) {
        let mut buf = [0u8; 64];
        let n = __stdio_common_vsprintf(
            format::PRINTF_STANDARD_SNPRINTF_BEHAVIOR,
            buf.as_mut_ptr(),
            buf.len(),
            cstr(fmt).as_ptr(),
            0,
            args.as_ptr(),
        );
        (
            n,
            String::from_utf8_lossy(&buf[..buf.iter().position(|&b| b == 0).unwrap()]).into_owned(),
        )
    }

    #[test]
    fn vsprintf_reads_a_windows_va_list() {
        let s = cstr("str");
        let args = [42u64, s.as_ptr() as u64, 2.5f64.to_bits(), u64::MAX];
        assert_eq!(
            sprintf("%d|%s|%.1f|%I64d", &args),
            (13, "42|str|2.5|-1".into())
        );
        // The size is what the text needs, whatever the buffer holds.
        let mut small = [0u8; 4];
        let n = __stdio_common_vsprintf(
            format::PRINTF_STANDARD_SNPRINTF_BEHAVIOR,
            small.as_mut_ptr(),
            4,
            cstr("%d").as_ptr(),
            0,
            [123456u64].as_ptr(),
        );
        assert_eq!((n, &small), (6, b"123\0"));
        let n = __stdio_common_vsprintf(
            format::PRINTF_STANDARD_SNPRINTF_BEHAVIOR,
            std::ptr::null_mut(),
            0,
            cstr("%d").as_ptr(),
            0,
            [123456u64].as_ptr(),
        );
        assert_eq!(n, 6);
    }

    /// `%d` of `value` into four bytes of `x` with `count` and `options`: the result and the bytes.
    fn into_four(options: u64, count: usize, value: u64) -> (i32, [u8; 4]) {
        let mut b = [b'x'; 4];
        let n = __stdio_common_vsprintf(
            options,
            b.as_mut_ptr(),
            count,
            cstr("%d").as_ptr(),
            0,
            [value].as_ptr(),
        );
        (n, b)
    }

    #[test]
    fn legacy_and_iso_printf_truncate_as_the_ucrt_does() {
        let legacy = format::PRINTF_LEGACY_VSPRINTF_NULL_TERMINATION;
        // `_vsnprintf`: terminated when there is room; a text that fills the buffer is returned
        // whole without a NUL; a longer one is cut without a NUL and gives -1.
        assert_eq!(into_four(legacy, 4, 123), (3, *b"123\0"));
        assert_eq!(into_four(legacy, 4, 1234), (4, *b"1234"));
        assert_eq!(into_four(legacy, 3, 1234), (-1, *b"123x"));
        // ISO `vswprintf` (`_vswprintf_c_l`, no option): always terminated, -1 when cut.
        assert_eq!(into_four(0, 4, 123), (3, *b"123\0"));
        assert_eq!(into_four(0, 4, 1234), (-1, *b"123\0"));
        assert_eq!(into_four(0, 3, 1234), (-1, *b"12\0x"));
        // A buffer with no room is left alone.
        assert_eq!(into_four(legacy, 0, 1), (-1, *b"xxxx"));
        assert_eq!(into_four(0, 0, 1), (-1, *b"xxxx"));
        // No buffer and no room is the sizing idiom in every mode; no buffer with room is invalid.
        for options in [0, legacy, format::PRINTF_STANDARD_SNPRINTF_BEHAVIOR] {
            let size = |count| {
                __stdio_common_vsprintf(
                    options,
                    std::ptr::null_mut(),
                    count,
                    cstr("%d").as_ptr(),
                    0,
                    [123456u64].as_ptr(),
                )
            };
            assert_eq!(size(0), 6);
            assert_eq!(size(8), -1);
            assert_eq!(errno(), EINVAL);
        }
        // `swprintf(buf, 4, L"%d", 1234)` ends in a NUL the title can read up to.
        let fmt: Vec<u16> = "%d".encode_utf16().chain([0]).collect();
        let mut w = [u16::from(b'x'); 4];
        let n = __stdio_common_vswprintf(0, w.as_mut_ptr(), 4, fmt.as_ptr(), 0, [1234u64].as_ptr());
        assert_eq!((n, String::from_utf16(&w).unwrap()), (-1, "123\0".to_string()));
    }

    #[test]
    fn the_s_variants_empty_the_buffer_when_it_is_too_small() {
        let mut b = [b'x'; 4];
        let n = __stdio_common_vsprintf_s(
            0,
            b.as_mut_ptr(),
            4,
            cstr("%d").as_ptr(),
            0,
            [12345u64].as_ptr(),
        );
        assert_eq!((n, b[0]), (-1, 0));
        let n = __stdio_common_vsprintf_s(
            0,
            b.as_mut_ptr(),
            4,
            cstr("%d").as_ptr(),
            0,
            [123u64].as_ptr(),
        );
        assert_eq!((n, &b), (3, b"123\0"));
        let n = __stdio_common_vsnprintf_s(
            0,
            b.as_mut_ptr(),
            4,
            TRUNCATE,
            cstr("%d").as_ptr(),
            0,
            [123456u64].as_ptr(),
        );
        assert_eq!((n, &b), (-1, b"123\0"));
    }

    #[test]
    fn vswprintf_writes_utf16() {
        let fmt: Vec<u16> = "%s=%ls %d".encode_utf16().chain([0]).collect();
        let name: Vec<u16> = "ключ".encode_utf16().chain([0]).collect();
        let val: Vec<u16> = "ok".encode_utf16().chain([0]).collect();
        let args = [name.as_ptr() as u64, val.as_ptr() as u64, 7u64];
        let mut out = [0u16; 32];
        let n = __stdio_common_vswprintf(
            format::PRINTF_STANDARD_SNPRINTF_BEHAVIOR,
            out.as_mut_ptr(),
            32,
            fmt.as_ptr(),
            0,
            args.as_ptr(),
        );
        assert_eq!(n, 9);
        assert_eq!(String::from_utf16(&out[..9]).unwrap(), "ключ=ok 7");
    }

    #[test]
    fn vfprintf_writes_a_stream_and_vsscanf_reads_a_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.txt");
        let f = open(&path, "w");
        let n = __stdio_common_vfprintf(0, f, cstr("x=%d\n").as_ptr(), 0, [9u64].as_ptr());
        assert_eq!(n, 4);
        fclose(f);
        assert_eq!(std::fs::read(&path).unwrap(), b"x=9\n");

        let (mut a, mut b) = (0i32, 0f64);
        let args = [&mut a as *mut i32 as u64, &mut b as *mut f64 as u64];
        let n = __stdio_common_vsscanf(
            0,
            cstr("17 2.5").as_ptr(),
            usize::MAX,
            cstr("%d %lf").as_ptr(),
            0,
            args.as_ptr(),
        );
        assert_eq!((n, a, b), (2, 17, 2.5));
        // Scanning a file leaves what it did not use for the next read.
        std::fs::write(&path, b"12 rest").unwrap();
        let f = open(&path, "r");
        let mut v = 0i32;
        let n = __stdio_common_vfscanf(
            0,
            f,
            cstr("%d").as_ptr(),
            0,
            [&mut v as *mut i32 as u64].as_ptr(),
        );
        assert_eq!((n, v), (1, 12));
        assert_eq!(fgetc(f), i32::from(b' '));
        fclose(f);
    }

    #[test]
    fn large_binary_fread_on_write_only_stream_and_ungetc_fail_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        std::fs::write(&path, vec![0x42u8; 8192]).unwrap();
        let f = open(&path, "wb");
        assert!(!f.is_null());
        let mut buf = vec![0xAAu8; 4096];
        set_errno(0);
        assert_eq!(fread(buf.as_mut_ptr(), 1, 4096, f), 0);
        assert_eq!(errno(), EBADF);
        assert_eq!(ferror(f), 1);
        assert_eq!(ungetc(i32::from(b'x'), f), EOF);
        assert_eq!(fclose(f), 0);
    }

    #[test]
    fn chsize_rejects_negative_sizes_and_read_only_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resize.bin");
        std::fs::write(&path, b"abcdef").unwrap();

        let fd = _open(cstr(path.to_str().unwrap()).as_ptr(), O_RDWR | O_BINARY, 0);
        assert!(fd >= 3);
        set_errno(0);
        assert_eq!(_chsize_s(fd, -5), EINVAL);
        assert_eq!(errno(), EINVAL);
        assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");
        assert_eq!(_close(fd), 0);

        let ro = _open(cstr(path.to_str().unwrap()).as_ptr(), O_BINARY, 0);
        assert!(ro >= 3);
        set_errno(0);
        assert_eq!(_chsize_s(ro, 2), EACCES);
        assert_eq!(errno(), EACCES);
        assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");
        assert_eq!(_close(ro), 0);
    }

    #[test]
    fn null_pointers_and_bad_fdopen_modes_set_einval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nulls.bin");
        std::fs::write(&path, b"abcd").unwrap();
        let f = open(&path, "r+b");
        assert!(!f.is_null());

        set_errno(0);
        assert_eq!(fread(std::ptr::null_mut(), 1, 4, f), 0);
        assert_eq!(errno(), EINVAL);
        set_errno(0);
        assert_eq!(fwrite(std::ptr::null(), 1, 4, f), 0);
        assert_eq!(errno(), EINVAL);
        set_errno(0);
        assert_eq!(fputs(std::ptr::null(), f), EOF);
        assert_eq!(errno(), EINVAL);
        set_errno(0);
        assert_eq!(puts(std::ptr::null()), EOF);
        assert_eq!(errno(), EINVAL);
        set_errno(0);
        assert_eq!(fputws(std::ptr::null(), f), EOF);
        assert_eq!(errno(), EINVAL);
        set_errno(0);
        assert_eq!(fgetpos(f, std::ptr::null_mut()), -1);
        assert_eq!(errno(), EINVAL);
        assert_eq!(fclose(f), 0);

        let fd = _open(cstr(path.to_str().unwrap()).as_ptr(), O_RDWR | O_BINARY, 0);
        assert!(fd >= 3);
        set_errno(0);
        assert_eq!(_read(fd, std::ptr::null_mut(), 4), -1);
        assert_eq!(errno(), EINVAL);
        set_errno(0);
        assert_eq!(_write(fd, std::ptr::null(), 4), -1);
        assert_eq!(errno(), EINVAL);
        // Zero-length _write with a null pointer is a no-op without UB.
        assert_eq!(_write(fd, std::ptr::null(), 0), 0);
        set_errno(0);
        assert_eq!(fstat(fd, std::ptr::null_mut(), true), -1);
        assert_eq!(errno(), EINVAL);
        set_errno(0);
        assert!(_fdopen(fd, std::ptr::null()).is_null());
        assert_eq!(errno(), EINVAL);
        set_errno(0);
        assert!(_fdopen(fd, c"invalid".as_ptr().cast()).is_null());
        assert_eq!(errno(), EINVAL);
        assert_eq!(_close(fd), 0);

        let mut buf = [0u8; 8];
        set_errno(0);
        assert_eq!(
            __stdio_common_vsprintf(0, buf.as_mut_ptr(), 8, std::ptr::null(), 0, std::ptr::null()),
            -1
        );
        assert_eq!(errno(), EINVAL);
        set_errno(0);
        assert_eq!(
            __stdio_common_vsscanf(0, std::ptr::null(), 4, c"%d".as_ptr().cast(), 0, std::ptr::null()),
            -1
        );
        assert_eq!(errno(), EINVAL);
        set_errno(0);
        assert_eq!(
            __stdio_common_vswscanf(0, std::ptr::null(), 4, std::ptr::null(), 0, std::ptr::null()),
            -1
        );
        assert_eq!(errno(), EINVAL);
    }
}
