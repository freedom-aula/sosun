//! Binding a native image's imports: each address the resolver gives is a
//! host function (System V, or Windows x64 if declared), so the IAT gets a
//! bridge stub for it; a title's own export is called directly.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;

use super::bridge::{StubBlock, is_direct, is_win64, callbacks_enabled, profile_seconds, trace_imports, traced_import};
use super::dump::enabled as dump_enabled;
use super::image::NativeImage;
use crate::{
    BindReport, ImportResolver, MissingImport, Provenance, Resolution, UnimplementedPolicy,
};

/// Identifies one unresolved import. Leaked: lives as long as the process.
struct ImportSite {
    dll: String,
    symbol: String,
    policy: UnimplementedPolicy,
    calls: AtomicU64,
}

/// What an unresolved import's stub calls, on the host side; the stub's
/// context word is its first argument.
extern "C" fn unimplemented_import(site: &'static ImportSite) -> u64 {
    let previous = site.calls.fetch_add(1, Ordering::Relaxed);
    match site.policy {
        UnimplementedPolicy::ReturnZero => {
            if previous == 0 {
                tracing::warn!(dll = %site.dll, symbol = %site.symbol, "unimplemented import called, returning 0");
            }
            0
        }
        UnimplementedPolicy::Trap => {
            tracing::error!(dll = %site.dll, symbol = %site.symbol, "unimplemented import called");
            eprintln!(
                "VIctory: unimplemented import {}!{} called",
                site.dll, site.symbol
            );
            std::process::abort();
        }
    }
}

/// Fills every IAT slot of `image` (delay-load slots too). What `resolver`
/// cannot resolve gets a stub that acts on `policy` when called.
pub fn bind_imports(
    image: &NativeImage,
    resolver: &dyn ImportResolver,
    policy: UnimplementedPolicy,
) -> Result<BindReport> {
    let info = image.info();
    let mut report = BindReport::default();
    let total: usize = info
        .imports
        .iter()
        .chain(&info.delay_imports)
        .map(|m| m.entries.len())
        .sum();
    let mut block = StubBlock::new(total)?;
    let mut slots = Vec::with_capacity(total);
    for module in info.imports.iter().chain(&info.delay_imports) {
        for entry in &module.entries {
            let address = match resolver.resolve(&module.dll, &entry.symbol) {
                Resolution::Found {
                    address,
                    provenance: Provenance::Title,
                } => {
                    report.title += 1;
                    address
                }
                Resolution::Found {
                    address,
                    provenance,
                } => {
                    match provenance {
                        Provenance::Hle => report.hle += 1,
                        _ => report.host += 1,
                    }
                    if is_direct(address) {
                        address
                    } else {
                        let win64 = is_win64(address);
                        // win64 functions skip the remapping, and the trace.
                        let context = if (trace_imports() || dump_enabled() || profile_seconds().is_some() || callbacks_enabled()) && !win64 {
                            traced_import(&module.dll, &entry.symbol.to_string())
                        } else {
                            0
                        };
                        block.emit(address, context, win64)?
                    }
                }
                Resolution::Missing => {
                    let site: &'static ImportSite = Box::leak(Box::new(ImportSite {
                        dll: module.dll.clone(),
                        symbol: entry.symbol.to_string(),
                        policy,
                        calls: AtomicU64::new(0),
                    }));
                    report.missing.push(MissingImport {
                        dll: site.dll.clone(),
                        symbol: site.symbol.clone(),
                    });
                    let handler = unimplemented_import as extern "C" fn(&'static ImportSite) -> u64;
                    block.emit(
                        handler as *const () as usize,
                        std::ptr::from_ref(site) as usize,
                        false,
                    )?
                }
            };
            slots.push((entry.iat_slot_rva, address));
        }
    }
    block.seal()?;
    image.write_slots(&slots)?;
    if report.missing.is_empty() {
        tracing::info!(
            hle = report.hle,
            host = report.host,
            missing = 0,
            "native imports bound"
        );
    } else {
        // Named here, once per image: a late-loaded DLL's missing imports are logged nowhere else.
        tracing::warn!(
            hle = report.hle,
            host = report.host,
            missing = report.missing.len(),
            unresolved = %names(&report.missing),
            "native imports bound"
        );
    }
    Ok(report)
}

/// `dll!symbol` for each import, an ordinal as `dll!#n`, comma separated.
fn names(missing: &[MissingImport]) -> String {
    let names: Vec<String> = missing
        .iter()
        .map(|m| format!("{}!{}", m.dll, m.symbol))
        .collect();
    names.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_imports_are_named_with_their_dll() {
        let missing = |dll: &str, symbol: &str| MissingImport {
            dll: dll.into(),
            symbol: symbol.into(),
        };
        let missing = [
            missing("api-ms-win-core-debug-l1-1-0.dll", "OutputDebugStringW"),
            missing(
                "api-ms-win-crt-math-l1-1-0.dll",
                &victory_pe::ImportSymbol::Ordinal(42).to_string(),
            ),
        ];
        assert_eq!(
            names(&missing),
            "api-ms-win-core-debug-l1-1-0.dll!OutputDebugStringW, \
             api-ms-win-crt-math-l1-1-0.dll!#42"
        );
        assert_eq!(names(&[]), "");
    }

    #[test]
    fn binds_delay_imports_past_a_full_stub_page() {
        // 128 regular imports fill a 4 KiB Rosetta stub page exactly (128 * 32 B);
        // delay-loaded imports must still fit in the same StubBlock.
        let mut pe = vec![0u8; 0x3200];
        let put16 = |d: &mut [u8], at: usize, v: u16| d[at..at + 2].copy_from_slice(&v.to_le_bytes());
        let put32 = |d: &mut [u8], at: usize, v: u32| d[at..at + 4].copy_from_slice(&v.to_le_bytes());
        let put64 = |d: &mut [u8], at: usize, v: u64| d[at..at + 8].copy_from_slice(&v.to_le_bytes());
        pe[0..2].copy_from_slice(b"MZ");
        put32(&mut pe, 0x3C, 0x40);
        pe[0x40..0x44].copy_from_slice(b"PE\0\0");
        put16(&mut pe, 0x44, victory_pe::MACHINE_AMD64);
        put16(&mut pe, 0x46, 1); // sections
        put16(&mut pe, 0x54, 240); // optional header size
        put16(&mut pe, 0x56, 0x2022); // DLL, large address aware
        let opt = 0x58;
        put16(&mut pe, opt, 0x20B);
        put64(&mut pe, opt + 24, 0x1_8000_0000);
        put32(&mut pe, opt + 32, 0x1000);
        put32(&mut pe, opt + 36, 0x200);
        put32(&mut pe, opt + 56, 0x4000); // SizeOfImage
        put32(&mut pe, opt + 60, 0x200); // SizeOfHeaders
        put32(&mut pe, opt + 108, 16); // NumberOfRvaAndSizes
        // Data directory 1 (IMPORT): RVA 0x1000, size 40
        put32(&mut pe, opt + 112 + 8, 0x1000);
        put32(&mut pe, opt + 112 + 12, 40);
        // Data directory 5 (BASERELOC): RVA 0x2800, size 8 (empty block so map can relocate)
        put32(&mut pe, opt + 112 + 5 * 8, 0x2800);
        put32(&mut pe, opt + 112 + 5 * 8 + 4, 8);
        // Data directory 13 (DELAY_IMPORT): RVA 0x2000, size 64
        put32(&mut pe, opt + 112 + 13 * 8, 0x2000);
        put32(&mut pe, opt + 112 + 13 * 8 + 4, 64);

        let sec = opt + 240;
        pe[sec..sec + 6].copy_from_slice(b".rdata");
        put32(&mut pe, sec + 8, 0x3000); // VirtualSize
        put32(&mut pe, sec + 12, 0x1000); // VirtualAddress
        put32(&mut pe, sec + 16, 0x3000); // SizeOfRawData
        put32(&mut pe, sec + 20, 0x200); // PointerToRawData
        put32(&mut pe, sec + 36, 0x4000_0000); // READ

        let at = |rva: usize| 0x200 + (rva - 0x1000);
        // Regular import descriptor: 128 ordinal entries.
        put32(&mut pe, at(0x1000), 0x1100); // OriginalFirstThunk
        put32(&mut pe, at(0x100C), 0x1040); // Name
        put32(&mut pe, at(0x1010), 0x1600); // FirstThunk (IAT)
        pe[at(0x1040)..at(0x1046)].copy_from_slice(b"a.dll\0");
        for i in 0..128usize {
            put64(&mut pe, at(0x1100 + i * 8), (1u64 << 63) | (i as u64 + 1));
        }
        // Delay import descriptor: 2 ordinal entries.
        put32(&mut pe, at(0x2000), 1); // RVA-based
        put32(&mut pe, at(0x2004), 0x2040); // DllNameRva
        put32(&mut pe, at(0x2008), 0x2050); // ModuleHandleRva
        put32(&mut pe, at(0x200C), 0x2200); // ImportAddressTableRva
        put32(&mut pe, at(0x2010), 0x2100); // ImportNameTableRva
        pe[at(0x2040)..at(0x2046)].copy_from_slice(b"b.dll\0");
        for i in 0..2usize {
            put64(&mut pe, at(0x2100 + i * 8), (1u64 << 63) | (i as u64 + 1));
        }
        // Minimal valid empty relocation block at 0x2800.
        put32(&mut pe, at(0x2800), 0x1000);
        put32(&mut pe, at(0x2804), 8);

        struct NoneResolver;
        impl ImportResolver for NoneResolver {
            fn resolve(&self, _: &str, _: &victory_pe::ImportSymbol) -> Resolution {
                Resolution::Missing
            }
        }

        let image = NativeImage::map(&pe, None).unwrap();
        let report = bind_imports(&image, &NoneResolver, UnimplementedPolicy::ReturnZero).unwrap();
        assert_eq!(report.missing.len(), 130);
        // Verify both delay-import IAT slots were filled with non-zero stub addresses.
        for rva in [0x2200usize, 0x2208] {
            // SAFETY: inside the mapped image.
            let slot = unsafe { ((image.base() + rva) as *const u64).read_unaligned() };
            assert_ne!(slot, 0, "delay-import slot {rva:#x} was not bound");
        }
        // SAFETY: no title code ran.
        unsafe { image.unmap() };
    }
}
