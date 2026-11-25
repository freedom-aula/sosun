//! Win32 implementation of `mem`: the calls the loader and runtime made
//! directly before the seam existed.

use std::ffi::c_void;
use std::io;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER,
    MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile3, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_NOACCESS, PAGE_READONLY,
    PAGE_READWRITE, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualProtect,
    VirtualQuery,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Page protection, as the raw `PAGE_*` value (so a value the host returns,
/// such as `PAGE_WRITECOPY`, can be passed back unchanged).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Protect(pub u32);

impl Protect {
    pub const NO_ACCESS: Protect = Protect(PAGE_NOACCESS);
    pub const READ: Protect = Protect(PAGE_READONLY);
    pub const READ_WRITE: Protect = Protect(PAGE_READWRITE);
    pub const READ_EXECUTE: Protect = Protect(PAGE_EXECUTE_READ);
    pub const READ_WRITE_EXECUTE: Protect = Protect(PAGE_EXECUTE_READWRITE);
}

fn system_info() -> SYSTEM_INFO {
    // SAFETY: fills a plain struct.
    unsafe {
        let mut info = std::mem::zeroed();
        GetSystemInfo(&mut info);
        info
    }
}

pub fn page_size() -> usize {
    system_info().dwPageSize as usize
}

/// 64 KiB: where reservations and views start.
pub fn granularity() -> usize {
    system_info().dwAllocationGranularity as usize
}

/// Reserves `len` bytes of address space, no access, at `at` exactly if
/// given (failing if anything is mapped there).
///
/// # Safety
/// `at`, if given, must be aligned to [`granularity`]; a reservation is the
/// caller's to release.
pub unsafe fn reserve(at: Option<usize>, len: usize) -> io::Result<*mut u8> {
    let at = at.map_or(std::ptr::null(), |a| a as *const c_void);
    // SAFETY: reserves fresh address space only.
    let p = unsafe { VirtualAlloc(at, len, MEM_RESERVE, PAGE_NOACCESS) };
    if p.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(p.cast())
}

/// Reserves and commits `len` bytes in one step (`at` as for [`reserve`]).
///
/// # Safety
/// As [`reserve`].
pub unsafe fn alloc(at: Option<usize>, len: usize, prot: Protect) -> io::Result<*mut u8> {
    let at = at.map_or(std::ptr::null(), |a| a as *const c_void);
    // SAFETY: reserves fresh address space only.
    let p = unsafe { VirtualAlloc(at, len, MEM_RESERVE | MEM_COMMIT, prot.0) };
    if p.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(p.cast())
}

/// Makes code just written to `[addr, addr + len)` visible to the CPU.
///
/// # Safety
/// The range must be mapped.
pub unsafe fn flush_icache(addr: *mut u8, len: usize) {
    // SAFETY: flushing our own process's instruction cache.
    unsafe { FlushInstructionCache(GetCurrentProcess(), addr.cast(), len) };
}

/// Makes `[addr, addr + len)` of a reservation usable, zero filled.
///
/// # Safety
/// The range must lie in a reservation of the caller.
pub unsafe fn commit(addr: *mut u8, len: usize, prot: Protect) -> io::Result<()> {
    // SAFETY: forwarded from the caller.
    let p = unsafe { VirtualAlloc(addr.cast(), len, MEM_COMMIT, prot.0) };
    if p.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sets the protection of a range and returns the first page's previous one.
///
/// # Safety
/// The range must be mapped and owned by the caller.
pub unsafe fn protect(addr: *mut u8, len: usize, prot: Protect) -> io::Result<Protect> {
    let mut old = 0;
    // SAFETY: forwarded from the caller.
    if unsafe { VirtualProtect(addr.cast(), len, prot.0, &mut old) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Protect(old))
}

/// Gives the pages back and returns the range to reserved, no access.
///
/// # Safety
/// The range must lie in a reservation of the caller.
pub unsafe fn decommit(addr: *mut u8, len: usize) -> io::Result<()> {
    // SAFETY: forwarded from the caller.
    if unsafe { VirtualFree(addr.cast(), len, MEM_DECOMMIT) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Releases the whole reservation that starts at `addr` (`len` is ignored
/// here; on macOS it is the length unmapped).
///
/// # Safety
/// The range must be the caller's, and not used afterwards.
pub unsafe fn release(addr: *mut u8, _len: usize) -> io::Result<()> {
    // SAFETY: forwarded from the caller.
    if unsafe { VirtualFree(addr.cast(), 0, MEM_RELEASE) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// What is mapped at an address.
#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub base: usize,
    pub len: usize,
    pub protect: Protect,
}

/// The mapped region containing `addr` (a run of pages with one protection),
/// or `None` if it is free.
pub fn query(addr: usize) -> Option<Region> {
    // SAFETY: fills a plain struct.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let n = unsafe { VirtualQuery(addr as *const c_void, &mut info, size_of_val(&info)) };
    use windows_sys::Win32::System::Memory::MEM_FREE;
    (n != 0 && info.State != MEM_FREE).then_some(Region {
        base: info.BaseAddress as usize,
        len: info.RegionSize,
        // Windows leaves `Protect` at 0 for reserved pages.
        protect: if info.State == MEM_RESERVE {
            Protect::NO_ACCESS
        } else {
            Protect(info.Protect)
        },
    })
}

/// A pool of pages that can be mapped, at once, at many addresses: a
/// pagefile-backed section. Each view is one Windows mapping (and under Wine
/// one wineserver view), 64 KiB granular.
#[derive(Debug)]
pub struct PagePool {
    section: HANDLE,
    len: usize,
}

// SAFETY: a section handle is process-wide.
unsafe impl Send for PagePool {}
// SAFETY: as above; the API takes `&self` only.
unsafe impl Sync for PagePool {}

impl PagePool {
    /// A pool of `len` bytes (a multiple of [`granularity`]), zero filled.
    pub fn new(len: usize) -> io::Result<PagePool> {
        // SAFETY: plain section creation.
        let section = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                (len as u64 >> 32) as u32,
                len as u32,
                std::ptr::null(),
            )
        };
        if section.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(PagePool { section, len })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Maps `[offset, offset + len)` of the pool, at `at` if given (the range
    /// must be free here, unlike on macOS where it is replaced), else
    /// anywhere. Returns the address.
    ///
    /// # Safety
    /// `at` must be aligned to [`granularity`] and free.
    pub unsafe fn map(
        &self,
        offset: usize,
        len: usize,
        at: Option<usize>,
        prot: Protect,
    ) -> io::Result<*mut u8> {
        // SAFETY: returns a pseudo handle.
        let process = unsafe { GetCurrentProcess() };
        let (base, kind) = match at {
            Some(at) => {
                // A view goes over a placeholder, so make one first.
                // SAFETY: reserves only; fails if the range is taken.
                let p = unsafe {
                    VirtualAlloc2(
                        process,
                        at as *const c_void,
                        len,
                        MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                        PAGE_NOACCESS,
                        std::ptr::null_mut(),
                        0,
                    )
                };
                if p.is_null() {
                    return Err(io::Error::last_os_error());
                }
                (at as *const c_void, MEM_REPLACE_PLACEHOLDER)
            }
            None => (std::ptr::null(), 0),
        };
        // SAFETY: maps our own section over our own placeholder (or anywhere).
        let view = unsafe {
            MapViewOfFile3(
                self.section,
                process,
                base,
                offset as u64,
                len,
                kind,
                prot.0,
                std::ptr::null_mut(),
                0,
            )
        };
        if view.Value.is_null() {
            let error = io::Error::last_os_error();
            if let Some(at) = at {
                // SAFETY: the placeholder made above.
                unsafe { VirtualFree(at as *mut c_void, 0, MEM_RELEASE) };
            }
            return Err(error);
        }
        Ok(view.Value.cast())
    }
}

impl Drop for PagePool {
    fn drop(&mut self) {
        // SAFETY: our own handle; views keep the section alive.
        unsafe { CloseHandle(self.section) };
    }
}

/// Unmaps a view made by [`PagePool::map`].
///
/// # Safety
/// The view must not be used afterwards.
pub unsafe fn unmap(addr: *mut u8, _len: usize) -> io::Result<()> {
    // SAFETY: forwarded from the caller.
    let ok = unsafe {
        UnmapViewOfFile2(
            GetCurrentProcess(),
            MEMORY_MAPPED_VIEW_ADDRESS { Value: addr.cast() },
            0,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
