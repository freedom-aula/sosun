//! Win32 implementation of `thread`: `CreateThread` and friends.

use std::ffi::c_void;
use std::io;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{
    CreateThread, GetCurrentThreadId, GetExitCodeThread, INFINITE,
    STACK_SIZE_PARAM_IS_A_RESERVATION, Sleep, WaitForSingleObject,
};

/// A thread started by [`spawn`].
#[derive(Debug)]
pub struct Thread {
    handle: HANDLE,
    id: u32,
}

// SAFETY: a thread handle is process-wide.
unsafe impl Send for Thread {}

type Job = Box<dyn FnOnce() -> u32 + Send>;

unsafe extern "system" fn start(param: *mut c_void) -> u32 {
    // SAFETY: `param` is the box `spawn` leaked for this thread alone.
    let job: Box<Job> = unsafe { Box::from_raw(param.cast()) };
    job()
}

/// Starts a thread with a stack of `stack_size` bytes that runs `f`; `f`'s
/// result is the thread's exit code. The name is only used on macOS.
pub fn spawn(
    _name: &str,
    stack_size: usize,
    f: impl FnOnce() -> u32 + Send + 'static,
) -> io::Result<Thread> {
    let job: Box<Job> = Box::new(Box::new(f));
    let param = Box::into_raw(job);
    let mut id = 0;
    // SAFETY: `start` takes ownership of `param`; on failure we take it back.
    let handle = unsafe {
        CreateThread(
            std::ptr::null(),
            stack_size,
            Some(start),
            param.cast(),
            STACK_SIZE_PARAM_IS_A_RESERVATION,
            &mut id,
        )
    };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: the thread never started, so the box is still ours.
        drop(unsafe { Box::from_raw(param) });
        return Err(error);
    }
    Ok(Thread { handle, id })
}

impl Thread {
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Waits for the thread and returns its exit code.
    pub fn join(self) -> u32 {
        let mut code = 0;
        // SAFETY: our own handle, closed once.
        unsafe {
            WaitForSingleObject(self.handle, INFINITE);
            GetExitCodeThread(self.handle, &mut code);
            CloseHandle(self.handle);
        }
        code
    }
}

/// The calling thread's OS id.
pub fn current_id() -> u32 {
    // SAFETY: no preconditions.
    unsafe { GetCurrentThreadId() }
}

pub fn sleep(time: Duration) {
    // SAFETY: no preconditions.
    unsafe { Sleep(time.as_millis().min(u128::from(u32::MAX - 1)) as u32) };
}
