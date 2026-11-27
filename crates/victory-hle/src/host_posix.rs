//! The host's shared Windows APIs, reimplemented on macOS for the native
//! host (no Wine, no Windows).
//!
//! On Windows `HostPolicy` passes `kernel32` and friends through to the real
//! DLLs. Here they are our own functions, written in terms of
//! `victory-platform` and the TEB. They are ordinary `extern "system"` Rust
//! functions (the System V ABI here), reached from title code through the
//! loader's bridge. Only the calls the first native slice needs exist; every
//! other import of an allowed DLL lands in the loader's unimplemented-import
//! stub, which names it.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use crate::modules::xmem_physical::{self, Held, Owner, enter, tracing_owners};
use victory_gpu::clock;
use victory_loader::native::seh::context::Context;
use victory_loader::native::{self, seh, teb};
use victory_platform::mem::{self, Protect};
use victory_platform::sync::{Event, Semaphore};
use victory_platform::thread;

mod coreaudio;
pub(crate) mod fake_dll;
pub(crate) mod files;
mod mf;
mod mixer;
mod sync;
mod system;
pub(crate) mod ucrt;
mod winhttp;
mod winsock;
mod wasapi;
mod xaudio2;

const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x102;
const WAIT_FAILED: u32 = 0xFFFF_FFFF;
const CREATE_SUSPENDED: u32 = 0x4;
const MAXIMUM_WAIT_OBJECTS: u32 = 64;
const INFINITE: u32 = 0xFFFF_FFFF;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_INVALID_ADDRESS: u32 = 487;
const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
const ERROR_NO_MORE_ITEMS: u32 = 259;
const TLS_OUT_OF_INDEXES: u32 = 0xFFFF_FFFF;

const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const MEM_DECOMMIT: u32 = 0x4000;
const MEM_RELEASE: u32 = 0x8000;
const MEM_COALESCE_PLACEHOLDERS: u32 = 0x1;
const MEM_PRESERVE_PLACEHOLDER: u32 = 0x2;
const DEFAULT_STACK: usize = 1 << 20;
/// The least stack a title thread gets: host work runs on title threads too (the native GPU host
/// converts and builds Metal pipelines at a draw), and the title's own sizes (1 MiB and less)
/// overflowed there at the first guessed draw.
const MIN_STACK: usize = 8 << 20;
const STILL_ACTIVE: u32 = 259;
/// `VirtualAlloc`'s allocation granularity: where a reservation starts, and
/// the least it takes of the address space.
const GRANULARITY: usize = 64 << 10;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_error(code: u32) {
    // SAFETY: the calling thread's own TEB.
    unsafe { teb::last_error_slot().write(code) };
}

pub extern "system" fn GetLastError() -> u32 {
    // SAFETY: the calling thread's own TEB.
    unsafe { teb::last_error_slot().read() }
}

pub extern "system" fn SetLastError(code: u32) {
    set_error(code);
}

pub extern "system" fn GetCurrentThreadId() -> u32 {
    thread::current_id()
}

pub extern "system" fn GetCurrentProcessId() -> u32 {
    std::process::id()
}

pub extern "system" fn QueryPerformanceFrequency(out: *mut i64) -> i32 {
    // SAFETY: the title passes a valid pointer.
    unsafe { out.write(clock::counter_frequency() as i64) };
    1
}

pub extern "system" fn QueryPerformanceCounter(out: *mut i64) -> i32 {
    // SAFETY: the title passes a valid pointer.
    unsafe { out.write(clock::counter() as i64) };
    1
}

pub extern "system" fn GetTickCount64() -> u64 {
    clock::ticks(1_000)
}

pub extern "system" fn GetTickCount() -> u32 {
    GetTickCount64() as u32
}

pub extern "system" fn Sleep(ms: u32) {
    thread::sleep(Duration::from_millis(u64::from(ms)));
}

// Memory.

/// What `VirtualAlloc` made, by base address: `VirtualFree(MEM_RELEASE)` takes
/// the whole region.
static REGIONS: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());

/// Who made each region of `REGIONS` (`VICTORY_TRACE_OWNERS` only).
static REGION_OWNERS: Mutex<BTreeMap<usize, Owner>> = Mutex::new(BTreeMap::new());

/// Lets `VirtualFree(MEM_RELEASE)` release a region another module reserved
/// (`xmem`'s `XMemVirtualAlloc`), and takes it back when that module frees it.
pub(crate) fn adopt_region(base: usize, len: usize) {
    lock(&REGIONS).insert(base, len);
    region_made(base);
}

pub(crate) fn forget_region(base: usize) {
    lock(&REGIONS).remove(&base);
    if tracing_owners() {
        lock(&REGION_OWNERS).remove(&base);
    }
}

fn region_made(base: usize) {
    if tracing_owners() {
        lock(&REGION_OWNERS).insert(base, Owner::now());
    }
}

/// The region of `REGIONS` holding `at`, if any: its base and length.
fn region_at(at: usize) -> Option<(usize, usize)> {
    lock(&REGIONS)
        .range(..=at)
        .next_back()
        .map(|(&base, &len)| (base, len))
        .filter(|&(base, len)| at < base + len)
}

fn region_held(what: &'static str, (base, len): (usize, usize), owner_at: usize) -> Held {
    Held {
        what,
        owner: lock(&REGION_OWNERS).get(&owner_at).copied(),
        base,
        len,
        pool: 0,
        index: 0,
    }
}

/// Whether one region holds all of `[base, base + len)`; if not, the nearest region at or
/// below `base` (for `VICTORY_TRACE_OWNERS`).
pub(crate) fn region_of(base: usize, len: usize) -> Result<(), Held> {
    let near = lock(&REGIONS)
        .range(..=base)
        .next_back()
        .map(|(&b, &l)| (b, l));
    match near {
        Some((b, l)) if base.saturating_add(len) <= b + l => Ok(()),
        Some(r) => Err(region_held("nearest region below", r, r.0)),
        None => Err(region_held("no region below", (0, 0), 0)),
    }
}

/// Win32 `PAGE_*` to a protection.
fn protection(flags: u32) -> Option<Protect> {
    Some(match flags & 0xFF {
        0x01 => Protect::NO_ACCESS,
        0x02 => Protect::READ,
        0x04 | 0x08 => Protect::READ_WRITE,
        0x10 | 0x20 => Protect::READ_EXECUTE,
        0x40 | 0x80 => Protect::READ_WRITE_EXECUTE,
        _ => return None,
    })
}

/// A protection as `PAGE_*`.
fn page_flags(p: Protect) -> u32 {
    match p {
        Protect::NO_ACCESS => 0x01,
        Protect::READ => 0x02,
        Protect::READ_WRITE => 0x04,
        Protect::READ_EXECUTE => 0x20,
        _ => 0x40,
    }
}

/// Reserves `len` bytes (a multiple of [`GRANULARITY`]) at `at`, or at an
/// address that is a multiple of it: more than that is reserved and the ends
/// given back, since macOS reserves on its page size (16 KiB; 4 KiB under
/// Rosetta).
fn reserve_granular(at: Option<usize>, len: usize) -> std::io::Result<*mut u8> {
    if at.is_some() {
        // SAFETY: fresh address space; a fixed address fails if taken.
        return unsafe { mem::reserve(at, len) };
    }
    let Some(padded) = len.checked_add(GRANULARITY) else {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    };
    // SAFETY: fresh address space.
    let p = unsafe { mem::reserve(None, padded) }? as usize;
    let start = p.next_multiple_of(GRANULARITY);
    // SAFETY: the ends of the reservation just made.
    unsafe {
        if start > p {
            mem::release(p as *mut u8, start - p)?;
        }
        let end = start + len;
        if p + padded > end {
            mem::release(end as *mut u8, p + padded - end)?;
        }
    }
    Ok(start as *mut u8)
}

pub extern "system" fn VirtualAlloc(
    address: *mut c_void,
    size: usize,
    kind: u32,
    protect: u32,
) -> *mut c_void {
    let (Some(prot), true) = (protection(protect), size > 0) else {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    };
    let _call = enter("VirtualAlloc");
    let page = mem::page_size();
    let at = address as usize;
    // A commit with no address reserves too, as on Windows.
    let result = if kind & MEM_RESERVE != 0 || at == 0 {
        let Some(len) = size.checked_next_multiple_of(GRANULARITY) else {
            set_error(ERROR_NOT_ENOUGH_MEMORY);
            return std::ptr::null_mut();
        };
        let at = (at != 0).then_some(at & !(GRANULARITY - 1));
        reserve_granular(at, len).and_then(|p| {
            if kind & MEM_COMMIT != 0 {
                // SAFETY: the reservation just made.
                if let Err(e) = unsafe { mem::commit(p, len, prot) } {
                    // SAFETY: as above.
                    unsafe { mem::release(p, len) }?;
                    return Err(e);
                }
            }
            lock(&REGIONS).insert(p as usize, len);
            region_made(p as usize);
            Ok(p)
        })
    } else if kind & MEM_COMMIT != 0 && at != 0 {
        let start = at & !(page - 1);
        let Some(end) = at
            .checked_add(size)
            .and_then(|e| e.checked_next_multiple_of(page))
        else {
            set_error(ERROR_NOT_ENOUGH_MEMORY);
            return std::ptr::null_mut();
        };
        // SAFETY: the title commits inside a reservation of its own.
        unsafe { mem::commit(start as *mut u8, end - start, prot) }.map(|()| start as *mut u8)
    } else {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    };
    match result {
        Ok(p) => p.cast(),
        Err(_) => {
            set_error(ERROR_NOT_ENOUGH_MEMORY);
            std::ptr::null_mut()
        }
    }
}

pub extern "system" fn VirtualFree(address: *mut c_void, size: usize, kind: u32) -> i32 {
    let _call = enter("VirtualFree");
    let ok = if kind & MEM_RELEASE != 0
        && kind & (MEM_PRESERVE_PLACEHOLDER | MEM_COALESCE_PLACEHOLDERS) != 0
    {
        // Splitting or merging a placeholder: a placeholder is plain reserved
        // address space here (see `xmem`), so there is nothing to change, only
        // a range inside a region of ours to vouch for.
        let at = address as usize;
        let inside = lock(&REGIONS)
            .range(..=at)
            .next_back()
            .is_some_and(|(&base, &len)| at + size <= base + len);
        if !inside {
            set_error(ERROR_INVALID_PARAMETER);
        }
        return i32::from(inside);
    } else if kind & MEM_RELEASE != 0 {
        let Some(len) = lock(&REGIONS).remove(&(address as usize)) else {
            set_error(ERROR_INVALID_PARAMETER);
            return 0;
        };
        // The physical-page views in it go first (`XMemVirtualAlloc`'s pool backing, views
        // `XMemMapPhysicalPages` put there), as `XMemFreeDefault`'s release does.
        crate::modules::xmem::forget_physical(address as usize, len);
        if tracing_owners() {
            lock(&REGION_OWNERS).remove(&(address as usize));
        }
        // SAFETY: a region this shim made.
        unsafe { mem::release(address.cast(), len) }.is_ok()
    } else if kind & MEM_DECOMMIT != 0 {
        let at = address as usize;
        // Size 0 is the rest of the region (all of it from its base), as on Windows; a range
        // outside our regions is the host's, never decommitted.
        let Some((base, len)) = region_at(at) else {
            set_error(ERROR_INVALID_ADDRESS);
            return 0;
        };
        let page = mem::page_size();
        let start = at & !(page - 1);
        let end = match size {
            0 => base + len,
            _ => at.saturating_add(size).next_multiple_of(page),
        };
        if region_of(start, end - start).is_err() {
            set_error(ERROR_INVALID_ADDRESS);
            return 0;
        }
        if tracing_owners() {
            let range = region_held("decommitted range", (start, end - start), base);
            xmem_physical::freeing("VirtualFree(MEM_DECOMMIT) over views", &range);
        }
        // Physical-page views there stay, zeroed.
        xmem_physical::decommit(start, end - start)
    } else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    i32::from(ok)
}

pub extern "system" fn VirtualProtect(
    address: *mut c_void,
    size: usize,
    protect: u32,
    old: *mut u32,
) -> i32 {
    let (Some(prot), true) = (protection(protect), size > 0) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let page = mem::page_size();
    let at = address as usize;
    let start = at & !(page - 1);
    let Some(end) = at
        .checked_add(size)
        .and_then(|e| e.checked_next_multiple_of(page))
    else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let len = end - start;
    // SAFETY: the title protects its own memory.
    match unsafe { mem::protect(start as *mut u8, len, prot) } {
        Ok(previous) => {
            if !old.is_null() {
                // SAFETY: the title passes a valid pointer or null.
                unsafe { old.write(page_flags(previous)) };
            }
            1
        }
        Err(_) => {
            set_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

// Thread-local storage slots (`TlsAlloc`), in the TEB.

static TLS_USED: AtomicU64 = AtomicU64::new(0);

pub extern "system" fn TlsAlloc() -> u32 {
    let mut used = TLS_USED.load(Ordering::Relaxed);
    loop {
        let free = (!used).trailing_zeros();
        if free as usize >= teb::TLS_SLOT_COUNT {
            set_error(ERROR_NO_MORE_ITEMS);
            return TLS_OUT_OF_INDEXES;
        }
        match TLS_USED.compare_exchange(used, used | 1 << free, Ordering::AcqRel, Ordering::Relaxed)
        {
            Ok(_) => return free,
            Err(now) => used = now,
        }
    }
}

pub extern "system" fn TlsFree(index: u32) -> i32 {
    if index as usize >= teb::TLS_SLOT_COUNT || TLS_USED.load(Ordering::Relaxed) >> index & 1 == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // Zeroed before the slot can be handed out again.
    teb::clear_tls_slot(index as usize);
    TLS_USED.fetch_and(!(1 << index), Ordering::AcqRel);
    1
}

pub extern "system" fn TlsGetValue(index: u32) -> *mut c_void {
    if index as usize >= teb::TLS_SLOT_COUNT {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    set_error(0);
    // SAFETY: inside the calling thread's TEB.
    unsafe { teb::tls_slots().add(index as usize).read() as *mut c_void }
}

pub extern "system" fn TlsSetValue(index: u32, value: *mut c_void) -> i32 {
    if index as usize >= teb::TLS_SLOT_COUNT {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: inside the calling thread's TEB.
    unsafe { teb::tls_slots().add(index as usize).write(value as usize) };
    1
}

// Kernel objects: a handle table over `victory-platform`.

pub(crate) enum Object {
    Event(Arc<Event>),
    Semaphore(Arc<Semaphore>),
    Thread(Arc<ThreadObject>),
    File(Arc<files::FileObject>),
    Find(Arc<Mutex<files::Find>>),
    Mutex(Arc<system::MutexObject>),
    Timer(Arc<system::TimerObject>),
}

/// A title thread: its end, its suspension count and its Mach port.
pub(crate) struct ThreadObject {
    /// Signalled when the thread has ended.
    done: Arc<Event>,
    /// Holds a thread created suspended before its first instruction.
    gate: Event,
    state: Mutex<ThreadState>,
    start: usize,
    param: usize,
}

struct ThreadState {
    suspend_count: u32,
    /// Past the gate, running title code (Mach suspension applies).
    running: bool,
    port: Option<native::threadctl::ThreadPort>,
    exit_code: u32,
}

static HANDLES: LazyLock<Mutex<HashMap<usize, Object>>> = LazyLock::new(Default::default);
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

fn new_handle(object: Object) -> *mut c_void {
    let handle = (NEXT_HANDLE.fetch_add(1, Ordering::Relaxed) as usize) << 2;
    if std::env::var_os("VICTORY_TRACE_HANDLES").is_some() {
        let kind = match &object {
            Object::Event(_) => "event",
            Object::Semaphore(_) => "semaphore",
            Object::Thread(_) => "thread",
            _ => "other",
        };
        let chain = native::dump::caller_chain();
        if matches!(object, Object::Semaphore(_)) {
            lock(&LEDGER).insert(handle, (chain.iter().take(4).copied().collect(), 0, 0));
            static START: std::sync::Once = std::sync::Once::new();
            START.call_once(|| {
                std::thread::spawn(|| loop {
                    std::thread::sleep(Duration::from_secs(4));
                    let mut rows: Vec<_> = lock(&LEDGER).iter().map(|(h, v)| (*h, v.clone())).collect();
                    rows.sort_by_key(|r| r.0);
                    for (h, (chain, rel, got)) in rows {
                        tracing::warn!("ledger {h:#x} made at {chain:x?}: released {rel}, taken {got}");
                    }
                });
            });
        }
        tracing::warn!("handle {handle:#x} ({kind}) made at image+ {chain:x?}");
    }
    lock(&HANDLES).insert(handle, object);
    handle as *mut c_void
}

pub extern "system" fn CreateEventW(
    _attributes: *const c_void,
    manual_reset: i32,
    initial: i32,
    _name: *const u16,
) -> *mut c_void {
    match Event::new(manual_reset != 0, initial != 0) {
        Ok(e) => new_handle(Object::Event(Arc::new(e))),
        Err(_) => {
            set_error(ERROR_NOT_ENOUGH_MEMORY);
            std::ptr::null_mut()
        }
    }
}

fn event_of(handle: *mut c_void) -> Option<Arc<Event>> {
    match lock(&HANDLES).get(&(handle as usize)) {
        Some(Object::Event(e)) => Some(e.clone()),
        _ => None,
    }
}

pub extern "system" fn SetEvent(handle: *mut c_void) -> i32 {
    let Some(e) = event_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    e.set();
    sync::signalled();
    1
}

pub extern "system" fn ResetEvent(handle: *mut c_void) -> i32 {
    let Some(e) = event_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    e.reset();
    1
}

pub extern "system" fn CreateSemaphoreW(
    _attributes: *const c_void,
    initial: i32,
    max: i32,
    _name: *const u16,
) -> *mut c_void {
    if initial < 0 || max <= 0 || initial > max {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    match Semaphore::new(initial as u32, max as u32) {
        Ok(s) => {
            let h = new_handle(Object::Semaphore(Arc::new(s)));
            if std::env::var_os("VICTORY_TRACE_HANDLES").is_some() {
                tracing::warn!("semaphore {h:?} initial {initial} max {max}");
            }
            h
        }
        Err(_) => {
            set_error(ERROR_INVALID_PARAMETER);
            std::ptr::null_mut()
        }
    }
}

pub extern "system" fn ReleaseSemaphore(
    handle: *mut c_void,
    count: i32,
    previous: *mut i32,
) -> i32 {
    watched(handle as usize, "release", count as u32);
    let semaphore = match lock(&HANDLES).get(&(handle as usize)) {
        Some(Object::Semaphore(s)) => Some(s.clone()),
        _ => None,
    };
    let Some(s) = semaphore else {
        tracing::warn!("ReleaseSemaphore on {handle:p}: not a semaphore");
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    if count <= 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    match s.release(count as u32) {
        Some(before) => {
            sync::signalled();
            if !previous.is_null() {
                // SAFETY: the title passes a valid pointer or null.
                unsafe { previous.write(before as i32) };
            }
            1
        }
        None => {
            tracing::warn!("ReleaseSemaphore on {handle:p}: past the maximum");
            set_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

/// An object a wait can take: cloned out of the table, so the table is not
/// locked during a long wait.
#[derive(Clone)]
pub(crate) enum Waitable {
    Event(Arc<Event>),
    Semaphore(Arc<Semaphore>),
    Mutex(Arc<system::MutexObject>),
}

impl Waitable {
    fn of(handle: *mut c_void) -> Option<Waitable> {
        match lock(&HANDLES).get(&(handle as usize)) {
            Some(Object::Event(e)) => Some(Waitable::Event(e.clone())),
            Some(Object::Thread(t)) => Some(Waitable::Event(t.done.clone())),
            Some(Object::Semaphore(s)) => Some(Waitable::Semaphore(s.clone())),
            Some(Object::Mutex(m)) => Some(Waitable::Mutex(m.clone())),
            Some(Object::Timer(t)) => Some(Waitable::Event(t.event.clone())),
            _ => None,
        }
    }

    fn wait(&self, timeout: Option<Duration>) -> bool {
        match self {
            Waitable::Event(e) => e.wait(timeout),
            Waitable::Semaphore(s) => s.wait(timeout),
            Waitable::Mutex(m) => m.acquire(timeout),
        }
    }

    /// Takes the object if it is signalled now.
    fn try_take(&self) -> bool {
        self.wait(Some(Duration::ZERO))
    }

    /// Undoes a take (a wait-all that could not take every object). A
    /// manual-reset event was not changed by it, and setting it again is
    /// a no-op.
    fn give_back(&self) {
        match self {
            Waitable::Event(e) => e.set(),
            Waitable::Semaphore(s) => {
                s.release(1);
            }
            Waitable::Mutex(m) => {
                m.release();
            }
        }
    }
}

pub extern "system" fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32 {
    let timeout = (ms != INFINITE).then(|| Duration::from_millis(u64::from(ms)));
    let Some(object) = Waitable::of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return WAIT_FAILED;
    };
    let got = object.wait(timeout);
    watched(handle as usize, if got { "wait got it" } else { "wait timed out" }, ms);
    if got {
        WAIT_OBJECT_0
    } else {
        WAIT_TIMEOUT
    }
}

static LEDGER: LazyLock<Mutex<HashMap<usize, (Vec<usize>, u64, u64)>>> = LazyLock::new(Default::default);

static WATCHED: LazyLock<Mutex<std::collections::HashSet<usize>>> = LazyLock::new(Default::default);

/// `VICTORY_WATCH_HANDLE=<hex>`: logs every wait and release on that handle.
fn watched(handle: usize, what: &str, arg: u32) {
    static ON: LazyLock<bool> = LazyLock::new(|| std::env::var_os("VICTORY_TRACE_HANDLES").is_some());
    if !*ON {
        return;
    }
    static WATCH: LazyLock<usize> = LazyLock::new(|| {
        std::env::var("VICTORY_WATCH_HANDLE")
            .ok()
            .and_then(|v| usize::from_str_radix(v.trim_start_matches("0x"), 16).ok())
            .unwrap_or(0)
    });
    if let Some(e) = lock(&LEDGER).get_mut(&handle) {
        match what {
            "release" => e.1 += 1,
            "wait got it" | "multi wait got it" => e.2 += 1,
            _ => {}
        }
    }
    if (handle == *WATCH && handle != 0 || lock(&WATCHED).contains(&handle)) && what != "wait timed out" {
        tracing::warn!("watch {handle:#x}: {what} ({arg}) at image+ {:x?}", native::dump::caller_chain());
    }
}

pub extern "system" fn WaitForSingleObjectEx(handle: *mut c_void, ms: u32, _alertable: i32) -> u32 {
    WaitForSingleObject(handle, ms)
}

pub extern "system" fn WaitForMultipleObjects(
    count: u32,
    handles: *const *mut c_void,
    wait_all: i32,
    ms: u32,
) -> u32 {
    if count == 0 || count > MAXIMUM_WAIT_OBJECTS || handles.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return WAIT_FAILED;
    }
    // SAFETY: the title passes `count` handles.
    let handles = unsafe { std::slice::from_raw_parts(handles, count as usize) };
    if wait_all != 0
        && handles
            .iter()
            .enumerate()
            .any(|(i, h)| handles[i + 1..].contains(h))
    {
        set_error(ERROR_INVALID_PARAMETER);
        return WAIT_FAILED;
    }
    let objects: Option<Vec<Waitable>> = handles.iter().map(|&h| Waitable::of(h)).collect();
    let Some(objects) = objects else {
        set_error(ERROR_INVALID_HANDLE);
        return WAIT_FAILED;
    };
    let timeout = (ms != INFINITE).then(|| Duration::from_millis(u64::from(ms)));
    let r = sync::wait_multiple(&objects, wait_all != 0, timeout);
    if let Some(&h) = handles.get(r as usize) {
        watched(h as usize, "multi wait got it", count);
    }
    r
}

pub extern "system" fn WaitForMultipleObjectsEx(
    count: u32,
    handles: *const *mut c_void,
    wait_all: i32,
    ms: u32,
    _alertable: i32,
) -> u32 {
    WaitForMultipleObjects(count, handles, wait_all, ms)
}

pub extern "system" fn CloseHandle(handle: *mut c_void) -> i32 {
    if lock(&HANDLES).remove(&(handle as usize)).is_none() {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    1
}

/// `CreateThread`: the start routine runs in title code, on a thread with
/// its own TEB and static TLS, after the images' thread-attach notices.
/// `CREATE_SUSPENDED` keeps it from running until `ResumeThread`.
pub extern "system" fn CreateThread(
    _attributes: *const c_void,
    stack_size: usize,
    start: usize,
    param: *mut c_void,
    flags: u32,
    thread_id: *mut u32,
) -> *mut c_void {
    let suspended = flags & CREATE_SUSPENDED != 0;
    let object = Arc::new(ThreadObject {
        done: Arc::new(Event::new(true, false).expect("event")),
        gate: Event::new(true, false).expect("event"),
        state: Mutex::new(ThreadState {
            suspend_count: u32::from(suspended),
            running: false,
            port: None,
            exit_code: STILL_ACTIVE,
        }),
        start,
        param: param as usize,
    });
    let (me, finished) = (object.clone(), object.clone());
    // SAFETY: `start` is the title's own thread routine.
    let started = unsafe {
        native::spawn_title_thread_with(
            "title-thread",
            if stack_size == 0 { DEFAULT_STACK } else { stack_size }.max(MIN_STACK),
            start,
            param as usize,
            // Before anything of the title's runs: park if suspended.
            move || {
                lock(&me.state).port = Some(native::threadctl::ThreadPort::current());
                // A suspension that came in before the thread was up. The count is read and
                // `running` set under one lock: a `SuspendThread` in between saw the thread not
                // running, did not stop it, and it then ran suspended.
                loop {
                    let mut state = lock(&me.state);
                    if state.suspend_count == 0 {
                        state.running = true;
                        break;
                    }
                    // Set again by the `ResumeThread` that brings the count to 0.
                    me.gate.reset();
                    drop(state);
                    me.gate.wait(None);
                }
            },
            move |code| {
                if std::env::var_os("VICTORY_TRACE_HANDLES").is_some() {
                    tracing::warn!("title thread ends, code {code:#x}");
                }
                lock(&finished.state).exit_code = code;
                finished.done.set();
                sync::signalled();
            },
        )
    };
    match started {
        Ok(t) => {
            if !thread_id.is_null() {
                // SAFETY: the title passes a valid pointer or null.
                unsafe { thread_id.write(t.id()) };
            }
            // The thread detaches: its end is seen through `done`.
            new_handle(Object::Thread(object))
        }
        Err(_) => {
            set_error(ERROR_NOT_ENOUGH_MEMORY);
            std::ptr::null_mut()
        }
    }
}

fn thread_of(handle: *mut c_void) -> Option<Arc<ThreadObject>> {
    match lock(&HANDLES).get(&(handle as usize)) {
        Some(Object::Thread(t)) => Some(t.clone()),
        _ => None,
    }
}

/// Where a thread is when Mach stops it should be title code, or it could
/// hold a host lock the caller then needs. Retries briefly, then takes
/// the thread wherever it is (one blocked in a host wait never gets there).
fn suspend_port(port: &native::threadctl::ThreadPort) -> bool {
    for _ in 0..200 {
        if !port.suspend() {
            return false;
        }
        match port.rip() {
            Some(rip) if seh::unwind::image_base_of(rip as usize).is_none() => {
                port.resume();
                std::thread::sleep(Duration::from_micros(100));
            }
            _ => return true,
        }
    }
    port.suspend()
}

pub extern "system" fn SuspendThread(handle: *mut c_void) -> u32 {
    let Some(t) = thread_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return WAIT_FAILED;
    };
    let mut state = lock(&t.state);
    let previous = state.suspend_count;
    state.suspend_count += 1;
    if previous == 0
        && state.running
        && let Some(port) = &state.port
    {
        suspend_port(port);
    }
    previous
}

pub extern "system" fn ResumeThread(handle: *mut c_void) -> u32 {
    let Some(t) = thread_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return WAIT_FAILED;
    };
    let mut state = lock(&t.state);
    let previous = state.suspend_count;
    if previous > 0 {
        state.suspend_count -= 1;
        if state.suspend_count == 0 {
            if !state.running {
                t.gate.set();
            } else if let Some(port) = &state.port {
                port.resume();
            }
        }
    }
    previous
}

/// `GetThreadContext`: the registers of a suspended thread (a thread that
/// has not run yet is at its start routine, with its parameter in `rcx`).
pub extern "system" fn GetThreadContext(handle: *mut c_void, context: *mut Context) -> i32 {
    let Some(t) = thread_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    let state = lock(&t.state);
    // SAFETY: the title passes a CONTEXT.
    let context = unsafe { &mut *context };
    let ok = if state.running {
        state.port.as_ref().is_some_and(|port| {
            // Mach reads a stopped thread only.
            let stopped = state.suspend_count > 0;
            (stopped || suspend_port(port)) && {
                let ok = port.context(context);
                if !stopped {
                    port.resume();
                }
                ok
            }
        })
    } else {
        *context = Context::zeroed();
        context.context_flags = seh::context::CONTEXT_FULL;
        context.rip = t.start as u64;
        context.gpr[seh::context::RCX] = t.param as u64;
        true
    };
    if !ok {
        set_error(ERROR_INVALID_PARAMETER);
    }
    i32::from(ok)
}

pub extern "system" fn GetExitCodeThread(handle: *mut c_void, code: *mut u32) -> i32 {
    let Some(t) = thread_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    // SAFETY: the title passes a valid pointer.
    unsafe { code.write(lock(&t.state).exit_code) };
    1
}

/// The function table `HostPolicy` consults on macOS.
pub(crate) fn lookup(dll: &str, name: &str) -> Option<usize> {
    let dll = dll.to_ascii_lowercase();
    if let Some(address) = system::lookup(&dll, name)
        .or_else(|| ucrt::lookup(&dll, name))
        .or_else(|| winsock::lookup(&dll, name))
        .or_else(|| winhttp::lookup(&dll, name))
        .or_else(|| mf::lookup(&dll, name))
        .or_else(|| xaudio2::lookup(&dll, name))
    {
        return Some(address);
    }
    // The API sets and kernelbase forward to kernel32's functions; the
    // exception functions are ntdll's (and `__C_specific_handler` the C
    // runtime's).
    let kernel = dll == "kernel32.dll"
        || dll == "kernelbase.dll"
        || dll == "ntdll.dll"
        // The title's own vcruntime140 wins when it ships one.
        || (dll.starts_with("vcruntime140")
            && victory_loader::native::dll::find(&dll).is_none())
        || dll.starts_with("api-ms-win-core-");
    if !kernel {
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
    if let Some(address) = exceptions(name) {
        return Some(address);
    }
    if let Some(address) = sync::lookup(name).or_else(|| files::lookup(name)) {
        return Some(address);
    }
    table!(
        GetLastError,
        SetLastError,
        GetCurrentThreadId,
        GetCurrentProcessId,
        QueryPerformanceFrequency,
        QueryPerformanceCounter,
        GetTickCount,
        GetTickCount64,
        Sleep,
        VirtualAlloc,
        VirtualFree,
        VirtualProtect,
        TlsAlloc,
        TlsFree,
        TlsGetValue,
        TlsSetValue,
        CreateEventW,
        SetEvent,
        ResetEvent,
        CreateSemaphoreW,
        ReleaseSemaphore,
        WaitForSingleObject,
        WaitForSingleObjectEx,
        WaitForMultipleObjects,
        WaitForMultipleObjectsEx,
        CloseHandle,
        CreateThread,
        SuspendThread,
        ResumeThread,
        GetThreadContext,
        GetExitCodeThread,
    )
}

/// Exceptions: `RtlCaptureContext` and `RaiseException` read the caller's
/// registers, so they are the loader's own Windows-ABI assembly, bound with
/// no bridge; the rest are the loader's host functions.
fn exceptions(name: &str) -> Option<usize> {
    use native::seh::api;
    let direct = |address: usize| {
        native::declare_direct(address);
        Some(address)
    };
    match name {
        "RtlCaptureContext" => direct(api::capture_context_address()),
        "RaiseException" => direct(api::raise_exception_address()),
        "DebugBreak" => direct(api::debug_break_address()),
        "RtlLookupFunctionEntry" => Some(api::lookup_function_entry as *const () as usize),
        "RtlVirtualUnwind" => Some(api::virtual_unwind as *const () as usize),
        "RtlAddFunctionTable" => Some(api::add_function_table as *const () as usize),
        "RtlDeleteFunctionTable" => Some(api::delete_function_table as *const () as usize),
        "RtlPcToFileHeader" => Some(api::pc_to_file_header as *const () as usize),
        "RtlRestoreContext" => Some(api::restore_context as *const () as usize),
        "RtlUnwindEx" => Some(api::unwind_ex as *const () as usize),
        "__C_specific_handler" => Some(api::c_specific_handler as *const () as usize),
        "AddVectoredExceptionHandler" => Some(api::add_vectored as *const () as usize),
        "RemoveVectoredExceptionHandler" => Some(api::remove_vectored as *const () as usize),
        "SetUnhandledExceptionFilter" => Some(api::set_unhandled as *const () as usize),
        _ => None,
    }
}

#[cfg(test)]
mod clock_tests {
    use super::*;
    use std::time::Instant;

    fn qpc() -> u64 {
        let mut ticks = 0;
        assert_eq!(QueryPerformanceCounter(&mut ticks), 1);
        ticks as u64
    }

    fn crt<F: Copy>(name: &str) -> F {
        let address = ucrt::lookup("ucrtbase.dll", name).unwrap();
        // SAFETY: the caller supplies the CRT export's function type.
        unsafe { std::mem::transmute_copy(&address) }
    }

    fn readings() -> impl Iterator<Item = u128> {
        let crt_clock: extern "system" fn() -> i32 = crt("clock");
        let timespec_get: extern "system" fn(*mut i64, i32) -> i32 = crt("timespec_get");
        let time64: extern "system" fn(*mut i64) -> i64 = crt("_time64");
        let mut timespec = [0i64; 2];
        let (mut filetime, mut precise) = (0, 0);
        let mut calendar = [0; 8];
        const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;
        let wall_before = clock::unix_time().as_nanos();
        let before = qpc();
        let tick = GetTickCount();
        let tick64 = GetTickCount64();
        let crt_ms = crt_clock();
        let input = crate::modules::gameinput::now();
        let after = qpc();
        system::GetSystemTimeAsFileTime(&mut filetime);
        system::GetSystemTimePreciseAsFileTime(&mut precise);
        system::GetSystemTime(&mut calendar);
        let mf = mf::MFGetSystemTime();
        assert_eq!(timespec_get(timespec.as_mut_ptr(), 1), 1);
        let seconds = time64(std::ptr::null_mut());
        let wall_after = clock::unix_time().as_nanos();
        let mut calendar_filetime = 0;
        assert_eq!(
            system::SystemTimeToFileTime(&calendar, &mut calendar_filetime),
            1
        );
        let unix_ns = |ft| u128::from(ft - FILETIME_UNIX_EPOCH) * 100;
        let monotonic = [
            (u128::from(tick) * 1_000_000, 1_000_000),
            (u128::from(tick64) * 1_000_000, 1_000_000),
            (crt_ms as u128 * 1_000_000, 1_000_000),
            (u128::from(input) * 1_000, 1_000),
        ];
        let wall = [
            (unix_ns(filetime), 100),
            (unix_ns(precise), 100),
            (unix_ns(calendar_filetime), 1_000_000),
            (unix_ns(mf as u64), 100),
            (timespec[0] as u128 * 1_000_000_000 + timespec[1] as u128, 1),
            (seconds as u128 * 1_000_000_000, 1_000_000_000),
        ];
        for (samples, low, high) in [
            (&monotonic[..], u128::from(before), u128::from(after)),
            (&wall[..], wall_before, wall_after),
        ] {
            for &(ns, quantum) in samples {
                assert!(
                    ns >= low.saturating_sub(quantum - 1) && ns <= high,
                    "{ns} ns (tick {quantum} ns) outside {low}..={high}"
                );
            }
        }
        monotonic.into_iter().chain(wall).map(|(ns, _)| ns)
    }

    fn check_rate(rate: f64) {
        let mut hz = 0;
        assert_eq!(QueryPerformanceFrequency(&mut hz), 1);
        assert_eq!(hz, 1_000_000_000);
        clock::initialize();
        std::thread::sleep(Duration::from_millis(20));
        let before_first = Instant::now();
        let first = qpc();
        let first_utc = clock::unix_time();
        let after_first = Instant::now();
        let first_readings = readings();
        std::thread::sleep(Duration::from_millis(20));
        let second_readings = readings();
        let before_second = Instant::now();
        let second = qpc();
        let second_utc = clock::unix_time();
        let after_second = Instant::now();
        for (a, b) in first_readings.zip(second_readings) {
            assert!(a <= b, "{a} ns followed by {b} ns");
        }
        let low = (before_second - after_first).as_nanos() as f64 * rate;
        let high = (after_second - before_first).as_nanos() as f64 * rate;
        for actual in [(second - first) as f64, (second_utc - first_utc).as_nanos() as f64] {
            assert!(
                (low - 1.0..=high + 1.0).contains(&actual),
                "{actual} ns at rate {rate}"
            );
        }
    }

    #[test]
    fn guest_clocks_agree_and_slow_down_together() {
        const NAME: &str = "host_posix::clock_tests::guest_clocks_agree_and_slow_down_together";
        const CHILD: &str = "VICTORY_TEST_CLOCK_RATE";
        if let Ok(rate) = std::env::var(CHILD) {
            check_rate(rate.parse().unwrap());
            return;
        }
        for (rate, setting) in [("1", None), ("0.25", Some("0.25"))] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.args(["--exact", NAME]).env(CHILD, rate);
            match setting {
                Some(setting) => child.env("VICTORY_CLOCK_SLOW", setting),
                None => child.env_remove("VICTORY_CLOCK_SLOW"),
            };
            let status = child.status().unwrap();
            assert!(status.success(), "clock rate {rate}: {status}");
        }
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    /// A TLS slot freed and handed out again reads 0 in every thread, not what it held before.
    #[test]
    fn a_reused_tls_slot_reads_zero_in_every_thread() {
        let slot = TlsAlloc();
        assert_ne!(slot, TLS_OUT_OF_INDEXES);
        let (set, go) = (std::sync::mpsc::channel(), std::sync::mpsc::channel::<u32>());
        let (set_tx, go_rx) = (set.0, go.1);
        let other = std::thread::spawn(move || {
            assert_eq!(TlsSetValue(slot, 5 as *mut c_void), 1);
            set_tx.send(()).unwrap();
            TlsGetValue(go_rx.recv().unwrap()) as usize
        });
        set.1.recv().unwrap();
        assert_eq!(TlsSetValue(slot, 6 as *mut c_void), 1);
        assert_eq!(TlsFree(slot), 1);
        let again = TlsAlloc();
        assert_ne!(again, TLS_OUT_OF_INDEXES);
        go.0.send(again).unwrap();
        assert_eq!((TlsGetValue(again) as usize, other.join().unwrap()), (0, 0));
        assert_eq!(TlsFree(again), 1);
    }

    #[test]
    fn reservations_start_on_64_kib_and_take_whole_granules() {
        let mut bases = Vec::new();
        for size in [1, 4096, 16384, 65536, 65537, 3 << 20] {
            let p = VirtualAlloc(
                std::ptr::null_mut(),
                size,
                MEM_RESERVE | MEM_COMMIT,
                0x04, // PAGE_READWRITE
            );
            assert!(!p.is_null());
            assert_eq!(p as usize % GRANULARITY, 0, "size {size}");
            // The whole granule is there to use, and is what was taken.
            let granules = size.div_ceil(GRANULARITY) * GRANULARITY;
            // SAFETY: committed read-write by the call above.
            unsafe {
                p.cast::<u8>().add(granules - 1).write(1);
                p.cast::<u8>().write(2);
            }
            assert_eq!(lock(&REGIONS)[&(p as usize)], granules);
            bases.push(p);
        }
        // Splitting a placeholder inside a region succeeds, outside it fails.
        let r = bases[3];
        assert_eq!(
            VirtualFree(r, GRANULARITY / 2, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER),
            1
        );
        assert_eq!(
            VirtualFree(r, 1 << 30, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER),
            0
        );
        for p in bases {
            assert_eq!(VirtualFree(p, 0, MEM_RELEASE), 1);
        }
    }

    /// `VirtualAlloc(NULL, n, MEM_COMMIT)` reserves as well, as on Windows.
    #[test]
    fn a_commit_without_an_address_reserves() {
        let p = VirtualAlloc(std::ptr::null_mut(), 100, MEM_COMMIT, 0x04);
        assert!(!p.is_null());
        // SAFETY: committed read-write.
        unsafe { p.cast::<u8>().write(1) };
        assert_eq!(VirtualFree(p, 0, MEM_RELEASE), 1);
    }

    /// `VirtualFree(MEM_DECOMMIT)` keeps the bytes of a physical page another address still
    /// shows: `XMemMapPhysicalPages` pages mapped twice, and `XMemVirtualAlloc` memory the GPU's
    /// page mapping (`map_alias`) shows elsewhere too (issue #15).
    #[test]
    fn decommitting_keeps_pages_shown_at_other_addresses() {
        use crate::modules::xmem::{
            XMemAllocatePhysicalPages, XMemFreePhysicalPages, XMemMapPhysicalPages,
            XMemVirtualAlloc, map_alias,
        };
        let region = || {
            let r = VirtualAlloc(std::ptr::null_mut(), GRANULARITY, MEM_RESERVE, 0x01);
            assert!(!r.is_null());
            r
        };
        let (mut n, mut page) = (1usize, [0usize]);
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, page.as_mut_ptr()), 1);
        let (a, b) = (region(), region());
        for r in [a, b] {
            assert_eq!(XMemMapPhysicalPages(r, 1, page.as_ptr()), r);
        }
        // SAFETY: mapped read-write.
        unsafe { a.cast::<u8>().write(7) };
        assert_eq!(VirtualFree(a, GRANULARITY, MEM_DECOMMIT), 1);
        // SAFETY: still mapped read-write.
        assert_eq!(unsafe { b.cast::<u8>().read() }, 7, "physical pages keep their bytes");

        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            GRANULARITY,
            MEM_RESERVE | MEM_COMMIT,
            0,
            0x04,
        );
        assert!(!p.is_null());
        let c = region();
        assert!(map_alias(c as u64, p as u64, GRANULARITY as u64));
        // SAFETY: committed read-write.
        unsafe { p.cast::<u8>().write(5) };
        assert_eq!(VirtualFree(p, GRANULARITY, MEM_DECOMMIT), 1);
        // SAFETY: the alias is still mapped read-write.
        assert_eq!(unsafe { c.cast::<u8>().read() }, 5, "an aliased page keeps its bytes");

        for r in [a, b, c, p] {
            assert_eq!(VirtualFree(r, 0, MEM_RELEASE), 1);
        }
        assert_eq!(XMemFreePhysicalPages(1, page.as_ptr()), 1);
    }

    /// `VirtualFree(MEM_DECOMMIT)` with size 0 decommits the whole region from its base, and an
    /// address outside our regions is refused (`ERROR_INVALID_ADDRESS`), not decommitted.
    #[test]
    fn decommit_takes_the_whole_region_and_only_ours() {
        let size = 2 * GRANULARITY;
        let p = VirtualAlloc(std::ptr::null_mut(), size, MEM_RESERVE | MEM_COMMIT, 0x04);
        assert!(!p.is_null());
        assert_eq!(VirtualFree(p, 0, MEM_DECOMMIT), 1);
        let last = p as usize + size - 1;
        assert_eq!(mem::query(last).map(|r| r.protect), Some(Protect::NO_ACCESS));
        assert_eq!(VirtualFree(p, 0, MEM_RELEASE), 1);

        let host = reserve_granular(None, GRANULARITY).expect("reserved");
        // SAFETY: the reservation just made.
        unsafe { mem::commit(host, GRANULARITY, Protect::READ_WRITE) }.expect("committed");
        // SAFETY: committed read-write above.
        unsafe { host.write(3) };
        assert_eq!(VirtualFree(host.cast(), GRANULARITY, MEM_DECOMMIT), 0);
        assert_eq!(GetLastError(), ERROR_INVALID_ADDRESS);
        // SAFETY: still committed read-write.
        assert_eq!(unsafe { host.read() }, 3, "the host's memory is left alone");
        // SAFETY: the reservation made above.
        unsafe { mem::release(host, GRANULARITY) }.expect("released");
    }

    /// `VICTORY_TRACE_OWNERS`: views left in a range `VirtualFree(MEM_DECOMMIT)` gives back, and
    /// a mapping outside every region, are each named with the pool they show.
    #[test]
    fn owners_name_views_left_in_freed_regions_and_mappings_outside_regions() {
        use crate::modules::xmem_physical::{
            TRIPS, XMemAllocatePhysicalPages, XMemFreePhysicalPages, XMemMapPhysicalPages, pool_of,
            pool_shown_at,
        };
        let trips = |case: &str| -> Vec<(usize, usize)> {
            TRIPS.with(|t| {
                t.borrow()
                    .iter()
                    .filter(|(c, ..)| *c == case)
                    .map(|&(_, first, second)| (first, second))
                    .collect()
            })
        };
        const OUTSIDE: &str = "a mapping outside every region";
        let (mut n, mut pages) = (2usize, [0usize; 2]);
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let pool = pool_of(pages[0]);
        // Inside a region of ours: no conflict.
        let r = VirtualAlloc(std::ptr::null_mut(), 2 * GRANULARITY, MEM_RESERVE, 0x01);
        assert!(!r.is_null());
        assert_eq!(XMemMapPhysicalPages(r, 2, pages.as_ptr()), r);
        assert!(trips(OUTSIDE).is_empty());
        // Decommitting or releasing it while the pages show there.
        assert_eq!(VirtualFree(r, GRANULARITY, MEM_DECOMMIT), 1);
        assert_eq!(trips("VirtualFree(MEM_DECOMMIT) over views"), [(0, pool)]);
        // Releasing it gives the views back first (`forget_physical`): nothing is left to name.
        assert_eq!(VirtualFree(r, 0, MEM_RELEASE), 1);
        assert_ne!(pool_shown_at(r as usize), Some(pool));
        assert_eq!(TRIPS.with(|t| t.borrow().len()), 1, "only the decommit");
        // Free address space no region holds: found high up, where other tests' reservations
        // do not land between the release and the mapping.
        // SAFETY: fresh address space, given back at once; only mapped over below.
        let q = unsafe {
            mem::reserve_in(0x5000_0000_0000, 0x5fff_ffff_ffff, GRANULARITY, GRANULARITY)
        }
        .expect("reserved");
        // SAFETY: the reservation just made.
        unsafe { mem::release(q, GRANULARITY) }.expect("released");
        assert_eq!(XMemMapPhysicalPages(q.cast(), 1, pages.as_ptr()), q.cast());
        assert_eq!(trips(OUTSIDE), [(pool, 0)]);
        assert_eq!(XMemMapPhysicalPages(q.cast(), 1, std::ptr::null()), q.cast());
        // SAFETY: the address mapped just above, no longer a view.
        unsafe { mem::release(q, GRANULARITY) }.expect("released");
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    /// Physical pages are mapped over a region of ours, a view of ours or free address space,
    /// never over memory the host has there: on macOS the mapping replaced it silently.
    #[test]
    fn physical_pages_are_not_mapped_over_host_memory() {
        use crate::modules::xmem::{
            XMemAllocatePhysicalPages, XMemFreePhysicalPages, XMemMapPhysicalPages,
            XMemMapPhysicalPagesScatter, map_alias,
        };
        let (mut n, mut pages) = (2usize, [0usize; 2]);
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        // Host memory no region holds, with bytes in it.
        let host = reserve_granular(None, 2 * GRANULARITY).expect("reserved");
        // SAFETY: the reservation just made.
        unsafe { mem::commit(host, 2 * GRANULARITY, Protect::READ_WRITE) }.expect("committed");
        // SAFETY: committed read-write above.
        unsafe { host.write(9) };
        assert!(XMemMapPhysicalPages(host.cast(), 2, pages.as_ptr()).is_null());
        let addresses = [host.cast::<c_void>(), host.wrapping_add(GRANULARITY).cast()];
        assert_eq!(XMemMapPhysicalPagesScatter(addresses.as_ptr(), 2, pages.as_ptr()), 0);
        let r = VirtualAlloc(std::ptr::null_mut(), GRANULARITY, MEM_RESERVE, 0x01);
        assert_eq!(XMemMapPhysicalPages(r, 1, pages.as_ptr()), r);
        assert!(!map_alias(host as u64, r as u64, GRANULARITY as u64));
        // SAFETY: still the host's, read-write.
        assert_eq!(unsafe { host.read() }, 9, "the host's bytes are left alone");
        // SAFETY: the reservation made above, never mapped over.
        unsafe { mem::release(host, 2 * GRANULARITY) }.expect("released");
        assert_eq!(VirtualFree(r, 0, MEM_RELEASE), 1);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    #[test]
    fn unaligned_commit_stays_within_reservation_and_overflow_does_not_panic() {
        let base = VirtualAlloc(std::ptr::null_mut(), GRANULARITY, MEM_RESERVE, 0x01);
        assert!(!base.is_null());
        // Committing the final 256 bytes of a reservation must not spill past the reservation end.
        let tail = base.cast::<u8>().wrapping_add(GRANULARITY - 256).cast();
        let committed = VirtualAlloc(tail, 256, MEM_COMMIT, 0x04);
        let page = mem::page_size();
        assert_eq!(committed as usize, (tail as usize) & !(page - 1));
        // SAFETY: the tail page was committed read-write.
        unsafe {
            tail.cast::<u8>().write(0x5A);
            assert_eq!(tail.cast::<u8>().read(), 0x5A);
        }

        // Overflowing sizes fail cleanly without panicking; zero-size VirtualProtect fails.
        assert!(VirtualAlloc(std::ptr::null_mut(), usize::MAX, MEM_RESERVE, 0x04).is_null());
        assert_eq!(GetLastError(), ERROR_NOT_ENOUGH_MEMORY);
        let mut old = 0u32;
        assert_eq!(VirtualProtect(base, 0, 0x02, &mut old), 0);
        assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
        assert_eq!(VirtualProtect(base, usize::MAX, 0x02, &mut old), 0);
        assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);

        assert_eq!(VirtualFree(base, 0, MEM_RELEASE), 1);
    }

    #[test]
    fn semaphore_bounds_and_wait_all_duplicates_are_rejected() {
        assert!(CreateSemaphoreW(std::ptr::null(), -1, -1, std::ptr::null()).is_null());
        assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
        assert!(CreateSemaphoreW(std::ptr::null(), 0, -5, std::ptr::null()).is_null());
        assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);

        let sem = CreateSemaphoreW(std::ptr::null(), 0, 2, std::ptr::null());
        assert!(!sem.is_null());
        let mut prev = -1;
        assert_eq!(ReleaseSemaphore(sem, 0, &mut prev), 0);
        assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
        assert_eq!(ReleaseSemaphore(sem, -1, &mut prev), 0);
        assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
        assert_eq!(ReleaseSemaphore(sem, 1, &mut prev), 1);
        assert_eq!(prev, 0);
        assert_eq!(CloseHandle(sem), 1);

        let ev = CreateEventW(std::ptr::null(), 1, 1, std::ptr::null());
        assert!(!ev.is_null());
        let dup = [ev, ev];
        assert_eq!(WaitForMultipleObjects(2, dup.as_ptr(), 1, 0), WAIT_FAILED);
        assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
        assert_eq!(WaitForMultipleObjects(2, dup.as_ptr(), 0, 0), WAIT_OBJECT_0);
        assert_eq!(CloseHandle(ev), 1);
    }
}
