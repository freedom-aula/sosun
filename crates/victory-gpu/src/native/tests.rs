//! victory-gpu on Metal, end to end, with command streams of our own (the HLSL under
//! `victory-metal/tests/fixtures`, our own data): the calls a title list receives, recorded into a
//! [`NativeList`], submitted through [`Host`], read back from title memory. Title memory is a pool
//! (`Alias::primary`, what the GPU buffer wraps) shown to the "title" at a second address
//! (`Alias::alias`) through the pool table, so every address the list names is translated.
//! Skipped when Apple's Metal Shader Converter is not installed.

use super::Host;
use super::list::NativeList;
use super::memory::{PoolInfo, PoolObserver};
use std::rc::Rc;
use victory_backend::backend::ComputeBackend;
use victory_backend::resources::*;
use victory_metal::mem::{Alias, page_size};
use victory_metal::metal::Metal;

fn host() -> Option<Host> {
    if !std::path::Path::new(victory_metal::msc::DEFAULT_DYLIB).exists()
        && std::env::var_os("VICTORY_MSC_DYLIB").is_none()
    {
        eprintln!("skipped: Metal Shader Converter not installed");
        return None;
    }
    Some(Host::new(Metal::new().unwrap()))
}

fn fixture(name: &str) -> Vec<u8> {
    let p = format!(
        "{}/../victory-metal/tests/fixtures/{name}.dxil",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}"))
}

/// A pool of `pages` pages that the title sees at `alias`; GPU buffers wrap `primary`.
struct Pool {
    mem: Alias,
}

impl Pool {
    fn new(h: &Host, pages: usize) -> Pool {
        let mem = Alias::new(pages * page_size()).unwrap();
        let info = PoolInfo {
            id: 1,
            base: mem.primary as usize,
            len: mem.len,
            memory_entry: None,
        };
        h.memory().pool_created(&info);
        h.memory().mapped(mem.alias as usize, mem.len, &info, 0);
        Pool { mem }
    }

    /// The title's address of byte `at`.
    fn va(&self, at: usize) -> u64 {
        self.mem.alias as u64 + at as u64
    }

    fn write<T: Copy>(&self, at: usize, v: &[T]) {
        // SAFETY: inside the mapping; the GPU is idle while the test writes.
        unsafe {
            std::ptr::copy_nonoverlapping(v.as_ptr(), self.mem.alias.add(at).cast(), v.len())
        };
    }

    fn read<T: Copy>(&self, at: usize, n: usize) -> Vec<T> {
        // SAFETY: inside the mapping; read after the fence wait.
        unsafe { std::slice::from_raw_parts(self.mem.alias.add(at).cast::<T>(), n).to_vec() }
    }
}

fn run(h: &Host, lists: &[&NativeList<Metal>]) {
    let q = h.backend.create_queue().unwrap();
    let f = h.backend.create_fence(0).unwrap();
    h.execute(&q, lists, &[], &[(&f, 1)]).unwrap();
    assert!(h.backend.wait(&f, 1, 10_000), "GPU did not finish");
}

const N: usize = 1024;

/// dst[i] = src[i] * 5 + 7, root views to title memory; the source arrives through a GPU copy and
/// the destination is cleared first with a GPU fill, all in one list.
#[test]
fn compute_dispatch_writes_title_memory() {
    let Some(h) = host() else { return };
    let pool = Pool::new(&h, 4);
    let (staged, src, dst) = (0, 16384, 32768);
    pool.write(staged, &(0..N as u32).collect::<Vec<_>>());
    pool.write(dst, &[0xEEEE_EEEEu32; N]);
    let pso = h.compute_pipeline(&fixture_affine(), [64, 1, 1]).unwrap();

    let mut l = NativeList::default();
    l.copy_buffer_region(pool.va(src), pool.va(staged), N * 4);
    l.fill(pool.va(dst), N * 4, 0);
    l.set_compute_pipeline(&pso);
    l.set_compute_root_constants(0, &[5, 7]);
    l.set_compute_root_view(1, pool.va(src));
    l.set_compute_root_view(2, pool.va(dst));
    l.dispatch((N / 64) as u32, 1, 1).unwrap();
    run(&h, &[&l]);

    let out: Vec<u32> = pool.read(dst, N);
    assert!(
        out.iter().enumerate().all(|(i, v)| *v == i as u32 * 5 + 7),
        "{:?}",
        &out[..8]
    );
}

fn fixture_affine() -> Vec<u8> {
    let p = format!(
        "{}/../victory-metal/tests/affine.dxil",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(p).unwrap()
}

/// Descriptors the way a title writes them: `CreateShaderResourceView`-family device calls leave a
/// record in 32-byte heap slots, which `mirror` decodes into the backend's heap; the dispatch finds
/// its buffers through a descriptor table. Two submissions on one queue, the second waiting on the
/// first's fence value (no CPU wait between).
#[test]
fn descriptor_table_buffers_decode_into_metal_heap() {
    let Some(h) = host() else { return };
    let pool = Pool::new(&h, 4);
    let (src, dst) = (0, 16384);
    pool.write(src, &(0..64u32).collect::<Vec<_>>());
    let slots = vec![0u32; 8 * 8]; // 8 views of 32 bytes: the title's descriptor memory
    let start = slots.as_ptr() as u64;
    let heap = h.heap(HeapKind::Resource, start, 8).unwrap();

    // CreateShaderResourceView / UnorderedAccessView of memory the title placed itself (raw buffers).
    let placed = |at: usize| {
        let mut d = [0u32; 16];
        d[1] = 1; // buffer view
        d[6] = 64; // elements
        d[8] = 1; // raw (SRV flags word)
        d[10] = pool.va(at) as u32;
        d[11] = (pool.va(at) >> 32) as u32;
        d
    };
    let mut resource = [0u32; 14];
    resource[0] = 1; // buffer
    crate::descriptor::create_placed_srv(std::ptr::null_mut(), &resource, &placed(src), start);
    let mut uav = placed(dst);
    (uav[4], uav[6]) = (64, 0); // UAV words: element count at 4, stride at 5, flags at 8
    crate::descriptor::create_placed_uav(std::ptr::null_mut(), &resource, &uav, start + 32);
    assert!(h.mirror(start).unwrap() && h.mirror(start + 32).unwrap());

    let pso = h.compute_pipeline(&fixture("heap_rw"), [64, 1, 1]).unwrap();
    let mut l = NativeList::default();
    l.set_heaps(Some(&heap), None);
    l.set_compute_pipeline(&pso);
    l.set_compute_root_constants(0, &[3, 1]);
    l.set_compute_root_table(1, &heap, 0);
    l.dispatch(1, 1, 1).unwrap();
    // A second list on the same queue reuses the output as its input through a root view.
    let aff = h.compute_pipeline(&fixture_affine(), [64, 1, 1]).unwrap();
    let mut l2 = NativeList::default();
    l2.set_compute_pipeline(&aff);
    l2.set_compute_root_constants(0, &[2, 0]);
    l2.set_compute_root_view(1, pool.va(dst));
    l2.set_compute_root_view(2, pool.va(dst + 4096));
    l2.dispatch(1, 1, 1).unwrap();

    let q = h.backend.create_queue().unwrap();
    let (a, b) = (
        h.backend.create_fence(0).unwrap(),
        h.backend.create_fence(0).unwrap(),
    );
    h.execute(&q, &[&l], &[], &[(&a, 1)]).unwrap();
    h.execute(&q, &[&l2], &[(&a, 1)], &[(&b, 1)]).unwrap();
    assert!(h.backend.wait(&b, 1, 10_000));
    assert_eq!(
        pool.read::<u32>(dst, 64),
        (0..64).map(|i| i * 3 + 1).collect::<Vec<_>>()
    );
    assert_eq!(
        pool.read::<u32>(dst + 4096, 64),
        (0..64).map(|i| (i * 3 + 1) * 2).collect::<Vec<_>>()
    );
    drop(slots);
}

type Vertex = [f32; 8];

fn vertex(x: f32, y: f32, c: [f32; 4]) -> Vertex {
    [x, y, 0.5, 0.5, c[0], c[1], c[2], c[3]]
}

/// Clear a render target, draw a triangle with a texture and sampler reached through descriptor
/// tables (written the title's way, then decoded), and read the pixels back.
#[test]
fn clear_and_draw_into_render_target() {
    let Some(h) = host() else { return };
    const W: usize = 16;
    let pool = Pool::new(&h, 2);
    pool.write(
        0,
        &[
            vertex(-0.5, -0.5, [1.0, 0.0, 0.0, 1.0]),
            vertex(0.5, -0.5, [1.0, 0.0, 0.0, 1.0]),
            vertex(0.0, 0.5, [1.0, 0.0, 0.0, 1.0]),
        ],
    );

    let attrs = [
        VertexAttr {
            format: VertexFormat::Float2,
            slot: 0,
            offset: 0,
        },
        VertexAttr {
            format: VertexFormat::Float2,
            slot: 0,
            offset: 8,
        },
        VertexAttr {
            format: VertexFormat::Float4,
            slot: 0,
            offset: 16,
        },
    ];
    let pso = h
        .render_pipeline(&RenderPipelineDesc {
            vs: &fixture("tri_vs"),
            ps: Some(&fixture("tri_ps")),
            attributes: &attrs,
            streams: &[VertexStream {
                slot: 0,
                stride: 32,
                per_instance: false,
            }],
            color: &[Format::Rgba8Unorm],
            depth: None,
            depth_test: None,
            cull: Cull::None,
        })
        .unwrap();
    let mut rt = TextureDesc::d2(Format::Rgba8Unorm, W, W);
    rt.render_target = true;
    let color = h.texture(&rt).unwrap();
    let white = h
        .texture(&TextureDesc::d2(Format::Rgba8Unorm, 1, 1))
        .unwrap();
    h.backend.upload_texture(&white, 0, 0, &[255; 4], 4, 0);
    h.register_texture(0xA11CE, &white);

    let views = vec![0u32; 8 * 4];
    let smp = vec![0u32; 4 * 4];
    let (vstart, sstart) = (views.as_ptr() as u64, smp.as_ptr() as u64);
    let vheap = h.heap(HeapKind::Resource, vstart, 4).unwrap();
    let sheap = h.heap(HeapKind::Sampler, sstart, 4).unwrap();
    // CreateShaderResourceView(texture, Texture2D, mips 0..1) and CreateSamplerX (bilinear, clamp).
    let mut srv = [0u32; 10];
    (srv[0], srv[1], srv[5]) = (28, 4, 1);
    crate::descriptor::create_srv(std::ptr::null_mut(), 0xA11CE as _, &srv, vstart);
    let mut sw = [0u32; 27];
    (sw[1], sw[2], sw[6], sw[7], sw[8]) = (1, 1, 3, 3, 3);
    crate::descriptor::create_sampler_x(std::ptr::null_mut(), &sw, sstart);
    assert!(h.mirror(vstart).unwrap() && h.mirror(sstart).unwrap());

    let mut l = NativeList::default();
    l.set_render_targets(std::slice::from_ref(&color), None);
    l.clear_render_target(&color, [0.0, 0.0, 1.0, 1.0]);
    l.set_graphics_pipeline(&pso, Topology::Triangles);
    l.set_heaps(Some(&vheap), Some(&sheap));
    l.set_graphics_root_constants(0, &[0.5f32.to_bits(), 0, 0, 0]);
    l.set_graphics_root_table(1, &vheap, 0);
    l.set_graphics_root_table(2, &sheap, 0);
    l.set_vertex_buffer(0, pool.va(0));
    l.draw_instanced(3, 1, 0, 0).unwrap();
    run(&h, &[&l]);

    let mut out = vec![0u8; W * W * 4];
    h.backend.read_texture(&color, 0, 0, &mut out, W * 4, 0);
    let px = |x: usize, y: usize| <[u8; 4]>::try_from(&out[(y * W + x) * 4..][..4]).unwrap();
    assert_eq!(px(8, 8), [255, 0, 0, 255], "inside the triangle");
    assert_eq!(px(0, 0), [0, 0, 255, 255], "cleared corner");
    assert_eq!(px(15, 15), [0, 0, 255, 255], "cleared corner");
    let _ = Rc::strong_count(&color);
    drop((views, smp));
}

/// An address outside every mapped pool is an error at submission, not a GPU fault.
#[test]
fn unmapped_title_address_is_refused() {
    let Some(h) = host() else { return };
    let pso = h.compute_pipeline(&fixture_affine(), [64, 1, 1]).unwrap();
    let mut l = NativeList::default();
    l.set_compute_pipeline(&pso);
    l.set_compute_root_constants(0, &[1, 0]);
    l.set_compute_root_view(1, 0xDEAD_0000);
    l.set_compute_root_view(2, 0xDEAD_1000);
    l.dispatch(1, 1, 1).unwrap();
    let q = h.backend.create_queue().unwrap();
    let err = h.execute(&q, &[&l], &[], &[]).unwrap_err();
    assert!(err.contains("not in a mapped pool"), "{err}");
}
