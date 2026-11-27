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
    if let Err(e) = check("mach_vm_protect", unsafe {
        mach_vm_protect(task(), addr, len as u64, 0, 0)
    }) {
        // SAFETY: release the allocation we just made so it does not leak.
        unsafe { mach_vm_deallocate(task(), addr, len as u64) };
        return Err(e);
    }
    Ok(addr as *mut u8)
}

/// Reserves and commits `len` bytes in one step (`at` as for [`reserve`]).
///
/// # Safety
/// As [`reserve`].
pub unsafe fn alloc(at: Option<usize>, len: usize, prot: Protect) -> io::Result<*mut u8> {
    // SAFETY: forwarded from the caller.
    let p = unsafe { reserve(at, len) }?;
    // SAFETY: the reservation just made.
    if let Err(e) = unsafe { commit(p, len, prot) } {
        // SAFETY: as above.
        unsafe { release(p, len) }?;
        return Err(e);
    }
    Ok(p)
}

/// Makes code just written to `[addr, addr + len)` visible to the CPU. Only
/// an arm64 host needs it.
///
/// # Safety
/// The range must be mapped.
pub unsafe fn flush_icache(addr: *mut u8, len: usize) {
    #[cfg(target_arch = "aarch64")]
    {
        unsafe extern "C" {
            fn sys_icache_invalidate(start: *mut u8, len: usize);
        }
        // SAFETY: forwarded from the caller.
        unsafe { sys_icache_invalidate(addr, len) };
    }
    #[cfg(not(target_arch = "aarch64"))]
    let _ = (addr, len);
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
        mach_vm_allocate(
            task(),
            &mut at,
            len as u64,
            VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE,
        )
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
    next_region(addr).filter(|r| r.base <= addr)
}

/// Reserves `len` bytes (no access) at the lowest free address that is a
/// multiple of `align` and at or above `lowest`, provided the range ends at
/// or below `highest` (inclusive). First fit from `lowest`, so a miss means
/// no free range of that size below `highest` exists.
///
/// # Safety
/// A reservation is the caller's to release.
pub unsafe fn reserve_in(
    lowest: usize,
    highest: usize,
    len: usize,
    align: usize,
) -> io::Result<*mut u8> {
    let Some(step) = align.max(page_size()).checked_next_power_of_two() else {
        return Err(io::Error::other("alignment overflow"));
    };
    // A gap the walk sees as free may not be (a test binary's page zero is
    // not a region): the next one up then.
    let mut from = lowest;
    while let Some(start) = free_gap(from, highest, len, align) {
        // SAFETY: the gap is free as far as the region walk saw; FIXED fails if not.
        if let Ok(p) = unsafe { reserve(Some(start), len) } {
            return Ok(p);
        }
        let Some(next) = start.checked_add(step) else {
            break;
        };
        from = next;
    }
    Err(io::Error::other("no free address range below the limit"))
}

/// The lowest address at or above `lowest`, a multiple of `align`, where
/// `len` bytes lie free and end at or below `highest`.
fn free_gap(lowest: usize, highest: usize, len: usize, align: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    // Walk the mapped regions for the first gap that fits: the kernel's own
    // "anywhere" placement ignores the hint when the process is full of
    // libraries (a window brings Metal's) and lands above `highest`.
    let align = align.max(page_size()).checked_next_power_of_two()?;
    let up = |a: usize| a.checked_add(align - 1).map(|v| v & !(align - 1));
    let mut at = up(lowest);
    while let Some(start) = at {
        let end = start.checked_add(len).filter(|&e| e - 1 <= highest)?;
        match next_region(start) {
            Some(r) if r.base < end => at = r.base.checked_add(r.len).and_then(up),
            _ => return Some(start),
        }
    }
    None
}

/// The process's memory footprint in bytes (what the system charges it for).
pub fn footprint() -> usize {
    const TASK_VM_INFO: i32 = 22;
    unsafe extern "C" {
        fn task_info(task: MachPort, flavor: i32, info: *mut u64, count: *mut u32) -> KernReturn;
    }
    // `task_vm_info`: `phys_footprint` is the u64 at byte 144.
    let mut info = [0u64; 64];
    let mut count = 128u32;
    // SAFETY: a buffer of `count` 32-bit words.
    let kr = unsafe { task_info(task(), TASK_VM_INFO, info.as_mut_ptr(), &mut count) };
    if kr == KERN_SUCCESS && count >= 38 {
        info[18] as usize
    } else {
        0
    }
}

/// The first mapped region that ends above `addr`: the one containing it, or
/// the next one up. `None` if nothing is mapped from there on.
pub fn next_region(addr: usize) -> Option<Region> {
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
    (kr == KERN_SUCCESS).then_some(Region {
        base: at as usize,
        len: size as usize,
        protect: Protect(info.protection as u32),
    })
}

/// A pool of pages that can be mapped, at once, at many addresses. One
/// Mach memory entry stands behind it, so writes through any mapping are
/// seen through all, and `mach_vm_protect` over a range spanning many
/// mappings is one call.
///
/// Every pool also has one mapping of the whole of it, read-write, at
/// [`base`](PagePool::base): never moved, protected or unmapped while the
/// pool lives, so a GPU backend can wrap it once (`newBufferWithBytesNoCopy`).
/// Other mappings come and go at the caller's addresses.
#[derive(Debug)]
pub struct PagePool {
    mapping: PoolMapping<MachCleanup>,
}

#[derive(Debug)]
struct PoolMapping<C: PoolCleanup> {
    entry: MachPort,
    len: usize,
    base: usize,
    cleanup: C,
}

trait PoolCleanup {
    fn unmap(&self, base: usize, len: usize);
    fn release_entry(&self, entry: MachPort);
}

#[derive(Debug)]
struct MachCleanup;

impl PoolCleanup for MachCleanup {
    fn unmap(&self, base: usize, len: usize) {
        // SAFETY: PoolMapping owns the mapping until drop.
        unsafe { mach_vm_deallocate(task(), base as u64, len as u64) };
    }

    fn release_entry(&self, entry: MachPort) {
        // SAFETY: this is the send right acquired for the pool.
        unsafe { mach_port_deallocate(task(), entry) };
    }
}

impl<C: PoolCleanup> PoolMapping<C> {
    fn new(
        entry: MachPort,
        len: usize,
        cleanup: C,
        map: impl FnOnce(MachPort, usize) -> io::Result<*mut u8>,
    ) -> io::Result<Self> {
        let base = map(entry, len).inspect_err(|_| cleanup.release_entry(entry))?;
        Ok(Self {
            entry,
            len,
            base: base as usize,
            cleanup,
        })
    }
}

impl<C: PoolCleanup> Drop for PoolMapping<C> {
    fn drop(&mut self) {
        self.cleanup.unmap(self.base, self.len);
        self.cleanup.release_entry(self.entry);
    }
}

// SAFETY: a Mach port name is a process-wide integer.
unsafe impl Send for PagePool {}
// SAFETY: as above; the API takes `&self` only.
unsafe impl Sync for PagePool {}

/// The kernel's page size (16 KiB, also in an x86-64 process under Rosetta,
/// where [`page_size`] says 4 KiB): pools and their bases are a multiple of it.
const KERNEL_PAGE: usize = 0x4000;

impl PagePool {
    /// A pool of `len` bytes (rounded up to 16 KiB), zero filled, committed
    /// as it is first touched.
    pub fn new(len: usize) -> io::Result<PagePool> {
        let mut size = len.next_multiple_of(KERNEL_PAGE) as u64;
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
        let mapping = PoolMapping::new(entry, size as usize, MachCleanup, |entry, len| {
            // SAFETY: map the entire new entry anywhere, without replacing a caller's range.
            unsafe { Self::map_entry(entry, 0, len, None, Protect::READ_WRITE) }
        })?;
        Ok(PagePool { mapping })
    }

    /// As [`new`](PagePool::new). Memory is committed on first touch here
    /// whichever way; Windows must be told.
    pub fn new_lazy(len: usize) -> io::Result<PagePool> {
        PagePool::new(len)
    }

    pub fn len(&self) -> usize {
        self.mapping.len
    }

    pub fn is_empty(&self) -> bool {
        self.mapping.len == 0
    }

    /// The pool's permanent read-write mapping of all of it (always `Some`
    /// here; a Windows host may fail to map a large pool).
    pub fn base(&self) -> Option<*mut u8> {
        Some(self.mapping.base as *mut u8)
    }

    /// The Mach memory entry (a send right the pool keeps): a `mach_vm_map`
    /// of the caller's own maps the same pages.
    pub fn memory_entry(&self) -> u32 {
        self.mapping.entry
    }

    /// Bytes `[offset, offset + len)` through [`base`](PagePool::base): no
    /// mapping of its own here.
    pub fn window(&self, offset: usize, len: usize) -> io::Result<Window> {
        if offset.checked_add(len).is_none_or(|end| end > self.mapping.len) {
            return Err(io::Error::other("range outside the page pool"));
        }
        Ok(Window {
            at: self.mapping.base + offset,
        })
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
        if offset.checked_add(len).is_none_or(|end| end > self.mapping.len) {
            return Err(io::Error::other("range outside the page pool"));
        }
        // SAFETY: the range is within the entry and the caller owns any fixed address.
        unsafe { Self::map_entry(self.mapping.entry, offset, len, at, prot) }
    }

    unsafe fn map_entry(
        entry: MachPort,
        offset: usize,
        len: usize,
        at: Option<usize>,
        prot: Protect,
    ) -> io::Result<*mut u8> {
        let try_map = |at: u64, flags: i32, mask: u64| -> io::Result<*mut u8> {
            let mut addr = at;
            // SAFETY: maps our own memory entry; FIXED|OVERWRITE replaces the
            // caller's range, which the caller guaranteed.
            check("mach_vm_map", unsafe {
                mach_vm_map(
                    task(),
                    &mut addr,
                    len as u64,
                    mask,
                    flags,
                    entry,
                    offset as u64,
                    0,
                    prot.0 as i32,
                    VM_PROT_READ | VM_PROT_WRITE,
                    VM_INHERIT_NONE,
                )
            })?;
            Ok(addr as *mut u8)
        };
        if let Some(at) = at {
            return try_map(at as u64, VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE, 0);
        }
        // A mapping of our own goes above 4 GiB, which the kernel does not
        // do by itself: the low 4 GiB is where 32-bit `XMemAlloc` memory
        // must come from, and gigabytes of pool bases would fill it.
        if let Some(high) = free_gap(1 << 32, 1 << 46, len, KERNEL_PAGE)
            && let Ok(p) = try_map(high as u64, VM_FLAGS_FIXED, 0)
        {
            return Ok(p);
        }
        // 16 KiB aligned anywhere, also under Rosetta (4 KiB there).
        try_map(0, VM_FLAGS_ANYWHERE, KERNEL_PAGE as u64 - 1)
    }

    /// Maps `[offset, offset + len)` at `at`, in place of what is there (a
    /// part of a reservation of the caller's). On Windows the range must lie
    /// in a placeholder, which it splits as needed.
    ///
    /// # Safety
    /// As [`map`](PagePool::map).
    pub unsafe fn map_over(
        &self,
        offset: usize,
        len: usize,
        at: usize,
        prot: Protect,
    ) -> io::Result<()> {
        // SAFETY: forwarded from the caller.
        unsafe { self.map(offset, len, Some(at), prot) }.map(|_| ())
    }
}

/// Bytes of a pool, writable in place (see [`PagePool::window`]).
#[derive(Debug)]
pub struct Window {
    at: usize,
}

impl Window {
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.at as *mut u8
    }
}

/// Returns a mapping made by [`PagePool::map_over`] to what a reservation
/// is: no access, nothing behind it. On Windows the whole view that starts
/// at `addr` goes, whatever `len`, and becomes a placeholder.
///
/// # Safety
/// The range must be the caller's, and not used afterwards.
pub unsafe fn unmap_over(addr: *mut u8, len: usize) -> io::Result<()> {
    // SAFETY: forwarded from the caller.
    unsafe { decommit(addr, len) }
}

/// Unmaps a view made by [`PagePool::map`].
///
/// # Safety
/// The view must not be used afterwards.
pub unsafe fn unmap(addr: *mut u8, len: usize) -> io::Result<()> {
    // SAFETY: forwarded from the caller.
    unsafe { release(addr, len) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    enum CleanupCall {
        Unmap(usize, usize),
        Release(MachPort),
    }

    #[derive(Default)]
    struct CleanupLog(std::cell::RefCell<Vec<CleanupCall>>);

    impl PoolCleanup for &CleanupLog {
        fn unmap(&self, base: usize, len: usize) {
            self.0.borrow_mut().push(CleanupCall::Unmap(base, len));
        }

        fn release_entry(&self, entry: MachPort) {
            self.0.borrow_mut().push(CleanupCall::Release(entry));
        }
    }

    #[test]
    fn failed_pool_mapping_only_releases_the_entry() {
        let log = CleanupLog::default();
        let result = PoolMapping::new(42, KERNEL_PAGE, &log, |_, _| {
            Err(io::Error::other("mapping failed"))
        });
        assert!(result.is_err());
        assert_eq!(*log.0.borrow(), [CleanupCall::Release(42)]);
    }

    #[test]
    fn pool_drop_unmaps_the_returned_range_then_releases_the_entry() {
        let log = CleanupLog::default();
        let base = 0x1_2345_0000;
        let len = 3 * KERNEL_PAGE;
        let mapping = PoolMapping::new(42, len, &log, |entry, size| {
            assert_eq!((entry, size), (42, len));
            Ok(base as *mut u8)
        })
        .unwrap();
        assert!(log.0.borrow().is_empty());
        drop(mapping);
        assert_eq!(
            *log.0.borrow(),
            [CleanupCall::Unmap(base, len), CleanupCall::Release(42)]
        );
    }

    #[test]
    fn free_gap_rejects_zero_length_and_overflowing_alignment() {
        assert_eq!(free_gap(0, usize::MAX, 0, page_size()), None);
        assert_eq!(free_gap(0x10000, usize::MAX, 0, page_size()), None);
        assert_eq!(free_gap(0, usize::MAX, page_size(), (usize::MAX >> 1) + 2), None);
        assert_eq!(free_gap(usize::MAX, usize::MAX, page_size(), page_size()), None);
    }
}
