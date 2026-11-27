//! Win32 implementation of `mem`: the calls the loader and runtime made
//! directly before the seam existed.

use std::ffi::c_void;
use std::io;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_WRITE, MEM_COMMIT, MEM_DECOMMIT, MEM_PRESERVE_PLACEHOLDER,
    MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE, MEM_RESERVE_PLACEHOLDER,
    MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, MapViewOfFile3,
    PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
    SEC_RESERVE, UnmapViewOfFile, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree,
    VirtualProtect, VirtualQuery,
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
    /// Our own view of the whole section, mapped the first time it is asked
    /// for and kept while the pool lives (0: it could not be mapped). A view
    /// per request cost a map and an unmap, and under Wine each holds the
    /// process's one virtual-memory lock across a wineserver request: every
    /// page fault and protection change on other threads waits for it.
    whole: OnceLock<usize>,
}

// SAFETY: a section handle is process-wide.
unsafe impl Send for PagePool {}
// SAFETY: as above; the API takes `&self` only.
unsafe impl Sync for PagePool {}

impl PagePool {
    /// A pool of `len` bytes (a multiple of [`granularity`]), zero filled,
    /// committed now (a sparse file under Wine, free until touched).
    pub fn new(len: usize) -> io::Result<PagePool> {
        PagePool::create(len, 0)
    }

    /// A pool whose pages are reserved only: commit what is mapped with
    /// [`commit`] (a request for 6 GiB at once often cannot be committed).
    pub fn new_lazy(len: usize) -> io::Result<PagePool> {
        PagePool::create(len, SEC_RESERVE)
    }

    fn create(len: usize, kind: u32) -> io::Result<PagePool> {
        // SAFETY: plain section creation.
        let section = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | kind,
                (len as u64 >> 32) as u32,
                len as u32,
                std::ptr::null(),
            )
        };
        if section.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(PagePool {
            section,
            len,
            whole: OnceLock::new(),
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The pool's mapping of all of it, read-write, made the first time and
    /// kept while the pool lives; `None` if the host would not map it.
    pub fn base(&self) -> Option<*mut u8> {
        let at = *self.whole.get_or_init(|| {
            // SAFETY: our section; the view is ours until the pool drops.
            unsafe { MapViewOfFile(self.section, FILE_MAP_WRITE, 0, 0, self.len) }.Value as usize
        });
        (at != 0).then_some(at as *mut u8)
    }

    /// Bytes `[offset, offset + len)`, through [`base`](PagePool::base), or
    /// through a view of their own if the host would not map the whole pool.
    pub fn window(&self, offset: usize, len: usize) -> io::Result<Window> {
        if offset.checked_add(len).is_none_or(|end| end > self.len) {
            return Err(io::Error::other("range outside the page pool"));
        }
        if let Some(base) = self.base() {
            return Ok(Window {
                at: base as usize + offset,
                own: false,
            });
        }
        // SAFETY: bytes of a section we own; the view is ours until dropped.
        let own = unsafe {
            MapViewOfFile(
                self.section,
                FILE_MAP_WRITE,
                (offset as u64 >> 32) as u32,
                offset as u32,
                len,
            )
        };
        if own.Value.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Window {
            at: own.Value as usize,
            own: true,
        })
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

    /// Maps `[offset, offset + len)` at `at`, in place of what is there. The
    /// range must lie in a placeholder (a reservation of the caller's made
    /// as one), which is split around it as needed.
    ///
    /// # Safety
    /// As [`map`](PagePool::map), except that the range is a placeholder.
    pub unsafe fn map_over(
        &self,
        offset: usize,
        len: usize,
        at: usize,
        prot: Protect,
    ) -> io::Result<()> {
        // SAFETY: splits the placeholder around the range (failing
        // harmlessly when the range is the whole placeholder), then maps the
        // section there; both act only on placeholders.
        let view = unsafe {
            VirtualFree(
                at as *mut c_void,
                len,
                MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
            );
            MapViewOfFile3(
                self.section,
                GetCurrentProcess(),
                at as *const c_void,
                offset as u64,
                len,
                MEM_REPLACE_PLACEHOLDER,
                prot.0,
                std::ptr::null_mut(),
                0,
            )
        };
        if view.Value as usize != at {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for PagePool {
    fn drop(&mut self) {
        if let Some(&at) = self.whole.get()
            && at != 0
        {
            // SAFETY: our own view; windows over it hold the pool.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: at as *mut c_void,
                })
            };
        }
        // SAFETY: our own handle; views keep the section alive.
        unsafe { CloseHandle(self.section) };
    }
}

/// Bytes of a pool, writable in place (see [`PagePool::window`]); a view of
/// their own is unmapped on drop.
#[derive(Debug)]
pub struct Window {
    at: usize,
    own: bool,
}

impl Window {
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.at as *mut u8
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if self.own {
            // SAFETY: the view `window` mapped.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.at as *mut c_void,
                })
            };
        }
    }
}

/// Returns a mapping made by [`PagePool::map_over`] to a placeholder: the
/// whole view that starts at `addr` goes, whatever `len` (on macOS it is the
/// length returned to no access).
///
/// # Safety
/// The range must be the caller's, and not used afterwards.
pub unsafe fn unmap_over(addr: *mut u8, _len: usize) -> io::Result<()> {
    // SAFETY: forwarded from the caller.
    let ok = unsafe {
        UnmapViewOfFile2(
            GetCurrentProcess(),
            MEMORY_MAPPED_VIEW_ADDRESS { Value: addr.cast() },
            MEM_PRESERVE_PLACEHOLDER,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
