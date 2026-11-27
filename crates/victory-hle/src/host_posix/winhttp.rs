//! WinHTTP (`winhttp.dll`) with no network, for the native macOS host.
//!
//! Sessions, connections and requests exist as handles, with their options,
//! timeouts, headers and status callbacks, so a title (or `libHttpClient`)
//! sets everything up as it does on Windows. A request never leaves the
//! machine: `WinHttpSendRequest` fails with `ERROR_WINHTTP_CANNOT_CONNECT`,
//! which is what a console with no network does, and everything after it
//! (receive, read, write, query) fails because the request was never sent.
//! Closing a handle delivers `WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING` to the
//! callback it has, as WinHTTP does, so the title can free what it attached.
//!
//! shortcut: a failed send is reported by the call's own return value, not
//! through an asynchronous callback; add `REQUEST_ERROR` delivery if a title
//! is seen waiting for it.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

use victory_loader::native;

use super::{lock, set_error};

type Handle = *mut c_void;

const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_WINHTTP_INVALID_OPTION: u32 = 12009;
const ERROR_WINHTTP_INCORRECT_HANDLE_TYPE: u32 = 12018;
const ERROR_WINHTTP_INCORRECT_HANDLE_STATE: u32 = 12019;
const ERROR_WINHTTP_CANNOT_CONNECT: u32 = 12029;
const ERROR_WINHTTP_INVALID_URL: u32 = 12005;
const ERROR_WINHTTP_UNRECOGNIZED_SCHEME: u32 = 12006;

const WINHTTP_OPTION_CONTEXT_VALUE: u32 = 45;
const WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING: u32 = 0x0000_0800;
/// `WINHTTP_INVALID_STATUS_CALLBACK`, `(WINHTTP_STATUS_CALLBACK)-1`.
const INVALID_STATUS_CALLBACK: usize = usize::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Session,
    Connection,
    Request,
}

#[derive(Clone)]
struct Object {
    kind: Kind,
    parent: usize,
    /// The status callback, the notifications it asked for, and its context.
    callback: usize,
    notifications: u32,
    context: usize,
    options: HashMap<u32, Vec<u8>>,
    timeouts: [i32; 4],
    headers: Vec<u16>,
    server: String,
    port: u16,
}

static OBJECTS: LazyLock<Mutex<HashMap<usize, Object>>> = LazyLock::new(Default::default);
static NEXT: AtomicUsize = AtomicUsize::new(0x1000);

fn wide(text: *const u16, length: u32) -> Option<String> {
    if text.is_null() {
        return None;
    }
    // SAFETY: the title passes a NUL-terminated string, or `length` characters.
    unsafe {
        let n = if length == u32::MAX || length == 0 {
            (0..).take_while(|&i| *text.add(i) != 0).count()
        } else {
            length as usize
        };
        Some(String::from_utf16_lossy(std::slice::from_raw_parts(text, n)))
    }
}

/// A new handle under `parent` (0 for a session), inheriting its callback.
fn create(kind: Kind, parent: usize) -> Handle {
    let mut objects = lock(&OBJECTS);
    let inherited = objects.get(&parent).cloned();
    let (callback, notifications, context) = inherited
        .as_ref()
        .map_or((0, 0, 0), |p| (p.callback, p.notifications, p.context));
    let handle = NEXT.fetch_add(1, Ordering::Relaxed) << 4;
    objects.insert(
        handle,
        Object {
            kind,
            parent,
            callback,
            notifications,
            context,
            options: HashMap::new(),
            timeouts: [0, 60_000, 30_000, 30_000],
            headers: Vec::new(),
            server: String::new(),
            port: 0,
        },
    );
    handle as Handle
}

fn kind_of(handle: Handle) -> Option<Kind> {
    lock(&OBJECTS).get(&(handle as usize)).map(|o| o.kind)
}

/// Fails the call, `FALSE` or null.
fn error(code: u32) -> i32 {
    set_error(code);
    0
}

/// Checks `handle` is an object of `kind`, or sets the error.
fn expect(handle: Handle, kind: Kind) -> bool {
    match kind_of(handle) {
        Some(k) if k == kind => true,
        Some(_) => {
            set_error(ERROR_WINHTTP_INCORRECT_HANDLE_TYPE);
            false
        }
        None => {
            set_error(ERROR_INVALID_HANDLE);
            false
        }
    }
}

pub extern "system" fn WinHttpOpen(
    _agent: *const u16,
    _access: u32,
    _proxy: *const u16,
    _bypass: *const u16,
    _flags: u32,
) -> Handle {
    create(Kind::Session, 0)
}

pub extern "system" fn WinHttpConnect(
    session: Handle,
    server: *const u16,
    port: u16,
    _reserved: u32,
) -> Handle {
    if !expect(session, Kind::Session) {
        return std::ptr::null_mut();
    }
    let Some(name) = wide(server, 0).filter(|n| !n.is_empty()) else {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    };
    let handle = create(Kind::Connection, session as usize);
    if let Some(o) = lock(&OBJECTS).get_mut(&(handle as usize)) {
        o.server = name;
        // INTERNET_DEFAULT_PORT (0) is 80 for HTTP, 443 for HTTPS.
        o.port = port;
    }
    handle
}

pub extern "system" fn WinHttpOpenRequest(
    connection: Handle,
    _verb: *const u16,
    _object: *const u16,
    _version: *const u16,
    _referrer: *const u16,
    _accept: *const *const u16,
    _flags: u32,
) -> Handle {
    if !expect(connection, Kind::Connection) {
        return std::ptr::null_mut();
    }
    create(Kind::Request, connection as usize)
}

pub extern "system" fn WinHttpSendRequest(
    request: Handle,
    _headers: *const u16,
    _headers_len: u32,
    _optional: *const c_void,
    _optional_len: u32,
    _total_len: u32,
    context: usize,
) -> i32 {
    if !expect(request, Kind::Request) {
        return 0;
    }
    if context != 0
        && let Some(o) = lock(&OBJECTS).get_mut(&(request as usize))
    {
        o.context = context;
    }
    let (server, port) = {
        let objects = lock(&OBJECTS);
        let parent = objects.get(&(request as usize)).map_or(0, |o| o.parent);
        objects
            .get(&parent)
            .map_or((String::new(), 0), |c| (c.server.clone(), c.port))
    };
    tracing::info!(%server, port, "WinHTTP request not sent: no network");
    error(ERROR_WINHTTP_CANNOT_CONNECT)
}

/// Every call that needs a sent request.
fn not_sent(request: Handle) -> i32 {
    if !expect(request, Kind::Request) {
        return 0;
    }
    error(ERROR_WINHTTP_INCORRECT_HANDLE_STATE)
}

pub extern "system" fn WinHttpReceiveResponse(request: Handle, _reserved: *mut c_void) -> i32 {
    not_sent(request)
}

pub extern "system" fn WinHttpQueryDataAvailable(request: Handle, _available: *mut u32) -> i32 {
    not_sent(request)
}

pub extern "system" fn WinHttpReadData(
    request: Handle,
    _buffer: *mut c_void,
    _size: u32,
    _read: *mut u32,
) -> i32 {
    not_sent(request)
}

pub extern "system" fn WinHttpWriteData(
    request: Handle,
    _buffer: *const c_void,
    _size: u32,
    _written: *mut u32,
) -> i32 {
    not_sent(request)
}

pub extern "system" fn WinHttpQueryHeaders(
    request: Handle,
    _level: u32,
    _name: *const u16,
    _buffer: *mut c_void,
    _length: *mut u32,
    _index: *mut u32,
) -> i32 {
    not_sent(request)
}

pub extern "system" fn WinHttpAddRequestHeaders(
    request: Handle,
    headers: *const u16,
    length: u32,
    _modifiers: u32,
) -> i32 {
    if !expect(request, Kind::Request) {
        return 0;
    }
    let Some(text) = wide(headers, length) else {
        return error(ERROR_INVALID_PARAMETER);
    };
    if let Some(o) = lock(&OBJECTS).get_mut(&(request as usize)) {
        o.headers.extend(text.encode_utf16());
        o.headers.extend("\r\n".encode_utf16());
    }
    1
}

pub extern "system" fn WinHttpSetTimeouts(
    handle: Handle,
    resolve: i32,
    connect: i32,
    send: i32,
    receive: i32,
) -> i32 {
    match lock(&OBJECTS).get_mut(&(handle as usize)) {
        Some(o) => {
            o.timeouts = [resolve, connect, send, receive];
            1
        }
        None => error(ERROR_INVALID_HANDLE),
    }
}

pub extern "system" fn WinHttpSetOption(
    handle: Handle,
    option: u32,
    buffer: *const c_void,
    length: u32,
) -> i32 {
    if buffer.is_null() && length != 0 {
        return error(ERROR_INVALID_PARAMETER);
    }
    let value = if length == 0 {
        Vec::new()
    } else {
        // SAFETY: the title passes `length` readable bytes.
        unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), length as usize) }.to_vec()
    };
    // A null handle sets a global default: accepted, with nothing to keep.
    if handle.is_null() {
        return 1;
    }
    let mut objects = lock(&OBJECTS);
    let Some(o) = objects.get_mut(&(handle as usize)) else {
        drop(objects);
        return error(ERROR_INVALID_HANDLE);
    };
    if option == WINHTTP_OPTION_CONTEXT_VALUE && value.len() >= 8 {
        o.context = usize::from_le_bytes(value[..8].try_into().unwrap());
    }
    o.options.insert(option, value);
    1
}

pub extern "system" fn WinHttpQueryOption(
    handle: Handle,
    option: u32,
    buffer: *mut c_void,
    length: *mut u32,
) -> i32 {
    if length.is_null() {
        return error(ERROR_INVALID_PARAMETER);
    }
    let value = {
        let objects = lock(&OBJECTS);
        let Some(o) = objects.get(&(handle as usize)) else {
            drop(objects);
            return error(ERROR_INVALID_HANDLE);
        };
        if option == WINHTTP_OPTION_CONTEXT_VALUE {
            Some(o.context.to_le_bytes().to_vec())
        } else {
            o.options.get(&option).cloned()
        }
    };
    let Some(value) = value else {
        return error(ERROR_WINHTTP_INVALID_OPTION);
    };
    // SAFETY: the title's buffer and its length.
    unsafe {
        let room = length.read() as usize;
        length.write(value.len() as u32);
        if buffer.is_null() || room < value.len() {
            return error(ERROR_INSUFFICIENT_BUFFER);
        }
        std::ptr::copy_nonoverlapping(value.as_ptr(), buffer.cast::<u8>(), value.len());
    }
    1
}

pub extern "system" fn WinHttpSetStatusCallback(
    handle: Handle,
    callback: usize,
    notifications: u32,
    _reserved: usize,
) -> usize {
    match lock(&OBJECTS).get_mut(&(handle as usize)) {
        Some(o) => {
            let previous = o.callback;
            o.callback = callback;
            o.notifications = notifications;
            previous
        }
        None => {
            set_error(ERROR_INVALID_HANDLE);
            INVALID_STATUS_CALLBACK
        }
    }
}

pub extern "system" fn WinHttpCloseHandle(handle: Handle) -> i32 {
    let Some(o) = lock(&OBJECTS).remove(&(handle as usize)) else {
        return error(ERROR_INVALID_HANDLE);
    };
    if o.callback != 0 && o.notifications & WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING != 0 {
        // Not under the table's lock: the callback may call back in. Its
        // information is the handle being closed.
        let mut closing = handle as u64;
        // SAFETY: the title's `WINHTTP_STATUS_CALLBACK`.
        unsafe {
            native::call_win64(
                o.callback,
                &[
                    handle as u64,
                    o.context as u64,
                    u64::from(WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING),
                    &raw mut closing as u64,
                    size_of::<u64>() as u64,
                ],
            )
        };
    }
    1
}

/// `URL_COMPONENTSW` on x64: a component is a pointer and a length, at these
/// offsets of the structure.
const SCHEME: usize = 8;
const HOST: usize = 24;
const USER: usize = 40;
const PASSWORD: usize = 56;
const PATH: usize = 72;
const EXTRA: usize = 88;
const COMPONENTS_SIZE: u32 = 104;

pub extern "system" fn WinHttpCrackUrl(
    url: *const u16,
    length: u32,
    _flags: u32,
    components: *mut u8,
) -> i32 {
    if url.is_null() || components.is_null() {
        return error(ERROR_INVALID_PARAMETER);
    }
    // SAFETY: the title passes a URL_COMPONENTSW.
    if unsafe { components.cast::<u32>().read() } != COMPONENTS_SIZE {
        return error(ERROR_INVALID_PARAMETER);
    }
    // SAFETY: a NUL-terminated URL, or `length` characters.
    let chars: Vec<u16> = unsafe {
        let n = if length == 0 {
            (0..).take_while(|&i| *url.add(i) != 0).count()
        } else {
            length as usize
        };
        std::slice::from_raw_parts(url, n).to_vec()
    };
    let text = String::from_utf16_lossy(&chars);
    let Some((scheme, rest)) = text.split_once("://") else {
        return error(ERROR_WINHTTP_UNRECOGNIZED_SCHEME);
    };
    let (kind, default_port) = match scheme.to_ascii_lowercase().as_str() {
        "http" => (1, 80),
        "https" => (2, 443),
        _ => return error(ERROR_WINHTTP_UNRECOGNIZED_SCHEME),
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let (user_info, host_port) = authority.rsplit_once('@').unwrap_or(("", authority));
    let (user, password) = user_info.split_once(':').unwrap_or((user_info, ""));
    // An IPv6 literal keeps its brackets out of the host and its colons out of the port.
    let (host, port) = match host_port.strip_prefix('[') {
        Some(v6) => match v6.split_once(']') {
            Some((h, "")) => (h, None),
            Some((h, p)) => match p.strip_prefix(':') {
                Some(port) => (h, Some(port)),
                None => return error(ERROR_WINHTTP_INVALID_URL),
            },
            None => return error(ERROR_WINHTTP_INVALID_URL),
        },
        None => match host_port.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (host_port, None),
        },
    };
    let port = match port {
        Some(p) => match p.parse::<u16>() {
            Ok(p) => p,
            Err(_) => return error(ERROR_WINHTTP_INVALID_URL),
        },
        None => default_port,
    };
    if host.is_empty() {
        return error(ERROR_WINHTTP_INVALID_URL);
    }
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    let (path, extra) = tail.split_at(path_end);
    // Where each part starts in the URL, for a caller that gave no buffer.
    let at = |part: &str| -> usize {
        let start = part.as_ptr() as usize - text.as_ptr() as usize;
        text[..start].encode_utf16().count()
    };
    let parts: [(usize, &str); 6] = [
        (SCHEME, scheme),
        (HOST, host),
        (USER, user),
        (PASSWORD, password),
        (PATH, path),
        (EXTRA, extra),
    ];
    for (offset, part) in parts {
        // SAFETY: the structure is 104 bytes; the title's buffer, when given,
        // holds `capacity` characters.
        unsafe {
            let pointer = components.add(offset).cast::<*mut u16>();
            let capacity = components.add(offset + 8).cast::<u32>();
            let (buffer, room) = (pointer.read_unaligned(), capacity.read_unaligned());
            if buffer.is_null() && room == 0 {
                continue; // not asked for
            }
            let units: Vec<u16> = part.encode_utf16().collect();
            if buffer.is_null() {
                if units.is_empty() {
                    pointer.write_unaligned(std::ptr::null_mut());
                    capacity.write_unaligned(0);
                } else {
                    // Point into the URL itself.
                    pointer.write_unaligned(url.add(at(part)).cast_mut());
                    capacity.write_unaligned(units.len() as u32);
                }
            } else if (room as usize) < units.len() + 1 {
                capacity.write_unaligned(units.len() as u32 + 1);
                return error(ERROR_INSUFFICIENT_BUFFER);
            } else {
                std::ptr::copy_nonoverlapping(units.as_ptr(), buffer, units.len());
                buffer.add(units.len()).write(0);
                capacity.write_unaligned(units.len() as u32);
            }
        }
    }
    // SAFETY: nScheme and nPort of the structure.
    unsafe {
        components.add(SCHEME + 12).cast::<i32>().write_unaligned(kind);
        components.add(HOST + 12).cast::<u16>().write_unaligned(port);
    }
    1
}

/// The function table of `winhttp.dll`.
pub(super) fn lookup(dll: &str, name: &str) -> Option<usize> {
    if dll != "winhttp.dll" {
        return None;
    }
    macro_rules! table {
        ($($f:ident),* $(,)?) => {
            match name {
                $(stringify!($f) => Some($f as *const () as usize),)*
                _ => None,
            }
        };
    }
    table!(
        WinHttpOpen,
        WinHttpConnect,
        WinHttpOpenRequest,
        WinHttpSendRequest,
        WinHttpReceiveResponse,
        WinHttpQueryDataAvailable,
        WinHttpReadData,
        WinHttpWriteData,
        WinHttpQueryHeaders,
        WinHttpAddRequestHeaders,
        WinHttpSetTimeouts,
        WinHttpSetOption,
        WinHttpQueryOption,
        WinHttpSetStatusCallback,
        WinHttpCloseHandle,
        WinHttpCrackUrl,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    fn last() -> u32 {
        super::super::GetLastError()
    }

    #[test]
    fn a_request_is_set_up_and_never_sent() {
        let agent = w("test");
        let session = WinHttpOpen(agent.as_ptr(), 0, std::ptr::null(), std::ptr::null(), 0);
        assert!(!session.is_null());
        assert_eq!(WinHttpSetTimeouts(session, 1, 2, 3, 4), 1);
        let host = w("example.com");
        let connection = WinHttpConnect(session, host.as_ptr(), 443, 0);
        assert!(!connection.is_null());
        // A connection is not a session.
        assert!(WinHttpConnect(connection, host.as_ptr(), 443, 0).is_null());
        assert_eq!(last(), ERROR_WINHTTP_INCORRECT_HANDLE_TYPE);
        let verb = w("GET");
        let path = w("/x");
        let request = WinHttpOpenRequest(
            connection,
            verb.as_ptr(),
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0x0080_0000,
        );
        assert!(!request.is_null());
        let header = w("Accept: */*");
        assert_eq!(WinHttpAddRequestHeaders(request, header.as_ptr(), u32::MAX, 0), 1);
        // Options round trip; the context value is a built-in.
        let value = 77u32.to_le_bytes();
        assert_eq!(WinHttpSetOption(request, 3, value.as_ptr().cast(), 4), 1);
        let mut out = [0u8; 4];
        let mut n = 4u32;
        assert_eq!(WinHttpQueryOption(request, 3, out.as_mut_ptr().cast(), &mut n), 1);
        assert_eq!(out, value);
        n = 0;
        assert_eq!(WinHttpQueryOption(request, 3, std::ptr::null_mut(), &mut n), 0);
        assert_eq!((last(), n), (ERROR_INSUFFICIENT_BUFFER, 4));
        assert_eq!(WinHttpQueryOption(request, 9999, out.as_mut_ptr().cast(), &mut n), 0);
        assert_eq!(last(), ERROR_WINHTTP_INVALID_OPTION);
        assert_eq!(WinHttpSetStatusCallback(request, 0x1234, 0xFFFF_FFFF, 0), 0);
        assert_eq!(WinHttpSetStatusCallback(request, 0, 0, 0), 0x1234);
        // Nothing is sent, and nothing follows from it.
        assert_eq!(WinHttpSendRequest(request, std::ptr::null(), 0, std::ptr::null(), 0, 0, 5), 0);
        assert_eq!(last(), ERROR_WINHTTP_CANNOT_CONNECT);
        assert_eq!(WinHttpReceiveResponse(request, std::ptr::null_mut()), 0);
        assert_eq!(last(), ERROR_WINHTTP_INCORRECT_HANDLE_STATE);
        let mut got = 0u32;
        assert_eq!(WinHttpReadData(request, std::ptr::null_mut(), 0, &mut got), 0);
        let mut context = [0u8; 8];
        let mut n = 8u32;
        assert_eq!(WinHttpQueryOption(request, WINHTTP_OPTION_CONTEXT_VALUE, context.as_mut_ptr().cast(), &mut n), 1);
        assert_eq!(usize::from_le_bytes(context), 5);
        for h in [request, connection, session] {
            assert_eq!(WinHttpCloseHandle(h), 1);
        }
        assert_eq!(WinHttpCloseHandle(request), 0);
        assert_eq!(last(), ERROR_INVALID_HANDLE);
    }

    #[test]
    fn urls_are_cracked_into_the_callers_buffers() {
        let url = w("https://user:pw@example.com:8443/a/b?x=1#f");
        let mut c = [0u8; 104];
        c[0..4].copy_from_slice(&104u32.to_le_bytes());
        let mut host = [0u16; 32];
        let mut scheme = [0u16; 8];
        c[SCHEME..SCHEME + 8].copy_from_slice(&(scheme.as_mut_ptr() as usize).to_le_bytes());
        c[SCHEME + 8..SCHEME + 12].copy_from_slice(&8u32.to_le_bytes());
        c[HOST..HOST + 8].copy_from_slice(&(host.as_mut_ptr() as usize).to_le_bytes());
        c[HOST + 8..HOST + 12].copy_from_slice(&32u32.to_le_bytes());
        // The path has no buffer, only a length of "give me a pointer".
        c[PATH + 8..PATH + 12].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(WinHttpCrackUrl(url.as_ptr(), 0, 0, c.as_mut_ptr()), 1);
        assert_eq!(String::from_utf16_lossy(&host[..11]), "example.com");
        assert_eq!(String::from_utf16_lossy(&scheme[..5]), "https");
        assert_eq!(u32::from_le_bytes(c[SCHEME + 8..SCHEME + 12].try_into().unwrap()), 5);
        assert_eq!(i32::from_le_bytes(c[SCHEME + 12..SCHEME + 16].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(c[HOST + 12..HOST + 14].try_into().unwrap()), 8443);
        let path_ptr = usize::from_le_bytes(c[PATH..PATH + 8].try_into().unwrap());
        let path_len = u32::from_le_bytes(c[PATH + 8..PATH + 12].try_into().unwrap()) as usize;
        // SAFETY: a pointer into `url`, as WinHTTP gives.
        let path = unsafe { std::slice::from_raw_parts(path_ptr as *const u16, path_len) };
        assert_eq!(String::from_utf16_lossy(path), "/a/b");
        // Too small a buffer asks for more.
        c[SCHEME + 8..SCHEME + 12].copy_from_slice(&8u32.to_le_bytes());
        c[HOST + 8..HOST + 12].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(WinHttpCrackUrl(url.as_ptr(), 0, 0, c.as_mut_ptr()), 0);
        assert_eq!(last(), ERROR_INSUFFICIENT_BUFFER);
        assert_eq!(u32::from_le_bytes(c[HOST + 8..HOST + 12].try_into().unwrap()), 12);
        let bad = w("ftp://x/");
        assert_eq!(WinHttpCrackUrl(bad.as_ptr(), 0, 0, c.as_mut_ptr()), 0);
        assert_eq!(last(), ERROR_WINHTTP_UNRECOGNIZED_SCHEME);
    }

    #[test]
    fn crack_url_handles_absent_pointer_components_and_ipv6_syntax() {
        // Requesting absent user/password/extra in pointer mode must return null/0, not panic.
        let url = w("https://example.com/a/b");
        let mut c = [0u8; 104];
        c[0..4].copy_from_slice(&104u32.to_le_bytes());
        for off in [SCHEME, HOST, USER, PASSWORD, PATH, EXTRA] {
            c[off + 8..off + 12].copy_from_slice(&u32::MAX.to_le_bytes());
        }
        assert_eq!(WinHttpCrackUrl(url.as_ptr(), 0, 0, c.as_mut_ptr()), 1);
        let read_ptr = |c: &[u8; 104], off: usize| {
            usize::from_le_bytes(c[off..off + 8].try_into().unwrap())
        };
        let read_len = |c: &[u8; 104], off: usize| {
            u32::from_le_bytes(c[off + 8..off + 12].try_into().unwrap())
        };
        for off in [USER, PASSWORD, EXTRA] {
            assert_eq!((read_ptr(&c, off), read_len(&c, off)), (0, 0));
        }
        // SAFETY: points into `url` for `host_len` UTF-16 units.
        let host = unsafe {
            std::slice::from_raw_parts(read_ptr(&c, HOST) as *const u16, read_len(&c, HOST) as usize)
        };
        assert_eq!(String::from_utf16_lossy(host), "example.com");

        // User without password in pointer mode.
        let user_only = w("https://alice@example.com/");
        let mut c2 = [0u8; 104];
        c2[0..4].copy_from_slice(&104u32.to_le_bytes());
        c2[USER + 8..USER + 12].copy_from_slice(&u32::MAX.to_le_bytes());
        c2[PASSWORD + 8..PASSWORD + 12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(WinHttpCrackUrl(user_only.as_ptr(), 0, 0, c2.as_mut_ptr()), 1);
        // SAFETY: points into `user_only`.
        let user = unsafe {
            std::slice::from_raw_parts(
                read_ptr(&c2, USER) as *const u16,
                read_len(&c2, USER) as usize,
            )
        };
        assert_eq!(String::from_utf16_lossy(user), "alice");
        assert_eq!((read_ptr(&c2, PASSWORD), read_len(&c2, PASSWORD)), (0, 0));

        // Trailing garbage after an IPv6 literal is rejected; valid IPv6 with/without port succeeds.
        let bad_v6 = w("http://[::1]garbage/path");
        let mut c3 = [0u8; 104];
        c3[0..4].copy_from_slice(&104u32.to_le_bytes());
        assert_eq!(WinHttpCrackUrl(bad_v6.as_ptr(), 0, 0, c3.as_mut_ptr()), 0);
        assert_eq!(last(), ERROR_WINHTTP_INVALID_URL);

        let good_v6 = w("http://[::1]:8080/path");
        c3[HOST + 8..HOST + 12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(WinHttpCrackUrl(good_v6.as_ptr(), 0, 0, c3.as_mut_ptr()), 1);
        assert_eq!(u16::from_le_bytes(c3[HOST + 12..HOST + 14].try_into().unwrap()), 8080);
    }
}
