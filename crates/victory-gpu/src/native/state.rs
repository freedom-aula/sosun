//! A title's graphics pipeline state, read from its pipeline-state stream (the subobjects of
//! `CreatePipelineState`: blend, rasterizer, depth and stencil, input layout, target formats) and
//! turned into what the backend's render pipeline takes. The same reading as
//! `pipeline/host.rs`'s `GraphicsState::read` (the D3D12 host keeps the raw D3D12 structures), in
//! plain Rust, so it needs no host API. Subobjects the stream leaves out keep D3D12's defaults.

use super::descriptors::format;
use victory_backend::resources::*;

/// One colour target of the blend description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TargetBlend {
    enable: bool,
    src: u32,
    dst: u32,
    op: u32,
    src_alpha: u32,
    dst_alpha: u32,
    op_alpha: u32,
    mask: u8,
}

impl Default for TargetBlend {
    fn default() -> Self {
        TargetBlend {
            enable: false,
            src: 2,
            dst: 1,
            op: 1,
            src_alpha: 2,
            dst_alpha: 1,
            op_alpha: 1,
            mask: 0xf,
        }
    }
}

/// An input element: its slot, byte offset (`APPEND_ALIGNED` is resolved), DXGI format, semantic,
/// and whether it steps per instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub name: String,
    pub index: u32,
    pub format: u32,
    pub slot: u32,
    pub offset: u32,
    pub per_instance: bool,
}

const APPEND_ALIGNED: u32 = u32::MAX;

#[derive(Clone, Debug, PartialEq)]
pub struct Graphics {
    independent: bool,
    blend: [TargetBlend; 8],
    fill: u32,
    cull: u32,
    front_ccw: bool,
    bias: (i32, f32, f32),
    depth_clip: bool,
    depth_enable: bool,
    depth_write: bool,
    depth_func: u32,
    stencil_enable: bool,
    stencil_masks: (u8, u8),
    /// Front then back: fail, depth fail, pass, compare.
    stencil_faces: [[u32; 4]; 2],
    inputs: Vec<Input>,
    pub topology_type: u32,
    rt_formats: Vec<u32>,
    depth_format: u32,
    pub samples: u32,
    /// A guessed pipeline (`VICTORY_GUESS_GRAPHICS`, [`Graphics::guessed`]): the formats of the
    /// targets bound at the draw, and the strides of the vertex buffers bound.
    guess: Option<Guess>,
}

/// What a draw gives a guessed pipeline that the description does not.
#[derive(Clone, Debug, PartialEq, Default)]
struct Guess {
    colors: Vec<Format>,
    depth: Option<Format>,
    strides: Vec<u32>,
}

impl Default for Graphics {
    fn default() -> Self {
        Graphics {
            independent: false,
            blend: [TargetBlend::default(); 8],
            fill: 3,
            cull: 3,
            front_ccw: false,
            bias: (0, 0.0, 0.0),
            depth_clip: true,
            depth_enable: true,
            depth_write: true,
            depth_func: 2,
            stencil_enable: false,
            stencil_masks: (0xff, 0xff),
            stencil_faces: [[1, 1, 1, 8]; 2],
            inputs: Vec::new(),
            topology_type: 0,
            rt_formats: Vec::new(),
            depth_format: 0,
            samples: 1,
            guess: None,
        }
    }
}

fn word(at: *const u8, byte: usize) -> u32 {
    // SAFETY: the caller passes a whole subobject, whose layout names `byte`.
    unsafe { at.add(byte).cast::<u32>().read_unaligned() }
}

impl Graphics {
    /// Takes in one subobject of a stream: its type, then its data at +4 (+8 for those holding
    /// pointers).
    ///
    /// # Safety
    /// `at` points at a whole subobject of type `kind`, whose pointers (input elements, their
    /// names) are valid.
    pub unsafe fn read(&mut self, kind: u32, at: *const u8) {
        // SAFETY: the caller passes a whole subobject; offsets follow the D3D12 structures.
        unsafe {
            let data = at.add(4);
            match kind {
                // D3D12_BLEND_DESC: alpha to coverage, independent blend, 8 targets of 10 words.
                8 => {
                    self.independent = word(data, 4) != 0;
                    for (i, t) in self.blend.iter_mut().enumerate() {
                        let w = |k: usize| word(data, 8 + i * 40 + k * 4);
                        *t = TargetBlend {
                            enable: w(0) != 0,
                            src: w(2),
                            dst: w(3),
                            op: w(4),
                            src_alpha: w(5),
                            dst_alpha: w(6),
                            op_alpha: w(7),
                            mask: w(9) as u8,
                        };
                    }
                }
                // D3D12_RASTERIZER_DESC.
                10 => {
                    self.fill = word(data, 0);
                    self.cull = word(data, 4);
                    self.front_ccw = word(data, 8) != 0;
                    self.bias = (
                        word(data, 12) as i32,
                        f32::from_bits(word(data, 16)),
                        f32::from_bits(word(data, 20)),
                    );
                    self.depth_clip = word(data, 24) != 0;
                }
                // D3D12_DEPTH_STENCIL_DESC, and its DESC1 (the same up to the depth bounds).
                11 | 21 => {
                    self.depth_enable = word(data, 0) != 0;
                    self.depth_write = word(data, 4) != 0;
                    self.depth_func = word(data, 8);
                    self.stencil_enable = word(data, 12) != 0;
                    let masks = word(data, 16);
                    self.stencil_masks = (masks as u8, (masks >> 8) as u8);
                    for (face, base) in [20usize, 36].into_iter().enumerate() {
                        self.stencil_faces[face] = [
                            word(data, base),
                            word(data, base + 4),
                            word(data, base + 8),
                            word(data, base + 12),
                        ];
                    }
                }
                12 => {
                    let ptr = at.add(8).cast::<*const u8>().read_unaligned();
                    let count = word(at, 16) as usize;
                    self.inputs.clear();
                    if ptr.is_null() {
                        return;
                    }
                    let mut next = [0u32; 32];
                    for i in 0..count {
                        let e = ptr.add(i * 32);
                        let name = e.cast::<*const std::ffi::c_char>().read_unaligned();
                        if name.is_null() {
                            continue;
                        }
                        let slot = word(e, 16).min(31);
                        let format = crate::resource::pc_format(word(e, 12));
                        let bytes = crate::resource::element(format).map_or(0, |(b, _)| b as u32);
                        // An appended element starts at the next multiple of min(4, its size).
                        let offset = match word(e, 20) {
                            APPEND_ALIGNED => next[slot as usize]
                                .checked_next_multiple_of(bytes.clamp(1, 4))
                                .unwrap_or(u32::MAX),
                            o => o,
                        };
                        next[slot as usize] = offset.saturating_add(bytes);
                        self.inputs.push(Input {
                            name: std::ffi::CStr::from_ptr(name).to_string_lossy().into_owned(),
                            index: word(e, 8),
                            format,
                            slot,
                            offset,
                            per_instance: word(e, 24) == 1,
                        });
                    }
                }
                14 => self.topology_type = word(data, 0),
                15 => {
                    let count = (word(data, 32) as usize).min(8);
                    self.rt_formats = (0..count)
                        .map(|i| crate::resource::pc_format(word(data, i * 4)))
                        .collect();
                }
                16 => self.depth_format = crate::resource::pc_format(word(data, 0)),
                17 => self.samples = word(data, 0).max(1),
                _ => {}
            }
        }
    }

    /// The state of a pipeline whose description has none (device slot 0x1c8; docs/native-host.md,
    /// "Guessed graphics pipelines"): no blend, no culling, depth test less-equal with writes, the
    /// formats of the targets bound, and the vertex buffer slots and strides the draw has. The
    /// input elements are the vertex shader's `VB<kind><n>` semantics (`reads`, as
    /// [`vertex_inputs`] gives them; index: slot << 16 | byte offset).
    pub fn guessed(
        colors: Vec<Format>,
        depth: Option<Format>,
        strides: Vec<u32>,
        reads: &[(String, u32)],
    ) -> Graphics {
        let mut g = Graphics { cull: 1, depth_func: 4, ..Graphics::default() };
        for (name, index) in reads {
            // F float, U uint, N unorm8x4 (DXGI 41/16/6/2, 42, 28).
            let format = match (name.as_bytes().get(2), name.as_bytes().get(3)) {
                (Some(b'F'), Some(b'1')) => 41,
                (Some(b'F'), Some(b'2')) => 16,
                (Some(b'F'), Some(b'3')) => 6,
                (Some(b'F'), Some(b'4')) => 2,
                (Some(b'U'), _) => 42,
                (Some(b'N'), _) => 28,
                _ => continue,
            };
            g.inputs.push(Input {
                name: name.clone(),
                index: *index,
                format,
                slot: (index >> 16).min(31),
                offset: index & 0xffff,
                per_instance: false,
            });
        }
        g.guess = Some(Guess { colors, depth, strides });
        g
    }

    /// Whether this is [`Graphics::guessed`]'s state, to be made at the first draw.
    pub fn is_guessed(&self) -> bool {
        self.guess.is_some()
    }

    /// The input elements, in the title's order.
    pub fn inputs(&self) -> &[Input] {
        &self.inputs
    }

    /// The colour target formats the host knows; `None` if one is not (the pipeline is refused).
    pub fn color_formats(&self) -> Option<Vec<Format>> {
        if let Some(g) = &self.guess {
            return Some(g.colors.clone());
        }
        self.rt_formats.iter().map(|&f| format(f)).collect()
    }

    /// The depth format; `Ok(None)` for no depth target, `Err` if the host does not know it.
    pub fn depth_format(&self) -> Result<Option<Format>, u32> {
        if let Some(g) = &self.guess {
            return Ok(g.depth);
        }
        match self.depth_format {
            0 => Ok(None),
            f => format(f).map(Some).ok_or(f),
        }
    }

    /// The attributes and streams of the input layout for a vertex shader reading `reads`
    /// (semantic, index) in its input signature's order. A stream's stride is where its elements
    /// end. `None`: the shader reads an input the layout does not have.
    pub fn vertex_layout(
        &self,
        reads: &[(String, u32)],
    ) -> Option<(Vec<VertexAttr>, Vec<VertexStream>)> {
        let mut attributes = Vec::new();
        for (name, index) in reads {
            let e = self
                .inputs
                .iter()
                .find(|e| e.name.eq_ignore_ascii_case(name) && e.index == *index)?;
            attributes.push(VertexAttr {
                format: vertex_format(e.format)?,
                slot: e.slot as usize,
                offset: e.offset as usize,
            });
        }
        let mut streams: Vec<VertexStream> = Vec::new();
        for e in &self.inputs {
            let end = (e.offset + crate::resource::element(e.format).map_or(0, |(b, _)| b as u32)) as usize;
            match streams.iter_mut().find(|s| s.slot == e.slot as usize) {
                Some(s) => s.stride = s.stride.max(end),
                None => streams.push(VertexStream {
                    slot: e.slot as usize,
                    stride: end,
                    per_instance: e.per_instance,
                }),
            }
        }
        for s in &mut streams {
            s.stride = s.stride.next_multiple_of(4);
            // A guessed pipeline's streams have the strides of the buffers bound.
            if let Some(&bound) = self.guess.as_ref().and_then(|g| g.strides.get(s.slot)) {
                s.stride = s.stride.max(bound as usize);
            }
        }
        Some((attributes, streams))
    }

    /// Blend and write mask of each colour target (target 0's description for all when the blend
    /// description is not independent).
    pub fn color_state(&self) -> Vec<ColorState> {
        let targets = self.guess.as_ref().map_or(self.rt_formats.len(), |g| g.colors.len());
        (0..targets.max(1))
            .map(|i| {
                let t = &self.blend[if self.independent { i } else { 0 }];
                ColorState {
                    blend: t.enable.then(|| Blend {
                        src: blend_factor(t.src),
                        dst: blend_factor(t.dst),
                        op: blend_op(t.op),
                        src_alpha: blend_factor(t.src_alpha),
                        dst_alpha: blend_factor(t.dst_alpha),
                        op_alpha: blend_op(t.op_alpha),
                    }),
                    write_mask: t.mask & 0xf,
                }
            })
            .collect()
    }

    pub fn depth_test(&self) -> Option<(bool, Compare)> {
        self.depth_enable
            .then(|| (self.depth_write, compare(self.depth_func)))
    }

    pub fn stencil(&self) -> Option<StencilState> {
        let face = |f: [u32; 4]| StencilFace {
            fail: stencil_op(f[0]),
            depth_fail: stencil_op(f[1]),
            pass: stencil_op(f[2]),
            compare: compare(f[3]),
        };
        self.stencil_enable.then(|| StencilState {
            read_mask: self.stencil_masks.0,
            write_mask: self.stencil_masks.1,
            front: face(self.stencil_faces[0]),
            back: face(self.stencil_faces[1]),
        })
    }

    pub fn cull(&self) -> Cull {
        match self.cull {
            1 => Cull::None,
            2 => Cull::Front,
            _ => Cull::Back,
        }
    }

    pub fn raster(&self) -> Raster {
        Raster {
            front_ccw: self.front_ccw,
            wireframe: self.fill == 2,
            depth_clip: self.depth_clip,
            depth_bias: (self.bias.0 as f32, self.bias.2, self.bias.1),
        }
    }
}

/// `D3D12_BLEND`.
pub fn blend_factor(f: u32) -> BlendFactor {
    match f {
        1 => BlendFactor::Zero,
        3 => BlendFactor::SrcColor,
        4 => BlendFactor::InvSrcColor,
        5 => BlendFactor::SrcAlpha,
        6 => BlendFactor::InvSrcAlpha,
        7 => BlendFactor::DestAlpha,
        8 => BlendFactor::InvDestAlpha,
        9 => BlendFactor::DestColor,
        10 => BlendFactor::InvDestColor,
        11 => BlendFactor::SrcAlphaSat,
        14 => BlendFactor::Constant,
        15 => BlendFactor::InvConstant,
        16 => BlendFactor::Src1Color,
        17 => BlendFactor::InvSrc1Color,
        18 => BlendFactor::Src1Alpha,
        19 => BlendFactor::InvSrc1Alpha,
        _ => BlendFactor::One,
    }
}

/// `D3D12_BLEND_OP`.
pub fn blend_op(o: u32) -> BlendOp {
    match o {
        2 => BlendOp::Subtract,
        3 => BlendOp::ReverseSubtract,
        4 => BlendOp::Min,
        5 => BlendOp::Max,
        _ => BlendOp::Add,
    }
}

/// `D3D12_COMPARISON_FUNC`.
pub fn compare(c: u32) -> Compare {
    match c {
        1 => Compare::Never,
        2 => Compare::Less,
        3 => Compare::Equal,
        4 => Compare::LessEqual,
        5 => Compare::Greater,
        6 => Compare::NotEqual,
        7 => Compare::GreaterEqual,
        _ => Compare::Always,
    }
}

/// `D3D12_STENCIL_OP`.
pub fn stencil_op(o: u32) -> StencilOp {
    match o {
        2 => StencilOp::Zero,
        3 => StencilOp::Replace,
        4 => StencilOp::IncrementClamp,
        5 => StencilOp::DecrementClamp,
        6 => StencilOp::Invert,
        7 => StencilOp::IncrementWrap,
        8 => StencilOp::DecrementWrap,
        _ => StencilOp::Keep,
    }
}

/// An input element's DXGI format (PC numbering) as a vertex format.
pub fn vertex_format(dxgi: u32) -> Option<VertexFormat> {
    Some(match dxgi {
        41 => VertexFormat::Float,
        16 => VertexFormat::Float2,
        6 => VertexFormat::Float3,
        2 => VertexFormat::Float4,
        42 => VertexFormat::Uint,
        28 => VertexFormat::Unorm8x4,
        _ => return None,
    })
}

/// `IASetPrimitiveTopology`'s value as the backend's topology: lists and strips of triangles, lines
/// and points. Patches, adjacency and the console's rectangle and quad lists are not.
pub fn topology(d3d: u32) -> Option<Topology> {
    Some(match d3d {
        1 => Topology::Points,
        2 => Topology::Lines,
        3 => Topology::LineStrip,
        4 => Topology::Triangles,
        5 => Topology::TriangleStrip,
        _ => return None,
    })
}

/// The semantics a vertex shader reads, in its input signature's order (system values, which the
/// pipeline supplies, left out), from its DXIL container's `ISG1` part.
pub fn vertex_inputs(dxil: &[u8]) -> Option<Vec<(String, u32)>> {
    use victory_shader::{Container, FourCC};
    let container = Container::parse(dxil).ok()?;
    let part = container.part(FourCC::ISG1)?;
    let d = part.data;
    let count = u32::from_le_bytes(d.get(..4)?.try_into().ok()?) as usize;
    let mut reads = Vec::new();
    for i in 0..count {
        let row = d.get(8 + 32 * i..8 + 32 * i + 32)?;
        let w = |k: usize| u32::from_le_bytes(row[k * 4..k * 4 + 4].try_into().unwrap());
        let (name_at, index, system) = (w(1) as usize, w(2), w(3));
        if system != 0 {
            continue;
        }
        let tail = d.get(name_at..)?;
        let end = tail.iter().position(|&b| b == 0)?;
        reads.push((String::from_utf8_lossy(&tail[..end]).into_owned(), index));
    }
    Some(reads)
}

/// The positions of the system values in a vertex shader's input signature that come before its
/// last attribute. The converter numbers the stage-in attributes by signature position, system
/// values included, so these positions are attribute slots no element fills.
pub fn system_gaps(dxil: &[u8]) -> Vec<usize> {
    use victory_shader::{Container, FourCC};
    let Some(part) = Container::parse(dxil).ok().and_then(|c| c.part(FourCC::ISG1).map(|p| p.data.to_vec())) else {
        return Vec::new();
    };
    let count = part.get(..4).map_or(0, |c| u32::from_le_bytes(c.try_into().unwrap()) as usize);
    let system: Vec<bool> = (0..count)
        .map(|i| part.get(8 + 32 * i + 12..8 + 32 * i + 16).is_some_and(|w| w != [0; 4]))
        .collect();
    let last = system.iter().rposition(|s| !s).unwrap_or(0);
    (0..last).filter(|&i| system[i]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream's subobject bytes: the type, then the data at +4.
    fn sub(kind: u32, data: &[u32]) -> Vec<u8> {
        let mut v = kind.to_le_bytes().to_vec();
        v.extend(data.iter().flat_map(|w| w.to_le_bytes()));
        v.resize(v.len().next_multiple_of(8), 0);
        v
    }

    #[test]
    fn blend_depth_stencil_and_raster_subobjects_are_read() {
        let mut g = Graphics::default();
        // Blend: target 0 on, (SRC_ALPHA, INV_SRC_ALPHA, ADD / ONE, ZERO, ADD), write mask 0b0111;
        // target 1 independent (off, mask 0b1000).
        let mut blend = vec![0u32; 2 + 80];
        blend[1] = 1;
        blend[2..12].copy_from_slice(&[1, 0, 5, 6, 1, 2, 1, 1, 0, 0b0111]);
        blend[12..22].copy_from_slice(&[0, 0, 2, 1, 1, 2, 1, 1, 0, 0b1000]);
        let rast = [3, 2, 1, 5, 0.5f32.to_bits(), 2.0f32.to_bits(), 0, 0, 0, 0, 0];
        // Depth: on, no write, LESS_EQUAL; stencil on, masks 0x0f/0xf0, front (KEEP, KEEP, REPLACE,
        // EQUAL), back (ZERO, INVERT, KEEP, NEVER).
        let depth = [1, 0, 4, 1, 0xf0_0f, 1, 1, 3, 3, 2, 6, 1, 1];
        for s in [sub(8, &blend), sub(10, &rast), sub(11, &depth)] {
            // SAFETY: whole subobjects without pointers.
            unsafe { g.read(u32::from_le_bytes(s[..4].try_into().unwrap()), s.as_ptr()) };
        }
        let mut formats = [0u32; 9];
        formats[0] = 28;
        formats[1] = 10;
        formats[8] = 2;
        let s = sub(15, &formats);
        // SAFETY: as above.
        unsafe { g.read(15, s.as_ptr()) };
        let s = sub(16, &[40]);
        // SAFETY: as above.
        unsafe { g.read(16, s.as_ptr()) };

        let state = g.color_state();
        assert_eq!(state.len(), 2);
        assert_eq!(
            state[0].blend,
            Some(Blend {
                src: BlendFactor::SrcAlpha,
                dst: BlendFactor::InvSrcAlpha,
                op: BlendOp::Add,
                src_alpha: BlendFactor::One,
                dst_alpha: BlendFactor::Zero,
                op_alpha: BlendOp::Add,
            })
        );
        assert_eq!((state[0].write_mask, state[1].blend, state[1].write_mask), (0b0111, None, 0b1000));
        assert_eq!(g.color_formats(), Some(vec![Format::Rgba8Unorm, Format::Rgba16Float]));
        assert_eq!(g.depth_format(), Ok(Some(Format::D32Float)));
        assert_eq!(g.depth_test(), Some((false, Compare::LessEqual)));
        let st = g.stencil().unwrap();
        assert_eq!((st.read_mask, st.write_mask), (0x0f, 0xf0));
        assert_eq!(st.front.pass, StencilOp::Replace);
        assert_eq!(st.front.compare, Compare::Equal);
        assert_eq!((st.back.fail, st.back.depth_fail, st.back.compare), (StencilOp::Zero, StencilOp::Invert, Compare::Never));
        assert_eq!(g.cull(), Cull::Front);
        let r = g.raster();
        assert!(r.front_ccw && !r.wireframe && !r.depth_clip);
        assert_eq!(r.depth_bias, (5.0, 2.0, 0.5));
    }

    #[test]
    fn input_layouts_resolve_offsets_and_strides_in_signature_order() {
        // POSITION float3 at 0, COLOR rgba8 appended, TEXCOORD float2 appended; slot 1: a
        // per-instance float4 at 16.
        let names = [c"POSITION", c"COLOR", c"TEXCOORD", c"OFFSET"];
        let el = |n: usize, format: u32, slot: u32, offset: u32, class: u32| {
            let mut e = [0u8; 32];
            e[..8].copy_from_slice(&(names[n].as_ptr() as u64).to_le_bytes());
            for (k, w) in [0, format, slot, offset, class, 0].into_iter().enumerate() {
                e[8 + 4 * k..12 + 4 * k].copy_from_slice(&u32::to_le_bytes(w));
            }
            e
        };
        let mut elements = Vec::new();
        for e in [
            el(0, 6, 0, 0, 0),
            el(1, 28, 0, u32::MAX, 0),
            el(2, 16, 0, u32::MAX, 0),
            el(3, 2, 1, 16, 1),
        ] {
            elements.extend(e);
        }
        let mut s = 12u32.to_le_bytes().to_vec();
        s.extend([0; 4]);
        s.extend((elements.as_ptr() as u64).to_le_bytes());
        s.extend(4u32.to_le_bytes());
        s.extend([0; 4]);
        let mut g = Graphics::default();
        // SAFETY: a whole input layout subobject over live elements and names.
        unsafe { g.read(12, s.as_ptr()) };
        assert_eq!(
            g.inputs().iter().map(|i| (i.slot, i.offset)).collect::<Vec<_>>(),
            [(0, 0), (0, 12), (0, 16), (1, 16)]
        );
        // The shader reads TEXCOORD, then POSITION, then OFFSET.
        let reads = [("TEXCOORD".to_owned(), 0), ("POSITION".to_owned(), 0), ("OFFSET".to_owned(), 0)];
        let (attrs, streams) = g.vertex_layout(&reads).unwrap();
        assert_eq!(attrs.iter().map(|a| (a.slot, a.offset)).collect::<Vec<_>>(), [(0, 16), (0, 0), (1, 16)]);
        assert_eq!(attrs[0].format, VertexFormat::Float2);
        assert_eq!(
            streams.iter().map(|s| (s.slot, s.stride, s.per_instance)).collect::<Vec<_>>(),
            [(0, 24, false), (1, 32, true)]
        );
        assert!(g.vertex_layout(&[("NORMAL".to_owned(), 0)]).is_none());
    }

    #[test]
    fn topologies_map_and_the_rest_are_refused() {
        assert_eq!(topology(4), Some(Topology::Triangles));
        assert_eq!(topology(3), Some(Topology::LineStrip));
        assert_eq!(topology(5), Some(Topology::TriangleStrip));
        // Adjacency, patches, the console's rectangle lists.
        assert_eq!([6, 33, 35, 100].map(topology), [None; 4]);
    }
}
