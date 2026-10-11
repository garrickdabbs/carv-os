use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=linker.ld");
    if env::var("TARGET").as_deref() == Ok("x86_64-unknown-none") {
        let manifest =
            PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set"));
        println!(
            "cargo:rustc-link-arg-bins=-T{}",
            manifest.join("linker.ld").display()
        );
    }
}
