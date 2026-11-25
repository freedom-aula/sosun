//! Convert DXIL files with the Metal Shader Converter and load each result into Metal.

use objc2_foundation::{NSString, NSURL};
use objc2_metal::*;
use std::time::Instant;

pub fn run(files: &[String]) {
    let msc = victory_metal::msc::Msc::open_default();
    let dev = MTLCreateSystemDefaultDevice().expect("no Metal device");
    for f in files {
        let dxil = std::fs::read(f).expect("read dxil");
        let t = Instant::now();
        let (stage, lib) = match msc.convert(&dxil, None) {
            Ok(r) => r,
            Err(e) => {
                println!("{f}: convert FAILED: {e}");
                continue;
            }
        };
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let out = std::env::temp_dir().join("victory-metal-conv.metallib");
        std::fs::write(&out, &lib).unwrap();
        let url = NSURL::fileURLWithPath(&NSString::from_str(out.to_str().unwrap()));
        let l = match dev.newLibraryWithURL_error(&url) {
            Ok(l) => l,
            Err(e) => {
                println!(
                    "{f}: stage {stage}, {} B metallib in {ms:.1} ms; Metal load FAILED: {e}",
                    lib.len()
                );
                continue;
            }
        };
        let names: Vec<String> = l.functionNames().iter().map(|n| n.to_string()).collect();
        println!(
            "{f}: stage {stage}, {} B DXIL -> {} B metallib in {ms:.1} ms; loaded, functions {names:?}",
            dxil.len(),
            lib.len()
        );
        if stage == 8 {
            let func = l
                .newFunctionWithName(&NSString::from_str(&names[0]))
                .unwrap();
            match dev.newComputePipelineStateWithFunction_error(&func) {
                Ok(p) => println!(
                    "   compute PSO ok, maxThreads/tg {}",
                    p.maxTotalThreadsPerThreadgroup()
                ),
                Err(e) => println!("   compute PSO FAILED: {e}"),
            }
        }
    }
}
