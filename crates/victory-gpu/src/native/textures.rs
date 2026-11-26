//! Title textures on the native host: the console's tiled layouts (`victory_formats::texture`)
//! detiled from title memory into backend textures.
//!
//! A title texture lives in title memory in the console's swizzled layout; the backend's texture is
//! Apple's own. Each texture the host samples is detiled into a linear staging buffer and uploaded,
//! mip by mip and slice by slice, when the memory's content changed since the last time (a hash of
//! the memory, taken when work is submitted). Render targets and depth targets are the GPU's: they
//! start empty and are not read from memory.
//!
//! shortcut: the detile runs on the CPU (`Layout::detile`, the code the D3D12 host uses too) and
//! every submission hashes the memory of every sampled texture; a compute detile that reads the
//! pool buffer directly and a per-page change test (what `watch.rs` gives the D3D12 host) are the
//! next step. A change detiles the whole texture.

use super::descriptors::format;
use super::title::Title;
use std::collections::HashMap;
use std::rc::Rc;
use victory_backend::resources::*;
use victory_formats::texture::{Dimension, Layout};

/// `D3D12_RESOURCE_FLAGS`.
const RENDER_TARGET: u32 = 1;
const DEPTH_STENCIL: u32 = 2;
const UNORDERED_ACCESS: u32 = 4;

/// A title texture as the title made it: its description and where its memory is.
#[derive(Clone)]
pub struct TextureInfo {
    pub dimension: Dimension,
    /// DXGI format number, the console's (before `pc_format`).
    pub format: u32,
    pub width: u32,
    pub height: u32,
    /// Array length, or depth for 3D.
    pub depth_or_layers: u32,
    pub flags: u32,
    /// The layout of plane 0 (depth, for depth and stencil).
    pub layout: Layout,
    pub memory: *const u8,
    pub size: usize,
}

impl TextureInfo {
    pub fn render_target(&self) -> bool {
        self.flags & (RENDER_TARGET | DEPTH_STENCIL) != 0
    }

    /// The backend's description of it; `None` for formats or shapes the host does not know.
    pub fn desc(&self) -> Option<TextureDesc> {
        let depth = self.flags & DEPTH_STENCIL != 0;
        let mut format = format(self.format)?;
        // A typeless 32-bit float is a depth target when the texture is one.
        if depth && format == Format::R32Float {
            format = Format::D32Float;
        }
        let (dim, d) = match self.dimension {
            Dimension::Tex3d => (TexDim::D3, self.depth_or_layers as usize),
            _ if self.depth_or_layers > 1 => (TexDim::D2Array, self.depth_or_layers as usize),
            _ => (TexDim::D2, 1),
        };
        Some(TextureDesc {
            dim,
            format,
            width: self.width as usize,
            height: self.height.max(1) as usize,
            depth_or_layers: d,
            mips: self.layout.mips.len().max(1),
            render_target: self.render_target(),
            unordered_access: self.flags & UNORDERED_ACCESS != 0,
        })
    }
}

/// Reads `len` bytes of title memory (zeros where it cannot be read: a page the title never mapped).
pub fn read_memory(at: *const u8, len: usize) -> Vec<u8> {
    const CHUNK: usize = 1 << 20;
    let mut out = vec![0u8; len];
    for (i, chunk) in out.chunks_mut(CHUNK).enumerate() {
        // Only an address, checked by the kernel when read.
        let _ = super::safe_read(at.wrapping_add(i * CHUNK), chunk);
    }
    out
}

/// Detiles every mip of every slice of `tiled` (the texture's whole memory) into `tex`.
pub fn upload<B: ResourceBackend>(
    backend: &B,
    tex: &B::Texture,
    info: &TextureInfo,
    tiled: &[u8],
) -> Result<(), String> {
    let layout = &info.layout;
    let slices = if info.dimension == Dimension::Tex3d {
        1
    } else {
        layout.slices
    };
    let n = layout.desc.element_bytes as usize;
    for mip in 0..layout.mips.len() as u32 {
        let m = &layout.mips[mip as usize];
        let row = m.width as usize * n;
        let slice_pitch = row * m.height as usize;
        let depth = layout.subresource_depth(mip).max(1) as usize;
        let mut linear = vec![0u8; slice_pitch * depth];
        for slice in 0..slices {
            layout
                .detile(tiled, mip, slice, &mut linear, row, slice_pitch)
                .map_err(|e| format!("detile mip {mip} slice {slice}: {e:?}"))?;
            backend.upload_texture(tex, mip as usize, slice as usize, &linear, row, slice_pitch);
        }
    }
    Ok(())
}

/// A texture the host keeps for a title texture.
pub struct Tracked<B: ResourceBackend> {
    pub texture: Rc<B::Texture>,
    info: TextureInfo,
    /// The hash of the memory last uploaded; `None` for targets and before the first upload.
    last: Option<blake3::Hash>,
    /// Uploads of its memory so far.
    pub uploads: u64,
}

/// The key of a texture a view names by address and description (no resource object): a number no
/// object address has (the top bit is set).
pub fn placed_key(address: u64, description: &[u32; 14]) -> usize {
    let mut h = blake3::Hasher::new();
    h.update(&address.to_le_bytes());
    for (i, w) in description.iter().enumerate() {
        // Words 1 and 13 are padding (stack bytes in the title's records).
        h.update(&(if i == 1 || i == 13 { 0 } else { *w }).to_le_bytes());
    }
    let b = h.finalize();
    (u64::from_le_bytes(b.as_bytes()[..8].try_into().expect("8 bytes")) | 1 << 63) as usize
}

impl<B: PresentBackend> Title<B> {
    /// A title texture exists. Its host texture is made when a view, a render target or a clear
    /// first names it (most of a title's textures are never used), and filled from title memory
    /// when work next goes to the GPU.
    pub fn texture_created(&mut self, key: usize, info: TextureInfo) {
        self.pending.insert(key, info);
    }

    /// Makes the host texture of title texture `key` if it has none yet and registers it for
    /// views. `false` when there is none (not a title texture, or one the host cannot represent).
    pub fn realize(&mut self, key: usize) -> bool {
        if self.textures.contains_key(&key) {
            return true;
        }
        let Some(info) = self.pending.remove(&key) else {
            return false;
        };
        let Some(desc) = info.desc() else {
            tracing::debug!(format = info.format, "title texture the host does not represent");
            return false;
        };
        let tex = match self.host.texture(&desc) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, ?desc, "no host texture");
                return false;
            }
        };
        self.host.register_texture(key, &tex);
        // Debugging (VICTORY_MAGENTA=1): render targets start magenta, so a frame that stays so
        // was never drawn into, and a black one was drawn black.
        if info.render_target() && std::env::var_os("VICTORY_MAGENTA").is_some() {
            let m = &info.layout.mips[0];
            let n = info.layout.desc.element_bytes as usize;
            let row = m.width as usize * n;
            let pixel = [0xffu8, 0, 0xff, 0xff];
            let linear: Vec<u8> = (0..row * m.height as usize).map(|i| pixel[i % 4]).collect();
            self.host.backend.upload_texture(&tex, 0, 0, &linear, row, row * m.height as usize);
        }
        self.textures.insert(
            key,
            Tracked {
                texture: tex,
                info,
                last: None,
                uploads: 0,
            },
        );
        true
    }

    /// The host texture of title texture `key`.
    pub fn texture_of(&mut self, key: usize) -> Option<Rc<B::Texture>> {
        self.realize(key);
        self.textures.get(&key).map(|t| t.texture.clone())
    }

    /// How many times title texture `key`'s memory was detiled into its host texture.
    pub fn uploads_of(&self, key: usize) -> u64 {
        self.textures.get(&key).map_or(0, |t| t.uploads)
    }

    pub fn texture_released(&mut self, key: usize) {
        self.textures.remove(&key);
        self.pending.remove(&key);
    }

    /// Brings the textures the host samples up to date with title memory: those whose memory
    /// changed since their last upload are detiled again.
    pub fn sync_textures(&mut self) {
        let Title {
            host,
            textures,
            stats,
            ..
        } = self;
        for (key, t) in textures.iter_mut().filter(|(_, t)| !t.info.render_target()) {
            let tiled = read_memory(t.info.memory, t.info.size);
            let hash = blake3::hash(&tiled);
            if t.last == Some(hash) {
                continue;
            }
            match upload(&host.backend, &t.texture, &t.info, &tiled) {
                Ok(()) => {
                    t.last = Some(hash);
                    t.uploads += 1;
                    stats.detiles += 1;
                }
                Err(e) => {
                    t.last = Some(hash);
                    stats.undetilable += 1;
                    tracing::warn!(texture = key, error = %e, "a title texture was not detiled");
                }
            }
        }
    }
}

/// The textures of a title host, by title resource key.
pub type Textures<B> = HashMap<usize, Tracked<B>>;
