//! `cargo xtask <command>`: build orchestration for CarvOS.
//!
//! Commands so far:
//! - `build [--release]`: build the kernel and check it is linked in the higher half.
//!
//! `image`, `run`, and `test` arrive in P0.2 and P0.4.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const KERNEL_TARGET: &str = "x86_64-unknown-none";
const KERNEL_PACKAGE: &str = "chisel";
/// Start of the top 2 GiB of the address space, where `kernel/linker.ld` places the kernel.
const HIGHER_HALF_BASE: u64 = 0xffff_ffff_8000_0000;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("build") => build(&args[1..]).map(|_| ()),
        Some("help") | None => {
            print_help();
            Ok(())
        }
        Some(other) => Err(format!(
            "unknown command `{other}` (try `cargo xtask help`)"
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("xtask: error: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    println!("Usage: cargo xtask <command>\n");
    println!("Commands:");
    println!("  build [--release]   Build the kernel and verify its layout");
}

/// Builds the kernel and returns the path to its ELF file.
fn build(args: &[String]) -> Result<PathBuf, String> {
    let release = match args {
        [] => false,
        [flag] if flag == "--release" => true,
        _ => return Err(format!("unexpected arguments to build: {args:?}")),
    };

    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(workspace_root()).args([
        "build",
        "--package",
        KERNEL_PACKAGE,
        "--target",
        KERNEL_TARGET,
    ]);
    if release {
        cmd.arg("--release");
    }
    let status = cmd
        .status()
        .map_err(|e| format!("failed to run cargo: {e}"))?;
    if !status.success() {
        return Err("kernel build failed".into());
    }

    let profile = if release { "release" } else { "debug" };
    let kernel = workspace_root()
        .join("target")
        .join(KERNEL_TARGET)
        .join(profile)
        .join(KERNEL_PACKAGE);
    let bytes = fs::read(&kernel).map_err(|e| format!("reading {}: {e}", kernel.display()))?;
    let entry = elf64_entry(&bytes)?;
    if entry < HIGHER_HALF_BASE {
        return Err(format!(
            "kernel entry point {entry:#x} is below the higher half ({HIGHER_HALF_BASE:#x}); check kernel/linker.ld"
        ));
    }
    println!("kernel: {} (entry {entry:#x})", kernel.display());
    Ok(kernel)
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the workspace root")
        .to_path_buf()
}

/// Reads the entry point from a little-endian x86_64 ELF64 file header.
fn elf64_entry(bytes: &[u8]) -> Result<u64, String> {
    const EM_X86_64: u16 = 62;
    if bytes.len() < 64 || &bytes[..4] != b"\x7fELF" {
        return Err("kernel is not an ELF file".into());
    }
    if bytes[4] != 2 || bytes[5] != 1 {
        return Err("kernel is not a little-endian ELF64 file".into());
    }
    let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
    if machine != EM_X86_64 {
        return Err(format!(
            "kernel is built for ELF machine {machine}, not x86_64"
        ));
    }
    Ok(u64::from_le_bytes(bytes[24..32].try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(entry: u64) -> Vec<u8> {
        let mut h = vec![0u8; 64];
        h[..4].copy_from_slice(b"\x7fELF");
        h[4] = 2; // ELFCLASS64
        h[5] = 1; // little-endian
        h[18..20].copy_from_slice(&62u16.to_le_bytes());
        h[24..32].copy_from_slice(&entry.to_le_bytes());
        h
    }

    #[test]
    fn reads_entry_point() {
        assert_eq!(elf64_entry(&header(HIGHER_HALF_BASE)), Ok(HIGHER_HALF_BASE));
    }

    #[test]
    fn rejects_non_elf() {
        assert!(elf64_entry(b"not an elf file at all, definitely not").is_err());
        assert!(elf64_entry(&[0x7f, b'E', b'L', b'F']).is_err());
    }

    #[test]
    fn rejects_wrong_machine() {
        let mut h = header(HIGHER_HALF_BASE);
        h[18..20].copy_from_slice(&3u16.to_le_bytes()); // EM_386
        assert!(elf64_entry(&h).is_err());
    }
}
