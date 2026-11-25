//! Win32 implementation of `sync`: the kernel objects themselves.

use std::ffi::c_void;
use std::io;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateSemaphoreW, INFINITE, ReleaseSemaphore, ResetEvent, SetEvent,
    WaitForSingleObject, WaitOnAddress, WakeByAddressAll, WakeByAddressSingle,
};

fn millis(timeout: Option<Duration>) -> u32 {
    timeout.map_or(INFINITE, |t| {
        t.as_millis().min(u128::from(INFINITE - 1)) as u32
    })
}

fn wait_handle(handle: HANDLE, timeout: Option<Duration>) -> bool {
    // SAFETY: a handle the caller's object owns.
    unsafe { WaitForSingleObject(handle, millis(timeout)) == WAIT_OBJECT_0 }
}

/// A Win32 event: signalled or not, manual or auto reset.
#[derive(Debug)]
pub struct Event(HANDLE);

// SAFETY: kernel handles are usable from any thread.
unsafe impl Send for Event {}
// SAFETY: as above.
unsafe impl Sync for Event {}

impl Event {
    pub fn new(manual_reset: bool, initial: bool) -> io::Result<Event> {
        // SAFETY: plain object creation.
        let handle = unsafe {
            CreateEventW(
                std::ptr::null(),
                manual_reset.into(),
                initial.into(),
                std::ptr::null(),
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Event(handle))
    }

    /// Signals the event. A manual-reset event releases every waiter and
    /// stays set; an auto-reset one releases one and resets.
    pub fn set(&self) {
        // SAFETY: our own handle.
        unsafe { SetEvent(self.0) };
    }

    pub fn reset(&self) {
        // SAFETY: our own handle.
        unsafe { ResetEvent(self.0) };
    }

    /// Waits until signalled; `false` if `timeout` passes first.
    pub fn wait(&self, timeout: Option<Duration>) -> bool {
        wait_handle(self.0, timeout)
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: our own handle, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// A counting semaphore with a maximum, as `CreateSemaphore` makes.
#[derive(Debug)]
pub struct Semaphore(HANDLE);

// SAFETY: kernel handles are usable from any thread.
unsafe impl Send for Semaphore {}
// SAFETY: as above.
unsafe impl Sync for Semaphore {}

impl Semaphore {
    pub fn new(initial: u32, max: u32) -> io::Result<Semaphore> {
        if max == 0 || initial > max {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        // SAFETY: plain object creation.
        let handle = unsafe {
            CreateSemaphoreW(
                std::ptr::null(),
                initial as i32,
                max as i32,
                std::ptr::null(),
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Semaphore(handle))
    }

    /// Adds `n` to the count and returns the count before; `None` (and no
    /// change) if that would pass the maximum.
    pub fn release(&self, n: u32) -> Option<u32> {
        let mut before = 0;
        // SAFETY: our own handle.
        let ok = unsafe { ReleaseSemaphore(self.0, n as i32, &mut before) };
        (ok != 0).then_some(before as u32)
    }

    /// Takes one from the count, waiting for it; `false` on timeout.
    pub fn wait(&self, timeout: Option<Duration>) -> bool {
        wait_handle(self.0, timeout)
    }
}

impl Drop for Semaphore {
    fn drop(&mut self) {
        // SAFETY: our own handle, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// Blocks while `*addr == expected` (`WaitOnAddress`). Returns at once if it
/// differs; `false` if `timeout` passes first. Spurious wake-ups are
/// possible: the caller re-checks.
pub fn wait_on_address(addr: &AtomicU32, expected: u32, timeout: Option<Duration>) -> bool {
    // SAFETY: both pointers name live 4-byte values.
    unsafe {
        WaitOnAddress(
            addr.as_ptr().cast::<c_void>(),
            (&raw const expected).cast::<c_void>(),
            4,
            millis(timeout),
        ) != 0
    }
}

/// Wakes one thread blocked in [`wait_on_address`] on `addr`.
pub fn wake_one(addr: &AtomicU32) {
    // SAFETY: a live atomic.
    unsafe { WakeByAddressSingle(addr.as_ptr().cast::<c_void>()) };
}

/// Wakes every thread blocked in [`wait_on_address`] on `addr`.
pub fn wake_all(addr: &AtomicU32) {
    // SAFETY: a live atomic.
    unsafe { WakeByAddressAll(addr.as_ptr().cast::<c_void>()) };
}
