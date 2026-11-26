//! Present: the title's back buffer into the next drawable of a `CAMetalLayer` the app owns.
//!
//! The app creates the layer (a window's view layer, or [`Metal::create_layer`] for one that is
//! shown by something else, or by nothing, in tests) and hands it over by its address
//! ([`Metal::create_surface`]); the backend draws the title's texture over the drawable with a
//! full-screen triangle, so that any size and channel order (the title's RGBA, the layer's BGRA) works,
//! and presents it after the work already submitted to the queue.

use crate::metal::{Metal, Queue};
use crate::gfx::Texture;
use crate::resources::PresentBackend;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::msg_send;
use objc2_foundation::NSSize as CGSize;
use objc2_foundation::NSString;
use objc2_metal::*;
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use std::cell::RefCell;
use std::collections::HashMap;

/// Draws a texture over the whole target. Compiled by Metal's own compiler at run time (this is
/// the one shader of the backend that is not converted DXIL).
const SHADER: &str = r#"
#include <metal_stdlib>
using namespace metal;
struct Out { float4 position [[position]]; float2 uv; };
vertex Out present_vertex(uint i [[vertex_id]]) {
    float2 p = float2((i << 1) & 2, i & 2);
    Out o;
    o.position = float4(p * 2.0 - 1.0, 0.0, 1.0);
    o.uv = float2(p.x, 1.0 - p.y);
    return o;
}
fragment float4 present_fragment(Out in [[stage_in]], texture2d<float> source [[texture(0)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    return source.sample(s, in.uv);
}
"#;

type Pipeline = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;

/// A `CAMetalLayer` presented to.
pub struct Surface {
    layer: Retained<CAMetalLayer>,
    library: Retained<ProtocolObject<dyn MTLLibrary>>,
    /// The copy pipeline for each drawable format seen.
    pipelines: RefCell<HashMap<usize, Pipeline>>,
    /// The texture of the last drawable presented, readable when the layer is not frame-buffer
    /// only (the tests read their pixels from it).
    last: RefCell<Option<Retained<ProtocolObject<dyn MTLTexture>>>>,
    /// Frames presented.
    frames: std::cell::Cell<u64>,
}

impl Surface {
    pub fn frames(&self) -> u64 {
        self.frames.get()
    }

    /// The pixels of the last presented drawable (width, height, BGRA rows), read back from its
    /// texture: complete once the GPU has drawn the frame (a fence signalled after the present).
    pub fn read_last(&self) -> Option<(usize, usize, Vec<u8>)> {
        let t = self.last.borrow().clone()?;
        let (w, h) = (t.width(), t.height());
        let mut out = vec![0u8; w * h * 4];
        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width: w,
                height: h,
                depth: 1,
            },
        };
        // SAFETY: `out` holds `h` rows of `w * 4` bytes, the region's size.
        unsafe {
            t.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                std::ptr::NonNull::new(out.as_mut_ptr().cast())?,
                w * 4,
                region,
                0,
            )
        };
        Some((w, h, out))
    }

    /// The last presented drawable's texture, with its size.
    pub fn last_texture(&self) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
        self.last.borrow().clone()
    }
}

/// A layer made by [`Metal::create_layer`]: its address is what [`Metal::create_surface`] takes,
/// and what an app puts into a view (`NSView.layer`).
pub struct Layer(pub Retained<CAMetalLayer>);

impl Layer {
    pub fn as_ptr(&self) -> *mut std::ffi::c_void {
        Retained::as_ptr(&self.0).cast_mut().cast()
    }
}

impl Metal {
    /// A layer drawing to this device: `width` by `height` drawables in BGRA, readable (not
    /// frame-buffer only), vsync off.
    pub fn create_layer(&self, width: usize, height: usize) -> Layer {
        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&self.dev));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        layer.setFramebufferOnly(false);
        layer.setDrawableSize(CGSize {
            width: width as f64,
            height: height as f64,
        });
        // SAFETY: a plain BOOL property of CAMetalLayer.
        let _: () = unsafe { msg_send![&*layer, setDisplaySyncEnabled: false] };
        Layer(layer)
    }
}

fn pipeline(
    m: &Metal,
    surface: &Surface,
    format: MTLPixelFormat,
) -> Result<Pipeline, String> {
    if let Some(p) = surface.pipelines.borrow().get(&(format.0 as usize)) {
        return Ok(p.clone());
    }
    let d = MTLRenderPipelineDescriptor::new();
    let name = |s: &str| NSString::from_str(s);
    d.setVertexFunction(surface.library.newFunctionWithName(&name("present_vertex")).as_deref());
    d.setFragmentFunction(
        surface
            .library
            .newFunctionWithName(&name("present_fragment"))
            .as_deref(),
    );
    // SAFETY: colour attachment 0 always exists.
    unsafe { d.colorAttachments().objectAtIndexedSubscript(0) }.setPixelFormat(format);
    let p = m
        .dev
        .newRenderPipelineStateWithDescriptor_error(&d)
        .map_err(|e| e.localizedDescription().to_string())?;
    surface.pipelines.borrow_mut().insert(format.0 as usize, p.clone());
    Ok(p)
}

impl PresentBackend for Metal {
    type Surface = Surface;

    fn create_surface(&self, layer: *mut std::ffi::c_void) -> Result<Surface, String> {
        // SAFETY: the caller passes a live CAMetalLayer (retained here).
        let layer = unsafe { Retained::retain(layer.cast::<CAMetalLayer>()) }
            .ok_or("a null CAMetalLayer")?;
        if layer.device().is_none() {
            layer.setDevice(Some(&self.dev));
        }
        // Drawn into, so the layer cannot be frame-buffer only for reading back; presenting
        // works either way.
        let library = self
            .dev
            .newLibraryWithSource_options_error(&NSString::from_str(SHADER), None)
            .map_err(|e| e.localizedDescription().to_string())?;
        Ok(Surface {
            layer,
            library,
            pipelines: RefCell::default(),
            last: RefCell::default(),
            frames: Default::default(),
        })
    }

    fn present(
        &self,
        queue: &Queue,
        surface: &Surface,
        source: &Texture,
    ) -> Result<(), String> {
        let drawable = surface.layer.nextDrawable().ok_or("no drawable")?;
        let target = drawable.texture();
        // An sRGB source is shown as the bytes it holds when the layer is not sRGB (the title's
        // swap chain holds display values).
        let src = source.view_without_srgb(self);
        let pass = MTLRenderPassDescriptor::new();
        // SAFETY: colour attachment 0 always exists.
        let a = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        a.setTexture(Some(&target));
        a.setLoadAction(MTLLoadAction::DontCare);
        a.setStoreAction(MTLStoreAction::Store);
        let cb = queue.0.commandBuffer().ok_or("no command buffer")?;
        let e = cb
            .renderCommandEncoderWithDescriptor(&pass)
            .ok_or("no render encoder")?;
        let state = pipeline(self, surface, target.pixelFormat())?;
        e.setRenderPipelineState(&state);
        // SAFETY: slot 0 of the fragment stage takes the source texture.
        unsafe { e.setFragmentTexture_atIndex(Some(&src), 0) };
        // SAFETY: three vertices, no buffers.
        unsafe {
            e.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3)
        };
        e.endEncoding();
        cb.presentDrawable(ProtocolObject::from_ref(&*drawable));
        cb.commit();
        *surface.last.borrow_mut() = Some(target);
        surface.frames.set(surface.frames.get() + 1);
        Ok(())
    }
}
