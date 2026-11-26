//! Critical sections, slim reader/writer locks, condition variables and
//! waits on several objects, for the native host.
//!
//! The first three live in the title's own memory (`CRITICAL_SECTION` is 40
//! bytes, the other two 8), so they are built on `wait_on_address` over a
//! word of the structure itself: no handle, nothing to free, and a
//! structure zeroed by the title (`SRWLOCK_INIT`) is a valid, free lock.

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use victory_platform::sync::{wait_on_address, wake_all, wake_one};
use victory_platform::thread;

use super::{WAIT_OBJECT_0, WAIT_TIMEOUT, Waitable, set_error};

const ERROR_TIMEOUT: u32 = 1460;

// Waits on several objects. Everything the title can signal goes through
// `signalled`, which bumps a counter that waiters sleep on; each waiter
// then looks at its own objects.

static EPOCH: AtomicU32 = AtomicU32::new(0);
/// Wait-all takers go one at a time: one that took some objects and could
/// not take the rest gives them back, and two doing it would starve.
static WAIT_ALL: Mutex<()> = Mutex::new(());

/// Something a wait could be waiting for was signalled.
pub(super) fn signalled() {
    EPOCH.fetch_add(1, Ordering::AcqRel);
    wake_all(&EPOCH);
}

/// `WaitForMultipleObjects` over `objects`: the index of the first one
/// signalled (any), or `WAIT_OBJECT_0` once all are taken (all).
pub(super) fn wait_multiple(objects: &[Waitable], all: bool, timeout: Option<Duration>) -> u32 {
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        let seen = EPOCH.load(Ordering::Acquire);
        if all {
            let _one_at_a_time = WAIT_ALL.lock().unwrap_or_else(|e| e.into_inner());
            let taken = objects.iter().take_while(|o| o.try_take()).count();
            if taken == objects.len() {
                return WAIT_OBJECT_0;
            }
            for o in &objects[..taken] {
                o.give_back();
            }
        } else if let Some(i) = objects.iter().position(Waitable::try_take) {
            return WAIT_OBJECT_0 + i as u32;
        }
        let left = match deadline {
            None => None,
            Some(d) => match d.checked_duration_since(Instant::now()) {
                Some(left) if !left.is_zero() => Some(left),
                _ => return WAIT_TIMEOUT,
            },
        };
        wait_on_address(&EPOCH, seen, left);
    }
}

// Critical sections.

const CS_LOCK_WORD: usize = 8;
const CS_RECURSION: usize = 12;
const CS_OWNER: usize = 16;

struct Cs(*mut u8);

impl Cs {
    fn word(&self) -> &AtomicU32 {
        // SAFETY: the title's initialised CRITICAL_SECTION, 4-byte aligned.
        unsafe { &*self.0.add(CS_LOCK_WORD).cast::<AtomicU32>() }
    }

    fn owner(&self) -> &AtomicU64 {
        // SAFETY: as above.
        unsafe { &*self.0.add(CS_OWNER).cast::<AtomicU64>() }
    }

    fn recursion(&self) -> *mut i32 {
        // SAFETY: as above.
        unsafe { self.0.add(CS_RECURSION).cast() }
    }

    fn take(&self) {
        let word = self.word();
        if word
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        // 2 means locked with waiters, so unlocking knows to wake one.
        while word.swap(2, Ordering::Acquire) != 0 {
            wait_on_address(word, 2, None);
        }
    }

    fn release(&self) {
        if self.word().swap(0, Ordering::Release) == 2 {
            wake_one(self.word());
        }
    }
}

pub extern "system" fn InitializeCriticalSection(cs: *mut u8) {
    // SAFETY: the title's 40-byte structure.
    unsafe { std::ptr::write_bytes(cs, 0, 40) };
}

pub extern "system" fn InitializeCriticalSectionAndSpinCount(cs: *mut u8, _spin: u32) -> i32 {
    InitializeCriticalSection(cs);
    1
}

pub extern "system" fn InitializeCriticalSectionEx(cs: *mut u8, _spin: u32, _flags: u32) -> i32 {
    InitializeCriticalSection(cs);
    1
}

pub extern "system" fn SetCriticalSectionSpinCount(_cs: *mut u8, _spin: u32) -> u32 {
    0
}

pub extern "system" fn DeleteCriticalSection(_cs: *mut u8) {}

pub extern "system" fn EnterCriticalSection(cs: *mut u8) {
    let cs = Cs(cs);
    let me = u64::from(thread::current_id());
    if cs.owner().load(Ordering::Relaxed) == me {
        // SAFETY: only the owner touches the count.
        unsafe { cs.recursion().write(cs.recursion().read() + 1) };
        return;
    }
    cs.take();
    cs.owner().store(me, Ordering::Relaxed);
    // SAFETY: we hold it.
    unsafe { cs.recursion().write(1) };
}

pub extern "system" fn TryEnterCriticalSection(cs: *mut u8) -> i32 {
    let cs = Cs(cs);
    let me = u64::from(thread::current_id());
    if cs.owner().load(Ordering::Relaxed) == me {
        // SAFETY: only the owner touches the count.
        unsafe { cs.recursion().write(cs.recursion().read() + 1) };
        return 1;
    }
    if cs
        .word()
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return 0;
    }
    cs.owner().store(me, Ordering::Relaxed);
    // SAFETY: we hold it.
    unsafe { cs.recursion().write(1) };
    1
}

pub extern "system" fn LeaveCriticalSection(cs: *mut u8) {
    let cs = Cs(cs);
    // SAFETY: the owner's count.
    let left = unsafe {
        let n = cs.recursion().read() - 1;
        cs.recursion().write(n);
        n
    };
    if left == 0 {
        cs.owner().store(0, Ordering::Relaxed);
        cs.release();
    }
}

// Slim reader/writer locks: bit 31 held exclusively, bit 30 someone is
// waiting, the rest the reader count.

const EXCLUSIVE: u32 = 1 << 31;
const WAITERS: u32 = 1 << 30;
const READERS: u32 = WAITERS - 1;

fn srw<'a>(lock: *mut c_void) -> &'a AtomicU32 {
    // SAFETY: the title's 8-byte SRWLOCK, whose low word we use.
    unsafe { &*lock.cast::<AtomicU32>() }
}

pub extern "system" fn InitializeSRWLock(lock: *mut c_void) {
    // SAFETY: the title's 8-byte structure.
    unsafe { lock.cast::<u64>().write(0) };
}

fn srw_try(word: &AtomicU32, shared: bool) -> Result<(), u32> {
    let s = word.load(Ordering::Relaxed);
    let free = if shared {
        s & EXCLUSIVE == 0
    } else {
        s & (EXCLUSIVE | READERS) == 0
    };
    if !free {
        return Err(s);
    }
    let next = if shared { s + 1 } else { s | EXCLUSIVE };
    word.compare_exchange(s, next, Ordering::Acquire, Ordering::Relaxed)
        .map(|_| ())
        .or(Err(word.load(Ordering::Relaxed)))
}

fn srw_acquire(word: &AtomicU32, shared: bool) {
    loop {
        let Err(seen) = srw_try(word, shared) else {
            return;
        };
        let held = seen | WAITERS;
        let marked = seen & WAITERS != 0
            || word
                .compare_exchange(seen, held, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok();
        if marked {
            wait_on_address(word, held, None);
        }
    }
}

pub extern "system" fn AcquireSRWLockExclusive(lock: *mut c_void) {
    srw_acquire(srw(lock), false);
}

pub extern "system" fn AcquireSRWLockShared(lock: *mut c_void) {
    srw_acquire(srw(lock), true);
}

pub extern "system" fn TryAcquireSRWLockExclusive(lock: *mut c_void) -> u8 {
    u8::from(srw_try(srw(lock), false).is_ok())
}

pub extern "system" fn TryAcquireSRWLockShared(lock: *mut c_void) -> u8 {
    u8::from(srw_try(srw(lock), true).is_ok())
}

pub extern "system" fn ReleaseSRWLockExclusive(lock: *mut c_void) {
    let word = srw(lock);
    if word.swap(0, Ordering::Release) & WAITERS != 0 {
        wake_all(word);
    }
}

pub extern "system" fn ReleaseSRWLockShared(lock: *mut c_void) {
    let word = srw(lock);
    let before = word.fetch_sub(1, Ordering::Release);
    if before & READERS == 1 && before & WAITERS != 0 {
        wake_all(word);
    }
}

// Condition variables: a counter that waking bumps.

fn condition<'a>(cv: *mut c_void) -> &'a AtomicU32 {
    // SAFETY: the title's 8-byte CONDITION_VARIABLE, whose low word we use.
    unsafe { &*cv.cast::<AtomicU32>() }
}

pub extern "system" fn InitializeConditionVariable(cv: *mut c_void) {
    // SAFETY: the title's 8-byte structure.
    unsafe { cv.cast::<u64>().write(0) };
}

fn sleep_condition(
    cv: *mut c_void,
    ms: u32,
    release: impl FnOnce(),
    reacquire: impl FnOnce(),
) -> i32 {
    let word = condition(cv);
    let ticket = word.load(Ordering::Acquire);
    release();
    let timeout = (ms != u32::MAX).then(|| Duration::from_millis(u64::from(ms)));
    // A wake between the load and the wait changes the word, and the wait
    // returns at once.
    let woken = wait_on_address(word, ticket, timeout);
    reacquire();
    if woken {
        1
    } else {
        set_error(ERROR_TIMEOUT);
        0
    }
}

pub extern "system" fn SleepConditionVariableCS(cv: *mut c_void, cs: *mut u8, ms: u32) -> i32 {
    let me = u64::from(thread::current_id());
    let held = Cs(cs);
    // SAFETY: the caller owns the section.
    let depth = unsafe { held.recursion().read() };
    sleep_condition(
        cv,
        ms,
        || {
            held.owner().store(0, Ordering::Relaxed);
            // SAFETY: still the owner's.
            unsafe { held.recursion().write(0) };
            held.release();
        },
        || {
            held.take();
            held.owner().store(me, Ordering::Relaxed);
            // SAFETY: owned again.
            unsafe { held.recursion().write(depth) };
        },
    )
}

pub extern "system" fn SleepConditionVariableSRW(
    cv: *mut c_void,
    lock: *mut c_void,
    ms: u32,
    flags: u32,
) -> i32 {
    let shared = flags & 1 != 0;
    sleep_condition(
        cv,
        ms,
        || {
            if shared {
                ReleaseSRWLockShared(lock);
            } else {
                ReleaseSRWLockExclusive(lock);
            }
        },
        || srw_acquire(srw(lock), shared),
    )
}

pub extern "system" fn WakeConditionVariable(cv: *mut c_void) {
    let word = condition(cv);
    word.fetch_add(1, Ordering::AcqRel);
    wake_one(word);
}

pub extern "system" fn WakeAllConditionVariable(cv: *mut c_void) {
    let word = condition(cv);
    word.fetch_add(1, Ordering::AcqRel);
    wake_all(word);
}

// Yielding.

pub extern "system" fn SwitchToThread() -> i32 {
    std::thread::yield_now();
    1
}

pub extern "system" fn SleepEx(ms: u32, _alertable: i32) -> u32 {
    super::Sleep(ms);
    0
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
        InitializeCriticalSection,
        InitializeCriticalSectionAndSpinCount,
        InitializeCriticalSectionEx,
        SetCriticalSectionSpinCount,
        DeleteCriticalSection,
        EnterCriticalSection,
        TryEnterCriticalSection,
        LeaveCriticalSection,
        InitializeSRWLock,
        AcquireSRWLockExclusive,
        AcquireSRWLockShared,
        TryAcquireSRWLockExclusive,
        TryAcquireSRWLockShared,
        ReleaseSRWLockExclusive,
        ReleaseSRWLockShared,
        InitializeConditionVariable,
        SleepConditionVariableCS,
        SleepConditionVariableSRW,
        WakeConditionVariable,
        WakeAllConditionVariable,
        SwitchToThread,
        SleepEx,
    )
}
