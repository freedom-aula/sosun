//! Winsock (`ws2_32.dll`) on BSD sockets, for the native macOS host.
//!
//! A subset the titles use: startup and cleanup, stream and datagram
//! sockets, `select` and `WSAPoll`, non-blocking mode, socket options with
//! their Windows constants mapped to macOS's, name lookup with the Windows
//! structure layouts, and `WSAGetLastError` with `WSAE*` codes. A `SOCKET` is
//! a handle of ours (never the descriptor); `closesocket` ends it.
//!
//! The policy is `modules::net`'s, the console without a network cable:
//! connects and datagram sends go to loopback only (`WSAENETUNREACH`
//! otherwise), forward lookups answer `localhost` and IP literals only
//! (`WSAHOST_NOT_FOUND` otherwise), and a bind to all interfaces is narrowed
//! to loopback.
//!
//! shortcut: no overlapped I/O (`WSASend`, `WSARecv`, `WSAEventSelect`,
//! `ConnectEx`) and no `WSAIoctl` beyond non-blocking mode and `FIONREAD`;
//! add them when a title calls them (the unresolved-import stub names it).
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_ulong, c_void};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

use super::lock;

type Socket = usize;
const INVALID_SOCKET: Socket = !0;
const SOCKET_ERROR: i32 = -1;

const WSAEINTR: i32 = 10004;
const WSAEBADF: i32 = 10009;
const WSAEACCES: i32 = 10013;
const WSAEFAULT: i32 = 10014;
const WSAEINVAL: i32 = 10022;
const WSAEMFILE: i32 = 10024;
const WSAEWOULDBLOCK: i32 = 10035;
const WSAENOTSOCK: i32 = 10038;
const WSAEMSGSIZE: i32 = 10040;
const WSAEOPNOTSUPP: i32 = 10045;
const WSAEAFNOSUPPORT: i32 = 10047;
const WSAENETUNREACH: i32 = 10051;
const WSAECONNRESET: i32 = 10054;
const WSAESOCKTNOSUPPORT: i32 = 10044;
const WSAVERNOTSUPPORTED: i32 = 10092;
const WSANOTINITIALISED: i32 = 10093;
const WSATYPE_NOT_FOUND: i32 = 10109;
const WSAHOST_NOT_FOUND: i32 = 11001;
const WSATRY_AGAIN: i32 = 11002;
const WSANO_RECOVERY: i32 = 11003;
const WSA_NOT_ENOUGH_MEMORY: i32 = 8;

const AF_INET: u16 = 2;
const AF_INET6: u16 = 23;
/// macOS's `AF_INET6`.
const BSD_AF_INET6: c_int = 30;

const FIONBIO: u32 = 0x8004_667E;
const FIONREAD: u32 = 0x4004_667F;

const NI_NAMEREQD: i32 = 0x04;

/// `FD_SETSIZE` of a Windows `fd_set`.
const WIN_FD_SETSIZE: usize = 64;
/// `FD_SETSIZE` of macOS's, as 32-bit words.
const BSD_FD_WORDS: usize = 32;

unsafe extern "C" {
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn connect(fd: c_int, addr: *const u8, len: u32) -> c_int;
    fn bind(fd: c_int, addr: *const u8, len: u32) -> c_int;
    fn listen(fd: c_int, backlog: c_int) -> c_int;
    fn accept(fd: c_int, addr: *mut u8, len: *mut u32) -> c_int;
    fn send(fd: c_int, buf: *const c_void, len: usize, flags: c_int) -> isize;
    fn sendto(
        fd: c_int,
        buf: *const c_void,
        len: usize,
        flags: c_int,
        addr: *const u8,
        addr_len: u32,
    ) -> isize;
    fn select(
        nfds: c_int,
        read: *mut u32,
        write: *mut u32,
        except: *mut u32,
        timeout: *mut BsdTimeval,
    ) -> c_int;
    fn poll(fds: *mut BsdPollFd, count: u32, timeout: c_int) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
    fn setsockopt(fd: c_int, level: c_int, name: c_int, value: *const c_void, len: u32) -> c_int;
    fn getsockopt(fd: c_int, level: c_int, name: c_int, value: *mut c_void, len: *mut u32)
    -> c_int;
    fn getsockname(fd: c_int, addr: *mut u8, len: *mut u32) -> c_int;
    fn getpeername(fd: c_int, addr: *mut u8, len: *mut u32) -> c_int;
    fn shutdown(fd: c_int, how: c_int) -> c_int;
    fn getaddrinfo(
        node: *const c_char,
        service: *const c_char,
        hints: *const BsdAddrInfo,
        result: *mut *mut BsdAddrInfo,
    ) -> c_int;
    fn freeaddrinfo(info: *mut BsdAddrInfo);
    fn gethostname(buf: *mut c_char, len: usize) -> c_int;
    fn inet_addr(text: *const c_char) -> u32;
    fn __error() -> *mut c_int;
}

#[repr(C)]
struct BsdTimeval {
    sec: i64,
    usec: i32,
}

#[repr(C)]
struct BsdPollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

#[repr(C)]
struct BsdAddrInfo {
    flags: c_int,
    family: c_int,
    socktype: c_int,
    protocol: c_int,
    addr_len: u32,
    canon_name: *mut c_char,
    addr: *mut u8,
    next: *mut BsdAddrInfo,
}

// Errors.

/// Winsock's last error is the thread's `GetLastError`.
fn set_wsa_error(code: i32) {
    super::set_error(code as u32);
}

/// `errno` as `WSAE*`: the BSD numbers 35 to 66 are Winsock's minus 10000.
fn wsa_from_errno(errno: i32) -> i32 {
    match errno {
        35..=66 => 10000 + errno,
        4 => WSAEINTR,
        9 => WSAEBADF,
        13 => WSAEACCES,
        14 => WSAEFAULT,
        22 => WSAEINVAL,
        24 => WSAEMFILE,
        32 => WSAECONNRESET, // EPIPE
        _ => WSAEINVAL,
    }
}

fn errno() -> i32 {
    // SAFETY: the calling thread's errno.
    unsafe { *__error() }
}

/// Fails the call with the current `errno`.
fn fail() -> i32 {
    set_wsa_error(wsa_from_errno(errno()));
    SOCKET_ERROR
}

fn fail_with(code: i32) -> i32 {
    set_wsa_error(code);
    SOCKET_ERROR
}

pub extern "system" fn WSAGetLastError() -> i32 {
    super::GetLastError() as i32
}

pub extern "system" fn WSASetLastError(code: i32) {
    set_wsa_error(code);
}

// Startup and the socket table.

static STARTUPS: AtomicU32 = AtomicU32::new(0);

pub extern "system" fn WSAStartup(version: u16, data: *mut u8) -> i32 {
    let (major, minor) = (version & 0xFF, version >> 8);
    if major == 0 || major > 2 || (major == 2 && minor > 2) {
        return WSAVERNOTSUPPORTED;
    }
    if !data.is_null() {
        // WSADATA, x64 layout (408 bytes).
        let mut d = [0u8; 408];
        d[0..2].copy_from_slice(&version.to_le_bytes());
        d[2..4].copy_from_slice(&0x0202u16.to_le_bytes());
        d[4..6].copy_from_slice(&32767u16.to_le_bytes());
        d[6..8].copy_from_slice(&65467u16.to_le_bytes());
        let text = b"VIctory Winsock on BSD sockets";
        d[16..16 + text.len()].copy_from_slice(text);
        d[273..273 + 7].copy_from_slice(b"Running");
        // SAFETY: the title passes a WSADATA.
        unsafe { std::ptr::copy_nonoverlapping(d.as_ptr(), data, d.len()) };
    }
    STARTUPS.fetch_add(1, Ordering::AcqRel);
    0
}

pub extern "system" fn WSACleanup() -> i32 {
    let ok = STARTUPS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
        .is_ok();
    if ok {
        0
    } else {
        fail_with(WSANOTINITIALISED)
    }
}

fn started() -> bool {
    if STARTUPS.load(Ordering::Acquire) == 0 {
        set_wsa_error(WSANOTINITIALISED);
        return false;
    }
    true
}

static SOCKETS: LazyLock<Mutex<HashMap<usize, c_int>>> = LazyLock::new(Default::default);
static NEXT_SOCKET: AtomicUsize = AtomicUsize::new(0x100);

fn new_socket(fd: c_int) -> Socket {
    // Not a descriptor, and not a kernel-object handle either.
    let handle = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed) << 2;
    lock(&SOCKETS).insert(handle, fd);
    handle
}

/// The descriptor behind `s`, or `WSAENOTSOCK`.
fn fd_of(s: Socket) -> Option<c_int> {
    let fd = lock(&SOCKETS).get(&s).copied();
    if fd.is_none() {
        set_wsa_error(if started() { WSAENOTSOCK } else { WSANOTINITIALISED });
    }
    fd
}

/// A descriptor made or accepted: no `SIGPIPE`, which Windows has not.
fn adopt(fd: c_int) -> Socket {
    const SOL_SOCKET: c_int = 0xFFFF;
    const SO_NOSIGPIPE: c_int = 0x1022;
    let on: c_int = 1;
    // SAFETY: our descriptor and a valid option.
    unsafe {
        setsockopt(
            fd,
            SOL_SOCKET,
            SO_NOSIGPIPE,
            (&raw const on).cast(),
            size_of::<c_int>() as u32,
        )
    };
    new_socket(fd)
}

fn bsd_family(af: i32) -> Option<c_int> {
    match af {
        0 => Some(0),
        2 => Some(2),
        23 => Some(BSD_AF_INET6),
        _ => None,
    }
}

pub extern "system" fn socket_(af: i32, kind: i32, protocol: i32) -> Socket {
    if !started() {
        return INVALID_SOCKET;
    }
    let Some(domain) = bsd_family(af).filter(|&d| d != 0) else {
        set_wsa_error(WSAEAFNOSUPPORT);
        return INVALID_SOCKET;
    };
    if !(1..=2).contains(&kind) {
        set_wsa_error(WSAESOCKTNOSUPPORT);
        return INVALID_SOCKET;
    }
    // SAFETY: plain call.
    let fd = unsafe { socket(domain, kind, protocol) };
    if fd < 0 {
        fail();
        return INVALID_SOCKET;
    }
    adopt(fd)
}

pub extern "system" fn closesocket(s: Socket) -> i32 {
    let Some(fd) = fd_of(s) else {
        return SOCKET_ERROR;
    };
    lock(&SOCKETS).remove(&s);
    // SAFETY: the descriptor was ours and is now out of the table.
    if unsafe { close(fd) } < 0 {
        return fail();
    }
    0
}

// Addresses.

/// A Windows `sockaddr` as a BSD one, and its length.
fn to_bsd(addr: *const u8, len: i32) -> Option<([u8; 28], u32)> {
    let len = usize::try_from(len).ok()?;
    if addr.is_null() || len < 2 {
        return None;
    }
    // SAFETY: the title passes `len` readable bytes.
    let w = unsafe { std::slice::from_raw_parts(addr, len) };
    let mut b = [0u8; 28];
    match u16::from_le_bytes([w[0], w[1]]) {
        AF_INET if len >= 16 => {
            b[0] = 16;
            b[1] = 2;
            b[2..8].copy_from_slice(&w[2..8]);
            Some((b, 16))
        }
        AF_INET6 if len >= 28 => {
            b[0] = 28;
            b[1] = BSD_AF_INET6 as u8;
            b[2..28].copy_from_slice(&w[2..28]);
            Some((b, 28))
        }
        _ => None,
    }
}

/// A BSD `sockaddr` as a Windows one, written to the title's buffer as far
/// as `*len` allows; `*len` gets the full size.
fn from_bsd(b: &[u8], out: *mut u8, len: *mut i32) {
    let mut w = [0u8; 28];
    let size = match b[1] as c_int {
        2 => {
            w[0..2].copy_from_slice(&AF_INET.to_le_bytes());
            w[2..8].copy_from_slice(&b[2..8]);
            16
        }
        BSD_AF_INET6 => {
            w[0..2].copy_from_slice(&AF_INET6.to_le_bytes());
            w[2..28].copy_from_slice(&b[2..28]);
            28
        }
        _ => 0,
    };
    if out.is_null() || len.is_null() {
        return;
    }
    // SAFETY: the title passes a buffer of `*len` bytes and the length.
    unsafe {
        let room = usize::try_from(len.read()).unwrap_or(0);
        std::ptr::copy_nonoverlapping(w.as_ptr(), out, size.min(room));
        len.write(size as i32);
    }
}

fn ip_of(b: &[u8]) -> Option<IpAddr> {
    match b[1] as c_int {
        2 => Some(IpAddr::V4(Ipv4Addr::new(b[4], b[5], b[6], b[7]))),
        BSD_AF_INET6 => Some(IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&b[8..24]).ok()?))),
        _ => None,
    }
}

fn loopback(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

/// Loopback only, or `WSAENETUNREACH`.
fn reachable(b: &[u8]) -> bool {
    if ip_of(b).is_some_and(loopback) {
        return true;
    }
    tracing::info!(address = ?ip_of(b), "network connect blocked");
    set_wsa_error(WSAENETUNREACH);
    false
}

/// The wildcard address becomes loopback, so nothing off the machine
/// connects in.
fn narrow_wildcard(b: &mut [u8; 28]) {
    match b[1] as c_int {
        2 if b[4..8] == [0; 4] => b[4..8].copy_from_slice(&[127, 0, 0, 1]),
        BSD_AF_INET6 if b[8..24] == [0; 16] => b[23] = 1,
        _ => {}
    }
}

pub extern "system" fn connect_(s: Socket, addr: *const u8, len: i32) -> i32 {
    let Some(fd) = fd_of(s) else {
        return SOCKET_ERROR;
    };
    let Some((b, n)) = to_bsd(addr, len) else {
        return fail_with(WSAEAFNOSUPPORT);
    };
    if !reachable(&b) {
        return SOCKET_ERROR;
    }
    // SAFETY: a socket of ours and a built address.
    if unsafe { connect(fd, b.as_ptr(), n) } < 0 {
        // A non-blocking connect is `WSAEWOULDBLOCK` on Windows.
        return if errno() == 36 {
            fail_with(WSAEWOULDBLOCK)
        } else {
            fail()
        };
    }
    0
}

pub extern "system" fn bind_(s: Socket, addr: *const u8, len: i32) -> i32 {
    let Some(fd) = fd_of(s) else {
        return SOCKET_ERROR;
    };
    let Some((mut b, n)) = to_bsd(addr, len) else {
        return fail_with(WSAEAFNOSUPPORT);
    };
    narrow_wildcard(&mut b);
    // SAFETY: a socket of ours and a built address.
    if unsafe { bind(fd, b.as_ptr(), n) } < 0 {
        return fail();
    }
    0
}

pub extern "system" fn listen_(s: Socket, backlog: i32) -> i32 {
    let Some(fd) = fd_of(s) else {
        return SOCKET_ERROR;
    };
    // SOMAXCONN on Windows is 0x7fffffff: the system's own limit.
    // SAFETY: a socket of ours.
    if unsafe { listen(fd, backlog.clamp(1, 128)) } < 0 {
        return fail();
    }
    0
}

pub extern "system" fn accept_(s: Socket, addr: *mut u8, len: *mut i32) -> Socket {
    let Some(fd) = fd_of(s) else {
        return INVALID_SOCKET;
    };
    let mut b = [0u8; 28];
    let mut n = 28u32;
    // SAFETY: a socket of ours and a buffer of `n` bytes.
    let client = unsafe { accept(fd, b.as_mut_ptr(), &mut n) };
    if client < 0 {
        fail();
        return INVALID_SOCKET;
    }
    from_bsd(&b, addr, len);
    adopt(client)
}

pub extern "system" fn getsockname_(s: Socket, addr: *mut u8, len: *mut i32) -> i32 {
    name_of(s, addr, len, false)
}

pub extern "system" fn getpeername_(s: Socket, addr: *mut u8, len: *mut i32) -> i32 {
    name_of(s, addr, len, true)
}

fn name_of(s: Socket, addr: *mut u8, len: *mut i32, peer: bool) -> i32 {
    let Some(fd) = fd_of(s) else {
        return SOCKET_ERROR;
    };
    if addr.is_null() || len.is_null() {
        return fail_with(WSAEFAULT);
    }
    let mut b = [0u8; 28];
    let mut n = 28u32;
    // SAFETY: a socket of ours and a buffer of `n` bytes.
    let r = unsafe {
        if peer {
            getpeername(fd, b.as_mut_ptr(), &mut n)
        } else {
            getsockname(fd, b.as_mut_ptr(), &mut n)
        }
    };
    if r < 0 {
        return fail();
    }
    from_bsd(&b, addr, len);
    0
}

pub extern "system" fn shutdown_(s: Socket, how: i32) -> i32 {
    let Some(fd) = fd_of(s) else {
        return SOCKET_ERROR;
    };
    // SAFETY: a socket of ours; SD_RECEIVE, SD_SEND, SD_BOTH are 0, 1, 2.
    if unsafe { shutdown(fd, how) } < 0 {
        return fail();
    }
    0
}

// Data.

/// `MSG_*` of Windows as macOS's: `MSG_WAITALL` is the one that differs.
fn bsd_msg_flags(flags: i32) -> c_int {
    const MSG_OOB: i32 = 1;
    const MSG_PEEK: i32 = 2;
    const MSG_WAITALL: i32 = 8;
    let mut out = 0;
    if flags & MSG_OOB != 0 {
        out |= 1;
    }
    if flags & MSG_PEEK != 0 {
        out |= 2;
    }
    if flags & MSG_WAITALL != 0 {
        out |= 0x40;
    }
    out
}

fn transferred(n: isize) -> i32 {
    if n < 0 { fail() } else { n as i32 }
}

fn validate_buf(buf: *const u8, len: i32) -> Result<usize, i32> {
    if len < 0 {
        return Err(fail_with(WSAEINVAL));
    }
    if len > 0 && buf.is_null() {
        return Err(fail_with(WSAEFAULT));
    }
    Ok(len as usize)
}

pub extern "system" fn send_(s: Socket, buf: *const u8, len: i32, flags: i32) -> i32 {
    let Some(fd) = fd_of(s) else {
        return SOCKET_ERROR;
    };
    let Ok(len) = validate_buf(buf, len) else {
        return SOCKET_ERROR;
    };
    // SAFETY: a socket of ours and the title's buffer.
    transferred(unsafe { send(fd, buf.cast(), len, bsd_msg_flags(flags)) })
}

pub extern "system" fn recv_(s: Socket, buf: *mut u8, len: i32, flags: i32) -> i32 {
    recvfrom_(s, buf, len, flags, std::ptr::null_mut(), std::ptr::null_mut())
}

pub extern "system" fn sendto_(
    s: Socket,
    buf: *const u8,
    len: i32,
    flags: i32,
    to: *const u8,
    to_len: i32,
) -> i32 {
    let Some(fd) = fd_of(s) else {
        return SOCKET_ERROR;
    };
    let Ok(len) = validate_buf(buf, len) else {
        return SOCKET_ERROR;
    };
    if to.is_null() {
        // Connected sockets may call sendto with a null destination address.
        // SAFETY: a socket of ours and the title's buffer.
        return transferred(unsafe { send(fd, buf.cast(), len, bsd_msg_flags(flags)) });
    }
    let (b, n) = match to_bsd(to, to_len) {
        Some((b, n)) if reachable(&b) => (b, n),
        Some(_) => return SOCKET_ERROR,
        None => return fail_with(WSAEAFNOSUPPORT),
    };
    // SAFETY: a socket of ours, the title's buffer and a built address.
    transferred(unsafe { sendto(fd, buf.cast(), len, bsd_msg_flags(flags), b.as_ptr(), n) })
}

pub extern "system" fn recvfrom_(
    s: Socket,
    buf: *mut u8,
    len: i32,
    flags: i32,
    from: *mut u8,
    from_len: *mut i32,
) -> i32 {
    let Some(fd) = fd_of(s) else {
        return SOCKET_ERROR;
    };
    let Ok(len) = validate_buf(buf.cast_const(), len) else {
        return SOCKET_ERROR;
    };
    let mut b = [0u8; 28];
    let mut iov = libc::iovec { iov_base: buf.cast(), iov_len: len };
    let mut msg = libc::msghdr {
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: &mut iov,
        msg_iovlen: 1,
        msg_control: std::ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    };
    if !from.is_null() && !from_len.is_null() {
        msg.msg_name = b.as_mut_ptr().cast();
        msg.msg_namelen = b.len() as u32;
    }
    // SAFETY: iov describes the title's writable buffer. The header, iov and address
    // all live through the call.
    let r = unsafe { libc::recvmsg(fd, &mut msg, bsd_msg_flags(flags)) };
    if r < 0 {
        return fail();
    }
    if msg.msg_namelen > 0 {
        from_bsd(&b, from, from_len);
    }
    if msg.msg_flags & libc::MSG_TRUNC != 0 {
        return fail_with(WSAEMSGSIZE);
    }
    r as i32
}

// Control.

pub extern "system" fn ioctlsocket(s: Socket, command: u32, argument: *mut u32) -> i32 {
    let Some(fd) = fd_of(s) else {
        return SOCKET_ERROR;
    };
    if argument.is_null() {
        return fail_with(WSAEFAULT);
    }
    const F_GETFL: c_int = 3;
    const F_SETFL: c_int = 4;
    const O_NONBLOCK: c_int = 4;
    match command {
        FIONBIO => {
            // SAFETY: a descriptor of ours; the argument is a u_long.
            let (on, flags) = unsafe { (argument.read() != 0, fcntl(fd, F_GETFL)) };
            let flags = if on { flags | O_NONBLOCK } else { flags & !O_NONBLOCK };
            // SAFETY: as above.
            if flags < 0 || unsafe { fcntl(fd, F_SETFL, flags) } < 0 {
                return fail();
            }
            0
        }
        FIONREAD => {
            let mut n: c_int = 0;
            // SAFETY: a descriptor of ours; FIONREAD writes an int.
            if unsafe { ioctl(fd, FIONREAD as c_ulong, &raw mut n) } < 0 {
                return fail();
            }
            // SAFETY: the title's out parameter.
            unsafe { argument.write(n as u32) };
            0
        }
        _ => fail_with(WSAEINVAL),
    }
}

/// `WSAIoctl`, for what `ioctlsocket` does.
pub extern "system" fn WSAIoctl(
    s: Socket,
    code: u32,
    input: *const u8,
    input_len: u32,
    output: *mut u8,
    output_len: u32,
    returned: *mut u32,
    _overlapped: *mut c_void,
    _completion: *mut c_void,
) -> i32 {
    if !returned.is_null() {
        // SAFETY: the title's out parameter.
        unsafe { returned.write(0) };
    }
    if fd_of(s).is_none() {
        return SOCKET_ERROR;
    }
    match code {
        FIONBIO if input_len >= 4 && !input.is_null() => {
            let mut v = unsafe { input.cast::<u32>().read_unaligned() };
            ioctlsocket(s, code, &mut v)
        }
        FIONREAD if output_len >= 4 && !output.is_null() => {
            let mut v = 0u32;
            let r = ioctlsocket(s, code, &mut v);
            if r == 0 {
                // SAFETY: the title's output buffer, four bytes at least.
                unsafe { output.cast::<u32>().write_unaligned(v) };
                if !returned.is_null() {
                    // SAFETY: the title's out parameter.
                    unsafe { returned.write(4) };
                }
            }
            r
        }
        _ => fail_with(WSAEOPNOTSUPP),
    }
}

const SOL_SOCKET_WIN: i32 = 0xFFFF;
const IPPROTO_TCP: i32 = 6;

/// An option of Windows as `(level, name)` on macOS, if it exists there.
fn bsd_option(level: i32, name: i32) -> Option<(c_int, c_int)> {
    match (level, name) {
        // The BSD socket options share their numbers with Winsock's, but
        // for the two Windows has and macOS does not.
        (SOL_SOCKET_WIN, 0x1..=0x100 | 0x1001 | 0x1002 | 0x1005 | 0x1006 | 0x1007 | 0x1008) => {
            Some((0xFFFF, name))
        }
        // IP and IPv6 options: TOS, TTL, multicast, V6ONLY keep their numbers
        // except the don't-fragment flag.
        (0, 14) => Some((0, 28)),
        (0 | 41, _) => Some((level, name)),
        // TCP_NODELAY; TCP_KEEPALIVE is 3 on Windows, 0x10 on macOS.
        (IPPROTO_TCP, 1) => Some((6, 1)),
        (IPPROTO_TCP, 3) => Some((6, 0x10)),
        _ => None,
    }
}

/// `SO_EXCLUSIVEADDRUSE` (`~SO_REUSEADDR`): macOS has no such thing, and
/// refusing the reuse is what a title asks for with it, which is the default.
const SO_EXCLUSIVEADDRUSE: i32 = !4;

pub extern "system" fn setsockopt_(
    s: Socket,
    level: i32,
    name: i32,
    value: *const u8,
    len: i32,
) -> i32 {
    let Some(fd) = fd_of(s) else {
        return SOCKET_ERROR;
    };
    if value.is_null() || len < 0 {
        return fail_with(WSAEFAULT);
    }
    if level == SOL_SOCKET_WIN && name == SO_EXCLUSIVEADDRUSE {
        return 0;
    }
    let Some((level, name)) = bsd_option(level, name) else {
        return fail_with(WSAEINVAL);
    };
    // SAFETY: the title passes `len` readable bytes.
    let given = unsafe { std::slice::from_raw_parts(value, len as usize) };
    let r = match (level, name) {
        // SO_LINGER: two u_shorts on Windows, two ints here.
        (0xFFFF, 0x80) => {
            if given.len() < 4 {
                return fail_with(WSAEFAULT);
            }
            let on = i32::from(u16::from_le_bytes([given[0], given[1]]));
            let secs = i32::from(u16::from_le_bytes([given[2], given[3]]));
            let linger = [on, secs];
            // SAFETY: our descriptor, an option of two ints.
            unsafe { setsockopt(fd, level, name, linger.as_ptr().cast(), 8) }
        }
        // Timeouts: milliseconds on Windows, a timeval here.
        (0xFFFF, 0x1005 | 0x1006) => {
            if given.len() < 4 {
                return fail_with(WSAEFAULT);
            }
            let ms = u32::from_le_bytes([given[0], given[1], given[2], given[3]]);
            let tv = BsdTimeval {
                sec: i64::from(ms / 1000),
                usec: (ms % 1000 * 1000) as i32,
            };
            // SAFETY: our descriptor, an option of a timeval.
            unsafe { setsockopt(fd, level, name, (&raw const tv).cast(), 16) }
        }
        // Booleans and sizes are a BOOL or DWORD on Windows, an int here;
        // a byte-sized value (some titles pass one) is widened.
        _ => {
            if given.is_empty() {
                return fail_with(WSAEFAULT);
            }
            let mut word = [0u8; 4];
            let n = given.len().min(4);
            word[..n].copy_from_slice(&given[..n]);
            let v = i32::from_le_bytes(word);
            // SAFETY: our descriptor, an option of one int.
            unsafe { setsockopt(fd, level, name, (&raw const v).cast(), 4) }
        }
    };
    if r < 0 { fail() } else { 0 }
}

pub extern "system" fn getsockopt_(
    s: Socket,
    level: i32,
    name: i32,
    value: *mut u8,
    len: *mut i32,
) -> i32 {
    let Some(fd) = fd_of(s) else {
        return SOCKET_ERROR;
    };
    if value.is_null() || len.is_null() {
        return fail_with(WSAEFAULT);
    }
    let Some((level, name)) = bsd_option(level, name) else {
        return fail_with(WSAEINVAL);
    };
    // SAFETY: the title's buffer and its length.
    let room = unsafe { len.read() }.max(0) as usize;
    let mut buf = [0u8; 16];
    let mut n = 16u32;
    // SAFETY: our descriptor and a buffer of `n` bytes.
    if unsafe { getsockopt(fd, level, name, buf.as_mut_ptr().cast(), &mut n) } < 0 {
        return fail();
    }
    let mut out = buf[..n as usize].to_vec();
    match (level, name) {
        // SO_ERROR as a WSAE code.
        (0xFFFF, 0x1007) => {
            let e = i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            out = (if e == 0 { 0 } else { wsa_from_errno(e) }).to_le_bytes().to_vec();
        }
        (0xFFFF, 0x80) if n >= 8 => {
            let on = u16::from_le_bytes([buf[0], buf[1]]);
            let secs = u16::from_le_bytes([buf[4], buf[5]]);
            out = [on.to_le_bytes(), secs.to_le_bytes()].concat();
        }
        (0xFFFF, 0x1005 | 0x1006) if n >= 16 => {
            let sec = i64::from_le_bytes(buf[0..8].try_into().unwrap());
            let usec = i32::from_le_bytes(buf[8..12].try_into().unwrap());
            out = ((sec * 1000 + i64::from(usec) / 1000) as u32).to_le_bytes().to_vec();
        }
        _ => {}
    }
    let copy = out.len().min(room);
    // SAFETY: the title's buffer holds `room` bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(out.as_ptr(), value, copy);
        len.write(copy as i32);
    }
    0
}

// Readiness.

/// A Windows `fd_set`: a count, then that many `SOCKET`s (64 at most).
#[repr(C)]
pub(super) struct WinFdSet {
    count: u32,
    sockets: [Socket; WIN_FD_SETSIZE],
}

#[repr(C)]
pub(super) struct WinTimeval {
    sec: i32,
    usec: i32,
}

enum FillError {
    NotSock,
    HighFd,
}

/// The descriptors of a Windows set, set in `bsd`; the highest descriptor
/// plus one. Fails on a socket that is not ours, or reports `HighFd` when a
/// descriptor exceeds BSD `FD_SETSIZE` (1024).
fn fill(set: *const WinFdSet, bsd: &mut [u32; BSD_FD_WORDS], max: &mut c_int) -> Result<(), FillError> {
    if set.is_null() {
        return Ok(());
    }
    // SAFETY: the title passes an fd_set.
    let set = unsafe { &*set };
    for &s in &set.sockets[..(set.count as usize).min(WIN_FD_SETSIZE)] {
        let Some(fd) = lock(&SOCKETS).get(&s).copied() else {
            return Err(FillError::NotSock);
        };
        if fd < 0 || fd as usize >= BSD_FD_WORDS * 32 {
            return Err(FillError::HighFd);
        }
        bsd[fd as usize / 32] |= 1 << (fd % 32);
        *max = (*max).max(fd + 1);
    }
    Ok(())
}

/// Keeps in the Windows set only the sockets set in `bsd`.
fn keep(set: *mut WinFdSet, bsd: &[u32; BSD_FD_WORDS]) -> i32 {
    if set.is_null() {
        return 0;
    }
    // SAFETY: the title passes an fd_set.
    let set = unsafe { &mut *set };
    let mut kept = 0;
    for i in 0..(set.count as usize).min(WIN_FD_SETSIZE) {
        let s = set.sockets[i];
        if let Some(&fd) = lock(&SOCKETS).get(&s)
            && fd >= 0
            && (fd as usize) < BSD_FD_WORDS * 32
            && bsd[fd as usize / 32] >> (fd % 32) & 1 != 0
        {
            set.sockets[kept] = s;
            kept += 1;
        }
    }
    set.count = kept as u32;
    kept as i32
}

/// Fallback for `select_` when a socket descriptor is `>= 1024` (past BSD `FD_SETSIZE`).
fn select_via_poll(
    read: *mut WinFdSet,
    write: *mut WinFdSet,
    except: *mut WinFdSet,
    timeout: *const WinTimeval,
) -> i32 {
    const POLLIN: i16 = 0x0001;
    const POLLPRI: i16 = 0x0002;
    const POLLOUT: i16 = 0x0004;
    const POLLERR: i16 = 0x0008;
    const POLLHUP: i16 = 0x0010;
    let mut fds: Vec<BsdPollFd> = Vec::new();
    let mut index: HashMap<c_int, usize> = HashMap::new();
    for (set, ev) in [(read, POLLIN), (write, POLLOUT), (except, POLLPRI)] {
        if set.is_null() {
            continue;
        }
        // SAFETY: the title passes an fd_set.
        let set = unsafe { &*set };
        for &s in &set.sockets[..(set.count as usize).min(WIN_FD_SETSIZE)] {
            let Some(fd) = lock(&SOCKETS).get(&s).copied() else {
                return fail_with(WSAENOTSOCK);
            };
            if let Some(&pos) = index.get(&fd) {
                fds[pos].events |= ev;
            } else {
                index.insert(fd, fds.len());
                fds.push(BsdPollFd { fd, events: ev, revents: 0 });
            }
        }
    }
    let timeout_ms = if timeout.is_null() {
        -1
    } else {
        // SAFETY: the title passes a timeval.
        let t = unsafe { &*timeout };
        if t.sec < 0 || t.usec < 0 {
            return fail_with(WSAEINVAL);
        }
        let ms = i64::from(t.sec)
            .saturating_mul(1000)
            .saturating_add(i64::from(t.usec).saturating_add(999) / 1000);
        ms.clamp(0, i64::from(i32::MAX)) as c_int
    };
    // SAFETY: `fds.len()` valid poll descriptors.
    let n = unsafe { poll(fds.as_mut_ptr(), fds.len() as u32, timeout_ms) };
    if n < 0 {
        return fail();
    }
    let mut ready = 0i32;
    for (set, mask) in [
        (read, POLLIN | POLLHUP | POLLERR),
        (write, POLLOUT | POLLERR),
        (except, POLLPRI | POLLERR),
    ] {
        if set.is_null() {
            continue;
        }
        // SAFETY: the title passes an fd_set.
        let set = unsafe { &mut *set };
        let mut kept = 0;
        for i in 0..(set.count as usize).min(WIN_FD_SETSIZE) {
            let s = set.sockets[i];
            if let Some(&fd) = lock(&SOCKETS).get(&s)
                && let Some(&pos) = index.get(&fd)
                && fds[pos].revents & mask != 0
            {
                set.sockets[kept] = s;
                kept += 1;
            }
        }
        set.count = kept as u32;
        ready += kept as i32;
    }
    ready
}

pub extern "system" fn select_(
    _nfds: i32,
    read: *mut WinFdSet,
    write: *mut WinFdSet,
    except: *mut WinFdSet,
    timeout: *const WinTimeval,
) -> i32 {
    if !started() {
        return SOCKET_ERROR;
    }
    let mut sets = [[0u32; BSD_FD_WORDS]; 3];
    let mut max = 0;
    for (set, bsd) in [read, write, except].into_iter().zip(&mut sets) {
        match fill(set, bsd, &mut max) {
            Ok(()) => {}
            Err(FillError::NotSock) => return fail_with(WSAENOTSOCK),
            Err(FillError::HighFd) => return select_via_poll(read, write, except, timeout),
        }
    }
    let mut tv = BsdTimeval { sec: 0, usec: 0 };
    let tv_ptr = if timeout.is_null() {
        std::ptr::null_mut()
    } else {
        // SAFETY: the title passes a timeval.
        let t = unsafe { &*timeout };
        tv.sec = i64::from(t.sec);
        tv.usec = t.usec;
        &raw mut tv
    };
    let [r, w, e] = &mut sets;
    // SAFETY: three sets of 1024 bits and a timeval.
    let n = unsafe { select(max, r.as_mut_ptr(), w.as_mut_ptr(), e.as_mut_ptr(), tv_ptr) };
    if n < 0 {
        return fail();
    }
    keep(read, r) + keep(write, w) + keep(except, e)
}

pub extern "system" fn __WSAFDIsSet(s: Socket, set: *const WinFdSet) -> i32 {
    if set.is_null() {
        return 0;
    }
    // SAFETY: the title passes an fd_set.
    let set = unsafe { &*set };
    i32::from(set.sockets[..(set.count as usize).min(WIN_FD_SETSIZE)].contains(&s))
}

pub extern "system" fn WSAPoll(fds: *mut u8, count: u32, timeout: i32) -> i32 {
    if !started() {
        return SOCKET_ERROR;
    }
    if fds.is_null() && count > 0 {
        return fail_with(WSAEFAULT);
    }
    // WSAPOLLFD: a SOCKET, then events and revents as shorts (16 bytes).
    let mut list = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        // SAFETY: the title passes `count` WSAPOLLFDs.
        let (s, events) = unsafe {
            let p = fds.add(i * 16);
            (p.cast::<usize>().read_unaligned(), p.add(8).cast::<i16>().read_unaligned())
        };
        let mut bsd = 0i16;
        if events & 0x300 != 0 {
            bsd |= 1;
        }
        if events & 0x30 != 0 {
            bsd |= 4;
        }
        let fd = lock(&SOCKETS).get(&s).copied().unwrap_or(-1);
        list.push(BsdPollFd { fd, events: bsd, revents: 0 });
    }
    // SAFETY: `count` descriptors.
    let n = unsafe { poll(list.as_mut_ptr(), count, timeout) };
    if n < 0 {
        return fail();
    }
    let mut ready = 0i32;
    for (i, p) in list.iter().enumerate() {
        let mut revents = 0i16;
        if p.fd < 0 || p.revents & 0x20 != 0 {
            revents |= 4;
        }
        if p.revents & 1 != 0 {
            revents |= 0x100;
        }
        if p.revents & 4 != 0 {
            revents |= 0x10;
        }
        if p.revents & 8 != 0 {
            revents |= 1;
        }
        if p.revents & 0x10 != 0 {
            revents |= 2;
        }
        if revents != 0 {
            ready += 1;
        }
        // SAFETY: as above.
        unsafe { fds.add(i * 16 + 10).cast::<i16>().write_unaligned(revents) };
    }
    ready
}

// Byte order and address text.

pub extern "system" fn htons(v: u16) -> u16 {
    v.to_be()
}

pub extern "system" fn ntohs(v: u16) -> u16 {
    u16::from_be(v)
}

pub extern "system" fn htonl(v: u32) -> u32 {
    v.to_be()
}

pub extern "system" fn ntohl(v: u32) -> u32 {
    u32::from_be(v)
}

pub extern "system" fn inet_addr_(text: *const c_char) -> u32 {
    if text.is_null() {
        return u32::MAX;
    }
    // SAFETY: the title passes a NUL-terminated string.
    unsafe { inet_addr(text) }
}

pub extern "system" fn inet_ntoa(addr: u32) -> *const c_char {
    thread_local! {
        static TEXT: RefCell<[u8; 16]> = const { RefCell::new([0; 16]) };
    }
    let text = Ipv4Addr::from(u32::from_be(addr)).to_string();
    TEXT.with(|t| {
        let mut t = t.borrow_mut();
        t.fill(0);
        t[..text.len()].copy_from_slice(text.as_bytes());
        t.as_ptr().cast()
    })
}

pub extern "system" fn inet_pton(family: i32, text: *const c_char, out: *mut u8) -> i32 {
    if text.is_null() || out.is_null() {
        return fail_with(WSAEFAULT);
    }
    // SAFETY: the title passes a NUL-terminated string.
    let Ok(text) = unsafe { CStr::from_ptr(text) }.to_str() else {
        return 0;
    };
    let bytes: Vec<u8> = match family {
        2 => match text.parse::<Ipv4Addr>() {
            Ok(a) => a.octets().to_vec(),
            Err(_) => return 0,
        },
        23 => match text.parse::<Ipv6Addr>() {
            Ok(a) => a.octets().to_vec(),
            Err(_) => return 0,
        },
        _ => {
            set_wsa_error(WSAEAFNOSUPPORT);
            return -1;
        }
    };
    // SAFETY: the title's buffer holds an address of that family.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, bytes.len()) };
    1
}

pub extern "system" fn inet_ntop(
    family: i32,
    addr: *const u8,
    out: *mut u8,
    size: usize,
) -> *const c_char {
    if addr.is_null() || out.is_null() {
        set_wsa_error(WSAEFAULT);
        return std::ptr::null();
    }
    // SAFETY: the title's address of that family.
    let text = unsafe {
        match family {
            2 => Ipv4Addr::from(<[u8; 4]>::try_from(std::slice::from_raw_parts(addr, 4)).unwrap())
                .to_string(),
            23 => {
                Ipv6Addr::from(<[u8; 16]>::try_from(std::slice::from_raw_parts(addr, 16)).unwrap())
                    .to_string()
            }
            _ => {
                set_wsa_error(WSAEAFNOSUPPORT);
                return std::ptr::null();
            }
        }
    };
    if text.len() + 1 > size {
        set_wsa_error(WSA_NOT_ENOUGH_MEMORY);
        return std::ptr::null();
    }
    // SAFETY: the title's buffer holds `size` bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len());
        out.add(text.len()).write(0);
    }
    out.cast()
}

// Names.

fn text_of(p: *const c_char) -> Option<String> {
    // SAFETY: the title passes a NUL-terminated string or null.
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// Names answered without asking the network: none, `localhost` and IP
/// literals (connecting to one is vetted separately).
fn resolvable(name: &Option<String>) -> bool {
    match name {
        Some(n) if !(n.is_empty() || n.eq_ignore_ascii_case("localhost") || n.parse::<IpAddr>().is_ok()) => {
            tracing::info!(name = %n, "network name lookup blocked");
            false
        }
        _ => true,
    }
}

/// One `ADDRINFOA` node of Windows (48 bytes), its address (32) and, for
/// the first, the canonical name (256), in one allocation.
const NODE_SIZE: usize = 48 + 32 + 256;

fn node_layout() -> std::alloc::Layout {
    std::alloc::Layout::from_size_align(NODE_SIZE, 8).unwrap()
}

/// The `EAI_*` code as Windows's own error (which `getaddrinfo` returns).
fn gai_error(code: c_int) -> i32 {
    match code {
        2 => WSATRY_AGAIN,
        3 => WSAEINVAL,
        4 => WSANO_RECOVERY,
        5 => WSAEAFNOSUPPORT,
        6 => WSA_NOT_ENOUGH_MEMORY,
        9 => WSATYPE_NOT_FOUND,
        10 => WSAESOCKTNOSUPPORT,
        _ => WSAHOST_NOT_FOUND,
    }
}

pub extern "system" fn getaddrinfo_(
    node: *const c_char,
    service: *const c_char,
    hints: *const u8,
    result: *mut *mut u8,
) -> i32 {
    if !started() {
        return WSANOTINITIALISED;
    }
    if result.is_null() {
        set_wsa_error(WSAEINVAL);
        return WSAEINVAL;
    }
    // SAFETY: the title's out parameter; cleared on every error path.
    unsafe { result.write(std::ptr::null_mut()) };
    let name = text_of(node);
    if !resolvable(&name) {
        set_wsa_error(WSAHOST_NOT_FOUND);
        return WSAHOST_NOT_FOUND;
    }
    let mut wanted = BsdAddrInfo {
        flags: 0,
        family: 0,
        socktype: 0,
        protocol: 0,
        addr_len: 0,
        canon_name: std::ptr::null_mut(),
        addr: std::ptr::null_mut(),
        next: std::ptr::null_mut(),
    };
    if !hints.is_null() {
        // SAFETY: the title passes an ADDRINFOA.
        let h = unsafe { std::slice::from_raw_parts(hints, 16) };
        let word = |i: usize| i32::from_le_bytes(h[i..i + 4].try_into().unwrap());
        // Windows AI_NUMERICSERV is 8; macOS's is 0x1000. The rest agree.
        let flags = word(0);
        wanted.flags = (flags & 0x507) | if flags & 8 != 0 { 0x1000 } else { 0 };
        let Some(family) = bsd_family(word(4)) else {
            set_wsa_error(WSAEAFNOSUPPORT);
            return WSAEAFNOSUPPORT;
        };
        wanted.family = family;
        wanted.socktype = word(8);
        wanted.protocol = word(12);
    }
    let mut list: *mut BsdAddrInfo = std::ptr::null_mut();
    let cname = name.as_deref().map(|n| std::ffi::CString::new(n).unwrap_or_default());
    let cservice = text_of(service).map(|n| std::ffi::CString::new(n).unwrap_or_default());
    // SAFETY: NUL-terminated strings or null, our hints, a result slot.
    let code = unsafe {
        getaddrinfo(
            cname.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            cservice.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            &wanted,
            &mut list,
        )
    };
    if code != 0 {
        set_wsa_error(gai_error(code));
        return gai_error(code);
    }
    let mut head: *mut u8 = std::ptr::null_mut();
    let mut tail: *mut u8 = std::ptr::null_mut();
    let mut p = list;
    while !p.is_null() {
        // SAFETY: a node of the system's list.
        let a = unsafe { &*p };
        p = a.next;
        // SAFETY: a non-zero size.
        let node = unsafe { std::alloc::alloc_zeroed(node_layout()) };
        if node.is_null() {
            break;
        }
        // SAFETY: `a.addr` is `addr_len` bytes; `node` is NODE_SIZE bytes.
        unsafe {
            let sa = std::slice::from_raw_parts(a.addr, a.addr_len as usize);
            let family = if a.family == BSD_AF_INET6 { 23 } else { a.family };
            node.cast::<i32>().write(a.flags & 0x7);
            node.add(4).cast::<i32>().write(family);
            node.add(8).cast::<i32>().write(a.socktype);
            node.add(12).cast::<i32>().write(a.protocol);
            let mut len = 0i32;
            from_bsd_into(sa, node.add(48), &mut len);
            node.add(16).cast::<usize>().write(len as usize);
            node.add(32).cast::<*mut u8>().write(node.add(48));
            if !a.canon_name.is_null() && head.is_null() {
                let c = CStr::from_ptr(a.canon_name).to_bytes();
                let n = c.len().min(255);
                std::ptr::copy_nonoverlapping(c.as_ptr(), node.add(80), n);
                node.add(24).cast::<*mut u8>().write(node.add(80));
            }
            if tail.is_null() {
                head = node;
            } else {
                tail.add(40).cast::<*mut u8>().write(node);
            }
        }
        tail = node;
    }
    // SAFETY: the list `getaddrinfo` made.
    unsafe { freeaddrinfo(list) };
    // SAFETY: the title's out parameter.
    unsafe { result.write(head) };
    if head.is_null() {
        set_wsa_error(WSAHOST_NOT_FOUND);
        return WSAHOST_NOT_FOUND;
    }
    0
}

/// [`from_bsd`] into a buffer of at least 28 bytes.
fn from_bsd_into(b: &[u8], out: *mut u8, len: &mut i32) {
    let mut room = 28i32;
    let mut padded = [0u8; 28];
    padded[..b.len().min(28)].copy_from_slice(&b[..b.len().min(28)]);
    from_bsd(&padded, out, &mut room);
    *len = room;
}

pub extern "system" fn freeaddrinfo_(mut node: *mut u8) {
    while !node.is_null() {
        // SAFETY: a node `getaddrinfo_` allocated, freed once.
        unsafe {
            let next = node.add(40).cast::<*mut u8>().read();
            std::alloc::dealloc(node, node_layout());
            node = next;
        }
    }
}

pub extern "system" fn getnameinfo_(
    addr: *const u8,
    len: i32,
    host: *mut u8,
    host_len: u32,
    service: *mut u8,
    service_len: u32,
    flags: i32,
) -> i32 {
    if !started() {
        return WSANOTINITIALISED;
    }
    let Some((b, _)) = to_bsd(addr, len) else {
        set_wsa_error(WSAEAFNOSUPPORT);
        return WSAEAFNOSUPPORT;
    };
    let ip = ip_of(&b);
    let port = u16::from_be_bytes([b[2], b[3]]);
    // Numeric only: a name would be a lookup. Loopback has one.
    let name = match ip {
        Some(ip) if loopback(ip) && flags & 2 == 0 => "localhost".to_owned(),
        Some(ip) if flags & NI_NAMEREQD == 0 || flags & 2 != 0 => ip.to_string(),
        _ => {
            set_wsa_error(WSAHOST_NOT_FOUND);
            return WSAHOST_NOT_FOUND;
        }
    };
    let put = |out: *mut u8, room: u32, text: &str| -> bool {
        if out.is_null() {
            return true;
        }
        if text.len() + 1 > room as usize {
            return false;
        }
        // SAFETY: the title's buffer holds `room` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len());
            out.add(text.len()).write(0);
        }
        true
    };
    if !put(host, host_len, &name) || !put(service, service_len, &port.to_string()) {
        set_wsa_error(WSA_NOT_ENOUGH_MEMORY);
        return WSA_NOT_ENOUGH_MEMORY;
    }
    0
}

pub extern "system" fn gethostname_(buf: *mut u8, len: i32) -> i32 {
    if !started() {
        return SOCKET_ERROR;
    }
    if buf.is_null() || len <= 0 {
        return fail_with(WSAEFAULT);
    }
    // SAFETY: the title's buffer of `len` bytes.
    if unsafe { gethostname(buf.cast(), len as usize) } < 0 {
        return fail_with(WSAEFAULT);
    }
    // SAFETY: as above; a truncated name is still terminated.
    unsafe { buf.add(len as usize - 1).write(0) };
    0
}

/// A `hostent` (x64: name, aliases, type and length as shorts, address
/// list) with its storage, one per thread, as Winsock keeps it.
#[repr(C)]
pub(super) struct HostEnt {
    name: *const u8,
    aliases: *const *const u8,
    kind: i16,
    length: i16,
    addresses: *const *const u8,
    text: [u8; 64],
    alias_list: [*const u8; 1],
    address_list: [*const u8; 2],
    address: [u8; 4],
}

pub extern "system" fn gethostbyname_(name: *const c_char) -> *const HostEnt {
    thread_local! {
        static ENTRY: RefCell<Box<HostEnt>> = RefCell::new(Box::new(HostEnt {
            name: std::ptr::null(),
            aliases: std::ptr::null(),
            kind: 2,
            length: 4,
            addresses: std::ptr::null(),
            text: [0; 64],
            alias_list: [std::ptr::null()],
            address_list: [std::ptr::null(); 2],
            address: [0; 4],
        }));
    }
    if !started() {
        return std::ptr::null();
    }
    let name = text_of(name);
    if !resolvable(&name) {
        set_wsa_error(WSAHOST_NOT_FOUND);
        return std::ptr::null();
    }
    let ip = match name.as_deref() {
        None | Some("") => None,
        Some(n) if n.eq_ignore_ascii_case("localhost") => Some(Ipv4Addr::LOCALHOST),
        Some(n) => n.parse::<Ipv4Addr>().ok(),
    };
    let Some(ip) = ip else {
        set_wsa_error(WSAHOST_NOT_FOUND);
        return std::ptr::null();
    };
    ENTRY.with(|e| {
        let mut e = e.borrow_mut();
        let text = name.unwrap_or_default();
        let n = text.len().min(63);
        e.text = [0; 64];
        e.text[..n].copy_from_slice(&text.as_bytes()[..n]);
        e.address = ip.octets();
        let e = &mut **e;
        e.name = e.text.as_ptr();
        e.alias_list = [std::ptr::null()];
        e.aliases = e.alias_list.as_ptr();
        e.address_list = [e.address.as_ptr(), std::ptr::null()];
        e.addresses = e.address_list.as_ptr();
        // The box lives as long as the thread.
        std::ptr::from_ref(e)
    })
}

/// The function table, by name and by Winsock's ordinal (`#n`).
pub(super) fn lookup(dll: &str, name: &str) -> Option<usize> {
    if dll != "ws2_32.dll" && dll != "wsock32.dll" {
        return None;
    }
    macro_rules! table {
        ($($($key:literal)|+ => $f:expr),* $(,)?) => {
            match name {
                $($($key)|+ => Some($f as *const () as usize),)*
                _ => None,
            }
        };
    }
    table!(
        "WSAStartup" | "#115" => WSAStartup,
        "WSACleanup" | "#116" => WSACleanup,
        "WSAGetLastError" | "#111" => WSAGetLastError,
        "WSASetLastError" | "#112" => WSASetLastError,
        "socket" | "#23" => socket_,
        "closesocket" | "#3" => closesocket,
        "connect" | "#4" => connect_,
        "bind" | "#2" => bind_,
        "listen" | "#13" => listen_,
        "accept" | "#1" => accept_,
        "getsockname" | "#6" => getsockname_,
        "getpeername" | "#5" => getpeername_,
        "shutdown" | "#22" => shutdown_,
        "send" | "#19" => send_,
        "recv" | "#16" => recv_,
        "sendto" | "#20" => sendto_,
        "recvfrom" | "#17" => recvfrom_,
        "ioctlsocket" | "#10" => ioctlsocket,
        "WSAIoctl" => WSAIoctl,
        "setsockopt" | "#21" => setsockopt_,
        "getsockopt" | "#7" => getsockopt_,
        "select" | "#18" => select_,
        "__WSAFDIsSet" | "#151" => __WSAFDIsSet,
        "WSAPoll" => WSAPoll,
        "htons" | "#9" => htons,
        "ntohs" | "#15" => ntohs,
        "htonl" | "#8" => htonl,
        "ntohl" | "#14" => ntohl,
        "inet_addr" | "#11" => inet_addr_,
        "inet_ntoa" | "#12" => inet_ntoa,
        "inet_pton" => inet_pton,
        "inet_ntop" => inet_ntop,
        "getaddrinfo" => getaddrinfo_,
        "freeaddrinfo" => freeaddrinfo_,
        "getnameinfo" => getnameinfo_,
        "gethostname" | "#57" => gethostname_,
        "gethostbyname" | "#52" => gethostbyname_,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The startup count is the process's: the tests that start and stop
    /// Winsock take turns.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static TURN: Mutex<()> = Mutex::new(());
        TURN.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn loopback_addr(port: u16) -> [u8; 16] {
        let mut a = [0u8; 16];
        a[0..2].copy_from_slice(&AF_INET.to_le_bytes());
        a[2..4].copy_from_slice(&port.to_be_bytes());
        a[4..8].copy_from_slice(&[127, 0, 0, 1]);
        a
    }

    fn port_of(s: Socket) -> u16 {
        let mut a = [0u8; 16];
        let mut n = 16;
        assert_eq!(getsockname_(s, a.as_mut_ptr(), &mut n), 0);
        assert_eq!(n, 16);
        u16::from_be_bytes([a[2], a[3]])
    }

    #[test]
    fn a_tcp_echo_over_loopback() {
        let _turn = serial();
        assert_eq!(WSAStartup(0x0202, std::ptr::null_mut()), 0);
        let server = socket_(2, 1, 6);
        assert_ne!(server, INVALID_SOCKET);
        // Bound to the wildcard: narrowed to loopback.
        let mut wild = loopback_addr(0);
        wild[4..8].copy_from_slice(&[0; 4]);
        assert_eq!(bind_(server, wild.as_ptr(), 16), 0);
        assert_eq!(listen_(server, 4), 0);
        let port = port_of(server);
        let mut name = [0u8; 16];
        let mut n = 16;
        assert_eq!(getsockname_(server, name.as_mut_ptr(), &mut n), 0);
        assert_eq!(name[4..8], [127, 0, 0, 1]);

        let client = std::thread::spawn(move || {
            let c = socket_(2, 1, 6);
            assert_eq!(connect_(c, loopback_addr(port).as_ptr(), 16), 0);
            assert_eq!(send_(c, b"hello".as_ptr(), 5, 0), 5);
            let mut buf = [0u8; 16];
            assert_eq!(recv_(c, buf.as_mut_ptr(), 16, 0), 5);
            assert_eq!(&buf[..5], b"HELLO");
            // The server closed: end of stream.
            assert_eq!(recv_(c, buf.as_mut_ptr(), 16, 0), 0);
            assert_eq!(closesocket(c), 0);
        });

        // select sees the pending connection.
        let mut set = WinFdSet { count: 1, sockets: [0; 64] };
        set.sockets[0] = server;
        let tv = WinTimeval { sec: 5, usec: 0 };
        assert_eq!(select_(0, &mut set, std::ptr::null_mut(), std::ptr::null_mut(), &tv), 1);
        assert_eq!(__WSAFDIsSet(server, &set), 1);
        let mut peer = [0u8; 16];
        let mut peer_len = 16;
        let conn = accept_(server, peer.as_mut_ptr(), &mut peer_len);
        assert_ne!(conn, INVALID_SOCKET);
        assert_eq!((peer_len, peer[4..8].to_vec()), (16, vec![127, 0, 0, 1]));
        let mut buf = [0u8; 16];
        assert_eq!(recv_(conn, buf.as_mut_ptr(), 16, 0), 5);
        let upper = buf[..5].to_ascii_uppercase();
        assert_eq!(send_(conn, upper.as_ptr(), 5, 0), 5);
        assert_eq!(closesocket(conn), 0);
        client.join().unwrap();
        assert_eq!(closesocket(server), 0);
        // A closed socket is not one.
        assert_eq!(send_(server, b"x".as_ptr(), 1, 0), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAENOTSOCK);
        assert_eq!(WSACleanup(), 0);
    }

    #[test]
    fn udp_receive_reports_truncation_and_accepts_an_exact_fit() {
        let _turn = serial();
        assert_eq!(WSAStartup(0x0202, std::ptr::null_mut()), 0);
        let server = socket_(2, 2, 17);
        let client = socket_(2, 2, 17);
        assert_eq!(bind_(server, loopback_addr(0).as_ptr(), 16), 0);
        assert_eq!(connect_(client, loopback_addr(port_of(server)).as_ptr(), 16), 0);
        let sender = loopback_addr(port_of(client));
        let mut buf = [0; 8];

        for (packet, expected) in [(b"123456789".as_slice(), SOCKET_ERROR), (b"abcdefgh", 8)] {
            let len = packet.len() as i32;
            assert_eq!(send_(client, packet.as_ptr(), len, 0), len);
            let mut from = [0; 28];
            let mut from_len = 28;
            assert_eq!(
                recvfrom_(server, buf.as_mut_ptr(), 8, 0, from.as_mut_ptr(), &mut from_len),
                expected
            );
            if expected == SOCKET_ERROR {
                assert_eq!(WSAGetLastError(), 10040);
            }
            assert_eq!(&buf, &packet[..8]);
            assert_eq!(from_len, 16);
            assert_eq!(from[..16], sender);
        }

        assert_eq!(send_(client, b"987654321".as_ptr(), 9, 0), 9);
        assert_eq!(recv_(server, buf.as_mut_ptr(), 8, 0), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), 10040);
        assert_eq!(&buf, b"98765432");
        assert_eq!(closesocket(client), 0);
        assert_eq!(closesocket(server), 0);
        assert_eq!(WSACleanup(), 0);
    }

    #[test]
    fn non_blocking_sockets_and_options_use_windows_semantics() {
        let _turn = serial();
        assert_eq!(WSAStartup(0x0202, std::ptr::null_mut()), 0);
        let s = socket_(2, 1, 6);
        let mut on = 1u32;
        assert_eq!(ioctlsocket(s, FIONBIO, &mut on), 0);
        assert_eq!(bind_(s, loopback_addr(0).as_ptr(), 16), 0);
        assert_eq!(listen_(s, 1), 0);
        // Nothing to accept yet.
        assert_eq!(accept_(s, std::ptr::null_mut(), std::ptr::null_mut()), INVALID_SOCKET);
        assert_eq!(WSAGetLastError(), WSAEWOULDBLOCK);
        // A non-blocking connect to a listener: pending, not an error code of BSD's.
        let c = socket_(2, 1, 6);
        assert_eq!(ioctlsocket(c, FIONBIO, &mut on), 0);
        let r = connect_(c, loopback_addr(port_of(s)).as_ptr(), 16);
        assert!(r == 0 || WSAGetLastError() == WSAEWOULDBLOCK);
        // Options: TCP_NODELAY and a receive timeout in milliseconds.
        let one = 1u32.to_le_bytes();
        assert_eq!(setsockopt_(c, IPPROTO_TCP, 1, one.as_ptr(), 4), 0);
        let mut got = [0u8; 4];
        let mut n = 4;
        assert_eq!(getsockopt_(c, IPPROTO_TCP, 1, got.as_mut_ptr(), &mut n), 0);
        assert_ne!(u32::from_le_bytes(got), 0);
        let timeout = 1500u32.to_le_bytes();
        assert_eq!(setsockopt_(c, SOL_SOCKET_WIN, 0x1006, timeout.as_ptr(), 4), 0);
        n = 4;
        assert_eq!(getsockopt_(c, SOL_SOCKET_WIN, 0x1006, got.as_mut_ptr(), &mut n), 0);
        assert_eq!(u32::from_le_bytes(got), 1500);
        // SO_LINGER is two shorts.
        let linger = [1u8, 0, 5, 0];
        assert_eq!(setsockopt_(c, SOL_SOCKET_WIN, 0x80, linger.as_ptr(), 4), 0);
        let mut back = [0u8; 4];
        n = 4;
        assert_eq!(getsockopt_(c, SOL_SOCKET_WIN, 0x80, back.as_mut_ptr(), &mut n), 0);
        assert_eq!(back, linger);
        // SO_ERROR is clear, and an unknown option is WSAEINVAL.
        n = 4;
        assert_eq!(getsockopt_(c, SOL_SOCKET_WIN, 0x1007, got.as_mut_ptr(), &mut n), 0);
        assert_eq!(u32::from_le_bytes(got), 0);
        assert_eq!(setsockopt_(c, SOL_SOCKET_WIN, 0x7777, one.as_ptr(), 4), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAEINVAL);
        closesocket(c);
        closesocket(s);
        assert_eq!(WSACleanup(), 0);
    }

    #[test]
    fn off_machine_traffic_is_refused() {
        let _turn = serial();
        assert_eq!(WSAStartup(0x0101, std::ptr::null_mut()), 0);
        let s = socket_(2, 2, 17);
        let mut far = loopback_addr(53);
        far[4..8].copy_from_slice(&[8, 8, 8, 8]);
        assert_eq!(connect_(s, far.as_ptr(), 16), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAENETUNREACH);
        assert_eq!(sendto_(s, b"x".as_ptr(), 1, 0, far.as_ptr(), 16), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAENETUNREACH);
        let name = c"example.com".as_ptr();
        let mut out = std::ptr::null_mut();
        assert_eq!(getaddrinfo_(name, std::ptr::null(), std::ptr::null(), &mut out), WSAHOST_NOT_FOUND);
        assert!(gethostbyname_(name).is_null());
        closesocket(s);
        assert_eq!(WSACleanup(), 0);
        // Not started again.
        assert_eq!(socket_(2, 1, 6), INVALID_SOCKET);
        assert_eq!(WSAGetLastError(), WSANOTINITIALISED);
    }

    #[test]
    fn names_resolve_with_windows_layouts() {
        let _turn = serial();
        assert_eq!(WSAStartup(0x0202, std::ptr::null_mut()), 0);
        // ADDRINFOA hints: stream, passive, IPv4.
        let mut hints = [0u8; 48];
        hints[0..4].copy_from_slice(&1i32.to_le_bytes());
        hints[4..8].copy_from_slice(&2i32.to_le_bytes());
        hints[8..12].copy_from_slice(&1i32.to_le_bytes());
        let mut list = std::ptr::null_mut();
        assert_eq!(getaddrinfo_(c"localhost".as_ptr(), c"8080".as_ptr(), hints.as_ptr(), &mut list), 0);
        assert!(!list.is_null());
        // SAFETY: the node getaddrinfo_ made.
        unsafe {
            assert_eq!(list.cast::<i32>().read(), 0);
            assert_eq!(list.add(4).cast::<i32>().read(), 2);
            assert_eq!(list.add(8).cast::<i32>().read(), 1);
            assert_eq!(list.add(16).cast::<usize>().read(), 16);
            let sa = list.add(32).cast::<*const u8>().read();
            assert_eq!(std::slice::from_raw_parts(sa, 8), &[2, 0, 0x1F, 0x90, 127, 0, 0, 1]);
        }
        freeaddrinfo_(list);
        // An IPv6 literal.
        hints[4..8].copy_from_slice(&23i32.to_le_bytes());
        assert_eq!(getaddrinfo_(c"::1".as_ptr(), std::ptr::null(), hints.as_ptr(), &mut list), 0);
        // SAFETY: as above.
        unsafe {
            assert_eq!(list.add(4).cast::<i32>().read(), 23);
            assert_eq!(list.add(16).cast::<usize>().read(), 28);
            let sa = list.add(32).cast::<*const u8>().read();
            assert_eq!(sa.cast::<u16>().read(), 23);
            assert_eq!(sa.add(23).read(), 1);
        }
        freeaddrinfo_(list);
        // getnameinfo: numeric host and service.
        let mut host = [0u8; 32];
        let mut serv = [0u8; 8];
        assert_eq!(
            getnameinfo_(loopback_addr(443).as_ptr(), 16, host.as_mut_ptr(), 32, serv.as_mut_ptr(), 8, 2),
            0
        );
        assert_eq!(CStr::from_bytes_until_nul(&host).unwrap(), c"127.0.0.1");
        assert_eq!(CStr::from_bytes_until_nul(&serv).unwrap(), c"443");
        // Text forms.
        let mut a = [0u8; 16];
        assert_eq!(inet_pton(23, c"fe80::1".as_ptr(), a.as_mut_ptr()), 1);
        let mut text = [0u8; 46];
        let shown = inet_ntop(23, a.as_ptr(), text.as_mut_ptr(), 46);
        // SAFETY: inet_ntop wrote into `text`.
        assert_eq!(unsafe { CStr::from_ptr(shown) }, c"fe80::1");
        assert_eq!(inet_pton(2, c"256.1.1.1".as_ptr(), a.as_mut_ptr()), 0);
        assert_eq!(htons(0x1234), 0x3412);
        assert_eq!(ntohl(htonl(0xAABBCCDD)), 0xAABBCCDD);
        assert_eq!(inet_addr_(c"127.0.0.1".as_ptr()), 0x0100_007F);
        // SAFETY: inet_ntoa's thread buffer.
        assert_eq!(unsafe { CStr::from_ptr(inet_ntoa(0x0100_007F)) }, c"127.0.0.1");
        assert_eq!(WSACleanup(), 0);
    }

    #[test]
    fn errors_and_ordinals() {
        let _turn = serial();
        assert_eq!(wsa_from_errno(35), WSAEWOULDBLOCK);
        assert_eq!(wsa_from_errno(54), WSAECONNRESET);
        assert_eq!(wsa_from_errno(61), 10061);
        assert_eq!(wsa_from_errno(48), 10048);
        assert_eq!(WSAStartup(0x0003, std::ptr::null_mut()), WSAVERNOTSUPPORTED);
        // The ordinals a title imports are Wine's ws2_32.spec.
        for (ordinal, name) in [("#4", "connect"), ("#10", "ioctlsocket"), ("#111", "WSAGetLastError"), ("#151", "__WSAFDIsSet")] {
            assert_eq!(lookup("ws2_32.dll", ordinal), lookup("ws2_32.dll", name));
            assert!(lookup("ws2_32.dll", name).is_some());
        }
        assert!(lookup("kernel32.dll", "connect").is_none());
    }

    #[test]
    fn getaddrinfo_getnameinfo_sendto_and_validation_return_wsa_codes() {
        let _turn = serial();
        assert_eq!(WSAStartup(0x0202, std::ptr::null_mut()), 0);

        // getaddrinfo returns the WSA error code directly (not SOCKET_ERROR) and clears *result.
        let mut hints = [0u8; 48];
        hints[4..8].copy_from_slice(&99i32.to_le_bytes());
        let mut out = 0x1234 as *mut u8;
        assert_eq!(
            getaddrinfo_(c"localhost".as_ptr(), std::ptr::null(), hints.as_ptr(), &mut out),
            WSAEAFNOSUPPORT
        );
        assert_eq!(WSAGetLastError(), WSAEAFNOSUPPORT);
        assert!(out.is_null());

        // getnameinfo returns the WSA error code directly (not SOCKET_ERROR).
        let mut small = [0u8; 2];
        assert_eq!(
            getnameinfo_(
                loopback_addr(443).as_ptr(),
                16,
                small.as_mut_ptr(),
                2,
                std::ptr::null_mut(),
                0,
                2
            ),
            WSA_NOT_ENOUGH_MEMORY
        );
        assert_eq!(WSAGetLastError(), WSA_NOT_ENOUGH_MEMORY);
        let mut bad_sa = loopback_addr(443);
        bad_sa[0..2].copy_from_slice(&99u16.to_le_bytes());
        assert_eq!(
            getnameinfo_(bad_sa.as_ptr(), 16, small.as_mut_ptr(), 2, std::ptr::null_mut(), 0, 2),
            WSAEAFNOSUPPORT
        );
        let mut remote = loopback_addr(443);
        remote[4..8].copy_from_slice(&[8, 8, 8, 8]);
        let mut host = [0u8; 32];
        assert_eq!(
            getnameinfo_(
                remote.as_ptr(),
                16,
                host.as_mut_ptr(),
                32,
                std::ptr::null_mut(),
                0,
                NI_NAMEREQD
            ),
            WSAHOST_NOT_FOUND
        );

        // Connected UDP socket accepts sendto with a null destination address.
        let server = socket_(2, 2, 17);
        let client = socket_(2, 2, 17);
        assert_eq!(bind_(server, loopback_addr(0).as_ptr(), 16), 0);
        let port = port_of(server);
        assert_eq!(connect_(client, loopback_addr(port).as_ptr(), 16), 0);
        assert_eq!(sendto_(client, b"ping".as_ptr(), 4, 0, std::ptr::null(), 0), 4);
        let mut buf = [0u8; 8];
        assert_eq!(recv_(server, buf.as_mut_ptr(), 8, 0), 4);
        assert_eq!(&buf[..4], b"ping");

        // Negative buffer lengths and null non-empty buffers fail instead of sending 0 bytes.
        assert_eq!(send_(client, b"x".as_ptr(), -1, 0), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAEINVAL);
        assert_eq!(recv_(server, buf.as_mut_ptr(), -5, 0), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAEINVAL);
        assert_eq!(send_(client, std::ptr::null(), 4, 0), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAEFAULT);

        // Truncated SO_LINGER / timeout options fail with WSAEFAULT instead of falling through.
        let short = [1u8; 2];
        assert_eq!(setsockopt_(client, SOL_SOCKET_WIN, 0x80, short.as_ptr(), 2), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAEFAULT);
        assert_eq!(setsockopt_(client, SOL_SOCKET_WIN, 0x1006, short.as_ptr(), 2), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAEFAULT);
        assert_eq!(setsockopt_(client, IPPROTO_TCP, 1, short.as_ptr(), 0), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAEFAULT);

        // WSAIoctl checks socket validity before reporting WSAEOPNOTSUPP.
        let mut returned = 7u32;
        assert_eq!(
            WSAIoctl(
                INVALID_SOCKET,
                0x1234_5678,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            SOCKET_ERROR
        );
        assert_eq!(WSAGetLastError(), WSAENOTSOCK);

        // select_via_poll handles readiness cleanly for descriptors beyond FD_SETSIZE.
        assert_eq!(sendto_(client, b"ok".as_ptr(), 2, 0, std::ptr::null(), 0), 2);
        let mut read = WinFdSet { count: 1, sockets: [0; 64] };
        read.sockets[0] = server;
        let tv = WinTimeval { sec: 1, usec: 0 };
        assert_eq!(
            select_via_poll(&mut read, std::ptr::null_mut(), std::ptr::null_mut(), &tv),
            1
        );
        assert_eq!(__WSAFDIsSet(server, &read), 1);

        closesocket(client);
        closesocket(server);
        assert_eq!(WSACleanup(), 0);
    }
}
