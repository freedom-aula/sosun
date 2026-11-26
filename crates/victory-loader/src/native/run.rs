//! Running title code: TLS callbacks, the entry point, and threads that
//! start in title code.

use std::sync::Mutex;

use anyhow::{Context, Result};
use victory_platform::thread::{self, Thread};

use super::bridge::call_win64;
use super::image::NativeImage;
use super::teb;
use crate::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH, DLL_THREAD_ATTACH, DLL_THREAD_DETACH};

/// Calls the image's TLS callbacks with `reason` (`DLL_PROCESS_ATTACH`...)
/// on the calling thread.
///
/// # Safety
/// Runs title code. Imports must be bound, and the image finalized.
pub unsafe fn attach(image: &NativeImage, reason: u32) {
    let Some(tls) = &image.info().tls else { return };
    for &rva in &tls.callback_rvas {
        // SAFETY: forwarded from the caller; the RVA came from the image.
        unsafe {
            call_win64(
                image.base() + rva as usize,
                &[image.base() as u64, u64::from(reason), 0],
            )
        };
    }
}

/// The TLS callbacks of the images that hear of threads starting and ending
/// (`DLL_THREAD_ATTACH`, `DLL_THREAD_DETACH`): image base and callback RVAs.
static THREAD_NOTICES: Mutex<Vec<(usize, Vec<u32>)>> = Mutex::new(Vec::new());

/// Has `image`'s TLS callbacks told of every title thread from now on.
/// (`run_entry` does it for the main image.)
pub fn register_thread_callbacks(image: &NativeImage) {
    if let Some(tls) = &image.info().tls
        && !tls.callback_rvas.is_empty()
    {
        THREAD_NOTICES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((image.base(), tls.callback_rvas.clone()));
    }
}

/// Calls every registered callback with `reason`, on the calling thread.
fn notify_threads(reason: u32) {
    let images = THREAD_NOTICES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    for (base, rvas) in images {
        for rva in rvas {
            // SAFETY: a callback of an image registered by its owner.
            unsafe { call_win64(base + rva as usize, &[base as u64, u64::from(reason), 0]) };
        }
    }
}

/// Starts a thread that runs the Windows-ABI function `start(param)` and
/// ends with its return value, which is passed to `on_exit` first. The
/// thread has its own TEB and static TLS, and its images hear of it
/// (`DLL_THREAD_ATTACH` before `start`, `DLL_THREAD_DETACH` after).
///
/// # Safety
/// `start` must be a Windows x64 function of one pointer argument in a
/// bound, finalized image.
pub unsafe fn spawn_title_thread(
    name: &str,
    stack_size: usize,
    start: usize,
    param: usize,
    on_exit: impl FnOnce(u32) + Send + 'static,
) -> std::io::Result<Thread> {
    // SAFETY: forwarded from the caller.
    unsafe { spawn_title_thread_with(name, stack_size, start, param, || {}, on_exit) }
}

/// [`spawn_title_thread`] with `on_start`, which runs on the new thread
/// before any title code (and may hold it there, as `CREATE_SUSPENDED` does).
///
/// # Safety
/// As for [`spawn_title_thread`].
pub unsafe fn spawn_title_thread_with(
    name: &str,
    stack_size: usize,
    start: usize,
    param: usize,
    on_start: impl FnOnce() + Send + 'static,
    on_exit: impl FnOnce(u32) + Send + 'static,
) -> std::io::Result<Thread> {
    thread::spawn(name, stack_size, move || {
        on_start();
        notify_threads(DLL_THREAD_ATTACH);
        // SAFETY: forwarded from the caller.
        let code = unsafe { call_win64(start, &[param as u64]) } as u32;
        notify_threads(DLL_THREAD_DETACH);
        on_exit(code);
        code
    })
}

/// Starts the title: on a thread with the stack size from the image header,
/// runs the image's TLS callbacks (`DLL_PROCESS_ATTACH`), then its entry
/// point, and returns what the entry point returned. Also records the image
/// as the PEB's `ImageBaseAddress`.
///
/// # Safety
/// Runs title code. Imports must be bound and the image finalized; the
/// image must be registered with [`teb::register_tls`] if it has TLS.
pub unsafe fn run_entry(image: &'static NativeImage) -> Result<u32> {
    let entry = image.entry().context("image has no entry point")?;
    let stack = usize::try_from(image.info().stack_reserve)
        .unwrap_or(usize::MAX)
        .max(1 << 20);
    teb::set_image_base(image.base());
    register_thread_callbacks(image);
    let thread = thread::spawn("title-main", stack, move || {
        // SAFETY: forwarded from the caller.
        unsafe {
            attach(image, DLL_PROCESS_ATTACH);
            let code = call_win64(entry, &[0]) as u32;
            attach(image, DLL_PROCESS_DETACH);
            code
        }
    })
    .context("spawning title main thread")?;
    Ok(thread.join())
}
