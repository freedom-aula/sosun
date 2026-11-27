//! The DLLs a title ships beside its executable, on the native path: each is
//! mapped, relocated and given static TLS like the main image, bound
//! through the same resolver (our runtime, the host, then the other title
//! DLLs), and started with `DllMain` by [`super::run`].
//!
//! Mapping and lookup are here and arch-neutral; nothing here runs title
//! code. Every DLL stays mapped for the life of the process (the registry
//! below holds it), as `FreeLibrary` on a title DLL does nothing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use victory_pe::{ExportInfo, ImageInfo, ImportSymbol};

use super::image::NativeImage;
use crate::{ImportResolver, Provenance, Resolution};

/// A DLL of the title, mapped.
#[derive(Debug)]
pub struct TitleDll {
    /// The file name, lower case: what imports are matched against.
    pub name: String,
    /// The file as it is in the title's folder.
    pub path: PathBuf,
    pub image: NativeImage,
}

static LOADED: Mutex<Vec<&'static TitleDll>> = Mutex::new(Vec::new());

fn registry() -> std::sync::MutexGuard<'static, Vec<&'static TitleDll>> {
    LOADED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Every title DLL mapped so far, in the order they were mapped
/// (dependencies first).
pub fn loaded() -> Vec<&'static TitleDll> {
    registry().clone()
}

/// The mapped title DLL called `name` (any case).
pub fn find(name: &str) -> Option<&'static TitleDll> {
    registry()
        .iter()
        .find(|d| d.name.eq_ignore_ascii_case(name))
        .copied()
}

/// The mapped title DLL at `base`.
pub fn find_base(base: usize) -> Option<&'static TitleDll> {
    registry().iter().find(|d| d.image.base() == base).copied()
}

/// Maps the DLLs the title ships in `dir` that `main` imports, directly or
/// through each other, in initialisation order (dependencies first). Only
/// names `wanted` accepts are considered, so DLLs our runtime provides stay
/// with it. Nothing is bound or started; `finalize` each once bound.
pub fn map_title_dlls(
    main: &ImageInfo,
    dir: &Path,
    wanted: &dyn Fn(&str) -> bool,
) -> Result<Vec<&'static TitleDll>> {
    let mut mapped = Vec::new();
    map_imports(main, dir, wanted, &mut HashSet::new(), &mut mapped)?;
    Ok(mapped)
}

/// [`map_title_dlls`] for one DLL a title loads itself: it and the title DLLs
/// it needs that are not mapped yet (itself last); empty if `wanted` refuses
/// it or the folder has no such file.
pub fn map_title_dll(
    name: &str,
    dir: &Path,
    wanted: &dyn Fn(&str) -> bool,
) -> Result<Vec<&'static TitleDll>> {
    let mut mapped = Vec::new();
    map_dll(name, dir, wanted, &mut HashSet::new(), &mut mapped)?;
    Ok(mapped)
}

fn map_imports(
    info: &ImageInfo,
    dir: &Path,
    wanted: &dyn Fn(&str) -> bool,
    seen: &mut HashSet<String>,
    mapped: &mut Vec<&'static TitleDll>,
) -> Result<()> {
    for module in info.imports.iter().chain(&info.delay_imports) {
        map_dll(&module.dll, dir, wanted, seen, mapped)?;
    }
    Ok(())
}

/// The file called `name` in `dir`, whatever its case (volumes can be case
/// sensitive; imports and the title's manifest name are not).
pub fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let direct = dir.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name))
        .map(|e| e.path())
        .filter(|p| p.is_file())
}

fn map_dll(
    name: &str,
    dir: &Path,
    wanted: &dyn Fn(&str) -> bool,
    seen: &mut HashSet<String>,
    mapped: &mut Vec<&'static TitleDll>,
) -> Result<()> {
    let name = name.to_ascii_lowercase();
    if !seen.insert(name.clone()) || find(&name).is_some() || !wanted(&name) {
        return Ok(());
    }
    let Some(path) = find_file(dir, &name) else {
        return Ok(());
    };
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let image = NativeImage::map(&bytes, None).with_context(|| format!("mapping {name}"))?;
    drop(bytes);
    // Its own imports first, so it is initialised after them.
    if let Err(err) = map_imports(image.info(), dir, wanted, seen, mapped) {
        // SAFETY: not registered in TLS or exposed to title code yet.
        unsafe { image.unmap() };
        return Err(err);
    }
    // Before any thread runs title code, and while its index word is writable.
    #[cfg(target_arch = "x86_64")]
    if let Some(tls) = image.tls() {
        super::teb::register_tls(tls);
    }
    let dll: &'static TitleDll = Box::leak(Box::new(TitleDll { name, path, image }));
    registry().push(dll);
    mapped.push(dll);
    Ok(())
}

/// Where an export is: in the image, or forwarded to another DLL.
enum Export {
    Address(usize),
    Forward(String),
}

impl TitleDll {
    fn export(&self, symbol: &ImportSymbol) -> Option<Export> {
        let exports = &self.image.info().exports;
        let found: &ExportInfo = match symbol {
            ImportSymbol::Name { name, .. } => exports
                .iter()
                .find(|e| e.name.as_deref() == Some(name.as_str()))?,
            ImportSymbol::Ordinal(o) => exports.iter().find(|e| e.ordinal == *o)?,
        };
        match (found.rva, &found.forward) {
            (Some(rva), _) => Some(Export::Address(self.image.base() + rva as usize)),
            (None, Some(forward)) => Some(Export::Forward(forward.clone())),
            (None, None) => None,
        }
    }
}

/// `"dll.name"` or `"dll.#7"`, as an export forwards to it.
fn forward_target(forward: &str) -> Option<(String, ImportSymbol)> {
    let (dll, name) = forward.rsplit_once('.')?;
    if dll.is_empty() || name.is_empty() {
        return None;
    }
    let symbol = match name.strip_prefix('#') {
        Some(ordinal) => ImportSymbol::Ordinal(ordinal.parse().ok()?),
        None => ImportSymbol::Name {
            hint: 0,
            name: name.to_owned(),
        },
    };
    Some((format!("{dll}.dll"), symbol))
}

/// Address of an export of the title DLL at `base` (what `GetProcAddress`
/// returns); forwards are followed among the title's DLLs.
pub fn proc_address(base: usize, symbol: &ImportSymbol) -> Option<usize> {
    let mut dll = find_base(base)?;
    let mut symbol = symbol.clone();
    for _ in 0..8 {
        match dll.export(&symbol)? {
            Export::Address(address) => return Some(address),
            Export::Forward(forward) => {
                let (next, target) = forward_target(&forward)?;
                dll = find(&next)?;
                symbol = target;
            }
        }
    }
    None
}

/// Resolves through `inner`, then through the exports of the title's DLLs.
pub struct WithTitleDlls<'a> {
    pub inner: &'a dyn ImportResolver,
}

impl WithTitleDlls<'_> {
    fn lookup_in_dlls(&self, dll: &str, symbol: &ImportSymbol, depth: u32) -> Resolution {
        let Some(found) = find(dll).filter(|_| depth < 8) else {
            return Resolution::Missing;
        };
        match found.export(symbol) {
            Some(Export::Address(address)) => Resolution::Found {
                address,
                provenance: Provenance::Title,
            },
            Some(Export::Forward(forward)) => {
                let Some((next, target)) = forward_target(&forward) else {
                    return Resolution::Missing;
                };
                match self.inner.resolve(&next, &target) {
                    Resolution::Missing => self.lookup_in_dlls(&next, &target, depth + 1),
                    found => found,
                }
            }
            None => Resolution::Missing,
        }
    }
}

impl ImportResolver for WithTitleDlls<'_> {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        match self.inner.resolve(dll, symbol) {
            Resolution::Missing => self.lookup_in_dlls(dll, symbol, 0),
            found => found,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_found_whatever_its_case_and_only_if_it_is_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MSVCP140.DLL"), b"").unwrap();
        std::fs::write(dir.path().join("microsoftgame.config"), b"").unwrap();
        std::fs::create_dir(dir.path().join("bink2scarlett.dll")).unwrap();

        let found = find_file(dir.path(), "msvcp140.dll").unwrap();
        assert!(found.is_file());
        let config = find_file(dir.path(), "MicrosoftGame.config").unwrap();
        assert!(config.is_file());
        assert_eq!(find_file(dir.path(), "vcruntime140.dll"), None);
        // A folder of that name is not the DLL.
        assert_eq!(find_file(dir.path(), "bink2scarlett.dll"), None);
    }

    #[test]
    fn forward_target_rejects_empty_dll_or_symbol() {
        assert_eq!(forward_target(".Func"), None);
        assert_eq!(forward_target("kernel32."), None);
        assert_eq!(forward_target("kernel32.#"), None);
        assert_eq!(forward_target("plain"), None);
        assert_eq!(
            forward_target("api-ms-win-crt-runtime-l1-1-0.exit"),
            Some((
                "api-ms-win-crt-runtime-l1-1-0.dll".into(),
                ImportSymbol::Name {
                    hint: 0,
                    name: "exit".into(),
                }
            ))
        );
        assert_eq!(
            forward_target("ws2_32.#7"),
            Some(("ws2_32.dll".into(), ImportSymbol::Ordinal(7)))
        );
    }
}
