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
    refs: RefCell<Vec<Option<Res>>>,
    /// Residency set holding the heap buffer and everything its entries name.
    set: Option<Retained<ProtocolObject<dyn MTLResidencySet>>>,
    dirty: Cell<bool>,
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
}

fn pixel_format(f: Format) -> MTLPixelFormat {
    match f {
        Format::Rgba8Unorm => MTLPixelFormat::RGBA8Unorm,
        Format::Rgba8UnormSrgb => MTLPixelFormat::RGBA8Unorm_sRGB,
        Format::Bgra8Unorm => MTLPixelFormat::BGRA8Unorm,
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

fn vertex_format(f: VertexFormat) -> MTLVertexFormat {
    match f {
        VertexFormat::Float => MTLVertexFormat::Float,
        VertexFormat::Float2 => MTLVertexFormat::Float2,
        VertexFormat::Float3 => MTLVertexFormat::Float3,
        VertexFormat::Float4 => MTLVertexFormat::Float4,
        VertexFormat::Uint => MTLVertexFormat::UInt,
        VertexFormat::Unorm8x4 => MTLVertexFormat::UChar4Normalized,
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
        if let Some(r) = keep {
            if let Some(set) = &heap.set {
                set.addAllocation(ProtocolObject::from_ref(&*r));
                heap.dirty.set(true);
            }
            heap.refs.borrow_mut()[index] = Some(r);
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
        heap.refs.borrow().iter().flatten().for_each(|r| use_res(r));
    }

    fn stage_function(
        &self,
        dxil: &[u8],
    ) -> Result<Retained<ProtocolObject<dyn MTLFunction>>, String> {
        let (_, lib) = self.msc.convert(dxil, None).map_err(|e| e.to_string())?;
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
    ) {
        if !root.0.is_empty() {
            // SAFETY: setBytes copies `root` (at most 4 KiB) before returning.
            unsafe {
                e.setBytes_length_atIndex(
                    NonNull::new(root.0.as_ptr().cast_mut().cast()).unwrap(),
                    root.0.len(),
                    ARGUMENT_BUFFER,
                )
            };
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
        heaps: &Heaps<'_, Metal>,
    ) {
        if !root.0.is_empty() {
            let p = NonNull::new(root.0.as_ptr().cast_mut().cast()).unwrap();
            // SAFETY: setBytes copies `root` before returning; both stages read the same table.
            unsafe {
                e.setVertexBytes_length_atIndex(p, root.0.len(), ARGUMENT_BUFFER);
                e.setFragmentBytes_length_atIndex(p, root.0.len(), ARGUMENT_BUFFER);
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
            refs: RefCell::new(vec![None; count]),
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
        // SAFETY: the ranges are checked by Metal (nil on failure).
        let tex = unsafe {
            t.tex
                .newTextureViewWithPixelFormat_textureType_levels_slices(
                    pixel_format(v.format),
                    ty,
                    NSRange::new(v.mips.0, v.mips.1),
                    NSRange::new(v.slices.0, v.slices.1),
                )
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
        let vf = self.stage_function(d.vs)?;
        let pd = MTLRenderPipelineDescriptor::new();
        pd.setVertexFunction(Some(&vf));
        if let Some(ps) = d.ps {
            pd.setFragmentFunction(Some(&*self.stage_function(ps)?));
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
            if let Some(b) = state.blend {
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
        let pso = self
            .dev
            .newRenderPipelineStateWithDescriptor_error(&pd)
            .map_err(|e| e.localizedDescription().to_string())?;
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
        })
    }

    fn submit_passes(&self, queue: &Queue, work: &PassSubmission<'_, Self>) -> Result<(), String> {
        let cb = queue.0.commandBuffer().ok_or("no command buffer")?;
        for (f, v) in work.waits {
            cb.encodeWaitForEvent_value(ProtocolObject::from_ref(&*f.0), *v);
        }
        for pass in work.passes {
            match pass {
                Pass::Compute(dispatches) => self.encode_compute(&cb, dispatches)?,
                Pass::Render(rp) => self.encode_render(&cb, rp)?,
                Pass::Blit(ops) => self.encode_blit(&cb, ops)?,
            }
        }
        for (f, v) in work.signals {
            cb.encodeSignalEvent_value(ProtocolObject::from_ref(&*f.0), *v);
        }
        cb.commit();
        Ok(())
    }
}

impl Metal {
    fn encode_blit(
        &self,
        cb: &ProtocolObject<dyn MTLCommandBuffer>,
        ops: &[BlitOp<'_, Metal>],
    ) -> Result<(), String> {
        let e = cb.blitCommandEncoder().ok_or("no blit encoder")?;
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
        e.endEncoding();
        Ok(())
    }
}

impl Metal {
    pub(crate) fn encode_compute(
        &self,
        cb: &ProtocolObject<dyn MTLCommandBuffer>,
        dispatches: &[crate::backend::Dispatch<'_, Metal>],
    ) -> Result<(), String> {
        if dispatches.is_empty() {
            return Ok(());
        }
        let e = cb.computeCommandEncoder().ok_or("no compute encoder")?;
        for d in dispatches {
            e.setComputePipelineState(&d.pipeline.0);
            for h in [d.heaps.resource, d.heaps.sampler].into_iter().flatten() {
                self.use_heap_cb(cb, h);
            }
            self.bind_root_compute(&e, &d.root, &d.heaps);
            for b in &d.resources {
                e.useResource_usage(
                    ProtocolObject::from_ref(&*b.buf),
                    MTLResourceUsage::Read | MTLResourceUsage::Write,
                );
            }
            e.dispatchThreadgroups_threadsPerThreadgroup(size(d.groups), size(d.threads_per_group));
        }
        e.endEncoding();
        Ok(())
    }

    fn encode_render(
        &self,
        cb: &ProtocolObject<dyn MTLCommandBuffer>,
        rp: &RenderPass<'_, Metal>,
    ) -> Result<(), String> {
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
                match dp.clear {
                    Some(_) => {
                        a.setLoadAction(MTLLoadAction::Clear);
                        a.setClearStencil(0);
                    }
                    None => a.setLoadAction(MTLLoadAction::Load),
                }
            }
        }
        let e = cb
            .renderCommandEncoderWithDescriptor(&desc)
            .ok_or("no render encoder")?;
        let rt_size = rp
            .color
            .first()
            .map(|c| &c.texture.tex)
            .or(rp.depth.as_ref().map(|d| &d.texture.tex))
            .map_or((0, 0), |t| (t.width(), t.height()));
        for d in &rp.draws {
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
            if r.depth_bias != (0.0, 0.0, 0.0) {
                e.setDepthBias_slopeScale_clamp(r.depth_bias.0, r.depth_bias.1, r.depth_bias.2);
            }
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
            self.bind_root_render(&e, &d.root, &d.heaps);
            for b in &d.resources {
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
                e.useResource_usage_stages(
                    ProtocolObject::from_ref(&*b.buf),
                    MTLResourceUsage::Read,
                    MTLRenderStages::Vertex,
                );
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
                    let (b, title) = d
                        .index_buffer
                        .ok_or("indexed draw without an index buffer")?;
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
                    e.useResource_usage_stages(
                        ProtocolObject::from_ref(&*b.buf),
                        MTLResourceUsage::Read,
                        MTLRenderStages::Vertex,
                    );
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
        e.setVertexBytes_length_atIndex(
            NonNull::new((&raw const index_type).cast_mut().cast()).unwrap(),
            2,
            UNIFORMS,
        );
    }
}
