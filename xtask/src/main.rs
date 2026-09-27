//! `cargo xtask <command>`: build orchestration for CarvOS.
//!
//! Commands:
//! - `build [--release]`: build the kernel and check it is linked in the higher half.
//! - `limine`: fetch and verify the pinned Limine bootloader files (done automatically by `image`).
//! - `image [--release]`: build a UEFI + BIOS bootable ISO (`target/carv-os.iso`) and a blank
//!   virtio data disk (`target/data.img`).
//! - `run [--release] [--bios] [--debug] [--timeout SECS]`: boot the image in QEMU with serial on
//!   stdio. Uses KVM when `/dev/kvm` is usable, TCG otherwise.
//!
//! `test` arrives in P0.4.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

const KERNEL_TARGET: &str = "x86_64-unknown-none";
const KERNEL_PACKAGE: &str = "chisel";
/// Start of the top 2 GiB of the address space, where `kernel/linker.ld` places the kernel.
const HIGHER_HALF_BASE: u64 = 0xffff_ffff_8000_0000;
const ISO_NAME: &str = "carv-os.iso";
const DATA_IMG_NAME: &str = "data.img";
const DATA_IMG_BYTES: u64 = 64 * 1024 * 1024;

/// Limine release pinned by the commit on its `v11.x-binary` branch ("Binary release v11.4.1").
/// Every file is verified against its SHA-256 before use; bump all of these together.
const LIMINE_VERSION: &str = "11.4.1";
const LIMINE_COMMIT: &str = "5be26a73d7b7b4d4477d18be94e1d16e615adf56";
const LIMINE_FILES: &[(&str, &str)] = &[
    (
        "BOOTX64.EFI",
        "333f7a69379b1f47e019be215885cdc078b9edd6e9255cbe2f706d6c5ec0ef06",
    ),
    (
        "limine-bios-cd.bin",
        "18f7823c631e92797e00c320395ed4151e9dcd36ca4c69c51de8da6a473bb3b2",
    ),
    (
        "limine-bios.sys",
        "13108feb6399758085ec09de073da5fc74d6b3b7d0df4034e307a28ec37b82a2",
    ),
    (
        "limine-uefi-cd.bin",
        "0c7a1daf60d2140d124bf020ea99fff95788963afc55ed9acdb9bbfe603a7b20",
    ),
    (
        "limine.c",
        "d448f0f1a3138c12858232b901a06e1d35e3223a7ee458af999217a916bde92e",
    ),
    (
        "limine-bios-hdd.h",
        "ddaa1af5a644b438634b8c83c056f12022903d67623807fd318048cf41d92927",
    ),
    (
        "LICENSE",
        "f174b8ff78f6f8982a1fc992668f8486eb8cf598f6938923c22e9bb50f8338a8",
    ),
];

/// Candidate OVMF (UEFI firmware) code/vars pairs, in probe order. Code and vars must match
/// in flavour (2M with 2M, 4M with 4M). Override with `CARV_OVMF_CODE` / `CARV_OVMF_VARS`.
const OVMF_CANDIDATES: &[(&str, &str)] = &[
    // Fedora / RHEL (edk2-ovmf)
    (
        "/usr/share/edk2/ovmf/OVMF_CODE.fd",
        "/usr/share/edk2/ovmf/OVMF_VARS.fd",
    ),
    // Debian / Ubuntu (ovmf)
    (
        "/usr/share/OVMF/OVMF_CODE_4M.fd",
        "/usr/share/OVMF/OVMF_VARS_4M.fd",
    ),
    (
        "/usr/share/OVMF/OVMF_CODE.fd",
        "/usr/share/OVMF/OVMF_VARS.fd",
    ),
    // Arch (edk2-ovmf)
    (
        "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
        "/usr/share/edk2/x64/OVMF_VARS.4m.fd",
    ),
    (
        "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd",
        "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd",
    ),
];

/// Exit status used when `run --timeout` fires, mirroring coreutils `timeout`.
const EXIT_TIMEOUT: u8 = 124;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("build") => build(&args[1..]).map(|_| ()),
        Some("limine") => ensure_limine().map(|_| ()),
        Some("image") => image(&args[1..]).map(|_| ()),
        Some("run") => run(&args[1..]),
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
        Err(msg) if msg == "timeout" => ExitCode::from(EXIT_TIMEOUT),
        Err(msg) => {
            eprintln!("xtask: error: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    println!("Usage: cargo xtask <command>\n");
    println!("Commands:");
    println!("  build [--release]                Build the kernel and verify its layout");
    println!(
        "  limine                           Fetch + verify the pinned Limine {LIMINE_VERSION} files"
    );
    println!(
        "  image [--release]                Build target/{ISO_NAME} (UEFI + BIOS) and target/{DATA_IMG_NAME}"
    );
    println!("  run [--release] [--bios] [--debug] [--timeout SECS]");
    println!("                                   Boot the image in QEMU, serial on stdio");
    println!("    --bios      boot with SeaBIOS instead of OVMF (UEFI)");
    println!("    --debug     log interrupts and CPU resets to target/qemu.log");
    println!("    --timeout   kill QEMU after SECS seconds and exit {EXIT_TIMEOUT}");
}

// ---------------------------------------------------------------- build

/// Builds the kernel and returns the path to its ELF file.
fn build(args: &[String]) -> Result<PathBuf, String> {
    let release = match args {
        [] => false,
        [flag] if flag == "--release" => true,
        _ => return Err(format!("unexpected arguments to build: {args:?}")),
    };
    build_kernel(release)
}

fn build_kernel(release: bool) -> Result<PathBuf, String> {
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
    let kernel = target_dir()
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

// ---------------------------------------------------------------- limine

/// Downloads (once) and verifies the pinned Limine files, and compiles the `limine` host tool
/// used for `bios-install`. Returns the directory holding them.
fn ensure_limine() -> Result<PathBuf, String> {
    let dir = target_dir().join("limine").join(LIMINE_COMMIT);
    fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;

    for (name, expected) in LIMINE_FILES {
        let path = dir.join(name);
        if path.is_file() && sha256_hex(&path)? == *expected {
            continue;
        }
        let url = format!(
            "https://raw.githubusercontent.com/limine-bootloader/limine/{LIMINE_COMMIT}/{name}"
        );
        println!("limine: fetching {name}");
        let status = Command::new("curl")
            .args(["-sSfL", "--retry", "3", "-o"])
            .arg(&path)
            .arg(&url)
            .status()
            .map_err(|e| format!("failed to run curl (is it installed?): {e}"))?;
        if !status.success() {
            return Err(format!("download of {url} failed"));
        }
        let actual = sha256_hex(&path)?;
        if actual != *expected {
            let _ = fs::remove_file(&path);
            return Err(format!(
                "SHA-256 mismatch for {name}: expected {expected}, got {actual}"
            ));
        }
    }

    // The BIOS installer is a single C file; skip it (UEFI-only ISO) when no C compiler exists.
    let tool = dir.join("limine");
    if !tool.is_file() {
        let cc = env::var("CC").unwrap_or_else(|_| "cc".into());
        let compiled = Command::new(&cc)
            .args(["-O2", "-std=c99", "-o"])
            .arg(&tool)
            .arg(dir.join("limine.c"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !compiled {
            eprintln!(
                "limine: warning: could not compile limine.c with `{cc}`; the ISO will boot on UEFI only"
            );
        }
    }
    println!("limine: {LIMINE_VERSION} ready in {}", dir.display());
    Ok(dir)
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let digest = Sha256::digest(&bytes);
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

// ---------------------------------------------------------------- image

/// Builds the kernel, assembles the ISO tree, and produces `target/carv-os.iso` plus a blank
/// `target/data.img`. Returns the ISO path.
fn image(args: &[String]) -> Result<PathBuf, String> {
    let release = match args {
        [] => false,
        [flag] if flag == "--release" => true,
        _ => return Err(format!("unexpected arguments to image: {args:?}")),
    };
    build_image(release)
}

fn build_image(release: bool) -> Result<PathBuf, String> {
    let kernel = build_kernel(release)?;
    let limine = ensure_limine()?;

    let root = target_dir().join("iso_root");
    if root.exists() {
        fs::remove_dir_all(&root).map_err(|e| format!("cleaning {}: {e}", root.display()))?;
    }
    let boot = root.join("boot");
    let boot_limine = boot.join("limine");
    let efi_boot = root.join("EFI").join("BOOT");
    for d in [&boot_limine, &efi_boot] {
        fs::create_dir_all(d).map_err(|e| format!("creating {}: {e}", d.display()))?;
    }

    copy(&kernel, &boot.join(KERNEL_PACKAGE))?;
    fs::write(boot_limine.join("limine.conf"), limine_conf())
        .map_err(|e| format!("writing limine.conf: {e}"))?;
    for name in [
        "limine-bios.sys",
        "limine-bios-cd.bin",
        "limine-uefi-cd.bin",
    ] {
        copy(&limine.join(name), &boot_limine.join(name))?;
    }
    copy(&limine.join("BOOTX64.EFI"), &efi_boot.join("BOOTX64.EFI"))?;

    let iso = target_dir().join(ISO_NAME);
    let status = Command::new("xorriso")
        .args([
            "-as",
            "mkisofs",
            "-quiet",
            "-R",
            "-r",
            "-J",
            "-b",
            "boot/limine/limine-bios-cd.bin",
            "-no-emul-boot",
            "-boot-load-size",
            "4",
            "-boot-info-table",
            "-hfsplus",
            "-apm-block-size",
            "2048",
            "--efi-boot",
            "boot/limine/limine-uefi-cd.bin",
            "-efi-boot-part",
            "--efi-boot-image",
            "--protective-msdos-label",
        ])
        .arg(&root)
        .arg("-o")
        .arg(&iso)
        .status()
        .map_err(|e| format!("failed to run xorriso (is it installed?): {e}"))?;
    if !status.success() {
        return Err("xorriso failed".into());
    }

    let installer = limine.join("limine");
    if installer.is_file() {
        let status = Command::new(&installer)
            .arg("bios-install")
            .arg(&iso)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("failed to run limine bios-install: {e}"))?;
        if !status.success() {
            return Err("limine bios-install failed".into());
        }
    }

    let data = target_dir().join(DATA_IMG_NAME);
    if !data.is_file() {
        let f = fs::File::create(&data).map_err(|e| format!("creating {}: {e}", data.display()))?;
        f.set_len(DATA_IMG_BYTES)
            .map_err(|e| format!("sizing {}: {e}", data.display()))?;
        println!(
            "data disk: {} ({} MiB, blank)",
            data.display(),
            DATA_IMG_BYTES >> 20
        );
    }

    println!("image: {}", iso.display());
    Ok(iso)
}

/// Limine configuration. `serial: yes` mirrors the boot menu to COM1 so the handoff is
/// visible (and testable) on `-serial stdio`.
fn limine_conf() -> String {
    format!(
        "timeout: 1\nserial: yes\n\n/CarvOS\n    protocol: limine\n    path: boot():/boot/{KERNEL_PACKAGE}\n    kaslr: no\n"
    )
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("copying {} -> {}: {e}", from.display(), to.display()))
}

// ---------------------------------------------------------------- run

struct RunOpts {
    release: bool,
    bios: bool,
    debug: bool,
    timeout: Option<Duration>,
}

fn parse_run_opts(args: &[String]) -> Result<RunOpts, String> {
    let mut opts = RunOpts {
        release: false,
        bios: false,
        debug: false,
        timeout: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--release" => opts.release = true,
            "--bios" => opts.bios = true,
            "--debug" => opts.debug = true,
            "--timeout" => {
                let secs = it
                    .next()
                    .ok_or("--timeout needs a value in seconds")?
                    .parse::<u64>()
                    .map_err(|e| format!("bad --timeout value: {e}"))?;
                opts.timeout = Some(Duration::from_secs(secs));
            }
            other => return Err(format!("unknown run option `{other}`")),
        }
    }
    Ok(opts)
}

fn run(args: &[String]) -> Result<(), String> {
    let opts = parse_run_opts(args)?;
    let iso = build_image(opts.release)?;
    let mut qemu = qemu_command(&iso, &opts)?;

    println!("qemu: {}", shell_words(&qemu));
    let mut child = qemu
        .spawn()
        .map_err(|e| format!("failed to start qemu-system-x86_64 (is it installed?): {e}"))?;

    let status = match opts.timeout {
        None => child.wait().map_err(|e| format!("waiting for qemu: {e}"))?,
        Some(limit) => {
            let start = Instant::now();
            loop {
                if let Some(s) = child
                    .try_wait()
                    .map_err(|e| format!("waiting for qemu: {e}"))?
                {
                    break s;
                }
                if start.elapsed() >= limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    eprintln!("\nxtask: qemu killed after {}s timeout", limit.as_secs());
                    return Err("timeout".into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    if status.success() {
        Ok(())
    } else {
        Err(format!("qemu exited with {status}"))
    }
}

fn qemu_command(iso: &Path, opts: &RunOpts) -> Result<Command, String> {
    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.args(["-machine", "q35", "-cpu", "max", "-m", "512M"]);

    if kvm_usable() {
        cmd.args(["-accel", "kvm"]);
    } else {
        eprintln!("xtask: /dev/kvm not usable, falling back to TCG (slow)");
        cmd.args(["-accel", "tcg"]);
    }

    if !opts.bios {
        let (code, vars) = find_ovmf()?;
        // OVMF wants a writable vars image; give it a private copy so runs don't share state.
        let vars_copy = target_dir().join("OVMF_VARS.fd");
        copy(&vars, &vars_copy)?;
        cmd.arg("-drive").arg(format!(
            "if=pflash,unit=0,format=raw,readonly=on,file={}",
            code.display()
        ));
        cmd.arg("-drive").arg(format!(
            "if=pflash,unit=1,format=raw,file={}",
            vars_copy.display()
        ));
    }

    cmd.arg("-cdrom").arg(iso).args(["-boot", "d"]);
    cmd.arg("-drive").arg(format!(
        "file={},if=none,id=d0,format=raw",
        target_dir().join(DATA_IMG_NAME).display()
    ));
    cmd.args(["-device", "virtio-blk-pci,drive=d0"]);
    cmd.args(["-netdev", "user,id=n0,hostfwd=tcp::5555-:23"]);
    cmd.args(["-device", "virtio-net-pci,netdev=n0"]);
    cmd.args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]);
    cmd.args(["-serial", "stdio", "-display", "none", "-no-reboot"]);

    if opts.debug {
        let log = target_dir().join("qemu.log");
        cmd.args(["-d", "int,cpu_reset,guest_errors", "-D"])
            .arg(log);
    }
    Ok(cmd)
}

fn kvm_usable() -> bool {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
}

fn find_ovmf() -> Result<(PathBuf, PathBuf), String> {
    if let (Ok(code), Ok(vars)) = (env::var("CARV_OVMF_CODE"), env::var("CARV_OVMF_VARS")) {
        return Ok((code.into(), vars.into()));
    }
    for (code, vars) in OVMF_CANDIDATES {
        if Path::new(code).is_file() && Path::new(vars).is_file() {
            return Ok((code.into(), vars.into()));
        }
    }
    Err("OVMF firmware not found; install edk2-ovmf (Fedora) / ovmf (Debian), or set CARV_OVMF_CODE and CARV_OVMF_VARS, or use --bios".into())
}

/// Renders a command line for display only (no escaping guarantees).
fn shell_words(cmd: &Command) -> String {
    let mut s = cmd.get_program().to_string_lossy().into_owned();
    for a in cmd.get_args() {
        let a = a.to_string_lossy();
        s.push(' ');
        if a.contains(' ') {
            s.push_str(&format!("'{a}'"));
        } else {
            s.push_str(&a);
        }
    }
    s
}

// ---------------------------------------------------------------- shared

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the workspace root")
        .to_path_buf()
}

fn target_dir() -> PathBuf {
    workspace_root().join("target")
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

    #[test]
    fn limine_pins_are_well_formed() {
        assert_eq!(LIMINE_COMMIT.len(), 40);
        for (name, sha) in LIMINE_FILES {
            assert!(!name.is_empty());
            assert_eq!(sha.len(), 64, "{name}: sha256 must be 64 hex chars");
            assert!(sha.chars().all(|c| c.is_ascii_hexdigit()), "{name}");
        }
    }

    #[test]
    fn limine_conf_points_at_kernel() {
        let conf = limine_conf();
        assert!(conf.contains("protocol: limine"));
        assert!(conf.contains(&format!("path: boot():/boot/{KERNEL_PACKAGE}")));
        assert!(conf.contains("serial: yes"));
    }

    #[test]
    fn run_opts_parse() {
        let o = parse_run_opts(&["--bios".into(), "--timeout".into(), "7".into()]).unwrap();
        assert!(o.bios && !o.release && !o.debug);
        assert_eq!(o.timeout, Some(Duration::from_secs(7)));
        assert!(parse_run_opts(&["--timeout".into()]).is_err());
        assert!(parse_run_opts(&["--bogus".into()]).is_err());
    }
}
