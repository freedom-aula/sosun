//! Mesh-style (NGG) vertex programs of guessed pipelines on the native host: hand-assembled GFX10
//! translated as mesh shaders, made into Metal mesh pipelines and drawn.

use super::addr_tests::title;
use crate::native::list::{Binding, NativeList};
use crate::native::shaders;
use crate::native::state::Graphics;
use crate::native::title::{Pipe, Title};
use std::rc::Rc;
use victory_backend::resources::{Format, GraphicsBackend, ResourceBackend, TextureDesc, Topology};
use victory_metal::metal::Metal;
use victory_shader::emit::HostShader;
use victory_shader::native::HwStage;

type Texture = <Metal as ResourceBackend>::Texture;

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
    let mut meta = [0; 0x10];
    meta[12] = 0x40;
    translate_with(stage, words, &meta)
}

fn translate_with(stage: HwStage, words: &[u32], meta: &[u8]) -> HostShader {
    let bytes: Vec<_> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let c = victory_shader::emit::graphics::container(stage, &bytes, meta).unwrap();
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

/// An 8x8 RGBA8 target cleared to black, `pso` and `binding` set on a list.
fn list_into(
    t: &Title<Metal>,
    pso: &Rc<<Metal as GraphicsBackend>::RenderPipeline>,
    binding: Option<Rc<Binding>>,
) -> (Rc<Texture>, NativeList<Metal>) {
    let mut desc = TextureDesc::d2(Format::Rgba8Unorm, 8, 8);
    desc.render_target = true;
    let color = t.host.texture(&desc).unwrap();
    let mut list = NativeList::default();
    list.set_render_targets(std::slice::from_ref(&color), None);
    list.clear_render_target(&color, [0.0; 4]);
    list.set_graphics_pipeline(pso, Topology::Triangles);
    list.set_graphics_binding(binding);
    (color, list)
}

fn pixels(t: &Title<Metal>, color: &Texture) -> Vec<[u8; 4]> {
    let mut p = [0; 8 * 8 * 4];
    t.host.backend.read_texture(color, 0, 0, &mut p, 8 * 4, 0);
    p.chunks(4).map(|c| c.try_into().unwrap()).collect()
}

/// `DispatchMesh` of one group of the triangle program colours the whole target.
#[test]
fn a_mesh_draw_colours_the_target() {
    let (ms, ps) = (translate(HwStage::Vs, TRIANGLE), translate(HwStage::Ps, RED));
    let Some(mut t) = title() else { return };
    let state = Graphics::guess_base().for_draw(vec![Format::Rgba8Unorm], None, vec![], &[], false);
    let Pipe::Render(pso, binding) = t.render_pipe(&ms, Some(&ps), &state, "mesh") else {
        panic!("{:?}", t.shaders);
    };
    let (color, mut list) = list_into(&t, &pso, binding);
    list.dispatch_mesh(1, 1, 1).unwrap();
    super::run(&t.host, &[&list]);
    assert!(pixels(&t, &color).iter().all(|p| *p == [255, 128, 128, 255]), "{:?}", pixels(&t, &color));
    assert_eq!(t.shaders.refusals(), 0, "{:?}", t.shaders.refused);
}

/// The mesh stage gets its root arguments: the triangle program with its corners scaled by the
/// root constant in user data dword 0 (s8 of a mesh program), 1.0. Without them it reads 0 and
/// draws nothing.
#[test]
fn a_mesh_shader_reads_its_root_constants() {
    let mut words = TRIANGLE.to_vec();
    let at = words.iter().position(|&w| w == 0x7E08_0280).unwrap(); // before v_mov_b32 v4, 0
    words.splice(at..at, [
        0x1004_0408, // v_mul_f32 v2, s8, v2
        0x1006_0608, // v_mul_f32 v3, s8, v3
    ]);
    let mut meta = [0; 0x30];
    meta[12..15].copy_from_slice(&[0x40, 0, 1]);
    meta[0x10] = 0x41;
    meta[0x14] = 2; // Root constants in slot 0, user data dword 0.
    let ms = translate_with(HwStage::Vs, &words, &meta);
    let ps = translate(HwStage::Ps, RED);
    let Some(mut t) = title() else { return };
    let state = Graphics::guess_base().for_draw(vec![Format::Rgba8Unorm], None, vec![], &[], false);
    let Pipe::Render(pso, binding) = t.render_pipe(&ms, Some(&ps), &state, "mesh") else {
        panic!("{:?}", t.shaders);
    };
    let (color, mut list) = list_into(&t, &pso, binding);
    list.set_graphics_root_constants(0, &[1.0f32.to_bits()]);
    list.dispatch_mesh(1, 1, 1).unwrap();
    super::run(&t.host, &[&list]);
    assert!(pixels(&t, &color).iter().all(|p| *p == [255, 128, 128, 255]), "{:?}", pixels(&t, &color));
}

/// `ExecuteIndirect` of `DispatchMesh` with a count buffer: the record (one group) and the count
/// (1) are read on the GPU, so record 0 colours the target and record 1 of the same count, not
/// below it, leaves it black.
#[test]
fn a_counted_indirect_mesh_dispatch_runs_only_the_records_below_the_count() {
    let (ms, ps) = (translate(HwStage::Vs, TRIANGLE), translate(HwStage::Ps, RED));
    let Some(mut t) = title() else { return };
    let state = Graphics::guess_base().for_draw(vec![Format::Rgba8Unorm], None, vec![], &[], false);
    let Pipe::Render(pso, binding) = t.render_pipe(&ms, Some(&ps), &state, "mesh") else {
        panic!("{:?}", t.shaders);
    };
    let pool = super::Pool::new(&t.host, 1);
    let (records, count) = (0, 64);
    pool.write(records, &[1u32, 1, 1, 1, 1, 1]);
    pool.write(count, &[1u32]);
    for (i, want) in [(0, [255, 128, 128, 255]), (1, [0; 4])] {
        let (color, mut list) = list_into(&t, &pso, binding.clone());
        list.dispatch_mesh_indirect(pool.va(records + 12 * i as usize), Some((pool.va(count), i)))
            .unwrap();
        super::run(&t.host, &[&list]);
        assert!(pixels(&t, &color).iter().all(|p| *p == want), "record {i}: {:?}", pixels(&t, &color));
    }
}
