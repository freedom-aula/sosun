//! macOS implementation of `sync`: mutex and condition variable objects,
//! and `os_sync` (macOS 14.4 and later) for wait-on-address.

use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Waits on `cv` until `ready` takes its turn (it consumes what it waits for).
fn wait_until<T>(
    mutex: &Mutex<T>,
    cv: &Condvar,
    timeout: Option<Duration>,
    mut ready: impl FnMut(&mut T) -> bool,
) -> bool {
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut state = lock(mutex);
    loop {
        if ready(&mut state) {
            return true;
        }
        state = match deadline {
            None => cv.wait(state).unwrap_or_else(|e| e.into_inner()),
            Some(deadline) => {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    return false;
                };
                cv.wait_timeout(state, left)
                    .unwrap_or_else(|e| e.into_inner())
                    .0
            }
        };
    }
}

/// A Win32-style event: signalled or not, manual or auto reset.
#[derive(Debug)]
pub struct Event {
    manual: bool,
    state: Mutex<bool>,
    cv: Condvar,
}

impl Event {
    pub fn new(manual_reset: bool, initial: bool) -> io::Result<Event> {
        Ok(Event {
            manual: manual_reset,
            state: Mutex::new(initial),
            cv: Condvar::new(),
        })
    }

    /// Signals the event. A manual-reset event releases every waiter and
    /// stays set; an auto-reset one releases one and resets.
    pub fn set(&self) {
        *lock(&self.state) = true;
        if self.manual {
            self.cv.notify_all();
        } else {
            self.cv.notify_one();
        }
    }

    pub fn reset(&self) {
        *lock(&self.state) = false;
    }

    /// Waits until signalled; `false` if `timeout` passes first.
    pub fn wait(&self, timeout: Option<Duration>) -> bool {
        let manual = self.manual;
        wait_until(&self.state, &self.cv, timeout, |set| {
            let was = *set;
            if was && !manual {
                *set = false;
            }
            was
        })
    }
}

/// A counting semaphore with a maximum, as `CreateSemaphore` makes.
#[derive(Debug)]
pub struct Semaphore {
    max: u32,
    count: Mutex<u32>,
    cv: Condvar,
}

impl Semaphore {
    pub fn new(initial: u32, max: u32) -> io::Result<Semaphore> {
        if max == 0 || initial > max {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        Ok(Semaphore {
            max,
            count: Mutex::new(initial),
            cv: Condvar::new(),
        })
    }

    /// Adds `n` to the count and returns the count before; `None` (and no
    /// change) if that would pass the maximum.
    pub fn release(&self, n: u32) -> Option<u32> {
        let mut count = lock(&self.count);
        let before = *count;
        if n > self.max - before {
            return None;
        }
        *count += n;
        drop(count);
        match n {
            0 => {}
            1 => self.cv.notify_one(),
            _ => self.cv.notify_all(),
        }
        Some(before)
    }

    /// Takes one from the count, waiting for it; `false` on timeout.
    pub fn wait(&self, timeout: Option<Duration>) -> bool {
        wait_until(&self.count, &self.cv, timeout, |count| {
            let some = *count > 0;
            if some {
                *count -= 1;
            }
            some
        })
    }
}

const OS_CLOCK_MACH_ABSOLUTE_TIME: u32 = 32;

unsafe extern "C" {
    fn os_sync_wait_on_address(addr: *mut u32, value: u64, size: usize, flags: u32) -> i32;
    fn os_sync_wait_on_address_with_timeout(
        addr: *mut u32,
        value: u64,
        size: usize,
        flags: u32,
        clock: u32,
        timeout_ns: u64,
    ) -> i32;
    fn os_sync_wake_by_address_any(addr: *mut u32, size: usize, flags: u32) -> i32;
    fn os_sync_wake_by_address_all(addr: *mut u32, size: usize, flags: u32) -> i32;
}

/// Blocks while `*addr == expected` (`WaitOnAddress`). Returns at once if it
/// differs; `false` if `timeout` passes first. Spurious wake-ups are
/// possible: the caller re-checks.
pub fn wait_on_address(addr: &AtomicU32, expected: u32, timeout: Option<Duration>) -> bool {
    if let Some(t) = timeout
        && t.is_zero()
    {
        return addr.load(Ordering::Acquire) != expected;
    }
    let deadline = timeout.map(|t| Instant::now() + t);
    let p = addr.as_ptr();
    loop {
        // SAFETY: `p` is a live 4-byte aligned atomic.
        let r = unsafe {
            match deadline {
                None => os_sync_wait_on_address(p, u64::from(expected), 4, 0),
                Some(d) => {
                    let Some(left) = d.checked_duration_since(Instant::now()) else {
                        return addr.load(Ordering::Acquire) != expected;
                    };
                    if left.is_zero() {
                        return addr.load(Ordering::Acquire) != expected;
                    }
                    os_sync_wait_on_address_with_timeout(
                        p,
                        u64::from(expected),
                        4,
                        0,
                        OS_CLOCK_MACH_ABSOLUTE_TIME,
                        left.as_nanos().min(u128::from(u64::MAX)) as u64,
                    )
                }
            }
        };
        if r >= 0 {
            return true;
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(4) => {}             // EINTR: retry with remaining time
            Some(60) => return false, // ETIMEDOUT
            _ => return true,         // EFAULT and the like: let the caller look
        }
    }
}

/// Wakes one thread blocked in [`wait_on_address`] on `addr`.
pub fn wake_one(addr: &AtomicU32) {
    // SAFETY: `addr` is a live 4-byte aligned atomic.
    unsafe { os_sync_wake_by_address_any(addr.as_ptr(), 4, 0) };
}

/// Wakes every thread blocked in [`wait_on_address`] on `addr`.
pub fn wake_all(addr: &AtomicU32) {
    // SAFETY: as above.
    unsafe { os_sync_wake_by_address_all(addr.as_ptr(), 4, 0) };
}

#[cfg(test)]
mod tests {
    use super::Semaphore;
    use std::sync::{Barrier, mpsc};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn semaphore_can_release_the_maximum_count_promptly() {
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let semaphore = Semaphore::new(0, u32::MAX).unwrap();
            let previous = semaphore.release(u32::MAX);
            tx.send((previous, semaphore.release(0))).unwrap();
        });

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2))
                .expect("bulk release timed out"),
            (Some(0), Some(u32::MAX))
        );
        worker.join().unwrap();
    }

    #[test]
    fn semaphore_waiters_can_only_take_released_permits() {
        let semaphore = Semaphore::new(0, 2).unwrap();
        let ready = Barrier::new(4);
        let (tx, rx) = mpsc::channel();
        thread::scope(|scope| {
            for _ in 0..3 {
                let (semaphore, ready, tx) = (&semaphore, &ready, tx.clone());
                scope.spawn(move || {
                    ready.wait();
                    tx.send(semaphore.wait(Some(Duration::from_secs(5))))
                        .unwrap();
                });
            }
            ready.wait();
            // Give the workers a chance to enter the condition variable wait.
            thread::sleep(Duration::from_millis(20));

            assert_eq!(semaphore.release(0), Some(0));
            assert!(!semaphore.wait(Some(Duration::ZERO)));
            assert_eq!(semaphore.release(2), Some(0));
            for _ in 0..2 {
                assert!(rx.recv_timeout(Duration::from_secs(1)).unwrap());
            }
            assert_eq!(semaphore.release(1), Some(0));
            assert!(rx.recv_timeout(Duration::from_secs(1)).unwrap());
            assert!(!semaphore.wait(Some(Duration::ZERO)));
        });
    }
}
