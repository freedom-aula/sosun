//! Page-aligned host reservations, standing in for what the native host will hand us.

use std::ffi::c_void;

/// Apple Silicon pages are 16 KiB; `bytesNoCopy` wants this alignment and a multiple of it in length.
pub fn page_size() -> usize {
    // SAFETY: sysconf has no preconditions.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

/// An anonymous mapping, zero-filled, optionally at a requested fixed address.
pub struct Region {
    pub ptr: *mut u8,
    pub len: usize,
}

impl Region {
    /// `fixed` maps at exactly that address (how the title sees its own memory).
    pub fn new(len: usize, fixed: Option<usize>) -> Option<Region> {
        let mut flags = libc::MAP_ANON | libc::MAP_PRIVATE;
        if fixed.is_some() {
            flags |= libc::MAP_FIXED;
        }
        let want = fixed.unwrap_or(0) as *mut c_void;
        // SAFETY: anonymous mapping; a fixed address is the caller's chosen free range.
        let p = unsafe { libc::mmap(want, len, libc::PROT_READ | libc::PROT_WRITE, flags, -1, 0) };
        (p != libc::MAP_FAILED).then(|| Region { ptr: p.cast(), len })
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: ptr/len came from mmap in `new`.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

unsafe extern "C" {
    fn mach_task_self() -> u32;
    fn mach_vm_allocate(task: u32, addr: *mut u64, size: u64, flags: i32) -> i32;
    fn mach_vm_deallocate(task: u32, addr: u64, size: u64) -> i32;
    #[allow(clippy::too_many_arguments)]
    fn mach_vm_remap(
        target: u32,
        addr: *mut u64,
        size: u64,
        mask: u64,
        flags: i32,
        src_task: u32,
        src_addr: u64,
        copy: i32,
        cur: *mut i32,
        max: *mut i32,
        inherit: u32,
    ) -> i32;
}

const VM_FLAGS_ANYWHERE: i32 = 1;
const VM_INHERIT_SHARE: u32 = 0;

/// Two virtual aliases of the same physical pages (the shape of "one mach VM object, mapped twice").
pub struct Alias {
    pub primary: *mut u8,
    pub alias: *mut u8,
    pub len: usize,
}

impl Alias {
    pub fn new(len: usize) -> Result<Alias, i32> {
        let mut a = 0u64;
        // SAFETY: plain Mach VM calls on our own task with local out-parameters.
        unsafe {
            let t = mach_task_self();
            let r = mach_vm_allocate(t, &mut a, len as u64, VM_FLAGS_ANYWHERE);
            if r != 0 {
                return Err(r);
            }
            let (mut b, mut cur, mut max) = (0u64, 0i32, 0i32);
            let r = mach_vm_remap(
                t,
                &mut b,
                len as u64,
                0,
                VM_FLAGS_ANYWHERE,
                t,
                a,
                0,
                &mut cur,
                &mut max,
                VM_INHERIT_SHARE,
            );
            if r != 0 {
                mach_vm_deallocate(t, a, len as u64);
                return Err(r);
            }
            Ok(Alias {
                primary: a as *mut u8,
                alias: b as *mut u8,
                len,
            })
        }
    }
}

impl Drop for Alias {
    fn drop(&mut self) {
        // SAFETY: both ranges came from the Mach calls in `new`.
        unsafe {
            let t = mach_task_self();
            mach_vm_deallocate(t, self.primary as u64, self.len as u64);
            mach_vm_deallocate(t, self.alias as u64, self.len as u64);
        }
    }
}
