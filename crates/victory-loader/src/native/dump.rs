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

/// Starts the dumping thread if `VICTORY_DUMP_THREADS` is set.
pub fn start() {
    let Some(secs) = seconds() else { return };
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(secs));
        tracing::warn!(
            "clzero emulations so far: {}",
            super::seh::signal::EMULATED.load(std::sync::atomic::Ordering::Relaxed)
        );
        for line in collect() {
            tracing::warn!("{line}");
        }
    });
}

/// A qword at `at`, if the kernel says it can be read.
fn read_checked(at: usize) -> Option<usize> {
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
