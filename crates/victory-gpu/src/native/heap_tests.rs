//! Guessed graphics shaders that take descriptors from the resource heap themselves (an SGPR the
//! translator infers as a `ResourceHeap` base, not a table): hand-assembled GFX10 drawn on Metal,
//! with the title's heap bound for the vertex, mesh and pixel stages.

use super::addr_tests::title;
use crate::native::list::{NativeHeap, NativeList};
use crate::native::shaders;
use crate::native::state::Graphics;
use crate::native::title::{Pipe, Title};
use std::rc::Rc;
use victory_backend::resources::*;
use victory_metal::metal::Metal;
use victory_shader::emit::HostShader;
use victory_shader::native::HwStage;

fn translate(stage: HwStage, words: &[u32]) -> HostShader {
    let mut meta = [0; 0x10];
    meta[12] = 0x40;
    let bytes: Vec<_> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let c = victory_shader::emit::graphics::container(stage, &bytes, &meta).unwrap();
    shaders::translate(&c).unwrap()
}

/// A title resource heap of two 1x1 RGBA8 textures: entry 0 red, entry 1 green. A shader that
/// reads entry 1 sees green; one that read entry 0 (or nothing) would not.
struct Heap {
    heap: Rc<NativeHeap<Metal>>,
    _records: Vec<u32>,
}

impl Heap {
    fn new(t: &Title<Metal>) -> Heap {
        let records = vec![0u32; 8 * 2];
        let start = records.as_ptr() as u64;
        let heap = t.host.heap(HeapKind::Resource, start, 2).unwrap();
        for (i, texel) in [[255, 0, 0, 255], [0, 255, 0, 255]].into_iter().enumerate() {
            let tex = t.host.texture(&TextureDesc::d2(Format::Rgba8Unorm, 1, 1)).unwrap();
            t.host.backend.upload_texture(&tex, 0, 0, &texel, 4, 0);
            let key = 0x4EA9_0000 + i;
            t.host.register_texture(key, &tex);
            // DXGI_FORMAT_R8G8B8A8_UNORM, TEXTURE2D, one mip.
            let mut srv = [0u32; 10];
            (srv[0], srv[1], srv[5]) = (28, 4, 1);
            let slot = start + 32 * i as u64;
            crate::descriptor::create_srv(std::ptr::null(), key as _, &srv, slot);
            assert!(t.host.mirror(slot).unwrap());
        }
        Heap { heap, _records: records }
    }
}

/// Draws `vs` and `ps` as a guessed pipeline into an 8x8 RGBA8 target cleared to black, with the
/// heap bound and `record` issuing the draw; the target's pixels.
fn draw(
    t: &mut Title<Metal>,
    heap: &Heap,
    vs: &HostShader,
    ps: &HostShader,
    record: impl FnOnce(&mut NativeList<Metal>),
) -> Vec<[u8; 4]> {
    let state = Graphics::guess_base().for_draw(vec![Format::Rgba8Unorm], None, vec![], &[], false);
    let Pipe::Render(pso, binding) = t.render_pipe(vs, Some(ps), &state, "heap") else {
        panic!("{:?}", t.shaders);
    };
    let mut desc = TextureDesc::d2(Format::Rgba8Unorm, 8, 8);
    desc.render_target = true;
    let color = t.host.texture(&desc).unwrap();
    let mut list = NativeList::default();
    list.set_render_targets(std::slice::from_ref(&color), None);
    list.clear_render_target(&color, [0.0; 4]);
    list.set_heaps(Some(&heap.heap), None);
    list.set_graphics_pipeline(&pso, Topology::Triangles);
    list.set_graphics_binding(binding);
    record(&mut list);
    super::run(&t.host, &[&list]);
    let mut p = [0; 8 * 8 * 4];
    t.host.backend.read_texture(&color, 0, 0, &mut p, 8 * 4, 0);
    assert_eq!(t.shaders.refusals(), 0, "{:?}", t.shaders.refused);
    p.chunks(4).map(|c| c.try_into().unwrap()).collect()
}

/// A vertex shader whose vertices 0-2 are the corners of a triangle covering the target.
const COVER: &[u32] = &[
    0x7E14_02F3, // v_mov_b32 v10, -1.0
    0x7E16_02FF, 0x4040_0000, // v_mov_b32 v11, 3.0
    0x7D84_0081, // v_cmp_eq_u32 vcc, 1, v0
    0x0204_170A, // v_cndmask_b32 v2, v10, v11, vcc
    0x7D84_0082, // v_cmp_eq_u32 vcc, 2, v0
    0x0206_170A, // v_cndmask_b32 v3, v10, v11, vcc
    0x7E08_0280, // v_mov_b32 v4, 0
    0x7E0C_02F2, // v_mov_b32 v6, 1.0
    0xF800_08CF, 0x0604_0302, // exp pos0 v2, v3, v4, v6 done
    0xBF81_0000, // s_endpgm
];

/// A pixel shader that loads the image descriptor at heap entry 1 (byte 0x20 from the heap base in
/// s2, high half 0) and writes its texel (0, 0).
const PS_HEAP: &[u32] = &[
    0xBE83_0380, // s_mov_b32 s3, 0
    0xF40C_0101, 0xFA00_0020, // s_load_dwordx8 s[4:11], s[2:3], 0x20
    0x7E00_0280, // v_mov_b32 v0, 0
    0x7E02_0280, // v_mov_b32 v1, 0
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xF000_1F08, 0x0001_0000, // image_load v[0:3], v[0:1], s[4:11] dmask:0xf dim:2D unorm
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0xF800_180F, 0x0302_0100, // exp mrt0 v0, v1, v2, v3 done vm
    0xBF81_0000, // s_endpgm
];

/// A pixel shader indexing the heap itself is not refused, and reads the entry it names.
#[test]
fn a_pixel_shader_loads_a_texture_by_heap_index() {
    let (vs, ps) = (translate(HwStage::Vs, COVER), translate(HwStage::Ps, PS_HEAP));
    assert!(ps.heap && !vs.heap);
    let Some(mut t) = title() else { return };
    let heap = Heap::new(&t);
    let pixels = draw(&mut t, &heap, &vs, &ps, |l| l.draw_instanced(3, 1, 0, 0).unwrap());
    assert!(pixels.iter().all(|p| *p == [0, 255, 0, 255]), "{pixels:?}");
}

/// Loads the image descriptor at heap entry 1 (heap base in `base`, high half 0) into s[12:19] and
/// its texel (0, 0) into v[12:15], then scales the position in v2, v3 by its green channel: 1.0
/// for entry 1, 0.0 (no area, nothing drawn) for entry 0 or an unbound heap.
fn scaled_by_heap(base: u8) -> Vec<u32> {
    let b = u32::from(base);
    vec![
        0xBE80_0380 | (b + 1) << 16, // s_mov_b32 s[base+1], 0
        0xF40C_0300 | b >> 1, 0xFA00_0020, // s_load_dwordx8 s[12:19], s[base:base+1], 0x20
        0x7E10_0280, // v_mov_b32 v8, 0
        0x7E12_0280, // v_mov_b32 v9, 0
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xF000_1F08, 0x0003_0C08, // image_load v[12:15], v[8:9], s[12:19] dmask:0xf dim:2D unorm
        0xBF8C_3F70, // s_waitcnt vmcnt(0)
        0x1004_050D, // v_mul_f32 v2, v13, v2
        0x1006_070D, // v_mul_f32 v3, v13, v3
    ]
}

/// `words` with `scaled_by_heap(base)` before its `v_mov_b32 v4, 0` (after the corners).
fn with_heap_scale(words: &[u32], base: u8) -> Vec<u32> {
    let mut words = words.to_vec();
    let at = words.iter().position(|&w| w == 0x7E08_0280).unwrap();
    words.splice(at..at, scaled_by_heap(base));
    words
}

/// Writes (1.0, 0.5, 0.5, 1.0) to mrt0.
const RED: &[u32] = &[
    0x7E00_02F2, // v_mov_b32 v0, 1.0
    0x7E02_02F0, // v_mov_b32 v1, 0.5
    0xF800_180F, 0x0001_0100, // exp mrt0 v0, v1, v1, v0 done vm
    0xBF81_0000, // s_endpgm
];

/// A vertex shader indexing the heap itself (base in s2) reads the entry it names.
#[test]
fn a_vertex_shader_loads_a_texture_by_heap_index() {
    let vs = translate(HwStage::Vs, &with_heap_scale(COVER, 2));
    let ps = translate(HwStage::Ps, RED);
    assert!(vs.heap && vs.stage == HwStage::Vs);
    let Some(mut t) = title() else { return };
    let heap = Heap::new(&t);
    let pixels = draw(&mut t, &heap, &vs, &ps, |l| l.draw_instanced(3, 1, 0, 0).unwrap());
    assert!(pixels.iter().all(|p| *p == [255, 128, 128, 255]), "{pixels:?}");
}

/// A mesh shader indexing the heap itself (base in s10, user data 2 of a mesh program) reads the
/// entry it names: the heap is bound for the mesh stage.
#[test]
fn a_mesh_shader_loads_a_texture_by_heap_index() {
    let ms = translate(HwStage::Vs, &with_heap_scale(super::mesh_tests::TRIANGLE, 10));
    let ps = translate(HwStage::Ps, RED);
    assert!(ms.heap && ms.stage == HwStage::Ms);
    let Some(mut t) = title() else { return };
    let heap = Heap::new(&t);
    let pixels = draw(&mut t, &heap, &ms, &ps, |l| l.dispatch_mesh(1, 1, 1).unwrap());
    assert!(pixels.iter().all(|p| *p == [255, 128, 128, 255]), "{pixels:?}");
}

/// A texture the title releases leaves the heap: the entries showing it are cleared, where they
/// used to keep the host texture alive and resident until the title wrote another view there.
/// Entries of other textures stay, and a texture only forgotten (another description placed at
/// its address) keeps its entries.
#[test]
fn a_released_texture_leaves_the_heap() {
    let Some(t) = title() else { return };
    let heap = Heap::new(&t);
    let (red, green) = (0x4EA9_0000, 0x4EA9_0001);
    t.host.forget_resource(red);
    assert_eq!(t.host.cleared(), 0, "forgetting a texture leaves its entries");
    t.host.register_texture(red, &t.host.texture(&TextureDesc::d2(Format::Rgba8Unorm, 1, 1)).unwrap());
    t.host.release_resource(green);
    assert_eq!(t.host.cleared(), 1);
    assert!(!t.host.knows(green) && t.host.knows(red));
    assert!(heap.heap.mirrored.borrow().contains_key(&1), "its old record is not mirrored again");
    assert!(heap.heap.take_entries(green).is_empty() && heap.heap.take_entries(red) == [0]);
    // Released again, or a buffer: nothing to clear.
    t.host.release_resource(green);
    assert_eq!(t.host.cleared(), 1);
    // A texture at the released one's address: the entry is checked again, the other is not.
    t.host.register_texture(green, &t.host.texture(&TextureDesc::d2(Format::Rgba8Unorm, 1, 1)).unwrap());
    assert!(!heap.heap.mirrored.borrow().contains_key(&1) && heap.heap.mirrored.borrow().contains_key(&0));
}
