//! Root signature serialization without a host `d3d12.dll` (macOS): the
//! `RTS0` blob `D3D12SerializeRootSignature` returns, versions 1.0 and 1.1,
//! and the `ID3DBlob` it comes in.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;

use crate::object::{
    self, E_INVALIDARG, E_NOTIMPL, E_POINTER, HRESULT, IUNKNOWN, Iid, Object, ROOT_SIGNATURE, S_OK,
    Table, This, guid,
};
use crate::root::Desc;

const TABLE: u32 = 0;
const CONSTANTS: u32 = 1;
const BLOB_IID: Iid = guid(
    0x8ba5fb08,
    0x5195,
    0x40e2,
    [0xac, 0x58, 0x0d, 0x98, 0x9c, 0x3a, 0x01, 0x02],
);

fn u32_at(p: *const u8, offset: usize) -> u32 {
    // SAFETY: callers stay inside the title's structure.
    unsafe { p.add(offset).cast::<u32>().read_unaligned() }
}

/// The blob for `desc` (`version` 1 or 2: the 1.0 or 1.1 layout), or `None`
/// for a version or parameter type we do not know.
///
/// # Safety
/// `desc`'s pointers are valid for their counts, laid out for `version`.
pub(crate) unsafe fn serialize(version: u32, desc: &Desc) -> Option<Vec<u8>> {
    if !(1..=2).contains(&version) || desc.parameter_count > 4096 || desc.sampler_count > 2048 {
        return None;
    }
    let (n, samplers) = (desc.parameter_count as usize, desc.sampler_count as usize);
    let range = if version == 1 { 20 } else { 24 };
    let mut bodies = Vec::new();
    let mut entries = Vec::new();
    let body_start = 24 + 12 * n;
    for i in 0..n {
        // SAFETY: parameter `i` of the description (32 bytes each).
        let p = unsafe { desc.parameters.cast::<u8>().add(32 * i) };
        let (kind, visibility) = (u32_at(p, 0), u32_at(p, 24));
        let at = (body_start + bodies.len()) as u32;
        entries.extend([kind, visibility, at]);
        match kind {
            TABLE => {
                let count = u32_at(p, 8) as usize;
                // SAFETY: the table's own pointer is at +16.
                let ranges = unsafe { p.add(16).cast::<*const u8>().read_unaligned() };
                bodies.extend_from_slice(&(count as u32).to_le_bytes());
                bodies.extend_from_slice(&(at + 8).to_le_bytes());
                for r in 0..count {
                    // SAFETY: range `r` of the table.
                    let range_at = unsafe { ranges.add(range * r) };
                    for w in 0..range / 4 {
                        bodies.extend_from_slice(&u32_at(range_at, w * 4).to_le_bytes());
                    }
                }
            }
            1..=4 => {
                // Constants {register, space, count}; descriptors {register,
                // space} and, from 1.1, flags.
                let words = if kind == CONSTANTS || version == 2 {
                    3
                } else {
                    2
                };
                for w in 0..words {
                    bodies.extend_from_slice(&u32_at(p, 8 + 4 * w).to_le_bytes());
                }
            }
            _ => return None,
        }
    }
    let static_at = body_start + bodies.len();
    let version_field = version;
    let mut blob: Vec<u8> = [
        version_field,
        n as u32,
        24,
        samplers as u32,
        static_at as u32,
        desc.flags,
    ]
    .iter()
    .flat_map(|w| w.to_le_bytes())
    .collect();
    blob.extend(entries.iter().flat_map(|w| w.to_le_bytes()));
    blob.extend(bodies);
    // SAFETY: `samplers` static samplers of 52 bytes.
    blob.extend_from_slice(unsafe {
        std::slice::from_raw_parts(desc.samplers.cast::<u8>(), 52 * samplers)
    });
    Some(blob)
}

#[repr(C)]
struct Blob {
    object: Object,
    bytes: Vec<u8>,
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &[BLOB_IID]).then_some(this);
    object::answer(ROOT_SIGNATURE, iid, found, out)
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Blob>(this)
}
extern "system" fn pointer(this: This) -> *const u8 {
    // SAFETY: one of our blobs.
    unsafe { &*this.cast::<Blob>() }.bytes.as_ptr()
}
extern "system" fn size(this: This) -> usize {
    // SAFETY: one of our blobs.
    unsafe { &*this.cast::<Blob>() }.bytes.len()
}

static BLOB_FUNCTIONS: Table<8> = Table::new([
    query as *const (),
    object::add_ref as *const (),
    release as *const (),
    pointer as *const (),
    size as *const (),
    std::ptr::null(),
    std::ptr::null(),
    std::ptr::null(),
]);

/// Serializes and hands the blob out.
pub(crate) fn serialize_to(
    version: u32,
    desc: &Desc,
    blob: *mut *mut c_void,
    error: *mut *mut c_void,
) -> HRESULT {
    if blob.is_null() {
        return E_POINTER;
    }
    if !error.is_null() {
        // SAFETY: the title passes room for a pointer.
        unsafe { error.write(std::ptr::null_mut()) };
    }
    // SAFETY: the title's description, as the host's serializer would read it.
    let Some(bytes) = (unsafe { serialize(version, desc) }) else {
        return if (1..=2).contains(&version) {
            E_INVALIDARG
        } else {
            E_NOTIMPL
        };
    };
    let object = Box::new(Blob {
        object: Object::new(&BLOB_FUNCTIONS),
        bytes,
    });
    let mut out: This = std::ptr::null();
    let hr = object::hand_out(
        ROOT_SIGNATURE,
        object,
        &BLOB_IID,
        &[BLOB_IID, IUNKNOWN],
        &mut out,
    );
    // SAFETY: checked non-null.
    unsafe { blob.write(out.cast_mut().cast()) };
    if hr == S_OK { S_OK } else { hr }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_a_table_constants_and_a_sampler() {
        // 1.1 ranges: type, count, base, space, flags, offset.
        let ranges: [u32; 12] = [0, 4, 0, 0, 0, 0, 1, 1, 2, 0, 0, 4];
        let mut params = [0u32; 16];
        params[0] = TABLE; // type
        params[2] = 2; // two ranges
        let at = ranges.as_ptr() as usize as u64;
        params[4] = at as u32;
        params[5] = (at >> 32) as u32;
        params[6] = 5; // visibility
        params[8] = CONSTANTS;
        params[10] = 3; // register
        params[11] = 1; // space
        params[12] = 8; // dwords
        params[14] = 0; // visibility
        let samplers = [7u8; 52];
        let desc = Desc::new(2, params.as_ptr().cast(), 1, samplers.as_ptr().cast(), 0x6);
        // SAFETY: the structures above.
        let blob = unsafe { serialize(2, &desc) }.unwrap();
        let w = |i: usize| u32::from_le_bytes(blob[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!([w(0), w(1), w(2), w(3), w(5)], [2, 2, 24, 1, 6]);
        // Entries: (type, visibility, body offset).
        assert_eq!([w(6), w(7)], [TABLE, 5]);
        assert_eq!([w(9), w(10)], [CONSTANTS, 0]);
        let table_body = w(8) as usize / 4;
        assert_eq!([w(table_body), w(table_body + 1)], [2, w(8) + 8]);
        let constants = w(11) as usize / 4;
        assert_eq!(
            [w(constants), w(constants + 1), w(constants + 2)],
            [3, 1, 8]
        );
        let static_at = w(4) as usize;
        assert_eq!(&blob[static_at..], &samplers);
        assert_eq!(blob.len(), static_at + 52);
        // SAFETY: as above; an unknown version is refused before any read.
        assert!(unsafe { serialize(3, &desc) }.is_none());
    }
}
