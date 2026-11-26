//! The thread environment block title code expects at `gs:`.
//!
//! Title code reads `gs:[0x30]` (TEB self), `gs:[0x58]` (the thread's static
//! TLS array), `gs:[0x60]` (the PEB), `gs:[0x10]` (stack limit) and so on. A
//! macOS x86-64 process, Rosetta included, keeps pthread's thread-specific
//! data at `gs:` instead, and slots 11 and 12 of it (our `0x58` and `0x60`)
//! belong to libSystem's own keys. So each thread has two GS bases: the
//! host's, while host code runs, and this TEB, while title code runs. The
//! bridge (`bridge.rs`) swaps them with the `thread_fast_set_cthread_self`
//! machdep call at every crossing, which Rosetta supports (about 190 ns
//! each way on an M-series Mac). `docs/native-host.md` has the numbers and
//! the alternatives.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::cell::RefCell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::image::TlsTemplate;

pub const TEB_SIZE: usize = 0x2000;
pub const OFF_STACK_BASE: usize = 0x08;
pub const OFF_STACK_LIMIT: usize = 0x10;
pub const OFF_SELF: usize = 0x30;
pub const OFF_PROCESS_ID: usize = 0x40;
pub const OFF_THREAD_ID: usize = 0x48;
pub const OFF_TLS_ARRAY: usize = 0x58;
pub const OFF_PEB: usize = 0x60;
pub const OFF_LAST_ERROR: usize = 0x68;
pub const OFF_TLS_SLOTS: usize = 0x1480;
pub const TLS_SLOT_COUNT: usize = 64;
/// Ours, in the part of the block Windows leaves unused: the thread's host
/// GS base, which the bridge swaps back to.
pub const OFF_HOST_TSD: usize = 0x1F00;
/// Modules with static TLS one thread can hold (the array's length).
pub const MAX_TLS_MODULES: usize = 64;
const PEB_SIZE: usize = 0x800;
const OFF_PEB_IMAGE_BASE: usize = 0x10;

static TLS_MODULES: Mutex<Vec<TlsTemplate>> = Mutex::new(Vec::new());
static PEB: AtomicUsize = AtomicUsize::new(0);

/// Gives a mapped image its TLS index, writes it where the image reads
/// `_tls_index`, and arranges for every thread started from now on to get a
/// block. Call before [`super::NativeImage::finalize`] makes that word
/// read-only, and before any thread runs title code.
pub fn register_tls(template: TlsTemplate) -> u32 {
    let mut modules = TLS_MODULES.lock().unwrap_or_else(|e| e.into_inner());
    let index = modules.len();
    assert!(index < MAX_TLS_MODULES, "too many modules with static TLS");
    modules.push(template);
    // SAFETY: the template's index word is in the image the caller mapped.
    unsafe { (template.index_at as *mut u32).write(index as u32) };
    index as u32
}

fn peb() -> usize {
    let p = PEB.load(Ordering::Acquire);
    if p != 0 {
        return p;
    }
    let layout = Layout::from_size_align(PEB_SIZE, 0x1000).expect("layout");
    // SAFETY: a non-zero layout.
    let fresh = unsafe { alloc_zeroed(layout) } as usize;
    match PEB.compare_exchange(0, fresh, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => fresh,
        Err(winner) => {
            // SAFETY: ours, never shared.
            unsafe { dealloc(fresh as *mut u8, layout) };
            winner
        }
    }
}

/// Records the title's main image in the PEB (`ImageBaseAddress`).
pub fn set_image_base(base: usize) {
    // SAFETY: inside our PEB block.
    unsafe { ((peb() + OFF_PEB_IMAGE_BASE) as *mut usize).write(base) };
}

struct ThreadTeb {
    teb: *mut u8,
    tls_array: Box<[usize; MAX_TLS_MODULES]>,
    blocks: Vec<(*mut u8, Layout)>,
}

impl ThreadTeb {
    /// Builds the calling thread's TEB. Must run with the host GS base.
    fn new() -> ThreadTeb {
        super::seh::install();
        let layout = Layout::from_size_align(TEB_SIZE, 0x1000).expect("layout");
        // SAFETY: a non-zero layout.
        let teb = unsafe { alloc_zeroed(layout) };
        assert!(!teb.is_null(), "out of memory for a TEB");
        let mut tls_array = Box::new([0usize; MAX_TLS_MODULES]);
        let mut blocks = Vec::new();
        for (i, t) in TLS_MODULES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .enumerate()
        {
            let layout = Layout::from_size_align(t.block_len().max(1), 64).expect("layout");
            // SAFETY: a non-zero layout; the template is in a mapped image.
            let block = unsafe { alloc_zeroed(layout) };
            // SAFETY: the template's initial data is `end - start` readable bytes.
            unsafe { std::ptr::copy_nonoverlapping(t.start as *const u8, block, t.end - t.start) };
            tls_array[i] = block as usize;
            blocks.push((block, layout));
        }
        let (top, size) = stack_bounds();
        let put = |off: usize, v: usize| {
            // SAFETY: inside the TEB block.
            unsafe { teb.add(off).cast::<usize>().write(v) }
        };
        put(OFF_STACK_BASE, top);
        put(OFF_STACK_LIMIT, top - size);
        put(OFF_SELF, teb as usize);
        put(OFF_PROCESS_ID, std::process::id() as usize);
        put(
            OFF_THREAD_ID,
            victory_platform::thread::current_id() as usize,
        );
        put(OFF_TLS_ARRAY, tls_array.as_ptr() as usize);
        put(OFF_PEB, peb());
        put(OFF_HOST_TSD, host_tsd_base());
        ThreadTeb {
            teb,
            tls_array,
            blocks,
        }
    }
}

impl Drop for ThreadTeb {
    fn drop(&mut self) {
        let _ = &self.tls_array;
        for (block, layout) in self.blocks.drain(..) {
            // SAFETY: allocated in `new` with this layout.
            unsafe { dealloc(block, layout) };
        }
        let layout = Layout::from_size_align(TEB_SIZE, 0x1000).expect("layout");
        // SAFETY: allocated in `new`.
        unsafe { dealloc(self.teb, layout) };
    }
}

thread_local! {
    static CURRENT: RefCell<Option<ThreadTeb>> = const { RefCell::new(None) };
}

/// The calling thread's TEB, made on first use. Call it from host code
/// (the host GS base).
pub fn current() -> *mut u8 {
    CURRENT.with(|c| c.borrow_mut().get_or_insert_with(ThreadTeb::new).teb)
}

/// `GetLastError`'s variable for the calling thread.
pub fn last_error_slot() -> *mut u32 {
    // SAFETY: inside the TEB block.
    unsafe { current().add(OFF_LAST_ERROR).cast() }
}

/// The 64 slots `TlsAlloc` hands out, for the calling thread.
pub fn tls_slots() -> *mut usize {
    // SAFETY: inside the TEB block.
    unsafe { current().add(OFF_TLS_SLOTS).cast() }
}

unsafe extern "C" {
    fn pthread_self() -> usize;
    fn pthread_get_stackaddr_np(thread: usize) -> usize;
    fn pthread_get_stacksize_np(thread: usize) -> usize;
}

/// Top of the stack and its size.
fn stack_bounds() -> (usize, usize) {
    // SAFETY: queries about the calling thread.
    unsafe {
        let me = pthread_self();
        (pthread_get_stackaddr_np(me), pthread_get_stacksize_np(me))
    }
}

/// Reads `gs:[8 * slot]` of the host's thread-specific data.
fn gs_slot(slot: usize) -> usize {
    let value: usize;
    // SAFETY: slots below 16 exist in every pthread's TSD.
    unsafe {
        std::arch::asm!("mov {}, gs:[{}]", out(reg) value, in(reg) slot * 8, options(nostack, readonly))
    };
    value
}

/// The calling thread's host GS base: the address of the pthread's
/// thread-specific data, whose first word is `pthread_self()`.
///
/// The TSD array sits inside the pthread structure at an offset that differs
/// between macOS versions (and the structure holds other words equal to
/// itself), so each candidate has to match the first words read through GS.
fn host_tsd_base() -> usize {
    let me = gs_slot(0);
    (0..0x400)
        .step_by(8)
        .map(|off| me + off)
        // SAFETY: inside the pthread structure, which is larger than 0x400 bytes.
        .find(|&p| (0..12).all(|i| unsafe { (p as *const usize).add(i).read() } == gs_slot(i)))
        .expect("cannot locate the pthread TSD base")
}
