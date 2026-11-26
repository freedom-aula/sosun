//! Binding a native image's imports: each address the resolver gives is a
//! host function (System V, or Windows x64 if declared), so the IAT gets a
//! bridge stub for it; a title's own export is called directly.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;

use super::bridge::{StubBlock, is_direct, is_win64, trace_imports, traced_import};
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
    let mut block = StubBlock::new(info.import_count())?;
    let mut slots = Vec::with_capacity(info.import_count());
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
                        let context = if trace_imports() && !win64 {
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
    tracing::info!(
        hle = report.hle,
        host = report.host,
        missing = report.missing.len(),
        "native imports bound"
    );
    Ok(report)
}
