//! Descriptor heaps, textures and samplers on Metal: compute shaders (our own HLSL, compiled by dxc
//! and converted by the Metal Shader Converter) reach buffers and textures only through descriptor
//! tables in a heap. Each test runs with `MTLResidencySet` residency and with `useResource` fallback.

mod common;

use common::*;
use victory_metal::backend::{ComputeBackend, Dispatch, RootArgs};
use victory_metal::metal::Metal;
use victory_metal::resources::*;

const N: usize = 4096;

#[test]
fn srv_and_uav_through_a_descriptor_table() {
    for m in backends() {
        let p = m.create_compute_pipeline(&fixture("heap_rw")).unwrap();
        let a = Arena::new(&m, 2);
        let half = a.region.len / 2;
        a.write(0, &(0..N as u32).collect::<Vec<_>>());
        let heap = m.create_heap(HeapKind::Resource, 16).unwrap();
        // Entries 5 and 6 are the table the root signature points at: SRV t0, UAV u0.
        m.write_buffer(&heap, 5, &a.buf, a.at(0), (N * 4) as u32);
        m.write_buffer(&heap, 6, &a.buf, a.at(half), (N * 4) as u32);
        let w = [Dispatch {
            pipeline: &p,
            root: RootArgs::default()
                .constants(&[5, 7])
                .address(m.heap_address(&heap, 5)),
            resources: vec![],
            heaps: victory_metal::backend::Heaps {
                resource: Some(&heap),
                sampler: None,
            },
            groups: [N / 64, 1, 1],
            threads_per_group: [64, 1, 1],
        }];
        run(&m, &w);
        let out: Vec<u32> = a.read(half, N);
        assert!(
            out.iter().enumerate().all(|(i, v)| *v == i as u32 * 5 + 7),
            "dst[i] must be src[i]*5+7"
        );
        // Re-pointing an entry takes effect: swap SRV and UAV roles' memory by rewriting entry 5.
        a.write(0, &vec![1u32; N]);
        m.write_buffer(&heap, 5, &a.buf, a.at(0), (N * 4) as u32);
        run(&m, &w);
        assert!(a.read::<u32>(half, N).iter().all(|v| *v == 12));
    }
}

/// A 2D/array/3D texture sampled at the cell centres of a `g` grid; one float4 per cell.
struct Grid<'a> {
    m: &'a Metal,
    dxil: &'a [u8],
    g: [u32; 2],
    z: usize,
    filter: Filter,
}

impl Grid<'_> {
    fn run(&self, tex: &<Metal as ResourceBackend>::Texture) -> Vec<[f32; 4]> {
        let m = self.m;
        let p = m.create_compute_pipeline(self.dxil).unwrap();
        let cells = self.g[0] as usize * self.g[1] as usize * self.z;
        let a = Arena::new(
            m,
            (cells * 16)
                .div_ceil(victory_metal::mem::page_size())
                .max(1),
        );
        a.write(0, &vec![0xdead_beefu32; cells * 4]);
        let res = m.create_heap(HeapKind::Resource, 4).unwrap();
        let smp = m.create_heap(HeapKind::Sampler, 2).unwrap();
        m.write_texture(&res, 0, tex, 0.0);
        m.write_buffer(&res, 1, &a.buf, a.at(0), (cells * 16) as u32);
        let s = m
            .create_sampler(&SamplerDesc {
                min_mag: self.filter,
                mip: Filter::Nearest,
                address: [Address::Clamp; 3],
                max_anisotropy: 1,
            })
            .unwrap();
        m.write_sampler(&smp, 0, &s, 0.0);
        let w = [Dispatch {
            pipeline: &p,
            root: RootArgs::default()
                .constants(&self.g)
                .address(m.heap_address(&res, 0))
                .address(m.heap_address(&smp, 0)),
            resources: vec![],
            heaps: victory_metal::backend::Heaps {
                resource: Some(&res),
                sampler: Some(&smp),
            },
            groups: [
                self.g[0].div_ceil(4) as usize,
                self.g[1].div_ceil(4) as usize,
                self.z,
            ],
            threads_per_group: [4, 4, 1],
        }];
        run(m, &w);
        let f: Vec<f32> = a.read(0, cells * 4);
        f.chunks(4).map(|c| [c[0], c[1], c[2], c[3]]).collect()
    }
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.01
}

fn d2(f: Format, w: usize, h: usize) -> TextureDesc {
    TextureDesc::d2(f, w, h)
}

#[test]
fn sample_rgba8_2d() {
    for m in backends() {
        let t = m.create_texture(&d2(Format::Rgba8Unorm, 4, 4)).unwrap();
        let px: Vec<u8> = (0..16)
            .flat_map(|i| [(i % 4) as u8 * 60, (i / 4) as u8 * 60, 128, 255])
            .collect();
        m.upload_texture(&t, 0, 0, &px, 16, 0);
        let g = Grid {
            m: &m,
            dxil: &fixture("sample2d"),
            g: [4, 4],
            z: 1,
            filter: Filter::Nearest,
        };
        let out = g.run(&t);
        for (i, c) in out.iter().enumerate() {
            let want = [
                (i % 4) as f32 * 60.0 / 255.0,
                (i / 4) as f32 * 60.0 / 255.0,
                128.0 / 255.0,
                1.0,
            ];
            assert!(
                (0..4).all(|k| near(c[k], want[k])),
                "texel {i}: {c:?} != {want:?}"
            );
        }
    }
}

#[test]
fn sample_rgba16f_and_r32f() {
    let Some(m) = backend() else { return };
    // Exact half floats: 0 = 0x0000, 0.25 = 0x3400, 0.5 = 0x3800, 1 = 0x3C00.
    let h = [0x0000u16, 0x3400, 0x3800, 0x3c00];
    let t = m.create_texture(&d2(Format::Rgba16Float, 2, 2)).unwrap();
    let px: Vec<u16> = (0..4)
        .flat_map(|i| [h[i], h[(i + 1) % 4], h[(i + 2) % 4], 0x3c00])
        .collect();
    m.upload_texture(&t, 0, 0, bytes(&px), 16, 0);
    let g = Grid {
        m: &m,
        dxil: &fixture("sample2d"),
        g: [2, 2],
        z: 1,
        filter: Filter::Nearest,
    };
    let v = [0.0, 0.25, 0.5, 1.0];
    for (i, c) in g.run(&t).iter().enumerate() {
        assert_eq!(*c, [v[i], v[(i + 1) % 4], v[(i + 2) % 4], 1.0], "texel {i}");
    }

    let t = m.create_texture(&d2(Format::R32Float, 2, 2)).unwrap();
    m.upload_texture(&t, 0, 0, bytes(&[1.5f32, -2.0, 3.25, 4.0]), 8, 0);
    let out = g.run(&t);
    assert_eq!(
        out.iter().map(|c| c[0]).collect::<Vec<_>>(),
        [1.5, -2.0, 3.25, 4.0]
    );
}

fn bytes<T>(v: &[T]) -> &[u8] {
    // SAFETY: plain-old-data slices of ints and floats.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast(), std::mem::size_of_val(v)) }
}

#[test]
fn sample_bc1_and_create_every_bc_format() {
    let Some(m) = backend() else { return };
    // 8x8 BC1 = 2x2 blocks: red, green, blue, white (colour0 = RGB565, colour1 = 0, all indices 0).
    let blk = |c: u16| {
        let mut b = c.to_le_bytes().to_vec();
        b.extend([0u8; 6]);
        b
    };
    let data: Vec<u8> = [0xf800u16, 0x07e0, 0x001f, 0xffff]
        .iter()
        .flat_map(|c| blk(*c))
        .collect();
    let t = m.create_texture(&d2(Format::Bc1, 8, 8)).unwrap();
    m.upload_texture(&t, 0, 0, &data, Format::Bc1.row_bytes(8), 0);
    let g = Grid {
        m: &m,
        dxil: &fixture("sample2d"),
        g: [8, 8],
        z: 1,
        filter: Filter::Nearest,
    };
    let out = g.run(&t);
    let want = [
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
        [1.0, 1.0, 1.0],
    ];
    for (i, c) in out.iter().enumerate() {
        let w = want[(i / 8 / 4) * 2 + (i % 8) / 4];
        assert!(
            (0..3).all(|k| near(c[k], w[k])) && near(c[3], 1.0),
            "texel {i}: {c:?} != {w:?}"
        );
    }
    for f in [
        Format::Bc1,
        Format::Bc2,
        Format::Bc3,
        Format::Bc4,
        Format::Bc5,
        Format::Bc6h,
        Format::Bc7,
    ] {
        let t = m
            .create_texture(&d2(f, 16, 16))
            .unwrap_or_else(|e| panic!("{f:?}: {e}"));
        m.upload_texture(
            &t,
            0,
            0,
            &vec![0u8; f.row_bytes(16) * 4],
            f.row_bytes(16),
            0,
        );
    }
}

#[test]
fn sample_2d_array_and_3d() {
    let Some(m) = backend() else { return };
    let mut d = d2(Format::Rgba8Unorm, 4, 4);
    d.dim = TexDim::D2Array;
    d.depth_or_layers = 3;
    let t = m.create_texture(&d).unwrap();
    for s in 0..3u8 {
        let px: Vec<u8> = (0..16)
            .flat_map(|i| [s * 100, (i % 4) as u8 * 60, (i / 4) as u8 * 60, 255])
            .collect();
        m.upload_texture(&t, 0, s as usize, &px, 16, 0);
    }
    let g = Grid {
        m: &m,
        dxil: &fixture("sample2darray"),
        g: [4, 4],
        z: 3,
        filter: Filter::Nearest,
    };
    for (i, c) in g.run(&t).iter().enumerate() {
        let (s, x, y) = (i / 16, i % 4, (i / 4) % 4);
        let want = [
            s as f32 * 100.0 / 255.0,
            x as f32 * 60.0 / 255.0,
            y as f32 * 60.0 / 255.0,
        ];
        assert!(
            (0..3).all(|k| near(c[k], want[k])),
            "slice {s} texel {x},{y}: {c:?}"
        );
    }

    let mut d = d2(Format::Rgba8Unorm, 4, 4);
    d.dim = TexDim::D3;
    d.depth_or_layers = 4;
    let t = m.create_texture(&d).unwrap();
    let px: Vec<u8> = (0..64)
        .flat_map(|i| {
            [
                (i % 4) as u8 * 60,
                ((i / 4) % 4) as u8 * 60,
                (i / 16) as u8 * 60,
                255,
            ]
        })
        .collect();
    m.upload_texture(&t, 0, 0, &px, 16, 64);
    let g = Grid {
        m: &m,
        dxil: &fixture("sample3d"),
        g: [4, 4],
        z: 4,
        filter: Filter::Nearest,
    };
    for (i, c) in g.run(&t).iter().enumerate() {
        let (x, y, z) = (i % 4, (i / 4) % 4, i / 16);
        let want = [
            x as f32 * 60.0 / 255.0,
            y as f32 * 60.0 / 255.0,
            z as f32 * 60.0 / 255.0,
        ];
        assert!(
            (0..3).all(|k| near(c[k], want[k])),
            "voxel {x},{y},{z}: {c:?}"
        );
    }
}

#[test]
fn texture_views_and_sampler_filters() {
    let Some(m) = backend() else { return };
    // A 4x4 texture with 3 mips: mip 0 all 0, mip 1 (2x2) all 200, mip 2 all 50. A view of mip 1 only.
    let mut d = d2(Format::Rgba8Unorm, 4, 4);
    d.mips = 3;
    let t = m.create_texture(&d).unwrap();
    for (mip, (w, v)) in [(4usize, 0u8), (2, 200), (1, 50)].into_iter().enumerate() {
        m.upload_texture(&t, mip, 0, &vec![v; w * w * 4], w * 4, 0);
    }
    let view = m
        .create_texture_view(
            &t,
            &ViewDesc {
                format: Format::Rgba8Unorm,
                mips: (1, 1),
                slices: (0, 1),
            },
        )
        .unwrap();
    let g = Grid {
        m: &m,
        dxil: &fixture("sample2d"),
        g: [2, 2],
        z: 1,
        filter: Filter::Nearest,
    };
    assert!(
        g.run(&view).iter().all(|c| near(c[0], 200.0 / 255.0)),
        "view of mip 1"
    );

    // The same bytes read as sRGB: 128 decodes to ~0.2158 instead of 0.502.
    let t = m.create_texture(&d2(Format::Rgba8Unorm, 2, 2)).unwrap();
    m.upload_texture(&t, 0, 0, &[128u8; 16], 8, 0);
    let srgb = m
        .create_texture_view(
            &t,
            &ViewDesc {
                format: Format::Rgba8UnormSrgb,
                mips: (0, 1),
                slices: (0, 1),
            },
        )
        .unwrap();
    assert!(g.run(&t).iter().all(|c| near(c[0], 0.502)));
    assert!(g.run(&srgb).iter().all(|c| near(c[0], 0.2158)));

    // Filters: a 2x2 texture, red 0 | 255 (both rows), sampled on a 4x4 grid. Nearest only ever
    // returns the two texel values; linear blends between the texel centres.
    let t = m.create_texture(&d2(Format::Rgba8Unorm, 2, 2)).unwrap();
    let px = [0u8, 0, 0, 255, 255, 0, 0, 255, 0, 0, 0, 255, 255, 0, 0, 255];
    m.upload_texture(&t, 0, 0, &px, 8, 0);
    let run = |filter| {
        Grid {
            m: &m,
            dxil: &fixture("sample2d"),
            g: [4, 4],
            z: 1,
            filter,
        }
        .run(&t)
    };
    assert!(
        run(Filter::Nearest)
            .iter()
            .all(|c| c[0] == 0.0 || c[0] == 1.0)
    );
    let lin = run(Filter::Linear);
    assert!(
        lin.iter().any(|c| c[0] > 0.1 && c[0] < 0.9),
        "linear must blend: {lin:?}"
    );
}

#[test]
fn texture_over_zero_copy_memory() {
    let Some(m) = backend() else { return };
    let a = Arena::new(&m, 2);
    let align = m.linear_alignment(Format::Rgba8Unorm);
    let pitch = 16usize.next_multiple_of(align);
    // The texture starts one alignment unit into the arena, like a title resource at an offset.
    let base = align;
    let px = |v: u8| -> Vec<u8> {
        (0..4)
            .flat_map(|r| {
                (0..pitch).map(move |c| {
                    if c < 16 {
                        v + r as u8 + (c / 4) as u8
                    } else {
                        0
                    }
                })
            })
            .collect()
    };
    a.write(base, &px(10));
    let t = m
        .create_texture_over_memory(&a.buf, a.at(base), &d2(Format::Rgba8Unorm, 4, 4), pitch)
        .unwrap();
    let g = Grid {
        m: &m,
        dxil: &fixture("sample2d"),
        g: [4, 4],
        z: 1,
        filter: Filter::Nearest,
    };
    let check = |v: u8| {
        for (i, c) in g.run(&t).iter().enumerate() {
            let want = (v as usize + i / 4 + i % 4) as f32 / 255.0;
            assert!(near(c[0], want), "texel {i}: {} != {want}", c[0]);
        }
    };
    check(10);
    // A CPU write into the title memory after the texture exists is seen by the next dispatch.
    a.write(base, &px(60));
    check(60);
}
