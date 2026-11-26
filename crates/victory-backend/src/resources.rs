//! Descriptor heaps, textures, samplers and render passes: the second slice of the seam (`backend`
//! has queues, fences and compute). Platform neutral, no Metal types.

use crate::backend::{ComputeBackend, Dispatch, Heaps, RootArgs};

/// Which descriptor heap type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeapKind {
    /// `D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV`: bound at slot 0.
    Resource,
    /// `D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER`: bound at slot 1.
    Sampler,
}

/// The DXGI formats the title's textures use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Rgba8Unorm,
    Rgba8UnormSrgb,
    Bgra8Unorm,
    Rgba16Float,
    R32Float,
    D32Float,
    Bc1,
    Bc2,
    Bc3,
    Bc4,
    Bc5,
    Bc6h,
    Bc7,
}

impl Format {
    /// (block width and height in texels, bytes per block): 1 for uncompressed, 4 for BC.
    pub fn block(self) -> (usize, usize) {
        use Format::*;
        match self {
            Rgba8Unorm | Rgba8UnormSrgb | Bgra8Unorm | R32Float | D32Float => (1, 4),
            Rgba16Float => (1, 8),
            Bc1 | Bc4 => (4, 8),
            Bc2 | Bc3 | Bc5 | Bc6h | Bc7 => (4, 16),
        }
    }

    /// Bytes of one row of `width` texels (of blocks, for BC).
    pub fn row_bytes(self, width: usize) -> usize {
        let (b, n) = self.block();
        width.div_ceil(b) * n
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TexDim {
    D2,
    D2Array,
    D3,
}

#[derive(Clone, Copy, Debug)]
pub struct TextureDesc {
    pub dim: TexDim,
    pub format: Format,
    pub width: usize,
    pub height: usize,
    /// Array length for `D2Array`, depth for `D3`, else 1.
    pub depth_or_layers: usize,
    pub mips: usize,
    /// `ALLOW_RENDER_TARGET` / `ALLOW_DEPTH_STENCIL`.
    pub render_target: bool,
    /// `ALLOW_UNORDERED_ACCESS`.
    pub unordered_access: bool,
}

impl TextureDesc {
    pub fn d2(format: Format, width: usize, height: usize) -> TextureDesc {
        TextureDesc {
            dim: TexDim::D2,
            format,
            width,
            height,
            depth_or_layers: 1,
            mips: 1,
            render_target: false,
            unordered_access: false,
        }
    }
}

/// A view of part of a texture, possibly in another (size-compatible) format.
#[derive(Clone, Copy, Debug)]
pub struct ViewDesc {
    pub format: Format,
    /// First mip, count.
    pub mips: (usize, usize),
    /// First slice, count.
    pub slices: (usize, usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Filter {
    Nearest,
    Linear,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Address {
    Wrap,
    Mirror,
    Clamp,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SamplerDesc {
    pub min_mag: Filter,
    pub mip: Filter,
    pub address: [Address; 3],
    pub max_anisotropy: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Compare {
    Never,
    Less,
    Equal,
    LessEqual,
    Greater,
    NotEqual,
    GreaterEqual,
    Always,
}

/// Descriptor heaps, textures and samplers. Heap entries are the converter's 24-byte
/// `IRDescriptorTableEntry`; a root descriptor table is the address of its first entry
/// (`heap_address`), pushed into `RootArgs` like a root view. Resources a heap entry names are kept
/// alive and resident by the heap until it is dropped (`MTLResidencySet`, or `useResource` per
/// encoder where residency sets are unavailable).
pub trait ResourceBackend: ComputeBackend {
    type Texture;
    type Sampler;

    fn create_heap(&self, kind: HeapKind, count: usize) -> Result<Self::Heap, String>;
    /// The GPU address of entry `index`: what a root descriptor table holds.
    fn heap_address(&self, heap: &Self::Heap, index: usize) -> u64;
    /// A raw buffer SRV or UAV over `size` bytes at `title` inside `buffer`.
    fn write_buffer(
        &self,
        heap: &Self::Heap,
        index: usize,
        buffer: &Self::Buffer,
        title: *const u8,
        size: u32,
    );
    /// A texture SRV or UAV.
    fn write_texture(&self, heap: &Self::Heap, index: usize, tex: &Self::Texture, min_lod: f32);
    fn write_sampler(&self, heap: &Self::Heap, index: usize, s: &Self::Sampler, lod_bias: f32);

    fn create_texture(&self, desc: &TextureDesc) -> Result<Self::Texture, String>;
    fn create_texture_view(
        &self,
        tex: &Self::Texture,
        view: &ViewDesc,
    ) -> Result<Self::Texture, String>;
    /// A 2D, single-mip texture whose texels are the title memory at `title` (zero copy): no upload,
    /// CPU and GPU see the same bytes. `title` and `row_pitch` must be multiples of `linear_alignment`.
    fn create_texture_over_memory(
        &self,
        buffer: &Self::Buffer,
        title: *const u8,
        desc: &TextureDesc,
        row_pitch: usize,
    ) -> Result<Self::Texture, String>;
    /// Alignment (bytes) of the offset and row pitch of a texture over memory.
    fn linear_alignment(&self, format: Format) -> usize;
    /// The whole mip of one slice (for 3D, of the whole volume); rows `row_pitch` apart, slices `slice_pitch`.
    fn upload_texture(
        &self,
        tex: &Self::Texture,
        mip: usize,
        slice: usize,
        data: &[u8],
        row_pitch: usize,
        slice_pitch: usize,
    );
    fn read_texture(
        &self,
        tex: &Self::Texture,
        mip: usize,
        slice: usize,
        out: &mut [u8],
        row_pitch: usize,
        slice_pitch: usize,
    );
    fn create_sampler(&self, desc: &SamplerDesc) -> Result<Self::Sampler, String>;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VertexFormat {
    Float,
    Float2,
    Float3,
    Float4,
    Uint,
    Unorm8x4,
}

/// One input element (`D3D12_INPUT_ELEMENT_DESC`), in the order of the vertex shader's input signature.
#[derive(Clone, Copy, Debug)]
pub struct VertexAttr {
    pub format: VertexFormat,
    pub slot: usize,
    pub offset: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct VertexStream {
    pub slot: usize,
    pub stride: usize,
    pub per_instance: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cull {
    None,
    Front,
    Back,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Topology {
    Triangles,
    TriangleStrip,
    Lines,
    Points,
}

pub struct RenderPipelineDesc<'a> {
    pub vs: &'a [u8],
    pub ps: Option<&'a [u8]>,
    pub attributes: &'a [VertexAttr],
    pub streams: &'a [VertexStream],
    pub color: &'a [Format],
    pub depth: Option<Format>,
    /// Depth test `(write, compare)`; `None` = off.
    pub depth_test: Option<(bool, Compare)>,
    pub cull: Cull,
}

pub struct ColorTarget<'a, B: GraphicsBackend + ?Sized> {
    pub texture: &'a B::Texture,
    /// `None` loads the previous contents.
    pub clear: Option<[f32; 4]>,
}

pub struct DepthTarget<'a, B: GraphicsBackend + ?Sized> {
    pub texture: &'a B::Texture,
    pub clear: Option<f32>,
}

#[derive(Clone, Copy, Debug)]
pub enum DrawCall {
    Arrays {
        vertex_count: u32,
        instance_count: u32,
        first_vertex: u32,
        first_instance: u32,
    },
    /// `u32` indices when `wide`, else `u16`, starting `first_index` into the index buffer.
    Indexed {
        index_count: u32,
        instance_count: u32,
        first_index: u32,
        base_vertex: i32,
        first_instance: u32,
        wide: bool,
    },
}

pub struct Draw<'a, B: GraphicsBackend + ?Sized> {
    pub pipeline: &'a B::RenderPipeline,
    pub topology: Topology,
    pub root: RootArgs,
    pub heaps: Heaps<'a, B>,
    pub resources: Vec<&'a B::Buffer>,
    /// `(buffer, title address)` per vertex stream, in `streams` order.
    pub vertex_buffers: Vec<(&'a B::Buffer, *const u8)>,
    pub index_buffer: Option<(&'a B::Buffer, *const u8)>,
    pub call: DrawCall,
}

pub struct RenderPass<'a, B: GraphicsBackend + ?Sized> {
    pub color: Vec<ColorTarget<'a, B>>,
    pub depth: Option<DepthTarget<'a, B>>,
    pub draws: Vec<Draw<'a, B>>,
}

/// A GPU-side buffer operation on title memory (`CopyBufferRegion`, a fill), ordered with the passes around it.
pub enum BlitOp<'a, B: ComputeBackend + ?Sized> {
    Copy {
        src: (&'a B::Buffer, *const u8),
        dst: (&'a B::Buffer, *const u8),
        len: usize,
    },
    Fill {
        dst: (&'a B::Buffer, *const u8),
        len: usize,
        value: u8,
    },
}

pub enum Pass<'a, B: GraphicsBackend + ?Sized> {
    Compute(&'a [Dispatch<'a, B>]),
    Render(RenderPass<'a, B>),
    Blit(Vec<BlitOp<'a, B>>),
}

/// Like `Submission`, with render passes among the dispatches, run in order.
pub struct PassSubmission<'a, B: GraphicsBackend + ?Sized> {
    pub waits: &'a [(&'a B::Fence, u64)],
    pub passes: &'a [Pass<'a, B>],
    pub signals: &'a [(&'a B::Fence, u64)],
}

pub trait GraphicsBackend: ResourceBackend {
    type RenderPipeline;

    /// Converted VS (+ PS) DXIL to a render pipeline: input layout, target formats, depth state, culling.
    fn create_render_pipeline(
        &self,
        desc: &RenderPipelineDesc<'_>,
    ) -> Result<Self::RenderPipeline, String>;
    fn submit_passes(
        &self,
        queue: &Self::Queue,
        work: &PassSubmission<'_, Self>,
    ) -> Result<(), String>;
}
