//! Running title code: TLS callbacks, the entry point, and threads that
//! start in title code.

use anyhow::{Context, Result};
use victory_platform::thread::{self, Thread};

use super::bridge::call_win64;
use super::image::NativeImage;
use super::teb;
use crate::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};

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

/// Starts a thread that runs the Windows-ABI function `start(param)` and
/// ends with its return value, which is passed to `on_exit` first. The
/// thread has its own TEB and static TLS.
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
    thread::spawn(name, stack_size, move || {
        // SAFETY: forwarded from the caller.
        let code = unsafe { call_win64(start, &[param as u64]) } as u32;
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
