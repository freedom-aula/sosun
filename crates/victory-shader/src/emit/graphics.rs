//! Graphics programs of the console's pipeline descriptions (device slot 0x1c8), inferred.
//!
//! A description gives a stage's GFX10 code and its metadata (the compute metadata's records
//! after a 0x10-byte header, [`crate::native::metadata_resources`]), but not the stage data an
//! `FSSM` record carries: the register image is undecoded. So what that data says is inferred
//! from the code, and a container is made that holds the code, the metadata, and the two
//! signatures the translator reads from a container (`ISG1`, `OSG1`), made from the same
//! inference. Every inference here is a guess (docs/native-host.md, "Guessed graphics
//! pipelines"), used only when asked for (`VICTORY_GUESS_GRAPHICS=1`):
//!
//! * a vertex shader fetches its own vertices (its VGPR inputs are the ids), so it has no
//!   attributes; its outputs are the `exp param<n>` it makes, each a `TEXCOORD<n>` of register n,
//!   with the channels it enables, and `SV_Position` after them;
//! * a pixel shader's attribute n is a `TEXCOORD<n>` of register n (so the interpolant of
//!   vertex parameter n), with the channels it reads; the VGPRs it reads on entry are, in order,
//!   barycentric pairs (when it interpolates), then position, front face, ancillary, coverage,
//!   with matching system-value elements declared in `ISG1`; its colour exports `exp mrt<n>` are
//!   targets of register n, in float (a compressed export is two FP16 values per dword);
//! * the wave size is inferred from wave64/wave32-only scalar mask instructions and high-half
//!   mask registers (`exec_hi`/`vcc_hi`), defaulting to 64 when neither signal is present.

use super::container::{SigElement, signature};
use crate::ShaderError::{self, Malformed};
use crate::dxbc::{self, FourCC, u32_at};
use crate::gfx10::{Fields, Instruction, Operand};
use crate::ir::{Inst, Program, Reg, Value};
use crate::native::{
    HwStage, NativeProgram, PsInput, RootParam, RootType, StageIo, Usage, VsOutput,
    metadata_resources,
};

/// The part a graphics piece pair is carried in: `"FSPG"`, u32 stage (0 vertex, 1 pixel), u32
/// code size, u32 metadata size, then the code and the metadata.
pub const MAGIC: &[u8] = b"FSPG";

pub(super) fn is_guessed(c: &dxbc::Container<'_>) -> bool {
    c.part(FourCC::DXIL)
        .or(c.part(FourCC::ACCC))
        .is_some_and(|part| part.data.starts_with(MAGIC))
}

/// The root slot a vertex shader's vertex buffer table (s0/s1) is given: no title root parameter.
pub const VERTEX_TABLE: u8 = 253;

/// Where the metadata's records start (the header is 0x10 bytes).
const RECORDS: usize = 0x10;

/// A container with the part of `stage`'s `code` and `meta`, and the signatures inferred from the
/// code.
pub fn container(stage: HwStage, code: &[u8], meta: &[u8]) -> Result<Vec<u8>, ShaderError> {
    let (program, sigs) = infer(stage, code, meta)?;
    let mut part = MAGIC.to_vec();
    for w in [u32::from(program.stage == HwStage::Ps), code.len() as u32, meta.len() as u32] {
        part.extend(w.to_le_bytes());
    }
    part.extend(code);
    part.extend(meta);
    let mut parts: Vec<(FourCC, Vec<u8>)> = vec![(FourCC::ACCC, part)];
    parts.push((FourCC::ISG1, signature(&sigs.0, true)));
    parts.push((FourCC::OSG1, signature(&sigs.1, false)));
    let parts: Vec<(FourCC, &[u8])> = parts.iter().map(|(f, d)| (*f, &d[..])).collect();
    Ok(dxbc::build(&parts))
}

/// The program of an `FSPG` part.
pub fn program(part: &[u8]) -> Result<NativeProgram, ShaderError> {
    let (stage, code_len, meta_len) =
        (u32_at(part, 4)?, u32_at(part, 8)? as usize, u32_at(part, 12)? as usize);
    let code = part.get(16..16 + code_len).ok_or(Malformed("FSPG code out of bounds"))?;
    let meta = part
        .get(16 + code_len..16 + code_len + meta_len)
        .ok_or(Malformed("FSPG metadata out of bounds"))?;
    let stage = if stage == 0 { HwStage::Vs } else { HwStage::Ps };
    Ok(infer(stage, code, meta)?.0)
}

type Signatures = (Vec<SigElement>, Vec<SigElement>);

fn element(name: &str, index: u32, kind: u8, row: u32, mask: u8) -> SigElement {
    let lo = mask.trailing_zeros().min(3) as u8;
    let hi = 8 - mask.leading_zeros() as u8;
    SigElement {
        name: name.into(),
        index,
        kind,
        comp: 9,
        interp: 0,
        row: Some(row),
        col: lo,
        cols: hi.saturating_sub(lo).max(1),
        usage: 0,
        half: false,
    }
}

fn infer(
    stage: HwStage,
    code: &[u8],
    meta: &[u8],
) -> Result<(NativeProgram, Signatures), ShaderError> {
    if !matches!(stage, HwStage::Vs | HwStage::Ps) {
        return Err(ShaderError::Unsupported {
            offset: 0,
            what: "a graphics stage but vertex and pixel",
        });
    }
    let list = crate::gfx10::decode(code)?;
    let wave64 = infer_wave64(&list);
    let (mut layout, mut used) = metadata_resources(meta, RECORDS, true)?;
    // The registers the program names: its VGPR count, and what it reads on entry.
    let p = crate::ir::build(code, wave64, 256)?;
    if stage == HwStage::Vs
        && matches!(u32_at(meta, 0), Ok(2))
        && let Some(&[0x40, first]) = meta.get(12..14)
    {
        // shortcut: try header-relative 0x41 indices only when at least one lies below
        // the stated start, until the register image identifies the user-data layout.
        if first > 0
            && layout.iter().any(|r| r.user_data < first)
            && layout.iter().all(|r| u32::from(first) + u32::from(r.user_data) + r.sgprs <= 32)
        {
            let mut relative = layout.clone();
            for r in &mut relative {
                r.user_data += first;
            }
            if crate::resource::vertex_roots_match_rebased(&p, &layout, &relative)? {
                layout = relative;
                for r in &mut used {
                    if r.root != RootType::ImmediateSampler {
                        r.user_data = r.user_data.checked_add(first)
                            .ok_or(Malformed("FSPG user-data index out of bounds"))?;
                    }
                }
            }
        }
    }
    // shortcut: multi-dword root constants are only expanded when the immediately following
    // root signature slot (`slot + 1`) is also present in metadata; a trailing multi-dword
    // constant block stays 1 SGPR until the register image is decoded.
    for i in 0..layout.len() {
        if layout[i].root != RootType::Constants {
            continue;
        }
        let (slot, start) = (layout[i].slot, layout[i].user_data);
        if let Some(next) =
            layout.iter().find(|p| p.slot == slot.wrapping_add(1) && p.user_data > start)
        {
            let span = u32::from(next.user_data - start);
            if span > layout[i].sgprs && span <= 32 {
                layout[i].sgprs = span;
            }
        }
    }
    let occupied = |layout: &[RootParam], reg: u8| {
        let r = u32::from(reg);
        layout
            .iter()
            .any(|p| r >= u32::from(p.user_data) && r < u32::from(p.user_data) + p.sgprs)
    };
    // shortcut: SM 6.6 ResourceHeap / SamplerHeap SGPRs are inferred from 32-bit descriptor loads
    // (`hi == 0`) feeding resource/sampler instructions; upgrade when the register image or root
    // signature blob is decoded.
    let inferred_roots = crate::resource::infer_roots(&p, &layout, stage)?;
    for (user_data, root) in inferred_roots.heaps {
        if !occupied(&layout, user_data) {
            layout.push(RootParam {
                root,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data,
                slot: if root == RootType::SamplerHeap { 254 } else { 252 },
            });
        }
    }
    // A vertex shader's own vertex fetch: usually s0/s1 address the table of vertex buffer
    // descriptors it loads from (observed: `s_load_dwordx4 sN, s[0:1]` before its `buffer_load`),
    // unless metadata already occupies s0..s1 or an inferred vertex table sits at another pair.
    if stage == HwStage::Vs {
        let vt = inferred_roots
            .vertex_table
            .filter(|&n| !occupied(&layout, n) && !occupied(&layout, n + 1))
            .or_else(|| (!occupied(&layout, 0) && !occupied(&layout, 1)).then_some(0));
        if let Some(user_data) = vt {
            layout.push(RootParam {
                root: RootType::Descriptor,
                table_offset_dwords: 0,
                sgprs: 2,
                base_register: 0,
                user_data,
                slot: VERTEX_TABLE,
            });
        }
    }
    let top = p
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .flat_map(|i| i.reads.iter().chain(&i.writes))
        .filter_map(|r| match r.0 {
            Reg::V(n) => Some(u16::from(n) + 1),
            _ => None,
        })
        .max()
        .unwrap_or(1);
    let vgprs = top.next_multiple_of(4);
    // shortcut: live-in VGPR count is inferred from entry VGPRs explicitly read as instruction
    // sources (before any write), ignoring IR old-value ties of writes; upgrade when
    // `SPI_PS_INPUT_ADDR` is read from the register image.
    let live_in = explicit_entry_vgprs(&p)
        .into_iter()
        .map(|n| u16::from(n) + 1)
        .max()
        .unwrap_or(0);

    let (mut params, mut targets) = (Vec::<(u8, u8)>::new(), Vec::<(u8, u8)>::new());
    let mut attrs: Vec<(u8, u8, bool)> = Vec::new(); // (attribute, channels, flat)
    let mut depth = false;
    let mut interpolates = false;
    for i in &list {
        let flat = i.name() == Some("v_interp_mov_f32");
        let mut attr = |a: &Operand| {
            if let Operand::Attr { index, chan, .. } = *a {
                match attrs.iter_mut().find(|e| e.0 == index) {
                    Some(e) => {
                        e.1 |= 1 << chan;
                        e.2 &= flat;
                    }
                    None => attrs.push((index, 1 << chan, flat)),
                }
                interpolates |= !flat;
            }
        };
        match i.fields(wave64) {
            Fields::Vintrp { attr: a, .. } => attr(&a),
            Fields::Valu(v) => v.src.iter().for_each(attr),
            Fields::Exp { target, src, .. } => {
                let mask = (0..4).filter(|&c| src[c].is_some()).fold(0u8, |m, c| m | 1 << c);
                match target {
                    0..=7 => targets.push((target, mask)),
                    8 => depth |= mask & 1 != 0,
                    32..=63 => params.push((target - 32, mask)),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let merge = |v: &mut Vec<(u8, u8)>| {
        let mut out: Vec<(u8, u8)> = Vec::new();
        for &(n, m) in v.iter() {
            match out.iter_mut().find(|e| e.0 == n) {
                Some(e) => e.1 |= m,
                None => out.push((n, m)),
            }
        }
        out.sort_unstable();
        *v = out;
    };
    merge(&mut params);
    merge(&mut targets);
    attrs.sort_unstable();

    let (io, sigs) = if stage == HwStage::Vs {
        let rows = params.iter().map(|p| u32::from(p.0) + 1).max().unwrap_or(0);
        let mut outs: Vec<SigElement> = params
            .iter()
            .map(|&(n, m)| element("TEXCOORD", u32::from(n), 0, u32::from(n), m))
            .collect();
        outs.push(element("SV_Position", 0, 3, rows, 0xf));
        let outputs = params
            .iter()
            .map(|&(n, _)| VsOutput {
                usage: Usage::Generic,
                row: n,
                param: n,
                fp16: false,
                pack2: false,
            })
            .collect();
        (StageIo::Vs { inputs: Vec::new(), outputs }, (Vec::new(), outs))
    } else {
        let mut ins: Vec<SigElement> = attrs
            .iter()
            .map(|&(n, m, _)| element("TEXCOORD", u32::from(n), 0, u32::from(n), m))
            .collect();
        let mut outs: Vec<SigElement> = targets
            .iter()
            .map(|&(n, m)| element("SV_Target", u32::from(n), 16, u32::from(n), m))
            .collect();
        if depth {
            outs.push(element("SV_Depth", 0, 17, 0, 1));
        }
        let inputs = attrs
            .iter()
            .map(|&(n, m, flat)| PsInput {
                usage: Usage::Generic,
                row: n,
                mask: m,
                attr: n,
                flat,
                fp16: false,
                pack2: false,
                row2: 0,
            })
            .collect();
        // Entry VGPRs, as `SPI_PS_INPUT_ADDR` enables them: barycentric pairs first (in common
        // order: PerspectiveCenter, PerspectiveCentroid, LinearCenter, LinearCentroid,
        // PerspectiveSample, LinearSample), then system values from position x (bit 8) on.
        // shortcut: without the register image, exact barycentric modes and sparse system-value
        // bits (e.g. FrontFace without Position.zw) cannot be distinguished; upgrade when
        // `SPI_PS_INPUT_ADDR` is decoded from the 0x1c8 register image.
        const BARY_BITS: [u32; 6] = [1, 2, 5, 6, 0, 4];
        let mut bary_pairs = barycentric_entry_vgprs(&p)
            .into_iter()
            .map(|r| usize::from(r) / 2 + 1)
            .max()
            .unwrap_or(0)
            .min(BARY_BITS.len());
        if interpolates && bary_pairs == 0 {
            bary_pairs = 1;
        }
        let mut addr = 0u32;
        let mut left = i32::from(live_in);
        for &bit in &BARY_BITS[..bary_pairs] {
            addr |= 1 << bit;
            left -= 2;
        }
        for bit in 8..=14 {
            if left > 0 {
                addr |= 1 << bit;
                left -= 1;
            }
        }
        let mut next_row = attrs.iter().map(|a| u32::from(a.0) + 1).max().unwrap_or(0);
        if addr & (0xF << 8) != 0 {
            ins.push(SigElement {
                name: "SV_Position".into(),
                index: 0,
                kind: 3,
                comp: 9,
                interp: 4,
                row: Some(next_row),
                col: 0,
                cols: 4,
                usage: 0,
                half: false,
            });
            next_row += 1;
        }
        if addr & (1 << 12) != 0 {
            ins.push(SigElement {
                name: "SV_IsFrontFace".into(),
                index: 0,
                kind: 13,
                comp: 5,
                interp: 1,
                row: Some(next_row),
                col: 0,
                cols: 1,
                usage: 0,
                half: false,
            });
        }
        if addr & (1 << 13) != 0 {
            ins.push(SigElement {
                name: "SV_SampleIndex".into(),
                index: 0,
                kind: 12,
                comp: 5,
                interp: 0,
                row: None,
                col: 0,
                cols: 1,
                usage: 0,
                half: false,
            });
        }
        if addr & (1 << 14) != 0 {
            ins.push(SigElement {
                name: "SV_Coverage".into(),
                index: 0,
                kind: 14,
                comp: 5,
                interp: 0,
                row: None,
                col: 0,
                cols: 1,
                usage: 0,
                half: false,
            });
        }
        let db_shader_control = if uses_pops(&list) { 1 << 16 } else { 0 };
        (
            StageIo::Ps { inputs, input_addr: addr, db_shader_control, patches: Vec::new() },
            (ins, outs),
        )
    };
    Ok((NativeProgram { stage, wave64, vgprs, layout, used, io, code: code.to_vec() }, sigs))
}

/// Infers whether `list` runs as wave64 (else wave32) from ISA signals rather than formatted text
/// (formatting with `wave64 = false` prints every implicit-VCC VALU op as `vcc_lo`).
/// Default is wave64 when neither wave64-only nor wave32-only signals are present.
fn infer_wave64(list: &[Instruction]) -> bool {
    let mut strong_w64 = false;
    let mut strong_w32 = false;
    let mut weak_w64 = false;
    let mut weak_w32 = false;
    for i in list {
        let name = i.name().unwrap_or("");
        let fields = i.fields(false);
        if name.ends_with("_saveexec_b64")
            || name.ends_with("_wrexec_b64")
            || matches!(name, "s_wqm_b64" | "s_quadmask_b64")
        {
            strong_w64 = true;
        }
        if name.ends_with("_saveexec_b32")
            || name.ends_with("_wrexec_b32")
            || matches!(name, "s_wqm_b32" | "s_quadmask_b32")
        {
            strong_w32 = true;
        }
        let (hi_exec, hi_vcc) = touches_hi_mask(&fields);
        if hi_exec {
            strong_w64 = true;
        }
        if hi_vcc {
            weak_w64 = true;
        }
        if let Fields::Sop { dst, src, .. } = &fields {
            let is_64 = name.ends_with("_b64") || name.ends_with("_u64") || name.ends_with("_i64");
            let is_32 = name.ends_with("_b32") || name.ends_with("_u32") || name.ends_with("_i32");
            for op in dst.iter().chain(src) {
                if let Operand::Scalar { code, .. } = *op {
                    if is_64 && matches!(code, 106 | 126) {
                        strong_w64 = true;
                    }
                    if is_32 && code == 126 {
                        weak_w32 = true;
                    }
                }
            }
        }
    }
    strong_w64 || (!strong_w32 && (weak_w64 || !weak_w32))
}

/// Whether `fields` (decoded with `wave64 = false`) explicitly names `exec_hi` (127) or `vcc_hi`
/// (107).
fn touches_hi_mask(fields: &Fields) -> (bool, bool) {
    let mut hi_exec = false;
    let mut hi_vcc = false;
    let mut check = |op: &Operand| {
        if let Operand::Scalar { code, .. } = *op {
            if code == 127 {
                hi_exec = true;
            } else if code == 107 {
                hi_vcc = true;
            }
        }
    };
    match fields {
        Fields::Sop { dst, src, .. } => dst.iter().chain(src).for_each(check),
        Fields::Smem { data, base, soffset, .. } => {
            data.iter().chain([base, soffset]).for_each(check);
        }
        Fields::Valu(v) => v.dst.iter().chain(&v.carry).chain(&v.src).for_each(check),
        Fields::Buffer { rsrc, soffset, .. } => {
            check(rsrc);
            check(soffset);
        }
        Fields::Image { rsrc, sampler, .. } => {
            check(rsrc);
            sampler.iter().for_each(check);
        }
        Fields::Flat { saddr, .. } => saddr.iter().for_each(check),
        Fields::Sopp { .. } | Fields::Vintrp { .. } | Fields::Ds { .. } | Fields::Exp { .. } => {}
    }
    (hi_exec, hi_vcc)
}

/// Whether a pixel shader uses Primitive Ordered Pixel Shading (POPS / ROVs).
fn uses_pops(list: &[Instruction]) -> bool {
    let is_pops_const = |op: &Operand| matches!(*op, Operand::Const(239));
    list.iter().any(|i| {
        let name = i.name().unwrap_or("");
        if name == "s_endpgm_ordered_ps_done" {
            return true;
        }
        match i.fields(false) {
            Fields::Sopp { imm } => {
                matches!(name, "s_sendmsg" | "s_sendmsghalt") && imm & 0xF == 7
            }
            Fields::Sop { src, .. } => src.iter().any(is_pops_const),
            Fields::Valu(v) => v.src.iter().any(is_pops_const),
            Fields::Smem { soffset, .. } | Fields::Buffer { soffset, .. } => {
                is_pops_const(&soffset)
            }
            _ => false,
        }
    })
}

/// Entry VGPRs that are explicitly read as source operands before being overwritten (ignoring
/// writes whose only read of the VGPR is the IR's inactive-lane old-value tie).
fn explicit_entry_vgprs(p: &Program) -> Vec<u8> {
    let entry: std::collections::HashMap<Value, u8> = p
        .inputs
        .iter()
        .filter_map(|&(reg, v)| match reg {
            Reg::V(n) => Some((v, n)),
            _ => None,
        })
        .collect();
    let mut found = Vec::new();
    let mark = |found: &mut Vec<u8>, inst: &Inst, reg: u8| {
        if let Some(v) = inst.read(Reg::V(reg))
            && let Some(&n) = entry.get(&v)
            && !found.contains(&n)
        {
            found.push(n);
        }
    };
    let mark_op = |found: &mut Vec<u8>, inst: &Inst, op: &Operand| {
        if let Operand::Vector { reg, dwords } = *op {
            for k in 0..dwords {
                if let Some(r) = reg.checked_add(k) {
                    mark(found, inst, r);
                }
            }
        }
    };
    for block in &p.blocks {
        for inst in &block.insts {
            let name = inst.name();
            match &inst.fields {
                Fields::Valu(v) => {
                    let src_indexed = matches!(
                        name,
                        "v_movrels_b32"
                            | "v_movrelsd_b32"
                            | "v_movrelsd_2_b32"
                            | "v_swaprel_b32"
                    );
                    for (idx, s) in v.src.iter().enumerate() {
                        if idx == 0
                            && src_indexed
                            && let Operand::Vector { reg, .. } = *s
                        {
                            for r in u16::from(reg)..p.vgprs {
                                mark(&mut found, inst, r as u8);
                            }
                        } else {
                            mark_op(&mut found, inst, s);
                        }
                    }
                    let reads_dst = name.contains("fmac")
                        || name.contains("_mac_")
                        || name.contains("dot2c")
                        || name.contains("dot4c")
                        || name.contains("dot8c")
                        || matches!(name, "v_swap_b32" | "v_swaprel_b32" | "v_writelane_b32");
                    if reads_dst && let Some(d) = &v.dst {
                        if name == "v_swaprel_b32"
                            && let Operand::Vector { reg, .. } = *d
                        {
                            for r in u16::from(reg)..p.vgprs {
                                mark(&mut found, inst, r as u8);
                            }
                        } else {
                            mark_op(&mut found, inst, d);
                        }
                    }
                }
                Fields::Vintrp { dst, src, .. } => {
                    mark_op(&mut found, inst, src);
                    if name == "v_interp_p2_f32" {
                        mark_op(&mut found, inst, dst);
                    }
                }
                Fields::Ds { addr, data, .. } => {
                    for o in addr.iter().chain(data) {
                        mark_op(&mut found, inst, o);
                    }
                }
                Fields::Buffer { data, addr, offen, idxen, .. } => {
                    if *offen || *idxen {
                        mark_op(&mut found, inst, addr);
                    }
                    if (name.contains("_store_") || name.contains("_atomic_"))
                        && let Some(d) = data
                    {
                        mark_op(&mut found, inst, d);
                    }
                }
                Fields::Image { data, addr, .. } => {
                    for a in addr {
                        mark_op(&mut found, inst, a);
                    }
                    if name.contains("_store") || name.contains("_atomic_") {
                        mark_op(&mut found, inst, data);
                    }
                }
                Fields::Flat { addr, data, .. } => {
                    for o in [addr, data].into_iter().flatten() {
                        mark_op(&mut found, inst, o);
                    }
                }
                Fields::Exp { src, .. } => {
                    for s in src.iter().flatten() {
                        mark_op(&mut found, inst, s);
                    }
                }
                Fields::Sop { .. } | Fields::Sopp { .. } | Fields::Smem { .. } => {}
            }
        }
    }
    found
}

/// Entry VGPR indices read as barycentrics (`i`/`j`) by `v_interp_p1*` / `v_interp_p2*`,
/// following plain `v_mov_b32` copies and phis.
fn barycentric_entry_vgprs(p: &Program) -> Vec<u8> {
    let entry: std::collections::HashMap<Value, u8> = p
        .inputs
        .iter()
        .filter_map(|&(reg, v)| match reg {
            Reg::V(n) => Some((v, n)),
            _ => None,
        })
        .collect();
    let defs: std::collections::HashMap<Value, &Inst> = p
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .flat_map(|i| i.writes.iter().map(move |w| (w.1, i)))
        .collect();
    let phis: std::collections::HashMap<Value, &[Value]> = p
        .blocks
        .iter()
        .flat_map(|b| &b.phis)
        .map(|phi| (phi.value, phi.args.as_slice()))
        .collect();

    let mut out = Vec::new();
    let mut trace = |start: Value| {
        let mut stack = vec![start];
        let mut seen = Vec::new();
        while let Some(v) = stack.pop() {
            if seen.contains(&v) {
                continue;
            }
            seen.push(v);
            if let Some(&n) = entry.get(&v) {
                if !out.contains(&n) {
                    out.push(n);
                }
            } else if let Some(&args) = phis.get(&v) {
                stack.extend_from_slice(args);
            } else if let Some(&inst) = defs.get(&v)
                && inst.name() == "v_mov_b32"
                && let Fields::Valu(f) = &inst.fields
                && f.abs == 0
                && f.neg == 0
                && f.sdwa.is_none()
                && f.dpp.is_none()
                && let Some(&Operand::Vector { reg, .. }) = f.src.first()
                && let Some(src_v) = inst.read(Reg::V(reg))
            {
                stack.push(src_v);
            }
        }
    };

    for block in &p.blocks {
        for inst in &block.insts {
            let name = inst.name();
            match &inst.fields {
                Fields::Vintrp { src: Operand::Vector { reg, .. }, .. }
                    if matches!(name, "v_interp_p1_f32" | "v_interp_p2_f32") =>
                {
                    if let Some(v) = inst.read(Reg::V(*reg)) {
                        trace(v);
                    }
                }
                Fields::Valu(v) if matches!(name, "v_interp_p1ll_f16" | "v_interp_p2_f16") => {
                    if let Some(&Operand::Vector { reg, .. }) = v.src.get(1)
                        && let Some(val) = inst.read(Reg::V(reg))
                    {
                        trace(val);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENDPGM: u32 = 0xBF81_0000;

    fn words(ws: &[u32]) -> Vec<u8> {
        ws.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    fn meta_with(records: &[[u8; 32]]) -> Vec<u8> {
        let mut meta = vec![0u8; 0x10];
        meta[12] = 0x40;
        meta[14] = records.len() as u8;
        for r in records {
            meta.extend_from_slice(r);
        }
        let mut hash = [0u8; 32];
        hash[0] = 0x43;
        meta.extend_from_slice(&hash);
        meta
    }

    fn rec(kind: u8, root: u8, user_data: u8, slot: u8) -> [u8; 32] {
        let mut r = [0u8; 32];
        r[0] = 0x41;
        r[4] = kind;
        r[5] = root;
        r[20..23].copy_from_slice(&[4, user_data, slot]);
        r
    }

    /// `v_interp_p1_f32 v2, v0, attr0.x`, `v_interp_p2_f32 v2, v1, attr0.x`, then `exp mrt0` of
    /// v2 in x and `s_endpgm`: a pixel shader of one attribute and one colour.
    fn pixel_code() -> Vec<u8> {
        words(&[0xC808_0000, 0xC809_0001, 0xF800_0801, 0x0000_0002, ENDPGM])
    }

    #[test]
    fn a_pixel_shaders_attributes_and_exports_are_inferred() {
        let meta = [0u8; 0x10];
        let (p, (ins, outs)) = infer(HwStage::Ps, &pixel_code(), &meta).unwrap();
        let StageIo::Ps { inputs, input_addr, .. } = &p.io else {
            panic!("not a pixel program");
        };
        assert_eq!((inputs.len(), inputs[0].attr, inputs[0].mask), (1, 0, 1));
        // Perspective-centre barycentrics only.
        assert_eq!(*input_addr, 2);
        assert_eq!(ins.len(), 1);
        assert_eq!(
            (outs[0].name.as_str(), outs[0].kind, outs[0].row),
            ("SV_Target", 16, Some(0))
        );
        let c = container(HwStage::Ps, &pixel_code(), &meta).unwrap();
        let c = crate::Container::parse(&c).unwrap();
        assert!(c.part(FourCC::ISG1).is_some() && c.part(FourCC::OSG1).is_some());
        let part = c.part(FourCC::ACCC).unwrap();
        assert_eq!(program(part.data).unwrap().stage, HwStage::Ps);
    }

    #[test]
    fn wave64_inference_uses_isa_signals_not_formatted_vcc_lo() {
        let meta = [0u8; 0x10];
        // Implicit-VCC VALU ops (v_cmp_lt_f32_e32 + v_cndmask_b32_e32) format as `vcc_lo` in
        // `text(false)`, yet must default to wave64=true.
        let implicit_vcc = words(&[0x7C02_0300, 0x0204_0903, ENDPGM]);
        assert!(infer(HwStage::Ps, &implicit_vcc, &meta).unwrap().0.wave64);

        // Wave64 saveexec_b64.
        let w64_saveexec = words(&[0xBE80_246A, ENDPGM]);
        assert!(infer(HwStage::Ps, &w64_saveexec, &meta).unwrap().0.wave64);

        // Wave64 split exec_lo + exec_hi moves (exec_hi wins over exec_lo).
        let w64_split = words(&[0xBE80_037E, 0xBE81_037F, ENDPGM]);
        assert!(infer(HwStage::Ps, &w64_split, &meta).unwrap().0.wave64);

        // Wave64 vcc_hi move.
        let w64_vcc_hi = words(&[0xBE80_036B, ENDPGM]);
        assert!(infer(HwStage::Ps, &w64_vcc_hi, &meta).unwrap().0.wave64);

        // Wave32 saveexec_b32 (does not name exec_lo/vcc_lo in text!).
        let w32_saveexec = words(&[0xBE80_3C01, ENDPGM]);
        assert!(!infer(HwStage::Ps, &w32_saveexec, &meta).unwrap().0.wave64);

        // Wave32 SOP touching exec_lo.
        let w32_exec_lo = words(&[0x877E_007E, ENDPGM]);
        assert!(!infer(HwStage::Ps, &w32_exec_lo, &meta).unwrap().0.wave64);

        // Wave32 saveexec_b32 with vcc_hi used as a scratch SGPR: strong wave32 wins.
        let w32_with_vcc_hi = words(&[0xBE80_3C01, 0xBEEB_0302, ENDPGM]);
        assert!(!infer(HwStage::Ps, &w32_with_vcc_hi, &meta).unwrap().0.wave64);
    }

    #[test]
    fn pixel_system_inputs_and_multiple_barycentrics_populate_isg1() {
        let meta = [0u8; 0x10];
        // Two barycentric pairs (v0..v3) + Position (v4..v7) + FrontFace (v8) + Ancillary (v9) +
        // SampleCoverage (v10); v20 is written via v_mov_b32 (not live-in!).
        let code = words(&[
            0xBEFC_0300, // s_mov_b32 m0, s0
            0xC830_0000, // v_interp_p1_f32 v12, v0, attr0.x
            0xC831_0001, // v_interp_p2_f32 v12, v1, attr0.x
            0xC834_0002, // v_interp_p1_f32 v13, v2, attr0.x
            0xC835_0003, // v_interp_p2_f32 v13, v3, attr0.x
            0x0618_090C, // v_add_f32 v12, v12, v4
            0x0618_110C, // v_add_f32 v12, v12, v8
            0x0618_130C, // v_add_f32 v12, v12, v9
            0x0618_150C, // v_add_f32 v12, v12, v10
            0x7E28_02F2, // v_mov_b32 v20, 1.0
            0xF800_180F,
            0x1414_0D0C, // exp mrt0 v12, v13, v20, v20 done vm
            ENDPGM,
        ]);
        let (p, (ins, _)) = infer(HwStage::Ps, &code, &meta).unwrap();
        let StageIo::Ps { input_addr, .. } = p.io else {
            panic!("not PS");
        };
        // Barycentrics bits 1 & 2 (v0..v3), plus bits 8..=14 (v4..v10).
        let want_addr = (1 << 1) | (1 << 2) | (0x7F << 8);
        assert_eq!(input_addr, want_addr);
        // ISG1 declares TEXCOORD0 + SV_Position + SV_IsFrontFace + SV_SampleIndex + SV_Coverage.
        let kinds: Vec<(u8, Option<u32>, u8)> =
            ins.iter().map(|e| (e.kind, e.row, e.cols)).collect();
        assert_eq!(
            kinds,
            [
                (0, Some(0), 1),
                (3, Some(1), 4),
                (13, Some(2), 1),
                (12, None, 1),
                (14, None, 1),
            ]
        );
    }

    #[test]
    fn entry_vgpr_read_before_interp_dst_overwrite_stays_live_in() {
        let meta = [0u8; 0x10];
        // v2 is read before v_interp_p1_f32 overwrites v2: must still be counted as Position.x.
        let code = words(&[
            0xD503_0004,
            0x0001_E502, // v_add_f32_e64 v4, v2, 1.0
            0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
            0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
            0xF800_180F,
            0x0404_0402, // exp mrt0 v2, v4, v4, v4 done vm
            ENDPGM,
        ]);
        let (p, (ins, _)) = infer(HwStage::Ps, &code, &meta).unwrap();
        let StageIo::Ps { input_addr, .. } = p.io else {
            panic!("not PS");
        };
        assert_eq!(input_addr, (1 << 1) | (1 << 8));
        assert!(ins.iter().any(|e| e.kind == 3 && e.name == "SV_Position"));
    }

    #[test]
    fn pops_pixel_shader_enables_db_shader_control() {
        let meta = [0u8; 0x10];
        let pops_const = words(&[0x8102_81EF, ENDPGM]); // s_add_i32 s2, src_pops_exiting_wave_id, 1
        let pops_msg = words(&[0xBF90_0007, ENDPGM]); // s_sendmsg sendmsg(MSG_ORDERED_PS_DONE)
        for code in [pops_const, pops_msg] {
            let (p, _) = infer(HwStage::Ps, &code, &meta).unwrap();
            let StageIo::Ps { db_shader_control, .. } = p.io else {
                panic!("not PS");
            };
            assert_eq!(db_shader_control, 1 << 16);
        }
        let (plain, _) = infer(HwStage::Ps, &pixel_code(), &meta).unwrap();
        let StageIo::Ps { db_shader_control, .. } = plain.io else {
            panic!("not PS");
        };
        assert_eq!(db_shader_control, 0);
    }

    #[test]
    fn vs_infers_heaps_avoids_vertex_table_collision_and_expands_constants() {
        // Metadata: Constants at s4 (slot 0), Table at s8 (slot 1) -> Constants expands to 4 SGPRs.
        let meta = meta_with(&[rec(2, 0, 4, 0), rec(0, 3, 8, 1)]);
        let code = words(&[
            0xBE8C_030A, // s_mov_b32 s12, s10
            0xBE8D_0380, // s_mov_b32 s13, 0
            0xF40C_0406,
            0x1200_0000, // s_load_dwordx8 s[16:23], s[12:13], s9
            0xBE8E_030B, // s_mov_b32 s14, s11
            0xBE8F_0380, // s_mov_b32 s15, 0
            0xF408_0607,
            0xFA00_0010, // s_load_dwordx4 s[24:27], s[14:15], 0x10
            0xF09C_0F08,
            0x00C4_0400, // image_sample_lz v[4:7], v[0:1], s[16:23], s[24:27] dmask:0xf dim:2D
            0xF800_08CF,
            0x0706_0504, // exp pos0 v4, v5, v6, v7 done
            ENDPGM,
        ]);
        let (p, _) = infer(HwStage::Vs, &code, &meta).unwrap();
        assert!(p.layout.iter().any(|r| r.root == RootType::Constants && r.user_data == 4 && r.sgprs == 4));
        assert!(p.layout.iter().any(|r| r.root == RootType::ResourceHeap && r.user_data == 10));
        assert!(p.layout.iter().any(|r| r.root == RootType::SamplerHeap && r.user_data == 11));
        assert!(p.layout.iter().any(|r| r.slot == VERTEX_TABLE && r.user_data == 0));

        // When metadata occupies s0, VERTEX_TABLE must not collide with s0, and an indexed vertex
        // fetch from s[2:3] infers VERTEX_TABLE at s[2:3].
        let meta_s0 = meta_with(&[rec(0, 3, 0, 0)]);
        let vs_vt2 = words(&[
            0xF408_0201,
            0xFA00_0000, // s_load_dwordx4 s[8:11], s[2:3], 0x0
            0xEA52_2000,
            0x8002_0100, // tbuffer_load_format_xyz v[1:3], v0, s[8:11], 0 format:32_32_32_FLOAT idxen
            0xF800_08CF,
            0x0303_0201, // exp pos0 v1, v2, v3, v3 done
            ENDPGM,
        ]);
        let (p2, _) = infer(HwStage::Vs, &vs_vt2, &meta_s0).unwrap();
        assert!(p2.layout.iter().any(|r| r.root == RootType::Table && r.user_data == 0));
        assert!(p2.layout.iter().any(|r| r.slot == VERTEX_TABLE && r.user_data == 2));
        assert!(!p2.layout.iter().any(|r| r.slot == VERTEX_TABLE && r.user_data == 0));

        // Genuinely unknown SGPR address (non-descriptor s_load_dword from s[6:7]) is NOT promoted
        // and still fails resource::track with "an address in an SGPR that is no root parameter".
        let unknown = words(&[
            0xF400_0203,
            0xFA00_0000, // s_load_dword s8, s[6:7], 0x0
            0xF800_08CF,
            0x0000_0000, // exp pos0 v0, v0, v0, v0 done
            ENDPGM,
        ]);
        let (p3, _) = infer(HwStage::Vs, &unknown, &meta_s0).unwrap();
        let ir = crate::ir::build(&p3.code, p3.wave64, p3.vgprs).unwrap();
        let err = crate::resource::track(&ir, &p3.layout, p3.stage, None).unwrap_err();
        assert_eq!(
            err,
            ShaderError::Unsupported {
                offset: 0,
                what: "an address in an SGPR that is no root parameter",
            }
        );
    }
}
