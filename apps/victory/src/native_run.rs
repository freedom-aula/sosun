//! `victory run` on macOS (x86-64, the title under Rosetta): no Wine. The
//! image is mapped by `victory_loader::native`, its imports bound against
//! `victory-hle`, and the entry point run on a title thread. Everything the
//! registry does not serve is bound to a logging stub (`--unimplemented`),
//! so loading never stops at a missing export.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use victory_hle::{HleResolver, HostPolicy};
use victory_loader::native::{self, NativeImage, teb};
use victory_loader::{ImportResolver, Provenance, Resolution, UnimplementedPolicy};
use victory_pe::ImportSymbol;

use crate::{Console, Unimplemented};

/// Remembers who served each import, for the listing.
struct Recording<'a> {
    inner: &'a HleResolver,
    seen: Mutex<Vec<(String, String, Option<Provenance>)>>,
}

impl ImportResolver for Recording<'_> {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let resolution = self.inner.resolve(dll, symbol);
        let provenance = match &resolution {
            Resolution::Found { provenance, .. } => Some(*provenance),
            Resolution::Missing => None,
        };
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).push((
            dll.to_owned(),
            symbol.to_string(),
            provenance,
        ));
        resolution
    }
}

pub fn run(
    exe: &Path,
    unimplemented: Unimplemented,
    strict: bool,
    execute: bool,
    list_imports: bool,
    console: Console,
) -> Result<u32> {
    let title_dir = exe.parent().unwrap_or(Path::new("."));
    let bytes = std::fs::read(exe).with_context(|| format!("reading {}", exe.display()))?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    match victory_game::identify(&hash) {
        Some(build) => println!("build          {} ({hash})", build.label),
        None => println!("build          unknown ({hash})"),
    }
    let image = NativeImage::map(&bytes, None)?;
    drop(bytes);
    println!(
        "mapped at      {:#x} (preferred {:#x})",
        image.base(),
        image.info().image_base
    );
    if let Some(tls) = image.tls() {
        teb::register_tls(tls);
    }
    victory_hle::runtime::set_main_image(image.base());
    victory_hle::runtime::set_image_path(image.base(), exe);

    let resolver = HleResolver::new(if strict {
        HostPolicy::none()
    } else {
        HostPolicy::default()
    });
    let recording = Recording {
        inner: &resolver,
        seen: Mutex::default(),
    };
    let policy = match unimplemented {
        Unimplemented::Trap => UnimplementedPolicy::Trap,
        Unimplemented::Zero => UnimplementedPolicy::ReturnZero,
    };
    let report = native::bind_imports(&image, &recording, policy)?;
    image.finalize()?;
    println!(
        "bound          {} hle, {} host, {} title, {} stubbed",
        report.hle,
        report.host,
        report.title,
        report.missing.len()
    );
    let seen = recording
        .seen
        .into_inner()
        .unwrap_or_else(|e| e.into_inner());
    if list_imports {
        for (dll, symbol, provenance) in &seen {
            let how = match provenance {
                Some(Provenance::Hle) => "native (hle)",
                Some(Provenance::Host) => "native (host)",
                Some(Provenance::Title) => "native (title)",
                None => "STUB",
            };
            println!("  {how:<14} {dll}!{symbol}");
        }
    }
    if !report.missing.is_empty() {
        let mut by_dll: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for m in &report.missing {
            by_dll.entry(&m.dll).or_default().push(&m.symbol);
        }
        println!(
            "unresolved     {} imports in {} dlls:",
            report.missing.len(),
            by_dll.len()
        );
        for (dll, symbols) in &by_dll {
            println!("  {dll}: {}", symbols.len());
        }
    }
    if !execute {
        println!("not executing (pass --execute to start the title)");
        return Ok(0);
    }

    // The manifest names the title and its storage quota; the console's
    // fixed drives are the package as `G:` and temporary files `T:`.
    let cfg = victory_core::Config::load()?;
    let manifest =
        std::fs::read_to_string(title_dir.join("MicrosoftGame.config")).unwrap_or_default();
    let title = victory_hle::runtime::Title::from_manifest(&manifest);
    let storage = cfg.data_subdir(&format!("{:08X}/storage", title.id))?;
    println!(
        "title          {:08X}, storage {}",
        title.id,
        storage.display()
    );
    victory_hle::posix::mount_at('G', title_dir);
    let temp = cfg.data_subdir(&format!("{:08X}/temp", title.id))?;
    victory_hle::posix::mount_at('T', &temp);
    victory_hle::runtime::set_title(victory_hle::runtime::Title { storage, ..title });
    victory_hle::console::set(match console {
        Console::SeriesX => victory_hle::console::Console::SeriesX,
        Console::SeriesS => victory_hle::console::Console::SeriesS,
    });
    println!("console        {}", victory_hle::console::current().name());

    // The image lives as long as the process: title threads outlive `run`.
    let image: &'static NativeImage = Box::leak(Box::new(image));
    // SAFETY: imports are bound and the image finalized; from here on we run
    // title code.
    let code = unsafe { native::run_entry(image) }?;
    println!("title exited with {code}");
    Ok(code)
}
