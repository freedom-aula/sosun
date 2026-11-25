//! Shader corpus run: every translated shader in a victory shader cache goes through the Metal
//! Shader Converter and, where it converts, into a Metal pipeline state. Prints counts only;
//! nothing from the cache (and no converted output) is written anywhere but the system temp dir.
//!
//! `victory-metal --corpus <cache/shaders> [--limit N] [--threads N] [--fails FILE]`
//! The cache layout is `<build>/<xx>/<hash>`, each file `VSH1 stage:u32 len:u32 dxil...` or `VSHE reason`
//! (see `victory-gpu/src/shader_cache.rs`). Duplicates across builds are counted once.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSString, NSURL};
use objc2_metal::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use victory_metal::msc::Msc;

type Dev = Retained<ProtocolObject<dyn MTLDevice>>;
const STAGES: [&str; 8] = ["Ls", "Hs", "Es", "Gs", "Vs", "Ps", "Ms", "Cs"];

#[derive(Default)]
struct Row {
    unique: usize,
    dxil_bytes: usize,
    converted: usize,
    metallib_bytes: usize,
    convert_secs: f64,
    /// Converter failures by "code: message".
    convert_failed: BTreeMap<String, usize>,
    load_failed: BTreeMap<String, usize>,
    pso_ok: usize,
    pso_secs: f64,
    pso_failed: BTreeMap<String, usize>,
}

#[derive(Default)]
struct Tally {
    files: usize,
    refused: BTreeMap<String, usize>,
    unreadable: usize,
    duplicates: usize,
    rows: BTreeMap<&'static str, Row>,
    /// Failed shaders: "<stage> <hash> <why>", for `--fails`.
    fails: Vec<String>,
}

struct Shared(Dev);
// SAFETY: MTLDevice is documented thread-safe.
unsafe impl Sync for Shared {}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if !e.file_name().to_string_lossy().starts_with('.') {
            out.push(p);
        }
    }
}

/// The first line of an `NSError`'s description, trimmed.
fn first_line(e: &objc2_foundation::NSError) -> String {
    let d = e.localizedDescription().to_string();
    d.lines().next().unwrap_or("").chars().take(160).collect()
}

/// `IRErrorCode` names, in the order of `metal_irconverter.h` (Apple's installer, version 3.x/4.x).
fn code_name(code: u32) -> &'static str {
    const NAMES: [&str; 20] = [
        "NoError",
        "ShaderRequiresRootSignature",
        "UnrecognizedRootSignatureDescriptor",
        "UnrecognizedParameterTypeInRootSignature",
        "ResourceNotReferencedByRootSignature",
        "ShaderIncompatibleWithDualSourceBlending",
        "UnsupportedWaveSize",
        "UnsupportedInstruction",
        "CompilationError",
        "FailedToSynthesizeStageInFunction",
        "FailedToSynthesizeStreamOutFunction",
        "FailedToSynthesizeIndirectIntersectionFunction",
        "UnableToVerifyModule",
        "UnableToLinkModule",
        "UnrecognizedDXILHeader",
        "InvalidRaytracingAttribute",
        "NullHullShaderInputOutputMismatch",
        "InvalidRaytracingUserAttributeSize",
        "IncorrectHitgroupType",
        "FP64Usage",
    ];
    NAMES.get(code as usize).copied().unwrap_or("Unknown")
}

/// Strip what varies per shader (hex addresses, numbers) so reasons group.
fn normalise(s: &str) -> String {
    let mut out = String::new();
    let mut prev_digit = false;
    for c in s.chars().take(160) {
        let d = c.is_ascii_digit();
        if d && !prev_digit {
            out.push('N');
        } else if !d {
            out.push(c);
        }
        prev_digit = d;
    }
    out
}

/// A vertex function that writes the named user varyings, so a fragment function alone can get a
/// render pipeline (Metal checks fragment inputs against vertex outputs). The names come from
/// the pipeline errors of earlier attempts.
fn dummy_vertex(dev: &Dev, names: &[(String, usize)]) -> Retained<ProtocolObject<dyn MTLFunction>> {
    let fields: String = (names.iter().enumerate())
        .map(|(i, (n, t))| {
            let ty = TYPES[*t % TYPES.len()];
            let flat = if ty.starts_with(['u', 'i']) {
                ", flat"
            } else {
                ""
            };
            format!("{ty} a{i} [[user({n}){flat}]];")
        })
        .collect();
    let writes: String = (0..names.len())
        .map(|i| format!("o.a{i} = {{}};"))
        .collect();
    let src = format!(
        "#include <metal_stdlib>\nusing namespace metal;\nstruct O {{ float4 p [[position]]; {fields} }};\n\
         vertex O dummy_vs(uint v [[vertex_id]]) {{ O o; o.p = float4(0); {writes} return o; }}"
    );
    let lib = dev
        .newLibraryWithSource_options_error(&NSString::from_str(&src), None)
        .expect("dummy vertex source");
    lib.newFunctionWithName(&NSString::from_str("dummy_vs"))
        .unwrap()
}

/// Types tried for a varying whose type is not known (the error names only the mismatches).
const TYPES: [&str; 12] = [
    "float4", "float2", "float3", "float", "half4", "half2", "half", "uint4", "uint", "int4",
    "uint2", "uint3",
];

/// The varying names in a "Fragment input(s) `user(a),user(b)` mismatching ..." error.
fn missing_varyings(why: &str) -> Vec<String> {
    let Some(list) = why.split('`').nth(1) else {
        return vec![];
    };
    (list.split(','))
        .filter_map(|u| Some(u.strip_prefix("user(")?.strip_suffix(')')?.to_string()))
        .collect()
}

fn vertex_format(t: MTLDataType) -> MTLVertexFormat {
    match t {
        MTLDataType::Float => MTLVertexFormat::Float,
        MTLDataType::Float2 => MTLVertexFormat::Float2,
        MTLDataType::Float3 => MTLVertexFormat::Float3,
        MTLDataType::Int => MTLVertexFormat::Int,
        MTLDataType::Int2 => MTLVertexFormat::Int2,
        MTLDataType::Int3 => MTLVertexFormat::Int3,
        MTLDataType::Int4 => MTLVertexFormat::Int4,
        MTLDataType::UInt => MTLVertexFormat::UInt,
        MTLDataType::UInt2 => MTLVertexFormat::UInt2,
        MTLDataType::UInt3 => MTLVertexFormat::UInt3,
        MTLDataType::UInt4 => MTLVertexFormat::UInt4,
        MTLDataType::Half2 => MTLVertexFormat::Half2,
        MTLDataType::Half4 => MTLVertexFormat::Half4,
        _ => MTLVertexFormat::Float4,
    }
}

struct Ctx {
    msc: Msc,
    dev: Shared,
    vs: Mutex<HashMap<Vec<(String, usize)>, Shared2>>,
    tally: Mutex<Tally>,
    seen: Mutex<HashSet<[u8; 24]>>,
    next: AtomicUsize,
    files: Vec<PathBuf>,
}
struct Shared2(Retained<ProtocolObject<dyn MTLFunction>>);
// SAFETY: MTLFunction objects are immutable after creation.
unsafe impl Sync for Shared2 {}

fn worker(ctx: &Ctx, id: usize) {
    let tmp = std::env::temp_dir().join(format!("victory-corpus-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&tmp).expect("temp dir");
    loop {
        let i = ctx.next.fetch_add(1, Ordering::Relaxed);
        let Some(path) = ctx.files.get(i) else { break };
        let Ok(bytes) = std::fs::read(path) else {
            ctx.tally.lock().unwrap().unreadable += 1;
            continue;
        };
        ctx.tally.lock().unwrap().files += 1;
        if let Some(reason) = bytes.strip_prefix(b"VSHE") {
            let r = normalise(&String::from_utf8_lossy(reason));
            *ctx.tally.lock().unwrap().refused.entry(r).or_default() += 1;
            continue;
        }
        let head = bytes.get(4..12).filter(|_| bytes.starts_with(b"VSH1"));
        let Some(head) = head else {
            ctx.tally.lock().unwrap().unreadable += 1;
            continue;
        };
        let stage = u32::from_le_bytes(head[..4].try_into().unwrap()) as usize;
        let n = u32::from_le_bytes(head[4..].try_into().unwrap()) as usize;
        let Some(dxil) = bytes.get(12..12 + n).filter(|_| stage < 8 && n >= 20) else {
            ctx.tally.lock().unwrap().unreadable += 1;
            continue;
        };
        // The DXBC container digest plus its length name a shader across builds.
        let mut key = [0u8; 24];
        key[..16].copy_from_slice(&dxil[4..20]);
        key[16..].copy_from_slice(&(n as u64).to_le_bytes());
        if !ctx.seen.lock().unwrap().insert(key) {
            ctx.tally.lock().unwrap().duplicates += 1;
            continue;
        }
        let name = STAGES[stage];
        let hex: String = key[..8].iter().map(|b| format!("{b:02x}")).collect();
        let mut row = Row {
            unique: 1,
            dxil_bytes: n,
            ..Row::default()
        };
        let mut fail = None;
        let t = Instant::now();
        match ctx.msc.convert(dxil, None) {
            Err(e) => {
                let k = format!("{} ({}): {}", e.code, code_name(e.code), normalise(&e.msg));
                fail = Some(format!("convert {}: {}", e.code, e.msg));
                row.convert_failed.insert(k, 1);
            }
            Ok((_, lib)) => {
                row.convert_secs = t.elapsed().as_secs_f64();
                row.converted = 1;
                row.metallib_bytes = lib.len();
                let t = Instant::now();
                // A fresh file name per shader: Metal may cache a library by its URL.
                let file = tmp.join(format!("{hex}.metallib"));
                let r = pipeline(&ctx.dev.0, &ctx.vs, name, &lib, &file);
                let _ = std::fs::remove_file(&file);
                match r {
                    Ok(()) => row.pso_ok = 1,
                    Err((load, why)) => {
                        fail = Some(format!("{} {why}", if load { "load" } else { "pso" }));
                        let m = if load {
                            &mut row.load_failed
                        } else {
                            &mut row.pso_failed
                        };
                        m.insert(normalise(&why), 1);
                    }
                }
                row.pso_secs = t.elapsed().as_secs_f64();
            }
        }
        let mut t = ctx.tally.lock().unwrap();
        if let Some(f) = fail {
            t.fails.push(format!("{name} {hex} {f}"));
        }
        let r = t.rows.entry(name).or_default();
        r.unique += row.unique;
        r.dxil_bytes += row.dxil_bytes;
        r.converted += row.converted;
        r.metallib_bytes += row.metallib_bytes;
        r.convert_secs += row.convert_secs;
        r.pso_ok += row.pso_ok;
        r.pso_secs += row.pso_secs;
        for (src, dst) in [
            (row.convert_failed, &mut r.convert_failed),
            (row.load_failed, &mut r.load_failed),
            (row.pso_failed, &mut r.pso_failed),
        ] {
            for (k, v) in src {
                *dst.entry(k).or_default() += v;
            }
        }
    }
    let _ = std::fs::remove_dir(&tmp);
}

/// Load the converted library and make the pipeline state the stage allows. `Err((true, ..))` is a
/// Metal library load failure, `(false, ..)` a pipeline failure.
fn pipeline(
    dev: &Dev,
    dummies: &Mutex<HashMap<Vec<(String, usize)>, Shared2>>,
    stage: &str,
    lib: &[u8],
    tmp: &Path,
) -> Result<(), (bool, String)> {
    std::fs::write(tmp, lib).map_err(|e| (true, e.to_string()))?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(tmp.to_str().unwrap()));
    let l = dev
        .newLibraryWithURL_error(&url)
        .map_err(|e| (true, first_line(&e)))?;
    let names = l.functionNames();
    let f = names
        .iter()
        .next()
        .and_then(|n| l.newFunctionWithName(&n))
        .ok_or((true, "library has no function".to_string()))?;
    let pso = |e: Retained<objc2_foundation::NSError>| (false, first_line(&e));
    match stage {
        "Cs" => dev
            .newComputePipelineStateWithFunction_error(&f)
            .map(drop)
            .map_err(pso),
        "Vs" => {
            // Depth-only: no fragment function, and the vertex descriptor the stage-in needs.
            let d = MTLRenderPipelineDescriptor::new();
            d.setVertexFunction(Some(&f));
            let vd = MTLVertexDescriptor::vertexDescriptor();
            if let Some(attrs) = f.vertexAttributes() {
                for (n, a) in attrs.iter().enumerate() {
                    // SAFETY: attribute and buffer indices are below Metal's 31 limit.
                    unsafe {
                        let at = vd.attributes().objectAtIndexedSubscript(a.attributeIndex());
                        at.setFormat(vertex_format(a.attributeType()));
                        at.setOffset(n * 16);
                        at.setBufferIndex(8);
                    }
                }
                // SAFETY: as above.
                let l = unsafe { vd.layouts().objectAtIndexedSubscript(8) };
                // SAFETY: the layout index is 8, below the 31 buffer limit.
                unsafe { l.setStride(attrs.len().max(1) * 16) };
            }
            d.setVertexDescriptor(Some(&vd));
            dev.newRenderPipelineStateWithDescriptor_error(&d)
                .map(drop)
                .map_err(pso)
        }
        "Ps" => {
            let mut names: Vec<(String, usize)> = vec![];
            let mut formats = [MTLPixelFormat::RGBA16Float; 8];
            let mut last = String::new();
            for _ in 0..24 {
                let dummy = {
                    let mut m = dummies.lock().unwrap();
                    m.entry(names.clone())
                        .or_insert_with(|| Shared2(dummy_vertex(dev, &names)))
                        .0
                        .clone()
                };
                let make = |formats: &[MTLPixelFormat; 8]| {
                    let d = MTLRenderPipelineDescriptor::new();
                    d.setVertexFunction(Some(&dummy));
                    d.setFragmentFunction(Some(&f));
                    for (i, fmt) in formats.iter().enumerate() {
                        // SAFETY: i < 8, the attachment limit.
                        unsafe { d.colorAttachments().objectAtIndexedSubscript(i) }
                            .setPixelFormat(*fmt);
                    }
                    d.setDepthAttachmentPixelFormat(MTLPixelFormat::Depth32Float);
                    dev.newRenderPipelineStateWithDescriptor_error(&d)
                        .map_err(|e| first_line(&e))
                };
                let e = match make(&formats) {
                    Ok(_) => return Ok(()),
                    Err(e) => e,
                };
                if e.contains("output of type") {
                    // The error does not name the attachment: probe each one alone.
                    for i in 0..8 {
                        for fmt in [
                            MTLPixelFormat::RGBA16Float,
                            MTLPixelFormat::RGBA32Uint,
                            MTLPixelFormat::RGBA32Sint,
                        ] {
                            let mut one = [MTLPixelFormat::Invalid; 8];
                            one[i] = fmt;
                            if make(&one).is_ok_and(|_| true)
                                || make(&one).is_err_and(|e| !e.contains("output of type"))
                            {
                                formats[i] = fmt;
                                break;
                            }
                        }
                    }
                    if let Err(e) = make(&formats)
                        && e.contains("output of type")
                    {
                        return Err((false, e));
                    }
                } else {
                    let listed = missing_varyings(&e);
                    if listed.is_empty() {
                        return Err((false, e));
                    }
                    // A listed name already offered has the wrong type: try the next one.
                    for n in listed {
                        match names.iter_mut().find(|(m, _)| *m == n) {
                            Some((_, t)) if *t + 1 < TYPES.len() => *t += 1,
                            Some(_) => return Err((false, e)),
                            None => names.push((n, 0)),
                        }
                    }
                }
                last = e;
            }
            Err((false, last))
        }
        "Ms" => {
            let d = MTLMeshRenderPipelineDescriptor::new();
            // SAFETY: `f` is a mesh function from the converter.
            unsafe { d.setMeshFunction(Some(&f)) };
            d.setRasterizationEnabled(false);
            dev.newRenderPipelineStateWithMeshDescriptor_options_reflection_error(
                &d,
                MTLPipelineOption::None,
                None,
            )
            .map(drop)
            .map_err(pso)
        }
        _ => Err((false, "no pipeline kind for this stage".into())),
    }
}

pub fn run(args: &[String]) {
    let arg = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
    };
    let root = args.first().expect("--corpus <cache dir>");
    let limit: usize = arg("--limit").map_or(usize::MAX, |v| v.parse().unwrap());
    let threads: usize = arg("--threads").map_or(8, |v| v.parse().unwrap());
    let msc = Msc::open_default();
    let dev = MTLCreateSystemDefaultDevice().expect("no Metal device");
    let mut files = Vec::new();
    walk(Path::new(root), &mut files);
    files.sort();
    files.truncate(limit);
    println!("{} cache files under {root}", files.len());
    let ctx = Ctx {
        msc,
        dev: Shared(dev),
        vs: Mutex::default(),
        tally: Mutex::default(),
        seen: Mutex::default(),
        next: AtomicUsize::new(0),
        files,
    };
    let t = Instant::now();
    std::thread::scope(|s| {
        for id in 0..threads {
            let ctx = &ctx;
            s.spawn(move || worker(ctx, id));
        }
    });
    let wall = t.elapsed().as_secs_f64();
    let t = ctx.tally.into_inner().unwrap();
    println!(
        "{} files read, {} refused by the translator, {} duplicate containers, {} unreadable; {wall:.0} s on {threads} threads",
        t.files,
        t.refused.values().sum::<usize>(),
        t.duplicates,
        t.unreadable
    );
    println!(
        "\nstage  unique  dxil MB  converted  metallib MB  conv ms avg  pipeline ok  pso ms avg"
    );
    for (name, r) in &t.rows {
        println!(
            "{name:5} {:7} {:8.1} {:10} {:12.1} {:12.1} {:12} {:11.1}",
            r.unique,
            r.dxil_bytes as f64 / 1e6,
            r.converted,
            r.metallib_bytes as f64 / 1e6,
            r.convert_secs * 1e3 / r.converted.max(1) as f64,
            r.pso_ok,
            r.pso_secs * 1e3 / r.converted.max(1) as f64,
        );
    }
    for (what, pick) in [
        ("convert failed", 0),
        ("metallib load failed", 1),
        ("pipeline failed", 2),
    ] {
        for (name, r) in &t.rows {
            let m = [&r.convert_failed, &r.load_failed, &r.pso_failed][pick];
            let mut v: Vec<_> = m.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1));
            for (why, n) in v.iter().take(12) {
                println!("{what} {name} x{n}: {why}");
            }
        }
    }
    let mut v: Vec<_> = t.refused.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1));
    for (why, n) in v.iter().take(15) {
        println!("translator refusal x{n}: {why}");
    }
    if let Some(f) = arg("--fails") {
        let mut fails = t.fails;
        fails.sort();
        std::fs::write(f, fails.join("\n")).expect("write --fails");
    }
}
