//! Metal Shader Converter (`libmetalirconverter`) loaded at run time: DXIL container -> metallib.
//!
//! The library is Apple's and not redistributable here, so it is dlopen'ed from a path the user
//! supplies (`VICTORY_MSC_DYLIB`). The C API is declared by hand from `metal_irconverter.h`.

use std::ffi::{CStr, CString, c_char, c_void};

type Create = unsafe extern "C" fn() -> *mut c_void;
type Destroy = unsafe extern "C" fn(*mut c_void);
type FromDxil = unsafe extern "C" fn(*const u8, usize, i32) -> *mut c_void;
type Compile =
    unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void, *mut *mut c_void) -> *mut c_void;
type Stage = unsafe extern "C" fn(*const c_void) -> i32;
type GetLib = unsafe extern "C" fn(*const c_void, i32, *mut c_void) -> bool;
type LibSize = unsafe extern "C" fn(*const c_void) -> usize;
type LibBytes = unsafe extern "C" fn(*const c_void, *mut u8) -> usize;
type ErrCode = unsafe extern "C" fn(*mut c_void) -> u32;
type SetEntry = unsafe extern "C" fn(*mut c_void, *const c_char);
type Payload = unsafe extern "C" fn(*mut c_void) -> *const c_char;

pub struct Msc {
    h: *mut c_void,
}

// SAFETY: the handle is a dlopen handle; every conversion makes and destroys its own IR objects.
unsafe impl Send for Msc {}
// SAFETY: as above, no state is shared between calls.
unsafe impl Sync for Msc {}

/// A converter failure: the `IRError` code and the converter's own message.
#[derive(Debug)]
pub struct ConvError {
    pub code: u32,
    pub msg: String,
}

impl std::fmt::Display for ConvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IRError code {}: {}", self.code, self.msg)
    }
}

fn sym<T: Copy>(h: *mut c_void, name: &str) -> T {
    let n = CString::new(name).unwrap();
    // SAFETY: h is a live dlopen handle and T is the function-pointer type of the named C symbol.
    let p = unsafe { libc::dlsym(h, n.as_ptr()) };
    assert!(!p.is_null(), "missing symbol {name}");
    // SAFETY: T is a pointer-sized fn pointer type.
    unsafe { std::mem::transmute_copy(&p) }
}

/// Where Apple's installer puts the converter (universal binary); `VICTORY_MSC_DYLIB` overrides.
pub const DEFAULT_DYLIB: &str = "/usr/local/lib/libmetalirconverter.dylib";

impl Msc {
    /// Opens `VICTORY_MSC_DYLIB` if set, else the installed default.
    pub fn open_default() -> Msc {
        let path = std::env::var("VICTORY_MSC_DYLIB").unwrap_or_else(|_| DEFAULT_DYLIB.into());
        Msc::open(&path).unwrap_or_else(|e| panic!("dlopen {path}: {e}"))
    }

    pub fn open(path: &str) -> Result<Msc, String> {
        let c = CString::new(path).unwrap();
        // SAFETY: dlopen of a user-named library.
        let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW) };
        if h.is_null() {
            // SAFETY: dlerror returns a C string or null.
            let e = unsafe { libc::dlerror() };
            let e = if e.is_null() {
                "?".into()
            } else {
                // SAFETY: non-null NUL-terminated string from dlerror.
                unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
            };
            return Err(e);
        }
        Ok(Msc { h })
    }

    /// DXIL (a DXBC-wrapped container) -> (shader stage, metallib bytes). Stage 8 is compute (`IRShaderStage`).
    pub fn convert(&self, dxil: &[u8], entry: Option<&str>) -> Result<(i32, Vec<u8>), ConvError> {
        let h = self.h;
        // SAFETY: the symbols are the converter's documented C API; every object is destroyed below.
        unsafe {
            let comp = sym::<Create>(h, "IRCompilerCreate")();
            if let Some(e) = entry {
                let e = CString::new(e).unwrap();
                sym::<SetEntry>(h, "IRCompilerSetEntryPointName")(comp, e.as_ptr());
            }
            // IRBytecodeOwnershipNone = 0: the converter borrows `dxil` for the call.
            let input = sym::<FromDxil>(h, "IRObjectCreateFromDXIL")(dxil.as_ptr(), dxil.len(), 0);
            let mut err: *mut c_void = std::ptr::null_mut();
            let out = sym::<Compile>(h, "IRCompilerAllocCompileAndLink")(
                comp,
                std::ptr::null(),
                input,
                &mut err,
            );
            let res = if out.is_null() {
                let (code, msg) = if err.is_null() {
                    (0, String::new())
                } else {
                    // The payload is the message text (the library formats "<Name> : <detail>").
                    let p = sym::<Payload>(h, "IRErrorGetPayload")(err);
                    let msg = if p.is_null() {
                        String::new()
                    } else {
                        CStr::from_ptr(p).to_string_lossy().into_owned()
                    };
                    (sym::<ErrCode>(h, "IRErrorGetCode")(err), msg)
                };
                Err(ConvError { code, msg })
            } else {
                let stage = sym::<Stage>(h, "IRObjectGetMetalIRShaderStage")(out);
                let lib = sym::<Create>(h, "IRMetalLibBinaryCreate")();
                let r = if sym::<GetLib>(h, "IRObjectGetMetalLibBinary")(out, stage, lib) {
                    let n = sym::<LibSize>(h, "IRMetalLibGetBytecodeSize")(lib);
                    let mut v = vec![0u8; n];
                    sym::<LibBytes>(h, "IRMetalLibGetBytecode")(lib, v.as_mut_ptr());
                    Ok((stage, v))
                } else {
                    Err(ConvError {
                        code: u32::MAX,
                        msg: "no metallib for stage".into(),
                    })
                };
                sym::<Destroy>(h, "IRMetalLibBinaryDestroy")(lib);
                sym::<Destroy>(h, "IRObjectDestroy")(out);
                r
            };
            if !err.is_null() {
                sym::<Destroy>(h, "IRErrorDestroy")(err);
            }
            sym::<Destroy>(h, "IRObjectDestroy")(input);
            sym::<Destroy>(h, "IRCompilerDestroy")(comp);
            res
        }
    }
}
