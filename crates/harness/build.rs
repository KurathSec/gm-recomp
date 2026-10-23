//! Builds the in-process native x86-64 reference (tests/cpu/refrun64.c) with the system
//! gcc and links it, for the 64-bit CPU differential tester (crates/harness/src/native64.rs).

use std::path::Path;
use std::process::Command;

fn main() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/cpu/refrun64.c");
    let out = std::env::var("OUT_DIR").unwrap();
    let obj = Path::new(&out).join("refrun64.o");
    let lib = Path::new(&out).join("librefrun64.a");
    println!("cargo:rerun-if-changed={}", src.display());
    // -O1 (not higher: the trampoline's globals must not be reordered around the asm)
    let gcc = Command::new("gcc")
        .args(["-O1", "-fPIC", "-fno-stack-protector", "-fcf-protection=none", "-c", "-o"])
        .arg(&obj)
        .arg(&src)
        .status()
        .expect("running gcc for refrun64.c");
    assert!(gcc.success(), "gcc failed to build refrun64.c");
    let ar = Command::new("ar").arg("rcs").arg(&lib).arg(&obj).status().expect("running ar");
    assert!(ar.success(), "ar failed");
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static=refrun64");
}
