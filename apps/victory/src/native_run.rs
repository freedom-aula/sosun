//! `victory run` on macOS (x86-64, the title under Rosetta): no Wine. The
//! image is mapped by `victory_loader::native`, its imports bound against
//! `victory-hle`, and the entry point run on a title thread. Everything the
//! registry does not serve is bound to a logging stub (`--unimplemented`),
//! so loading never stops at a missing export.
//!
//! The DLLs the title ships beside its executable (the C++ runtime, Bink,
//! libHttpClient, ...) are mapped and bound the same way, through the same
//! resolver: our runtime first, then the host, then the other title DLLs.
//! Their `DllMain` runs only with `--execute`, on the title's main thread,
//! before the executable's own TLS callbacks and entry point.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use anyhow::{Context, Result};
use victory_hle::modules::xmem_physical::{Observer, PoolInfo};
use victory_hle::{HleResolver, HostPolicy};
use victory_loader::native::dll::{self, TitleDll, WithTitleDlls};
use victory_loader::native::{self, NativeImage, teb};
use victory_loader::{BindReport, ImportResolver, Provenance, Resolution, UnimplementedPolicy};
use victory_pe::ImportSymbol;

use crate::{Console, Unimplemented};

/// Remembers who served each import, for the listing.
struct Recording<'a> {
    inner: &'a dyn ImportResolver,
    seen: Mutex<Vec<(String, String, Option<Provenance>)>>,
}

impl<'a> Recording<'a> {
    fn new(inner: &'a dyn ImportResolver) -> Self {
        Recording {
            inner,
            seen: Mutex::default(),
        }
    }

    /// What was resolved since the last call.
    fn take(&self) -> Vec<(String, String, Option<Provenance>)> {
        std::mem::take(&mut *self.seen.lock().unwrap_or_else(|e| e.into_inner()))
    }
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

/// DLLs our runtime serves are never taken from the title's folder; any
/// other the title ships is (the host has no C++ runtime to offer).
fn title_dll(dll: &str) -> bool {
    !victory_hle::registry().iter().any(|m| m.serves(dll))
}

/// Binds and finalizes `image`, then prints what was bound (and, with
/// `list`, every import and who served it).
fn bind(
    name: &str,
    image: &NativeImage,
    resolver: &dyn ImportResolver,
    policy: UnimplementedPolicy,
    list: bool,
) -> Result<BindReport> {
    let recording = Recording::new(resolver);
    let report = native::bind_imports(image, &recording, policy)
        .with_context(|| format!("binding {name}"))?;
    image.finalize()?;
    println!(
        "bound          {name}: {} hle, {} host, {} title, {} stubbed",
        report.hle,
        report.host,
        report.title,
        report.missing.len()
    );
    let seen = recording.take();
    if list {
        for (dll, symbol, provenance) in &seen {
            let how = match provenance {
                Some(Provenance::Hle) => "native (hle)",
                Some(Provenance::Host) => "native (host)",
                Some(Provenance::Title) => "native (title)",
                None => "STUB",
            };
            println!("  {how:<14} {dll}!{symbol}");
        }
        // Served and stubbed per DLL imported.
        let mut by_dll: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for (dll, _, provenance) in &seen {
            let entry = by_dll.entry(dll).or_default();
            if provenance.is_some() {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
        }
        println!("imports of     {name}, by dll (served, stub):");
        for (dll, (served, stub)) in &by_dll {
            println!("  {dll}: {served}, {stub}");
        }
    }
    if !report.missing.is_empty() {
        let mut by_dll: BTreeMap<&str, usize> = BTreeMap::new();
        for m in &report.missing {
            *by_dll.entry(&m.dll).or_default() += 1;
        }
        println!(
            "unresolved     {name}: {} imports in {} dlls:",
            report.missing.len(),
            by_dll.len()
        );
        for (dll, n) in &by_dll {
            println!("  {dll}: {n}");
        }
    }
    Ok(report)
}

/// The Windows loader lock's shape: one thread at a time, re-entered by the
/// thread that holds it (a `DllMain` that loads another DLL).
#[derive(Default)]
struct LoaderLock {
    owner: Mutex<(Option<std::thread::ThreadId>, usize)>,
    free: Condvar,
}

impl LoaderLock {
    fn hold<T>(&self, f: impl FnOnce() -> T) -> T {
        let me = std::thread::current().id();
        let lock = || self.owner.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut owner = lock();
            while owner.0.is_some_and(|o| o != me) {
                owner = self.free.wait(owner).unwrap_or_else(|e| e.into_inner());
            }
            *owner = (Some(me), owner.1 + 1);
        }
        let result = f();
        let mut owner = lock();
        owner.1 -= 1;
        if owner.1 == 0 {
            owner.0 = None;
            self.free.notify_one();
        }
        result
    }
}

/// Records where a title DLL came from, for `GetModuleFileName` and the
/// by-name lookups of `LoadLibrary`.
fn note(dll: &TitleDll) {
    println!("title dll      {} at {:#x}", dll.name, dll.image.base());
    victory_hle::runtime::set_image_path(dll.image.base(), &dll.path);
}

/// `LoadLibrary` of a title DLL nothing imported (or that an import did not
/// need until now): maps it and the title DLLs it needs, binds them, runs
/// their `DllMain` on the calling thread, and returns its base; `None` if
/// the title ships no such DLL.
fn load_late(
    file: &str,
    title_dir: &Path,
    policy: UnimplementedPolicy,
    host: &HostPolicy,
    lock: &LoaderLock,
) -> Result<Option<usize>> {
    use victory_hle::runtime::image_named;
    lock.hold(|| {
        if let Some(base) = image_named(file) {
            return Ok(Some(base));
        }
        let new = dll::map_title_dll(file, title_dir, &title_dll)?;
        let hle = HleResolver::new(host.clone());
        let resolver = WithTitleDlls { inner: &hle };
        for d in &new {
            note(d);
            if d.image.tls().is_some() {
                // Threads that already run have no block for it.
                tracing::warn!(dll = d.name, "a DLL with static TLS loaded late");
            }
            bind(&d.name, &d.image, &resolver, policy, false)?;
        }
        for d in &new {
            // SAFETY: bound and finalized above, dependencies first.
            let started = unsafe { native::attach_dll(&d.image, 1, 0) };
            anyhow::ensure!(started, "DllMain of {} returned FALSE", d.name);
            native::register_dll_thread_notices(&d.image);
        }
        Ok(new.last().map(|d| d.image.base()))
    })
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    exe: &Path,
    unimplemented: Unimplemented,
    strict: bool,
    execute: bool,
    list_imports: bool,
    console: Console,
    title_args: &[String],
    headless: bool,
) -> Result<u32> {
    victory_gpu::clock::initialize();
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

    let host = if strict {
        HostPolicy::none()
    } else {
        HostPolicy::default()
    };
    let resolver = HleResolver::new(host.clone());
    let policy = match unimplemented {
        Unimplemented::Trap => UnimplementedPolicy::Trap,
        Unimplemented::Zero => UnimplementedPolicy::ReturnZero,
    };

    // The title's own DLLs: mapped (dependencies first) before anything is
    // bound, as they bind against each other. Their entry points do not run
    // here, so a dry run runs no title code.
    let dlls = dll::map_title_dlls(image.info(), title_dir, &title_dll)?;
    for d in &dlls {
        note(d);
    }
    let chain = WithTitleDlls { inner: &resolver };
    for d in &dlls {
        bind(&d.name, &d.image, &chain, policy, list_imports)?;
    }
    let exe_name = exe.file_name().unwrap_or_default().to_string_lossy();
    let report = bind(&exe_name, &image, &chain, policy, list_imports)?;
    println!(
        "bound          {} hle, {} host, {} title, {} stubbed ({} title dlls)",
        report.hle,
        report.host,
        report.title,
        report.missing.len(),
        dlls.len()
    );
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
    // The title sees its executable on the package drive, then its arguments.
    let exe_name_on_g = exe.file_name().unwrap_or_default().to_string_lossy();
    let argv: Vec<String> = std::iter::once(format!("G:\\{exe_name_on_g}"))
        .chain(title_args.iter().cloned())
        .collect();
    victory_hle::posix::set_command_line(&argv);
    let temp = cfg.data_subdir(&format!("{:08X}/temp", title.id))?;
    victory_hle::posix::mount_at('T', &temp);
    victory_hle::runtime::set_title(victory_hle::runtime::Title { storage, ..title });
    victory_hle::console::set(match console {
        Console::SeriesX => victory_hle::console::Console::SeriesX,
        Console::SeriesS => victory_hle::console::Console::SeriesS,
    });
    println!("console        {}", victory_hle::console::current().name());

    // DLLs the title loads itself (the FidelityFX loader's upscaler, ...).
    let (late_dir, lock): (PathBuf, LoaderLock) = (title_dir.to_owned(), LoaderLock::default());
    victory_hle::runtime::set_title_dll_loader(Box::new(move |file| {
        load_late(file, &late_dir, policy, &host, &lock).map_err(|e| format!("{e:#}"))
    }));

    // The image lives as long as the process: title threads outlive `run`.
    let image: &'static NativeImage = Box::leak(Box::new(image));
    connect_gpu();
    // The title's graphics go to a layer: the window's, or one with no window.
    let layer = victory_gpu::native::create_layer(crate::window::WIDTH, crate::window::HEIGHT)
        .map_err(|e| anyhow::anyhow!("no layer to present into: {e}"))?;
    victory_gpu::native::set_layer(layer.as_ptr())
        .map_err(|e| anyhow::anyhow!("no layer to present into: {e}"))?;
    let layer = layer.as_ptr();
    // Debugging aid: VICTORY_FRAME_DUMP=<seconds>,<file.ppm> saves the last frame presented and
    // logs the frame count and the shader work refused so far, once.
    if let Some((secs, path)) = std::env::var("VICTORY_FRAME_DUMP")
        .ok()
        .and_then(|v| {
            v.split_once(',')
                .map(|(s, p)| (s.parse::<u64>().ok(), p.to_owned()))
        })
        .and_then(|(s, p)| Some((s?, p)))
    {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            tracing::warn!(
                frames = victory_gpu::native::frames_presented(),
                shaders = ?victory_gpu::native::shader_stats(),
                "frame dump"
            );
            if let Some((w, h, bgra)) = victory_gpu::native::last_frame() {
                let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
                out.extend(bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]));
                let _ = std::fs::write(&path, out);
                tracing::warn!(w, h, path, "frame saved");
            }
        });
    }
    let launch = Launch(image, dlls);
    let go = move || {
        let launch = launch;
        // SAFETY: imports are bound and the images finalized; from here on we
        // run title code.
        unsafe { native::run_entry_with(launch.0, &launch.1) }
    };
    let code = if headless {
        go()?
    } else {
        // AppKit owns this (the main) thread; the title runs on its own.
        crate::window::run(layer, go)?.unwrap_or(0)
    };
    println!("title exited with {code}");
    Ok(code)
}

/// What the title launch thread takes along: images that live for the process.
struct Launch(&'static NativeImage, Vec<&'static TitleDll>);
// SAFETY: the images are mapped memory that is never freed; the title runs on whichever thread
// starts it.
unsafe impl Send for Launch {}

/// Wires the title-facing graphics objects to the native host: their method tables become
/// bridged ones (the title is Windows x64), the runtime's physical-memory pools feed the GPU's
/// pool table, and fence events signal the title's event handles.
fn connect_gpu() {
    victory_gpu::object::set_bridge(victory_gpu::object::Bridge {
        is_bridged: native::is_bridged,
        copy_of: native::bridged_copy_of,
        expose: |this, len, area| {
            // SAFETY: forwarded: a live object of ours whose first word is such a table.
            unsafe { native::expose_object(this, len, area) }
        },
        declare_win64: native::declare_win64,
    });
    victory_hle::modules::xmem_physical::set_observer(Some(std::sync::Arc::new(PoolFeed(
        victory_gpu::native::pool_observer(),
    ))));
    victory_gpu::native::set_event_hook(victory_hle::posix::set_event);
}

/// `victory_gpu`'s pool observer behind the runtime's (victory-gpu does not know victory-hle).
struct PoolFeed(std::sync::Arc<dyn victory_gpu::native::memory::PoolObserver>);

fn info(pool: &PoolInfo) -> victory_gpu::native::memory::PoolInfo {
    victory_gpu::native::memory::PoolInfo {
        id: pool.id,
        base: pool.base,
        len: pool.len,
        memory_entry: pool.memory_entry,
    }
}

impl Observer for PoolFeed {
    fn pool_created(&self, pool: &PoolInfo) {
        self.0.pool_created(&info(pool));
    }
    fn pool_released(&self, pool: &PoolInfo) {
        self.0.pool_released(&info(pool));
    }
    fn mapped(&self, at: usize, len: usize, pool: &PoolInfo, offset: usize) {
        self.0.mapped(at, len, &info(pool), offset);
    }
    fn unmapped(&self, at: usize, len: usize) {
        self.0.unmapped(at, len);
    }
}
