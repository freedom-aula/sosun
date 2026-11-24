use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let gnu = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu");
    let llvm = env::var("CARGO_CFG_TARGET_ABI").as_deref() == Ok("llvm");
    if gnu && llvm {
        // lld in MinGW mode delay-loads by name, like MSVC's /DELAYLOAD.
        println!("cargo:rustc-link-arg-cdylib=-Wl,--delayload=victory-probe-delayed.dll");
    } else if gnu {
        // MinGW's linker has no /DELAYLOAD, so dlltool makes a delay-import
        // library for the one delay-loaded DLL instead.
        let out = PathBuf::from(env::var("OUT_DIR").unwrap());
        let def = out.join("victory-probe-delayed.def");
        std::fs::write(&def, "LIBRARY victory-probe-delayed.dll\nEXPORTS\nVIctoryProbeDelayed\n").unwrap();
        let status = Command::new("x86_64-w64-mingw32-dlltool")
            .arg("-d")
            .arg(&def)
            .arg("-y")
            .arg(out.join("libprobe_delayed.a"))
            .status()
            .expect("running x86_64-w64-mingw32-dlltool");
        assert!(status.success(), "dlltool failed");
        println!("cargo:rustc-link-search=native={}", out.display());
        println!("cargo:rerun-if-changed=build.rs");
    } else {
        println!("cargo:rustc-link-arg-cdylib=/DELAYLOAD:victory-probe-delayed.dll");
        println!("cargo:rustc-link-lib=delayimp");
    }
}
