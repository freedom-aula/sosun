//! The title's descriptors, decoded for a native backend.
//!
//! A title writes a view with `CreateShaderResourceView` and friends; `descriptor::record` keeps the
//! description (in the host's layout, as words) and leaves `[tag, index, generation]` in the 32-byte
//! (16 for samplers) slot, so that titles that copy slots as bytes move whole descriptors. The D3D12
//! host builds its views from those records (`descriptor::host`); this module reads the same records
//! and gives the backend's `write_*` calls what they need. Texture descriptors made by
//! `CreateShaderResourceViewPlaced` (the title's own tiled memory) are not decoded here: they need the
//! detile pass (see docs/metal-backend.md, "What is left").

use crate::descriptor::View;
use victory_backend::resources::{Address, Filter, Format, SamplerDesc};

/// What a heap entry holds, in backend terms.
#[derive(Debug, Clone, PartialEq)]
pub enum Decoded {
    /// A raw buffer SRV, UAV or CBV: `size` bytes at title address `addr`.
    Buffer {
        addr: u64,
        size: u32,
    },
    /// A texture SRV or UAV of a registered resource: the first mip and count of its view.
    Image {
        resource: usize,
        mips: Option<(u32, u32)>,
        format: Option<Format>,
    },
    Sampler {
        desc: SamplerDesc,
        lod_bias: f32,
    },
}

/// SRV and UAV `ViewDimension` of a buffer.
const BUFFER: u32 = 1;
/// SRV `ViewDimension` of `Texture2D`; UAV's numbering is the same up to 3D.
const TEXTURE_2D: u32 = 4;
const FLAG_RAW: u32 = 1;

/// Bytes per element of a buffer view and the range it covers from `base`.
fn buffer_view(
    base: u64,
    format: u32,
    stride: u32,
    flags: u32,
    first: u64,
    count: u32,
) -> Option<(u64, u32)> {
    let element = if stride > 0 {
        u64::from(stride)
    } else if flags & FLAG_RAW != 0 {
        4
    } else {
        crate::resource::element(crate::resource::pc_format(format))?.0
    };
    let size = (u64::from(count) * element).min(u64::from(u32::MAX));
    Some((base + first * element, size as u32))
}

/// The DXGI formats of textures the native host knows.
pub fn format(dxgi: u32) -> Option<Format> {
    Some(match crate::resource::pc_format(dxgi) {
        28 => Format::Rgba8Unorm,
        29 => Format::Rgba8UnormSrgb,
        87 => Format::Bgra8Unorm,
        10 => Format::Rgba16Float,
        41 => Format::R32Float,
        40 => Format::D32Float,
        20 | 45 => Format::D32FloatS8,
        71 => Format::Bc1,
        74 => Format::Bc2,
        77 => Format::Bc3,
        80 => Format::Bc4,
        83 => Format::Bc5,
        95 => Format::Bc6h,
        98 => Format::Bc7,
        _ => return None,
    })
}

/// `buffer_of` gives the title address and size of a buffer resource object.
pub(crate) fn decode(
    view: &View,
    buffer_of: &dyn Fn(usize) -> Option<(u64, u64)>,
) -> Option<Decoded> {
    match view {
        View::Constant(addr, size) => Some(Decoded::Buffer {
            addr: *addr,
            size: *size,
        }),
        // SRV words: format, dimension, mapping, pad, then the union (buffer: first element (8
        // bytes), count, stride, flags; 2D: first mip, mip count).
        View::Shader(r, Some(d)) if d[1] == BUFFER => {
            let (base, _) = buffer_of(*r)?;
            let first = u64::from(d[4]) | u64::from(d[5]) << 32;
            let (addr, size) = buffer_view(base, d[0], d[7], d[8], first, d[6])?;
            Some(Decoded::Buffer { addr, size })
        }
        // UAV words: format, dimension, then the union (buffer: first element (8 bytes), count,
        // stride, counter offset (8 bytes), flags; 2D: mip slice, plane).
        View::Unordered(r, _, Some(d)) if d[1] == BUFFER => {
            let (base, _) = buffer_of(*r)?;
            let first = u64::from(d[2]) | u64::from(d[3]) << 32;
            let (addr, size) = buffer_view(base, d[0], d[5], d[8], first, d[4])?;
            Some(Decoded::Buffer { addr, size })
        }
        View::Placed(unordered, res, d) if res[0] == BUFFER => {
            let base = u64::from(d[10]) | u64::from(d[11]) << 32;
            let w: &[u32] = &d[..10];
            if *unordered {
                let first = u64::from(w[2]) | u64::from(w[3]) << 32;
                let (addr, size) = buffer_view(base, w[0], w[5], w[8], first, w[4])?;
                Some(Decoded::Buffer { addr, size })
            } else {
                let first = u64::from(w[4]) | u64::from(w[5]) << 32;
                let (addr, size) = buffer_view(base, w[0], w[7], w[8], first, w[6])?;
                Some(Decoded::Buffer { addr, size })
            }
        }
        // A whole buffer (no description): raw.
        View::Shader(r, None) | View::Unordered(r, _, None) => {
            let (addr, size) = buffer_of(*r)?;
            Some(Decoded::Buffer {
                addr,
                size: size.min(u64::from(u32::MAX)) as u32,
            })
        }
        View::Shader(r, Some(d)) if d[1] == TEXTURE_2D => Some(Decoded::Image {
            resource: *r,
            mips: Some((d[4], if d[5] == u32::MAX { 0 } else { d[5] })),
            format: format(d[0]),
        }),
        View::Unordered(r, _, Some(d)) if d[1] == TEXTURE_2D => Some(Decoded::Image {
            resource: *r,
            mips: Some((d[2], 1)),
            format: format(d[0]),
        }),
        View::Shader(r, Some(d)) | View::Unordered(r, _, Some(d)) => Some(Decoded::Image {
            resource: *r,
            mips: None,
            format: format(d[0]),
        }),
        View::ConsoleSampler(w) => Some(sampler(&crate::sampler::console_fields(w))),
        // shortcut: the plain D3D12 sampler description (13 words) is not decoded; titles on this
        // console use `CreateSamplerX`.
        View::Sampler(_) | View::Placed(..) => None,
    }
}

fn sampler(f: &crate::sampler::Fields) -> Decoded {
    let linear = |v: u32| {
        if v == 1 || v == 3 {
            Filter::Linear
        } else {
            Filter::Nearest
        }
    };
    let aniso = f.mag >= 2 || f.min >= 2;
    let mode = |m: u32| match m {
        1 => Address::Wrap,
        2 => Address::Mirror,
        _ => Address::Clamp,
    };
    Decoded::Sampler {
        desc: SamplerDesc {
            min_mag: linear(f.mag),
            mip: if f.mip == 2 {
                Filter::Linear
            } else {
                Filter::Nearest
            },
            address: [mode(f.address[0]), mode(f.address[1]), mode(f.address[2])],
            max_anisotropy: if aniso {
                f.anisotropy.clamp(1, 16) as usize
            } else {
                1
            },
        },
        lod_bias: if f.bias.is_nan() {
            0.0
        } else {
            f.bias.clamp(-16.0, 15.99)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_views_decode_to_raw_ranges() {
        let base = |r: usize| (r == 7).then_some((0x1000_0000u64, 0x1_0000u64));
        // A raw SRV: 16 elements of 4 bytes from element 4.
        let mut srv = [0u32; 10];
        srv[1] = BUFFER;
        srv[4] = 4;
        srv[6] = 16;
        srv[8] = FLAG_RAW;
        assert_eq!(
            decode(&View::Shader(7, Some(srv)), &base),
            Some(Decoded::Buffer {
                addr: 0x1000_0010,
                size: 64
            })
        );
        // A structured UAV: 5 elements of 12 bytes.
        let mut uav = [0u32; 10];
        uav[1] = BUFFER;
        uav[4] = 5;
        uav[5] = 12;
        assert_eq!(
            decode(&View::Unordered(7, 0, Some(uav)), &base),
            Some(Decoded::Buffer {
                addr: 0x1000_0000,
                size: 60
            })
        );
        // A typed R32_FLOAT view: the format gives the element size.
        srv[0] = 41;
        srv[4] = 0;
        srv[8] = 0;
        assert_eq!(
            decode(&View::Shader(7, Some(srv)), &base),
            Some(Decoded::Buffer {
                addr: 0x1000_0000,
                size: 64
            })
        );
        assert_eq!(decode(&View::Shader(8, Some(srv)), &base), None);
        assert_eq!(
            decode(&View::Constant(0x2000, 256), &base),
            Some(Decoded::Buffer {
                addr: 0x2000,
                size: 256
            })
        );
    }

    #[test]
    fn console_samplers_decode() {
        let mut w = [0u32; 27];
        w[1] = 1; // bilinear magnification
        w[3] = 2; // linear mips
        w[6..9].copy_from_slice(&[1, 2, 3]);
        let Some(Decoded::Sampler { desc, .. }) = decode(&View::ConsoleSampler(w), &|_| None)
        else {
            panic!("not a sampler");
        };
        assert_eq!(desc.min_mag, Filter::Linear);
        assert_eq!(desc.mip, Filter::Linear);
        assert_eq!(
            desc.address,
            [Address::Wrap, Address::Mirror, Address::Clamp]
        );
    }
}
