//! The pool-offset contract on Metal ([`super::addr`]): synthetic GFX10 programs of our own,
//! assembled with `llvm-mc -arch=amdgcn -mcpu=gfx1030 -mattr=+wavefrontsize64`, translated with the
//! host's options, converted by Apple's Metal Shader Converter with the host's root layout and run on
//! the GPU. They read and write title memory at addresses they compute (a global load, a scalar load
//! through a pointer loaded from memory, global stores through an SGPR base and through a VGPR pair,
//! an atomic), at title addresses no CPU pointer is at: the pool is shown to the title at addresses
//! the process never mapped, and the results are read from the pool's own mapping. Skipped without
//! the Metal Shader Converter.

use crate::native::list::NativeList;
use crate::native::memory::{PoolInfo, PoolObserver};
use crate::native::shaders;
use crate::native::state::Graphics;
use crate::native::title::{Pipe, Title};
use std::sync::Arc;
use victory_backend::resources::{Format, HeapKind, ResourceBackend, TextureDesc, Topology};
use victory_metal::mem::Alias;
use victory_metal::metal::Metal;
use victory_shader::emit::{HostShader, emit_with};
use victory_shader::native::{ComputeIo, HwStage, NativeProgram, RootParam, RootType, StageIo};
use victory_shader::{Container, io, ir, resource, structure};

/// Every lane: `dst[tid] = src[tid] + **(src + 0x1000)`; the same again at `dst + 1024` through a
/// 64-bit VGPR address; `dst[+2040] += 1`. `s[0:1]` is `src`, `s[2:3]` is `dst`.
const COMPUTED: &[u32] = &[
    0x3402_0082, // v_lshlrev_b32_e32 v1, 2, v0
    0xDC30_8000,
    0x0200_0001, // global_load_dword v2, v1, s[0:1]
    0xF404_0100,
    0xFA00_1000, // s_load_dwordx2 s[4:5], s[0:1], 0x1000
    0xBF8C_0070, // s_waitcnt vmcnt(0) lgkmcnt(0)
    0xF400_0182,
    0xFA00_0000, // s_load_dword s6, s[4:5], 0x0
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x4A04_0406, // v_add_nc_u32_e32 v2, s6, v2
    0xDC70_8000,
    0x0002_0201, // global_store_dword v1, v2, s[2:3]
    0x7E0C_0202, // v_mov_b32_e32 v6, s2
    0x7E0E_0203, // v_mov_b32_e32 v7, s3
    0xD70F_6A06,
    0x0002_0306, // v_add_co_u32 v6, vcc, v6, v1
    0x500E_0E80, // v_add_co_ci_u32_e32 v7, vcc, 0, v7, vcc
    0xDC70_8400,
    0x007D_0206, // global_store_dword v[6:7], v2, off offset:1024
    0x7E10_0280, // v_mov_b32_e32 v8, 0
    0x7E12_0281, // v_mov_b32_e32 v9, 1
    0xDCC8_87F8,
    0x0002_0908, // global_atomic_add v8, v9, s[2:3] offset:2040
    0xBF81_0000, // s_endpgm
];

/// `dst[tid] = src[tid] + 0x55` stored to `s[2:3]`, and the same value stored to `s[4:5]`, where
/// the pointers may point at memory no pool shows.
const MISSES: &[u32] = &[
    0x3402_0082, // v_lshlrev_b32_e32 v1, 2, v0
    0xDC30_8000,
    0x0200_0001, // global_load_dword v2, v1, s[0:1]
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0x4A04_04FF,
    0x0000_0055, // v_add_nc_u32_e32 v2, 0x55, v2
    0xDC70_8000,
    0x0002_0201, // global_store_dword v1, v2, s[2:3]
    0xDC70_8000,
    0x0004_0201, // global_store_dword v1, v2, s[4:5]
    0xBF81_0000, // s_endpgm
];

/// A 64-lane compute shader whose user SGPRs `s0..s<sgprs>` are root constants, translated the way
/// the host translates, from machine code of our own in a container of no parts.
fn shader(words: &[u32], sgprs: u32) -> HostShader {
    translate(words, sgprs, shaders::options())
}

fn translate(words: &[u32], sgprs: u32, options: victory_shader::emit::Options) -> HostShader {
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 16,
        layout: vec![RootParam {
            root: RootType::Constants,
            table_offset_dwords: 0,
            sgprs,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            rsrc2: sgprs << 1,
            threads: [64, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, &native).expect("io::raise");
    // A container of no parts: `DXBC`, a digest, version 1.0, its size, no parts.
    let mut empty = b"DXBC".to_vec();
    empty.extend([0; 16]);
    empty.extend([1, 0, 0, 0]);
    empty.extend(32u32.to_le_bytes());
    empty.extend(0u32.to_le_bytes());
    let c = Container::parse(&empty).expect("container");
    emit_with(&c, &native, &p, &st, &io, &res, options).expect("emit")
}

fn title() -> Option<Title<Metal>> {
    if !std::path::Path::new(victory_metal::msc::DEFAULT_DYLIB).exists()
        && std::env::var_os("VICTORY_MSC_DYLIB").is_none()
    {
        eprintln!("skipped: Metal Shader Converter not installed");
        return None;
    }
    Some(Title::new(
        Metal::new().unwrap(),
        Arc::new(crate::native::memory::PoolTable::default()),
    ))
}

/// Title addresses nothing in this process is mapped at.
const VA_A: u64 = 0x12_3450_0000;
const VA_B: u64 = 0x7000_0000;
const POOL: usize = 0x10_0000;
const CANARY: u32 = 0xEEEE_EEEE;

/// A pool of 1 MiB filled with a canary, shown at `VA_A` (512 KiB from pool offset 0x2_0000) and
/// `VA_B` (128 KiB from the pool's start).
struct Fixture {
    mem: Alias,
    info: PoolInfo,
}

impl Fixture {
    fn new(t: &Title<Metal>) -> Fixture {
        let mem = Alias::new(POOL).unwrap();
        let info = PoolInfo {
            id: 1,
            base: mem.primary as usize,
            len: POOL,
            memory_entry: None,
        };
        let f = Fixture { mem, info };
        f.put(0, &[CANARY; POOL / 4]);
        let m = t.host.memory();
        m.pool_created(&f.info);
        m.mapped(VA_A as usize, 0x8_0000, &f.info, 0x2_0000);
        m.mapped(VA_B as usize, 0x2_0000, &f.info, 0);
        f
    }

    fn put<T: Copy>(&self, at: usize, v: &[T]) {
        // SAFETY: inside the pool's own mapping; the GPU is idle.
        unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), self.mem.primary.add(at).cast(), v.len()) };
    }

    fn get(&self, at: usize, n: usize) -> Vec<u32> {
        // SAFETY: inside the pool's own mapping; read after the fence wait.
        unsafe { std::slice::from_raw_parts(self.mem.primary.add(at).cast::<u32>(), n).to_vec() }
    }
}

fn dispatch(t: &mut Title<Metal>, shader: &HostShader, root: &[u32], heap: bool) {
    let Pipe::Compute(pso) = t.compute_pipe(shader, "addr") else {
        panic!("not made: {:?}", t.shaders)
    };
    assert_eq!(pso.threads, [64, 1, 1]);
    let mut l = NativeList::default();
    l.set_compute_pipeline(&pso);
    l.set_compute_root_constants(0, root);
    if heap {
        // The title's own resource heap, 8 views of 32 bytes of its descriptor memory.
        let slots = vec![0u32; 8 * 8];
        let h = t.host.heap(HeapKind::Resource, slots.as_ptr() as u64, 8).unwrap();
        l.set_heaps(Some(&h), None);
    }
    l.dispatch(1, 1, 1).unwrap();
    super::run(&t.host, &[&l]);
}

fn halves(v: u64) -> [u32; 2] {
    [v as u32, (v >> 32) as u32]
}

/// Expectations of [`COMPUTED`] with `src` at pool offset `src_at`, `dst` at `dst_at`.
fn check_computed(f: &Fixture, src_at: usize, dst_at: usize) {
    let first = f.get(dst_at, 64);
    let second = f.get(dst_at + 1024, 64);
    for i in 0..64u32 {
        let want = i * 3 + 1 + 0xBEEF;
        assert_eq!(first[i as usize], want, "dst[{i}]");
        assert_eq!(second[i as usize], want, "dst2[{i}]");
    }
    assert_eq!(f.get(dst_at + 2040, 1), [64], "the atomic");
    // Nothing else was written: the source, the pointer's target and the stretch between.
    assert_eq!(f.get(src_at, 64), (0..64).map(|i| i * 3 + 1).collect::<Vec<u32>>());
    assert_eq!(f.get(dst_at + 256, 192), [CANARY; 192]);
    assert_eq!(f.get(dst_at + 1280, 190), [CANARY; 190]);
    assert_eq!(f.get(dst_at + 2044, 1), [CANARY]);
}

fn prepare_computed(f: &Fixture, src_at: usize, dst_at: usize) {
    f.put(src_at, &(0..64u32).map(|i| i * 3 + 1).collect::<Vec<_>>());
    // At src + 0x1000, a pointer to VA_B + 0x100, where there is 0xBEEF.
    f.put(src_at + 0x1000, &[VA_B + 0x100]);
    f.put(0x100, &[0xBEEFu32]);
    f.put(dst_at + 2040, &[0u32]);
}

#[test]
fn computed_addresses_read_and_write_the_pool_at_its_title_addresses() {
    let Some(mut t) = title() else { return };
    let f = Fixture::new(&t);
    let cs = shader(COMPUTED, 4);
    // The translation as the host asks for it: the page table and nothing to mark.
    assert!(cs.memory && !cs.memory_writes && !cs.view_addresses && !cs.heap);
    prepare_computed(&f, 0x2_0000, 0x6_0000);
    let [a, b] = halves(VA_A);
    let [c, d] = halves(VA_A + 0x4_0000);
    dispatch(&mut t, &cs, &[a, b, c, d], false);
    check_computed(&f, 0x2_0000, 0x6_0000);
    assert_eq!(t.shaders.refusals(), 0, "{:?}", t.shaders.refused);
}

#[test]
fn guessed_fragment_buffer_reads_the_pool_and_checks_record_bounds() {
    let vertex = [
        0x7D84_0081, // v_cmp_eq_u32 vcc, 1, v0
        0xD501_0001, 0x01A9_FEF3, 0x4040_0000, // v_cndmask_b32 v1, -1.0, 3.0, vcc
        0x7D84_0082, // v_cmp_eq_u32 vcc, 2, v0
        0xD501_0002, 0x01A9_FEF3, 0x4040_0000, // v_cndmask_b32 v2, -1.0, 3.0, vcc
        0x7E06_0280, // v_mov_b32 v3, 0
        0x7E08_02F2, // v_mov_b32 v4, 1.0
        0xF800_08CF, 0x0403_0201, // exp pos0 v1, v2, v3, v4 done
        0xBF81_0000, // s_endpgm
    ];
    let pixel = [
        0x8004_9000, // s_add_u32 s4, s0, 16
        0x8205_8080, // s_addc_u32 s5, 0, 0
        0x8805_FF05, 0x0010_0000, // s_or_b32 s5, s5, 0x100000
        0xBE86_0382, // s_mov_b32 s6, 2
        0xBE87_03FF, 0x0001_4FAC, // s_mov_b32 s7, 0x14fac
        0x7E00_0281, // v_mov_b32 v0, 1
        0xE030_2004, 0x8001_0200, // buffer_load_dword v2, v0, s[4:7], 0 idxen offset:4
        0xE030_2010, 0x8001_0300, // buffer_load_dword v3, v0, s[4:7], 0 idxen offset:16
        0x7E00_0282, // v_mov_b32 v0, 2
        0xE030_2000, 0x8001_0400, // buffer_load_dword v4, v0, s[4:7], 0 idxen
        0xF420_0202, 0xFA00_0004, // s_buffer_load_dword s8, s[4:7], 4
        0xBF8C_0070, // s_waitcnt vmcnt(0) lgkmcnt(0)
        0x7E0A_0208, // v_mov_b32 v5, s8
        0xF800_180F, 0x0504_0302, // exp mrt0 v2, v3, v4, v5 done vm
        0xBF81_0000, // s_endpgm
    ];
    let translate = |stage, words: &[u32], meta: &[u8]| {
        let bytes: Vec<_> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let c = victory_shader::emit::graphics::container(stage, &bytes, meta).unwrap();
        shaders::translate(&c).unwrap()
    };
    let vs = translate(HwStage::Vs, &vertex, &[0; 0x10]);
    let mut meta = [0; 0x30];
    meta[12..15].copy_from_slice(&[0x40, 0, 1]);
    meta[0x10] = 0x41;
    meta[0x14] = 2; // Root constants in slot 0.
    let ps = translate(HwStage::Ps, &pixel, &meta);
    use victory_metal::msc::{DEFAULT_DYLIB, Msc};
    if !std::path::Path::new(DEFAULT_DYLIB).exists() && std::env::var_os("VICTORY_MSC_DYLIB").is_none() {
        eprintln!("skipped: Metal Shader Converter not installed");
        return;
    }
    let layout = crate::native::list::Binding::of(&ps).layout().unwrap();
    Msc::open_default().convert_with(&ps.dxil, None, Some(&layout)).unwrap();
    let Ok(metal) = Metal::new() else {
        eprintln!("skipped the draw: no Metal device");
        return;
    };
    let mut t = Title::new(metal, Arc::new(crate::native::memory::PoolTable::default()));
    let f = Fixture::new(&t);
    f.put(20, &[1.0f32]); // base + 4
    f.put(36, &[0.5f32]); // base + 1 * stride + 4
    f.put(48, &[1.0f32]); // backing memory at the address of both out-of-bounds reads

    let state = Graphics::guess_base()
        .for_draw(vec![Format::Rgba8Unorm], None, vec![], &[], false);
    let Pipe::Render(pso, binding) = t.render_pipe(&vs, Some(&ps), &state, "computed fragment") else {
        panic!("{:?}", t.shaders);
    };
    let mut desc = TextureDesc::d2(Format::Rgba8Unorm, 8, 8);
    desc.render_target = true;
    let color = t.host.texture(&desc).unwrap();
    let mut list = NativeList::default();
    list.set_render_targets(std::slice::from_ref(&color), None);
    list.set_graphics_pipeline(&pso, Topology::Triangles);
    list.set_graphics_binding(binding);
    list.set_graphics_root_constants(0, &[VA_B as u32]);
    list.draw_instanced(3, 1, 0, 0).unwrap();
    super::run(&t.host, &[&list]);
    let mut pixels = [0; 8 * 8 * 4];
    t.host.backend.read_texture(&color, 0, 0, &mut pixels, 8 * 4, 0);
    assert_eq!(&pixels[(4 * 8 + 4) * 4..][..4], [128, 0, 0, 255]);
}

/// The table follows the title's maps: the same dispatch, the same title addresses, after the title
/// mapped the pages somewhere else.
#[test]
fn the_table_follows_remaps_and_a_title_heap_serves_as_well() {
    let Some(mut t) = title() else { return };
    let f = Fixture::new(&t);
    let cs = shader(COMPUTED, 4);
    prepare_computed(&f, 0x2_0000, 0x6_0000);
    let [a, b] = halves(VA_A);
    let [c, d] = halves(VA_A + 0x4_0000);
    dispatch(&mut t, &cs, &[a, b, c, d], true);
    check_computed(&f, 0x2_0000, 0x6_0000);

    // VA_A now shows the pool from 0x4_0000: source at 0x4_0000, destination at 0x8_0000.
    t.host.memory().mapped(VA_A as usize, 0x8_0000, &f.info, 0x4_0000);
    f.put(0, &[CANARY; POOL / 4]);
    prepare_computed(&f, 0x4_0000, 0x8_0000);
    dispatch(&mut t, &cs, &[a, b, c, d], true);
    check_computed(&f, 0x4_0000, 0x8_0000);
    // The old destination was left alone.
    assert_eq!(f.get(0x6_0000, 64), [CANARY; 64]);
    assert_eq!(t.shaders.pipelines, 1);
}

/// Memory no pool shows reads as zero and its writes are dropped, in a page past a mapping, in a
/// 256 MiB region with no mapping at all and in the pool's own range below it; none of it lands in
/// the pool where a naive offset from the mapping would put it.
#[test]
fn memory_no_pool_shows_reads_zero_and_drops_writes() {
    let Some(mut t) = title() else { return };
    let f = Fixture::new(&t);
    let cs = shader(MISSES, 6);
    assert!(cs.memory && !cs.memory_writes);
    // dst is mapped; src and the store target are the page after the mapping (pool 0xA_0000 if the
    // mapping went on) and a region of its own.
    for (src, wild) in [(VA_A + 0x8_0000, VA_A + 0x9_0000), (0x55_0000_0040, 0x7F_FFFF_0000)] {
        f.put(0, &[CANARY; POOL / 4]);
        let [a, b] = halves(src);
        let [c, d] = halves(VA_A + 0x4_0000);
        let [e, g] = halves(wild);
        dispatch(&mut t, &cs, &[a, b, c, d, e, g], false);
        assert_eq!(f.get(0x6_0000, 64), [0x55; 64]);
        // Everything but the 64 words stored is as it was.
        assert_eq!(f.get(0x6_0100, 64), [CANARY; 64]);
        assert_eq!(f.get(0xA_0000, 64), [CANARY; 64]);
        assert_eq!(f.get(0xB_0000, 64), [CANARY; 64]);
        assert_eq!(f.get(0, 64), [CANARY; 64]);
    }
}

/// Shaders the translator marks pages in (translated without the host's option) are refused, not
/// run with a bitmap nobody binds.
#[test]
fn a_shader_that_marks_pages_is_refused() {
    let Some(mut t) = title() else { return };
    let mut cs = shader(COMPUTED, 4);
    cs.memory_writes = true;
    assert!(matches!(t.compute_pipe(&cs, "marking"), Pipe::Refused));
    assert!(t.shaders.refused.iter().any(|(why, _)| why.contains("marks the pages")));
}

/// `zero_copy_memory` is what removes the bitmap and its atomics; D3D12's translation has both.
#[test]
fn only_the_zero_copy_option_drops_the_page_marks() {
    let d3d12 = translate(COMPUTED, 4, victory_shader::emit::Options::default());
    assert!(d3d12.memory && d3d12.memory_writes);
    let host = shader(COMPUTED, 4);
    assert!(host.memory && !host.memory_writes);
    assert_ne!(d3d12.dxil, host.dxil);
}
