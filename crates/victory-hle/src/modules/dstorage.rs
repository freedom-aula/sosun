//! The console's DirectStorage (`XCRDAPI.dll`): a factory that opens files
//! and creates request queues and status arrays the title polls. Its
//! interfaces have grown since 2020 (new interface ids, new slots); the
//! slots here are the ones Grand Theft Auto VI calls, read from its call
//! sites. Slots nobody has needed yet log their offset.
//!
//! Submitted requests of every queue run on one pool of workers, as the
//! console's hardware runs them: in parallel, started in queue order,
//! queues served by weighted round robin over their priorities (about 10x
//! per level, counted in bytes), with at most the staging buffer's worth of
//! compressed source in flight. Each request reads its source (a file or
//! memory), inflates zlib data and/or unpacks BCPack data, and writes the
//! destination (an address, or physical pages). Completion is what stays
//! in order: a status entry is written once every request enqueued before
//! it on its queue has finished, with the first failure (in queue order)
//! since the previous status entry, or success.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ffi::{CStr, c_char, c_void};
#[cfg(windows)]
use std::os::windows::fs::FileExt;
#[cfg(windows)]
use std::os::windows::io::FromRawHandle;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

use victory_formats::bcpack::{self, Tables};

use super::xasync::{E_INVALIDARG, E_NOTIMPL, E_PENDING};
use super::xgameruntime::{Guid, HRESULT, Methods, Object, S_OK, add_ref, missing, put};
use crate::{HleModule, hle_export};

/// The factory interface Grand Theft Auto VI asks for.
const FACTORY: Guid = Guid::new(
    0x6295_9d11,
    0xd45f,
    0x4884,
    [0xa0, 0xa3, 0xf7, 0x86, 0x2e, 0xa2, 0x43, 0xae],
);
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_HANDLE_EOF)`: the source ends early.
const END_OF_FILE: HRESULT = 0x8007_0026_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_INVALID_DATA)`: a bad compressed stream.
const INVALID_DATA: HRESULT = 0x8007_000D_u32 as i32;

type Out = *mut *mut c_void;

static FACTORY_OBJECT: Object = Object::new("dstorage factory", &FACTORY_VTABLE);

pub extern "system" fn DStorageGetFactory(iid: *const Guid, out: Out) -> HRESULT {
    // SAFETY: the title passes a GUID (null tolerated).
    match unsafe { iid.as_ref() } {
        Some(&iid) if iid == FACTORY => put(out, FACTORY_OBJECT.expose()),
        iid => {
            tracing::warn!(iid = ?iid.map(Guid::to_string), "DStorageGetFactory: unknown interface");
            E_NOINTERFACE
        }
    }
}

/// One of our objects with state: the runtime's object header (method
/// table first), a reference count, then `T`.
#[repr(C)]
struct Instance<T> {
    base: Object,
    refs: AtomicU32,
    state: T,
}

impl<T> Instance<T> {
    fn create(base: Object, state: T) -> *mut c_void {
        let instance = Box::into_raw(Box::new(Instance {
            base,
            refs: AtomicU32::new(1),
            state,
        }));
        // SAFETY: just allocated.
        unsafe { (*instance).base.expose() }
    }
}

extern "system" fn add_ref_instance<T>(this: *const Instance<T>) -> u32 {
    // SAFETY: an instance we created and the title still holds.
    unsafe { &*this }.refs.fetch_add(1, Ordering::Relaxed) + 1
}

extern "system" fn release<T>(this: *mut Instance<T>) -> u32 {
    // SAFETY: as above.
    let left = unsafe { &*this }.refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: the last reference; created by `Instance::create`.
        drop(unsafe { Box::from_raw(this) });
    }
    left
}

fn iid_text(iid: *const Guid) -> String {
    // SAFETY: the title passes a GUID (null tolerated).
    unsafe { iid.as_ref() }.map_or_else(|| "null".into(), Guid::to_string)
}

/// `HRESULT_FROM_WIN32` of a host error.
#[cfg(windows)]
fn from_win32(error: &std::io::Error) -> HRESULT {
    error
        .raw_os_error()
        .map_or(E_INVALIDARG, |e| (0x8007_0000 | (e as u32 & 0xffff)) as i32)
}

/// The Win32 error a POSIX one stands for, as an `HRESULT`: the inverse of the UCRT's
/// `_dosmaperr` table (Win32 error to `errno`), with `ERROR_READ_FAULT` for `EIO` and the rest.
#[cfg(not(windows))]
fn from_win32(error: &std::io::Error) -> HRESULT {
    use std::io::ErrorKind;
    // `errno` values below 35 are the same on every Unix.
    let code: u32 = match (error.raw_os_error(), error.kind()) {
        (Some(9), _) => 6,                     // EBADF: ERROR_INVALID_HANDLE
        (Some(23 | 24), _) => 4,               // ENFILE, EMFILE: ERROR_TOO_MANY_OPEN_FILES
        (Some(28), _) => 112,                  // ENOSPC: ERROR_DISK_FULL
        (_, ErrorKind::NotFound) => 2,         // ENOENT: ERROR_FILE_NOT_FOUND
        (_, ErrorKind::PermissionDenied) => 5, // EACCES, EPERM: ERROR_ACCESS_DENIED
        (_, ErrorKind::OutOfMemory) => 8,      // ENOMEM: ERROR_NOT_ENOUGH_MEMORY
        (_, ErrorKind::InvalidInput) => 87,    // EINVAL: ERROR_INVALID_PARAMETER
        _ => 30,                               // EIO and the rest: ERROR_READ_FAULT
    };
    (0x8007_0000 | code) as i32
}

/// The `HRESULT` of a failed read: `END_OF_FILE` past the end, else the host's error.
fn read_hresult(error: &std::io::Error) -> HRESULT {
    if error.kind() == std::io::ErrorKind::UnexpectedEof {
        END_OF_FILE
    } else {
        from_win32(error)
    }
}

/// A host error for the log: its text (with the OS error number) and kind.
fn host_error(error: &std::io::Error) -> String {
    format!("{error} ({:?})", error.kind())
}

/// Reads at `offset` without moving a file position (`ReadFile` with an
/// offset on Windows, `pread` elsewhere).
fn read_at(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    #[cfg(windows)]
    {
        file.seek_read(buf, offset)
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::FileExt::read_at(file, buf, offset)
    }
}

// The factory.

type Factory = *const Object;

/// Opens `path` for reading; the file object owns the handle.
extern "system" fn open_file(_: Factory, path: *const u16, iid: *const Guid, out: Out) -> HRESULT {
    if path.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes a NUL-terminated path.
    let name = unsafe {
        let len = (0..).take_while(|&i| *path.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(path, len))
    };
    let file = match open_for_reading(path, &name) {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(path = name, %error, "dstorage: opening a file failed");
            return from_win32(&error);
        }
    };
    tracing::info!(path = name, iid = iid_text(iid), "dstorage: file opened");
    let id = file_id(&file);
    let file = Instance::create(
        Object::new("dstorage file", &FILE_VTABLE),
        File {
            file,
            path: name,
            id,
        },
    );
    put(out, file)
}

/// Goes through `files` on Windows, so paths on our virtual drives work too.
#[cfg(windows)]
fn open_for_reading(path: *const u16, _name: &str) -> std::io::Result<std::fs::File> {
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 1;
    const OPEN_EXISTING: u32 = 3;
    let handle = super::files::CreateFileW(
        path,
        GENERIC_READ,
        FILE_SHARE_READ,
        std::ptr::null(),
        OPEN_EXISTING,
        0,
        std::ptr::null_mut(),
    );
    if handle as isize == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a handle we just opened and nobody else owns.
    Ok(unsafe { std::fs::File::from_raw_handle(handle) })
}

/// Goes through the POSIX host's drive table, so virtual drives work too.
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
fn open_for_reading(path: *const u16, _name: &str) -> std::io::Result<std::fs::File> {
    // SAFETY: the title passes a NUL-terminated path.
    let wide = unsafe {
        std::slice::from_raw_parts(path, (0..).take_while(|&i| *path.add(i) != 0).count())
    };
    let host = crate::host_posix::files::translate(wide)
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
    std::fs::File::open(host)
}

#[cfg(not(any(windows, all(target_os = "macos", target_arch = "x86_64"))))]
fn open_for_reading(_path: *const u16, name: &str) -> std::io::Result<std::fs::File> {
    std::fs::File::open(name)
}

/// `(capacity, name, iid, out)`.
extern "system" fn create_status_array(
    _: Factory,
    capacity: u32,
    name: *const c_char,
    iid: *const Guid,
    out: Out,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    tracing::info!(
        capacity,
        name = text(name),
        iid = iid_text(iid),
        "dstorage: status array"
    );
    let entries = (0..capacity).map(|_| AtomicI32::new(S_OK)).collect();
    let array = Instance::create(
        Object::new("dstorage status array", &STATUS_VTABLE),
        StatusArray { entries },
    );
    put(out, array)
}

/// The queue's description: as in 2020 (source type, capacity, priority,
/// name), plus a field Grand Theft Auto VI sets to 1.
#[repr(C)]
struct QueueDesc {
    source_type: u64,
    capacity: u16,
    priority: i8,
    name: *const c_char,
    unknown: u64,
}

extern "system" fn create_queue(
    _: Factory,
    desc: *const QueueDesc,
    iid: *const Guid,
    out: Out,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes its description (null tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    let name = text(desc.name);
    tracing::info!(
        source = desc.source_type,
        capacity = desc.capacity,
        priority = desc.priority,
        name,
        unknown = desc.unknown,
        iid = iid_text(iid),
        "dstorage: queue"
    );
    let Some(scheduler) = scheduler() else {
        return E_OUTOFMEMORY;
    };
    let mut lanes = scheduler.lock();
    let id = lanes.next_id;
    lanes.next_id += 1;
    let lane = Lane::new(desc.priority, desc.capacity);
    lanes.lanes.insert(id, lane);
    drop(lanes);
    put(
        out,
        Instance::create(Object::new("dstorage queue", &QUEUE_VTABLE), Queue { id }),
    )
}

/// Grand Theft Auto VI asks for 32 MiB and stops if this fails.
extern "system" fn set_staging_buffer_size(_: Factory, size: u32) -> HRESULT {
    tracing::info!(size, "dstorage: staging buffer size");
    if let Some(scheduler) = scheduler() {
        scheduler.lock().staging = u64::from(size);
    }
    S_OK
}

fn text(name: *const c_char) -> String {
    if name.is_null() {
        return String::new();
    }
    // SAFETY: a NUL-terminated name from the title.
    unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned()
}

static FACTORY_VTABLE: Methods<16> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    missing::<3> as *const (),            // 0x18 (2020: queue creation)
    open_file as *const (),               // 0x20
    create_status_array as *const (),     // 0x28
    missing::<6> as *const (),            // 0x30 (2020: CPU affinity)
    missing::<7> as *const (),            // 0x38 (2020: debug flags)
    set_staging_buffer_size as *const (), // 0x40
    missing::<9> as *const (),
    create_queue as *const (), // 0x50
    missing::<11> as *const (),
    missing::<12> as *const (),
    missing::<13> as *const (),
    missing::<14> as *const (),
    missing::<15> as *const (),
]);

// Files.

struct File {
    file: std::fs::File,
    /// As the title named it (diagnostics).
    path: String,
    /// The volume and inode the file is on: requests of one pack share
    /// read-ahead blocks whatever object they came through, and two files
    /// never share one. `None` off Unix, where the cache is off.
    id: Option<(u64, u64)>,
}

/// The file's identity for the read-ahead cache.
#[cfg(unix)]
fn file_id(file: &std::fs::File) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata().ok()?;
    Some((m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn file_id(_file: &std::fs::File) -> Option<(u64, u64)> {
    None
}

static FILE_VTABLE: Methods<8> = Methods([
    missing::<0> as *const (),
    add_ref_instance::<File> as *const (),
    release::<File> as *const (),
    missing::<3> as *const (),
    missing::<4> as *const (),
    missing::<5> as *const (),
    missing::<6> as *const (),
    missing::<7> as *const (),
]);

// Status arrays.

struct StatusArray {
    entries: Box<[AtomicI32]>,
}

type Array = *mut Instance<StatusArray>;

/// The result of the requests before entry `index`: pending, the first
/// failure, or success.
extern "system" fn status_result(this: Array, index: u32) -> HRESULT {
    // SAFETY: an array we created and the title still holds.
    let entries = &unsafe { &*this }.state.entries;
    entries
        .get(index as usize)
        .map_or(E_INVALIDARG, |e| e.load(Ordering::Acquire))
}

/// Grand Theft Auto VI polls this (entry 0, a `Sleep(1)` loop) and never
/// reads the result itself.
extern "system" fn is_complete(this: Array, index: u32) -> bool {
    let pending = status_result(this, index) == E_PENDING;
    if pending {
        // Debugging: a request polled this long is stuck; say where its queue's requests are.
        // The title spins on the entry about a million times a second, so the
        // first report comes quickly and the rest only every ten millionth
        // poll (about once per ten seconds of spinning) — the log stays
        // readable while a load is stuck instead of drowning in repeats.
        static POLLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = POLLS.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 5000 || n.is_multiple_of(10_000_000) {
            log_pending(this, index, n);
        }
    }
    !pending
}

/// Logs the state of every lane (the scheduler's queue) for a status entry still pending.
#[inline(never)]
fn log_pending(this: Array, index: u32, polls: u64) {
    let Some(s) = scheduler() else { return };
    let lanes = s.lock();
    let state: Vec<String> = lanes
        .lanes
        .iter()
        .filter(|(_, l)| l.entries() > 0)
        .map(|(id, l)| {
            format!(
                "lane {id}: unsubmitted {} held {} reads {} statuses {} running {} failures {} capacity {}",
                l.unsubmitted.len(),
                l.held.len(),
                l.reads.len(),
                l.statuses.len(),
                l.running.len(),
                l.failures.len(),
                l.capacity
            )
        })
        .collect();
    tracing::warn!(array = ?this, index, polls, staged = lanes.staged, ?state, "dstorage: a status entry is still pending");
}

static STATUS_VTABLE: Methods<5> = Methods([
    missing::<0> as *const (),
    add_ref_instance::<StatusArray> as *const (),
    release::<StatusArray> as *const (),
    is_complete as *const (),   // 0x18
    status_result as *const (), // 0x20
]);

// Queues.

/// A queue object: its lane in the scheduler.
struct Queue {
    id: u64,
}

impl Drop for Queue {
    fn drop(&mut self) {
        close_lane(self.id);
    }
}

type QueueRef = *const Instance<Queue>;

/// Every queue's requests, run by a pool of workers.
struct Scheduler {
    lanes: Mutex<Lanes>,
    /// Requests to start, or staging space freed.
    work: Condvar,
    /// A request finished (for full queues and `close`).
    finished: Condvar,
}

/// Priority weights per class (low, normal, high and real-time), as the
/// console's: about 10x per level, counted against request sizes.
const COST: [u64; 3] = [100, 10, 1];
/// The staging limit: the most decompression scratch (and compressed source)
/// the workers may hold at once. The console's dedicated decompressor has a
/// 32 MiB staging buffer; the host has none, so this stands for the host's
/// memory instead — 512 MiB keeps a load's requests in parallel without
/// letting a wave of large ones evict the page cache (which is what turns a
/// ten-second load into a black screen and a watchdog crash on a 16 GB Mac).
const STAGING: u64 = 512 << 20;

/// `VICTORY_DSTORAGE_STAGING_MB=<mib>`: the staging limit, overriding the
/// 512 MiB default. `0` and anything unparsable leave the default.
fn staging_limit() -> u64 {
    std::env::var("VICTORY_DSTORAGE_STAGING_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&mb| mb > 0)
        .map_or(STAGING, |mb| mb << 20)
}

struct Lanes {
    lanes: HashMap<u64, Lane>,
    next_id: u64,
    /// Submission order over all queues.
    next_seq: u64,
    /// Bytes of staging held by running requests, and the limit.
    staged: u64,
    staging: u64,
    /// A start waited for staging space since the last finish.
    starved: bool,
    /// Weighted round robin: each class's virtual time (bytes × cost), and
    /// the time of the last start; within a class, the lane clock of the
    /// last start.
    clock: [u64; 3],
    now: u64,
    lane_now: [u64; 3],
    /// Since the last debug summary: requests, bytes written, time spent
    /// running them.
    done: (u64, u64, std::time::Duration),
    refused: u64,
    /// BCPack bytes written and time spent on them.
    bcpack: (u64, std::time::Duration),
    /// Requests into physical pages.
    pages: u64,
    since: Option<std::time::Instant>,
}

/// A queue's requests in the scheduler.
struct Lane {
    class: usize,
    capacity: usize,
    /// Bytes started, for round robin among the lanes of a class.
    clock: u64,
    /// Enqueued, not yet submitted.
    unsubmitted: Vec<Command>,
    /// Left out when the queue submitted itself, until the title's Submit:
    /// reads into physical pages and the status entries behind them.
    held: Vec<Command>,
    /// Submitted, by sequence number: reads not started, status entries
    /// not written, reads running, and failures not yet reported.
    reads: VecDeque<(u64, Request)>,
    statuses: VecDeque<(u64, Status)>,
    running: BTreeSet<u64>,
    failures: BTreeMap<u64, HRESULT>,
    closed: bool,
}

// SAFETY: requests point into title memory and our own objects, which stay
// valid until the title has seen them complete; status arrays count their
// references atomically.
unsafe impl Send for Lane {}

/// A read a worker runs.
struct Job {
    lane: u64,
    seq: u64,
    request: Request,
    staging: u64,
}

// SAFETY: as for `Lane`.
unsafe impl Send for Job {}

/// The scheduler, with its workers started on first use; `None` if not
/// even one worker could start.
fn scheduler() -> Option<&'static Scheduler> {
    static SCHEDULER: OnceLock<Option<Scheduler>> = OnceLock::new();
    static STARTED: std::sync::Once = std::sync::Once::new();
    let scheduler = SCHEDULER
        .get_or_init(|| {
            Some(Scheduler {
                lanes: Mutex::new(Lanes {
                    lanes: HashMap::new(),
                    next_id: 0,
                    next_seq: 0,
                    staged: 0,
                    staging: staging_limit(),
                    starved: false,
                    clock: [0; 3],
                    now: 0,
                    lane_now: [0; 3],
                    done: Default::default(),
                    refused: 0,
                    bcpack: Default::default(),
                    pages: 0,
                    since: None,
                }),
                work: Condvar::new(),
                finished: Condvar::new(),
            })
        })
        .as_ref()?;
    STARTED.call_once(|| {
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
        let started = (0..workers)
            .filter(|i| {
                std::thread::Builder::new()
                    .name(format!("dstorage {i}"))
                    .spawn(|| work(scheduler))
                    .is_ok()
            })
            .count();
        tracing::info!(workers = started, "dstorage: workers");
        if std::env::var_os("VICTORY_DSTORAGE_LOG").is_some() {
            let _ = std::thread::Builder::new().name("dstorage log".into()).spawn(|| {
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    let g = scheduler.lock();
                    let lanes: Vec<String> = g
                        .lanes
                        .iter()
                        .filter(|(_, l)| l.entries() > 0)
                        .map(|(id, l)| {
                            format!(
                                "lane {id}: unsubmitted {} held {} reads {} statuses {} running {} cap {}",
                                l.unsubmitted.len(), l.held.len(), l.reads.len(), l.statuses.len(), l.running.len(), l.capacity
                            )
                        })
                        .collect();
                    tracing::warn!(staged = g.staged, staging = g.staging, ?lanes, "dstorage state");
                }
            });
        }
    });
    Some(scheduler)
}

impl Scheduler {
    fn lock(&self) -> MutexGuard<'_, Lanes> {
        self.lanes.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Puts the calling thread below the title's own: `QOS_CLASS_DEFAULT` with the lowest relative
/// priority it allows, as the shader prewarm pool does. The dstorage workers are many (one a
/// core) and busy through a load; the title's threads are few and latency-sensitive.
#[cfg(target_os = "macos")]
fn lower_priority() {
    const QOS_CLASS_DEFAULT: u32 = 0x15;
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos: u32, relative_priority: i32) -> i32;
    }
    // SAFETY: libSystem's documented call, for the calling thread only.
    unsafe { pthread_set_qos_class_self_np(QOS_CLASS_DEFAULT, -15) };
}

#[cfg(not(target_os = "macos"))]
fn lower_priority() {}

/// The process's physical footprint in MiB, as Activity Monitor's "Memory"
/// column counts it (anonymous and IOKit memory, compressed pages, and more;
/// `task_info`'s `phys_footprint`), for the load's 5 s summary — the number
/// that says whether the host is thrashing. `None` where it cannot be read
/// (or off macOS, where the summary omits it).
#[cfg(target_os = "macos")]
fn footprint() -> Option<u64> {
    #[repr(C)]
    struct TaskVmInfo {
        virtual_size: u64,
        region_count: i32,
        page_size: i32,
        rest: [u64; 40],
    }
    // SAFETY: libSystem's documented calls, for this process only. `phys_footprint`
    // is the sixteenth `mach_vm_size_t` of `task_vm_info` (byte offset 136):
    // `resident_size` .. `compressed_lifetime`, then it.
    unsafe extern "C" {
        fn mach_task_self() -> u32;
        fn task_info(target: u32, flavor: u32, info: *mut TaskVmInfo, count: *mut u32) -> i32;
    }
    const TASK_VM_INFO: u32 = 22;
    let mut info = TaskVmInfo {
        virtual_size: 0,
        region_count: 0,
        page_size: 0,
        rest: [0; 40],
    };
    let mut count = (std::mem::size_of::<TaskVmInfo>() / 4) as u32;
    // SAFETY: this task's own info, into a `TaskVmInfo` of the declared count.
    let ok = unsafe { task_info(mach_task_self(), TASK_VM_INFO, &mut info, &mut count) };
    (ok == 0).then(|| info.rest[15] >> 20)
}

/// Off macOS the summary's `footprint_mib` says `None`.
#[cfg(not(target_os = "macos"))]
fn footprint() -> Option<u64> {
    None
}

/// A worker: runs the next read of the most deserving queue. Runs below the title's own
/// threads' priority, so a load's BCPack and zlib work never starves the title's render thread
/// or its status-array polls (its `Sleep(1)` loop sees a finished request within a millisecond
/// of the worker's condvar-woken finish, not whenever the workers happen to yield).
fn work(scheduler: &'static Scheduler) {
    lower_priority();
    let mut lanes = scheduler.lock();
    loop {
        let Some(job) = lanes.start() else {
            lanes = scheduler
                .work
                .wait(lanes)
                .unwrap_or_else(|e| e.into_inner());
            continue;
        };
        drop(lanes);
        let started = std::time::Instant::now();
        let result = execute(&job.request);
        lanes = scheduler.lock();
        lanes.finish(&job, result);
        lanes.count(&job, started);
        if std::mem::take(&mut lanes.starved) {
            scheduler.work.notify_all();
        }
        scheduler.finished.notify_all();
    }
}

impl Lanes {
    /// Counts a finished request; every 5 s a debug line sums them up.
    fn count(&mut self, job: &Job, started: std::time::Instant) {
        let (requests, bytes, busy) = &mut self.done;
        *requests += 1;
        *bytes += u64::from(job.request.destination_size);
        *busy += started.elapsed();
        if job.request.options & BCPACK != 0 {
            self.bcpack.0 += u64::from(job.request.destination_size);
            self.bcpack.1 += started.elapsed();
        }
        if job.request.options & PAGE_DESTINATION != 0 {
            self.pages += 1;
        }
        let since = *self.since.get_or_insert(started);
        let elapsed = since.elapsed();
        if elapsed.as_secs() >= 5 {
            let waiting: usize = self.lanes.values().map(|l| l.reads.len()).sum();
            let reading = READING.swap(0, Ordering::Relaxed) as f64 / 1e9;
            let (hits, misses, read) = readahead::take_stats();
            tracing::info!(
                requests = *requests,
                mb_per_s = format_args!("{:.1}", *bytes as f64 / elapsed.as_secs_f64() / 1e6),
                busy_workers = format_args!("{:.2}", busy.as_secs_f64() / elapsed.as_secs_f64()),
                reading_workers = format_args!("{:.2}", reading / elapsed.as_secs_f64()),
                staging_waits = std::mem::take(&mut self.refused),
                bcpack_mb = self.bcpack.0 / 1_000_000,
                bcpack_workers =
                    format_args!("{:.2}", self.bcpack.1.as_secs_f64() / elapsed.as_secs_f64()),
                page_destinations = std::mem::take(&mut self.pages),
                waiting,
                staged_mib = self.staged >> 20,
                readahead_hits = hits,
                readahead_misses = misses,
                readahead_read_mb = read / 1_000_000,
                footprint_mib = footprint(),
                "dstorage: last {:.1} s",
                elapsed.as_secs_f64()
            );
            self.done = Default::default();
            self.bcpack = Default::default();
            self.since = None;
        }
    }

    /// Starts the next read of the class with the least virtual time, from
    /// its lane that has started the fewest bytes, if the staging limit has
    /// room for it (or nothing else holds any). A read that does not fit is
    /// passed over — the next lane's, and then the next class's, are tried —
    /// so one large request at the head of a queue cannot leave every worker
    /// asleep with reads waiting behind it; `starved` is only set when
    /// nothing waiting fits, which is when a finish has to wake the pool.
    fn start(&mut self) -> Option<Job> {
        let mut classes = [0usize, 1, 2];
        classes.sort_by_key(|&c| self.clock[c]);
        let mut order: Vec<(u64, u64, u64)> = Vec::new();
        for &class in &classes {
            order.clear();
            order.extend(
                self.lanes
                    .iter()
                    .filter(|(_, l)| l.class == class && !l.reads.is_empty())
                    .filter_map(|(&id, l)| Some((l.clock, l.reads.front()?.0, id))),
            );
            order.sort_unstable();
            for (_, _, id) in &order {
                let fits = self
                    .lanes
                    .get(id)
                    .and_then(|l| l.reads.front())
                    .is_some_and(|(_, r)| {
                        self.staged == 0 || self.staged + staging_bytes(r) <= self.staging
                    });
                if !fits {
                    continue;
                }
                let Some(lane) = self.lanes.get_mut(id) else {
                    continue;
                };
                let Some((seq, request)) = lane.reads.pop_front() else {
                    continue;
                };
                let staging = staging_bytes(&request);
                lane.running.insert(seq);
                self.staged += staging;
                let size = u64::from(request.destination_size.max(request.source_size)).max(1);
                self.now = self.clock[class];
                self.clock[class] += size * COST[class];
                self.lane_now[class] = lane.clock;
                lane.clock += size;
                return Some(Job {
                    lane: *id,
                    seq,
                    request,
                    staging,
                });
            }
        }
        if self.lanes.values().any(|l| !l.reads.is_empty()) {
            self.starved = true;
            self.refused += 1;
        }
        None
    }

    fn finish(&mut self, job: &Job, result: Result<(), HRESULT>) {
        self.staged -= job.staging;
        if let Some(lane) = self.lanes.get_mut(&job.lane) {
            lane.running.remove(&job.seq);
            if let Err(hr) = result {
                lane.failures.insert(job.seq, hr);
            }
            lane.flush();
        }
    }

    /// Moves a queue's enqueued commands to the workers (the title's
    /// Submit).
    fn submit(&mut self, id: u64) {
        self.submit_as(id, true);
    }

    /// As [`Lanes::submit`]; `explicit` false when a half-full queue submits
    /// itself, which leaves reads into physical pages for the title's
    /// Submit: their page lists point into the title's page table, which
    /// Grand Theft Auto VI fills for the batch in its own submit path, right
    /// before Submit (`sub_1400e88f0`), so until then they name no page or a
    /// page that now holds something else. A status entry behind a held
    /// read waits with it.
    fn submit_as(&mut self, id: u64, explicit: bool) {
        let Some(class) = self.lanes.get(&id).map(|l| l.class) else {
            return;
        };
        // A class that had nothing waiting starts from now, not from the
        // credit it left behind.
        if !self
            .lanes
            .values()
            .any(|l| l.class == class && !l.reads.is_empty())
        {
            self.clock[class] = self.clock[class].max(self.now);
        }
        let Some(lane) = self.lanes.get_mut(&id) else {
            return;
        };
        if lane.reads.is_empty() {
            lane.clock = lane.clock.max(self.lane_now[class]);
        }
        let mut commands = if explicit {
            std::mem::take(&mut lane.held)
        } else {
            Vec::new()
        };
        commands.append(&mut lane.unsubmitted);
        for command in commands {
            let hold = !explicit
                && match &command {
                    Command::Read(r) => r.options & PAGE_DESTINATION != 0,
                    Command::Status(_) => !lane.held.is_empty(),
                };
            if hold {
                lane.held.push(command);
                continue;
            }
            let seq = self.next_seq;
            self.next_seq += 1;
            match command {
                Command::Read(request) => lane.reads.push_back((seq, request)),
                Command::Status(status) => lane.statuses.push_back((seq, status)),
            }
        }
        lane.flush();
    }
}

impl Lane {
    fn new(priority: i8, capacity: u16) -> Lane {
        Lane {
            class: priority.clamp(-1, 1).wrapping_add(1) as usize,
            capacity: usize::from(capacity),
            clock: 0,
            unsubmitted: Vec::new(),
            held: Vec::new(),
            reads: VecDeque::new(),
            statuses: VecDeque::new(),
            running: BTreeSet::new(),
            failures: BTreeMap::new(),
            closed: false,
        }
    }

    /// Requests and status entries held, submitted or not.
    fn entries(&self) -> usize {
        self.unsubmitted.len()
            + self.held.len()
            + self.reads.len()
            + self.statuses.len()
            + self.running.len()
    }

    /// Writes the status entries whose earlier reads have all finished.
    fn flush(&mut self) {
        while let Some(&(seq, _)) = self.statuses.front() {
            let waiting = self.reads.front().map(|r| r.0);
            if waiting
                .into_iter()
                .chain(self.running.first().copied())
                .any(|s| s < seq)
            {
                return;
            }
            let Some((_, status)) = self.statuses.pop_front() else {
                return;
            };
            let later = self.failures.split_off(&seq);
            let first = std::mem::replace(&mut self.failures, later)
                .into_values()
                .next();
            status
                .entry()
                .store(first.unwrap_or(S_OK), Ordering::Release);
        }
    }
}

/// Staging bytes a request holds while it runs: everything the host buffers
/// between its source and its destination. Raw reads into an address go
/// straight to their destination and memory sources need none (as on the
/// console), but a compressed or paged request also holds decompression
/// scratch: the BCPack decoder's per-block records (up to twice the
/// destination again) and the copy a paged destination takes when its pages
/// are not one straight window. Grand Theft Auto VI's load streams requests
/// like that by the dozen; uncounted, ten workers' worth of scratch is
/// gigabytes at once, which is the difference between a load that fits on a
/// 16 GB host and one that thrashes it (every `pread` then takes a disk
/// latency, and the title's status entries never complete).
fn staging_bytes(request: &Request) -> u64 {
    let buffered = request.options & (ZLIB | BCPACK | PAGE_DESTINATION) != 0;
    if !buffered {
        return 0;
    }
    let mut bytes = 0u64;
    if request.options & MEMORY_SOURCE == 0 {
        bytes += u64::from(request.source_size);
        if request.options & ZLIB != 0 && request.options & BCPACK != 0 {
            bytes += u64::from(request.intermediate_size);
        }
    }
    if request.options & PAGE_DESTINATION != 0 {
        bytes += u64::from(request.destination_size);
    }
    if request.options & BCPACK != 0 {
        bytes += 2 * u64::from(request.destination_size);
    }
    bytes
}

/// Ends a queue: what was not started is dropped, and this waits for the
/// reads already running (they write title memory).
fn close_lane(id: u64) {
    let Some(scheduler) = scheduler() else {
        return;
    };
    let mut lanes = scheduler.lock();
    let Some(lane) = lanes.lanes.get_mut(&id) else {
        return;
    };
    lane.closed = true;
    lane.unsubmitted.clear();
    lane.held.clear();
    lane.reads.clear();
    lane.statuses.clear(); // releases their arrays
    while lanes.lanes.get(&id).is_some_and(|l| !l.running.is_empty()) {
        lanes = scheduler
            .finished
            .wait(lanes)
            .unwrap_or_else(|e| e.into_inner());
    }
    lanes.lanes.remove(&id);
    scheduler.finished.notify_all();
}

/// A read request as the title builds it, 0x48 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct Request {
    options: u64,
    /// An address, or with `PAGE_DESTINATION` an array of 64 KiB page
    /// numbers.
    destination: *mut c_void,
    /// With `PAGE_DESTINATION`, the low 16 bits are where in the first page
    /// the destination starts.
    page_offset: u64,
    destination_size: u32,
    /// Our file object, or with `MEMORY_SOURCE` an address.
    source: *const c_void,
    offset: u64,
    source_size: u32,
    /// Compressed size between two decompression stages; unused here.
    intermediate_size: u32,
    /// Matched by cancellation; Grand Theft Auto VI leaves it 0.
    tag: u64,
    name: *const c_char,
}

// Request options.
/// Inflate zlib (RFC 1950) data.
const ZLIB: u64 = 1;
/// Bits 1-4: decode BCPack data of this BC format number (0: none).
const BCPACK: u64 = 0xf << 1;
const PAGE_DESTINATION: u64 = 1 << 21;
const MEMORY_SOURCE: u64 = 1 << 22;
// Bits 5-20 select a further transform, bit 23 a source in physical pages;
// Grand Theft Auto VI's packages need neither, so requests with them fail.

const PAGE: usize = 64 * 1024;

enum Command {
    Read(Request),
    Status(Status),
}

/// A status entry to write, holding a reference on its array.
struct Status {
    array: Array,
    index: u32,
}

impl Drop for Status {
    fn drop(&mut self) {
        release(self.array);
    }
}

impl Status {
    fn entry(&self) -> &AtomicI32 {
        // SAFETY: we hold a reference; the index was checked when enqueued.
        &unsafe { &*self.array }.state.entries[self.index as usize]
    }
}

extern "system" fn enqueue_request(this: QueueRef, request: *const Request) {
    // SAFETY: the title passes its request (null tolerated).
    let Some(&request) = (unsafe { request.as_ref() }) else {
        return;
    };
    queue(this, Command::Read(request));
}

extern "system" fn enqueue_status(this: QueueRef, array: Array, index: u32) {
    // SAFETY: the title passes an array we created (null tolerated).
    let Some(entries) = (unsafe { array.as_ref() }).map(|a| &a.state.entries) else {
        return;
    };
    let Some(entry) = entries.get(index as usize) else {
        tracing::warn!(index, "dstorage: status entry out of range");
        return;
    };
    entry.store(E_PENDING, Ordering::Release);
    add_ref_instance(array);
    queue(this, Command::Status(Status { array, index }));
}

/// Adds a command to the queue; blocks while the queue is full, and
/// submits on its own once half of it is enqueued, as the console does.
fn queue(this: QueueRef, command: Command) {
    // SAFETY: a queue we created and the title still holds.
    let id = unsafe { &*this }.state.id;
    let Some(scheduler) = scheduler() else {
        return;
    };
    let mut lanes = scheduler.lock();
    loop {
        let Some(lane) = lanes.lanes.get(&id).filter(|l| !l.closed) else {
            return; // closed: dropped (a status releases its array)
        };
        // One slot stays free, as the console's count of empty slots says.
        if lane.entries() + 1 < lane.capacity.max(2) {
            break;
        }
        // Full with reads held for a Submit the title cannot make while it
        // waits here: they go now, page lists as they are.
        if !lane.held.is_empty() {
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "dstorage: a full queue's reads into physical pages start before the title's Submit"
                );
            }
            lanes.submit(id);
            scheduler.work.notify_all();
        }
        lanes = scheduler
            .finished
            .wait(lanes)
            .unwrap_or_else(|e| e.into_inner());
    }
    let Some(lane) = lanes.lanes.get_mut(&id) else {
        return;
    };
    lane.unsubmitted.push(command);
    if lane.unsubmitted.len() >= (lane.capacity / 2).max(1) {
        let capacity = lane.capacity;
        lanes.submit_as(id, false);
        scheduler.work.notify_all();
        let held = lanes.lanes.get(&id).map_or(0, |l| l.held.len());
        let n = AUTO_SUBMITS.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_power_of_two() {
            tracing::info!(
                n,
                capacity,
                held,
                "dstorage: a queue half full submitted itself; reads into physical pages wait for the title's Submit"
            );
        }
    }
}

extern "system" fn submit(this: QueueRef) {
    // SAFETY: as above.
    let id = unsafe { &*this }.state.id;
    let Some(scheduler) = scheduler() else {
        return;
    };
    let mut lanes = scheduler.lock();
    let pending = lanes
        .lanes
        .get(&id)
        .map_or(0, |l| l.unsubmitted.len() + l.held.len());
    tracing::debug!(commands = pending, "dstorage: submit");
    lanes.submit(id);
    scheduler.work.notify_all();
}

/// Ends the queue whatever its reference count: what was not started never
/// completes, and this returns once the reads already running have
/// finished. Grand Theft Auto VI calls this, not `Release`.
extern "system" fn close(this: QueueRef) {
    // SAFETY: as above.
    close_lane(unsafe { &*this }.state.id);
}

/// Reads, inflates and/or unpacks, and writes one request.
fn execute(request: &Request) -> Result<(), HRESULT> {
    let options = request.options;
    let fail = |hr: HRESULT, why: &str| {
        tracing::warn!(
            options = format_args!("{options:#x}"),
            offset = request.offset,
            source_size = request.source_size,
            destination_size = request.destination_size,
            intermediate_size = request.intermediate_size,
            tag = request.tag,
            name = text(request.name),
            hr = format_args!("{:#010x}", hr as u32),
            "dstorage: request failed: {why}"
        );
        hr
    };
    if options & !(ZLIB | BCPACK | PAGE_DESTINATION | MEMORY_SOURCE) != 0 {
        return Err(fail(E_NOTIMPL, "unsupported options"));
    }
    if request.source.is_null() || request.destination.is_null() {
        return Err(fail(E_INVALIDARG, "no source or destination"));
    }
    if std::env::var_os("VICTORY_TRACE_DSTORAGE_FILES").is_some() {
        static SEEN: Mutex<BTreeMap<String, (u64, u64)>> = Mutex::new(BTreeMap::new());
        let name = source_name(request).to_owned();
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        let e = seen.entry(name.clone()).or_default();
        e.0 += 1;
        e.1 += u64::from(request.source_size);
        if e.0 == 1 || e.0.is_power_of_two() {
            tracing::info!(
                file = name,
                requests = e.0,
                bytes = e.1,
                "dstorage: reads of a file so far"
            );
        }
    }
    let bc = ((options & BCPACK) >> 1) as u8;
    let size = request.destination_size as usize;
    if options & (ZLIB | BCPACK) == 0 {
        if request.source_size > request.destination_size {
            return Err(fail(E_INVALIDARG, "source larger than destination"));
        }
        let size = request.source_size as usize;
        let mut read = None;
        return into_destination(request, size, |out| {
            read_into(request, out).map(|()| size).map_err(|error| {
                let hr = read_hresult(&error);
                read = Some(error);
                hr
            })
        })
        .map_err(|hr| match &read {
            Some(error) => fail(hr, &format!("copying: {}", host_error(error))),
            None => fail(hr, "copying"),
        });
    }
    let tables = if bc != 0 {
        Some(bcpack_tables()?)
    } else {
        None
    };
    let source = read_source(request).map_err(|error| {
        let why = format!("reading the source: {}", host_error(&error));
        fail(read_hresult(&error), &why)
    })?;
    // The console's order: zlib first, then BCPack.
    let source = if bc != 0 && options & ZLIB != 0 {
        let mut packed = vec![0; request.intermediate_size as usize];
        let n = inflate(&source, &mut packed).map_err(|hr| fail(hr, "zlib"))?;
        packed.truncate(n);
        Cow::Owned(packed)
    } else {
        source
    };
    into_destination(request, size, |out| match tables {
        Some(tables) => bcpack::decode(bc, &source, out, tables)
            .map(|()| out.len())
            .map_err(|e| fail(INVALID_DATA, &e.to_string())),
        None => inflate(&source, out).map_err(|hr| fail(hr, "zlib")),
    })?;
    tracing::trace!(
        options = format_args!("{options:#x}"),
        offset = request.offset,
        size,
        "dstorage: request done"
    );
    Ok(())
}

/// Inflates a zlib stream into `out`, returning the bytes written. A stream
/// that ends short of `out` passes: the title's chunks fill it exactly, and
/// nothing says that is required.
fn inflate(source: &[u8], out: &mut [u8]) -> Result<usize, HRESULT> {
    miniz_oxide::inflate::decompress_slice_iter_to_slice(out, std::iter::once(source), true, false)
        .map_err(|status| {
            tracing::debug!(?status, "dstorage: zlib stream");
            INVALID_DATA
        })
}

/// The BCPack format's constant tables, parsed once for the process. If
/// they do not parse every BCPack request fails: one warning, then debug.
fn bcpack_tables() -> Result<&'static Tables, HRESULT> {
    static TABLES: OnceLock<Result<Tables, String>> = OnceLock::new();
    static WARNED: AtomicBool = AtomicBool::new(false);
    match TABLES.get_or_init(load_bcpack_tables) {
        Ok(tables) => Ok(tables),
        Err(why) if !WARNED.swap(true, Ordering::Relaxed) => {
            tracing::warn!("dstorage: BCPack requests fail: {why} (more at debug)");
            Err(E_NOTIMPL)
        }
        Err(_) => {
            tracing::debug!("dstorage: BCPack request failed: no tables");
            Err(E_NOTIMPL)
        }
    }
}

/// The tables VIctory ships with.
#[cfg(not(test))]
fn load_bcpack_tables() -> Result<Tables, String> {
    Tables::builtin().map_err(|e| format!("the built-in BCPack tables: {e}"))
}

/// Tests decode streams made against tables of their own, from this folder.
#[cfg(test)]
static BCPACK_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();

#[cfg(test)]
fn use_bcpack_tables(dir: std::path::PathBuf) {
    let _ = BCPACK_DIR.set(dir);
}

#[cfg(test)]
fn load_bcpack_tables() -> Result<Tables, String> {
    let dir = BCPACK_DIR
        .get()
        .ok_or("no folder set for the test tables")?;
    let bytes = std::fs::read(dir.join("tables.bin")).map_err(|e| e.to_string())?;
    Tables::parse(&bytes).map_err(|e| e.to_string())
}

/// A compressed request's source: borrowed from memory, or read from the
/// file.
fn read_source(request: &Request) -> std::io::Result<Cow<'static, [u8]>> {
    let size = request.source_size as usize;
    if request.options & MEMORY_SOURCE != 0 {
        // SAFETY: the title's source holds `size` bytes until completion.
        let bytes = unsafe { std::slice::from_raw_parts(request.source.cast(), size) };
        return Ok(Cow::Borrowed(bytes));
    }
    let mut bytes = vec![0; size];
    read_into(request, &mut bytes)?;
    Ok(Cow::Owned(bytes))
}

/// The shared read-ahead cache: the title streams its pack files in many small
/// pieces in roughly ascending order, and the host's volume (an external disk)
/// serves a 128 KiB read barely slower than a 20 KiB one — a miss reads a
/// whole aligned block, the pieces after it are hits, and a run of ascending
/// misses reads up to a MiB ahead in one go. `VICTORY_READAHEAD_MB` caps the
/// cache (0 turns it off); the direct path stays for reads of a block or more.
mod readahead {
    use std::collections::{HashMap, VecDeque};
    #[cfg(windows)]
    use std::os::windows::fs::FileExt;
    use std::sync::{Arc, Mutex, OnceLock};

    /// A block's size: small enough a stray read wastes little, large enough
    /// that a resource's pieces share one.
    pub(super) const BLOCK: u64 = 128 << 10;
    /// Reads of this size or less go through the cache; bigger ones are
    /// single-shot with no reuse to offer it.
    pub(super) const MAX: usize = 1 << 20;
    /// Blocks an ascending miss reads: the stream's next pieces are hits.
    const RUN: u64 = 8;
    /// The whole cache's budget.
    const BUDGET: u64 = 256 << 20;

    /// (file id, block) to its bytes, with the tick it was last used at.
    type Blocks = HashMap<(u64, u64), (u64, Arc<Vec<u8>>)>;

    struct Cache {
        blocks: Blocks,
        /// The keys least-recently-used first.
        order: VecDeque<(u64, u64)>,
        bytes: u64,
        budget: u64,
        tick: u64,
        /// Each file's last miss, for the ascending-run readahead.
        last: HashMap<u64, u64>,
        /// Diagnostics.
        hits: u64,
        misses: u64,
        read: u64,
    }

    fn cache() -> Option<&'static Mutex<Cache>> {
        static CACHE: OnceLock<Option<Mutex<Cache>>> = OnceLock::new();
        CACHE
            .get_or_init(|| {
                let budget = budget();
                (budget > 0).then(|| {
                    Mutex::new(Cache {
                        blocks: HashMap::new(),
                        order: VecDeque::new(),
                        bytes: 0,
                        budget,
                        tick: 0,
                        last: HashMap::new(),
                        hits: 0,
                        misses: 0,
                        read: 0,
                    })
                })
            })
            .as_ref()
    }

    /// Takes the read-ahead counters since the last call, for the load's summary.
    pub(super) fn take_stats() -> (u64, u64, u64) {
        cache().map_or((0, 0, 0), |c| {
            let mut c = c.lock().unwrap_or_else(|e| e.into_inner());
            (
                std::mem::take(&mut c.hits),
                std::mem::take(&mut c.misses),
                std::mem::take(&mut c.read),
            )
        })
    }

    /// Fills `out` from `offset` of the file, through the cache. A file whose
    /// identity cannot be read bypasses it.
    pub(super) fn fill(
        file: &std::fs::File,
        id: (u64, u64),
        offset: u64,
        out: &mut [u8],
    ) -> std::io::Result<()> {
        let Some(lock) = cache() else {
            return fill_at(out, offset, |buf, at| read_at(file, buf, at)).and_then(|n| {
                (n == out.len())
                    .then_some(())
                    .ok_or(std::io::ErrorKind::UnexpectedEof.into())
            });
        };
        let mut done = 0;
        while done < out.len() {
            // Where the next unfilled byte sits: its block, and how far in.
            let at = offset + done as u64;
            let block = at / BLOCK;
            let skip = (at % BLOCK) as usize;
            let bytes = get(lock, file, id, block)?;
            let take = bytes.len().saturating_sub(skip).min(out.len() - done);
            if take == 0 {
                // The block ended short of the need: the file's end.
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            out[done..done + take].copy_from_slice(&bytes[skip..skip + take]);
            done += take;
        }
        Ok(())
    }

    /// A block's bytes: the cache's, or the file's with readahead.
    fn get(
        lock: &Mutex<Cache>,
        file: &std::fs::File,
        id: (u64, u64),
        block: u64,
    ) -> std::io::Result<Arc<Vec<u8>>> {
        let key = (id.0.wrapping_mul(31).wrapping_add(id.1), block);
        let mut c = lock.lock().unwrap_or_else(|e| e.into_inner());
        c.tick += 1;
        if c.blocks.contains_key(&key) {
            let now = c.tick;
            c.hits += 1;
            c.order.retain(|&k| k != key);
            c.order.push_back(key);
            if let Some((tick, bytes)) = c.blocks.get_mut(&key) {
                *tick = now;
                return Ok(bytes.clone());
            }
        }
        c.misses += 1;
        // An ascending run — the last miss just below this one — reads ahead.
        let run = if c
            .last
            .get(&key.0)
            .is_some_and(|&l| block > l && block - l <= RUN)
        {
            RUN
        } else {
            1
        };
        c.last.insert(key.0, block);
        drop(c);
        let at = block * BLOCK;
        let mut bytes = vec![0; (run * BLOCK) as usize];
        let n = fill_at(&mut bytes, at, |buf, at| read_at(file, buf, at))?;
        // A short read is the file's end: the entry is what was read, no more.
        bytes.truncate(n);
        let mut c = lock.lock().unwrap_or_else(|e| e.into_inner());
        c.read += n as u64;
        // The span as one entry per block it covers: a later request starting
        // inside it is a hit from its own first block on.
        let whole = n / BLOCK as usize;
        for i in 0..whole as u64 {
            let k = (key.0, block + i);
            let part = bytes[(i * BLOCK) as usize..][..BLOCK as usize].to_vec();
            insert(&mut c, k, Arc::new(part));
        }
        let rest = n % BLOCK as usize;
        if rest > 0 {
            let k = (key.0, block + whole as u64);
            let part = bytes[whole * BLOCK as usize..].to_vec();
            insert(&mut c, k, Arc::new(part));
        }
        let mine = if whole > 0 {
            bytes[..BLOCK as usize].to_vec()
        } else {
            bytes
        };
        Ok(Arc::new(mine))
    }

    /// Puts a block in, evicting the least-recently-used over the budget.
    fn insert(c: &mut Cache, key: (u64, u64), bytes: Arc<Vec<u8>>) {
        if let Some((_, old)) = c.blocks.insert(key, (c.tick, bytes.clone())) {
            c.bytes -= old.len() as u64;
        } else {
            c.order.push_back(key);
        }
        c.bytes += bytes.len() as u64;
        while c.bytes > c.budget && c.order.len() > 1 {
            let Some(front) = c.order.pop_front() else {
                break;
            };
            if let Some((_, old)) = c.blocks.remove(&front) {
                c.bytes = c.bytes.saturating_sub(old.len() as u64);
            }
        }
    }

    fn budget() -> u64 {
        std::env::var("VICTORY_READAHEAD_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(BUDGET, |mb| mb << 20)
    }

    /// Reads at `offset` without moving a file position, as the request path does.
    fn read_at(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        #[cfg(windows)]
        {
            file.seek_read(buf, offset)
        }
        #[cfg(not(windows))]
        {
            std::os::unix::fs::FileExt::read_at(file, buf, offset)
        }
    }

    /// `fill_at` of the request path, copied for this private module.
    fn fill_at(
        out: &mut [u8],
        offset: u64,
        mut read: impl FnMut(&mut [u8], u64) -> std::io::Result<usize>,
    ) -> std::io::Result<usize> {
        let mut done = 0;
        while done < out.len() {
            let Some(at) = offset.checked_add(done as u64) else {
                return Err(std::io::ErrorKind::InvalidInput.into());
            };
            match read(&mut out[done..], at) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(done)
    }

    #[cfg(all(test, unix))]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicU64, Ordering as Seq};

        /// A file with a known body, opened as the requests open theirs. Each
        /// test's file is its own, and stays alive for the process: the cache
        /// is keyed by the file's identity, and an inode freed and handed to a
        /// later file would bring its stale blocks with it.
        fn temp(body: &[u8]) -> (std::fs::File, (u64, u64)) {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let n = NEXT.fetch_add(1, Seq::Relaxed);
            let path =
                std::env::temp_dir().join(format!("victory-readahead-{}-{n}", std::process::id()));
            std::fs::write(&path, body).unwrap();
            let file = std::fs::File::open(&path).unwrap();
            use std::os::unix::fs::MetadataExt;
            let m = file.metadata().unwrap();
            std::mem::forget(file.try_clone().unwrap());
            (file, (m.dev(), m.ino()))
        }

        /// Pieces of one block are hits after the first; an ascending run
        /// fills the blocks ahead; a later piece is served from them.
        #[test]
        fn pieces_of_a_run_are_hits() {
            let body: Vec<u8> = (0..(BLOCK as usize * 3)).map(|i| (i % 251) as u8).collect();
            let (file, id) = temp(&body);
            let mut out = [0u8; 20_000];
            fill(&file, id, 100, &mut out).unwrap();
            assert_eq!(&out, &body[100..100 + out.len()]);
            let mut out = [0u8; 30_000];
            fill(&file, id, 30_000, &mut out).unwrap();
            assert_eq!(&out, &body[30_000..30_000 + out.len()]);
            let (hits, _, _) = take_stats();
            assert!(hits >= 2, "the second piece is a hit: {hits}");
        }

        /// A read that starts inside a block is served from where it starts,
        /// not from the block's first byte.
        #[test]
        fn a_read_starts_where_it_says() {
            let body: Vec<u8> = (0..(BLOCK as usize * 2)).map(|i| (i % 251) as u8).collect();
            let (file, id) = temp(&body);
            let mut out = [0u8; 4_096];
            fill(&file, id, 100_000, &mut out).unwrap();
            assert_eq!(&out, &body[100_000..100_000 + out.len()]);
        }

        /// A read past the end of the file is `UnexpectedEof`, as the direct
        /// path's `Ok(0)` is.
        #[test]
        fn past_the_end_is_unexpected_eof() {
            let (file, id) = temp(&[7; 100]);
            let mut out = [0u8; 200];
            assert!(matches!(
                fill(&file, id, 0, &mut out),
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof
            ));
        }

        /// A read across a block boundary gets both blocks' pieces.
        #[test]
        fn a_read_across_a_boundary_is_whole() {
            let body: Vec<u8> = (0..(BLOCK as usize * 2)).map(|i| (i % 251) as u8).collect();
            let (file, id) = temp(&body);
            let start = BLOCK as usize - 2_048;
            let mut out = [0u8; 4_096];
            fill(&file, id, start as u64, &mut out).unwrap();
            assert_eq!(&out, &body[start..start + out.len()]);
        }
    }
}

/// Nanoseconds workers spent reading files, for the debug summary.
static READING: AtomicU64 = AtomicU64::new(0);

/// Fills `out` (the source size) from the request's file or memory. File
/// reads of a MiB or less go through the read-ahead cache; the direct path
/// stays for the big ones, which have no reuse to offer it.
fn read_into(request: &Request, out: &mut [u8]) -> std::io::Result<()> {
    if request.options & MEMORY_SOURCE != 0 {
        // SAFETY: the title's source holds `out.len()` bytes until completion.
        out.copy_from_slice(unsafe {
            std::slice::from_raw_parts(request.source.cast(), out.len())
        });
        return Ok(());
    }
    // SAFETY: a file object we created; the title keeps it open.
    let state = &unsafe { &*request.source.cast::<Instance<File>>() }.state;
    let file = &state.file;
    let started = std::time::Instant::now();
    let result = match (state.id, out.len() <= readahead::MAX) {
        (Some(id), true) => readahead::fill(file, id, request.offset, out),
        _ => fill_at(out, request.offset, |buf, at| read_at(file, buf, at)),
    };
    READING.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    result
}

/// Fills `out` with `read(buf, offset)` from `offset` on, as `read_exact` does: short reads go
/// on, `Interrupted` (EINTR: this process takes many signals) is tried again, and the end of
/// the file is `UnexpectedEof`.
fn fill_at(
    out: &mut [u8],
    offset: u64,
    mut read: impl FnMut(&mut [u8], u64) -> std::io::Result<usize>,
) -> std::io::Result<()> {
    let mut done = 0;
    while done < out.len() {
        let Some(at) = offset.checked_add(done as u64) else {
            return Err(std::io::ErrorKind::InvalidInput.into());
        };
        match read(&mut out[done..], at) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => done += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Runs `fill` on the destination's first `len` bytes; `fill` returns how
/// many it produced. GPU-visible destinations are host memory like the
/// rest (GPU address = CPU address), so an address is written in place;
/// physical pages are too when they are consecutive pages of one
/// allocation, and otherwise go through a buffer.
fn into_destination(
    request: &Request,
    len: usize,
    fill: impl FnOnce(&mut [u8]) -> Result<usize, HRESULT>,
) -> Result<(), HRESULT> {
    if request.options & PAGE_DESTINATION == 0 {
        // Raw reads land through the kernel, which does not fault into the
        // GPU side's guard on title buffers; open them first.
        #[cfg(windows)]
        let _writing = victory_gpu::kernel_will_write(request.destination as usize, len);
        // SAFETY: the title's destination holds `destination_size` bytes, at
        // least `len`, and nothing else writes it until completion.
        let out = unsafe { std::slice::from_raw_parts_mut(request.destination.cast::<u8>(), len) };
        return fill(out).map(drop);
    }
    let offset = (request.page_offset & 0xffff) as usize;
    let count = (offset + len).div_ceil(PAGE);
    // The title's page list, copied: the title may still be filling it
    // (below), so each attempt is judged by the list it wrote through, not
    // by a later read of it.
    let list = || {
        // SAFETY: the title passes a page number per 64 KiB of destination,
        // which holds at least `len` bytes from `offset`.
        unsafe { std::slice::from_raw_parts(request.destination.cast::<usize>(), count) }.to_vec()
    };
    // Consecutive pages of one allocation (the usual case): written in
    // place through one view.
    if let Some(mut window) = super::xmem::page_window(&list(), offset, len) {
        return fill(window.bytes()).map(drop);
    }
    let mut data = vec![0; len];
    let n = fill(&mut data)?;
    let mut pages = list();
    if super::xmem::write_physical(&pages, offset, &data[..n]) {
        return Ok(());
    }
    // Page number 0 is no page (numbers start at 0x10_0000): a list naming
    // it is one the title has not filled yet. The title writes a request's
    // page list after enqueueing it, and a queue half full is submitted on
    // its own, so a read can start first. Read the list again for a few
    // milliseconds before failing: the title never looks at a request's
    // result, so a failed one is a model or texture left unwritten.
    let mut rereads = 0;
    while pages.contains(&0) && rereads < LATE_LIST_TRIES {
        rereads += 1;
        std::thread::sleep(std::time::Duration::from_millis(1));
        pages = list();
        if !pages.contains(&0) && super::xmem::write_physical(&pages, offset, &data[..n]) {
            let late = LATE_LISTS.fetch_add(1, Ordering::Relaxed) + 1;
            if late.is_power_of_two() {
                tracing::info!(
                    late,
                    rereads,
                    "dstorage: a page list filled after its request started: written late"
                );
            }
            return Ok(());
        }
    }
    // `still_empty`: the list still named page 0 after the last read;
    // else the pages it names could not be written.
    tracing::warn!(
        offset,
        len,
        pages = pages.len(),
        still_empty = pages.contains(&0),
        rereads,
        unknown = super::xmem::unknown_pages(&pages),
        list = format_args!("{:x?}", &pages[..pages.len().min(8)]),
        options = format_args!("{:#x}", request.options),
        file = source_name(request),
        file_offset = request.offset,
        "dstorage: a destination's physical pages could not be written; its request fails"
    );
    Err(E_INVALIDARG)
}

/// The file a request reads, as the title named it, or "memory".
fn source_name(request: &Request) -> &str {
    if request.options & MEMORY_SOURCE != 0 {
        return "memory";
    }
    if request.source.is_null() {
        return "<null>";
    }
    // SAFETY: a file object we created; the title keeps it open.
    &unsafe { &*request.source.cast::<Instance<File>>() }
        .state
        .path
}

/// Queues that submitted themselves, half full (diagnostics).
static AUTO_SUBMITS: AtomicU32 = AtomicU32::new(0);

/// Milliseconds a page list naming page 0 is read again for.
const LATE_LIST_TRIES: u32 = 8;
/// Page lists filled after their request started (written late).
static LATE_LISTS: AtomicU32 = AtomicU32::new(0);

/// Slots by the 2020 order, which the argument shapes at Grand Theft Auto
/// VI's call sites match (0x18, 0x20, 0x30 and 0x40 are called).
static QUEUE_VTABLE: Methods<12> = Methods([
    missing::<0> as *const (),
    add_ref_instance::<Queue> as *const (),
    release::<Queue> as *const (),
    enqueue_request as *const (), // 0x18
    enqueue_status as *const (),  // 0x20
    missing::<5> as *const (),    // 0x28 (2020: fence signal)
    submit as *const (),          // 0x30
    missing::<7> as *const (),    // 0x38 (2020: cancel by tag)
    close as *const (),           // 0x40
    missing::<9> as *const (),    // 0x48 (2020: error event)
    missing::<10> as *const (),   // 0x50 (2020: error record)
    missing::<11> as *const (),   // 0x58 (2020: queue info)
]);

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "dstorage",
        dlls: &["xcrdapi.dll"],
        exports: vec![hle_export!(DStorageGetFactory)],
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::modules::xgameruntime::method;

    /// A read interrupted by a signal is tried again, and short reads go on, until `out` is full.
    #[test]
    fn reads_go_on_past_signals_and_short_reads() {
        let file: Vec<u8> = (0..10).collect();
        let mut calls = Vec::new();
        let mut out = [0; 6];
        let result = fill_at(&mut out, 3, |buf, at| {
            calls.push(at);
            if calls.len() == 1 {
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            let n = buf.len().min(2);
            buf[..n].copy_from_slice(&file[at as usize..at as usize + n]);
            Ok(n)
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(out, [3, 4, 5, 6, 7, 8]);
        assert_eq!(calls, [3, 3, 5, 7]);
        let short = fill_at(&mut [0; 4], 0, |_, _| Ok(0)).unwrap_err();
        assert_eq!(read_hresult(&short), END_OF_FILE);
    }

    /// Host errors reach the title as the Win32 errors Windows would give (`_dosmaperr`).
    #[cfg(not(windows))]
    #[test]
    fn host_errors_map_to_their_win32_codes() {
        let hr = |errno| from_win32(&std::io::Error::from_raw_os_error(errno)) as u32;
        for (errno, win32) in [
            (2, 2),    // ENOENT: ERROR_FILE_NOT_FOUND
            (1, 5),    // EPERM: ERROR_ACCESS_DENIED
            (13, 5),   // EACCES
            (12, 8),   // ENOMEM: ERROR_NOT_ENOUGH_MEMORY
            (9, 6),    // EBADF: ERROR_INVALID_HANDLE
            (24, 4),   // EMFILE: ERROR_TOO_MANY_OPEN_FILES
            (23, 4),   // ENFILE
            (22, 87),  // EINVAL: ERROR_INVALID_PARAMETER
            (28, 112), // ENOSPC: ERROR_DISK_FULL
            (5, 30),   // EIO: ERROR_READ_FAULT
            (35, 30),  // anything else
        ] {
            assert_eq!(hr(errno), 0x8007_0000 | win32, "errno {errno}");
        }
        let eof = std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
        assert_eq!(read_hresult(&eof), END_OF_FILE);
    }

    fn call<F: Copy>(object: *mut c_void, slot: usize) -> F {
        // SAFETY: every object here starts with the runtime header.
        method(unsafe { &*object.cast::<Object>() }, slot)
    }

    fn factory() -> *mut c_void {
        let mut factory = std::ptr::null_mut();
        assert_eq!(DStorageGetFactory(&FACTORY, &mut factory), S_OK);
        factory
    }

    fn open(path: &std::path::Path) -> *mut c_void {
        let wide: Vec<u16> = path.to_str().unwrap().encode_utf16().chain([0]).collect();
        let open: extern "system" fn(*mut c_void, *const u16, *const Guid, Out) -> HRESULT =
            call(factory(), 0x20);
        let mut file = std::ptr::null_mut();
        assert_eq!(
            open(factory(), wide.as_ptr(), std::ptr::null(), &mut file),
            S_OK
        );
        file
    }

    fn status_array(capacity: u32) -> *mut c_void {
        let create: extern "system" fn(
            *mut c_void,
            u32,
            *const c_char,
            *const Guid,
            Out,
        ) -> HRESULT = call(factory(), 0x28);
        let mut array = std::ptr::null_mut();
        let name = c"test".as_ptr();
        assert_eq!(
            create(factory(), capacity, name, std::ptr::null(), &mut array),
            S_OK
        );
        array
    }

    fn new_queue(source_type: u64) -> *mut c_void {
        let create: extern "system" fn(*mut c_void, *const QueueDesc, *const Guid, Out) -> HRESULT =
            call(factory(), 0x50);
        let desc = QueueDesc {
            source_type,
            capacity: 0x80,
            priority: 0,
            name: c"test".as_ptr(),
            unknown: 1,
        };
        let mut queue = std::ptr::null_mut();
        assert_eq!(create(factory(), &desc, std::ptr::null(), &mut queue), S_OK);
        queue
    }

    fn request(
        options: u64,
        destination: *mut c_void,
        destination_size: usize,
        source: *const c_void,
        offset: u64,
        source_size: usize,
    ) -> Request {
        Request {
            options,
            destination,
            page_offset: 0,
            destination_size: destination_size as u32,
            source,
            offset,
            source_size: source_size as u32,
            intermediate_size: 0,
            tag: 0,
            name: std::ptr::null(),
        }
    }

    fn enqueue(queue: *mut c_void, request: &Request) {
        let f: extern "system" fn(*mut c_void, *const Request) = call(queue, 0x18);
        f(queue, request);
    }

    fn enqueue_status(queue: *mut c_void, array: *mut c_void, index: u32) {
        let f: extern "system" fn(*mut c_void, *mut c_void, u32) = call(queue, 0x20);
        f(queue, array, index);
    }

    fn void(object: *mut c_void, slot: usize) {
        let f: extern "system" fn(*mut c_void) = call(object, slot);
        f(object);
    }

    fn result(array: *mut c_void, index: u32) -> HRESULT {
        let f: extern "system" fn(*mut c_void, u32) -> HRESULT = call(array, 0x20);
        f(array, index)
    }

    /// Polls as the title does, then returns the entry's result.
    fn wait(array: *mut c_void, index: u32) -> HRESULT {
        let done: extern "system" fn(*mut c_void, u32) -> bool = call(array, 0x18);
        let start = Instant::now();
        while !done(array, index) {
            assert!(start.elapsed() < Duration::from_secs(10), "never completed");
            std::thread::sleep(Duration::from_millis(1));
        }
        result(array, index)
    }

    fn file_with(bytes: &[u8]) -> (tempfile::TempDir, *mut c_void) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, bytes).unwrap();
        let file = open(&path);
        (dir, file)
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    #[test]
    fn request_layout_is_the_titles() {
        assert_eq!(size_of::<Request>(), 0x48);
        assert_eq!(std::mem::offset_of!(Request, destination_size), 0x18);
        assert_eq!(std::mem::offset_of!(Request, source), 0x20);
        assert_eq!(std::mem::offset_of!(Request, source_size), 0x30);
        assert_eq!(std::mem::offset_of!(Request, name), 0x40);
        assert_eq!(std::mem::offset_of!(QueueDesc, unknown), 0x18);
    }

    #[test]
    fn raw_and_zlib_reads_from_a_file() {
        let plain = pattern(300_000);
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);
        let mut contents = plain.clone();
        contents.extend_from_slice(&packed);
        let (_dir, file) = file_with(&contents);
        let (queue, array) = (new_queue(0), status_array(1));

        let mut raw = vec![0u8; 1000];
        let mut inflated = vec![0u8; plain.len()];
        let at = |b: &mut Vec<u8>| b.as_mut_ptr().cast();
        enqueue(queue, &request(0, at(&mut raw), 1000, file, 5000, 1000));
        let offset = plain.len() as u64;
        let zlib = request(
            ZLIB,
            at(&mut inflated),
            plain.len(),
            file,
            offset,
            packed.len(),
        );
        enqueue(queue, &zlib);
        enqueue_status(queue, array, 0);
        assert_eq!(result(array, 0), E_PENDING, "pending until done");
        void(queue, 0x30);
        assert_eq!(wait(array, 0), S_OK);
        assert_eq!(raw, plain[5000..6000]);
        assert_eq!(inflated, plain);
        void(queue, 0x40);
    }

    #[test]
    fn memory_sources_inflate_too() {
        let plain = pattern(70_000);
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);
        let (queue, array) = (new_queue(1), status_array(1));
        let mut out = vec![0u8; plain.len()];
        let options = ZLIB | MEMORY_SOURCE;
        let r = request(
            options,
            out.as_mut_ptr().cast(),
            out.len(),
            packed.as_ptr().cast(),
            0,
            packed.len(),
        );
        enqueue(queue, &r);
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), S_OK);
        assert_eq!(out, plain);
    }

    #[test]
    fn statuses_complete_in_order_with_the_first_failure() {
        let contents = pattern(4096);
        let (_dir, file) = file_with(&contents);
        let (queue, array) = (new_queue(0), status_array(3));
        // Requests in flight together run in any order, so none of them
        // shares a destination (failed ones leave theirs undefined).
        let mut buffers = [[0u8; 16]; 5];
        let [failed, failed_too, out, other, bcpack] =
            buffers.each_mut().map(|b| b.as_mut_ptr().cast());

        // Segment 0: a read past the end, then a bad stream; the first wins.
        enqueue(queue, &request(0, failed, 16, file, 4090, 16));
        enqueue(queue, &request(ZLIB, failed_too, 16, file, 0, 16));
        enqueue_status(queue, array, 0);
        // Segment 1: two reads.
        enqueue(queue, &request(0, other, 16, file, 0, 16));
        enqueue(queue, &request(0, out, 16, file, 100, 16));
        enqueue_status(queue, array, 1);
        void(queue, 0x30);
        // Segment 2: a BCPack request (BC7) whose stream is empty fails.
        with_test_tables();
        enqueue(queue, &request(7 << 1, bcpack, 16, file, 0, 0));
        enqueue_status(queue, array, 2);
        void(queue, 0x30);

        assert_eq!(wait(array, 2), INVALID_DATA);
        assert_eq!(result(array, 0), END_OF_FILE);
        assert_eq!(result(array, 1), S_OK);
        assert_eq!(buffers[2], contents[100..116]);
        assert_eq!(buffers[3], contents[..16]);
    }

    #[test]
    fn half_a_queue_submits_itself() {
        let (_dir, file) = file_with(&pattern(64));
        let create: extern "system" fn(*mut c_void, *const QueueDesc, *const Guid, Out) -> HRESULT =
            call(factory(), 0x50);
        let desc = QueueDesc {
            source_type: 0,
            capacity: 4,
            priority: 1,
            name: c"small".as_ptr(),
            unknown: 1,
        };
        let mut queue = std::ptr::null_mut();
        assert_eq!(create(factory(), &desc, std::ptr::null(), &mut queue), S_OK);
        let array = status_array(1);
        let mut out = [0u8; 16];
        enqueue(queue, &request(0, out.as_mut_ptr().cast(), 16, file, 8, 16));
        enqueue_status(queue, array, 0); // the second of four: no submit needed
        assert_eq!(wait(array, 0), S_OK);
        assert_eq!(out, pattern(64)[8..24]);
    }

    /// Lanes without workers: what `start` picks, and when statuses land.
    fn lanes(staging: u64) -> Lanes {
        Lanes {
            lanes: HashMap::new(),
            next_id: 0,
            next_seq: 0,
            staged: 0,
            staging,
            starved: false,
            clock: [0; 3],
            now: 0,
            lane_now: [0; 3],
            done: Default::default(),
            refused: 0,
            bcpack: Default::default(),
            pages: 0,
            since: None,
        }
    }

    fn read(lanes: &mut Lanes, id: u64, options: u64, size: u32) {
        let mut r = request(options, std::ptr::null_mut(), 0, std::ptr::null(), 0, 0);
        (r.destination_size, r.source_size) = (size, size);
        let lane = lanes.lanes.get_mut(&id).unwrap();
        lane.unsubmitted.push(Command::Read(r));
    }

    #[test]
    fn priorities_share_by_bytes_about_tenfold() {
        let mut l = lanes(STAGING);
        l.lanes.insert(0, Lane::new(0, 4096));
        l.lanes.insert(1, Lane::new(1, 4096));
        l.lanes.insert(2, Lane::new(2, 4096)); // real-time: as high
        for _ in 0..400 {
            for id in 0..3 {
                read(&mut l, id, 0, 4096);
            }
        }
        (0..3).for_each(|id| l.submit(id));
        let mut started = [0u32; 3];
        for _ in 0..330 {
            let job = l.start().unwrap();
            started[job.lane as usize] += 1;
            l.finish(&job, Ok(()));
        }
        // Normal gets about a tenth of what high and real-time get together.
        assert_eq!(started[1] + started[2], 300, "{started:?}");
        assert_eq!(started[0], 30, "{started:?}");
        assert!(started[1].abs_diff(started[2]) <= 1, "{started:?}");
    }

    #[test]
    fn staging_bounds_compressed_bytes_in_flight() {
        let mut l = lanes(100);
        l.lanes.insert(0, Lane::new(0, 64));
        read(&mut l, 0, ZLIB, 60);
        read(&mut l, 0, ZLIB, 60);
        read(&mut l, 0, 0, 1000); // raw: straight to its destination
        l.submit(0);
        let first = l.start().unwrap();
        assert!(l.start().is_none(), "60 + 60 > 100");
        l.finish(&first, Ok(()));
        let second = l.start().unwrap();
        let raw = l.start().unwrap();
        assert_eq!((second.staging, raw.staging), (60, 0));
        // Alone, a request larger than the buffer still runs.
        read(&mut l, 0, ZLIB, 500);
        l.submit(0);
        assert!(l.start().is_none());
        l.finish(&second, Ok(()));
        assert!(l.start().is_some());
    }

    #[test]
    fn staging_counts_the_decompression_scratch_too() {
        let mut l = lanes(1000);
        l.lanes.insert(0, Lane::new(0, 64));
        // A BCPack read: its source, plus twice its destination in records.
        let mut r = request(4 << 1, std::ptr::null_mut(), 16, std::ptr::null(), 0, 0);
        (r.destination_size, r.source_size) = (100, 40);
        let lane = l.lanes.get_mut(&0).unwrap();
        lane.unsubmitted.push(Command::Read(r));
        // A paged raw read: its source, plus its destination as the copy.
        let mut p = request(
            PAGE_DESTINATION,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            0,
            0,
        );
        (p.destination_size, p.source_size) = (60, 20);
        let lane = l.lanes.get_mut(&0).unwrap();
        lane.unsubmitted.push(Command::Read(p));
        // zlib and BCPack together: the intermediate stage is counted as well.
        let mut z = request(
            ZLIB | 4 << 1,
            std::ptr::null_mut(),
            16,
            std::ptr::null(),
            0,
            0,
        );
        (z.destination_size, z.source_size, z.intermediate_size) = (10, 4, 7);
        let lane = l.lanes.get_mut(&0).unwrap();
        lane.unsubmitted.push(Command::Read(z));
        l.submit(0);
        let a = l.start().unwrap();
        assert_eq!(a.staging, 40 + 2 * 100, "the source and the records");
        l.finish(&a, Ok(()));
        let b = l.start().unwrap();
        assert_eq!(b.staging, 20 + 60, "the source and the paged copy");
        l.finish(&b, Ok(()));
        let c = l.start().unwrap();
        assert_eq!(
            c.staging,
            4 + 7 + 2 * 10,
            "source, intermediate and records"
        );
    }

    #[test]
    fn a_read_that_does_not_fit_does_not_block_the_pool() {
        let mut l = lanes(100);
        l.lanes.insert(0, Lane::new(0, 64));
        l.lanes.insert(1, Lane::new(0, 64));
        read(&mut l, 1, ZLIB, 40);
        read(&mut l, 1, ZLIB, 40);
        read(&mut l, 0, ZLIB, 500); // alone it would run, but not beside anything
        l.submit(1);
        l.submit(0);
        let first = l.start().unwrap();
        assert_eq!((first.lane, first.staging), (1, 40));
        assert!(!l.starved, "something was started, so nothing is starved");
        // The deserving lane is the large read's now (the fewest bytes
        // started); it does not fit, so the other lane's read goes instead.
        let second = l.start().unwrap();
        assert_eq!((second.lane, second.staging), (1, 40));
        // Only the oversized read is left: nothing fits, and that is starvation.
        assert!(l.start().is_none());
        assert!(l.starved);
        assert_eq!(l.refused, 1);
        l.finish(&first, Ok(()));
        l.finish(&second, Ok(()));
        // With the pool empty it runs, as the console's staging would let it.
        let alone = l.start().unwrap();
        assert_eq!(alone.lane, 0);
        assert_eq!(alone.staging, 500);
    }

    #[test]
    fn a_status_waits_for_every_earlier_read() {
        let mut l = lanes(STAGING);
        l.lanes.insert(0, Lane::new(0, 64));
        let array = status_array(2).cast::<Instance<StatusArray>>();
        let status = |index| {
            add_ref_instance(array);
            Command::Status(Status { array, index })
        };
        read(&mut l, 0, 0, 16);
        read(&mut l, 0, 0, 16);
        l.lanes.get_mut(&0).unwrap().unsubmitted.push(status(0));
        read(&mut l, 0, 0, 16);
        l.lanes.get_mut(&0).unwrap().unsubmitted.push(status(1));
        // SAFETY: an array we created.
        for entry in unsafe { &(*array).state.entries }.iter() {
            entry.store(E_PENDING, Ordering::Release);
        }
        l.submit(0);
        let jobs: Vec<Job> = (0..3).map(|_| l.start().unwrap()).collect();
        // The last read and the first finish; the second then fails.
        l.finish(&jobs[2], Ok(()));
        l.finish(&jobs[0], Ok(()));
        let results = || (result(array.cast(), 0), result(array.cast(), 1));
        assert_eq!(results(), (E_PENDING, E_PENDING));
        l.finish(&jobs[1], Err(INVALID_DATA));
        assert_eq!(results(), (INVALID_DATA, S_OK));
    }

    #[test]
    fn zlib_then_bcpack() {
        with_test_tables();
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&BC4_STREAM, 6);
        let (_dir, file) = file_with(&packed);
        let (queue, array) = (new_queue(0), status_array(1));
        let mut out = [0u8; 16];
        let options = ZLIB | 4 << 1;
        let mut r = request(options, out.as_mut_ptr().cast(), 16, file, 0, packed.len());
        r.intermediate_size = BC4_STREAM.len() as u32;
        enqueue(queue, &r);
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), S_OK);
        assert_eq!(out, bc4_blocks());
    }

    /// BCPack tables we made (uniform models; BC4 index pairs: low 3 bits,
    /// next 3 bits), in a folder kept for the test process.
    fn with_test_tables() {
        static DIR: OnceLock<()> = OnceLock::new();
        DIR.get_or_init(|| {
            let mut t = Vec::new();
            for m in 0..128 {
                let g = m / 16;
                for b in 0..9 {
                    let f: u16 = if b < g + 2 { 1 << (15 - g) } else { 0 };
                    t.extend_from_slice(&f.to_le_bytes());
                }
            }
            t.extend([0; 256 + 64 + 256]);
            t.extend((0..64u8).map(|v| (v & 7) | (v >> 3) << 4));
            t.resize(bcpack::TABLES_SIZE, 0);
            let dir = tempfile::tempdir().unwrap().keep();
            std::fs::write(dir.join("tables.bin"), t).unwrap();
            use_bcpack_tables(dir);
        });
    }

    /// Two BC4 blocks from the format's test encoder: endpoints 10, -3 with
    /// indices p -> p & 7, then the same block plus 1 everywhere.
    const BC4_STREAM: [u8; 88] = [
        0, 0, 0, 0, 0, 0, 0, 0, 5, 5, 5, 5, 4, 4, 4, 4, 1, 1, 1, 1, 0x10, 0x10, 0x10, 0x10, 2, 1,
        0, 0, 0, 0, 0, 0, 0, 0, 0x14, 5, 0x20, 0x68, 0xb0, 0xf8, 3, 3, 2, 2, 0x24, 0x24, 0x24,
        0x24, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 4, 4, 0x10, 0x10, 0x10, 0x10, 0, 0, 0, 0,
        0x20, 0x68, 0xb0, 0xf8, 0x24, 0x24, 0x24, 0x24, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    /// What `BC4_STREAM` unpacks to.
    fn bc4_blocks() -> [u8; 16] {
        let code = [0u64, 2, 3, 4, 5, 6, 7, 1];
        let block = |e0: u64, e1: u64, add: usize| {
            let bits = (0..16).fold(0, |a, p| a | code[(p + add) & 7] << (16 + 3 * p));
            (e0 | e1 << 8 | bits).to_le_bytes()
        };
        let mut out = [0; 16];
        out[..8].copy_from_slice(&block(138, 125, 0));
        out[8..].copy_from_slice(&block(139, 126, 1));
        out
    }

    #[test]
    fn bcpack_requests_unpack_into_the_destination() {
        with_test_tables();
        let mut contents = pattern(100);
        contents.extend_from_slice(&BC4_STREAM);
        let (_dir, file) = file_with(&contents);
        let (queue, array) = (new_queue(0), status_array(1));
        let mut out = [0u8; 16];
        enqueue(
            queue,
            &request(4 << 1, out.as_mut_ptr().cast(), 16, file, 100, 88),
        );
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), S_OK);
        assert_eq!(out, bc4_blocks());
    }

    #[test]
    fn a_closed_queue_completes_nothing() {
        let (_dir, file) = file_with(&pattern(64));
        let (queue, array) = (new_queue(0), status_array(1));
        let mut out = [0u8; 16];
        enqueue(queue, &request(0, out.as_mut_ptr().cast(), 16, file, 0, 16));
        enqueue_status(queue, array, 0);
        void(queue, 0x40);
        void(queue, 0x30);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(result(array, 0), E_PENDING);
        assert_eq!(out, [0; 16]);
    }

    #[test]
    fn physical_page_destinations() {
        use crate::modules::xmem;
        let mut pages = [0usize; 2];
        let mut n = 2;
        assert_eq!(
            xmem::XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()),
            1
        );
        // Where the title would see them: a reservation of 64 KiB pages.
        let mapped = xmem::XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            0x2000 | 0x2040_0000, // reserve, 64 KiB pages
            0,
            0x04,
        );
        assert!(!mapped.is_null());
        assert_eq!(
            xmem::XMemMapPhysicalPages(mapped, 2, pages.as_ptr()),
            mapped
        );

        let plain = pattern(100);
        let (queue, array) = (new_queue(1), status_array(1));
        let mut r = request(
            PAGE_DESTINATION | MEMORY_SOURCE,
            pages.as_mut_ptr().cast(),
            plain.len(),
            plain.as_ptr().cast(),
            0,
            plain.len(),
        );
        r.page_offset = (PAGE - 40) as u64; // across the page boundary
        enqueue(queue, &r);
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), S_OK);
        // SAFETY: both pages are mapped read-write.
        let seen = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>().add(PAGE - 40), 100) };
        assert_eq!(seen, plain);
    }
    /// File reads (`pread` on the workers) into plain memory and into
    /// physical pages (a page boundary crossed, in place through the pool's
    /// view), several at once on two queues, each signalling its own status.
    #[test]
    fn file_reads_land_in_memory_and_in_physical_pages() {
        use crate::modules::xmem;
        let plain = pattern(3 * PAGE + 1234);
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&plain[..100_000], 6);
        let mut contents = plain.clone();
        contents.extend_from_slice(&packed);
        let (_dir, file) = file_with(&contents);

        let (mut pages, mut n) = ([0usize; 3], 3);
        assert_eq!(
            xmem::XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()),
            1
        );
        let mapped = xmem::XMemVirtualAlloc(
            std::ptr::null_mut(),
            3 * PAGE,
            0x2000 | 0x2040_0000, // reserve, 64 KiB pages
            0,
            0x04,
        );
        assert!(!mapped.is_null());
        assert_eq!(
            xmem::XMemMapPhysicalPages(mapped, 3, pages.as_ptr()),
            mapped
        );

        let (q_pages, q_memory) = (new_queue(1), new_queue(1));
        let (a_pages, a_memory) = (status_array(1), status_array(2));

        // Raw bytes from the middle of the file, across the first page edge.
        let mut r = request(
            PAGE_DESTINATION,
            pages.as_mut_ptr().cast(),
            120_000,
            file,
            1000,
            120_000,
        );
        r.page_offset = (PAGE - 500) as u64;
        enqueue(q_pages, &r);
        enqueue_status(q_pages, a_pages, 0);
        // The compressed tail into plain memory, and a raw read of the end.
        let mut inflated = vec![0u8; 100_000];
        let mut tail = vec![0u8; 1234];
        enqueue(
            q_memory,
            &request(
                ZLIB,
                inflated.as_mut_ptr().cast(),
                inflated.len(),
                file,
                plain.len() as u64,
                packed.len(),
            ),
        );
        enqueue_status(q_memory, a_memory, 0);
        enqueue(
            q_memory,
            &request(
                0,
                tail.as_mut_ptr().cast(),
                1234,
                file,
                3 * PAGE as u64,
                1234,
            ),
        );
        // Past the end of the file: fails with end-of-file.
        let mut beyond = vec![0u8; 64];
        enqueue(
            q_memory,
            &request(
                0,
                beyond.as_mut_ptr().cast(),
                64,
                file,
                contents.len() as u64,
                64,
            ),
        );
        enqueue_status(q_memory, a_memory, 1);
        void(q_pages, 0x30);
        void(q_memory, 0x30);

        assert_eq!(wait(a_pages, 0), S_OK);
        assert_eq!(wait(a_memory, 0), S_OK);
        assert_eq!(wait(a_memory, 1), END_OF_FILE);
        // SAFETY: all three pages are mapped read-write.
        let seen = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), 3 * PAGE) };
        assert_eq!(
            &seen[PAGE - 500..PAGE - 500 + 120_000],
            &plain[1000..121_000]
        );
        assert_eq!(seen[PAGE - 501], 0, "nothing before the offset is written");
        assert_eq!(inflated, plain[..100_000]);
        assert_eq!(tail, plain[3 * PAGE..]);
    }

    #[test]
    fn opening_a_missing_file_reports_not_found() {
        let open: extern "system" fn(*mut c_void, *const u16, *const Guid, Out) -> HRESULT =
            call(factory(), 0x20);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nothing.bin");
        let wide: Vec<u16> = path.to_str().unwrap().encode_utf16().chain([0]).collect();
        let mut file = std::ptr::null_mut();
        assert_eq!(
            open(factory(), wide.as_ptr(), std::ptr::null(), &mut file),
            0x8007_0002_u32 as i32
        );
        assert!(file.is_null());
    }

    #[test]
    fn a_page_list_filled_after_its_request_starts() {
        use crate::modules::xmem;
        let (mut given, mut page) = (1usize, [0usize; 1]);
        assert_eq!(
            xmem::XMemAllocatePhysicalPages(0, &mut given, page.as_mut_ptr()),
            1
        );
        let mapped = xmem::XMemVirtualAlloc(
            std::ptr::null_mut(),
            PAGE,
            0x2000 | 0x2040_0000, // reserve, 64 KiB pages
            0,
            0x04,
        );
        assert!(!mapped.is_null());
        assert_eq!(xmem::XMemMapPhysicalPages(mapped, 1, page.as_ptr()), mapped);

        // The title's list, still empty when the request is submitted.
        let list = Box::leak(Box::new([0usize; 1]));
        let at = std::ptr::from_mut(list) as usize;
        let plain = pattern(4096);
        let (queue, array) = (new_queue(1), status_array(1));
        let r = request(
            PAGE_DESTINATION | MEMORY_SOURCE,
            list.as_mut_ptr().cast(),
            plain.len(),
            plain.as_ptr().cast(),
            0,
            plain.len(),
        );
        let number = page[0];
        let filler = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(2));
            // SAFETY: the leaked list, alive for the whole test.
            unsafe { std::ptr::write_volatile(at as *mut usize, number) };
        });
        enqueue(queue, &r);
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), S_OK);
        filler.join().unwrap();
        // SAFETY: the page is mapped read-write.
        let seen = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), plain.len()) };
        assert_eq!(seen, plain);
    }

    #[test]
    fn a_page_list_never_filled_fails_its_request() {
        let list = [0usize; 1];
        let plain = pattern(4096);
        let (queue, array) = (new_queue(1), status_array(1));
        let r = request(
            PAGE_DESTINATION | MEMORY_SOURCE,
            list.as_ptr().cast_mut().cast(),
            plain.len(),
            plain.as_ptr().cast(),
            0,
            plain.len(),
        );
        enqueue(queue, &r);
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), E_INVALIDARG);
    }
    #[test]
    fn a_queue_that_submits_itself_holds_reads_into_pages() {
        let mut l = lanes(STAGING);
        l.lanes.insert(0, Lane::new(0, 64));
        let array = status_array(1).cast::<Instance<StatusArray>>();
        read(&mut l, 0, PAGE_DESTINATION, 16);
        read(&mut l, 0, 0, 16);
        add_ref_instance(array);
        l.lanes
            .get_mut(&0)
            .unwrap()
            .unsubmitted
            .push(Command::Status(Status { array, index: 0 }));
        l.submit_as(0, false);
        // Only the plain read starts; the status waits behind the held one.
        let plain = l.start().unwrap();
        assert_eq!(plain.request.options, 0);
        assert!(l.start().is_none());
        l.finish(&plain, Ok(()));
        assert_eq!(l.lanes[&0].held.len(), 2);
        l.submit(0);
        let paged = l.start().unwrap();
        assert_eq!(paged.request.options, PAGE_DESTINATION);
        assert!(l.lanes[&0].held.is_empty());
        assert!(!l.lanes[&0].statuses.is_empty());
        l.finish(&paged, Ok(()));
        assert!(l.lanes[&0].statuses.is_empty());
    }

    #[test]
    fn a_page_list_the_title_changes_before_its_submit() {
        use crate::modules::xmem;
        let (mut given, mut pages) = (2usize, [0usize; 2]);
        assert_eq!(
            xmem::XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()),
            1
        );
        let mapped = xmem::XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            0x2000 | 0x2040_0000, // reserve, 64 KiB pages
            0,
            0x04,
        );
        assert!(!mapped.is_null());
        assert_eq!(
            xmem::XMemMapPhysicalPages(mapped, 2, pages.as_ptr()),
            mapped
        );
        // The title's page table: still the page's previous owner when the
        // request is enqueued, the right page by its Submit.
        let mut list = [pages[0]];
        let plain = pattern(4096);
        let create: extern "system" fn(*mut c_void, *const QueueDesc, *const Guid, Out) -> HRESULT =
            call(factory(), 0x50);
        let desc = QueueDesc {
            source_type: 0,
            capacity: 4,
            priority: 1,
            name: c"pages".as_ptr(),
            unknown: 1,
        };
        let mut queue = std::ptr::null_mut();
        assert_eq!(create(factory(), &desc, std::ptr::null(), &mut queue), S_OK);
        let array = status_array(1);
        let r = request(
            PAGE_DESTINATION | MEMORY_SOURCE,
            list.as_mut_ptr().cast(),
            plain.len(),
            plain.as_ptr().cast(),
            0,
            plain.len(),
        );
        enqueue(queue, &r);
        let mut out = [0u8; 16];
        enqueue(
            queue,
            &request(
                MEMORY_SOURCE,
                out.as_mut_ptr().cast(),
                16,
                plain.as_ptr().cast(),
                0,
                16,
            ),
        );
        // Half of four: the queue submitted itself. Time for a read started
        // then to land in the old page.
        std::thread::sleep(std::time::Duration::from_millis(50));
        // SAFETY: the list outlives the request, which reads it when it runs.
        unsafe { std::ptr::write_volatile(list.as_mut_ptr(), pages[1]) };
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), S_OK);
        assert_eq!(out, plain[..16]);
        // SAFETY: both pages are mapped read-write.
        let (first, second) = unsafe {
            (
                std::slice::from_raw_parts(mapped.cast::<u8>(), plain.len()),
                std::slice::from_raw_parts(mapped.cast::<u8>().add(PAGE), plain.len()),
            )
        };
        assert_eq!(second, plain);
        assert!(first.iter().all(|&b| b == 0), "the old page was written");
    }

    #[test]
    fn zlib_into_pages_in_place_and_across_allocations() {
        use crate::modules::xmem;
        let allocate = |n: usize| {
            let (mut pages, mut given) = (vec![0usize; n], n);
            assert_eq!(
                xmem::XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()),
                1
            );
            pages
        };
        let (a, b) = (allocate(3), allocate(1));
        let plain = pattern(2 * PAGE + 100);
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);
        // One allocation's pages in order (one window), then pages of two
        // (through a buffer), both from 50 bytes into the first.
        let mut destinations = [a.clone(), vec![a[0], a[1], b[0]]];
        let (queue, array) = (new_queue(1), status_array(1));
        for pages in &mut destinations {
            let mut r = request(
                ZLIB | PAGE_DESTINATION | MEMORY_SOURCE,
                pages.as_mut_ptr().cast(),
                plain.len(),
                packed.as_ptr().cast(),
                0,
                packed.len(),
            );
            r.page_offset = 50;
            enqueue(queue, &r);
        }
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), S_OK);
        for pages in &destinations {
            let mapped = xmem::XMemVirtualAlloc(
                std::ptr::null_mut(),
                3 * PAGE,
                0x2000 | 0x2040_0000, // reserve, 64 KiB pages
                0,
                0x04,
            );
            assert!(!mapped.is_null());
            assert_eq!(
                xmem::XMemMapPhysicalPages(mapped, 3, pages.as_ptr()),
                mapped
            );
            // SAFETY: all three pages are mapped read-write.
            let seen = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), 3 * PAGE) };
            assert!(seen[..50].iter().all(|&b| b == 0));
            assert_eq!(seen[50..50 + plain.len()], plain);
            assert_eq!(
                xmem::XMemMapPhysicalPages(mapped, 3, std::ptr::null()),
                mapped
            );
        }
        assert_eq!(xmem::XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(xmem::XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn null_out_pointers_fail_without_leaking_lanes_or_handles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, b"abc").unwrap();
        let wide: Vec<u16> = path.to_str().unwrap().encode_utf16().chain([0]).collect();

        let open: extern "system" fn(*mut c_void, *const u16, *const Guid, Out) -> HRESULT =
            call(factory(), 0x20);
        assert_eq!(
            open(
                factory(),
                wide.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut()
            ),
            E_INVALIDARG
        );

        let create_array: extern "system" fn(
            *mut c_void,
            u32,
            *const c_char,
            *const Guid,
            Out,
        ) -> HRESULT = call(factory(), 0x28);
        assert_eq!(
            create_array(
                factory(),
                4,
                c"null-out".as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut()
            ),
            E_INVALIDARG
        );

        let before = scheduler().unwrap().lock().next_id;
        let create_q: extern "system" fn(
            *mut c_void,
            *const QueueDesc,
            *const Guid,
            Out,
        ) -> HRESULT = call(factory(), 0x50);
        let desc = QueueDesc {
            source_type: 0,
            capacity: 8,
            priority: 0,
            name: c"null-out".as_ptr(),
            unknown: 1,
        };
        assert_eq!(
            create_q(factory(), &desc, std::ptr::null(), std::ptr::null_mut()),
            E_INVALIDARG
        );
        assert_eq!(scheduler().unwrap().lock().next_id, before);
    }

    #[test]
    fn null_source_and_overflowing_file_offset_fail_safely() {
        let mut dst = [0u8; 4];
        let req = request(0, dst.as_mut_ptr().cast(), 4, std::ptr::null(), 0, 4);
        assert_eq!(source_name(&req), "<null>");
        assert_eq!(execute(&req), Err(E_INVALIDARG));

        let err = fill_at(&mut dst, u64::MAX - 1, |buf, _| {
            buf[0] = 1;
            Ok(2)
        })
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
