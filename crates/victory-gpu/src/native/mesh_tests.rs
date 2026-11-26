//! Mesh-style (NGG) vertex programs of guessed pipelines on the native host: hand-assembled GFX10
//! translated as mesh shaders, made into Metal mesh pipelines and drawn.

use super::addr_tests::title;
use crate::native::shaders;
use crate::native::state::Graphics;
use crate::native::title::Pipe;
use victory_backend::resources::Format;
use victory_shader::emit::HostShader;
use victory_shader::native::HwStage;

/// A mesh-style vertex program as the title's start (lane 0 alone takes the group id, shifted by
/// 7: groups of 128 lanes), whose lanes 0-2 export the corners of a triangle covering the target
/// and whose lane 0 exports primitive (0, 1, 2).
const TRIANGLE: &[u32] = &[
    0xBEFE_0481, // s_mov_b64 exec, 1
    0x2C0A_0A87, // v_lshrrev_b32 v5, 7, v5
    0xBEFE_04C1, // s_mov_b64 exec, -1
    0xD765_0001, 0x0201_00C1, // v_mbcnt_lo_u32_b32 v1, -1, 0
    0xD766_0001, 0x0202_02C1, // v_mbcnt_hi_u32_b32 v1, -1, v1
    0x7E14_02F3, // v_mov_b32 v10, -1.0
    0x7E16_02FF, 0x4040_0000, // v_mov_b32 v11, 3.0
    0x7D84_0281, // v_cmp_eq_u32 vcc, 1, v1
    0x0204_170A, // v_cndmask_b32 v2, v10, v11, vcc
    0x7D84_0282, // v_cmp_eq_u32 vcc, 2, v1
    0x0206_170A, // v_cndmask_b32 v3, v10, v11, vcc
    0x7E08_0280, // v_mov_b32 v4, 0
    0x7E0C_02F2, // v_mov_b32 v6, 1.0
    0xF800_08CF, 0x0604_0302, // exp pos0 v2, v3, v4, v6 done
    0x7E0E_02FF, 0x0020_0400, // v_mov_b32 v7, 0x200400 (corners 0, 1, 2)
    0x7DA4_0280, // v_cmpx_eq_u32 0, v1
    0xF800_0941, 0x0000_0007, // exp prim v7 done
    0xBF81_0000, // s_endpgm
];

/// Writes (1.0, 0.5, 0.5, 1.0) to mrt0.
const RED: &[u32] = &[
    0x7E00_02F2, // v_mov_b32 v0, 1.0
    0x7E02_02F0, // v_mov_b32 v1, 0.5
    0xF800_180F, 0x0001_0100, // exp mrt0 v0, v1, v1, v0 done vm
    0xBF81_0000, // s_endpgm
];

fn translate(stage: HwStage, words: &[u32]) -> HostShader {
    let bytes: Vec<_> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut meta = [0; 0x10];
    meta[12] = 0x40;
    let c = victory_shader::emit::graphics::container(stage, &bytes, &meta).unwrap();
    shaders::translate(&c).unwrap()
}

/// A guessed pipeline whose vertex program is mesh-style becomes a Metal mesh pipeline, with the
/// thread-group size the converter reflects for its mesh shader.
#[test]
fn a_mesh_style_vertex_program_makes_a_mesh_pipeline() {
    let ms = translate(HwStage::Vs, TRIANGLE);
    assert_eq!(ms.stage, HwStage::Ms);
    let ps = translate(HwStage::Ps, RED);
    let Some(mut t) = title() else { return };
    let state = Graphics::guess_base().for_draw(vec![Format::Rgba8Unorm], None, vec![], &[], false);
    let Pipe::Render(pso, _) = t.render_pipe(&ms, Some(&ps), &state, "mesh") else {
        panic!("{:?}", t.shaders);
    };
    assert_eq!(pso.mesh_threads(), Some([128, 1, 1]));
}
