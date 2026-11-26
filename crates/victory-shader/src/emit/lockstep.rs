//! Lockstep: a console wave of at most 64 lanes whose fast translation is refused, run with every
//! thread of the host group following the wave's one program counter.
//!
//! The fast path (`structure`, decision 0003) runs the program as one lane does: a branch on EXEC
//! becomes a branch on the lane's own bit, and lanes split up. On a 32-lane host a console wave is
//! two host waves, and an operation that reaches across them (`lower/halves.rs`) needs both at a
//! group barrier, which split lanes never reach together: such a program is refused there
//! (`DIVERGENT_EXCHANGE`, `SPLIT_LDS` and others), one proof per shape.
//!
//! Here every branch the console takes on a lane mask (`s_cbranch_execz`, `execnz`, `vccz`,
//! `vccnz`, SCC of a mask operation) becomes a test of the whole mask as a number, which
//! [`branches`] makes explicit (`s_cmp_{eq,lg}_u64 mask, 0` before an SCC branch). The mask's
//! number is the console's ballot (`wide_ballot`), the same in every thread, so every branch is
//! uniform across the group: all threads reach every block, and every group barrier, the same
//! number of times, loops included. EXEC is then only a predicate: what reaching a block no longer
//! proves stays each vector instruction's guard (a per-thread `if` around it, with the old value
//! kept where it is skipped), and every scalar value stays the same in all threads because each
//! lane-to-scalar operation goes through the exchange: ballots, `v_readlane_b32`, and
//! `v_readfirstlane_b32` (the first lane of the wide EXEC). LDS gets a group barrier after each
//! write, outside its guard.
//!
//! Only programs whose group is one console wave (64 lanes or fewer) run this way: in a group of
//! several waves, one wave's branches are not the others'.

use super::{HostShader, Options, emit_fast};
use crate::dxbc::Container;
use crate::gfx10::{self, Fields};
use crate::io::Io;
use crate::ir::{EXEC, Exit, Inst, Program, Reg, VCC, Value};
use crate::native::{HwStage, NativeProgram, StageIo};
use crate::structure::{self, Atom, LaneExit, Structured};
use crate::{ShaderError, resource};

/// A branch the lockstep structuriser could not make uniform.
pub(crate) const NOT_UNIFORM: &str = "a branch not uniform across a console wave run in lockstep";

/// Whether a refused program may run in lockstep: wave64 compute on a 32-lane host whose group is
/// one console wave.
pub(super) fn fits(native: &NativeProgram, st: &Structured, options: &Options) -> bool {
    let StageIo::Cs(cs) = &native.io else { return false };
    options.wave32_host
        && native.stage == HwStage::Cs
        && native.wave64
        && !st.lockstep
        && cs.threads.iter().product::<u32>() <= 64
}

/// The lockstep translation of `p` (see the module).
pub(super) fn retry(
    c: &Container,
    native: &NativeProgram,
    p: &Program,
    io: &Io,
    options: Options,
) -> Result<HostShader, ShaderError> {
    let q = branches(p);
    let st = structure::lockstep(&q)?;
    if let Some(offset) = divergent(&q, &st) {
        return Err(ShaderError::Unsupported { offset, what: NOT_UNIFORM });
    }
    let res = resource::track_graphics(
        &q, &native.layout, native.stage, Some(&st), options.zero_copy_memory,
    )?;
    emit_fast(c, native, &q, &st, io, &res, options)
}

/// The offset of a branch whose condition is not a uniform (SCC) value alone.
fn divergent(p: &Program, st: &Structured) -> Option<usize> {
    (0..p.blocks.len()).find_map(|b| {
        let LaneExit::Branch { when, .. } = st.exits[b] else { return None };
        let lanes = st.bools.support(when).iter().any(|a| !matches!(a, Atom::Uniform(_)));
        lanes.then(|| p.blocks[b].insts.last().map_or(0, |i| i.machine.offset))
    })
}

/// `p` with each branch on EXEC or VCC (`s_cbranch_execz` and the like) made a 64-bit compare of
/// the mask with 0 and a branch on its SCC.
pub(crate) fn branches(p: &Program) -> Program {
    let mut q = p.clone();
    let mut next = max_value(p).map_or(0, |v| v.0 + 1);
    for block in &mut q.blocks {
        if !matches!(block.exit, Exit::Branch { .. }) {
            continue;
        }
        let Some(last) = block.insts.last() else { continue };
        let (reg, eq) = match last.name() {
            "s_cbranch_execz" => (EXEC, true),
            "s_cbranch_execnz" => (EXEC, false),
            "s_cbranch_vccz" => (VCC, true),
            "s_cbranch_vccnz" => (VCC, false),
            _ => continue,
        };
        let (Some(lo), Some(hi)) = (last.read(Reg::S(reg)), last.read(Reg::S(reg + 1))) else {
            continue;
        };
        let scc = Value(next);
        next += 1;
        // s_cmp_eq_u64 / s_cmp_lg_u64 reg, 0; s_cbranch_scc1, the branch's distance kept.
        let cmp = 0xBF00_8000 | u32::from(0x12 + u8::from(!eq)) << 16 | u32::from(reg);
        let branch = 0xBF85_0000 | last.machine.words[0] & 0xFFFF;
        let offset = last.machine.offset;
        let made = |word: u32, reads: Vec<(Reg, Value)>, writes: Vec<(Reg, Value)>| {
            let mut machine = gfx10::decode(&[word, 0xBF81_0000].map(u32::to_le_bytes).concat())
                .ok()?
                .swap_remove(0);
            machine.offset = offset;
            machine.words[0] = word;
            let fields: Fields = machine.fields(true);
            Some(Inst { machine, fields, reads, writes })
        };
        // The distance decodes from offset 0: made at a distance of 0, then given the branch's.
        let Some(cmp) = made(cmp, vec![(Reg::S(reg), lo), (Reg::S(reg + 1), hi)], vec![(Reg::Scc, scc)])
        else {
            continue;
        };
        let Some(mut br) = made(0xBF85_0000, vec![(Reg::Scc, scc)], vec![]) else { continue };
        br.machine.words[0] = branch;
        br.fields = br.machine.fields(true);
        block.insts.pop();
        block.insts.extend([cmp, br]);
    }
    q
}

/// Where lockstep LDS takes group barriers, by block and instruction (a barrier goes before the
/// instruction; at the block's length, after its last): around each LDS write, so what one host
/// wave writes is seen by the other's next read, and neither writes over what the other has yet to
/// read. Every thread reaches each one, so they sit outside the guards.
pub(crate) fn lds_barriers(p: &Program) -> crate::structure::Set<(usize, usize)> {
    let mut at = crate::structure::Set::default();
    for (b, block) in p.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            let n = inst.name();
            let reads = n.starts_with("ds_read") || n.starts_with("ds_swizzle") || n.contains("permute");
            if matches!(inst.fields, Fields::Ds { .. }) && !reads {
                at.extend([(b, i), (b, i + 1)]);
            }
        }
    }
    at
}

fn max_value(p: &Program) -> Option<Value> {
    let insts = p.blocks.iter().flat_map(|b| &b.insts);
    let regs = insts.flat_map(|i| i.reads.iter().chain(&i.writes).map(|r| r.1));
    let phis = p.blocks.iter().flat_map(|b| &b.phis).flat_map(|f| f.args.iter().copied().chain([f.value]));
    regs.chain(phis).chain(p.inputs.iter().map(|r| r.1)).filter(|v| v.0 != u32::MAX).max()
}
