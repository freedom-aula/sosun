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
                .map_or(format!("host function {target:#x}"), |(dll, name)| format!("{dll}!{name}"));
            line += &format!("in {name}");
            // The bridge's frame: the title's rbp is at [rbp], the return
            // address at [rbp + 8], the title's rsp after it at rbp + 16.
            let rbp = read(teb, OFF_IN_HOST_RBP);
            ctx.rip = ret as u64;
            ctx.gpr[RSP] = (rbp + 16) as u64;
            // SAFETY: the bridge's own frame on that thread's stack.
            ctx.gpr[RBP] = unsafe { (rbp as *const u64).read() };
            chain = super::seh::backtrace(ctx, bounds);
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
