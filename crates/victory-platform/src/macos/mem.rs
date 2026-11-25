//! Mach VM implementation of `mem`.

use std::io;

type KernReturn = i32;
type MachPort = u32;

const KERN_SUCCESS: KernReturn = 0;
const VM_FLAGS_FIXED: i32 = 0;
const VM_FLAGS_ANYWHERE: i32 = 1;
const VM_FLAGS_OVERWRITE: i32 = 0x4000;
const VM_INHERIT_NONE: u32 = 2;
const MAP_MEM_NAMED_CREATE: i32 = 0x2_0000;
const VM_REGION_BASIC_INFO_64: i32 = 9;
const VM_REGION_BASIC_INFO_COUNT_64: u32 = 9;

const VM_PROT_READ: i32 = 1;
const VM_PROT_WRITE: i32 = 2;
const VM_PROT_EXECUTE: i32 = 4;

#[repr(C)]
#[derive(Default)]
struct RegionBasicInfo64 {
    protection: i32,
    max_protection: i32,
    inheritance: u32,
    shared: u32,
    reserved: u32,
    offset: u64,
    behavior: i32,
    user_wired_count: u16,
}

unsafe extern "C" {
    static mach_task_self_: MachPort;
    fn mach_vm_allocate(task: MachPort, addr: *mut u64, size: u64, flags: i32) -> KernReturn;
    fn mach_vm_deallocate(task: MachPort, addr: u64, size: u64) -> KernReturn;
    fn mach_vm_protect(task: MachPort, addr: u64, size: u64, max: u32, prot: i32) -> KernReturn;
    #[allow(clippy::too_many_arguments)]
    fn mach_vm_map(
        task: MachPort,
        addr: *mut u64,
        size: u64,
        mask: u64,
        flags: i32,
        object: MachPort,
        offset: u64,
        copy: u32,
        cur: i32,
        max: i32,
        inherit: u32,
    ) -> KernReturn;
    fn mach_make_memory_entry_64(
        task: MachPort,
        size: *mut u64,
        offset: u64,
        permission: i32,
        handle: *mut MachPort,
        parent: MachPort,
    ) -> KernReturn;
    fn mach_vm_region(
        task: MachPort,
        addr: *mut u64,
        size: *mut u64,
        flavor: i32,
        info: *mut RegionBasicInfo64,
        count: *mut u32,
        object: *mut MachPort,
    ) -> KernReturn;
    fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
}

fn task() -> MachPort {
    // SAFETY: a process-wide constant set by libSystem at start-up.
    unsafe { mach_task_self_ }
}

fn check(call: &str, kr: KernReturn) -> io::Result<()> {
    if kr == KERN_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::other(format!("{call} failed: kern_return {kr}")))
    }
}

/// Page protection, as the raw `VM_PROT_*` bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Protect(pub u32);

impl Protect {
    pub const NO_ACCESS: Protect = Protect(0);
    pub const READ: Protect = Protect(VM_PROT_READ as u32);
    pub const READ_WRITE: Protect = Protect((VM_PROT_READ | VM_PROT_WRITE) as u32);
    pub const READ_EXECUTE: Protect = Protect((VM_PROT_READ | VM_PROT_EXECUTE) as u32);
    pub const READ_WRITE_EXECUTE: Protect =
        Protect((VM_PROT_READ | VM_PROT_WRITE | VM_PROT_EXECUTE) as u32);
}

/// The host's VM page size: 4 KiB in an x86-64 process under Rosetta, 16 KiB
/// in a native arm64 one.
pub fn page_size() -> usize {
    // SAFETY: plain libc query.
    unsafe extern "C" {
        fn getpagesize() -> i32;
    }
    // SAFETY: no preconditions.
    unsafe { getpagesize() as usize }
}

/// Alignment of reservations and views: the page size here (Windows: 64 KiB).
pub fn granularity() -> usize {
    page_size()
}

/// Reserves `len` bytes of address space, no access, at `at` exactly if
/// given (failing if anything is mapped there).
///
/// # Safety
/// `at`, if given, must be page aligned; a reservation is the caller's to release.
pub unsafe fn reserve(at: Option<usize>, len: usize) -> io::Result<*mut u8> {
    let mut addr = at.unwrap_or(0) as u64;
    let flags = if at.is_some() {
        VM_FLAGS_FIXED
    } else {
        VM_FLAGS_ANYWHERE
    };
    // SAFETY: allocates fresh address space; FIXED fails rather than overwrites.
    check("mach_vm_allocate", unsafe {
        mach_vm_allocate(task(), &mut addr, len as u64, flags)
    })?;
    // SAFETY: the range was just allocated.
    check("mach_vm_protect", unsafe {
        mach_vm_protect(task(), addr, len as u64, 0, 0)
    })?;
    Ok(addr as *mut u8)
}

/// Makes `[addr, addr + len)` of a reservation usable, zero filled.
///
/// # Safety
/// The range must lie in a reservation of the caller.
pub unsafe fn commit(addr: *mut u8, len: usize, prot: Protect) -> io::Result<()> {
    // SAFETY: forwarded from the caller.
    unsafe { protect(addr, len, prot) }.map(|_| ())
}

/// Sets the protection of a range and returns the first page's previous one.
///
/// # Safety
/// The range must be mapped and owned by the caller.
pub unsafe fn protect(addr: *mut u8, len: usize, prot: Protect) -> io::Result<Protect> {
    let old = query(addr as usize).map_or(Protect::NO_ACCESS, |r| r.protect);
    // The maximum protection is left alone: it only ever shrinks.
    // SAFETY: forwarded from the caller.
    check("mach_vm_protect", unsafe {
        mach_vm_protect(task(), addr as u64, len as u64, 0, prot.0 as i32)
    })?;
    Ok(old)
}

/// Gives the pages back and returns the range to reserved, no access.
///
/// # Safety
/// The range must lie in a reservation of the caller.
pub unsafe fn decommit(addr: *mut u8, len: usize) -> io::Result<()> {
    let mut at = addr as u64;
    // SAFETY: replaces the caller's own pages with fresh no-access ones.
    check("mach_vm_allocate", unsafe {
        mach_vm_allocate(task(), &mut at, len as u64, VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE)
    })?;
    // SAFETY: just allocated.
    check("mach_vm_protect", unsafe {
        mach_vm_protect(task(), at, len as u64, 0, 0)
    })
}

/// Unmaps `[addr, addr + len)`. On Windows the whole reservation starting at
/// `addr` goes, whatever `len`.
///
/// # Safety
/// The range must be the caller's, and not used afterwards.
pub unsafe fn release(addr: *mut u8, len: usize) -> io::Result<()> {
    // SAFETY: forwarded from the caller.
    check("mach_vm_deallocate", unsafe {
        mach_vm_deallocate(task(), addr as u64, len as u64)
    })
}

/// What is mapped at an address.
#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub base: usize,
    pub len: usize,
    pub protect: Protect,
}

/// The mapped region containing `addr` (a run of pages with one protection),
/// or `None` if nothing is mapped there.
pub fn query(addr: usize) -> Option<Region> {
    let mut at = addr as u64;
    let mut size = 0u64;
    let mut info = RegionBasicInfo64::default();
    let mut count = VM_REGION_BASIC_INFO_COUNT_64;
    let mut object = 0;
    // SAFETY: out-pointers to locals of the sizes the flavour documents.
    let kr = unsafe {
        mach_vm_region(
            task(),
            &mut at,
            &mut size,
            VM_REGION_BASIC_INFO_64,
            &mut info,
            &mut count,
            &mut object,
        )
    };
    // The call returns the next region at or after `addr`.
    (kr == KERN_SUCCESS && at <= addr as u64).then_some(Region {
        base: at as usize,
        len: size as usize,
        protect: Protect(info.protection as u32),
    })
}

/// A pool of pages that can be mapped, at once, at many addresses. One
/// Mach memory entry stands behind it, so writes through any mapping are
/// seen through all, and `mach_vm_protect` over a range spanning many
/// mappings is one call.
#[derive(Debug)]
pub struct PagePool {
    entry: MachPort,
    len: usize,
}

// SAFETY: a Mach port name is a process-wide integer.
unsafe impl Send for PagePool {}
// SAFETY: as above; the API takes `&self` only.
unsafe impl Sync for PagePool {}

impl PagePool {
    /// A pool of `len` bytes (a multiple of the page size), zero filled,
    /// committed as it is first touched.
    pub fn new(len: usize) -> io::Result<PagePool> {
        let mut size = len as u64;
        let mut entry = 0;
        // SAFETY: creates a new anonymous memory object; out-pointers are locals.
        check("mach_make_memory_entry_64", unsafe {
            mach_make_memory_entry_64(
                task(),
                &mut size,
                0,
                MAP_MEM_NAMED_CREATE | VM_PROT_READ | VM_PROT_WRITE,
                &mut entry,
                0,
            )
        })?;
        Ok(PagePool {
            entry,
            len: size as usize,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Maps `[offset, offset + len)` of the pool, at `at` if given (over
    /// whatever is there, as `MAP_FIXED` does), else anywhere. Returns the
    /// address.
    ///
    /// # Safety
    /// `at` must be page aligned and, if given, the range the caller's to replace.
    pub unsafe fn map(
        &self,
        offset: usize,
        len: usize,
        at: Option<usize>,
        prot: Protect,
    ) -> io::Result<*mut u8> {
        if offset.checked_add(len).is_none_or(|end| end > self.len) {
            return Err(io::Error::other("range outside the page pool"));
        }
        let mut addr = at.unwrap_or(0) as u64;
        let flags = if at.is_some() {
            VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE
        } else {
            VM_FLAGS_ANYWHERE
        };
        // SAFETY: maps our own memory entry; FIXED|OVERWRITE replaces the
        // caller's range, which the caller guaranteed.
        check("mach_vm_map", unsafe {
            mach_vm_map(
                task(),
                &mut addr,
                len as u64,
                0,
                flags,
                self.entry,
                offset as u64,
                0,
                prot.0 as i32,
                VM_PROT_READ | VM_PROT_WRITE,
                VM_INHERIT_NONE,
            )
        })?;
        Ok(addr as *mut u8)
    }
}

impl Drop for PagePool {
    fn drop(&mut self) {
        // Mappings keep the memory alive; this only drops our name for it.
        // SAFETY: our own port right.
        unsafe { mach_port_deallocate(task(), self.entry) };
    }
}

/// Unmaps a view made by [`PagePool::map`].
///
/// # Safety
/// The view must not be used afterwards.
pub unsafe fn unmap(addr: *mut u8, len: usize) -> io::Result<()> {
    // SAFETY: forwarded from the caller.
    unsafe { release(addr, len) }
}
