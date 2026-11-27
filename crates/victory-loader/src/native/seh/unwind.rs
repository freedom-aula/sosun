//! Function tables (`.pdata`) and the x64 unwinder, over images we mapped
//! ourselves: `RtlLookupFunctionEntry` and `RtlVirtualUnwind` with no
//! Windows to ask. Pure computation over the image's bytes and a
//! [`Context`], so the recompiler's arm64 host uses it too.

use std::sync::{Mutex, MutexGuard};

use super::context::{Context, RBP, RSP};

/// `RUNTIME_FUNCTION`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeFunction {
    pub begin: u32,
    pub end: u32,
    pub unwind: u32,
}

pub const UNW_FLAG_EHANDLER: u32 = 1;
pub const UNW_FLAG_UHANDLER: u32 = 2;
const UNW_FLAG_CHAININFO: u32 = 4;

struct Table {
    base: usize,
    /// The whole mapped image (so leaf functions without a table entry
    /// are told from foreign code); 0 for a table added at run time.
    image_len: usize,
    entries: usize,
    count: usize,
}

static TABLES: Mutex<Vec<Table>> = Mutex::new(Vec::new());

fn tables() -> MutexGuard<'static, Vec<Table>> {
    TABLES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Holds the tables' lock, as a thread in the middle of a lookup does (for the dump's test).
#[cfg(test)]
pub(crate) fn hold_tables() -> impl Sized {
    tables()
}

/// Registers a mapped image: `pdata` is its exception directory (RVA and
/// size in bytes).
pub fn add_image(base: usize, len: usize, pdata: Option<(u32, u32)>) {
    let (entries, count) = match pdata {
        Some((rva, size)) if rva != 0 => (base + rva as usize, size as usize / 12),
        _ => (0, 0),
    };
    tables().push(Table {
        base,
        image_len: len,
        entries,
        count,
    });
}

pub fn remove_image(base: usize) {
    tables().retain(|t| !(t.base == base && t.image_len != 0));
}

/// `RtlAddFunctionTable`: `count` sorted entries relative to `base`.
pub fn add_function_table(entries: usize, count: u32, base: usize) {
    tables().push(Table {
        base,
        image_len: 0,
        entries,
        count: count as usize,
    });
}

/// `RtlDeleteFunctionTable`.
pub fn delete_function_table(entries: usize) -> bool {
    let mut tables = tables();
    let before = tables.len();
    tables.retain(|t| !(t.image_len == 0 && t.entries == entries));
    tables.len() != before
}

/// Whether any image has been mapped (so title code exists to hand things to).
pub fn any_image() -> bool {
    tables().iter().any(|t| t.image_len != 0)
}

/// The base of the image holding `pc`, mapped by us (what
/// `RtlPcToFileHeader` answers).
pub fn image_base_of(pc: usize) -> Option<usize> {
    tables()
        .iter()
        .find(|t| t.image_len != 0 && (t.base..t.base + t.image_len).contains(&pc))
        .map(|t| t.base)
}

/// The function table entry covering `pc` and its image base.
pub fn lookup(pc: usize) -> Option<(usize, *const RuntimeFunction)> {
    for t in tables().iter() {
        let in_range = if t.image_len != 0 {
            (t.base..t.base + t.image_len).contains(&pc)
        } else {
            pc >= t.base
        };
        if !in_range || t.count == 0 {
            continue;
        }
        let Ok(rva) = u32::try_from(pc - t.base) else {
            continue;
        };
        let (mut lo, mut hi) = (0, t.count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let entry = (t.entries as *const RuntimeFunction).wrapping_add(mid);
            // SAFETY: inside the table the image or its owner registered.
            let e = unsafe { entry.read_unaligned() };
            if rva < e.begin {
                hi = mid;
            } else if rva >= e.end {
                lo = mid + 1;
            } else {
                return Some((t.base, entry));
            }
        }
    }
    None
}

/// What unwinding one frame found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Unwound {
    /// The language handler of the frame's function, if it has one for
    /// the kind asked (`UNW_FLAG_EHANDLER` or `UNW_FLAG_UHANDLER`).
    pub handler: usize,
    pub handler_data: usize,
    /// The establisher frame: the function's stack frame base.
    pub frame: u64,
}

/// The stack memory the unwinder may read: `low..high`, or anywhere.
pub type Bounds = Option<(usize, usize)>;

fn read(addr: u64, bounds: Bounds) -> Option<u64> {
    let addr = addr as usize;
    if let Some((low, high)) = bounds
        && (addr < low || addr.checked_add(8)? > high)
    {
        return None;
    }
    // SAFETY: inside the stack bounds the caller gave, or a stack it
    // vouches for.
    Some(unsafe { (addr as *const u64).read_unaligned() })
}

fn read128(addr: u64, bounds: Bounds) -> Option<[u8; 16]> {
    let (a, b) = (read(addr, bounds)?, read(addr.checked_add(8)?, bounds)?);
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&a.to_le_bytes());
    out[8..].copy_from_slice(&b.to_le_bytes());
    Some(out)
}

/// `RtlVirtualUnwind`: moves `ctx` from the frame of the function at `entry`
/// (`pc` inside it) to its caller's, and says which handler, if any, the
/// frame has. `None` if the stack or the unwind data cannot be read.
///
/// # Safety
/// `entry` must belong to the image at `base`, which must be mapped.
pub unsafe fn virtual_unwind(
    kind: u32,
    base: usize,
    pc: usize,
    entry: *const RuntimeFunction,
    ctx: &mut Context,
    bounds: Bounds,
) -> Option<Unwound> {
    // SAFETY: forwarded from the caller.
    let first = unsafe { entry.read_unaligned() };
    let mut offset = (pc.wrapping_sub(base) as u64).wrapping_sub(u64::from(first.begin));
    let mut frame = ctx.gpr[RSP];
    // SAFETY: the image's unwind info.
    let prolog = u64::from(unsafe { ((base + first.unwind as usize + 1) as *const u8).read() });
    // Inside an epilogue the frame is already coming apart, and there is
    // no handler for it.
    // SAFETY: `pc` is in the mapped image.
    if offset >= prolog && unsafe { epilogue(pc, ctx, bounds) }? {
        return Some(Unwound {
            frame,
            ..Unwound::default()
        });
    }
    let mut entry = first;
    let mut out = Unwound::default();
    let mut machine_frame = false;
    loop {
        let info = base + entry.unwind as usize;
        // SAFETY: the image's unwind info: a 4-byte header, `count` codes
        // of 2 bytes, then a chained entry or a handler.
        let (version_flags, count, frame_info) = unsafe {
            let p = info as *const u8;
            (p.read(), p.add(2).read() as usize, p.add(3).read())
        };
        if version_flags & 7 == 0 || version_flags & 7 > 2 {
            return None;
        }
        let flags = u32::from(version_flags >> 3);
        let frame_register = usize::from(frame_info & 0xF);
        let frame_offset = u64::from(frame_info >> 4) * 16;
        let code = |i: usize| -> u16 {
            // SAFETY: inside the unwind info (`i < count` below).
            unsafe { ((info + 4 + i * 2) as *const u16).read_unaligned() }
        };
        let mut i = 0;
        while i < count {
            let c = code(i);
            let applies = offset >= u64::from(c & 0xFF);
            let op = (c >> 8) & 0xF;
            let reg = usize::from(c >> 12);
            let rsp = ctx.gpr[RSP];
            let wide = |at: usize| u64::from(code(at)) | u64::from(code(at + 1)) << 16;
            let mut slots = 1;
            match op {
                // UWOP_PUSH_NONVOL
                0 => {
                    if applies {
                        ctx.gpr[reg] = read(rsp, bounds)?;
                        ctx.gpr[RSP] = rsp + 8;
                    }
                }
                // UWOP_ALLOC_LARGE
                1 => {
                    let size = if reg == 0 {
                        slots = 2;
                        u64::from(code(i + 1)) * 8
                    } else {
                        slots = 3;
                        wide(i + 1)
                    };
                    if applies {
                        ctx.gpr[RSP] = rsp.wrapping_add(size);
                    }
                }
                // UWOP_ALLOC_SMALL
                2 => {
                    if applies {
                        ctx.gpr[RSP] = rsp + reg as u64 * 8 + 8;
                    }
                }
                // UWOP_SET_FPREG
                3 => {
                    if applies {
                        ctx.gpr[RSP] = ctx.gpr[frame_register].wrapping_sub(frame_offset);
                        frame = ctx.gpr[RSP];
                    }
                }
                // UWOP_SAVE_NONVOL, UWOP_SAVE_NONVOL_FAR
                4 | 5 => {
                    let at = if op == 4 {
                        slots = 2;
                        u64::from(code(i + 1)) * 8
                    } else {
                        slots = 3;
                        wide(i + 1)
                    };
                    if applies {
                        ctx.gpr[reg] = read(rsp.wrapping_add(at), bounds)?;
                    }
                }
                // UWOP_EPILOG (version 2), UWOP_SPARE_CODE
                6 => slots = 2,
                7 => slots = 3,
                // UWOP_SAVE_XMM128, UWOP_SAVE_XMM128_FAR
                8 | 9 => {
                    let at = if op == 8 {
                        slots = 2;
                        u64::from(code(i + 1)) * 16
                    } else {
                        slots = 3;
                        wide(i + 1)
                    };
                    if applies {
                        *ctx.xmm_mut(reg) = read128(rsp.wrapping_add(at), bounds)?;
                    }
                }
                // UWOP_PUSH_MACHFRAME
                10 => {
                    if applies {
                        let rsp = if reg != 0 { rsp + 8 } else { rsp };
                        ctx.rip = read(rsp, bounds)?;
                        ctx.gpr[RSP] = read(rsp + 24, bounds)?;
                        machine_frame = true;
                    }
                }
                _ => return None,
            }
            i += slots;
        }
        let after = info + 4 + count.next_multiple_of(2) * 2;
        if flags & UNW_FLAG_CHAININFO != 0 {
            // The parent's codes run in full.
            // SAFETY: a chained RUNTIME_FUNCTION follows the codes.
            entry = unsafe { (after as *const RuntimeFunction).read_unaligned() };
            offset = u64::MAX;
            continue;
        }
        if flags & kind != 0 {
            // SAFETY: the handler's RVA follows the codes.
            let rva = unsafe { (after as *const u32).read_unaligned() };
            out.handler = base + rva as usize;
            out.handler_data = after + 4;
        }
        break;
    }
    if !machine_frame {
        let rsp = ctx.gpr[RSP];
        ctx.rip = read(rsp, bounds)?;
        ctx.gpr[RSP] = rsp + 8;
    }
    out.frame = frame;
    Some(out)
}

/// If the code at `pc` is the tail of an epilogue (`add rsp`/`lea rsp`,
/// pops, then `ret` or a jump), moves `ctx` to the caller as executing it
/// would and returns `Some(true)`.
///
/// # Safety
/// `pc` must be in mapped code.
unsafe fn epilogue(pc: usize, ctx: &mut Context, bounds: Bounds) -> Option<bool> {
    enum Step {
        AddRsp(i64),
        LeaRsp(i64),
        Pop(usize),
    }
    let p = pc as *const u8;
    // SAFETY: reads code at `pc`, which the caller says is mapped; the
    // scan stops at the first byte that is not an epilogue's.
    let byte = |i: usize| unsafe { p.add(i).read() };
    let word = |i: usize| {
        i64::from(i32::from_le_bytes([
            byte(i),
            byte(i + 1),
            byte(i + 2),
            byte(i + 3),
        ]))
    };
    let mut steps = Vec::with_capacity(8);
    let mut i = 0;
    let mut tail = false;
    while steps.len() < 12 {
        match byte(i) {
            0x48 if byte(i + 1) == 0x83 && byte(i + 2) == 0xC4 => {
                steps.push(Step::AddRsp(i64::from(byte(i + 3) as i8)));
                i += 4;
            }
            0x48 if byte(i + 1) == 0x81 && byte(i + 2) == 0xC4 => {
                steps.push(Step::AddRsp(word(i + 3)));
                i += 7;
            }
            0x48 if byte(i + 1) == 0x8D && byte(i + 2) == 0x65 => {
                steps.push(Step::LeaRsp(i64::from(byte(i + 3) as i8)));
                i += 4;
            }
            0x48 if byte(i + 1) == 0x8D && byte(i + 2) == 0xA5 => {
                steps.push(Step::LeaRsp(word(i + 3)));
                i += 7;
            }
            r @ 0x58..=0x5F => {
                steps.push(Step::Pop(usize::from(r - 0x58)));
                i += 1;
            }
            0x41 if (0x58..=0x5F).contains(&byte(i + 1)) => {
                steps.push(Step::Pop(8 + usize::from(byte(i + 1) - 0x58)));
                i += 2;
            }
            0xC3 => {
                tail = true;
                break;
            }
            0xF3 if byte(i + 1) == 0xC3 => {
                tail = true;
                break;
            }
            // A jump ends an epilogue only after something was undone (a
            // jump in a function's body is not one).
            0xE9 | 0xEB if !steps.is_empty() => {
                tail = true;
                break;
            }
            0xFF if byte(i + 1) == 0x25 && !steps.is_empty() => {
                tail = true;
                break;
            }
            _ => break,
        }
    }
    if !tail {
        return Some(false);
    }
    for step in steps {
        let rsp = ctx.gpr[RSP];
        match step {
            Step::AddRsp(n) => ctx.gpr[RSP] = rsp.wrapping_add_signed(n),
            Step::LeaRsp(n) => ctx.gpr[RSP] = ctx.gpr[RBP].wrapping_add_signed(n),
            Step::Pop(r) => {
                ctx.gpr[r] = read(rsp, bounds)?;
                if r != RSP {
                    ctx.gpr[RSP] = rsp + 8;
                }
            }
        }
    }
    let rsp = ctx.gpr[RSP];
    ctx.rip = read(rsp, bounds)?;
    ctx.gpr[RSP] = rsp + 8;
    Some(true)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fake image: one function `push rbx; sub rsp, 0x20; ...; add rsp,
    /// 0x20; pop rbx; ret` and its unwind info, laid out as the linker does.
    pub(crate) struct Fake {
        mem: Vec<u8>,
    }

    impl Fake {
        pub(crate) fn new() -> Fake {
            let mut mem = vec![0u8; 0x1000];
            let code = [
                0x53, // 0x100 push rbx
                0x48, 0x83, 0xEC, 0x20, // 0x101 sub rsp, 0x20
                0x90, 0x90, // 0x105 body
                0x48, 0x83, 0xC4, 0x20, // 0x107 add rsp, 0x20
                0x5B, // 0x10B pop rbx
                0xC3, // 0x10C ret
            ];
            mem[0x100..0x100 + code.len()].copy_from_slice(&code);
            // .pdata at 0x200: begin 0x100, end 0x10D, unwind 0x300.
            for (i, v) in [0x100u32, 0x10D, 0x300].iter().enumerate() {
                mem[0x200 + i * 4..0x204 + i * 4].copy_from_slice(&v.to_le_bytes());
            }
            // Unwind info: version 1, prolog 5, 2 codes, no frame register.
            // Codes in reverse order: alloc small 0x20 (info 3) at offset
            // 5, push rbx (reg 3) at offset 1.
            mem[0x300..0x304].copy_from_slice(&[1, 5, 2, 0]);
            mem[0x304..0x306].copy_from_slice(&((3u16 << 12) | (2 << 8) | 5).to_le_bytes());
            mem[0x306..0x308].copy_from_slice(&((3u16 << 12) | 1).to_le_bytes());
            Fake { mem }
        }

        pub(crate) fn base(&self) -> usize {
            self.mem.as_ptr() as usize
        }
    }

    fn unwind_at(fake: &Fake, rip: usize, stack: &mut [u64; 8]) -> Context {
        let mut ctx = Context::zeroed();
        ctx.rip = rip as u64;
        ctx.gpr[RSP] = stack.as_mut_ptr() as u64;
        let entry = (fake.base() + 0x200) as *const RuntimeFunction;
        // SAFETY: the fake image above.
        let r = unsafe { virtual_unwind(0, fake.base(), rip, entry, &mut ctx, None) };
        assert!(r.is_some());
        ctx
    }

    #[test]
    fn unwinds_a_body_a_prologue_and_an_epilogue() {
        let fake = Fake::new();
        // Body: rsp + 0x20 holds the saved rbx, then the return address.
        let mut stack = [0u64; 8];
        stack[4] = 0xB0B;
        stack[5] = 0xDEAD;
        let ctx = unwind_at(&fake, fake.base() + 0x105, &mut stack);
        assert_eq!(ctx.rip, 0xDEAD);
        assert_eq!(ctx.gpr[3], 0xB0B);
        assert_eq!(ctx.gpr[RSP], stack.as_ptr() as u64 + 48);
        // Epilogue after the `add rsp`: `pop rbx; ret` at 0x10B.
        let mut stack = [0u64; 8];
        stack[0] = 0xB0B;
        stack[1] = 0xBEEF;
        let ctx = unwind_at(&fake, fake.base() + 0x10B, &mut stack);
        assert_eq!((ctx.rip, ctx.gpr[3]), (0xBEEF, 0xB0B));
        // Start of the function: nothing pushed yet.
        let mut stack = [0u64; 8];
        stack[0] = 0xCAFE;
        let ctx = unwind_at(&fake, fake.base() + 0x100, &mut stack);
        assert_eq!(ctx.rip, 0xCAFE);
        assert_eq!(ctx.gpr[RSP], stack.as_ptr() as u64 + 8);
    }

    #[test]
    fn looks_entries_up_by_address() {
        let fake = Fake::new();
        add_image(fake.base(), 0x1000, Some((0x200, 12)));
        let (base, entry) = lookup(fake.base() + 0x105).unwrap();
        assert_eq!(base, fake.base());
        // SAFETY: the entry just found in the fake table.
        assert_eq!(unsafe { entry.read_unaligned() }.begin, 0x100);
        assert!(lookup(fake.base() + 0x10D).is_none());
        assert_eq!(image_base_of(fake.base() + 0x10D), Some(fake.base()));
        remove_image(fake.base());
        assert!(lookup(fake.base() + 0x105).is_none());
    }

    #[test]
    fn dynamic_lookup_rejects_pc_four_gib_above_function() {
        let entry = RuntimeFunction {
            begin: 0x100,
            end: 0x110,
            unwind: 0x200,
        };
        let entry_ptr = &raw const entry;
        let base = 0x1000_0000_0000;
        let pc = base + 0x105;
        add_function_table(entry_ptr as usize, 1, base);
        let inside = lookup(pc);
        let four_gib_above = lookup(pc + (1usize << 32));
        assert!(delete_function_table(entry_ptr as usize));

        assert_eq!(inside, Some((base, entry_ptr)));
        assert_eq!(four_gib_above, None);
    }
}
