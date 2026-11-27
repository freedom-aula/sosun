//! `VICTORY_NULL_SINK=1`: a last-resort guard for the title writing through a null pointer,
//! and the sink page failed allocations hand out instead of null.
//!
//! The watchdog's deliberate crash write ([`crate::watchdog`]) is declined by default; a
//! *genuine* null-pointer write — a register that holds a low address, `mov [rax], ...` with
//! `rax` zero — is the title's own bug, and its `0xC0000005` ends the run in the crash
//! handler. With the switch set, the signal handler instead **redirects the store into a
//! 64 KiB sink page**: the register is pointed at the sink, the store's displacement
//! recomputed from the fault (it lands at the offset it wanted, `sink + (reg & 0xffff) +
//! (at - reg)`, bounded to the page first), and the instruction runs again, so the value it
//! carried lands somewhere harmless and the title goes on. Later accesses through the same
//! register read the sink — zeros, or what earlier stores put there — instead of faulting in
//! their turn. A store that cannot be kept inside the page (a REP form, which re-runs its
//! count; a displacement that reaches past the end) or one that addresses its destination
//! absolutely (no register to point) is *skipped*, the saved rip moving past it as the
//! emulated instructions are stepped over.
//!
//! The same page serves the allocation audit (`VirtualAlloc` on the POSIX host): an
//! allocation that fails would hand the title null, and the title's first write through it
//! is the crash above — with the switch set, one of at most the sink's size is handed the
//! sink instead (larger failures keep their null: the sink cannot stand for them), and every
//! failure is logged with the size it asked for. Both are counted here lock-free (the signal
//! handler takes no locks and allocates nothing) for the thread dump.
//!
//! This changes what the title sees (a lie where Windows promises failure), so it is off
//! unless asked for: a bring-up aid that keeps the prologue alive long enough to be
//! debugged, not a semantics anyone should ship.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// The bytes of the sink page: 64 KiB, so a `[reg + disp]` store keeps its offset inside it
/// for any displacement the title's structures use, and two allocations that both fell back to
/// the sink do not overlap until their offsets say so.
pub const SINK: usize = 64 * 1024;

/// Whether the switch is on: `VICTORY_NULL_SINK` set.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_NULL_SINK").is_some())
}

/// The sink page, made once for the process and never freed: 64 KiB of zeros.
pub fn sink() -> *mut u8 {
    static PAGE: OnceLock<usize> = OnceLock::new();
    (*PAGE.get_or_init(|| Box::leak(vec![0u8; SINK].into_boxed_slice()).as_mut_ptr() as usize)) as *mut u8
}

/// A store instruction the guard can move: its length, the bytes it writes, whether a REP
/// hint prefixes it (its count re-runs, so it cannot be redirected, only skipped), and the
/// general-purpose register its destination address goes through (`None` for one addressed
/// absolutely — the watchdog's form — or through RIP, which no data store addresses).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Store {
    pub len: usize,
    pub width: usize,
    pub rep: bool,
    pub base: Option<usize>,
}

/// The store instruction `code` starts with, if the guard can move it. Only the plain stores:
/// `88`/`89` (a register's value), `C6`/`C7` (an immediate), `AA`/`AB` (`stos`, through RDI)
/// and `A4`/`A5` (`movs`, through RDI); atomics, `xchg` and the rest are left to the crash
/// they cause (their side effects are not a single store to fake).
pub fn store(code: &[u8]) -> Option<Store> {
    // The prefixes skip: operand size, the segment overrides, lock and the REP hints. Which of
    // them appeared is decided at the opcode, not here: REX.W outranks the operand-size prefix.
    let mut i = 0;
    let mut rep = false;
    while let Some(&b) = code.get(i) {
        match b {
            0xF2 | 0xF3 => {
                rep = true;
                i += 1;
            }
            0x66 | 0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 | 0xF0 => i += 1,
            _ => break,
        }
    }
    // The REX prefix, if any (its W bit makes the store 8 bytes; B extends the register the
    // modrm's rm field or the SIB's base names).
    let rex = match code.get(i) {
        Some(&b @ 0x40..=0x4F) => {
            i += 1;
            Some(b)
        }
        _ => None,
    };
    let operand = |w: usize| {
        if rex.is_some_and(|r| r & 8 != 0) {
            8
        } else if code[..i].contains(&0x66) {
            2
        } else {
            w
        }
    };
    let (w, rm_at) = match code.get(i)? {
        // mov rm8, reg8 / mov rm, reg
        0x88 => (1, i + 1),
        0x89 => (4, i + 1),
        // mov rm8, imm8 / mov rm, imm32
        0xC6 => (1, i + 1),
        0xC7 => (4, i + 1),
        // stos and movs: the destination is [rdi] (never RIP-relative, never absolute).
        0xAA | 0xA4 => (1, usize::MAX),
        0xAB | 0xA5 => (4, usize::MAX),
        _ => return None,
    };
    let w = operand(w);
    if rm_at == usize::MAX {
        // RDI, register 7 (no REX.B on these one-byte opcodes).
        return Some(Store { len: i + 1, width: w, rep, base: Some(7) });
    }
    let modrm = *code.get(rm_at)?;
    let (md, rm) = (modrm >> 6, modrm & 7);
    if md == 3 {
        return None; // register to register: no memory access at all.
    }
    let b = usize::from(rex.is_some_and(|r| r & 1 != 0));
    // A SIB byte follows rm = 100; its base of 101 with md = 00 addresses absolutely, but that
    // is the watchdog's form and is not reached here (no register to point at the sink).
    let (len, base) = if rm == 4 {
        let sib = *code.get(rm_at + 1)?;
        let base = (sib & 7) as usize;
        if md == 0 && base == 5 {
            return None; // no base register: an absolute disp32.
        }
        let len = 1 + match md {
            0 => 0,
            1 => 1,
            _ => 4,
        };
        (len, base + 8 * b)
    } else if md == 0 && rm == 5 {
        return None; // disp32 alone: absolute, or RIP-relative.
    } else {
        let len = match md {
            0 => 0,
            1 => 1,
            _ => 4,
        };
        (len, rm as usize + 8 * b)
    };
    // The immediate of C6/C7 follows the modrm and displacement; the displacement is counted.
    let imm = if matches!(code.get(i), Some(&0xC6) | Some(&0xC7)) { w } else { 0 };
    let end = rm_at + 1 + len + imm;
    // The whole instruction must be in `code` (the handler reads fifteen bytes; a shorter
    // slice is a truncated read, not a store to move).
    code.get(..end)?;
    Some(Store { len: end, width: w, rep, base: Some(base) })
}

/// Stores moved so far.
pub static MOVED: AtomicU64 = AtomicU64::new(0);

/// How many sites are kept (a fixed table: the signal handler allocates nothing).
const SITES: usize = 8;
static SITE_RIP: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static SITE_COUNT: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
/// The first eight bytes of the faulting instruction (for the dump).
static SITE_CODE: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
/// The address the store went to, and `len | (slot + 1) << 8` (0: no register).
static SITE_AT: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static SITE_META: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
/// The base register's value at the fault.
static SITE_VAL: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];

/// Records a store the guard moved at `rip`: the total always, and the first sites with the
/// instruction's first bytes (`code`, at most eight are kept), the address, the register and
/// its value (a full table still counts the total). From the signal handler: no locks, no
/// allocation.
pub fn record(rip: u64, at: u64, s: Store, value: u64, code: &[u8]) {
    MOVED.fetch_add(1, Ordering::Relaxed);
    let mut bytes = 0u64;
    for (i, &b) in code.iter().take(8).enumerate() {
        bytes |= u64::from(b) << (8 * i);
    }
    let meta = s.len as u64 | (s.base.map_or(0, |slot| slot + 1) as u64) << 8;
    for i in 0..SITES {
        if SITE_RIP[i].load(Ordering::Relaxed) == rip
            || SITE_RIP[i]
                .compare_exchange(0, rip, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            SITE_CODE[i].store(bytes, Ordering::Relaxed);
            SITE_AT[i].store(at, Ordering::Relaxed);
            SITE_META[i].store(meta, Ordering::Relaxed);
            SITE_VAL[i].store(value, Ordering::Relaxed);
            SITE_COUNT[i].fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
}

/// One moved-store site, for the thread dump.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Site {
    pub rip: u64,
    pub count: u64,
    /// The first eight bytes of the instruction.
    pub code: u64,
    /// The address the store went to.
    pub at: u64,
    /// Its length, and the register it went through with that register's value.
    pub len: usize,
    pub base: Option<(usize, u64)>,
}

/// The moved stores so far, most taken first.
pub fn sites() -> Vec<Site> {
    let mut v: Vec<Site> = (0..SITES)
        .filter(|&i| SITE_RIP[i].load(Ordering::Relaxed) != 0)
        .map(|i| {
            let meta = SITE_META[i].load(Ordering::Relaxed);
            let slot = (meta >> 8) as usize;
            Site {
                rip: SITE_RIP[i].load(Ordering::Relaxed),
                count: SITE_COUNT[i].load(Ordering::Relaxed),
                code: SITE_CODE[i].load(Ordering::Relaxed),
                at: SITE_AT[i].load(Ordering::Relaxed),
                len: (meta & 0xff) as usize,
                base: (slot > 0).then(|| (slot - 1, SITE_VAL[i].load(Ordering::Relaxed))),
            }
        })
        .collect();
    v.sort_by_key(|s| std::cmp::Reverse(s.count));
    v
}

/// Failed allocations handed the sink so far (the audit).
pub static SUNK: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    /// The plain register stores, every width, with and without prefixes.
    #[test]
    fn register_stores_decode_with_their_width_and_base() {
        // mov [rax], ecx: opcode 89, modrm 08 (md 0, rm 000): 2 bytes, 4 wide, rax.
        assert_eq!(store(&[0x89, 0x08, 0x90]), Some(Store { len: 2, width: 4, rep: false, base: Some(0) }));
        // mov byte [rbx], dl: 88 /r with md 0, rm 011.
        assert_eq!(store(&[0x88, 0x13]), Some(Store { len: 2, width: 1, rep: false, base: Some(3) }));
        // mov qword [rcx], r8: REX.W and REX.R (4C) — the source is r8, the base stays rcx.
        assert_eq!(store(&[0x4C, 0x89, 0x01]), Some(Store { len: 3, width: 8, rep: false, base: Some(1) }));
        // mov word [rdx], bx: the operand-size prefix (66) makes it 2 bytes wide.
        assert_eq!(store(&[0x66, 0x89, 0x0A]), Some(Store { len: 3, width: 2, rep: false, base: Some(2) }));
        // REX.W outranks the operand-size prefix: still 8.
        assert_eq!(store(&[0x66, 0x48, 0x89, 0x0A]), Some(Store { len: 4, width: 8, rep: false, base: Some(2) }));
        // mov [r8], eax: REX.B (41) extends the base to r8 (slot 8).
        assert_eq!(store(&[0x41, 0x89, 0x00]), Some(Store { len: 3, width: 4, rep: false, base: Some(8) }));
        // mov [r9+r10*4], edi: REX.X and REX.B (45), modrm 0C (SIB), SIB 91 (scale 1, index
        // r10's low three, base r9's).
        assert_eq!(store(&[0x45, 0x89, 0x0C, 0x91]), Some(Store { len: 4, width: 4, rep: false, base: Some(9) }));
        // mov dword [rsi+0x10], eax: md 1, a one-byte displacement.
        assert_eq!(store(&[0x89, 0x46, 0x10]), Some(Store { len: 3, width: 4, rep: false, base: Some(6) }));
        // mov qword [rbp-0x20], rax: md 2, a four-byte displacement.
        assert_eq!(store(&[0x48, 0x89, 0x45, 0xE0]), Some(Store { len: 4, width: 8, rep: false, base: Some(5) }));
        // stosb/stosd: [rdi], one and four bytes, rdi.
        assert_eq!(store(&[0xAA]), Some(Store { len: 1, width: 1, rep: false, base: Some(7) }));
        assert_eq!(store(&[0xAB]), Some(Store { len: 1, width: 4, rep: false, base: Some(7) }));
        // rep stosq: the REP hint skips and marks, REX.W widens.
        assert_eq!(store(&[0xF3, 0x48, 0xAB]), Some(Store { len: 3, width: 8, rep: true, base: Some(7) }));
        // movsb/movsd: [rdi] too.
        assert_eq!(store(&[0xA4]), Some(Store { len: 1, width: 1, rep: false, base: Some(7) }));
        assert_eq!(store(&[0xA5]), Some(Store { len: 1, width: 4, rep: false, base: Some(7) }));
        // An immediate store: mov dword [rbx+8], 0x4d8bd421 — C7 43 08 imm32, 7 bytes.
        assert_eq!(
            store(&[0xC7, 0x43, 0x08, 0x21, 0xD4, 0x8B, 0x4D]),
            Some(Store { len: 7, width: 4, rep: false, base: Some(3) })
        );
        // mov byte [r12], 0xFF: C6 with a SIB byte (rm 100), whose base 100 REX.B extends to
        // r12; the index 100 (rsp) is no index at all.
        assert_eq!(store(&[0x41, 0xC6, 0x04, 0x24, 0xFF]), Some(Store { len: 5, width: 1, rep: false, base: Some(12) }));
    }

    /// What the guard is not for: absolute and RIP-relative stores (the watchdog's form has
    /// no register to point at the sink; a RIP-relative one is a global, never null), loads,
    /// and the instructions whose side effects are more than one store.
    #[test]
    fn what_the_guard_leaves_alone() {
        // The watchdog's own store: absolute zero, no register.
        assert_eq!(store(&[0xC7, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00, 0x21, 0xD4, 0x8B, 0x4D]), None);
        assert_eq!(store(&[0xC7, 0x05, 0x00, 0x00, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44]), None);
        // A RIP-relative store of a global.
        assert_eq!(store(&[0xC7, 0x05, 0x10, 0x00, 0x00, 0x00, 0x21, 0xD4, 0x8B, 0x4D]), None);
        // A load, not a store.
        assert_eq!(store(&[0x8B, 0x00]), None);
        // Register to register: no memory access.
        assert_eq!(store(&[0x89, 0xC1]), None);
        // An atomic exchange: its side effect is the exchange, not a plain store.
        assert_eq!(store(&[0x87, 0x00]), None);
        // Truncated.
        assert_eq!(store(&[0x89]), None);
        assert_eq!(store(&[0xC7, 0x43]), None);
        assert_eq!(store(&[]), None);
    }

    /// The recording the signal handler does, read back for the thread dump.
    #[test]
    fn sites_are_counted_most_taken_first() {
        let (a, b) = (0x1423_cf000, 0x1422_10000);
        let s = Store { len: 3, width: 4, rep: false, base: Some(0) };
        record(a, 0, s, 0, &[0x89, 0x08, 0x90]);
        record(a, 0, s, 0, &[0x89, 0x08, 0x90]);
        record(b, 0x20, Store { len: 7, width: 4, rep: false, base: None }, 0, &[0xC7, 0x43, 0x08]);
        let sites = sites();
        assert_eq!(sites.len(), 2, "two distinct sites");
        assert_eq!(sites[0].rip, a);
        assert_eq!(sites[0].count, 2, "the twice-taken site first");
        assert_eq!(sites[0].base, Some((0, 0)));
        assert_eq!(sites[0].len, 3);
        assert_eq!(sites[0].code, 0x9008_89, "the instruction's first bytes");
        assert_eq!(sites[1].base, None, "the absolute store has no register");
        assert_eq!(sites[1].len, 7);
        assert!(MOVED.load(Ordering::Relaxed) >= 3);
    }

    /// The sink page is 64 KiB of zeros, the same page every call.
    #[test]
    fn the_sink_is_one_zeroed_page() {
        let p = sink();
        // SAFETY: the page is ours, 64 KiB of it.
        let bytes = unsafe { std::slice::from_raw_parts(p, SINK) };
        assert!(bytes.iter().all(|&b| b == 0));
        assert_eq!(sink(), p, "made once");
    }
}
