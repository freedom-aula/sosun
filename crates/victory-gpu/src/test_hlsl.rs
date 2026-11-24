//! HLSL test shaders compiled with FXC, for the tests that need one.
//!
//! Wine's HLSL compiler cannot build some of them (unbounded descriptor
//! tables, root signatures that D3DMetal refuses), so under Wine the bytes
//! that Microsoft's compiler made on a Windows runner are used instead
//! (`test_bytecode.rs`, written by `cargo xtask embed-test-shaders`). On
//! Windows, `VICTORY_DUMP_TEST_SHADERS=<folder>` saves every compiled shader
//! there under its key, which is what the test-shaders workflow uploads.

use std::ffi::CString;

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::core::PCSTR;

/// FNV-1a of the source, the profile and the flags: the key of a shader.
pub(crate) fn key(source: &str, profile: &str, flags: u32) -> u64 {
    let mut data = source.as_bytes().to_vec();
    data.push(0);
    data.extend(profile.as_bytes());
    data.push(0);
    data.extend(flags.to_le_bytes());
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

/// Whether this process runs under Wine.
fn wine() -> bool {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    // SAFETY: ntdll is always loaded; both names end with a NUL.
    unsafe { GetProcAddress(GetModuleHandleA(c"ntdll.dll".as_ptr().cast()), c"wine_get_version".as_ptr().cast()) }
        .is_some()
}

/// The bytecode of `source`'s `main` for `profile`, or the compiler's
/// errors.
pub(crate) fn compile(source: &str, name: &str, profile: &str, flags: u32) -> Result<Vec<u8>, String> {
    let key = key(source, profile, flags);
    if wine()
        && let Some((_, bytes)) = crate::test_bytecode::SHADERS.iter().find(|(k, _)| *k == key)
    {
        return Ok(bytes.to_vec());
    }
    let (name, entry, target) = (
        CString::new(name).unwrap(),
        c"main",
        CString::new(profile).unwrap(),
    );
    let (mut code, mut errors): (Option<ID3DBlob>, Option<ID3DBlob>) = (None, None);
    let blob = |b: &ID3DBlob| {
        // SAFETY: the blob's own bytes.
        unsafe { std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize()) }.to_vec()
    };
    // SAFETY: the source and names outlive the call; out pointers are ours.
    let made = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            PCSTR(name.as_ptr().cast()),
            None,
            None,
            PCSTR(entry.as_ptr().cast()),
            PCSTR(target.as_ptr().cast()),
            flags,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if made.is_err() {
        let text = errors.as_ref().map(blob).unwrap_or_default();
        return Err(format!("{made:?}: {}", String::from_utf8_lossy(&text)));
    }
    let bytes = blob(code.as_ref().ok_or("no bytecode")?);
    if let Some(dir) = std::env::var_os("VICTORY_DUMP_TEST_SHADERS") {
        let dir = std::path::PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(format!("{key:016x}.bin")), &bytes);
    }
    Ok(bytes)
}
