//! Descriptor heaps, textures, samplers, render pipelines and passes on Metal, plus the one encoder
//! loop (`submit_passes`) that compute `submit` also goes through.
//!
//! Descriptor encoding is `metal_irconverter_runtime.h` re-implemented from its documented layout
//! (`IRDescriptorTableSetBuffer` / `SetTexture` / `SetSampler`, 24-byte entries), so no Apple header
//! is needed at build time.

use crate::backend::{ComputeBackend, Heaps, RootArgs};
use crate::metal::{Buffer, Metal, Queue};
use crate::resources::*;
use objc2::Message;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSRange;
use objc2_metal::*;
use std::cell::{Cell, RefCell};
use std::ptr::NonNull;

type Res = Retained<ProtocolObject<dyn MTLResource>>;

/// A fence chaining the encoders of one command buffer, and whether one has run: what a pass waits
/// for and updates when the queue's buffers are made resident by a set instead of named.
struct Order {
    fence: Option<Retained<ProtocolObject<dyn MTLFence>>>,
    started: Cell<bool>,
}

type Ordering<'a> = Option<(&'a ProtocolObject<dyn MTLFence>, &'a Cell<bool>)>;

/// `IRDescriptorTableEntry`: `{ gpuVA, textureViewID, metadata }`.
const ENTRY: usize = 24;

/// `kIRDescriptorHeapBindPoint`, `kIRSamplerHeapBindPoint`, `kIRArgumentBufferBindPoint`,
/// `kIRArgumentBufferDrawArgumentsBindPoint`, `kIRArgumentBufferUniformsBindPoint`,
/// `kIRVertexBufferBindPoint`, `kIRStageInAttributeStartIndex` (`metal_irconverter_runtime.h`).
const HEAP_SLOT: [usize; 2] = [0, 1];
const ARGUMENT_BUFFER: usize = 2;
const DRAW_ARGUMENTS: usize = 4;
const UNIFORMS: usize = 5;
const VERTEX_BUFFERS: usize = 6;
const STAGE_IN_ATTRIBUTES: usize = 11;

pub struct Heap {
    buf: Retained<ProtocolObject<dyn MTLBuffer>>,
    kind: HeapKind,
    count: usize,
    /// What each entry names, kept alive here and made resident with the heap.
    refs: RefCell<std::collections::HashMap<usize, Res>>,
    /// Reference counts per raw `MTLResource` pointer currently in `set`, so an allocation is
    /// added once and removed + committed before its last `refs` entry is dropped.
    alloc_counts: RefCell<std::collections::HashMap<usize, usize>>,
    /// Residency set holding the heap buffer and everything its entries name.
    set: Option<Retained<ProtocolObject<dyn MTLResidencySet>>>,
    dirty: Cell<bool>,
}

impl Drop for Heap {
    fn drop(&mut self) {
        // MTLResidencySet does not retain allocations: detach everything and commit before
        // `buf` and `refs` are dropped.
        if let Some(set) = self.set.take() {
            set.removeAllAllocations();
            set.commit();
        }
    }
}

pub struct Texture {
    tex: Retained<ProtocolObject<dyn MTLTexture>>,
}

impl Texture {
    /// The texture as the bytes it holds when it is sRGB (a view in the plain format), else itself.
    pub(crate) fn view_without_srgb(&self, _m: &Metal) -> Retained<ProtocolObject<dyn MTLTexture>> {
        let plain = match self.tex.pixelFormat() {
            MTLPixelFormat::RGBA8Unorm_sRGB => MTLPixelFormat::RGBA8Unorm,
            MTLPixelFormat::BGRA8Unorm_sRGB => MTLPixelFormat::BGRA8Unorm,
            _ => return self.tex.clone(),
        };
        self.tex
            .newTextureViewWithPixelFormat(plain)
            .unwrap_or_else(|| self.tex.clone())
    }
}

pub struct Sampler(Retained<ProtocolObject<dyn MTLSamplerState>>);

pub struct RenderPipeline {
    pso: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    depth: Option<Retained<ProtocolObject<dyn MTLDepthStencilState>>>,
    cull: MTLCullMode,
    raster: Raster,
    /// The pipeline's target has a stencil plane (the pass attaches it too).
    stencil: bool,
    /// Each stage's static samplers, which the stage's argument buffer points at.
    vs_static: Option<StaticSamplers>,
    ps_static: Option<StaticSamplers>,
}

/// One shader stage's static samplers, as Apple's converter reads them: not built into the
/// shader, but one table of `IRDescriptorTableEntry` (sampler, 0, LOD bias), `s0` first, whose
/// address the stage's top-level argument buffer holds at `offset`, right after the root
/// parameters. Nothing wrote that address before: the shader read a sampler through whatever
/// bytes followed the parameters, which sampled zeros on M1 and M2 (Apple7, Apple8) GPUs and
/// left the window black at the title's present pass (Native-Victory-macOS #26).
pub(crate) struct StaticSamplers {
    table: Retained<ProtocolObject<dyn MTLBuffer>>,
    offset: usize,
    /// The table holds only their resource IDs.
    _samplers: Vec<Retained<ProtocolObject<dyn MTLSamplerState>>>,
}

impl StaticSamplers {
    /// `root` with the table's address at its offset.
    fn args<'a>(t: Option<&Self>, root: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        let Some(t) = t else { return root.into() };
        let mut v = root.to_vec();
        if v.len() < t.offset + 8 {
            v.resize(t.offset + 8, 0);
        }
        v[t.offset..t.offset + 8].copy_from_slice(&t.table.gpuAddress().to_le_bytes());
        v.into()
    }

    fn table(t: Option<&Self>) -> Option<&ProtocolObject<dyn MTLResource>> {
        t.map(|t| ProtocolObject::from_ref(&*t.table))
    }
}

fn pixel_format(f: Format) -> MTLPixelFormat {
    match f {
        Format::Rgba8Unorm => MTLPixelFormat::RGBA8Unorm,
        Format::Rgba8UnormSrgb => MTLPixelFormat::RGBA8Unorm_sRGB,
        Format::Bgra8Unorm => MTLPixelFormat::BGRA8Unorm,
        Format::Bgra8UnormSrgb => MTLPixelFormat::BGRA8Unorm_sRGB,
        Format::Rgba16Float => MTLPixelFormat::RGBA16Float,
        Format::R32Float => MTLPixelFormat::R32Float,
        Format::D32Float => MTLPixelFormat::Depth32Float,
        Format::D32FloatS8 => MTLPixelFormat::Depth32Float_Stencil8,
        Format::Bc1 => MTLPixelFormat::BC1_RGBA,
        Format::Bc2 => MTLPixelFormat::BC2_RGBA,
        Format::Bc3 => MTLPixelFormat::BC3_RGBA,
        Format::Bc4 => MTLPixelFormat::BC4_RUnorm,
        Format::Bc5 => MTLPixelFormat::BC5_RGUnorm,
        Format::Bc6h => MTLPixelFormat::BC6H_RGBFloat,
        Format::Bc7 => MTLPixelFormat::BC7_RGBAUnorm,
        Format::R8Unorm => MTLPixelFormat::R8Unorm,
        Format::A8Unorm => MTLPixelFormat::A8Unorm,
        Format::R16Unorm => MTLPixelFormat::R16Unorm,
        Format::Rgba32Float => MTLPixelFormat::RGBA32Float,
        Format::Rg32Float => MTLPixelFormat::RG32Float,
        Format::Rgb10a2Unorm => MTLPixelFormat::RGB10A2Unorm,
        Format::Rg11b10Float => MTLPixelFormat::RG11B10Float,
        Format::Rgba8Uint => MTLPixelFormat::RGBA8Uint,
        Format::Rg16Float => MTLPixelFormat::RG16Float,
        Format::Rg16Unorm => MTLPixelFormat::RG16Unorm,
        Format::R32Uint => MTLPixelFormat::R32Uint,
        Format::R8Uint => MTLPixelFormat::R8Uint,
        Format::Rgb9e5Float => MTLPixelFormat::RGB9E5Float,
        Format::Bc7Srgb => MTLPixelFormat::BC7_RGBAUnorm_sRGB,
        Format::Rgba32Uint => MTLPixelFormat::RGBA32Uint,
        Format::Rgba16Unorm => MTLPixelFormat::RGBA16Unorm,
        Format::Rgba16Snorm => MTLPixelFormat::RGBA16Snorm,
        Format::Rg32Uint => MTLPixelFormat::RG32Uint,
        Format::Rg16Uint => MTLPixelFormat::RG16Uint,
        Format::Rg8Unorm => MTLPixelFormat::RG8Unorm,
        Format::Rg8Uint => MTLPixelFormat::RG8Uint,
        Format::Rg8Snorm => MTLPixelFormat::RG8Snorm,
        Format::R16Float => MTLPixelFormat::R16Float,
        Format::R16Uint => MTLPixelFormat::R16Uint,
        Format::D16Unorm => MTLPixelFormat::Depth16Unorm,
        Format::Bc1Srgb => MTLPixelFormat::BC1_RGBA_sRGB,
        Format::Bc3Srgb => MTLPixelFormat::BC3_RGBA_sRGB,
        Format::Bgr5a1Unorm => MTLPixelFormat::BGR5A1Unorm,
        Format::Abgr4Unorm => MTLPixelFormat::ABGR4Unorm,
    }
}

fn tex_type(d: TexDim) -> MTLTextureType {
    match d {
        TexDim::D2 => MTLTextureType::Type2D,
        TexDim::D2Array => MTLTextureType::Type2DArray,
        TexDim::D3 => MTLTextureType::Type3D,
    }
}

fn compare(c: Compare) -> MTLCompareFunction {
    match c {
        Compare::Never => MTLCompareFunction::Never,
        Compare::Less => MTLCompareFunction::Less,
        Compare::Equal => MTLCompareFunction::Equal,
        Compare::LessEqual => MTLCompareFunction::LessEqual,
        Compare::Greater => MTLCompareFunction::Greater,
        Compare::NotEqual => MTLCompareFunction::NotEqual,
        Compare::GreaterEqual => MTLCompareFunction::GreaterEqual,
        Compare::Always => MTLCompareFunction::Always,
    }
}

fn blend_factor(f: BlendFactor) -> MTLBlendFactor {
    match f {
        BlendFactor::Zero => MTLBlendFactor::Zero,
        BlendFactor::One => MTLBlendFactor::One,
        BlendFactor::SrcColor => MTLBlendFactor::SourceColor,
        BlendFactor::InvSrcColor => MTLBlendFactor::OneMinusSourceColor,
        BlendFactor::SrcAlpha => MTLBlendFactor::SourceAlpha,
        BlendFactor::InvSrcAlpha => MTLBlendFactor::OneMinusSourceAlpha,
        BlendFactor::DestAlpha => MTLBlendFactor::DestinationAlpha,
        BlendFactor::InvDestAlpha => MTLBlendFactor::OneMinusDestinationAlpha,
        BlendFactor::DestColor => MTLBlendFactor::DestinationColor,
        BlendFactor::InvDestColor => MTLBlendFactor::OneMinusDestinationColor,
        BlendFactor::SrcAlphaSat => MTLBlendFactor::SourceAlphaSaturated,
        BlendFactor::Constant => MTLBlendFactor::BlendColor,
        BlendFactor::InvConstant => MTLBlendFactor::OneMinusBlendColor,
        BlendFactor::Src1Color => MTLBlendFactor::Source1Color,
        BlendFactor::InvSrc1Color => MTLBlendFactor::OneMinusSource1Color,
        BlendFactor::Src1Alpha => MTLBlendFactor::Source1Alpha,
        BlendFactor::InvSrc1Alpha => MTLBlendFactor::OneMinusSource1Alpha,
    }
}

fn blend_op(o: BlendOp) -> MTLBlendOperation {
    match o {
        BlendOp::Add => MTLBlendOperation::Add,
        BlendOp::Subtract => MTLBlendOperation::Subtract,
        BlendOp::ReverseSubtract => MTLBlendOperation::ReverseSubtract,
        BlendOp::Min => MTLBlendOperation::Min,
        BlendOp::Max => MTLBlendOperation::Max,
    }
}

fn stencil_op(o: StencilOp) -> MTLStencilOperation {
    match o {
        StencilOp::Keep => MTLStencilOperation::Keep,
        StencilOp::Zero => MTLStencilOperation::Zero,
        StencilOp::Replace => MTLStencilOperation::Replace,
        StencilOp::IncrementClamp => MTLStencilOperation::IncrementClamp,
        StencilOp::DecrementClamp => MTLStencilOperation::DecrementClamp,
        StencilOp::Invert => MTLStencilOperation::Invert,
        StencilOp::IncrementWrap => MTLStencilOperation::IncrementWrap,
        StencilOp::DecrementWrap => MTLStencilOperation::DecrementWrap,
    }
}

fn stencil_face(f: &StencilFace, s: &StencilState) -> Retained<MTLStencilDescriptor> {
    let d = MTLStencilDescriptor::new();
    d.setStencilFailureOperation(stencil_op(f.fail));
    d.setDepthFailureOperation(stencil_op(f.depth_fail));
    d.setDepthStencilPassOperation(stencil_op(f.pass));
    d.setStencilCompareFunction(compare(f.compare));
    d.setReadMask(u32::from(s.read_mask));
    d.setWriteMask(u32::from(s.write_mask));
    d
}

fn address(a: Address) -> MTLSamplerAddressMode {
    match a {
        Address::Wrap => MTLSamplerAddressMode::Repeat,
        Address::Mirror => MTLSamplerAddressMode::MirrorRepeat,
        Address::Clamp => MTLSamplerAddressMode::ClampToEdge,
    }
}

fn filter(f: Filter) -> MTLSamplerMinMagFilter {
    match f {
        Filter::Nearest => MTLSamplerMinMagFilter::Nearest,
        Filter::Linear => MTLSamplerMinMagFilter::Linear,
    }
}

/// A sampler for a static sampler's `D3D12_STATIC_SAMPLER_DESC` words up to `MaxLOD` (filter,
/// address U, V, W, LOD bias, anisotropy, comparison, border, minimum and maximum LOD). The LOD
/// bias is the table entry's. Minimum and maximum filtering (filter bits 0x100) have no
/// equivalent here and filter normally.
fn static_sampler(
    dev: &ProtocolObject<dyn MTLDevice>,
    w: &[u32; 10],
) -> Result<Retained<ProtocolObject<dyn MTLSamplerState>>, String> {
    let [
        filter,
        u,
        v,
        ww,
        _,
        anisotropy,
        comparison,
        border,
        min_lod,
        max_lod,
    ] = *w;
    let aniso = filter & 0x40 != 0;
    let linear = |bit: u32| {
        if aniso || filter & bit != 0 {
            MTLSamplerMinMagFilter::Linear
        } else {
            MTLSamplerMinMagFilter::Nearest
        }
    };
    let mode = |m: u32| match m {
        1 => MTLSamplerAddressMode::Repeat,
        2 => MTLSamplerAddressMode::MirrorRepeat,
        4 => MTLSamplerAddressMode::ClampToBorderColor,
        5 => MTLSamplerAddressMode::MirrorClampToEdge,
        _ => MTLSamplerAddressMode::ClampToEdge,
    };
    let sd = MTLSamplerDescriptor::new();
    sd.setMinFilter(linear(0x10));
    sd.setMagFilter(linear(0x4));
    sd.setMipFilter(if aniso || filter & 1 != 0 {
        MTLSamplerMipFilter::Linear
    } else {
        MTLSamplerMipFilter::Nearest
    });
    sd.setSAddressMode(mode(u));
    sd.setTAddressMode(mode(v));
    sd.setRAddressMode(mode(ww));
    sd.setMaxAnisotropy(if aniso {
        anisotropy.clamp(1, 16) as usize
    } else {
        1
    });
    if filter & 0x80 != 0 {
        // `D3D12_COMPARISON_FUNC`, 1 (never) to 8 (always).
        let c = [
            Compare::Never,
            Compare::Less,
            Compare::Equal,
            Compare::LessEqual,
            Compare::Greater,
            Compare::NotEqual,
            Compare::GreaterEqual,
            Compare::Always,
        ];
        sd.setCompareFunction(compare(c[(comparison.clamp(1, 8) - 1) as usize]));
    }
    sd.setBorderColor(match border {
        1 => MTLSamplerBorderColor::OpaqueBlack,
        2 => MTLSamplerBorderColor::OpaqueWhite,
        _ => MTLSamplerBorderColor::TransparentBlack,
    });
    let (lo, hi) = (f32::from_bits(min_lod), f32::from_bits(max_lod));
    let lo = if lo.is_nan() { 0.0 } else { lo.max(0.0) };
    sd.setLodMinClamp(lo);
    sd.setLodMaxClamp(if hi.is_nan() { f32::MAX } else { hi.max(lo) });
    // The table holds the sampler's resource ID, which exists only for argument-buffer samplers.
    sd.setSupportArgumentBuffers(true);
    dev.newSamplerStateWithDescriptor(&sd)
        .ok_or_else(|| "newSamplerState failed for a static sampler".into())
}

fn vertex_format(f: VertexFormat) -> MTLVertexFormat {
    match f {
        VertexFormat::Float => MTLVertexFormat::Float,
        VertexFormat::Float2 => MTLVertexFormat::Float2,
        VertexFormat::Float3 => MTLVertexFormat::Float3,
        VertexFormat::Float4 => MTLVertexFormat::Float4,
        VertexFormat::Uint => MTLVertexFormat::UInt,
        VertexFormat::Uint2 => MTLVertexFormat::UInt2,
        VertexFormat::Uint3 => MTLVertexFormat::UInt3,
        VertexFormat::Uint4 => MTLVertexFormat::UInt4,
        VertexFormat::Sint => MTLVertexFormat::Int,
        VertexFormat::Sint2 => MTLVertexFormat::Int2,
        VertexFormat::Sint3 => MTLVertexFormat::Int3,
        VertexFormat::Sint4 => MTLVertexFormat::Int4,
        VertexFormat::Half => MTLVertexFormat::Half,
        VertexFormat::Half2 => MTLVertexFormat::Half2,
        VertexFormat::Half4 => MTLVertexFormat::Half4,
        VertexFormat::Unorm16 => MTLVertexFormat::UShortNormalized,
        VertexFormat::Unorm16x2 => MTLVertexFormat::UShort2Normalized,
        VertexFormat::Unorm16x4 => MTLVertexFormat::UShort4Normalized,
        VertexFormat::Snorm16 => MTLVertexFormat::ShortNormalized,
        VertexFormat::Snorm16x2 => MTLVertexFormat::Short2Normalized,
        VertexFormat::Snorm16x4 => MTLVertexFormat::Short4Normalized,
        VertexFormat::Uint16 => MTLVertexFormat::UShort,
        VertexFormat::Uint16x2 => MTLVertexFormat::UShort2,
        VertexFormat::Uint16x4 => MTLVertexFormat::UShort4,
        VertexFormat::Sint16 => MTLVertexFormat::Short,
        VertexFormat::Sint16x2 => MTLVertexFormat::Short2,
        VertexFormat::Sint16x4 => MTLVertexFormat::Short4,
        VertexFormat::Unorm8 => MTLVertexFormat::UCharNormalized,
        VertexFormat::Unorm8x2 => MTLVertexFormat::UChar2Normalized,
        VertexFormat::Unorm8x4 => MTLVertexFormat::UChar4Normalized,
        VertexFormat::Snorm8 => MTLVertexFormat::CharNormalized,
        VertexFormat::Snorm8x2 => MTLVertexFormat::Char2Normalized,
        VertexFormat::Snorm8x4 => MTLVertexFormat::Char4Normalized,
        VertexFormat::Uint8 => MTLVertexFormat::UChar,
        VertexFormat::Uint8x2 => MTLVertexFormat::UChar2,
        VertexFormat::Uint8x4 => MTLVertexFormat::UChar4,
        VertexFormat::Sint8 => MTLVertexFormat::Char,
        VertexFormat::Sint8x2 => MTLVertexFormat::Char2,
        VertexFormat::Sint8x4 => MTLVertexFormat::Char4,
        VertexFormat::Unorm1010102 => MTLVertexFormat::UInt1010102Normalized,
        VertexFormat::Snorm1010102 => MTLVertexFormat::Int1010102Normalized,
        VertexFormat::FloatRG11B10 => MTLVertexFormat::FloatRG11B10,
    }
}

/// `MTLResourceID` is a `#[repr(transparent)]` u64 whose field is crate-private in `objc2-metal`.
fn resource_id(id: MTLResourceID) -> u64 {
    // SAFETY: MTLResourceID is one u64 (`Encode`d as such).
    unsafe { std::mem::transmute::<MTLResourceID, u64>(id) }
}

fn size(a: [usize; 3]) -> MTLSize {
    MTLSize {
        width: a[0],
        height: a[1],
        depth: a[2],
    }
}

impl Metal {
    fn heap_entry(&self, heap: &Heap, index: usize, words: [u64; 3], keep: Option<Res>) {
        assert!(
            index < heap.count,
            "descriptor index {index} out of {}",
            heap.count
        );
        // SAFETY: `index` is inside the heap buffer, which is shared memory the GPU is not
        // reading while the title writes descriptors (D3D12 has the same rule).
        unsafe {
            let p = heap
                .buf
                .contents()
                .as_ptr()
                .cast::<u64>()
                .add(index * ENTRY / 8);
            words
                .iter()
                .enumerate()
                .for_each(|(i, w)| p.add(i).write_volatile(*w));
        }
        let old = match keep {
            Some(r) => {
                let ptr = (&*r as *const ProtocolObject<dyn MTLResource>).cast::<()>() as usize;
                let mut counts = heap.alloc_counts.borrow_mut();
                let count = counts.entry(ptr).or_insert(0);
                if *count == 0
                    && let Some(set) = &heap.set
                {
                    set.addAllocation(ProtocolObject::from_ref(&*r));
                    heap.dirty.set(true);
                }
                *count += 1;
                drop(counts);
                heap.refs.borrow_mut().insert(index, r)
            }
            None => heap.refs.borrow_mut().remove(&index),
        };
        if let Some(old) = old {
            let old_ptr = (&*old as *const ProtocolObject<dyn MTLResource>).cast::<()>() as usize;
            let mut counts = heap.alloc_counts.borrow_mut();
            let last = match counts.get_mut(&old_ptr) {
                Some(n) if *n > 1 => {
                    *n -= 1;
                    false
                }
                Some(_) => {
                    counts.remove(&old_ptr);
                    true
                }
                None => false,
            };
            drop(counts);
            if last && let Some(set) = &heap.set {
                set.removeAllocation(ProtocolObject::from_ref(&*old));
                set.commit();
                heap.dirty.set(false);
            }
            drop(old);
        }
    }

    /// Residency for one command buffer: the heap's set, committed if entries changed.
    fn use_heap_cb(&self, cb: &ProtocolObject<dyn MTLCommandBuffer>, heap: &Heap) {
        if let Some(set) = &heap.set {
            if heap.dirty.replace(false) {
                set.commit();
            }
            cb.useResidencySet(set);
        }
    }

    /// Residency without sets: every entry's resource is named to the encoder.
    fn use_heap_encoder(
        &self,
        heap: &Heap,
        mut use_res: impl FnMut(&ProtocolObject<dyn MTLResource>),
    ) {
        if heap.set.is_some() {
            return;
        }
        use_res(ProtocolObject::from_ref(&*heap.buf));
        heap.refs.borrow().values().for_each(|r| use_res(r));
    }

    /// The static samplers of one stage's `layout`, if it declares any.
    pub(crate) fn static_samplers(
        &self,
        layout: Option<&victory_backend::backend::RootLayout>,
    ) -> Result<Option<StaticSamplers>, String> {
        let Some(l) = layout.filter(|l| !l.static_samplers.is_empty()) else {
            return Ok(None);
        };
        // The converter's layout names the table as a table resource with no register.
        // With no table in the layout there is nothing to fill: the pipeline is still made.
        let Some(offset) = self
            .msc()?
            .locations(l)
            .map_err(|e| e.to_string())?
            .iter()
            .find(|&&(kind, space, slot, ..)| kind == 0 && space == u32::MAX && slot == u32::MAX)
            .map(|&(.., offset, _)| offset as usize)
        else {
            return Ok(None);
        };
        let table = self
            .dev
            .newBufferWithLength_options(
                l.static_samplers.len() * ENTRY,
                MTLResourceOptions::StorageModeShared,
            )
            .ok_or("no static sampler table")?;
        let mut samplers = Vec::new();
        for (n, w) in l.static_samplers.iter().enumerate() {
            let s = static_sampler(&self.dev, w)?;
            let bias = f32::from_bits(w[4]);
            let bias = if bias.is_nan() {
                0.0
            } else {
                bias.clamp(-16.0, 15.99)
            };
            let entry = [resource_id(s.gpuResourceID()), 0, u64::from(bias.to_bits())];
            // SAFETY: entry `n` is inside the table, which no GPU work reads yet.
            unsafe {
                let p = table.contents().as_ptr().cast::<u64>().add(n * ENTRY / 8);
                entry
                    .iter()
                    .enumerate()
                    .for_each(|(i, w)| p.add(i).write(*w));
            }
            samplers.push(s);
        }
        Ok(Some(StaticSamplers {
            table,
            offset,
            _samplers: samplers,
        }))
    }

    fn stage_function(
        &self,
        dxil: &[u8],
        layout: Option<&victory_backend::backend::RootLayout>,
    ) -> Result<Retained<ProtocolObject<dyn MTLFunction>>, String> {
        let (_, lib) = self
            .msc()?
            .convert_with(dxil, None, layout)
            .map_err(|e| e.to_string())?;
        let l = self.load_library(&lib)?;
        let name = l
            .functionNames()
            .iter()
            .next()
            .ok_or("converted library is empty")?;
        l.newFunctionWithName(&name)
            .ok_or_else(|| "no function".into())
    }

    fn bind_root_compute(
        &self,
        e: &ProtocolObject<dyn MTLComputeCommandEncoder>,
        root: &RootArgs,
        heaps: &Heaps<'_, Metal>,
        statics: Option<&StaticSamplers>,
    ) {
        let root = StaticSamplers::args(statics, &root.0);
        if !root.is_empty() {
            // SAFETY: setBytes copies `root` (at most 4 KiB) before returning.
            unsafe {
                e.setBytes_length_atIndex(
                    NonNull::new(root.as_ptr().cast_mut().cast()).unwrap(),
                    root.len(),
                    ARGUMENT_BUFFER,
                )
            };
        }
        if let Some(t) = StaticSamplers::table(statics) {
            e.useResource_usage(t, MTLResourceUsage::Read);
        }
        for (h, slot) in [
            (heaps.resource, HEAP_SLOT[0]),
            (heaps.sampler, HEAP_SLOT[1]),
        ] {
            if let Some(h) = h {
                // SAFETY: a live buffer at an in-range slot.
                unsafe { e.setBuffer_offset_atIndex(Some(&h.buf), 0, slot) };
                self.use_heap_encoder(h, |r| {
                    e.useResource_usage(r, MTLResourceUsage::Read | MTLResourceUsage::Write)
                });
            }
        }
    }

    fn bind_root_render(
        &self,
        e: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        root: &RootArgs,
        fragment: Option<&RootArgs>,
        heaps: &Heaps<'_, Metal>,
        pipeline: &RenderPipeline,
    ) {
        let fragment = fragment.unwrap_or(root);
        let (vs, ps) = (pipeline.vs_static.as_ref(), pipeline.ps_static.as_ref());
        let (root, fragment) = (
            StaticSamplers::args(vs, &root.0),
            StaticSamplers::args(ps, &fragment.0),
        );
        // SAFETY: setBytes copies the bytes before returning.
        unsafe {
            if !root.is_empty() {
                let p = NonNull::new(root.as_ptr().cast_mut().cast()).unwrap();
                e.setVertexBytes_length_atIndex(p, root.len(), ARGUMENT_BUFFER);
            }
            if !fragment.is_empty() {
                let p = NonNull::new(fragment.as_ptr().cast_mut().cast()).unwrap();
                e.setFragmentBytes_length_atIndex(p, fragment.len(), ARGUMENT_BUFFER);
            }
        }
        for (t, stage) in [
            (vs, MTLRenderStages::Vertex),
            (ps, MTLRenderStages::Fragment),
        ] {
            if let Some(t) = StaticSamplers::table(t) {
                e.useResource_usage_stages(t, MTLResourceUsage::Read, stage);
            }
        }
        for (h, slot) in [
            (heaps.resource, HEAP_SLOT[0]),
            (heaps.sampler, HEAP_SLOT[1]),
        ] {
            if let Some(h) = h {
                // SAFETY: a live buffer at an in-range slot.
                unsafe {
                    e.setVertexBuffer_offset_atIndex(Some(&h.buf), 0, slot);
                    e.setFragmentBuffer_offset_atIndex(Some(&h.buf), 0, slot);
                }
                self.use_heap_encoder(h, |r| {
                    e.useResource_usage_stages(
                        r,
                        MTLResourceUsage::Read | MTLResourceUsage::Write,
                        MTLRenderStages::Vertex | MTLRenderStages::Fragment,
                    )
                });
            }
        }
    }
}

impl ResourceBackend for Metal {
    type Texture = Texture;
    type Sampler = Sampler;

    fn create_heap(&self, kind: HeapKind, count: usize) -> Result<Heap, String> {
        let buf = self
            .dev
            .newBufferWithLength_options(count * ENTRY, MTLResourceOptions::StorageModeShared)
            .ok_or("descriptor heap allocation failed")?;
        let set = if self.residency_sets {
            let d = MTLResidencySetDescriptor::new();
            let s = self
                .dev
                .newResidencySetWithDescriptor_error(&d)
                .map_err(|e| e.localizedDescription().to_string())?;
            s.addAllocation(ProtocolObject::from_ref(&*buf));
            s.commit();
            Some(s)
        } else {
            None
        };
        Ok(Heap {
            buf,
            kind,
            count,
            refs: RefCell::default(),
            alloc_counts: RefCell::default(),
            set,
            dirty: Cell::new(false),
        })
    }

    fn heap_address(&self, heap: &Heap, index: usize) -> u64 {
        heap.buf.gpuAddress() + (index * ENTRY) as u64
    }

    fn write_buffer(
        &self,
        heap: &Heap,
        index: usize,
        buffer: &Buffer,
        title: *const u8,
        size: u32,
    ) {
        debug_assert_eq!(heap.kind, HeapKind::Resource);
        let gpu = self.gpu_address(buffer, title);
        let keep: Res = ProtocolObject::from_ref(&*buffer.buf).retain();
        // IRDescriptorTableSetBuffer(entry, gpu, metadata = size & 0xffffffff).
        self.heap_entry(heap, index, [gpu, 0, size as u64], Some(keep));
    }

    fn write_texture(&self, heap: &Heap, index: usize, t: &Texture, min_lod: f32) {
        debug_assert_eq!(heap.kind, HeapKind::Resource);
        let id = resource_id(t.tex.gpuResourceID());
        let keep: Res = ProtocolObject::from_ref(&*t.tex).retain();
        // IRDescriptorTableSetTexture(entry, tex, minLODClamp, metadata = 0).
        self.heap_entry(heap, index, [0, id, min_lod.to_bits() as u64], Some(keep));
    }

    fn write_sampler(&self, heap: &Heap, index: usize, s: &Sampler, lod_bias: f32) {
        debug_assert_eq!(heap.kind, HeapKind::Sampler);
        let id = resource_id(s.0.gpuResourceID());
        // IRDescriptorTableSetSampler(entry, sampler, lodBias): samplers need no residency.
        self.heap_entry(heap, index, [id, 0, lod_bias.to_bits() as u64], None);
    }

    fn create_texture(&self, d: &TextureDesc) -> Result<Texture, String> {
        let td = MTLTextureDescriptor::new();
        td.setTextureType(tex_type(d.dim));
        td.setPixelFormat(pixel_format(d.format));
        // SAFETY: plain size properties of a fresh descriptor; Metal validates them at creation.
        unsafe {
            td.setWidth(d.width);
            td.setHeight(d.height);
            td.setDepth(if d.dim == TexDim::D3 {
                d.depth_or_layers
            } else {
                1
            });
            td.setArrayLength(if d.dim == TexDim::D2Array {
                d.depth_or_layers
            } else {
                1
            });
            td.setMipmapLevelCount(d.mips);
        }
        let mut usage = MTLTextureUsage::ShaderRead | MTLTextureUsage::PixelFormatView;
        if d.unordered_access {
            usage |= MTLTextureUsage::ShaderWrite;
        }
        if d.render_target {
            usage |= MTLTextureUsage::RenderTarget;
        }
        td.setUsage(usage);
        td.setStorageMode(MTLStorageMode::Shared);
        let tex = self
            .dev
            .newTextureWithDescriptor(&td)
            .ok_or("newTextureWithDescriptor failed")?;
        Ok(Texture { tex })
    }

    fn create_texture_view(&self, t: &Texture, v: &ViewDesc) -> Result<Texture, String> {
        let ty = t.tex.textureType();
        // A depth texture cannot be viewed as a color format (D3D's R32_FLOAT
        // over a depth buffer): it keeps its own, and shaders read it as the
        // float it holds.
        let source = t.tex.pixelFormat();
        let format = if is_depth(source) {
            source
        } else {
            pixel_format(v.format)
        };
        // Metal asserts (it does not return nil) on a range past the texture, which a
        // title view asks for over a texture we made with fewer levels or layers (one
        // over title memory has one level): the range is cut to what is there.
        let fit = |(start, len): (usize, usize), count: usize| {
            (start < count).then(|| NSRange::new(start, len.min(count - start)))
        };
        let slices = if ty == MTLTextureType::Type2DArray {
            t.tex.arrayLength()
        } else {
            1
        };
        let (Some(levels), Some(layers)) =
            (fit(v.mips, t.tex.mipmapLevelCount()), fit(v.slices, slices))
        else {
            return Err(format!(
                "view {:?} {:?} is past the texture",
                v.mips, v.slices
            ));
        };
        // SAFETY: the ranges are inside the texture.
        let tex = unsafe {
            t.tex
                .newTextureViewWithPixelFormat_textureType_levels_slices(format, ty, levels, layers)
        }
        .ok_or("newTextureView refused (format not view-compatible)")?;
        Ok(Texture { tex })
    }

    fn create_texture_over_memory(
        &self,
        buffer: &Buffer,
        title: *const u8,
        d: &TextureDesc,
        row_pitch: usize,
    ) -> Result<Texture, String> {
        // SAFETY: plain sizes; Metal validates them at creation.
        let td = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                pixel_format(d.format),
                d.width,
                d.height,
                false,
            )
        };
        td.setStorageMode(MTLStorageMode::Shared);
        let mut usage = MTLTextureUsage::ShaderRead | MTLTextureUsage::PixelFormatView;
        if d.unordered_access {
            usage |= MTLTextureUsage::ShaderWrite;
        }
        td.setUsage(usage);
        let offset = title as usize - buffer.base as usize;
        let tex = buffer
            .buf
            .newTextureWithDescriptor_offset_bytesPerRow(&td, offset, row_pitch)
            .ok_or("buffer-backed texture refused (offset, pitch or format)")?;
        Ok(Texture { tex })
    }

    fn linear_alignment(&self, f: Format) -> usize {
        self.dev
            .minimumLinearTextureAlignmentForPixelFormat(pixel_format(f))
    }

    fn upload_texture(
        &self,
        t: &Texture,
        mip: usize,
        slice: usize,
        data: &[u8],
        row_pitch: usize,
        slice_pitch: usize,
    ) {
        let (w, h, d) = (
            (t.tex.width() >> mip).max(1),
            (t.tex.height() >> mip).max(1),
            (t.tex.depth() >> mip).max(1),
        );
        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: size([w, h, d]),
        };
        // SAFETY: `data` covers the region at the given pitches (callers size it by `Format::row_bytes`).
        unsafe {
            t.tex
                .replaceRegion_mipmapLevel_slice_withBytes_bytesPerRow_bytesPerImage(
                    region,
                    mip,
                    slice,
                    NonNull::new(data.as_ptr().cast_mut().cast()).unwrap(),
                    row_pitch,
                    slice_pitch,
                )
        };
    }

    fn read_texture(
        &self,
        t: &Texture,
        mip: usize,
        slice: usize,
        out: &mut [u8],
        row_pitch: usize,
        slice_pitch: usize,
    ) {
        let (w, h, d) = (
            (t.tex.width() >> mip).max(1),
            (t.tex.height() >> mip).max(1),
            (t.tex.depth() >> mip).max(1),
        );
        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: size([w, h, d]),
        };
        // SAFETY: `out` covers the region at the given pitches.
        unsafe {
            t.tex
                .getBytes_bytesPerRow_bytesPerImage_fromRegion_mipmapLevel_slice(
                    NonNull::new(out.as_mut_ptr().cast()).unwrap(),
                    row_pitch,
                    slice_pitch,
                    region,
                    mip,
                    slice,
                )
        };
    }

    fn create_sampler(&self, d: &SamplerDesc) -> Result<Sampler, String> {
        let sd = MTLSamplerDescriptor::new();
        sd.setMinFilter(filter(d.min_mag));
        sd.setMagFilter(filter(d.min_mag));
        sd.setMipFilter(match d.mip {
            Filter::Nearest => MTLSamplerMipFilter::Nearest,
            Filter::Linear => MTLSamplerMipFilter::Linear,
        });
        sd.setSAddressMode(address(d.address[0]));
        sd.setTAddressMode(address(d.address[1]));
        sd.setRAddressMode(address(d.address[2]));
        sd.setMaxAnisotropy(d.max_anisotropy.max(1));
        // The heap entry stores the sampler's resource ID, which exists only for argument-buffer samplers.
        sd.setSupportArgumentBuffers(true);
        let s = self
            .dev
            .newSamplerStateWithDescriptor(&sd)
            .ok_or("newSamplerState failed")?;
        Ok(Sampler(s))
    }
}

impl GraphicsBackend for Metal {
    type RenderPipeline = RenderPipeline;

    fn create_render_pipeline(&self, d: &RenderPipelineDesc<'_>) -> Result<RenderPipeline, String> {
        let vf = self.stage_function(d.vs, d.vs_layout)?;
        let pd = MTLRenderPipelineDescriptor::new();
        pd.setVertexFunction(Some(&vf));
        if let Some(ps) = d.ps {
            pd.setFragmentFunction(Some(&*self.stage_function(ps, d.ps_layout)?));
        }
        // The converter's stage-in function takes its inputs at attribute indices from
        // `kIRStageInAttributeStartIndex`, in input-signature order; buffers from `kIRVertexBufferBindPoint`.
        let vd = MTLVertexDescriptor::new();
        // SAFETY: indices are below the attribute and buffer limits for sane layouts; plain sizes.
        unsafe {
            for (i, a) in d.attributes.iter().enumerate() {
                let at = vd
                    .attributes()
                    .objectAtIndexedSubscript(STAGE_IN_ATTRIBUTES + i);
                at.setFormat(vertex_format(a.format));
                at.setOffset(a.offset);
                at.setBufferIndex(VERTEX_BUFFERS + a.slot);
            }
            for s in d.streams {
                let l = vd
                    .layouts()
                    .objectAtIndexedSubscript(VERTEX_BUFFERS + s.slot);
                l.setStride(s.stride);
                l.setStepFunction(if s.per_instance {
                    MTLVertexStepFunction::PerInstance
                } else {
                    MTLVertexStepFunction::PerVertex
                });
            }
        }
        pd.setVertexDescriptor(Some(&vd));
        for (i, f) in d.color.iter().enumerate() {
            // SAFETY: colour attachment index below 8.
            let a = unsafe { pd.colorAttachments().objectAtIndexedSubscript(i) };
            a.setPixelFormat(pixel_format(*f));
            let state = d.color_state.get(i).copied().unwrap_or_default();
            let mut mask = MTLColorWriteMask::empty();
            for (bit, channel) in [
                MTLColorWriteMask::Red,
                MTLColorWriteMask::Green,
                MTLColorWriteMask::Blue,
                MTLColorWriteMask::Alpha,
            ]
            .into_iter()
            .enumerate()
            {
                if state.write_mask >> bit & 1 != 0 {
                    mask |= channel;
                }
            }
            a.setWriteMask(mask);
            if let Some(b) = state.blend.filter(|_| !f.is_integer()) {
                a.setBlendingEnabled(true);
                a.setSourceRGBBlendFactor(blend_factor(b.src));
                a.setDestinationRGBBlendFactor(blend_factor(b.dst));
                a.setRgbBlendOperation(blend_op(b.op));
                a.setSourceAlphaBlendFactor(blend_factor(b.src_alpha));
                a.setDestinationAlphaBlendFactor(blend_factor(b.dst_alpha));
                a.setAlphaBlendOperation(blend_op(b.op_alpha));
            }
        }
        let has_stencil = d.depth == Some(Format::D32FloatS8);
        if let Some(f) = d.depth {
            pd.setDepthAttachmentPixelFormat(pixel_format(f));
            if has_stencil {
                pd.setStencilAttachmentPixelFormat(pixel_format(f));
            }
        }
        let pso = match self.archive() {
            Some(a) => a.render(&self.dev, &pd)?,
            None => self
                .dev
                .newRenderPipelineStateWithDescriptor_error(&pd)
                .map_err(|e| e.localizedDescription().to_string())?,
        };
        let depth = (d.depth_test.is_some() || (d.stencil.is_some() && has_stencil)).then(|| {
            let dd = MTLDepthStencilDescriptor::new();
            let (write, cmp) = d.depth_test.unwrap_or((false, Compare::Always));
            dd.setDepthCompareFunction(compare(cmp));
            dd.setDepthWriteEnabled(write);
            if let Some(s) = d.stencil.filter(|_| has_stencil) {
                dd.setFrontFaceStencil(Some(&stencil_face(&s.front, &s)));
                dd.setBackFaceStencil(Some(&stencil_face(&s.back, &s)));
            }
            self.dev
                .newDepthStencilStateWithDescriptor(&dd)
                .expect("depth state")
        });
        let cull = match d.cull {
            Cull::None => MTLCullMode::None,
            Cull::Front => MTLCullMode::Front,
            Cull::Back => MTLCullMode::Back,
        };
        Ok(RenderPipeline {
            pso,
            depth,
            cull,
            raster: d.raster,
            stencil: has_stencil,
            vs_static: self.static_samplers(d.vs_layout)?,
            ps_static: if d.ps.is_some() {
                self.static_samplers(d.ps_layout)?
            } else {
                None
            },
        })
    }

    fn submit_passes(&self, queue: &Queue, work: &PassSubmission<'_, Self>) -> Result<(), String> {
        if self.trace_fails {
            report_in_flight();
            self.trace_enc.borrow_mut().clear();
        }
        let cb = if self.trace_fails {
            let d = MTLCommandBufferDescriptor::new();
            d.setErrorOptions(MTLCommandBufferErrorOption::EncoderExecutionStatus);
            queue.0.commandBufferWithDescriptor(&d)
        } else {
            queue.0.commandBuffer()
        }
        .ok_or("no command buffer")?;
        let mut serial = 0;
        let mut label = String::new();
        if self.trace_fails {
            serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            label = format!(
                "q{:x} #{} t={:.3} waits={} signals={} passes={}",
                Retained::as_ptr(&queue.0) as *const u8 as usize,
                serial,
                crate::metal::since_start(),
                work.waits.len(),
                work.signals.len(),
                work.passes.len()
            );
            cb.setLabel(Some(&objc2_foundation::NSString::from_str(&label)));
        }
        for (f, v) in work.waits {
            cb.encodeWaitForEvent_value(ProtocolObject::from_ref(&*f.0), *v);
        }
        // With the pool set on the queue the encoders do not name the title's buffers, so Metal
        // cannot order them: this command buffer starts after the previous one finished, and each
        // encoder after the one before.
        let set_on_queue = queue.1.get();
        let order = set_on_queue.then(|| {
            let n = queue.3.get();
            queue.3.set(n + 1);
            if n > 0 {
                cb.encodeWaitForEvent_value(ProtocolObject::from_ref(&*queue.2), n);
            }
            Order {
                fence: self.dev.newFence(),
                started: Cell::new(false),
            }
        });
        let order = order.as_ref().and_then(|o| o.fence.as_ref().map(|f| (&**f, &o.started)));
        // A pass that cannot be encoded must not take the submission's fence signals and
        // bottom-of-pipe value writes with it: the title's compute queues wait for those writes,
        // and a lost one is `Unsignaled CrossQueueBarrier` (the title then dies). The failing pass
        // is dropped, the command buffer still commits, and the caller is told.
        let mut refused: Option<String> = None;
        for pass in work.passes {
            let encoded = match pass {
                Pass::Compute(dispatches) => self.encode_compute(&cb, dispatches, set_on_queue, order),
                Pass::Render(rp) => self.encode_render(&cb, rp, set_on_queue, order),
                Pass::Blit(ops) => self.encode_blit(&cb, ops, order),
            };
            if let Err(e) = encoded {
                // A pass that cannot be encoded (a render target with no attachments, a piece
                // whose root arguments were never set) is dropped; the passes after it still run,
                // so one bad pass does not cost the frame's whole picture.
                if refused.is_none() {
                    refused = Some(e);
                }
            }
        }
        if set_on_queue {
            cb.encodeSignalEvent_value(ProtocolObject::from_ref(&*queue.2), queue.3.get());
        }
        for (f, v) in work.signals {
            cb.encodeSignalEvent_value(ProtocolObject::from_ref(&*f.0), *v);
        }
        let owed = Owed::new(queue, work, set_on_queue);
        // A command buffer the GPU fails is logged (the first few), not silently dropped.
        let trace = self.trace_fails;
        let tinfo = trace.then(|| (serial, label, self.trace_enc.borrow().join(" | ")));
        if let Some((s, l, enc)) = &tinfo {
            IN_FLIGHT.lock().unwrap().insert(*s, (std::time::Instant::now(), format!("{l} | {enc}")));
        }
        let failed = block2::RcBlock::new(move |done: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            // SAFETY: Metal passes the command buffer that completed.
            let done = unsafe { done.as_ref() };
            if let Some((s, l, enc)) = &tinfo {
                let started = IN_FLIGHT.lock().unwrap().remove(s).map(|(t, _)| t);
                if done.error().is_none()
                    && let Some(t) = started
                    && t.elapsed().as_secs_f64() > 1.0
                    && TRACE_LINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 100
                {
                    tracing::warn!(
                        "metal: slow command buffer {l} took {:.3}s (gpu {:.3}s, kernel {:.3}s) | {enc}",
                        t.elapsed().as_secs_f64(),
                        done.GPUEndTime() - done.GPUStartTime(),
                        done.kernelEndTime() - done.kernelStartTime()
                    );
                }
            }
            if let Some(e) = done.error() {
                if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < if trace { 200 } else { 20 } {
                    if trace {
                        trace_failure(done, &e);
                    } else {
                        eprintln!("metal: command buffer failed: {e:?} ({})", e.localizedDescription());
                    }
                }
                owed.recover();
            }
        });
        // SAFETY: the block is retained by Metal until it has run.
        unsafe { cb.addCompletedHandler(&*failed as *const _ as *mut _) };
        cb.commit();
        match refused {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// What a submission owes the title once it completes: the order event, the title's fence signals
/// and its value writes (fills). A command buffer the GPU fails does none of them, so the queue and
/// every fence and barrier word waiting on it would hang; `recover` pays them from the CPU.
///
/// Not `Send`: the completed-handler block is only `Fn`, and Metal's objects are thread-safe to use
/// this way. The fills write title memory the pool keeps mapped while the buffer lives (`buf` is
/// held here to keep the wrapper alive).
pub(crate) struct Owed {
    events: Vec<(Retained<ProtocolObject<dyn MTLSharedEvent>>, u64)>,
    /// Buffer, offset, length, byte.
    #[allow(clippy::type_complexity)]
    fills: Vec<(Retained<ProtocolObject<dyn MTLBuffer>>, usize, usize, u8)>,
    signals: usize,
}

impl Owed {
    pub(crate) fn new(queue: &Queue, work: &PassSubmission<'_, Metal>, set_on_queue: bool) -> Owed {
        let mut events = Vec::with_capacity(work.signals.len() + 1);
        if set_on_queue {
            events.push((queue.2.clone(), queue.3.get()));
        }
        events.extend(work.signals.iter().map(|(f, v)| (f.0.clone(), *v)));
        let mut fills = Vec::new();
        for pass in work.passes {
            if let Pass::Blit(ops) = pass {
                for op in ops {
                    // shortcut: copies are not replayed (one may have partly run and its source
                    // may have changed since), when a title needs a failed copy's data upgrade
                    // to a replay that checks the source is unchanged.
                    if let BlitOp::Fill { dst, len, value } = op {
                        fills.push((dst.0.buf.clone(), dst.1 as usize - dst.0.base as usize, *len, *value));
                    }
                }
            }
        }
        Owed { events, fills, signals: work.signals.len() }
    }

    /// Raises each event (never lowers it) and writes each fill, in order.
    pub(crate) fn recover(&self) {
        static WARNED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        for (e, v) in &self.events {
            if e.signaledValue() < *v {
                e.setSignaledValue(*v);
            }
        }
        for (buf, off, len, value) in &self.fills {
            // SAFETY: the range lies inside the wrapped title memory (the caller resolved it).
            unsafe { std::ptr::write_bytes(buf.contents().as_ptr().cast::<u8>().add(*off), *value, *len) };
        }
        if WARNED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 5 {
            tracing::warn!(
                "metal: a command buffer failed; paid {} fence signals and {} value writes on the CPU",
                self.signals,
                self.fills.len()
            );
        }
    }
}

static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Tracing: command buffers committed and not yet completed, by serial: when, and what they carry.
static IN_FLIGHT: std::sync::Mutex<std::collections::BTreeMap<u64, (std::time::Instant, String)>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());
static REPORTED: std::sync::Mutex<std::collections::BTreeSet<u64>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());
/// Slow and in-flight lines printed so far (each capped at 100).
static TRACE_LINES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static FLIGHT_LINES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Logs each command buffer in flight for over 3 s, once (the locks are not held while logging).
fn report_in_flight() {
    let old: Vec<(u64, f64, String)> = IN_FLIGHT
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, (t, _))| t.elapsed().as_secs_f64() > 3.0)
        .map(|(s, (t, d))| (*s, t.elapsed().as_secs_f64(), d.clone()))
        .collect();
    for (s, secs, d) in old {
        if REPORTED.lock().unwrap().insert(s)
            && FLIGHT_LINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 100
        {
            tracing::warn!("metal: command buffer in flight for {secs:.1}s: {d}");
        }
    }
}

/// One `tracing::warn!` line for a failed command buffer: its label and each encoder's state.
fn trace_failure(cb: &ProtocolObject<dyn MTLCommandBuffer>, e: &objc2_foundation::NSError) {
    let label = cb.label().map(|l| l.to_string()).unwrap_or_default();
    let mut encoders = String::new();
    // SAFETY: the key is a Metal constant; the value is an NSArray of encoder infos.
    let infos = e.userInfo().objectForKey(unsafe { MTLCommandBufferEncoderInfoErrorKey });
    if let Some(infos) = infos {
        // SAFETY: Metal documents the value as an array of `MTLCommandBufferEncoderInfo`.
        let infos: Retained<objc2_foundation::NSArray<ProtocolObject<dyn MTLCommandBufferEncoderInfo>>> =
            unsafe { Retained::cast_unchecked(infos) };
        for i in infos.iter() {
            let st = match i.errorState() {
                MTLCommandEncoderErrorState::Completed => "Completed",
                MTLCommandEncoderErrorState::Pending => "Pending",
                MTLCommandEncoderErrorState::Faulted => "Faulted",
                MTLCommandEncoderErrorState::Affected => "Affected",
                _ => "Unknown",
            };
            encoders.push_str(&format!(" | [{st}] {}", i.label()));
        }
    }
    tracing::warn!(
        "metal: command buffer failed: [{label}] now={:.3} ({}){encoders}",
        crate::metal::since_start(),
        e.localizedDescription()
    );
}

/// Caps an encoder label.
fn cap_label_str(mut s: String) -> String {
    if s.len() > 600 {
        let mut n = 600;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
    }
    s
}

impl Metal {
    /// The capped label for an encoder, kept for the command buffer's description.
    fn cap_label(&self, s: String) -> Retained<objc2_foundation::NSString> {
        let s = cap_label_str(s);
        let n = objc2_foundation::NSString::from_str(&s);
        self.trace_enc.borrow_mut().push(s);
        n
    }

    fn encode_blit(
        &self,
        cb: &ProtocolObject<dyn MTLCommandBuffer>,
        ops: &[BlitOp<'_, Metal>],
        order: Ordering<'_>,
    ) -> Result<(), String> {
        let e = cb.blitCommandEncoder().ok_or("no blit encoder")?;
        if let Some((f, started)) = order
            && started.get()
        {
            e.waitForFence(f);
        }
        let off = |b: &Buffer, p: *const u8| p as usize - b.base as usize;
        for op in ops {
            match op {
                BlitOp::Copy { src, dst, len } => {
                    // SAFETY: both ranges lie inside live buffers (the caller resolved them).
                    unsafe {
                        e.copyFromBuffer_sourceOffset_toBuffer_destinationOffset_size(
                            &src.0.buf,
                            off(src.0, src.1),
                            &dst.0.buf,
                            off(dst.0, dst.1),
                            *len,
                        )
                    };
                }
                BlitOp::Fill { dst, len, value } => {
                    let o = off(dst.0, dst.1);
                    e.fillBuffer_range_value(&dst.0.buf, NSRange::new(o, *len), *value);
                }
            }
        }
        if let Some((f, started)) = order {
            e.updateFence(f);
            started.set(true);
        }
        if self.trace_fails {
            let fills = ops.iter().filter(|o| matches!(o, BlitOp::Fill { .. })).count();
            e.setLabel(Some(&self.cap_label(format!("blit: {} ops ({fills} fills)", ops.len()))));
        }
        e.endEncoding();
        Ok(())
    }
}

/// `ExecuteIndirect` with a count buffer runs record `index` only while `index < count`, both read
/// on the GPU: this one-thread kernel copies the record's `words`, or zeros (no groups, no
/// vertices or instances), into a buffer of its own, which the indirect command then reads.
/// Bound at 20 and up, apart from the converter's slots.
const GATE: &str = r#"
#include <metal_stdlib>
using namespace metal;
kernel void gate(device const uint *count [[buffer(20)]],
                 device const uint *record [[buffer(21)]],
                 device uint *out [[buffer(22)]],
                 constant uint2 &at [[buffer(23)]]) {
    bool run = at.x < count[0];
    for (uint w = 0; w < at.y; w++) {
        out[w] = run ? record[w] : 0u;
    }
}
"#;

impl Metal {
    fn gate_pipeline(&self) -> Result<&ProtocolObject<dyn MTLComputePipelineState>, String> {
        self.gate
            .get_or_init(|| {
                let lib = self
                    .dev
                    .newLibraryWithSource_options_error(
                        &objc2_foundation::NSString::from_str(GATE),
                        None,
                    )
                    .map_err(|e| e.localizedDescription().to_string())?;
                let f = lib
                    .newFunctionWithName(&objc2_foundation::NSString::from_str("gate"))
                    .ok_or("no gate function")?;
                self.dev
                    .newComputePipelineStateWithFunction_error(&f)
                    .map_err(|e| e.localizedDescription().to_string())
            })
            .as_deref()
            .map_err(Clone::clone)
    }

    /// Encodes the gate of one counted record (`words` long at `record`) into `e`; returns the
    /// buffer the indirect command reads instead of the record. Tracked by Metal (unlike the
    /// title's pools), so whatever reads it later in the command buffer waits for the write.
    /// shortcut: one gate dispatch and one small buffer per record; gate all the records of one
    /// `ExecuteIndirect` in a single dispatch if a large `MaxCommandCount` shows up in profiles.
    fn encode_gate(
        &self,
        e: &ProtocolObject<dyn MTLComputeCommandEncoder>,
        (count, at, index): (&Buffer, *const u8, u32),
        (record, title): (&Buffer, *const u8),
        words: u32,
        set_on_queue: bool,
    ) -> Result<Retained<ProtocolObject<dyn MTLBuffer>>, String> {
        let out = self
            .dev
            .newBufferWithLength_options(words as usize * 4, MTLResourceOptions::StorageModePrivate)
            .ok_or("no buffer for a gated indirect record")?;
        for b in [count, record] {
            if self.names(b, set_on_queue) {
                e.useResource_usage(ProtocolObject::from_ref(&*b.buf), MTLResourceUsage::Read);
            }
        }
        // What the work before wrote (the count, the record) is seen.
        e.memoryBarrierWithScope(MTLBarrierScope::Buffers);
        e.setComputePipelineState(self.gate_pipeline()?);
        let args = [index, words];
        // SAFETY: both title ranges lie inside live buffers (the caller resolved them); the bytes
        // are copied before the call returns.
        unsafe {
            e.setBuffer_offset_atIndex(Some(&count.buf), at as usize - count.base as usize, 20);
            e.setBuffer_offset_atIndex(
                Some(&record.buf),
                title as usize - record.base as usize,
                21,
            );
            e.setBuffer_offset_atIndex(Some(&out), 0, 22);
            e.setBytes_length_atIndex(
                NonNull::new(args.as_ptr().cast_mut().cast()).unwrap(),
                8,
                23,
            );
        }
        e.dispatchThreadgroups_threadsPerThreadgroup(size([1, 1, 1]), size([1, 1, 1]));
        e.memoryBarrierWithScope(MTLBarrierScope::Buffers);
        Ok(out)
    }

    /// Whether encoder `use` calls are needed for `b`: not when the queue has the pool residency set
    /// and the buffer is in it.
    fn names(&self, b: &Buffer, set_on_queue: bool) -> bool {
        let needed = !(set_on_queue && b.is_resident());
        if needed {
            self.count(|c| c.use_resource += 1);
        }
        needed
    }

    pub(crate) fn encode_compute(
        &self,
        cb: &ProtocolObject<dyn MTLCommandBuffer>,
        dispatches: &[crate::backend::Dispatch<'_, Metal>],
        set_on_queue: bool,
        order: Ordering<'_>,
    ) -> Result<(), String> {
        if dispatches.is_empty() {
            return Ok(());
        }
        let e = cb.computeCommandEncoder().ok_or("no compute encoder")?;
        if let Some((f, started)) = order
            && started.get()
        {
            e.waitForFence(f);
        }
        for d in dispatches {
            let gated = match (d.count, d.indirect) {
                (Some(count), Some(record)) => {
                    Some(self.encode_gate(&e, count, record, 3, set_on_queue)?)
                }
                _ => None,
            };
            e.setComputePipelineState(&d.pipeline.0);
            for h in [d.heaps.resource, d.heaps.sampler].into_iter().flatten() {
                self.use_heap_cb(cb, h);
            }
            self.bind_root_compute(&e, &d.root, &d.heaps, d.pipeline.1.as_ref());
            for b in d.resources.iter().filter(|b| self.names(b, set_on_queue)) {
                e.useResource_usage(
                    ProtocolObject::from_ref(&*b.buf),
                    MTLResourceUsage::Read | MTLResourceUsage::Write,
                );
            }
            match d.indirect {
                Some((b, title)) => {
                    if self.names(b, set_on_queue) {
                        e.useResource_usage(ProtocolObject::from_ref(&*b.buf), MTLResourceUsage::Read);
                    }
                    let (buf, at) = match &gated {
                        Some(g) => (&**g, 0),
                        None => (&*b.buf, title as usize - b.base as usize),
                    };
                    // SAFETY: the caller resolved three words at `title` inside the live buffer
                    // (or the gate wrote them to its own).
                    unsafe {
                        e.dispatchThreadgroupsWithIndirectBuffer_indirectBufferOffset_threadsPerThreadgroup(
                            buf,
                            at,
                            size(d.threads_per_group),
                        )
                    };
                }
                None => e.dispatchThreadgroups_threadsPerThreadgroup(size(d.groups), size(d.threads_per_group)),
            }
        }
        if let Some((f, started)) = order {
            e.updateFence(f);
            started.set(true);
        }
        if self.trace_fails {
            let names = self.pipe_names.borrow();
            let mut l = String::from("compute: ");
            for d in dispatches {
                let k = Retained::as_ptr(&d.pipeline.0) as *const u8 as usize;
                match names.get(&k) {
                    Some(n) => l.push_str(n),
                    None => l.push_str(&format!("{k:#x}")),
                }
                l.push_str(&format!(" {}x{}x{}; ", d.groups[0], d.groups[1], d.groups[2]));
            }
            e.setLabel(Some(&self.cap_label(l)));
        }
        e.endEncoding();
        Ok(())
    }

    fn encode_render(
        &self,
        cb: &ProtocolObject<dyn MTLCommandBuffer>,
        rp: &RenderPass<'_, Metal>,
        set_on_queue: bool,
        order: Ordering<'_>,
    ) -> Result<(), String> {
        // Attachments must agree in size and sample count. Metal renders to the intersection and
        // does not complain, which hides a title or decoder bug behind a cropped picture; refusing
        // the pass is also the one refusal a test can build without a device error.
        let mut shape = None;
        for t in rp
            .color
            .iter()
            .map(|c| &c.texture.tex)
            .chain(rp.depth.as_ref().map(|d| &d.texture.tex))
        {
            let s = (t.width(), t.height(), t.sampleCount());
            if let Some(want) = shape
                && want != s
            {
                return Err(format!(
                    "render targets of different sizes or samples: {want:?} and {s:?}"
                ));
            }
            shape = Some(s);
        }
        if rp.color.is_empty() && rp.depth.is_none() {
            // The title drew with no render targets. D3D12 runs it (it writes nothing visible);
            // Metal needs an attachment to make an encoder, and Metal refuses one without. The
            // pass is skipped rather than refused, so the rest of the submission still runs.
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                eprintln!(
                    "metal: a pass with no render targets is skipped ({} draws, writes nothing)",
                    rp.draws.len()
                );
            });
            return Ok(());
        }
        let desc = MTLRenderPassDescriptor::new();
        for (i, c) in rp.color.iter().enumerate() {
            // SAFETY: colour attachment index below 8.
            let a = unsafe { desc.colorAttachments().objectAtIndexedSubscript(i) };
            a.setTexture(Some(&c.texture.tex));
            a.setStoreAction(MTLStoreAction::Store);
            match c.clear {
                Some(v) => {
                    a.setLoadAction(MTLLoadAction::Clear);
                    a.setClearColor(MTLClearColor {
                        red: v[0] as f64,
                        green: v[1] as f64,
                        blue: v[2] as f64,
                        alpha: v[3] as f64,
                    });
                }
                None => a.setLoadAction(MTLLoadAction::Load),
            }
        }
        // Counted indirect draws are gated first, in a compute pass of their own.
        let mut gates = Vec::new();
        if rp
            .draws
            .iter()
            .any(|d| d.count.is_some() && d.indirect.is_some())
        {
            let c = cb.computeCommandEncoder().ok_or("no compute encoder")?;
            if let Some((f, started)) = order
                && started.get()
            {
                c.waitForFence(f);
            }
            for d in &rp.draws {
                let words = if matches!(d.call, DrawCall::Indexed { .. }) {
                    5
                } else {
                    4
                };
                gates.push(match (d.count, d.indirect) {
                    (Some(count), Some(record)) => {
                        Some(self.encode_gate(&c, count, record, words, set_on_queue)?)
                    }
                    _ => None,
                });
            }
            if let Some((f, started)) = order {
                c.updateFence(f);
                started.set(true);
            }
            c.endEncoding();
        }
        if let Some(dp) = &rp.depth {
            let a = desc.depthAttachment();
            a.setTexture(Some(&dp.texture.tex));
            a.setStoreAction(MTLStoreAction::Store);
            match dp.clear {
                Some(v) => {
                    a.setLoadAction(MTLLoadAction::Clear);
                    a.setClearDepth(v as f64);
                }
                None => a.setLoadAction(MTLLoadAction::Load),
            }
            // A combined depth and stencil texture is attached as both.
            if dp.texture.tex.pixelFormat() == MTLPixelFormat::Depth32Float_Stencil8 {
                let a = desc.stencilAttachment();
                a.setTexture(Some(&dp.texture.tex));
                a.setStoreAction(MTLStoreAction::Store);
                match dp.clear_stencil {
                    Some(v) => {
                        a.setLoadAction(MTLLoadAction::Clear);
                        a.setClearStencil(u32::from(v));
                    }
                    None => a.setLoadAction(MTLLoadAction::Load),
                }
            }
        }
        let e = cb.renderCommandEncoderWithDescriptor(&desc).ok_or_else(|| {
            // Metal refuses an encoder whose attachments disagree (size, sample count, format
            // kind); the caller's log then says which targets the pass named.
            let size = |t: &ProtocolObject<dyn MTLTexture>| {
                format!("{}x{} fmt {}", t.width(), t.height(), t.pixelFormat().0)
            };
            let color: Vec<String> = rp.color.iter().map(|c| size(&c.texture.tex)).collect();
            let depth = rp.depth.as_ref().map(|d| size(&d.texture.tex));
            format!(
                "no render encoder: {} colour {color:?}, depth {depth:?}, {} draws",
                rp.color.len(),
                rp.draws.len()
            )
        })?;
        if let Some((f, started)) = order
            && started.get()
        {
            e.waitForFence_beforeStages(f, MTLRenderStages::Vertex);
        }
        let rt_size = rp
            .color
            .first()
            .map(|c| &c.texture.tex)
            .or(rp.depth.as_ref().map(|d| &d.texture.tex))
            .map_or((0, 0), |t| (t.width(), t.height()));
        for (k, d) in rp.draws.iter().enumerate() {
            for h in [d.heaps.resource, d.heaps.sampler].into_iter().flatten() {
                self.use_heap_cb(cb, h);
            }
            e.setRenderPipelineState(&d.pipeline.pso);
            e.setCullMode(d.pipeline.cull);
            let r = &d.pipeline.raster;
            e.setFrontFacingWinding(if r.front_ccw {
                MTLWinding::CounterClockwise
            } else {
                MTLWinding::Clockwise
            });
            e.setTriangleFillMode(if r.wireframe {
                MTLTriangleFillMode::Lines
            } else {
                MTLTriangleFillMode::Fill
            });
            e.setDepthClipMode(if r.depth_clip {
                MTLDepthClipMode::Clip
            } else {
                MTLDepthClipMode::Clamp
            });
            e.setDepthBias_slopeScale_clamp(r.depth_bias.0, r.depth_bias.1, r.depth_bias.2);
            if let Some(ds) = &d.pipeline.depth {
                e.setDepthStencilState(Some(ds));
            }
            if d.pipeline.stencil {
                e.setStencilReferenceValue(d.state.stencil_ref);
            }
            let [r, g, b, a] = d.state.blend_factor;
            e.setBlendColorRed_green_blue_alpha(r, g, b, a);
            // The target's size bounds the scissor (Metal refuses a rectangle outside it).
            let (tw, th) = rt_size;
            if let Some(v) = d.state.viewport {
                e.setViewport(MTLViewport {
                    originX: v.x as f64,
                    originY: v.y as f64,
                    width: v.width as f64,
                    height: v.height as f64,
                    znear: v.min_depth as f64,
                    zfar: v.max_depth as f64,
                });
            }
            let scissor = match d.state.scissor {
                Some([l, t, r, b]) => {
                    let (l, t) = (l.clamp(0, tw as i32) as usize, t.clamp(0, th as i32) as usize);
                    let (r, b) = (r.clamp(0, tw as i32) as usize, b.clamp(0, th as i32) as usize);
                    MTLScissorRect {
                        x: l,
                        y: t,
                        width: r.saturating_sub(l),
                        height: b.saturating_sub(t),
                    }
                }
                None => MTLScissorRect {
                    x: 0,
                    y: 0,
                    width: tw,
                    height: th,
                },
            };
            if scissor.width == 0 || scissor.height == 0 {
                // Nothing of the target is inside the rectangle: nothing is drawn.
                continue;
            }
            e.setScissorRect(scissor);
            self.bind_root_render(&e, &d.root, d.fragment_root.as_ref(), &d.heaps, d.pipeline);
            for b in d.resources.iter().filter(|b| self.names(b, set_on_queue)) {
                e.useResource_usage_stages(
                    ProtocolObject::from_ref(&*b.buf),
                    MTLResourceUsage::Read | MTLResourceUsage::Write,
                    MTLRenderStages::Vertex | MTLRenderStages::Fragment,
                );
            }
            for (slot, (b, title)) in d.vertex_buffers.iter().enumerate() {
                let off = *title as usize - b.base as usize;
                // SAFETY: the buffer is live and `off` is inside it.
                unsafe {
                    e.setVertexBuffer_offset_atIndex(Some(&b.buf), off, VERTEX_BUFFERS + slot)
                };
                if self.names(b, set_on_queue) {
                    e.useResource_usage_stages(
                        ProtocolObject::from_ref(&*b.buf),
                        MTLResourceUsage::Read,
                        MTLRenderStages::Vertex,
                    );
                }
            }
            let prim = match d.topology {
                Topology::Triangles => MTLPrimitiveType::Triangle,
                Topology::TriangleStrip => MTLPrimitiveType::TriangleStrip,
                Topology::Lines => MTLPrimitiveType::Line,
                Topology::LineStrip => MTLPrimitiveType::LineStrip,
                Topology::Points => MTLPrimitiveType::Point,
            };
            // What `IRRuntimeDrawPrimitives` / `IRRuntimeDrawIndexedPrimitives` do: draw arguments
            // (`IRRuntimeDrawParams`, 5 words) at slot 4 and the index type at slot 5 (0 = not indexed,
            // else Metal's index type + 1), then the draw itself.
            if let Some((ab, title)) = d.indirect {
                // What the converter runtime's indirect `IRRuntimeDraw(Indexed)Primitives` do: the
                // argument record itself is the shader's draw arguments (D3D12's records are
                // `IRRuntimeDraw(Indexed)Argument`s and Metal's indirect ones, word for word).
                if self.names(ab, set_on_queue) {
                    e.useResource_usage_stages(
                        ProtocolObject::from_ref(&*ab.buf),
                        MTLResourceUsage::Read,
                        MTLRenderStages::Vertex,
                    );
                }
                // A counted record is read from its gate's buffer.
                let (args, at) = match gates.get(k).and_then(Option::as_ref) {
                    Some(g) => (&**g, 0),
                    None => (&*ab.buf, title as usize - ab.base as usize),
                };
                // SAFETY: the record is inside the live buffer (the caller resolved it).
                unsafe { e.setVertexBuffer_offset_atIndex(Some(args), at, DRAW_ARGUMENTS) };
                match (d.call, d.index_buffer) {
                    (DrawCall::Arrays { .. }, _) => {
                        set_index_type(&e, 0);
                        // SAFETY: as above.
                        unsafe { e.drawPrimitives_indirectBuffer_indirectBufferOffset(prim, args, at) };
                    }
                    (DrawCall::Indexed { wide, .. }, Some((b, title))) => {
                        set_index_type(&e, if wide { 2 } else { 1 });
                        if self.names(b, set_on_queue) {
                            e.useResource_usage_stages(
                                ProtocolObject::from_ref(&*b.buf),
                                MTLResourceUsage::Read,
                                MTLRenderStages::Vertex,
                            );
                        }
                        // SAFETY: both buffers are live; the record's start index counts from `title`.
                        unsafe {
                            e.drawIndexedPrimitives_indexType_indexBuffer_indexBufferOffset_indirectBuffer_indirectBufferOffset(
                                prim,
                                if wide { MTLIndexType::UInt32 } else { MTLIndexType::UInt16 },
                                &b.buf,
                                title as usize - b.base as usize,
                                args,
                                at,
                            )
                        };
                    }
                    (DrawCall::Indexed { .. }, None) => return Err("indexed draw without an index buffer".into()),
                }
                continue;
            }
            let mut dp = [0u32; 5];
            let ib: u16;
            match d.call {
                DrawCall::Arrays {
                    vertex_count,
                    instance_count,
                    first_vertex,
                    first_instance,
                } => {
                    dp[..4].copy_from_slice(&[
                        vertex_count,
                        instance_count,
                        first_vertex,
                        first_instance,
                    ]);
                    ib = 0;
                    set_draw_bytes(&e, &dp, ib);
                    // SAFETY: arguments are plain counts.
                    unsafe {
                        e.drawPrimitives_vertexStart_vertexCount_instanceCount_baseInstance(
                            prim,
                            first_vertex as usize,
                            vertex_count as usize,
                            instance_count as usize,
                            first_instance as usize,
                        )
                    };
                }
                DrawCall::Indexed {
                    index_count,
                    instance_count,
                    first_index,
                    base_vertex,
                    first_instance,
                    wide,
                } => {
                    // A draw that names no index buffer is dropped, not the pass: returning here
                    // would leave the render encoder open and, now that a refused pass still
                    // commits (for its fence signals), Metal would assert on the open encoder.
                    let Some((b, title)) = d.index_buffer else {
                        static ONCE: std::sync::Once = std::sync::Once::new();
                        ONCE.call_once(|| {
                            eprintln!("metal: an indexed draw without an index buffer is dropped")
                        });
                        continue;
                    };
                    let width = if wide { 4 } else { 2 };
                    let off = title as usize - b.base as usize + first_index as usize * width;
                    dp = [
                        index_count,
                        instance_count,
                        off as u32,
                        base_vertex as u32,
                        first_instance,
                    ];
                    ib = if wide { 2 } else { 1 };
                    set_draw_bytes(&e, &dp, ib);
                    if self.names(b, set_on_queue) {
                        e.useResource_usage_stages(
                            ProtocolObject::from_ref(&*b.buf),
                            MTLResourceUsage::Read,
                            MTLRenderStages::Vertex,
                        );
                    }
                    // SAFETY: the index buffer is live and holds `index_count` indices at `off`.
                    unsafe {
                        e.drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferOffset_instanceCount_baseVertex_baseInstance(
                            prim,
                            index_count as usize,
                            if wide { MTLIndexType::UInt32 } else { MTLIndexType::UInt16 },
                            &b.buf,
                            off,
                            instance_count as usize,
                            base_vertex as isize,
                            first_instance as usize,
                        )
                    };
                }
            }
        }
        if let Some((f, started)) = order {
            e.updateFence_afterStages(f, MTLRenderStages::Fragment);
            started.set(true);
        }
        if self.trace_fails {
            e.setLabel(Some(&self.cap_label(format!("render: {} draws", rp.draws.len()))));
        }
        e.endEncoding();
        Ok(())
    }
}

fn set_draw_bytes(e: &ProtocolObject<dyn MTLRenderCommandEncoder>, dp: &[u32; 5], index_type: u16) {
    // SAFETY: setVertexBytes copies the bytes before returning.
    unsafe {
        e.setVertexBytes_length_atIndex(
            NonNull::new(dp.as_ptr().cast_mut().cast()).unwrap(),
            20,
            DRAW_ARGUMENTS,
        );
    }
    set_index_type(e, index_type);
}

/// The converter's index type word (0 = not indexed, else Metal's index type + 1).
fn set_index_type(e: &ProtocolObject<dyn MTLRenderCommandEncoder>, index_type: u16) {
    // SAFETY: setVertexBytes copies the bytes before returning.
    unsafe {
        e.setVertexBytes_length_atIndex(
            NonNull::new((&raw const index_type).cast_mut().cast()).unwrap(),
            2,
            UNIFORMS,
        );
    }
}

/// Whether a pixel format is a depth or stencil one.
fn is_depth(f: MTLPixelFormat) -> bool {
    [
        MTLPixelFormat::Depth16Unorm,
        MTLPixelFormat::Depth32Float,
        MTLPixelFormat::Stencil8,
        MTLPixelFormat::Depth24Unorm_Stencil8,
        MTLPixelFormat::Depth32Float_Stencil8,
    ]
    .contains(&f)
}

#[cfg(test)]
mod owed_tests {
    use super::*;
    use crate::backend::ComputeBackend;
    use crate::mem::{Region, page_size};

    #[test]
    fn a_failed_command_buffer_is_paid_on_the_cpu() {
        let m = Metal::new().unwrap();
        let region = Region::new(page_size(), None).unwrap();
        let buf = m.wrap_title_memory(region.ptr, region.len).unwrap();
        // SAFETY: inside the region; the GPU is idle.
        let at = |o: usize| unsafe { region.ptr.add(o) };
        let q = m.create_queue().unwrap();
        q.1.set(true); // as with the pool's residency set: the order event is in play
        let fence = m.create_fence(0).unwrap();
        let passes = [Pass::Blit(vec![
            BlitOp::Fill { dst: (&buf, at(64) as *const u8), len: 8, value: 0xAB },
            BlitOp::Copy { src: (&buf, at(0) as *const u8), dst: (&buf, at(256) as *const u8), len: 16 },
        ])];
        let (s5, s7, s9) = ([(&fence, 5)], [(&fence, 7)], [(&fence, 9)]);
        let sub = |signals| PassSubmission { waits: &[], passes: &passes, signals };
        // The recovery a submission signalling the fence to 7 would owe, built as `submit_passes` does.
        q.3.set(3);
        let owed = Owed::new(&q, &sub(&s7), true);
        owed.recover();
        // SAFETY: bounds as above.
        let bytes = unsafe { std::slice::from_raw_parts(at(60), 16) };
        assert_eq!(bytes, [0, 0, 0, 0, 0xAB, 0xAB, 0xAB, 0xAB, 0xAB, 0xAB, 0xAB, 0xAB, 0, 0, 0, 0]);
        // SAFETY: inside the region.
        assert_eq!(unsafe { *at(256) }, 0, "copies are not replayed");
        assert_eq!(m.completed(&fence), 7);
        assert_eq!(q.2.signaledValue(), 3);
        // An older submission failing later does not lower anything.
        Owed::new(&q, &sub(&s5), false).recover();
        assert_eq!(m.completed(&fence), 7);
        // A normal submission after it still completes: the queue is not stuck behind the order value.
        m.submit_passes(&q, &sub(&s9)).unwrap();
        assert!(m.wait(&fence, 9, 5000), "queue stuck after recovery");
    }
}
