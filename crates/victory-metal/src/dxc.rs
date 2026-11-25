//! HLSL -> DXIL container through a dlopen'ed `libdxcompiler` (only to make a test shader; the
//! backend itself never compiles HLSL). WCHAR is 32-bit on macOS.

use std::ffi::{CString, c_void};

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);

const CLSID_DXC_COMPILER: Guid = Guid(
    0x73e22d93,
    0xe6ce,
    0x47f3,
    [0xb5, 0xbf, 0xf0, 0x66, 0x4f, 0x39, 0xc1, 0xb0],
);
const IID_DXC_COMPILER3: Guid = Guid(
    0x228b4687,
    0x5a6a,
    0x4730,
    [0x90, 0x0c, 0x97, 0x02, 0xb2, 0x20, 0x3f, 0x54],
);
const IID_DXC_RESULT: Guid = Guid(
    0x58346cda,
    0xdde7,
    0x4497,
    [0x94, 0x61, 0x6f, 0x87, 0xaf, 0x5e, 0x06, 0x59],
);
const IID_DXC_BLOB: Guid = Guid(
    0x8ba5fb08,
    0x5195,
    0x40e2,
    [0xac, 0x58, 0x0d, 0x98, 0x9c, 0x3a, 0x01, 0x02],
);

#[repr(C)]
struct DxcBuffer {
    ptr: *const c_void,
    size: usize,
    encoding: u32,
}

type Vtbl = *const *const c_void;

/// Calls vtable slot `i` of a COM object.
macro_rules! com {
    ($o:expr, $i:expr, fn($($t:ty),* $(,)?) -> $r:ty, $($a:expr),*) => {{
        // SAFETY (caller): `$o` is a live COM object whose slot `$i` has this signature.
        let obj = $o;
        let vt = *(obj as *const Vtbl);
        let f: unsafe extern "C" fn(*mut c_void, $($t),*) -> $r = std::mem::transmute(*vt.add($i));
        f(obj, $($a),*)
    }};
}

pub fn hlsl_to_dxil(dylib: &str, hlsl: &str, profile: &str) -> Result<Vec<u8>, String> {
    let path = CString::new(dylib).unwrap();
    // SAFETY: COM calls follow dxcapi.h: IDxcCompiler3 { Compile = slot 3 }, IDxcResult { GetStatus = 3,
    // GetOutput = 7 }, IDxcBlob { GetBufferPointer = 3, GetBufferSize = 4 }; the objects are leaked (probe code).
    unsafe {
        let h = libc::dlopen(path.as_ptr(), libc::RTLD_NOW);
        if h.is_null() {
            return Err("dlopen libdxcompiler failed".into());
        }
        let create = libc::dlsym(h, c"DxcCreateInstance".as_ptr());
        if create.is_null() {
            return Err("no DxcCreateInstance".into());
        }
        let create: unsafe extern "C" fn(*const Guid, *const Guid, *mut *mut c_void) -> i32 =
            std::mem::transmute(create);
        let mut comp = std::ptr::null_mut();
        let hr = create(&CLSID_DXC_COMPILER, &IID_DXC_COMPILER3, &mut comp);
        if hr != 0 {
            return Err(format!("DxcCreateInstance hr {hr:#x}"));
        }
        let wide: Vec<Vec<u32>> = ["-T", profile, "-E", "main", "-Vd"]
            .iter()
            .map(|s| s.chars().map(|c| c as u32).chain([0]).collect())
            .collect();
        let argv: Vec<*const u32> = wide.iter().map(|w| w.as_ptr()).collect();
        let src = DxcBuffer {
            ptr: hlsl.as_ptr().cast(),
            size: hlsl.len(),
            encoding: 65001,
        };
        let mut res = std::ptr::null_mut();
        let hr = com!(
            comp,
            3,
            fn(
                *const DxcBuffer,
                *const *const u32,
                u32,
                *mut c_void,
                *const Guid,
                *mut *mut c_void,
            ) -> i32,
            &src,
            argv.as_ptr(),
            argv.len() as u32,
            std::ptr::null_mut(),
            &IID_DXC_RESULT,
            &mut res
        );
        if hr != 0 {
            return Err(format!("Compile hr {hr:#x}"));
        }
        let blob = |kind: u32| -> Option<Vec<u8>> {
            let mut b = std::ptr::null_mut();
            let hr = com!(
                res,
                7,
                fn(u32, *const Guid, *mut *mut c_void, *mut *mut c_void) -> i32,
                kind,
                &IID_DXC_BLOB,
                &mut b,
                std::ptr::null_mut()
            );
            if hr != 0 || b.is_null() {
                return None;
            }
            let p = com!(b, 3, fn() -> *const u8,);
            let n = com!(b, 4, fn() -> usize,);
            Some(std::slice::from_raw_parts(p, n).to_vec())
        };
        let mut status = 0i32;
        com!(res, 3, fn(*mut i32) -> i32, &mut status);
        if status < 0 {
            let msg = blob(2)
                .map(|e| String::from_utf8_lossy(&e).into_owned())
                .unwrap_or_default();
            return Err(format!("HLSL compile failed: {msg}"));
        }
        blob(1).ok_or_else(|| "no DXIL object output".into())
    }
}
