//! Root signature serialization without a host `d3d12.dll` (macOS): the
//! `RTS0` blob `D3D12SerializeRootSignature` returns, versions 1.0 and 1.1,
//! and the `ID3DBlob` it comes in; and the reverse, the deserializer
//! `D3D12CreateRootSignatureDeserializer` hands out.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;

use victory_shader::{Container, FourCC};

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

const DESERIALIZER_IID: Iid = guid(
    0x34ab647b,
    0x3cc8,
    0x46ac,
    [0x84, 0x1b, 0xc0, 0x96, 0x56, 0x45, 0xc0, 0x46],
);

/// An `ID3D12RootSignatureDeserializer`: the 1.0 description and the arrays
/// its pointers point into, which live as long as the object.
#[repr(C)]
struct Deserializer {
    object: Object,
    desc: Desc,
    /// `D3D12_ROOT_PARAMETER`s (32 bytes): type, union at +8, visibility at +24.
    parameters: Vec<[u64; 4]>,
    /// `D3D12_DESCRIPTOR_RANGE`s: type, count, base register, space, offset.
    ranges: Vec<[u32; 5]>,
    samplers: Vec<[u32; 13]>,
}

/// `N` little-endian words at `at`, if the blob has them.
fn words<const N: usize>(bytes: &[u8], at: usize) -> Option<[u32; N]> {
    let span = bytes.get(at..at.checked_add(4 * N)?)?;
    Some(std::array::from_fn(|i| {
        u32::from_le_bytes(span[4 * i..4 * i + 4].try_into().expect("4 bytes"))
    }))
}

/// Element `i` of `stride` bytes from the blob offset `base`.
fn element(base: u32, i: usize, stride: usize) -> Option<usize> {
    (base as usize).checked_add(i.checked_mul(stride)?)
}

/// Reads an `RTS0` blob (bare, or the part of a DXBC container) as the 1.0
/// description; `None` if it is malformed. Guess: a 1.1 blob is read as
/// Microsoft's 1.0 deserializer is believed to read it, its range and
/// descriptor flags dropped (`we_read_the_hosts_blobs_as_it_does` checks this
/// on Windows).
fn deserialize(bytes: &[u8]) -> Option<Box<Deserializer>> {
    let bytes = if bytes.starts_with(b"DXBC") {
        Container::parse(bytes).ok()?.part(FourCC::RTS0)?.data
    } else {
        bytes
    };
    let [
        version,
        count,
        parameters_at,
        sampler_count,
        samplers_at,
        flags,
    ] = words(bytes, 0)?;
    if !(1..=2).contains(&version) || count > 4096 || sampler_count > 2048 {
        return None;
    }
    let range_size = if version == 1 { 20 } else { 24 };
    let mut parameters = vec![[0u64; 4]; count as usize];
    let mut ranges = Vec::new();
    // (parameter, first range) of each table, given its pointer at the end.
    let mut tables = Vec::new();
    // Two 32-bit fields share each 64-bit word (little-endian).
    let pair = |[low, high]: [u32; 2]| u64::from(low) | u64::from(high) << 32;
    for (i, parameter) in parameters.iter_mut().enumerate() {
        let [kind, visibility, at] = words(bytes, element(parameters_at, i, 12)?)?;
        let at = at as usize;
        *parameter = [kind.into(), 0, 0, visibility.into()];
        match kind {
            TABLE => {
                let [n, ranges_at] = words(bytes, at)?;
                // Each range has its own bytes in a well-formed blob, so tables
                // sharing theirs cannot make the reader allocate past its size.
                if ranges.len().checked_add(n as usize)? > bytes.len() / range_size {
                    return None;
                }
                tables.push((i, ranges.len()));
                parameter[1] = n.into();
                for r in 0..n as usize {
                    let at = element(ranges_at, r, range_size)?;
                    ranges.push(if version == 1 {
                        words(bytes, at)?
                    } else {
                        let [kind, n, base, space, _flags, offset] = words(bytes, at)?;
                        [kind, n, base, space, offset]
                    });
                }
            }
            CONSTANTS => {
                let [register, space, values] = words(bytes, at)?;
                (parameter[1], parameter[2]) = (pair([register, space]), values.into());
            }
            2..=4 => {
                // 1.1 descriptors end with flags, which 1.0 has no field for.
                let [register, space] = if version == 1 {
                    words(bytes, at)?
                } else {
                    let [register, space, _flags] = words(bytes, at)?;
                    [register, space]
                };
                parameter[1] = pair([register, space]);
            }
            _ => return None,
        }
    }
    let samplers = (0..sampler_count as usize)
        .map(|s| words(bytes, element(samplers_at, s, 52)?))
        .collect::<Option<Vec<_>>>()?;
    for (i, first) in tables {
        parameters[i][2] = ranges[first..].as_ptr() as usize as u64;
    }
    let desc = Desc {
        parameter_count: count,
        parameters: parameters.as_ptr().cast(),
        sampler_count,
        samplers: samplers.as_ptr().cast(),
        flags,
    };
    Some(Box::new(Deserializer {
        object: Object::new(&DESERIALIZER_FUNCTIONS),
        desc,
        parameters,
        ranges,
        samplers,
    }))
}

extern "system" fn deserializer_query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &[DESERIALIZER_IID]).then_some(this);
    object::answer(ROOT_SIGNATURE, iid, found, out)
}
extern "system" fn deserializer_release(this: This) -> u32 {
    object::release_boxed::<Deserializer>(this)
}
extern "system" fn description(this: This) -> *const Desc {
    // SAFETY: one of our deserializers.
    &unsafe { &*this.cast::<Deserializer>() }.desc
}

static DESERIALIZER_FUNCTIONS: Table<4> = Table::new([
    deserializer_query as *const (),
    object::add_ref as *const (),
    deserializer_release as *const (),
    description as *const (),
]);

/// `D3D12CreateRootSignatureDeserializer` without a host `d3d12.dll`.
pub(crate) fn deserialize_to(bytes: &[u8], iid: *const Iid, out: *mut *mut c_void) -> HRESULT {
    let out = out.cast::<This>();
    let Some(object) = deserialize(bytes) else {
        tracing::warn!(size = bytes.len(), "malformed root signature blob");
        if !out.is_null() {
            // SAFETY: the title passes room for a pointer.
            unsafe { out.write(std::ptr::null()) };
        }
        return E_INVALIDARG;
    };
    object::hand_out(ROOT_SIGNATURE, object, iid, &[DESERIALIZER_IID], out)
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

    use crate::object::{E_NOINTERFACE, slot};

    /// A table of an SRV and a UAV range, root constants, a root CBV and a
    /// static sampler, valid for the host's validation too.
    struct Sample {
        _ranges: Vec<u32>,
        parameters: Vec<[u64; 4]>,
        samplers: Vec<[u32; 13]>,
    }

    impl Sample {
        fn new(version: u32) -> Self {
            // 1.1 adds flags: descriptors volatile, data volatile, data static.
            let ranges = if version == 1 {
                vec![0, 4, 0, 0, 0, 1, 2, 1, 0, 4]
            } else {
                vec![0, 4, 0, 0, 1, 0, 1, 2, 1, 0, 2, 4]
            };
            let pair = |low: u32, high: u32| u64::from(low) | u64::from(high) << 32;
            let parameters = vec![
                [0, 2, ranges.as_ptr() as usize as u64, 5],
                [1, pair(3, 1), 8, 0],
                [2, 0, if version == 1 { 0 } else { 8 }, 0],
            ];
            // Linear, wrap, anisotropy 16, always, LOD 0 to max, s0, pixel.
            let max = f32::MAX.to_bits();
            let samplers = vec![[0x15, 1, 1, 1, 0, 16, 8, 0, 0, max, 0, 0, 5]];
            Sample {
                _ranges: ranges,
                parameters,
                samplers,
            }
        }

        fn desc(&self) -> Desc {
            let (p, s) = (self.parameters.as_ptr(), self.samplers.as_ptr());
            Desc::new(3, p.cast(), 1, s.cast(), 1)
        }

        fn blob(version: u32) -> Vec<u8> {
            // SAFETY: the sample's arrays, laid out for `version`.
            unsafe { serialize(version, &Self::new(version).desc()) }.unwrap()
        }
    }

    /// What a 1.0 reader gives for [`Sample`], whichever the version.
    fn expected() -> Vec<u32> {
        let max = f32::MAX.to_bits();
        vec![
            3, 1, 1, // counts, flags
            0, 5, 2, 0, 4, 0, 0, 0, 1, 2, 1, 0, 4, // table: SRV t0-t3, UAV u1-u2
            1, 0, 3, 1, 8, // 8 constants at b3, space 1
            2, 0, 0, 0, // CBV b0
            0x15, 1, 1, 1, 0, 16, 8, 0, 0, max, 0, 0, 5,
        ]
    }

    /// A description as the words of a 1.0 reader: counts and flags, each
    /// parameter's type, visibility and fields (ranges without 1.1 flags,
    /// read through the table's pointer), then the samplers.
    ///
    /// # Safety
    /// As for [`serialize`].
    unsafe fn flat(version: u32, desc: &Desc) -> Vec<u32> {
        let mut out = vec![desc.parameter_count, desc.sampler_count, desc.flags];
        for i in 0..desc.parameter_count as usize {
            // SAFETY: parameter `i` of the description.
            let p = unsafe { desc.parameters.cast::<u8>().add(32 * i) };
            let kind = u32_at(p, 0);
            out.extend([kind, u32_at(p, 24)]);
            match kind {
                TABLE => {
                    let n = u32_at(p, 8) as usize;
                    out.push(n as u32);
                    // SAFETY: the table's own pointer is at +16.
                    let ranges = unsafe { p.add(16).cast::<*const u8>().read_unaligned() };
                    let size = if version == 1 { 20 } else { 24 };
                    for r in 0..n {
                        // SAFETY: range `r` of the table.
                        let range = unsafe { ranges.add(size * r) };
                        out.extend([0, 4, 8, 12, size - 4].map(|o| u32_at(range, o)));
                    }
                }
                CONSTANTS => out.extend([8, 12, 16].map(|o| u32_at(p, o))),
                _ => out.extend([8, 12].map(|o| u32_at(p, o))),
            }
        }
        let samplers = desc.samplers.cast::<u8>();
        out.extend((0..13 * desc.sampler_count as usize).map(|w| u32_at(samplers, 4 * w)));
        out
    }

    fn read(bytes: &[u8]) -> This {
        let mut out = std::ptr::null_mut();
        assert_eq!(deserialize_to(bytes, &DESERIALIZER_IID, &mut out), S_OK);
        out.cast_const().cast()
    }

    fn description_of(deserializer: This) -> &'static Desc {
        let get = slot::<extern "system" fn(This) -> *const Desc>(deserializer, 0x18);
        // SAFETY: valid while the deserializer lives, which the tests ensure.
        unsafe { &*get(deserializer) }
    }

    /// Malformed: `E_INVALIDARG` and a null object.
    fn rejected(bytes: &[u8]) -> bool {
        let mut out = std::ptr::NonNull::<c_void>::dangling().as_ptr();
        deserialize_to(bytes, &DESERIALIZER_IID, &mut out) == E_INVALIDARG && out.is_null()
    }

    #[test]
    fn both_versions_read_back_as_the_1_0_description() {
        for version in [1, 2] {
            let sample = Sample::new(version);
            // SAFETY: the sample's arrays.
            assert_eq!(unsafe { flat(version, &sample.desc()) }, expected());
            let reader = read(&Sample::blob(version));
            // SAFETY: our object's description.
            let words = unsafe { flat(1, description_of(reader)) };
            assert_eq!(words, expected(), "version {version}");
            assert_eq!(object::release(reader), 0);
        }
    }

    #[test]
    fn the_rts0_part_of_a_container_is_read() {
        let blob = Sample::blob(2);
        let container =
            victory_shader::dxbc::build(&[(FourCC::SFI0, &[0; 8][..]), (FourCC::RTS0, &blob)]);
        let reader = read(&container);
        // SAFETY: our object's description.
        assert_eq!(unsafe { flat(1, description_of(reader)) }, expected());
        assert_eq!(object::release(reader), 0);

        let no_rts0 = victory_shader::dxbc::build(&[(FourCC::SFI0, &[0; 8][..])]);
        assert!(rejected(&no_rts0));
        assert!(rejected(&container[..container.len() - 1]));
        assert!(rejected(b"DXBC"));
    }

    #[test]
    fn malformed_blobs_are_refused() {
        for version in [1, 2] {
            let blob = Sample::blob(version);
            // The samplers end the blob, so every prefix is short of them.
            for len in 0..blob.len() {
                assert!(rejected(&blob[..len]), "version {version}, {len} bytes");
            }
            let patched = |word: usize, value: u32| {
                let mut b = blob.clone();
                b[4 * word..4 * word + 4].copy_from_slice(&value.to_le_bytes());
                b
            };
            let w = |i: usize| u32::from_le_bytes(blob[4 * i..4 * i + 4].try_into().unwrap());
            let table_body = w(8) as usize / 4;
            for bad in [
                patched(0, 0),                        // version
                patched(0, 3),                        // version
                patched(1, 4097),                     // parameter count
                patched(1, u32::MAX),                 // parameter count
                patched(2, u32::MAX - 3),             // parameter offset
                patched(3, 2049),                     // sampler count
                patched(3, 2),                        // one sampler short
                patched(4, u32::MAX - 51),            // sampler offset
                patched(12, 5),                       // third parameter's type
                patched(8, blob.len() as u32),        // table body offset
                patched(14, u32::MAX),                // CBV body offset
                patched(table_body, u32::MAX),        // range count
                patched(table_body + 1, u32::MAX),    // range offset
                patched(table_body + 1, 0xffff_fff0), // range offset
            ] {
                assert!(rejected(&bad), "version {version}");
            }
            assert!(!rejected(&blob));

            // A table of every range that fits before the blob's end reads,
            // but two such tables over the same bytes exceed what it holds.
            let size = if version == 1 { 20 } else { 24 };
            let fit = (blob.len() - 4 * (table_body + 2)) / size;
            assert!(2 * fit > blob.len() / size);
            let mut one = patched(table_body, fit as u32);
            assert!(!rejected(&one), "version {version}");
            one[36..40].copy_from_slice(&TABLE.to_le_bytes());
            one[44..48].copy_from_slice(&blob[32..36]);
            assert!(rejected(&one), "version {version}");
        }
        assert!(rejected(&[0xff; 64]));
    }

    #[test]
    fn the_deserializer_is_a_com_object() {
        let reader = read(&Sample::blob(1));
        let query = slot::<extern "system" fn(This, *const Iid, *mut This) -> HRESULT>(reader, 0);
        for iid in [DESERIALIZER_IID, IUNKNOWN] {
            let mut out = std::ptr::null();
            assert_eq!(query(reader, &iid, &mut out), S_OK);
            assert!(std::ptr::eq(out, reader));
        }
        let mut out: This = std::ptr::NonNull::dangling().as_ptr();
        assert_eq!(query(reader, &BLOB_IID, &mut out), E_NOINTERFACE);
        assert!(out.is_null());
        let add_ref = slot::<extern "system" fn(This) -> u32>(reader, 8);
        let release = slot::<extern "system" fn(This) -> u32>(reader, 0x10);
        assert_eq!(add_ref(reader), 4);
        let first = description_of(reader) as *const Desc;
        assert!(std::ptr::eq(first, description_of(reader)));
        assert_eq!(
            [release(reader), release(reader), release(reader)],
            [3, 2, 1]
        );
        assert_eq!(release(reader), 0);

        // An unknown interface hands nothing out, however good the blob.
        let mut out = std::ptr::NonNull::<c_void>::dangling().as_ptr();
        let blob = Sample::blob(2);
        assert_eq!(deserialize_to(&blob, &BLOB_IID, &mut out), E_NOINTERFACE);
        assert!(out.is_null());
    }

    /// Without a host `d3d12.dll` the export reads the blob itself.
    #[cfg(not(windows))]
    #[test]
    fn the_export_reads_blobs_without_a_host() {
        let blob = Sample::blob(1);
        let mut out = std::ptr::null_mut();
        let hr = crate::root::create_deserializer(
            blob.as_ptr(),
            blob.len(),
            &DESERIALIZER_IID,
            &mut out,
        );
        assert_eq!(hr, S_OK);
        let reader = out.cast_const().cast();
        // SAFETY: our object's description.
        assert_eq!(unsafe { flat(1, description_of(reader)) }, expected());
        assert_eq!(object::release(reader), 0);
    }

    /// Microsoft's serializer writes the sample (1.0 by
    /// `D3D12SerializeRootSignature`, 1.1 by the versioned form), and its
    /// deserializer and ours read the same 1.0 description back (this
    /// settles the guess that 1.1 flags are dropped). Its blobs are DXBC
    /// containers, whose `RTS0` part we read.
    #[cfg(windows)]
    #[test]
    fn we_read_the_hosts_blobs_as_it_does() {
        use crate::root::VersionedDesc;
        for version in [1, 2] {
            let sample = Sample::new(version);
            let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
            let hr = if version == 1 {
                crate::root::serialize(&sample.desc(), 1, &mut blob, &mut error)
            } else {
                let desc = VersionedDesc {
                    version,
                    desc: sample.desc(),
                };
                crate::root::serialize_versioned(&desc, &mut blob, &mut error)
            };
            assert_eq!(hr, S_OK, "version {version}");
            let blob = blob.cast_const().cast::<Object>();
            let data = slot::<extern "system" fn(This) -> *const u8>(blob, 0x18)(blob);
            let size = slot::<extern "system" fn(This) -> usize>(blob, 0x20)(blob);
            // SAFETY: the host's blob holds `size` bytes while it lives.
            let bytes = unsafe { std::slice::from_raw_parts(data, size) }.to_vec();
            slot::<extern "system" fn(This) -> u32>(blob, 0x10)(blob);
            assert!(bytes.starts_with(b"DXBC"), "version {version}");

            let mut host = std::ptr::null_mut();
            let hr = crate::root::create_deserializer(
                bytes.as_ptr(),
                bytes.len(),
                &DESERIALIZER_IID,
                &mut host,
            );
            assert_eq!(hr, S_OK, "version {version}");
            let host = host.cast_const().cast::<Object>();
            let ours = read(&bytes);
            // SAFETY: both objects' 1.0 descriptions.
            let (theirs, mine) =
                unsafe { (flat(1, description_of(host)), flat(1, description_of(ours))) };
            assert_eq!(theirs, mine, "version {version}");
            assert_eq!(mine, expected());
            slot::<extern "system" fn(This) -> u32>(host, 0x10)(host);
            assert_eq!(object::release(ours), 0);
        }
    }
}
