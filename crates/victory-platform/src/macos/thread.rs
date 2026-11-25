//! pthread implementation of `thread` (through `std::thread`, which wraps
//! them and takes the stack size).

use std::io;
use std::os::unix::thread::JoinHandleExt;
use std::time::Duration;

unsafe extern "C" {
    fn pthread_threadid_np(thread: usize, id: *mut u64) -> i32;
}

/// A thread started by [`spawn`].
#[derive(Debug)]
pub struct Thread {
    handle: std::thread::JoinHandle<u32>,
    id: u32,
}

/// Starts a thread named `name` with a stack of `stack_size` bytes that
/// runs `f`; `f`'s result is the thread's exit code.
pub fn spawn(
    name: &str,
    stack_size: usize,
    f: impl FnOnce() -> u32 + Send + 'static,
) -> io::Result<Thread> {
    let handle = std::thread::Builder::new()
        .name(name.to_owned())
        .stack_size(stack_size)
        .spawn(f)?;
    let mut id = 0;
    // SAFETY: a live thread's pthread_t and a local out-pointer.
    unsafe { pthread_threadid_np(handle.as_pthread_t() as usize, &mut id) };
    Ok(Thread {
        handle,
        id: id as u32,
    })
}

impl Thread {
    /// The OS thread id, as `current_id` returns on that thread.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Waits for the thread and returns its exit code (`u32::MAX` if it panicked).
    pub fn join(self) -> u32 {
        self.handle.join().unwrap_or(u32::MAX)
    }
}

/// The calling thread's OS id.
pub fn current_id() -> u32 {
    let mut id = 0;
    // SAFETY: 0 names the calling thread; local out-pointer.
    unsafe { pthread_threadid_np(0, &mut id) };
    id as u32
}

pub fn sleep(time: Duration) {
    std::thread::sleep(time);
}
