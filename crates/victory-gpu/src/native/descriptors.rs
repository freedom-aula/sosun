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
use victory_backend::resources::{Address, BorderColor, Filter, Format, SamplerDesc};

/// What a heap entry holds, in backend terms.
#[derive(Debug, Clone, PartialEq)]
pub enum Decoded {
    /// A raw buffer SRV, UAV or CBV: `size` bytes at title address `addr`.
    Buffer {
        addr: u64,
        size: u32,
    },
    /// A texture SRV or UAV of a registered resource: the first mip and count of its view.
    /// A count of `u32::MAX` includes every remaining mip; the backend clips it to the texture.
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
        28 | 27 => Format::Rgba8Unorm,
        39 => Format::R32Float,
        29 => Format::Rgba8UnormSrgb,
        87 => Format::Bgra8Unorm,
        91 => Format::Bgra8UnormSrgb,
        10 => Format::Rgba16Float,
        41 => Format::R32Float,
        40 => Format::D32Float,
        // D32_FLOAT_S8X24_UINT (19), its shader views (20) and the console's variant (45).
        19 | 20 | 45 => Format::D32FloatS8,
        71 => Format::Bc1,
        74 => Format::Bc2,
        77 => Format::Bc3,
        80 => Format::Bc4,
        83 => Format::Bc5,
        95 => Format::Bc6h,
        96 => Format::Bc6hSigned,
        98 => Format::Bc7,
        61 => Format::R8Unorm,
        65 => Format::A8Unorm,
        56 => Format::R16Unorm,
        2 => Format::Rgba32Float,
        16 => Format::Rg32Float,
        24 => Format::Rgb10a2Unorm,
        26 => Format::Rg11b10Float,
        30 => Format::Rgba8Uint,
        34 => Format::Rg16Float,
        35 => Format::Rg16Unorm,
        42 => Format::R32Uint,
        62 => Format::R8Uint,
        67 => Format::Rgb9e5Float,
        99 => Format::Bc7Srgb,
        3 => Format::Rgba32Uint,
        11 => Format::Rgba16Unorm,
        13 => Format::Rgba16Snorm,
        17 => Format::Rg32Uint,
        36 => Format::Rg16Uint,
        49 => Format::Rg8Unorm,
        50 => Format::Rg8Uint,
        51 => Format::Rg8Snorm,
        54 => Format::R16Float,
        // R16_TYPELESS: a depth target when the texture is one (`TextureInfo::desc`).
        53 => Format::R16Unorm,
        57 => Format::R16Uint,
        55 => Format::D16Unorm,
        72 => Format::Bc1Srgb,
        78 => Format::Bc3Srgb,
        86 => Format::Bgr5a1Unorm,
        // B4G4R4A4: its title bytes are converted at upload (`textures::to_host`).
        115 => Format::Abgr4Unorm,
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
        // A texture the title placed in its own memory: named by address and description.
        View::Placed(unordered, res, d) if res[0] > BUFFER => {
            let base = u64::from(d[10]) | u64::from(d[11]) << 32;
            Some(Decoded::Image {
                resource: super::textures::placed_key(base, res),
                mips: if *unordered {
                    Some((d[2], 1))
                } else {
                    Some((d[4], d[5]))
                },
                format: format(d[0]),
            })
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
            mips: Some((d[4], d[5])),
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
    // Metal has three fixed border colors. Keep the existing edge clamp for other colors.
    let border_color = if f.address.contains(&4) {
        match f.border {
            [0.0, 0.0, 0.0, 0.0] => Some(BorderColor::TransparentBlack),
            [0.0, 0.0, 0.0, 1.0] => Some(BorderColor::OpaqueBlack),
            [1.0, 1.0, 1.0, 1.0] => Some(BorderColor::OpaqueWhite),
            _ => None,
        }
    } else {
        None
    };
    let mode = |m: u32| match m {
        1 => Address::Wrap,
        2 => Address::Mirror,
        4 if border_color.is_some() => Address::Border,
        _ => Address::Clamp,
    };
    Decoded::Sampler {
        desc: SamplerDesc {
            min: linear(f.min),
            mag: linear(f.mag),
            mip: if f.mip == 2 {
                Filter::Linear
            } else {
                Filter::Nearest
            },
            address: f.address.map(mode),
            border_color: border_color.unwrap_or(BorderColor::TransparentBlack),
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
    #[test]
    fn host_formats_have_the_title_formats_element_size() {
        for dxgi in [2, 3, 10, 11, 13, 16, 17, 24, 26, 28, 30, 34, 35, 36, 41, 42, 49, 50, 51, 53, 54, 55, 56, 57, 61, 62, 65, 67, 71, 72, 77, 78, 86, 91, 98, 99, 115] {
            let f = super::format(dxgi).expect("a host format");
            let (bytes, block) = crate::resource::element(dxgi).expect("a title format");
            assert_eq!(f.block(), (block as usize, bytes as usize), "DXGI {dxgi}");
        }
        assert!(super::format(42).unwrap().is_integer() && !super::format(26).unwrap().is_integer());
    }

    /// Both BC6H formats, apart: the unsigned one (95) and the signed one (96), 16-byte blocks of
    /// 4x4 texels as the title's.
    #[test]
    fn bc6h_keeps_its_signedness() {
        use victory_backend::resources::Format;
        assert_eq!(super::format(95), Some(Format::Bc6h));
        assert_eq!(super::format(96), Some(Format::Bc6hSigned));
        for dxgi in [95, 96] {
            let (bytes, block) = crate::resource::element(dxgi).expect("a title format");
            assert_eq!(
                super::format(dxgi).unwrap().block(),
                (block as usize, bytes as usize)
            );
        }
    }

    #[test]
    fn r8g8_formats_keep_their_numeric_kind() {
        use victory_backend::resources::Format;
        assert_eq!(super::format(49), Some(Format::Rg8Unorm));
        assert_eq!(super::format(50), Some(Format::Rg8Uint));
        assert_eq!(super::format(51), Some(Format::Rg8Snorm));
        assert!(super::format(50).unwrap().is_integer());
        assert!(!super::format(51).unwrap().is_integer());
    }

    #[test]
    fn bgra8_srgb_has_a_host_format() {
        use victory_backend::resources::Format;
        assert_eq!(super::format(91), Some(Format::Bgra8UnormSrgb));
        assert!(!super::format(91).unwrap().is_integer());
    }

    #[test]
    fn b4g4r4a4_texels_become_abgr4() {
        // DXGI: blue 1 in bits 0-3, green 2, red 3, alpha 4 in bits 12-15. Metal: alpha low, red top.
        let mut t = 0x4321u16.to_le_bytes().to_vec();
        crate::native::textures::to_host(115, &mut t);
        assert_eq!(u16::from_le_bytes([t[0], t[1]]), 0x3214);
        let mut t = 0x4321u16.to_le_bytes().to_vec();
        crate::native::textures::to_host(86, &mut t);
        assert_eq!(u16::from_le_bytes([t[0], t[1]]), 0x4321);
    }

    use super::*;

    #[test]
    fn texture_srvs_keep_all_remaining_mips_from_the_first_requested() {
        let mut srv = [0; 10];
        (srv[0], srv[1], srv[4], srv[5]) = (28, TEXTURE_2D, 2, u32::MAX);
        let mut placed = [0; 16];
        placed[..srv.len()].copy_from_slice(&srv);
        let mut resource = [0; 14];
        resource[0] = 3; // D3D12_RESOURCE_DIMENSION_TEXTURE2D

        for view in [View::Shader(7, Some(srv)), View::Placed(false, resource, placed)] {
            let Some(Decoded::Image { mips, .. }) = decode(&view, &|_| None) else {
                panic!("expected a texture view");
            };
            assert_eq!(mips, Some((2, u32::MAX)));
        }
    }

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
    fn console_samplers_preserve_supported_border_modes_and_colors() {
        let sampler = |mode, rgba: [f32; 4]| {
            let mut words = [0; 27];
            words[6..9].copy_from_slice(&[mode, 3, 3]);
            words[17..21].copy_from_slice(&rgba.map(f32::to_bits));
            let Some(Decoded::Sampler { desc, .. }) = decode(&View::ConsoleSampler(words), &|_| None) else {
                panic!("expected a sampler");
            };
            desc
        };

        for (rgba, color) in [
            ([0.0; 4], BorderColor::TransparentBlack),
            ([0.0, 0.0, 0.0, 1.0], BorderColor::OpaqueBlack),
            ([1.0; 4], BorderColor::OpaqueWhite),
        ] {
            let border = sampler(4, rgba);
            let edge = sampler(3, rgba);
            assert_eq!(border.address, [Address::Border, Address::Clamp, Address::Clamp]);
            assert_eq!(border.border_color, color);
            assert_eq!(edge.address, [Address::Clamp; 3]);
            assert_eq!(edge.border_color, BorderColor::TransparentBlack);
            assert_ne!(border, edge);
        }
        let custom = [0.25, 0.5, 0.0, 1.0];
        assert_eq!(sampler(4, custom), sampler(3, custom), "unsupported color retains edge clamp");
    }

    #[test]
    fn console_samplers_preserve_minification_with_nearest_magnification() {
        let sampler = |min| {
            let mut words = [0; 27];
            words[2] = min;
            let Some(Decoded::Sampler { desc, .. }) = decode(&View::ConsoleSampler(words), &|_| None)
            else {
                panic!("expected a sampler");
            };
            desc
        };
        let nearest = sampler(0);
        let linear = sampler(1);
        assert_eq!((nearest.min, nearest.mag), (Filter::Nearest, Filter::Nearest));
        assert_eq!((linear.min, linear.mag), (Filter::Linear, Filter::Nearest));
        assert_ne!(nearest, linear);
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
        assert_eq!((desc.min, desc.mag), (Filter::Nearest, Filter::Linear));
        assert_eq!(desc.mip, Filter::Linear);
        assert_eq!(
            desc.address,
            [Address::Wrap, Address::Mirror, Address::Clamp]
        );
    }
}
