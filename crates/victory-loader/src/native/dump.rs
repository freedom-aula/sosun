//! `VICTORY_DUMP_THREADS=<seconds>`: after that long, log what every title
//! thread is doing: in a host call (which import, from where in the title)
//! or in title code, with a call chain from the unwind tables. A debugging
//! aid for a title that stops making progress.

use std::time::Duration;

use super::bridge::traced_name;
use super::seh::context::{Context, RBP, RSP};
use super::seh::unwind;
use super::teb::{
    OFF_IN_HOST, OFF_IN_HOST_CTX, OFF_IN_HOST_RBP, OFF_IN_HOST_RET, OFF_STACK_BASE, OFF_STACK_LIMIT,
};

fn seconds() -> Option<u64> {
    static SECS: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *SECS.get_or_init(|| std::env::var("VICTORY_DUMP_THREADS").ok()?.parse().ok())
}

/// Whether the dump is asked for (imports then carry their names).
pub fn enabled() -> bool {
    seconds().is_some()
}

/// `VICTORY_WATCH_MEM=<hex address>`: every second, the qwords of the 4 KiB around the address
/// that changed since the last look are logged (data only; for finding what writes a flag).
pub fn start_mem_watch() {
    let Some(at) = std::env::var("VICTORY_WATCH_MEM")
        .ok()
        .and_then(|v| usize::from_str_radix(v.trim_start_matches("0x"), 16).ok())
    else {
        return;
    };
    std::thread::spawn(move || {
        let base = (at & !0xfff) as usize;
        let mut last: Vec<Option<usize>> = vec![None; 512];
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let mut lines = Vec::new();
            for (i, old) in last.iter_mut().enumerate() {
                let now = read_checked(base + i * 8);
                if *old != now && old.is_some() {
                    lines.push(format!("{:#x}: {:x?} -> {:x?}", base + i * 8, old, now));
                }
                *old = now;
            }
            if !lines.is_empty() {
                tracing::warn!("mem watch: {} changed: {}", lines.len(), lines[..lines.len().min(24)].join("; "));
            }
        }
    });
}

/// Starts the dumping thread if `VICTORY_DUMP_THREADS` is set.
pub fn start() {
    let Some(secs) = seconds() else { return };
    std::thread::spawn(move || {
        // 0: only at the title's first breakpoint (`report_at_break`).
        if secs == 0 {
            return;
        }
        std::thread::sleep(Duration::from_secs(secs));
        report();
    });
}

/// With `VICTORY_DUMP_THREADS=0`, the dump is taken when the title first hits a breakpoint
/// (its asserts) or an access violation (its fatal-error path), from the thread that hit it.
pub fn report_at_break() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    if seconds() == Some(0) {
        ONCE.call_once(report);
    }
}

fn report() {
    {
        tracing::warn!(
            "clzero emulations so far: {}",
            super::seh::signal::EMULATED.load(std::sync::atomic::Ordering::Relaxed)
        );
        if let Some((at, n)) = std::env::var("VICTORY_DUMP_MEM").ok().and_then(|v| {
            let (a, n) = v.split_once(',')?;
            Some((usize::from_str_radix(a.trim_start_matches("0x"), 16).ok()?, n.parse::<usize>().ok()?))
        }) {
            let words: Vec<String> = (0..n)
                .map(|i| read_checked(at + 8 * i).map_or("-".into(), |x| format!("{x:x}")))
                .collect();
            tracing::warn!("memory at {at:#x}: {}", words.join(" "));
        }
        if let Ok(list) = std::env::var("VICTORY_FIND_QWORD") {
            // `@<hex>`: the qword at that address (an object's method table pointer, say).
            let values: Vec<u64> = list
                .split(',')
                .filter_map(|v| match v.strip_prefix('@') {
                    Some(at) => read_checked(usize::from_str_radix(at, 16).ok()?).map(|x| x as u64),
                    None => u64::from_str_radix(v.trim_start_matches("0x"), 16).ok(),
                })
                .collect();
            find_qwords(&values);
        }
        super::bridge::report_callbacks();
        for line in collect() {
            tracing::warn!("{line}");
        }
    }
}

/// `VICTORY_FIND_QWORD=<hex>,<hex>,...` (with `VICTORY_DUMP_THREADS`): every aligned qword of
/// resident readable memory equal to one of the values is logged with the 0x100 bytes from it
/// (for finding heap objects by their method table, data only). `@<hex>` stands for the qword
/// at that address.
fn find_qwords(values: &[u64]) {
    let mut at = 0x1_0000usize;
    let mut found = 0;
    let mut chunk = vec![0u64; 0x2_0000];
    while let Some(r) = victory_platform::mem::next_region(at) {
        // A region that grows while we look is seen again from where we stopped.
        let (start, end) = (r.base.max(at), r.base + r.len);
        at = end;
        if r.protect.0 & 1 == 0 || found >= 64 {
            continue;
        }
        let mut p = start;
        while p < end && found < 64 {
            let n = (end - p).min(chunk.len() * 8);
            // Pages never touched would take minutes to read in huge reservations.
            if resident(p, n) && read_into(p, &mut chunk[..n / 8]) {
                for (i, &w) in chunk[..n / 8].iter().enumerate() {
                    // Not our own buffer or locals.
                    let own = chunk.as_ptr_range();
                    let near_stack = (p + 8 * i).abs_diff(&raw const found as usize) < 0x1_0000;
                    if values.contains(&w)
                        && !own.contains(&((p + 8 * i) as *const u64))
                        && !near_stack
                    {
                        let hit = p + 8 * i;
                        let words: Vec<String> = (0..32)
                            .map(|k| {
                                read_checked(hit + 8 * k).map_or("-".into(), |x| format!("{x:x}"))
                            })
                            .collect();
                        tracing::warn!("qword {w:#x} at {hit:#x}: {}", words.join(" "));
                        found += 1;
                    }
                }
            }
            p += n;
        }
    }
    tracing::warn!("qwords {values:x?}: {found} found");
}

/// Whether any page of the range is in memory.
fn resident(at: usize, len: usize) -> bool {
    unsafe extern "C" {
        fn mincore(addr: *const std::ffi::c_void, len: usize, vec: *mut u8) -> i32;
    }
    let mut pages = vec![0u8; len.div_ceil(0x1000)];
    // SAFETY: `pages` has one byte per page of the range.
    unsafe {
        mincore(at as *const _, len, pages.as_mut_ptr()) == 0 && pages.iter().any(|&b| b & 1 != 0)
    }
}

/// Reads `out.len()` qwords from `at`, if the kernel can.
fn read_into(at: usize, out: &mut [u64]) -> bool {
    unsafe extern "C" {
        static mach_task_self_: u32;
        fn mach_vm_read_overwrite(
            task: u32,
            address: u64,
            size: u64,
            data: u64,
            out: *mut u64,
        ) -> i32;
    }
    let mut got = 0u64;
    let size = (out.len() * 8) as u64;
    // SAFETY: `out` is that many writable bytes; the kernel checks the source.
    let r = unsafe {
        mach_vm_read_overwrite(
            mach_task_self_,
            at as u64,
            size,
            out.as_mut_ptr() as u64,
            &mut got,
        )
    };
    r == 0 && got == size
}

/// A qword at `at`, if the kernel says it can be read.
pub(crate) fn read_checked(at: usize) -> Option<usize> {
    unsafe extern "C" {
        static mach_task_self_: u32;
        fn mach_vm_read_overwrite(task: u32, address: u64, size: u64, data: u64, out: *mut u64) -> i32;
    }
    let (mut v, mut got) = (0usize, 0u64);
    // SAFETY: `v` is 8 writable bytes; the kernel checks the source.
    let r = unsafe { mach_vm_read_overwrite(mach_task_self_, at as u64, 8, &raw mut v as u64, &mut got) };
    (r == 0 && got == 8).then_some(v)
}

fn read(teb: usize, off: usize) -> usize {
    // SAFETY: inside a live TEB block.
    unsafe { ((teb + off) as *const usize).read() }
}

fn collect() -> Vec<String> {
    let me = super::threadctl::ThreadPort::current();
    let mut out = Vec::new();
    for (teb, port, id) in super::teb::live_threads() {
        if port == me || !port.suspend() {
            continue;
        }
        let mut line = format!("thread {id}: ");
        let bounds = Some((read(teb, OFF_STACK_LIMIT), read(teb, OFF_STACK_BASE)));
        let mut ctx = Context::zeroed();
        let target = read(teb, OFF_IN_HOST);
        let mut chain = Vec::new();
        if target != 0 {
            let ret = read(teb, OFF_IN_HOST_RET);
            let name = traced_name(read(teb, OFF_IN_HOST_CTX))
                .map_or(format!("host function {target:#x}"), |(dll, name, slot)| {
                    if name.is_empty() {
                        format!("{dll} method {slot:#x}")
                    } else {
                        format!("{dll}!{name}")
                    }
                });
            line += &format!("in {name}");
            // The bridge's frame: the title's rbp is at [rbp], the return
            // address at [rbp + 8], the title's rsp after it at rbp + 16.
            let rbp = read(teb, OFF_IN_HOST_RBP);
            ctx.rip = ret as u64;
            ctx.gpr[RSP] = (rbp + 16) as u64;
            // SAFETY: the bridge's own frame on that thread's stack.
            ctx.gpr[RBP] = unsafe { (rbp as *const u64).read() };
            chain = super::seh::backtrace(ctx, bounds);
            // VICTORY_DUMP_REGS=1: the title's callee-saved registers at the call (the bridge
            // pushed them below its frame pointer) and the qwords they point at.
            if std::env::var_os("VICTORY_DUMP_REGS").is_some() {
                let regs: Vec<String> = ["rbx", "rdi", "rsi", "r12", "r13", "r14"]
                    .iter()
                    .enumerate()
                    .map(|(i, n)| {
                        let v = read_checked(rbp - 8 * (i + 1)).unwrap_or(0);
                        // Not into the image: only data.
                        let words: Vec<String> = (0..4)
                            .filter(|_| !(0x1_4000_0000..0x1_4600_0000).contains(&v))
                            .map(|w| read_checked(v + 8 * w).map_or("-".into(), |x| format!("{x:x}")))
                            .collect();
                        format!("{n}={v:#x} [{}]", words.join(" "))
                    })
                    .collect();
                let arg = |off: usize| read_checked(rbp - off).unwrap_or(0);
                line += &format!(" args {:#x} {:#x} regs {}", arg(0x100), arg(0xF8), regs.join(" "));
            }
            // VICTORY_DUMP_STACK=1: the title's stack words above the call, and 4 qwords behind
            // each that points at data (not at the image's code).
            if std::env::var_os("VICTORY_DUMP_STACK").is_some() && chain.len() > 2 && !read_all_bases(&chain) {
                let mut out = Vec::new();
                for i in 0..96 {
                    let at = rbp + 16 + 8 * i;
                    let Some(v) = read_checked(at) else { break };
                    let data = (0x1_4600_0000..0x7fff_ffff_0000).contains(&v) && v & 7 == 0;
                    let words: Vec<String> = if data {
                        (0..4).map(|w| read_checked(v + 8 * w).map_or("-".into(), |x| format!("{x:x}"))).collect()
                    } else {
                        Vec::new()
                    };
                    out.push(format!("+{:#x}: {v:x} [{}]", 8 * i, words.join(" ")));
                }
                line += &format!(" stack {}", out.join(" | "));
            }
        } else if port.context(&mut ctx) {
            if unwind::image_base_of(ctx.rip as usize).is_some() {
                line += "in title code";
                chain = super::seh::backtrace(ctx, bounds);
            } else {
                line += &format!("in host code at {:#x}", ctx.rip);
            }
        }
        port.resume();
        let base = chain.first().and_then(|&pc| unwind::image_base_of(pc)).unwrap_or(0);
        let offsets: Vec<usize> = chain.iter().map(|&pc| pc.wrapping_sub(base)).collect();
        out.push(format!("{line}, title image+ {offsets:x?}"));
    }
    out
}

/// The calling thread's title call chain (image offsets) while it is in a host call, for
/// diagnostics such as `VICTORY_TRACE_HANDLES`.
pub fn caller_chain() -> Vec<usize> {
    let teb = super::teb::current() as usize;
    if teb == 0 || read(teb, OFF_IN_HOST) == 0 {
        return Vec::new();
    }
    let rbp = read(teb, OFF_IN_HOST_RBP);
    let mut ctx = Context::zeroed();
    ctx.rip = read(teb, OFF_IN_HOST_RET) as u64;
    ctx.gpr[RSP] = (rbp + 16) as u64;
    // SAFETY: the bridge's own frame on this thread's stack.
    ctx.gpr[RBP] = unsafe { (rbp as *const u64).read() };
    let bounds = Some((read(teb, OFF_STACK_LIMIT), read(teb, OFF_STACK_BASE)));
    let chain = super::seh::backtrace(ctx, bounds);
    let base = chain.first().and_then(|&pc| unwind::image_base_of(pc)).unwrap_or(0);
    chain.iter().map(|&pc| pc.wrapping_sub(base)).collect()
}

/// Whether the chain is not the main wait loop's (the stack dump is for one thread).
fn read_all_bases(chain: &[usize]) -> bool {
    let base = chain.first().and_then(|&pc| unwind::image_base_of(pc)).unwrap_or(0);
    !chain.iter().any(|&pc| pc.wrapping_sub(base) == 0x1d83996)
}

/// `VICTORY_TRAP_WRITES=<hex page address>,<hex watched address>,...`: the page is kept read-only
/// and each title write to it faults here; a write to a watched address is logged with the
/// writer's title call chain. The page is opened for the faulting write and closed again a moment
/// later by a thread, so most writes are not seen (statistics only: look for rare writers).
pub fn write_trap(c: &Context, r: &super::seh::context::ExceptionRecord) -> bool {
    use std::sync::OnceLock;
    static SPEC: OnceLock<Option<(usize, Vec<usize>)>> = OnceLock::new();
    let spec = SPEC.get_or_init(|| {
        let v = std::env::var("VICTORY_TRAP_WRITES").ok()?;
        let mut it = v.split(',').filter_map(|x| usize::from_str_radix(x.trim_start_matches("0x"), 16).ok());
        let page = it.next()?;
        Some((page & !0xfff, it.collect()))
    });
    let Some((page, watched)) = spec else { return false };
    if r.code != 0xc000_0005 || r.information[0] != 1 || (r.information[1] as usize) & !0xfff != *page {
        return false;
    }
    let at = r.information[1] as usize;
    if watched.contains(&(at & !7)) {
        let base = unwind::image_base_of(c.rip as usize).unwrap_or(0);
        let chain = super::seh::backtrace(*c, None);
        let chain: Vec<usize> = chain.iter().map(|&pc| pc.wrapping_sub(base)).collect();
        tracing::warn!("write to {at:#x} from image+ {:x?}", chain);
    }
    unsafe extern "C" {
        fn mprotect(addr: *mut std::ffi::c_void, len: usize, prot: i32) -> i32;
    }
    // SAFETY: the page is the image's own data.
    unsafe { mprotect(*page as *mut _, 0x1000, 3) };
    true
}

/// Starts the thread that closes the trapped page again.
pub fn start_write_trap() {
    let Some(page) = std::env::var("VICTORY_TRAP_WRITES")
        .ok()
        .and_then(|v| usize::from_str_radix(v.split(',').next()?.trim_start_matches("0x"), 16).ok())
    else {
        return;
    };
    unsafe extern "C" {
        fn mprotect(addr: *mut std::ffi::c_void, len: usize, prot: i32) -> i32;
    }
    std::thread::spawn(move || {
        loop {
            // SAFETY: the image's own data page.
            unsafe { mprotect((page & !0xfff) as *mut _, 0x1000, 1) };
            std::thread::sleep(Duration::from_micros(300));
        }
    });
}
