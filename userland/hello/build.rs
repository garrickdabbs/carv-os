use std::{env, path::PathBuf};

fn main() {
    let linker = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set"))
        .join("../../crates/carv-rt/linker.ld");
    println!("cargo:rerun-if-changed={}", linker.display());
    if env::var("TARGET").as_deref() == Ok("x86_64-unknown-none") {
        println!("cargo:rustc-link-arg-bins=-T{}", linker.display());
    }
}
