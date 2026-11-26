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
//!   perspective-centre barycentrics (when it interpolates), then position, front face,
//!   ancillary, coverage; its colour exports `exp mrt<n>` are targets of register n, in float
//!   (a compressed export is two FP16 values per dword);
//! * the wave size is 32 when the code names `exec_lo` or `vcc_lo`, else 64.

use super::container::{SigElement, signature};
use crate::ShaderError::{self, Malformed};
use crate::dxbc::{self, FourCC, u32_at};
use crate::gfx10::{Fields, Operand};
use crate::native::{
    HwStage, NativeProgram, PsInput, RootParam, RootType, StageIo, Usage, VsOutput,
    metadata_resources,
};

/// The part a graphics piece pair is carried in: `"FSPG"`, u32 stage (0 vertex, 1 pixel), u32
/// code size, u32 metadata size, then the code and the metadata.
pub const MAGIC: &[u8] = b"FSPG";

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
    let (stage, code_len, meta_len) = (u32_at(part, 4)?, u32_at(part, 8)? as usize, u32_at(part, 12)? as usize);
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

fn infer(stage: HwStage, code: &[u8], meta: &[u8]) -> Result<(NativeProgram, Signatures), ShaderError> {
    if !matches!(stage, HwStage::Vs | HwStage::Ps) {
        return Err(ShaderError::Unsupported { offset: 0, what: "a graphics stage but vertex and pixel" })
    }
    let wave64 = !crate::gfx10::decode(code)?
        .iter()
        .any(|i| i.text(false).contains("exec_lo") || i.text(false).contains("vcc_lo"));
    let list = crate::gfx10::decode(code)?;
    let (mut layout, used) = metadata_resources(meta, RECORDS, true)?;
    // The sampler heap's address follows the last user data of the title's root signature by one
    // dword (observed: the pixel shaders' root constants of the sampler index at s9, the heap at
    // s11, the primitive mask at s12).
    if used.iter().any(|u| u.root == RootType::Constants) && stage == HwStage::Ps {
        let end = layout.iter().map(|p| u32::from(p.user_data) + p.sgprs).max().unwrap_or(0);
        layout.push(RootParam {
            root: RootType::SamplerHeap,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: (end + 1) as u8,
            slot: 254,
        });
    }
    // A vertex shader's own vertex fetch: s0/s1 address the table of vertex buffer descriptors it
    // loads from (observed: `s_load_dwordx4 sN, s[0:1]` before its `buffer_load`).
    if stage == HwStage::Vs {
        layout.push(RootParam {
            root: RootType::Descriptor,
            table_offset_dwords: 0,
            sgprs: 2,
            base_register: 0,
            user_data: 0,
            slot: VERTEX_TABLE,
        });
    }
    // The registers the program names: its VGPR count, and what it reads on entry.
    let p = crate::ir::build(code, wave64, 256)?;
    let top = p
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .flat_map(|i| i.reads.iter().chain(&i.writes))
        .filter_map(|r| match r.0 {
            crate::ir::Reg::V(n) => Some(u16::from(n) + 1),
            _ => None,
        })
        .max()
        .unwrap_or(1);
    let vgprs = top.next_multiple_of(4);
    // The interpolation instructions read their destination (the IR's tie), not an input.
    let interp_dst: Vec<u8> = list
        .iter()
        .filter_map(|i| match i.fields(wave64) {
            Fields::Vintrp { dst: Operand::Vector { reg, .. }, .. } => Some(reg),
            _ => None,
        })
        .collect();
    let live_in = p
        .inputs
        .iter()
        .filter_map(|r| match r.0 {
            crate::ir::Reg::V(n) if !interp_dst.contains(&n) => Some(u16::from(n) + 1),
            _ => None,
        })
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
            .map(|&(n, _)| VsOutput { usage: Usage::Generic, row: n, param: n, fp16: false, pack2: false })
            .collect();
        (StageIo::Vs { inputs: Vec::new(), outputs }, (Vec::new(), outs))
    } else {
        let ins: Vec<SigElement> = attrs
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
        // Entry VGPRs, as `SPI_PS_INPUT_ADDR` enables them: perspective centre (bit 1, two
        // VGPRs) when it interpolates, then the system values from position x (bit 8) on.
        let mut addr = 0u32;
        let mut left = i32::from(live_in);
        if interpolates {
            addr |= 1 << 1;
            left -= 2;
        }
        for bit in 8..=14 {
            if left > 0 {
                addr |= 1 << bit;
                left -= 1;
            }
        }
        (
            StageIo::Ps { inputs, input_addr: addr, db_shader_control: 0, patches: Vec::new() },
            (ins, outs),
        )
    };
    Ok((NativeProgram { stage, wave64, vgprs, layout, used, io, code: code.to_vec() }, sigs))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENDPGM: u32 = 0xBF81_0000;

    /// `v_interp_p1_f32 v2, v0, attr0.x`, `v_interp_p2_f32 v2, v1, attr0.x`, then `exp mrt0` of
    /// v2 in x and `s_endpgm`: a pixel shader of one attribute and one colour.
    fn pixel_code() -> Vec<u8> {
        [0xC808_0000u32, 0xC809_0001, 0xF800_0801, 0x0000_0002, ENDPGM]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect()
    }

    #[test]
    fn a_pixel_shaders_attributes_and_exports_are_inferred() {
        let meta = [0u8; 0x10];
        let (p, (ins, outs)) = infer(HwStage::Ps, &pixel_code(), &meta).unwrap();
        let StageIo::Ps { inputs, input_addr, .. } = &p.io else { panic!("not a pixel program") };
        assert_eq!((inputs.len(), inputs[0].attr, inputs[0].mask), (1, 0, 1));
        // Perspective-centre barycentrics only.
        assert_eq!(*input_addr, 2);
        assert_eq!(ins.len(), 1);
        assert_eq!((outs[0].name.as_str(), outs[0].kind, outs[0].row), ("SV_Target", 16, Some(0)));
        let c = container(HwStage::Ps, &pixel_code(), &meta).unwrap();
        let c = crate::Container::parse(&c).unwrap();
        assert!(c.part(FourCC::ISG1).is_some() && c.part(FourCC::OSG1).is_some());
        let part = c.part(FourCC::ACCC).unwrap();
        assert_eq!(program(part.data).unwrap().stage, HwStage::Ps);
    }
}
