//! Graphics pipeline state on the native host: blend, write mask, stencil, culling and winding,
//! viewport and scissor, as a title's pipeline and list calls set them, drawn with our own HLSL
//! (`victory-metal/tests/fixtures/tri_*`) into render targets and read back.

use super::{Pool, fixture, host, run};
use crate::native::Host;
use crate::native::list::NativeList;
use std::rc::Rc;
use victory_backend::resources::*;
use victory_metal::metal::Metal;

type Vertex = [f32; 8];
const W: usize = 16;

fn vertex(x: f32, y: f32, c: [f32; 4]) -> Vertex {
    [x, y, 0.5, 0.5, c[0], c[1], c[2], c[3]]
}

fn triangle(c: [f32; 4]) -> [Vertex; 3] {
    [vertex(-0.5, -0.5, c), vertex(0.5, -0.5, c), vertex(0.0, 0.5, c)]
}

const ATTRS: [VertexAttr; 3] = [
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

const STREAMS: [VertexStream; 1] = [VertexStream {
    slot: 0,
    stride: 32,
    per_instance: false,
}];

/// A host, a white texture and a sampler in heaps, and a way to make pipelines of the triangle
/// shaders with a given state.
struct Rig {
    h: Host,
    pool: Pool,
    vheap: Rc<crate::native::list::NativeHeap<Metal>>,
    sheap: Rc<crate::native::list::NativeHeap<Metal>>,
    _slots: (Vec<u32>, Vec<u32>),
}

impl Rig {
    fn new() -> Option<Rig> {
        let h = host()?;
        let pool = Pool::new(&h, 2);
        let white = h
            .texture(&TextureDesc::d2(Format::Rgba8Unorm, 1, 1))
            .unwrap();
        h.backend.upload_texture(&white, 0, 0, &[255; 4], 4, 0);
        let key = 0x57A7E;
        h.register_texture(key, &white);
        let views = vec![0u32; 8 * 4];
        let smp = vec![0u32; 4 * 4];
        let (vstart, sstart) = (views.as_ptr() as u64, smp.as_ptr() as u64);
        let vheap = h.heap(HeapKind::Resource, vstart, 4).unwrap();
        let sheap = h.heap(HeapKind::Sampler, sstart, 4).unwrap();
        let mut srv = [0u32; 10];
        (srv[0], srv[1], srv[5]) = (28, 4, 1);
        crate::descriptor::create_srv(std::ptr::null(), key as _, &srv, vstart);
        let mut sw = [0u32; 27];
        (sw[1], sw[2], sw[6], sw[7], sw[8]) = (1, 1, 3, 3, 3);
        crate::descriptor::create_sampler_x(std::ptr::null(), &sw, sstart);
        assert!(h.mirror(vstart).unwrap() && h.mirror(sstart).unwrap());
        Some(Rig {
            h,
            pool,
            vheap,
            sheap,
            _slots: (views, smp),
        })
    }

    fn pipeline(
        &self,
        depth: Option<Format>,
        color_state: &[ColorState],
        configure: impl FnOnce(&mut RenderPipelineDesc<'_>),
    ) -> Rc<<Metal as GraphicsBackend>::RenderPipeline> {
        let (vs, ps) = (fixture("tri_vs"), fixture("tri_ps"));
        let mut desc = RenderPipelineDesc {
            vs: &vs,
            ps: Some(&ps),
            vs_layout: None,
            ps_layout: None,
            attributes: &ATTRS,
            streams: &STREAMS,
            color: &[Format::Rgba8Unorm],
            depth,
            cull: Cull::None,
            color_state,
            ..Default::default()
        };
        configure(&mut desc);
        self.h.render_pipeline(&desc).unwrap()
    }

    fn target(&self, format: Format, depth: bool) -> Rc<<Metal as ResourceBackend>::Texture> {
        let mut d = TextureDesc::d2(format, W, W);
        d.render_target = true;
        let _ = depth;
        self.h.texture(&d).unwrap()
    }

    /// A list that clears `color` to `clear` and draws each vertex buffer (at pool offset) with
    /// `pso`, with whatever `state` sets first.
    fn list(
        &self,
        color: &Rc<<Metal as ResourceBackend>::Texture>,
        depth: Option<&Rc<<Metal as ResourceBackend>::Texture>>,
        clear: [f32; 4],
    ) -> NativeList<Metal> {
        let mut l = NativeList::default();
        l.set_render_targets(std::slice::from_ref(color), depth);
        l.clear_render_target(color, clear);
        if let Some(d) = depth {
            l.clear_depth_stencil(d, Some(1.0), Some(0));
        }
        l.set_heaps(Some(&self.vheap), Some(&self.sheap));
        l.set_graphics_root_constants(0, &[0.5f32.to_bits(), 0, 0, 0]);
        l.set_graphics_root_table(1, &self.vheap, 0);
        l.set_graphics_root_table(2, &self.sheap, 0);
        l
    }

    fn draw(
        &self,
        l: &mut NativeList<Metal>,
        pso: &Rc<<Metal as GraphicsBackend>::RenderPipeline>,
        at: usize,
        verts: &[Vertex],
    ) {
        self.pool.write(at, verts);
        l.set_graphics_pipeline(pso, Topology::Triangles);
        l.set_vertex_buffer(0, self.pool.va(at));
        l.draw_instanced(verts.len() as u32, 1, 0, 0).unwrap();
    }

    fn read(&self, color: &Rc<<Metal as ResourceBackend>::Texture>) -> Vec<u8> {
        let mut out = vec![0u8; W * W * 4];
        self.h.backend.read_texture(color, 0, 0, &mut out, W * 4, 0);
        out
    }
}

fn px(out: &[u8], x: usize, y: usize) -> [u8; 4] {
    <[u8; 4]>::try_from(&out[(y * W + x) * 4..][..4]).unwrap()
}

fn near(a: [u8; 4], b: [u8; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 2)
}

/// Blending: half-transparent red over blue with (src alpha, 1 - src alpha) gives purple; the
/// write mask keeps a channel out; the blend constant works as a factor.
#[test]
fn blend_and_write_mask_are_applied() {
    let Some(r) = Rig::new() else { return };
    let color = r.target(Format::Rgba8Unorm, false);
    let alpha_blend = Blend {
        src: BlendFactor::SrcAlpha,
        dst: BlendFactor::InvSrcAlpha,
        op: BlendOp::Add,
        src_alpha: BlendFactor::One,
        dst_alpha: BlendFactor::InvSrcAlpha,
        op_alpha: BlendOp::Add,
    };
    let state = [ColorState {
        blend: Some(alpha_blend),
        write_mask: 0xf,
    }];
    let pso = r.pipeline(None, &state, |_| {});
    let mut l = r.list(&color, None, [0.0, 0.0, 1.0, 1.0]);
    r.draw(&mut l, &pso, 0, &triangle([1.0, 0.0, 0.0, 0.5]));
    run(&r.h, &[&l]);
    let out = r.read(&color);
    assert!(near(px(&out, 8, 8), [128, 0, 128, 255]), "{:?}", px(&out, 8, 8));
    assert_eq!(px(&out, 0, 0), [0, 0, 255, 255]);

    // Without blending but with the green and blue channels masked, only red lands.
    let masked = [ColorState {
        blend: None,
        write_mask: 0b0001,
    }];
    let pso = r.pipeline(None, &masked, |_| {});
    let mut l = r.list(&color, None, [0.0, 0.0, 1.0, 1.0]);
    r.draw(&mut l, &pso, 0, &triangle([1.0, 1.0, 1.0, 1.0]));
    run(&r.h, &[&l]);
    assert_eq!(px(&r.read(&color), 8, 8), [255, 0, 255, 255]);

    // The blend constant: factor (constant, 1 - constant) with 0.25 gives a quarter white.
    let constant = [ColorState {
        blend: Some(Blend {
            src: BlendFactor::Constant,
            dst: BlendFactor::InvConstant,
            op: BlendOp::Add,
            src_alpha: BlendFactor::One,
            dst_alpha: BlendFactor::Zero,
            op_alpha: BlendOp::Add,
        }),
        write_mask: 0xf,
    }];
    let pso = r.pipeline(None, &constant, |_| {});
    let mut l = r.list(&color, None, [0.0, 0.0, 0.0, 1.0]);
    l.set_blend_factor([0.25; 4]);
    r.draw(&mut l, &pso, 0, &triangle([1.0, 1.0, 1.0, 1.0]));
    run(&r.h, &[&l]);
    assert!(near(px(&r.read(&color), 8, 8), [64, 64, 64, 255]));
}

/// Viewport and scissor: the viewport maps the triangle into the left half; the scissor cuts the
/// right of the target away; an empty scissor draws nothing.
#[test]
fn viewport_and_scissor_are_applied() {
    let Some(r) = Rig::new() else { return };
    let color = r.target(Format::Rgba8Unorm, false);
    let pso = r.pipeline(None, &[], |_| {});
    let red = triangle([1.0, 0.0, 0.0, 1.0]);

    let mut l = r.list(&color, None, [0.0, 0.0, 1.0, 1.0]);
    l.set_viewport(Some(Viewport {
        x: 0.0,
        y: 0.0,
        width: 8.0,
        height: 16.0,
        min_depth: 0.0,
        max_depth: 1.0,
    }));
    r.draw(&mut l, &pso, 0, &red);
    run(&r.h, &[&l]);
    let out = r.read(&color);
    assert_eq!(px(&out, 4, 8), [255, 0, 0, 255], "inside the viewport");
    assert_eq!(px(&out, 12, 8), [0, 0, 255, 255], "right of the viewport");

    let mut l = r.list(&color, None, [0.0, 0.0, 1.0, 1.0]);
    l.set_scissor(Some([0, 0, 8, 16]));
    r.draw(&mut l, &pso, 0, &red);
    run(&r.h, &[&l]);
    let out = r.read(&color);
    assert_eq!(px(&out, 6, 8), [255, 0, 0, 255], "inside the scissor");
    assert_eq!(px(&out, 10, 8), [0, 0, 255, 255], "right of it");

    // A rectangle outside the target is clipped to it, an empty one draws nothing.
    let mut l = r.list(&color, None, [0.0, 0.0, 1.0, 1.0]);
    l.set_scissor(Some([-100, -100, 100, 100]));
    r.draw(&mut l, &pso, 0, &red);
    l.set_scissor(Some([4, 4, 4, 12]));
    r.draw(&mut l, &pso, 4096, &triangle([0.0, 1.0, 0.0, 1.0]));
    run(&r.h, &[&l]);
    assert_eq!(px(&r.read(&color), 8, 8), [255, 0, 0, 255]);
}

/// Stencil: a triangle writes 1 where it covers (replace, reference 1); a second, larger one
/// passes only where the stencil equals 1, so it paints over the first and nowhere else.
#[test]
fn stencil_test_and_reference_are_applied() {
    let Some(r) = Rig::new() else { return };
    let color = r.target(Format::Rgba8Unorm, false);
    let depth = r.target(Format::D32FloatS8, true);
    let face = |compare, pass| StencilFace {
        fail: StencilOp::Keep,
        depth_fail: StencilOp::Keep,
        pass,
        compare,
    };
    let stencil = |compare, pass| StencilState {
        read_mask: 0xff,
        write_mask: 0xff,
        front: face(compare, pass),
        back: face(compare, pass),
    };
    let write = r.pipeline(Some(Format::D32FloatS8), &[], |d| {
        d.stencil = Some(stencil(Compare::Always, StencilOp::Replace))
    });
    let test = r.pipeline(Some(Format::D32FloatS8), &[], |d| {
        d.stencil = Some(stencil(Compare::Equal, StencilOp::Keep))
    });
    let mut l = r.list(&color, Some(&depth), [0.0, 0.0, 1.0, 1.0]);
    l.set_stencil_ref(1);
    r.draw(&mut l, &write, 0, &triangle([1.0, 0.0, 0.0, 1.0]));
    let green = [0.0, 1.0, 0.0, 1.0];
    let big = [vertex(-3.0, -1.0, green), vertex(3.0, -1.0, green), vertex(0.0, 5.0, green)];
    r.draw(&mut l, &test, 4096, &big);
    run(&r.h, &[&l]);
    let out = r.read(&color);
    assert_eq!(px(&out, 8, 8), [0, 255, 0, 255], "stencil equal: painted");
    assert_eq!(px(&out, 1, 8), [0, 0, 255, 255], "stencil zero: left alone");
    assert_eq!(px(&out, 14, 14), [0, 0, 255, 255], "stencil zero: left alone");
}

/// A colour target smaller than its depth target (a 1920x1080 target over a 3840x2160 depth
/// buffer in GTA VI's story load) draws into the area both cover, as the console does, with a
/// scissor of the whole depth target. The pass used to be refused, and the whole piece with it.
#[test]
fn a_depth_target_larger_than_the_colour_target_still_draws() {
    let Some(r) = Rig::new() else { return };
    let color = r.target(Format::Rgba8Unorm, false);
    let mut d = TextureDesc::d2(Format::D32FloatS8, W * 2, W * 2);
    d.render_target = true;
    let depth = r.h.texture(&d).unwrap();
    let pso = r.pipeline(Some(Format::D32FloatS8), &[], |d| d.depth_test = Some((true, Compare::Always)));
    let mut l = r.list(&color, Some(&depth), [0.0, 0.0, 1.0, 1.0]);
    l.set_scissor(Some([0, 0, 2 * W as i32, 2 * W as i32]));
    r.draw(&mut l, &pso, 0, &triangle([1.0, 0.0, 0.0, 1.0]));
    run(&r.h, &[&l]);
    let out = r.read(&color);
    assert_eq!(px(&out, 8, 8), [255, 0, 0, 255], "inside the triangle");
    assert_eq!(px(&out, 0, 0), [0, 0, 255, 255], "cleared corner");
}

/// Clearing depth must preserve stencil written by an earlier draw.
#[test]
fn depth_only_clear_preserves_stencil() {
    let Some(r) = Rig::new() else { return };
    let color = r.target(Format::Rgba8Unorm, false);
    let depth = r.target(Format::D32FloatS8, true);
    let stencil = |compare, pass| {
        let face = StencilFace {
            fail: StencilOp::Keep,
            depth_fail: StencilOp::Keep,
            pass,
            compare,
        };
        StencilState {
            read_mask: 0xff,
            write_mask: 0xff,
            front: face,
            back: face,
        }
    };
    let write = r.pipeline(Some(Format::D32FloatS8), &[], |d| {
        d.stencil = Some(stencil(Compare::Always, StencilOp::Replace));
        d.depth_test = Some((true, Compare::Always));
    });
    let test = r.pipeline(Some(Format::D32FloatS8), &[], |d| {
        d.stencil = Some(stencil(Compare::Equal, StencilOp::Keep));
        d.depth_test = Some((false, Compare::Less));
    });
    let mut l = r.list(&color, Some(&depth), [0.0, 0.0, 1.0, 1.0]);
    l.set_stencil_ref(37);
    r.draw(&mut l, &write, 0, &triangle([1.0, 0.0, 0.0, 1.0]));
    l.clear_depth(&depth, 1.0);
    r.draw(&mut l, &test, 4096, &triangle([0.0, 1.0, 0.0, 1.0]));
    run(&r.h, &[&l]);
    assert_eq!(
        px(&r.read(&color), 8, 8),
        [0, 255, 0, 255],
        "depth-only clear erased stencil"
    );
}

/// Nonzero and zero clears, either stencil alone or both planes. The draw checks the exact
/// depth and stencil together, then mismatching draws must be rejected by each test in turn.
#[test]
fn stencil_clears_select_the_requested_planes_and_value() {
    let Some(r) = Rig::new() else { return };
    let color = r.target(Format::Rgba8Unorm, false);
    let depth = r.target(Format::D32FloatS8, true);
    let pso = r.pipeline(Some(Format::D32FloatS8), &[], |d| {
        let face = StencilFace {
            fail: StencilOp::Keep,
            depth_fail: StencilOp::Keep,
            pass: StencilOp::Keep,
            compare: Compare::Equal,
        };
        d.stencil = Some(StencilState {
            read_mask: 0xff,
            write_mask: 0xff,
            front: face,
            back: face,
        });
        d.depth_test = Some((false, Compare::Equal));
    });
    for clear_depth in [None, Some(0.75f32)] {
        for value in [0u8, 173, 255] {
            let mut l = r.list(&color, Some(&depth), [0.0, 0.0, 1.0, 1.0]);
            l.clear_depth_stencil(&depth, Some(0.25), Some(!value));
            l.clear_depth_stencil(&depth, clear_depth, Some(value));
            let expected_depth = clear_depth.unwrap_or(0.25);
            l.set_graphics_root_constants(0, &[expected_depth.to_bits(), 0, 0, 0]);
            l.set_stencil_ref(u32::from(value));
            r.draw(&mut l, &pso, 0, &triangle([0.0, 1.0, 0.0, 1.0]));
            l.set_stencil_ref(u32::from(!value));
            r.draw(&mut l, &pso, 4096, &triangle([1.0, 0.0, 0.0, 1.0]));
            l.set_stencil_ref(u32::from(value));
            l.set_graphics_root_constants(0, &[0.5f32.to_bits(), 0, 0, 0]);
            r.draw(&mut l, &pso, 8192, &triangle([0.0, 0.0, 1.0, 1.0]));
            // Submission snapshots copy commands, not the mutable recording state.
            let snapshot = l.snapshot();
            run(&r.h, &[&snapshot]);
            assert_eq!(
                px(&r.read(&color), 8, 8),
                [0, 255, 0, 255],
                "depth={clear_depth:?}, stencil={value}"
            );
        }
    }
}

/// Culling and winding: the triangle's vertices run counter-clockwise on screen; back-face
/// culling with clockwise fronts (D3D's default) removes it, with counter-clockwise fronts keeps
/// it; front-face culling does the opposite.
#[test]
fn culling_follows_the_winding_and_its_front_face() {
    let Some(r) = Rig::new() else { return };
    let color = r.target(Format::Rgba8Unorm, false);
    let red = [1.0, 0.0, 0.0, 1.0];
    // Counter-clockwise as seen on screen (y up in clip space).
    let ccw = [vertex(-0.5, -0.5, red), vertex(0.5, -0.5, red), vertex(0.0, 0.5, red)];
    let drawn = |cull, front_ccw| {
        let pso = r.pipeline(None, &[], |d| {
            d.cull = cull;
            d.raster.front_ccw = front_ccw;
        });
        let mut l = r.list(&color, None, [0.0, 0.0, 1.0, 1.0]);
        r.draw(&mut l, &pso, 0, &ccw);
        run(&r.h, &[&l]);
        px(&r.read(&color), 8, 8) == [255, 0, 0, 255]
    };
    assert!(!drawn(Cull::Back, false), "back culled: it is a back face when fronts are clockwise");
    assert!(drawn(Cull::Back, true));
    assert!(drawn(Cull::Front, false));
    assert!(!drawn(Cull::Front, true));
    assert!(drawn(Cull::None, false));
}

/// Depth bias applies to a draw and resets to zero on the following draw in the same render pass.
#[test]
fn depth_bias_resets_between_draws_in_the_same_pass() {
    let Some(r) = Rig::new() else { return };
    let color = r.target(Format::Rgba8Unorm, false);
    let depth = r.target(Format::D32Float, true);
    let biased = r.pipeline(Some(Format::D32Float), &[], |d| {
        d.depth_test = Some((true, Compare::Always));
        d.raster.depth_bias = (10000.0, 0.0, 0.0);
    });
    let unbiased = r.pipeline(Some(Format::D32Float), &[], |d| {
        d.depth_test = Some((false, Compare::Less));
    });

    let mut l = r.list(&color, Some(&depth), [0.0; 4]);
    // Draw 1 pushes depth back with positive bias; Draw 2 at the same Z with zero bias must pass Less.
    r.draw(&mut l, &biased, 0, &triangle([1.0, 0.0, 0.0, 1.0]));
    r.draw(&mut l, &unbiased, 256, &triangle([0.0, 1.0, 0.0, 1.0]));
    run(&r.h, &[&l]);
    let out = r.read(&color);
    assert_eq!(
        px(&out, 8, 8),
        [0, 255, 0, 255],
        "zero depth bias must not inherit the previous draw's bias"
    );
}
