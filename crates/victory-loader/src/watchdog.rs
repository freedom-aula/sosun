//! The title's deliberate crash write: `mov dword ptr [0], <signature>`, the
//! form its init watchdog suicides in.
//!
//! What tells it from a genuine null-pointer write is that the store's
//! **address is in the instruction itself** — an absolute `disp32` of zero,
//! of the two addressings x86-64 has for that (a SIB byte with no base
//! register, or a `modrm` with `rm = 101`). A compiler never emits a constant
//! absolute address for a data write a bug could reach: a null pointer is
//! addressed *through a register* (`mov [rax], ...` with `rax` zero), and the
//! image's own globals are RIP-relative. Grand Theft Auto VI's watchdog
//! fires the absolute form when its init takes longer than it allows (a first
//! world frame with a cold shader cache, game files on a slow disk): the
//! store of a signature dword (`0x4d8bd421` in the CYBERLEEK v1.0.2.1 build)
//! to address zero, then the crash handler, which faults in its turn — both
//! gone once the store is declined.
//!
//! The native host declines it: the signal handler moves the saved rip past
//! the store, exactly as it emulates the instructions Rosetta cannot run, so
//! the watchdog's thread returns to its caller and the run goes on. What was
//! declined is recorded here lock-free — the handler takes no locks and
//! allocates nothing — for the thread dump to report.

use std::sync::atomic::{AtomicU64, Ordering};

/// A store of an immediate dword to absolute address zero, if `code` starts
/// with one: its length, and the dword it writes (the crash signature). Both
/// addressings of absolute zero: `C7 04 25 00 00 00 00 <imm32>` (a SIB byte
/// with no base) and `C7 05 00 00 00 00 <imm32>` (`modrm 05`).
pub fn crash_write(code: &[u8]) -> Option<(usize, u32)> {
    let imm32 = |at: usize| -> Option<u32> {
        code.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    if code.first() != Some(&0xC7) {
        return None;
    }
    // `C7 04 25 <disp32=0> <imm32>`: modrm 04 (a SIB byte follows), the SIB
    // 25 (no index, no base), the displacement alone is the address.
    if code.get(1..3) == Some(&[0x04, 0x25]) && code.get(3..7) == Some(&[0, 0, 0, 0]) {
        return Some((11, imm32(7)?));
    }
    // `C7 05 <disp32=0> <imm32>`: modrm 05 addresses the displacement directly.
    if code.get(1) == Some(&0x05) && code.get(2..6) == Some(&[0, 0, 0, 0]) {
        return Some((10, imm32(6)?));
    }
    None
}

/// Crash writes declined so far.
pub static DECLINED: AtomicU64 = AtomicU64::new(0);

/// How many sites are kept (a fixed table: the handler allocates nothing).
const SITES: usize = 8;
static SITE_RIP: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static SITE_MAGIC: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static SITE_COUNT: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];

/// Records a declined crash write at `rip` of `magic`, from the signal handler:
/// the total always, and the first sites (a full table still counts the total).
pub fn record(rip: u64, magic: u32) {
    DECLINED.fetch_add(1, Ordering::Relaxed);
    for i in 0..SITES {
        if SITE_RIP[i].load(Ordering::Relaxed) == rip
            || SITE_RIP[i]
                .compare_exchange(0, rip, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            SITE_MAGIC[i].store(u64::from(magic), Ordering::Relaxed);
            SITE_COUNT[i].fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
}

/// The declined crash writes so far, for the thread dump: `(rip, magic, count)`
/// per site, most taken first.
pub fn sites() -> Vec<(u64, u32, u64)> {
    let mut v: Vec<(u64, u32, u64)> = (0..SITES)
        .map(|i| {
            (
                SITE_RIP[i].load(Ordering::Relaxed),
                SITE_MAGIC[i].load(Ordering::Relaxed) as u32,
                SITE_COUNT[i].load(Ordering::Relaxed),
            )
        })
        .filter(|&(rip, _, _)| rip != 0)
        .collect();
    v.sort_by_key(|&(_, _, n)| std::cmp::Reverse(n));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The watchdog of the CYBERLEEK v1.0.2.1 build, as the tester's
    /// disassembly has it (`image+0x23cf683`).
    #[test]
    fn the_watchdog_store_is_recognised() {
        assert_eq!(
            crash_write(&[0xC7, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00, 0x21, 0xD4, 0x8B, 0x4D]),
            Some((11, 0x4D8B_D421))
        );
        // The same store through modrm 05, no SIB byte.
        assert_eq!(
            crash_write(&[0xC7, 0x05, 0x00, 0x00, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44]),
            Some((10, 0x4433_2211))
        );
    }

    /// A genuine null-pointer write addresses through a register; a constant
    /// absolute address is what only a deliberate crash has.
    #[test]
    fn a_null_write_through_a_register_is_left_alone() {
        // mov [rax], imm32 — the register holds the null pointer.
        assert_eq!(crash_write(&[0xC7, 0x00, 0x21, 0xD4, 0x8B, 0x4D]), None);
        // mov [rbp+0], imm32.
        assert_eq!(crash_write(&[0xC7, 0x45, 0x00, 0x21, 0xD4, 0x8B, 0x4D]), None);
        // mov [r8+r9*4], imm32: a SIB byte, but a based one.
        assert_eq!(crash_write(&[0x41, 0xC7, 0x04, 0x08, 0x21, 0xD4, 0x8B, 0x4D]), None);
        // mov qword [rax], imm32.
        assert_eq!(crash_write(&[0x48, 0xC7, 0x00, 0x21, 0xD4, 0x8B, 0x4D]), None);
        // A RIP-relative store of a global.
        assert_eq!(crash_write(&[0xC7, 0x05, 0x10, 0x00, 0x00, 0x00, 0x21, 0xD4, 0x8B, 0x4D]), None);
    }

    /// Only address zero is a crash signature, and only a complete instruction.
    #[test]
    fn other_absolute_addresses_and_short_codes_are_left_alone() {
        // mov dword [0x10], imm32: absolute, but not zero.
        assert_eq!(crash_write(&[0xC7, 0x04, 0x25, 0x10, 0x00, 0x00, 0x00, 0x21, 0xD4, 0x8B, 0x4D]), None);
        assert_eq!(crash_write(&[0xC7, 0x05, 0x01, 0x00, 0x00, 0x00, 0x21, 0xD4, 0x8B, 0x4D]), None);
        // Truncated: no displacement, no immediate.
        assert_eq!(crash_write(&[0xC7, 0x04, 0x25, 0x00, 0x00, 0x00]), None);
        assert_eq!(crash_write(&[0xC7, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00, 0x21]), None);
        assert_eq!(crash_write(&[0xC7, 0x05, 0x00, 0x00, 0x00, 0x00, 0x21]), None);
        assert_eq!(crash_write(&[]), None);
        // Not a store of an immediate at all.
        assert_eq!(crash_write(&[0x89, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00]), None);
    }

    /// The recording the signal handler does, read back for the thread dump.
    #[test]
    fn sites_are_counted_most_taken_first() {
        let (a, b) = (0x1423_cf683, 0x1422_15950);
        record(a, 0x4D8B_D421);
        record(a, 0x4D8B_D421);
        record(b, 0x4D8B_D421);
        let sites = sites();
        assert_eq!(sites.len(), 2, "two distinct sites");
        assert_eq!(sites[0], (a, 0x4D8B_D421, 2), "the twice-taken site first");
        assert_eq!(sites[1], (b, 0x4D8B_D421, 1));
        assert!(DECLINED.load(Ordering::Relaxed) >= 3);
    }
}
