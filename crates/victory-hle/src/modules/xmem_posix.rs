//! The macOS side of `xmem`: `XMemVirtualAlloc`, `XMemVirtualQuery` and the
//! export lookup, on `victory-platform` (Mach VM) in place of the Win32 calls
//! `xmem.rs` makes on Windows. Everything else of `xmem` is shared.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::BTreeMap;
use std::ffi::{CStr, c_void};
use std::sync::{Mutex, MutexGuard};

use victory_platform::mem::{self, Protect};

use super::xmem::{GRAPHICS, HOST_PROTECTION, PAGES_2M, PAGES_64K, PAGES_64K_BIT, address_range};

#[allow(clippy::upper_case_acronyms)]
pub(super) type BOOL = i32;

// The Win32 values titles pass (`windows-sys` does not exist here).
pub(super) const MEM_COMMIT: u32 = 0x1000;
pub(super) const MEM_RESERVE: u32 = 0x2000;
const MEM_REPLACE_PLACEHOLDER: u32 = 0x4000;
const MEM_FREE: u32 = 0x1_0000;
const MEM_PRIVATE: u32 = 0x2_0000;
pub(super) const PAGE_READWRITE: u32 = 0x04;
const PAGE_NOACCESS: u32 = 0x01;

/// Where `VirtualAlloc2` places and sizes reservations on Windows: 64 KiB.
const GRANULE: usize = 64 << 10;
const MIB: usize = 1 << 20;

/// Reservations `XMemVirtualAlloc` made, by base: a free needs the length.
static RESERVATIONS: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());

fn reservations() -> MutexGuard<'static, BTreeMap<usize, usize>> {
    RESERVATIONS.lock().unwrap_or_else(|e| e.into_inner())
}

fn track(base: usize, len: usize) {
    reservations().insert(base, len);
    #[cfg(target_arch = "x86_64")]
    crate::host_posix::adopt_region(base, len);
}

/// Gives back a reservation `XMemVirtualAlloc` made (the title's
/// `VirtualFree(MEM_RELEASE)` does the same through `host_posix`).
pub(super) fn release_reservation(base: usize) {
    let Some(len) = reservations().remove(&base) else {
        return;
    };
    #[cfg(target_arch = "x86_64")]
    crate::host_posix::forget_region(base);
    // SAFETY: a reservation we made and no longer track.
    if let Err(error) = unsafe { mem::release(base as *mut u8, len) } {
        tracing::warn!(base, len, %error, "releasing an xmem reservation failed");
    }
}

/// Win32 `PAGE_*` to a protection (copy-on-write is plain write here: no
/// section is behind these pages).
fn protection(flags: u32) -> Option<Protect> {
    Some(match flags & 0xFF {
        0 | 0x01 => Protect::NO_ACCESS,
        0x02 => Protect::READ,
        0x04 | 0x08 => Protect::READ_WRITE,
        0x10 | 0x20 => Protect::READ_EXECUTE,
        0x40 | 0x80 => Protect::READ_WRITE_EXECUTE,
        _ => return None,
    })
}

fn page_flags(p: Protect) -> u32 {
    match p {
        Protect::NO_ACCESS => PAGE_NOACCESS,
        Protect::READ => 0x02,
        Protect::READ_WRITE => PAGE_READWRITE,
        Protect::READ_EXECUTE => 0x20,
        _ => 0x40,
    }
}

pub extern "system" fn XMemVirtualAlloc(
    base: *mut c_void,
    size: usize,
    kind: u32,
    flags: u64,
    protect: u32,
) -> *mut c_void {
    let Some(mut prot) = protection(protect & HOST_PROTECTION).filter(|_| size > 0) else {
        return std::ptr::null_mut();
    };
    // A reservation of 64 KiB pages takes physical pages later
    // (`XMemMapPhysicalPages`): here it is plain reserved address space, which
    // the pages are mapped over, so a placeholder is a reservation.
    let placeholder = kind & PAGES_64K == PAGES_64K && kind & MEM_COMMIT == 0;
    if placeholder {
        if kind & MEM_REPLACE_PLACEHOLDER != 0 {
            // Part of a reservation the title split: it stays one.
            return base;
        }
        prot = Protect::NO_ACCESS;
    }
    let commit = kind & MEM_COMMIT != 0;
    let reserve = kind & MEM_RESERVE != 0 || placeholder || base.is_null();
    let len = size.next_multiple_of(GRANULE);
    let at = base as usize;
    let result = if reserve {
        // SAFETY: fresh address space, or exactly the range asked for (which
        // fails if taken).
        let reserved = unsafe {
            if base.is_null() {
                let (lowest, highest) = address_range(flags);
                let align = if kind & (PAGES_2M | PAGES_64K_BIT) == PAGES_2M {
                    2 * MIB
                } else {
                    GRANULE
                };
                mem::reserve_in(lowest, highest, len, align)
            } else {
                mem::reserve(Some(at), len)
            }
        };
        reserved.and_then(|p| {
            if commit {
                // SAFETY: the reservation just made.
                if let Err(e) = unsafe { mem::commit(p, len, prot) } {
                    // SAFETY: as above.
                    unsafe { mem::release(p, len) }?;
                    return Err(e);
                }
            }
            track(p as usize, len);
            Ok(p)
        })
    } else if commit {
        let page = mem::page_size();
        let start = at & !(page - 1);
        // SAFETY: the title commits inside a reservation of its own.
        unsafe {
            mem::commit(
                start as *mut u8,
                (at + size).next_multiple_of(page) - start,
                prot,
            )
        }
        .map(|()| at as *mut u8)
    } else {
        return std::ptr::null_mut();
    };
    match result {
        Ok(p) => {
            if base.is_null() {
                tracing::debug!(
                    size,
                    kind = format_args!("{kind:#x}"),
                    flags = format_args!("{flags:#x}"),
                    at = format_args!("{p:p}"),
                    "XMemVirtualAlloc"
                );
            }
            p.cast()
        }
        Err(error) => {
            tracing::warn!(
                size,
                kind = format_args!("{kind:#x}"),
                flags = format_args!("{flags:#x}"),
                %error,
                "XMemVirtualAlloc refused"
            );
            std::ptr::null_mut()
        }
    }
}

/// `MEMORY_BASIC_INFORMATION` (the console's first 0x30 bytes are the same),
/// then the XMem flags and tag.
#[repr(C)]
pub struct BasicInformation {
    base_address: usize,
    allocation_base: usize,
    allocation_protect: u32,
    partition_id: u16,
    region_size: usize,
    state: u32,
    protect: u32,
    kind: u32,
    flags: u64,
    tag_code: u32,
    tag: [u8; 4],
}

// shortcut: every committed page reports graphics memory and no tag, since
// all of it is GPU-reachable here (as on Windows); a reserved page and a
// committed one without access cannot be told apart (both report reserved),
// and every page is `MEM_PRIVATE` (a pool's views are mapped).
pub extern "system" fn XMemVirtualQuery(
    _process: *mut c_void,
    address: *const c_void,
    out: *mut BasicInformation,
) -> BOOL {
    if out.is_null() {
        return 0;
    }
    let page = mem::page_size();
    let at = address as usize & !(page - 1);
    // The reservation `XMemVirtualAlloc` made around the address, if it did.
    let allocation = reservations()
        .range(..=at)
        .next_back()
        .map(|(&base, &len)| (base, base + len))
        .filter(|&(_, end)| at < end);
    let mut info = BasicInformation {
        base_address: at,
        allocation_base: 0,
        allocation_protect: 0,
        partition_id: 0,
        region_size: 0,
        state: MEM_FREE,
        protect: PAGE_NOACCESS,
        kind: 0,
        flags: 0,
        tag_code: 0,
        tag: [0; 4],
    };
    match mem::next_region(at) {
        Some(r) if r.base <= at => {
            let end = allocation.map_or(r.base + r.len, |(_, end)| end.min(r.base + r.len));
            info.region_size = end - at;
            info.allocation_base = allocation.map_or(r.base, |(base, _)| base);
            info.kind = MEM_PRIVATE;
            info.allocation_protect = page_flags(r.protect);
            if r.protect == Protect::NO_ACCESS {
                info.state = MEM_RESERVE;
                info.protect = 0;
            } else {
                info.state = MEM_COMMIT;
                info.protect = page_flags(r.protect);
                info.flags = GRAPHICS;
            }
        }
        // Free: up to the next mapping, or the top of user space.
        next => {
            info.region_size = next.map_or(1 << 47, |r| r.base).saturating_sub(at);
        }
    }
    // SAFETY: the title passes room for the whole structure.
    unsafe { out.write(info) };
    1
}

/// Address of the export `name` of the PE image mapped at `base`, as
/// `GetProcAddress` gives it, or `None` (also for a forwarded export).
///
/// # Safety
/// `base` must be a mapped PE32+ image.
pub(super) unsafe fn find_export(base: usize, name: &CStr) -> Option<usize> {
    // SAFETY: reads inside the mapped headers and the export directory they name.
    unsafe {
        let u32_at = |at: usize| ((base + at) as *const u32).read_unaligned();
        let nt = u32_at(0x3c) as usize;
        // Data directory 0 of the PE32+ optional header.
        let (dir, dir_len) = (u32_at(nt + 0x88) as usize, u32_at(nt + 0x8c) as usize);
        if dir == 0 {
            return None;
        }
        let (names, functions, ordinals) = (
            u32_at(dir + 0x20) as usize,
            u32_at(dir + 0x1c) as usize,
            u32_at(dir + 0x24) as usize,
        );
        (0..u32_at(dir + 0x18) as usize).find_map(|i| {
            let at = u32_at(names + 4 * i) as usize;
            (CStr::from_ptr((base + at) as *const _) == name).then(|| {
                let ordinal = ((base + ordinals + 2 * i) as *const u16).read_unaligned();
                let rva = u32_at(functions + 4 * ordinal as usize) as usize;
                // An address inside the directory is a forwarder string.
                (rva < dir || rva >= dir + dir_len).then_some(base + rva)
            })?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(address: *const c_void) -> BasicInformation {
        // SAFETY: plain data; zero is a valid value.
        let mut info: BasicInformation = unsafe { std::mem::zeroed() };
        assert_eq!(
            XMemVirtualQuery(std::ptr::null_mut(), address, &mut info),
            1
        );
        info
    }

    #[test]
    fn layouts_match_the_console() {
        assert_eq!(size_of::<BasicInformation>(), 0x40);
        assert_eq!(std::mem::offset_of!(BasicInformation, region_size), 0x18);
        assert_eq!(std::mem::offset_of!(BasicInformation, state), 0x20);
        assert_eq!(std::mem::offset_of!(BasicInformation, flags), 0x30);
    }

    #[test]
    fn virtual_alloc_meets_console_constraints() {
        const GPU_READ_WRITE: u32 = 0x2000;
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * MIB,
            PAGES_2M | MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | 7 << 12, // 40-bit addresses
            PAGE_READWRITE | GPU_READ_WRITE,
        );
        assert!(!p.is_null());
        assert_eq!(p as usize % (2 * MIB), 0);
        assert!((p as usize) < 1 << 40);
        let info = query(p);
        assert_eq!(info.state, MEM_COMMIT);
        assert_eq!(info.protect, PAGE_READWRITE);
        assert_eq!(info.flags & GRAPHICS, GRAPHICS);
        assert_eq!(
            (info.allocation_base, info.region_size),
            (p as usize, 4 * MIB)
        );
        // The pages are real.
        // SAFETY: 4 MiB were committed.
        unsafe { p.cast::<u8>().add(4 * MIB - 1).write(7) };
        // A query inside reports the rest of the region.
        let inside = query(p.cast::<u8>().wrapping_add(MIB + 5).cast());
        assert_eq!(inside.region_size, 3 * MIB);
        release_reservation(p as usize);
        let freed = query(p);
        assert_eq!((freed.state, freed.flags), (MEM_FREE, 0));
        // No address field: above the 32-bit range and below 2^40, where the
        // console keeps it.
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            16 << 30,
            MEM_RESERVE,
            GRAPHICS,
            PAGE_NOACCESS,
        );
        assert!(!p.is_null() && (p as usize) >= 1 << 32, "{p:p}");
        assert!((p as usize) + (16 << 30) <= 1 << 40, "{p:p}");
        let info = query(p);
        assert_eq!((info.state, info.flags), (MEM_RESERVE, 0));
        release_reservation(p as usize);
    }

    #[test]
    fn a_placeholder_reservation_commits_in_parts() {
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            8 * GRANULE,
            PAGES_64K, // a placeholder for physical pages
            GRAPHICS,
            PAGE_NOACCESS,
        );
        assert!(!p.is_null());
        assert_eq!(query(p).state, MEM_RESERVE);
        // Reserving the same place again fails; splitting is a no-op.
        let again = XMemVirtualAlloc(p, GRANULE, MEM_RESERVE, GRAPHICS, PAGE_NOACCESS);
        assert!(again.is_null());
        let split = XMemVirtualAlloc(p, GRANULE, PAGES_64K | MEM_REPLACE_PLACEHOLDER, 0, 0);
        assert_eq!(split, p);
        // Commit the third granule only.
        let at = p.cast::<u8>().wrapping_add(2 * GRANULE).cast();
        let committed = XMemVirtualAlloc(at, GRANULE, MEM_COMMIT, 0, PAGE_READWRITE);
        assert_eq!(committed, at);
        let info = query(at);
        assert_eq!((info.state, info.region_size), (MEM_COMMIT, GRANULE));
        assert_eq!(info.allocation_base, p as usize);
        assert_eq!(query(p).state, MEM_RESERVE);
        release_reservation(p as usize);
    }

    #[test]
    fn bad_arguments_give_null() {
        let null = std::ptr::null_mut();
        assert!(XMemVirtualAlloc(null, 0, MEM_RESERVE, 0, PAGE_READWRITE).is_null());
        assert!(XMemVirtualAlloc(null, GRANULE, MEM_RESERVE, 0, 0x03).is_null());
        assert_eq!(XMemVirtualQuery(null, null, std::ptr::null_mut()), 0);
    }

    #[test]
    fn finds_a_named_export_in_a_mapped_image() {
        // A hand-made image: headers and an export directory of two names.
        fn put32(image: &mut [u8], at: usize, v: u32) {
            image[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        let mut image = vec![0u8; 0x400];
        put32(&mut image, 0x3c, 0x80);
        put32(&mut image, 0x80 + 0x88, 0x100); // export directory RVA
        put32(&mut image, 0x80 + 0x8c, 0x100); // and size
        put32(&mut image, 0x100 + 0x18, 2); // names
        put32(&mut image, 0x100 + 0x1c, 0x140); // functions
        put32(&mut image, 0x100 + 0x20, 0x160); // name pointers
        put32(&mut image, 0x100 + 0x24, 0x180); // ordinals
        put32(&mut image, 0x160, 0x200);
        put32(&mut image, 0x164, 0x220);
        image[0x200..0x205].copy_from_slice(b"alpha");
        image[0x220..0x224].copy_from_slice(b"beta");
        // Ordinals: alpha is function 1, beta is function 0.
        image[0x180..0x184].copy_from_slice(&[1, 0, 0, 0]);
        put32(&mut image, 0x140, 0x300);
        put32(&mut image, 0x144, 0x310);
        let base = image.as_ptr() as usize;
        // SAFETY: the vector is the image.
        unsafe {
            assert_eq!(find_export(base, c"alpha"), Some(base + 0x310));
            assert_eq!(find_export(base, c"beta"), Some(base + 0x300));
            assert_eq!(find_export(base, c"gamma"), None);
        }
    }
}
