//! Running title code: TLS callbacks, the entry point, and threads that
//! start in title code.

use std::sync::Mutex;

use anyhow::{Context, Result};
use victory_platform::thread::{self, Thread};

use super::bridge::{call_thread_entry, call_win64};
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

const STATUS_DLL_INIT_FAILED: u32 = 0xC000_0142;

/// What an image hears of threads starting and ending
/// (`DLL_THREAD_ATTACH`, `DLL_THREAD_DETACH`).
struct Notice {
    base: usize,
    tls_callbacks: Vec<u32>,
    /// A DLL's `DllMain`, unless it called `DisableThreadLibraryCalls`.
    dll_main: Option<u32>,
}

static THREAD_NOTICES: Mutex<Vec<Notice>> = Mutex::new(Vec::new());

fn notices() -> std::sync::MutexGuard<'static, Vec<Notice>> {
    THREAD_NOTICES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Has `image`'s TLS callbacks told of every title thread from now on.
/// (`run_entry` does it for the main image.)
pub fn register_thread_callbacks(image: &NativeImage) {
    register(image, false);
}

/// As [`register_thread_callbacks`] for a DLL: its `DllMain` hears of threads
/// too (until `DisableThreadLibraryCalls`).
pub fn register_dll_thread_notices(image: &NativeImage) {
    register(image, true);
}

fn register(image: &NativeImage, dll_main: bool) {
    let tls_callbacks = image
        .info()
        .tls
        .as_ref()
        .map(|t| t.callback_rvas.clone())
        .unwrap_or_default();
    let dll_main = dll_main
        .then(|| image.info().entry_rva)
        .filter(|&rva| rva != 0);
    if tls_callbacks.is_empty() && dll_main.is_none() {
        return;
    }
    notices().push(Notice {
        base: image.base(),
        tls_callbacks,
        dll_main,
    });
}

/// `DisableThreadLibraryCalls`: the DLL at `base` no longer hears of threads
/// in its `DllMain` (its TLS callbacks still do). False if it is not one
/// that was registered.
pub fn disable_thread_calls(base: usize) -> bool {
    let mut notices = notices();
    let mut found = false;
    for n in notices.iter_mut().filter(|n| n.base == base) {
        found |= n.dll_main.take().is_some();
    }
    found
}

/// Calls every registered callback with `reason`, on the calling thread.
fn notify_threads(reason: u32) {
    let calls: Vec<(usize, Vec<u32>)> = notices()
        .iter()
        .map(|n| {
            let rvas = n.tls_callbacks.iter().chain(&n.dll_main).copied();
            (n.base, rvas.collect())
        })
        .collect();
    for (base, rvas) in calls {
        for rva in rvas {
            // SAFETY: a callback of an image registered by its owner.
            unsafe { call_win64(base + rva as usize, &[base as u64, u64::from(reason), 0]) };
        }
    }
}

/// Calls a DLL's TLS callbacks and its `DllMain` with `reason`, on the
/// calling thread: callbacks first, except for `DLL_PROCESS_DETACH`.
/// `reserved` is `DllMain`'s third argument (non-zero for a DLL the
/// executable imports). False if `DllMain` returned `FALSE`.
///
/// # Safety
/// Runs title code. The DLL's imports must be bound, and the DLL finalized.
pub unsafe fn attach_dll(image: &NativeImage, reason: u32, reserved: u64) -> bool {
    let base = image.base();
    let main = |reason| match image.entry() {
        // The Windows `BOOL` is the low 32 bits of `rax`.
        Some(entry) => {
            // SAFETY: forwarded from the caller; `DllMain`'s signature.
            let result = unsafe { call_win64(entry, &[base as u64, u64::from(reason), reserved]) };
            result as u32 != 0
        }
        None => true,
    };
    if reason == DLL_PROCESS_DETACH {
        let ok = main(reason);
        // SAFETY: forwarded from the caller.
        unsafe { attach(image, reason) };
        ok
    } else {
        // SAFETY: forwarded from the caller.
        unsafe { attach(image, reason) };
        main(reason)
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
        let code = unsafe { call_thread_entry(start, param) };
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
    // SAFETY: forwarded from the caller.
    unsafe { run_entry_with(image, &[]) }
}

/// [`run_entry`] after starting the title's DLLs (`dlls`, dependencies
/// first, as `map_title_dlls` returns them) on the same thread: each hears
/// `DLL_PROCESS_ATTACH` (TLS callbacks, then `DllMain`) and then every
/// thread start and end. A `DllMain` that returns `FALSE` stops the title
/// with an error, with the DLLs started before it detached.
///
/// # Safety
/// As for [`run_entry`], and the DLLs' imports must be bound and the DLLs
/// finalized.
pub unsafe fn run_entry_with(
    image: &'static NativeImage,
    dlls: &[&'static super::dll::TitleDll],
) -> Result<u32> {
    let entry = image.entry().context("image has no entry point")?;
    let stack = usize::try_from(image.info().stack_reserve)
        .unwrap_or(usize::MAX)
        .max(1 << 20);
    teb::set_image_base(image.base());
    victory_platform::watch::set_image(image.base(), image.len());
    super::dump::start();
    super::dump::start_mem_watch();
    super::dump::start_hold_byte();
    super::dump::start_write_trap();
    super::bridge::start_profile();
    let dlls = dlls.to_vec();
    let failure = std::sync::Arc::new(Mutex::new(None::<String>));
    let failed = failure.clone();
    let thread = thread::spawn("title-main", stack, move || {
        // SAFETY: forwarded from the caller.
        unsafe {
            for (i, dll) in dlls.iter().enumerate() {
                if !attach_dll(&dll.image, DLL_PROCESS_ATTACH, 1) {
                    for done in dlls[..i].iter().rev() {
                        attach_dll(&done.image, DLL_PROCESS_DETACH, 1);
                    }
                    *failed.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(format!("DllMain of {} returned FALSE", dll.name));
                    return STATUS_DLL_INIT_FAILED;
                }
                register_dll_thread_notices(&dll.image);
            }
            register_thread_callbacks(image);
            attach(image, DLL_PROCESS_ATTACH);
            let code = call_thread_entry(entry, 0);
            attach(image, DLL_PROCESS_DETACH);
            for dll in dlls.iter().rev() {
                attach_dll(&dll.image, DLL_PROCESS_DETACH, 1);
            }
            code
        }
    })
    .context("spawning title main thread")?;
    let code = thread.join();
    match failure.lock().unwrap_or_else(|e| e.into_inner()).take() {
        Some(message) => Err(anyhow::anyhow!(message)),
        None => Ok(code),
    }
}
