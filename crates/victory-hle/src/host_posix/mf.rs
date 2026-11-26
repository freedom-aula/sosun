//! Media Foundation (`mfplat.dll`, `mfreadwrite.dll`) without a media
//! pipeline, for the native macOS host.
//!
//! The platform starts and shuts down, and tells the time. Everything that
//! would make a media object (attributes, media types, the source resolver,
//! source readers) fails with `MF_E_UNSUPPORTED_BYTESTREAM_TYPE` and leaves
//! the caller's out pointer null, which is what Media Foundation says of a
//! file it cannot play. A title checks the `HRESULT` of its first call into a
//! video path and skips the path; one that does not would get a null object
//! back to dereference.
//!
//! shortcut: no COM objects at all; AVFoundation (`media.rs`'s Windows
//! reader) is the place to start if a title's video is wanted.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

type HRESULT = i32;

const S_OK: HRESULT = 0;
const MF_E_UNSUPPORTED_BYTESTREAM_TYPE: HRESULT = 0xC00D_36C4_u32 as i32;
static STARTED: AtomicU32 = AtomicU32::new(0);

pub extern "system" fn MFStartup(_version: u32, _flags: u32) -> HRESULT {
    STARTED.fetch_add(1, Ordering::AcqRel);
    S_OK
}

pub extern "system" fn MFShutdown() -> HRESULT {
    let _ = STARTED.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
    S_OK
}

/// `MFGetSystemTime`: the clock Media Foundation timestamps with, 100 ns
/// ticks since 1601.
pub extern "system" fn MFGetSystemTime() -> i64 {
    super::system::now_ticks() as i64
}

/// Fails with a null object in `*out`.
fn unsupported(out: *mut *mut c_void) -> HRESULT {
    if !out.is_null() {
        // SAFETY: the title's out parameter.
        unsafe { out.write(std::ptr::null_mut()) };
    }
    MF_E_UNSUPPORTED_BYTESTREAM_TYPE
}

pub extern "system" fn MFCreateAttributes(out: *mut *mut c_void, _size: u32) -> HRESULT {
    unsupported(out)
}

pub extern "system" fn MFCreateMediaType(out: *mut *mut c_void) -> HRESULT {
    unsupported(out)
}

pub extern "system" fn MFCreateSourceResolver(out: *mut *mut c_void) -> HRESULT {
    unsupported(out)
}

pub extern "system" fn MFCreateSourceReaderFromMediaSource(
    _source: *mut c_void,
    _attributes: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    unsupported(out)
}

pub extern "system" fn MFCreateSourceReaderFromURL(
    _url: *const u16,
    _attributes: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    unsupported(out)
}

pub extern "system" fn MFCreateSourceReaderFromByteStream(
    _stream: *mut c_void,
    _attributes: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    unsupported(out)
}

/// The function tables of `mfplat.dll` and `mfreadwrite.dll`.
pub(super) fn lookup(dll: &str, name: &str) -> Option<usize> {
    macro_rules! table {
        ($($f:ident),* $(,)?) => {
            match name {
                $(stringify!($f) => Some($f as *const () as usize),)*
                _ => None,
            }
        };
    }
    match dll {
        "mfplat.dll" => table!(
            MFStartup,
            MFShutdown,
            MFGetSystemTime,
            MFCreateAttributes,
            MFCreateMediaType,
            MFCreateSourceResolver,
        ),
        "mfreadwrite.dll" => table!(
            MFCreateSourceReaderFromMediaSource,
            MFCreateSourceReaderFromURL,
            MFCreateSourceReaderFromByteStream,
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_platform_starts_and_nothing_can_be_created() {
        assert_eq!(MFStartup(0x0002_0070, 0), S_OK);
        let mut object = 1usize as *mut c_void;
        let none = std::ptr::null_mut();
        let results = [
            MFCreateAttributes(&mut object, 4),
            MFCreateMediaType(&mut object),
            MFCreateSourceResolver(&mut object),
            MFCreateSourceReaderFromURL(c"x.mp4".as_ptr().cast(), none, &mut object),
            MFCreateSourceReaderFromMediaSource(none, none, &mut object),
        ];
        assert!(results.iter().all(|&r| r == MF_E_UNSUPPORTED_BYTESTREAM_TYPE));
        assert!(object.is_null());
        assert_eq!(MFShutdown(), S_OK);
        // 2020 or later, in 100 ns ticks since 1601.
        assert!(MFGetSystemTime() > 132_000_000_000_000_000);
        assert!(lookup("mfplat.dll", "MFStartup").is_some());
        assert!(lookup("mfreadwrite.dll", "MFStartup").is_none());
    }
}
