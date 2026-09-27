//! `cargo xtask <command>`: build orchestration for CarvOS.
//!
//! Commands:
//! - `build [--release]`: build the kernel and check it is linked in the higher half.
//! - `limine`: fetch and verify the pinned Limine bootloader files (done automatically by `image`).
//! - `image [--release]`: build a UEFI + BIOS bootable ISO (`target/carv-os.iso`) and a blank
//!   virtio data disk (`target/data.img`).
//! - `run [--release] [--bios] [--debug] [--timeout SECS] [--cmdline STR]`: boot the image in QEMU
//!   with serial on stdio. Uses KVM when `/dev/kvm` is usable, TCG otherwise.
//!
//! - `smoke [--release] [--timeout SECS]`: boot UEFI, BIOS and panic-test scenarios in QEMU and check
//!   the serial output (CI's boot gate until `test` arrives in P0.4).
//! - `docs-gate [--base REF] [--repo PATH]`: fail when code changed without matching documentation
//!   changes (content-checked CHANGELOG growth, new-ADR rule, reasoned skip marker).
//! - `perf [--runs N]`: measure kernel/ISO size and boot-to-banner time against budgets.
//! - `test [--host] [--kernel] [--integration] [--release] [--timeout SECS]`: the one command every
//!   PR must pass — host unit tests, the in-kernel test binary under QEMU (`isa-debug-exit`), and
//!   the boot smoke scenarios. With no selector, all three run.
//! - `release-check [--tag vX.Y.Z[-pre]]`: verify the tag matches the workspace version and the
//!   CHANGELOG has a section for it; prints `version=`/`prerelease=` (also to `$GITHUB_OUTPUT`).
//!
//! `run` and `smoke` accept `--iso PATH` to boot an existing image (e.g. a release artifact)
//! instead of building one. ISO assembly honours `SOURCE_DATE_EPOCH` (defaulting to the commit
//! time) and `--release` images carry a debuginfo-stripped kernel; together with the release
//! profile's `trim-paths` this makes two builds of the same commit byte-identical on any machine.

#![deny(clippy::undocumented_unsafe_blocks)]
#![deny(missing_docs)]

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
        Some("smoke") => smoke(&args[1..]),
        Some("docs-gate") => docs_gate(&args[1..]),
        Some("perf") => perf(&args[1..]),
        Some("test") => test(&args[1..]),
        Some("release-check") => release_check(&args[1..]),
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
    println!("  run [--release] [--bios] [--debug] [--timeout SECS] [--cmdline STR]");
    println!("                                   Boot the image in QEMU, serial on stdio");
    println!("    --bios      boot with SeaBIOS instead of OVMF (UEFI)");
    println!("    --debug     log interrupts and CPU resets to target/qemu.log");
    println!("    --timeout   kill QEMU after SECS seconds and exit {EXIT_TIMEOUT}");
    println!("    --cmdline   kernel command line passed through Limine (e.g. panic-test)");
    println!("  smoke [--release] [--timeout SECS]");
    println!(
        "                                   Boot UEFI, BIOS and panic scenarios; check serial output"
    );
    println!("  docs-gate [--base REF] [--repo PATH]");
    println!(
        "                                   Fail if code changed without documentation (default base: main)"
    );
    println!(
        "  perf [--runs N]                  Kernel/ISO size and boot timings vs budgets (report in target/perf/)"
    );
    println!("  test [--host] [--kernel] [--integration] [--release] [--timeout SECS]");
    println!(
        "                                   Host unit tests + in-kernel tests in QEMU + boot smoke (default: all)"
    );
    println!(
        "  release-check [--tag vX.Y.Z]      Tag ↔ Cargo version ↔ CHANGELOG consistency (release workflow gate)"
    );
    println!("    run/smoke --iso PATH           boot an existing image instead of building one");
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
    build_image(release, "")
}

fn build_image(release: bool, cmdline: &str) -> Result<PathBuf, String> {
    let kernel = build_kernel(release)?;
    let iso = assemble_iso(&kernel, cmdline, &target_dir(), ISO_NAME, release)?;
    ensure_data_disk()?;
    println!("image: {}", iso.display());
    Ok(iso)
}

/// Creates the blank virtio data disk if it is missing. Called from every QEMU launch path, so a
/// fresh checkout that goes straight to `xtask test --kernel` still has the drive to attach.
fn ensure_data_disk() -> Result<PathBuf, String> {
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
    Ok(data)
}

/// Assembles a bootable ISO around `kernel` (any ELF built for the kernel target — the normal
/// kernel or a test kernel) in `out_dir/iso_root`, writing `out_dir/iso_name`.
/// Release images get a debuginfo-stripped copy of the kernel (#29); the unstripped ELF stays in
/// `target/` and ships separately as `chisel-vX.Y.Z.elf`.
fn assemble_iso(
    kernel: &Path,
    cmdline: &str,
    out_dir: &Path,
    iso_name: &str,
    release: bool,
) -> Result<PathBuf, String> {
    let limine = ensure_limine()?;

    let root = out_dir.join("iso_root");
    if root.exists() {
        fs::remove_dir_all(&root).map_err(|e| format!("cleaning {}: {e}", root.display()))?;
    }
    let boot = root.join("boot");
    let boot_limine = boot.join("limine");
    let efi_boot = root.join("EFI").join("BOOT");
    for d in [&boot_limine, &efi_boot] {
        fs::create_dir_all(d).map_err(|e| format!("creating {}: {e}", d.display()))?;
    }

    let kernel_iso = boot.join(KERNEL_PACKAGE);
    copy(kernel, &kernel_iso)?;
    if release {
        // Smaller image, and immune to any DWARF detail that could still differ between machines.
        let mut objcopy = Command::new(llvm_objcopy()?);
        objcopy.arg("--strip-debug").arg(&kernel_iso);
        run_tool(&mut objcopy, "llvm-objcopy --strip-debug")?;
    }
    fs::write(boot_limine.join("limine.conf"), limine_conf(cmdline))
        .map_err(|e| format!("writing limine.conf: {e}"))?;
    for name in [
        "limine-bios.sys",
        "limine-bios-cd.bin",
        "limine-uefi-cd.bin",
    ] {
        copy(&limine.join(name), &boot_limine.join(name))?;
    }
    copy(&limine.join("BOOTX64.EFI"), &efi_boot.join("BOOTX64.EFI"))?;

    let iso = out_dir.join(iso_name);
    let mut xorriso = Command::new("xorriso");
    // Reproducibility: pin every timestamp in the image to SOURCE_DATE_EPOCH (the commit time).
    // The env var alone leaves file dates and the GPT/MBR ids derived from the source mtimes and
    // "now"; --modification-date and --set_all_file_dates make them a function of the epoch.
    let epoch = source_date_epoch();
    let stamp = iso_timestamp(epoch);
    xorriso.env("SOURCE_DATE_EPOCH", epoch.to_string());
    xorriso
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
        .arg(format!("--modification-date={stamp}00"))
        .arg("--set_all_file_dates")
        .arg(format!("@{epoch}"))
        .arg(&root)
        .arg("-o")
        .arg(&iso);
    run_tool(&mut xorriso, "xorriso (is it installed?)")?;

    let installer = limine.join("limine");
    if installer.is_file() {
        let mut bios_install = Command::new(&installer);
        bios_install.arg("bios-install").arg(&iso);
        run_tool(&mut bios_install, "limine bios-install")?;
        // bios-install writes a pseudorandom MBR disk id (limine.c: srand(time(NULL))). It is only
        // an identifier, so overwrite it with the epoch to keep the image reproducible.
        write_mbr_disk_id(&iso, epoch as u32)?;
    }
    Ok(iso)
}

/// Offset of the 4-byte MBR disk identifier (a.k.a. NT disk signature) inside sector 0.
const MBR_DISK_ID_OFFSET: u64 = 0x1B8;

fn write_mbr_disk_id(image: &Path, id: u32) -> Result<(), String> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = fs::OpenOptions::new()
        .write(true)
        .open(image)
        .map_err(|e| format!("opening {}: {e}", image.display()))?;
    f.seek(SeekFrom::Start(MBR_DISK_ID_OFFSET))
        .and_then(|_| f.write_all(&id.to_le_bytes()))
        .map_err(|e| format!("writing MBR disk id to {}: {e}", image.display()))
}

/// `YYYYMMDDhhmmss` (UTC) for a Unix timestamp; the format xorriso's date options take.
fn iso_timestamp(epoch: u64) -> String {
    let (y, m, d, hh, mm, ss) = utc_civil(epoch);
    format!("{y:04}{m:02}{d:02}{hh:02}{mm:02}{ss:02}")
}

/// Unix time → proleptic Gregorian UTC (Howard Hinnant's days-to-civil algorithm).
fn utc_civil(epoch: u64) -> (u64, u64, u64, u64, u64, u64) {
    let days = epoch / 86_400;
    let rem = epoch % 86_400;
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u64;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u64;
    let y = if m <= 2 { y + 1 } else { y } as u64;
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// `SOURCE_DATE_EPOCH` from the environment, else the HEAD commit's timestamp, else 0.
fn source_date_epoch() -> u64 {
    if let Some(v) = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse().ok())
    {
        return v;
    }
    git(&workspace_root(), &["log", "-1", "--format=%ct"])
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Runs an external tool with its output captured. Output is shown when the tool fails (so
/// diagnostics are never lost) or when `CARV_XTASK_VERBOSE` is set; otherwise it stays quiet.
fn run_tool(cmd: &mut Command, what: &str) -> Result<(), String> {
    let out = cmd
        .output()
        .map_err(|e| format!("failed to run {what}: {e}"))?;
    let verbose = env::var_os("CARV_XTASK_VERBOSE").is_some();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        let detail = join_tool_output(&stdout, &stderr);
        return Err(format!("{what} failed ({}):\n{detail}", out.status));
    }
    if verbose && !(stdout.trim().is_empty() && stderr.trim().is_empty()) {
        eprintln!("--- {what} ---\n{}{}", stdout, stderr);
    }
    Ok(())
}

/// Limine configuration. `serial: yes` mirrors the boot menu to COM1 so the handoff is
/// visible (and testable) on `-serial stdio`. `cmdline` is passed to the kernel verbatim.
fn limine_conf(cmdline: &str) -> String {
    let mut conf = format!(
        "timeout: 1\nserial: yes\n\n/CarvOS\n    protocol: limine\n    path: boot():/boot/{KERNEL_PACKAGE}\n    kaslr: no\n"
    );
    if !cmdline.is_empty() {
        assert!(
            !cmdline.contains('\n'),
            "kernel cmdline must be a single line"
        );
        conf.push_str(&format!("    cmdline: {cmdline}\n"));
    }
    conf
}

/// The toolchain's own `llvm-objcopy` (component `llvm-tools-preview`, pinned in
/// `rust-toolchain.toml`), so stripping never depends on a system binutils.
fn llvm_objcopy() -> Result<PathBuf, String> {
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let print = |args: &[&str]| -> Result<String, String> {
        let out = Command::new(&rustc)
            .args(args)
            .output()
            .map_err(|e| format!("failed to run rustc: {e}"))?;
        if !out.status.success() {
            return Err(format!("rustc {} failed", args.join(" ")));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let sysroot = print(&["--print", "sysroot"])?;
    let host = print(&["-vV"])?
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .ok_or("rustc -vV did not report a host triple")?
        .to_string();
    let path = Path::new(&sysroot)
        .join("lib/rustlib")
        .join(host)
        .join("bin/llvm-objcopy");
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "{} not found: is the llvm-tools-preview component installed? (rust-toolchain.toml lists it)",
            path.display()
        ))
    }
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
    cmdline: String,
    /// Boot this image instead of building one.
    iso: Option<PathBuf>,
}

fn parse_run_opts(args: &[String]) -> Result<RunOpts, String> {
    let mut opts = RunOpts {
        release: false,
        bios: false,
        debug: false,
        timeout: None,
        cmdline: String::new(),
        iso: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--release" => opts.release = true,
            "--bios" => opts.bios = true,
            "--debug" => opts.debug = true,
            "--iso" => opts.iso = Some(PathBuf::from(it.next().ok_or("--iso needs a path")?)),
            "--cmdline" => {
                opts.cmdline = it.next().ok_or("--cmdline needs a value")?.clone();
            }
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
    let iso = match &opts.iso {
        Some(p) => {
            if !opts.cmdline.is_empty() {
                return Err("--cmdline cannot be combined with --iso (the command line is baked into the image)".into());
            }
            ensure_data_disk()?;
            p.clone()
        }
        None => build_image(opts.release, &opts.cmdline)?,
    };
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
        let stem = iso.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
        let vars_copy = target_dir().join(format!("OVMF_VARS-{stem}.fd"));
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
    let data = ensure_data_disk()?;
    cmd.arg("-drive")
        .arg(format!("file={},if=none,id=d0,format=raw", data.display()));
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

// ---------------------------------------------------------------- smoke

/// One boot scenario checked by `smoke`.
struct Scenario {
    name: &'static str,
    bios: bool,
    cmdline: &'static str,
    /// Every entry must appear on the serial console for the scenario to pass.
    expect: &'static [&'static str],
}

const SMOKE_SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "uefi",
        bios: false,
        cmdline: "",
        expect: &["CarvOS chisel v", "bootloader: Limine", "halting"],
    },
    Scenario {
        name: "bios",
        bios: true,
        cmdline: "",
        expect: &["CarvOS chisel v", "bootloader: Limine", "halting"],
    },
    Scenario {
        name: "panic",
        bios: false,
        cmdline: "panic-test",
        expect: &["CarvOS chisel v", "chisel: PANIC at "],
    },
    Scenario {
        name: "dfault",
        bios: false,
        cmdline: "double-fault-test",
        expect: &["CarvOS chisel v", "chisel: DOUBLE FAULT"],
    },
    Scenario {
        name: "pfault",
        bios: false,
        cmdline: "page-fault-test",
        expect: &[
            "touching unmapped page",
            "PAGE FAULT accessing 0xffff900000100000",
        ],
    },
    Scenario {
        name: "sovflw",
        bios: false,
        cmdline: "stack-overflow-test",
        expect: &["recursing on a 16 KiB stack", "chisel: DOUBLE FAULT"],
    },
];

/// Boots every [`SMOKE_SCENARIOS`] entry in QEMU and checks the serial output. Each run stops as
/// soon as all expected strings have appeared, so a passing suite takes seconds. Serial logs are
/// written to `target/smoke/<name>.log` for CI to upload.
fn smoke(args: &[String]) -> Result<(), String> {
    let mut release = false;
    let mut timeout = Duration::from_secs(60);
    let mut it = args.iter();
    let mut iso: Option<PathBuf> = None;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--release" => release = true,
            "--iso" => iso = Some(PathBuf::from(it.next().ok_or("--iso needs a path")?)),
            "--timeout" => {
                let secs = it
                    .next()
                    .ok_or("--timeout needs a value in seconds")?
                    .parse::<u64>()
                    .map_err(|e| format!("bad --timeout value: {e}"))?;
                timeout = Duration::from_secs(secs);
            }
            other => return Err(format!("unknown smoke option `{other}`")),
        }
    }

    run_smoke(release, timeout, iso.as_deref())
}

/// Runs every smoke scenario; used by both `smoke` and `test --integration`. With `iso`, boots
/// that image instead of building one and skips scenarios that need their own kernel command line.
fn run_smoke(release: bool, timeout: Duration, iso: Option<&Path>) -> Result<(), String> {
    let log_dir = target_dir().join("smoke");
    fs::create_dir_all(&log_dir).map_err(|e| format!("creating {}: {e}", log_dir.display()))?;

    let mut failures = Vec::new();
    let mut skipped = 0usize;
    for sc in SMOKE_SCENARIOS {
        let iso = match iso {
            Some(p) if sc.cmdline.is_empty() => {
                ensure_data_disk()?;
                p.to_path_buf()
            }
            Some(_) => {
                println!(
                    "smoke: {:<6} SKIP  (needs its own cmdline; not applicable to --iso)",
                    sc.name
                );
                skipped += 1;
                continue;
            }
            None => build_image(release, sc.cmdline)?,
        };
        let opts = RunOpts {
            release,
            bios: sc.bios,
            debug: false,
            timeout: Some(timeout),
            cmdline: sc.cmdline.to_string(),
            iso: None,
        };
        let mut cmd = qemu_command(&iso, &opts)?;
        let started = Instant::now();
        let (output, satisfied) = run_qemu_capture(&mut cmd, timeout, sc.expect)?;
        let log = log_dir.join(format!("{}.log", sc.name));
        fs::write(&log, &output).map_err(|e| format!("writing {}: {e}", log.display()))?;
        if satisfied {
            println!(
                "smoke: {:<6} PASS  ({:.1}s, log: {})",
                sc.name,
                started.elapsed().as_secs_f32(),
                log.display()
            );
        } else {
            let missing: Vec<&str> = sc
                .expect
                .iter()
                .copied()
                .filter(|e| !output.contains(e))
                .collect();
            eprintln!(
                "smoke: {:<6} FAIL  missing {:?} after {:.1}s (log: {})",
                sc.name,
                missing,
                started.elapsed().as_secs_f32(),
                log.display()
            );
            failures.push(sc.name);
        }
    }
    if failures.is_empty() {
        let ran = SMOKE_SCENARIOS.len() - skipped;
        if skipped == 0 {
            println!("smoke: all {ran} scenarios passed");
        } else {
            println!("smoke: {ran} scenarios passed, {skipped} skipped");
        }
        Ok(())
    } else {
        Err(format!(
            "smoke: {} of {} scenarios failed: {:?}",
            failures.len(),
            SMOKE_SCENARIOS.len(),
            failures
        ))
    }
}

/// Runs QEMU with its stdout (the serial console) captured. Returns once every expected string
/// has appeared, QEMU exits, or `timeout` elapses; QEMU is killed in every case. Yields the
/// captured text and whether all expectations were met.
fn run_qemu_capture(
    cmd: &mut Command,
    timeout: Duration,
    expect: &[&str],
) -> Result<(String, bool), String> {
    use std::io::Read;
    use std::sync::mpsc;

    cmd.stdin(Stdio::null()).stdout(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to start qemu-system-x86_64 (is it installed?): {e}"))?;
    let mut stdout = child.stdout.take().expect("stdout was requested as piped");

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let start = Instant::now();
    let mut out: Vec<u8> = Vec::new();
    let all_seen = |bytes: &[u8]| {
        let text = String::from_utf8_lossy(bytes);
        expect.iter().all(|e| text.contains(e))
    };
    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(chunk) => {
                out.extend_from_slice(&chunk);
                if all_seen(&out) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break; // QEMU exited on its own (e.g. isa-debug-exit, from P0.4 on)
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    while let Ok(chunk) = rx.try_recv() {
        out.extend_from_slice(&chunk);
    }
    let satisfied = all_seen(&out);
    Ok((String::from_utf8_lossy(&out).into_owned(), satisfied))
}

// ---------------------------------------------------------------- docs-gate

/// Exit status when the gate finds violations (Claude Code treats hook exit 2 as "block").
const EXIT_DOCS_GATE: i32 = 2;

/// Compares a checkout against the merge-base with `--base` (default `main`) and fails if code
/// changed without the documentation that must travel with it. Runs in CI on every PR (from the
/// *base branch's* copy of this tool, so a PR cannot weaken the gate it is judged by) and as a
/// Claude Code stop hook (`.claude/settings.json`).
///
/// Options: `--base REF`, `--repo PATH` (checkout to inspect; default: this workspace).
/// The PR body may be supplied in `DOCS_GATE_PR_BODY`; a line `docs-gate: skip — <reason>` with a
/// reason of at least 20 characters skips the gate, and a bare marker is itself a violation.
fn docs_gate(args: &[String]) -> Result<(), String> {
    let mut base = String::from("main");
    let mut repo = workspace_root();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--base" => base = it.next().ok_or("--base needs a git ref")?.clone(),
            "--repo" => repo = PathBuf::from(it.next().ok_or("--repo needs a path")?),
            other => return Err(format!("unknown docs-gate option `{other}`")),
        }
    }

    if let Ok(body) = env::var("DOCS_GATE_PR_BODY") {
        match skip_reason(&body) {
            Ok(Some(reason)) => {
                println!("docs-gate: skipped by PR body — {reason}");
                return Ok(());
            }
            Ok(None) => {}
            Err(msg) => {
                eprintln!("docs-gate: {msg}");
                std::process::exit(EXIT_DOCS_GATE);
            }
        }
    }

    let merge_base = git(&repo, &["merge-base", &base, "HEAD"])?;
    let merge_base = merge_base.trim();
    // Committed and uncommitted changes since the merge-base (with status), plus untracked files.
    let mut changed: Vec<Change> = git(&repo, &["diff", "--name-status", merge_base])?
        .lines()
        .filter_map(Change::parse)
        .collect();
    changed.extend(
        git(&repo, &["ls-files", "--others", "--exclude-standard"])?
            .lines()
            .map(|p| Change {
                status: 'A',
                path: p.to_string(),
            }),
    );
    changed.sort_by(|a, b| a.path.cmp(&b.path));
    changed.dedup_by(|a, b| a.path == b.path);

    // The changelog rule inspects content, not just paths: something must have been added under
    // the Unreleased heading.
    let changelog_grew = {
        let before =
            git(&repo, &["show", &format!("{merge_base}:CHANGELOG.md")]).unwrap_or_default();
        let after = fs::read_to_string(repo.join("CHANGELOG.md")).unwrap_or_default();
        unreleased_grew(&before, &after)
    };

    let violations = docs_gate_violations(&changed, changelog_grew);
    if violations.is_empty() {
        println!(
            "docs-gate: OK ({} changed file(s) since merge-base with {base})",
            changed.len()
        );
        return Ok(());
    }
    for v in &violations {
        eprintln!("docs-gate: {v}");
    }
    eprintln!(
        "docs-gate: documentation must travel with code. Add the missing docs; if a change truly needs none, put `docs-gate: skip — <reason>` in the PR body."
    );
    std::process::exit(EXIT_DOCS_GATE);
}

/// One changed path with its git status letter (`A`dded, `M`odified, `D`eleted, `R`enamed…).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Change {
    status: char,
    path: String,
}

impl Change {
    /// Parses one `git diff --name-status` line (renames report the new path).
    fn parse(line: &str) -> Option<Self> {
        let mut parts = line.split('\t');
        let status = parts.next()?.chars().next()?;
        let path = parts.next_back()?.to_string();
        Some(Change { status, path })
    }

    #[cfg(test)]
    fn m(path: &str) -> Self {
        Change {
            status: 'M',
            path: path.to_string(),
        }
    }
}

/// Finds a `docs-gate: skip` directive in a PR body. `Ok(Some(reason))` when a reason of at least
/// 20 characters follows the marker (after `—`, `-` or `:`), `Err` when the marker appears without
/// one, `Ok(None)` when absent.
fn skip_reason(body: &str) -> Result<Option<String>, String> {
    const MARKER: &str = "docs-gate: skip";
    const SEPARATORS: [char; 5] = [' ', '—', '-', ':', '–'];
    // A directive is a line that *starts* with the marker (after optional list/quote punctuation)
    // and continues with a separator or nothing — never a mention inside prose or code.
    for line in body.lines() {
        let l = line
            .trim()
            .trim_start_matches(['-', '*', '>', ' '])
            .trim_start();
        let Some(rest) = l.strip_prefix(MARKER) else {
            continue;
        };
        if !(rest.is_empty() || rest.starts_with(SEPARATORS)) {
            continue; // e.g. "docs-gate: skipfoo …" is not the directive
        }
        let reason = rest.trim_start_matches(SEPARATORS).trim();
        return if reason.chars().count() >= 20 {
            Ok(Some(reason.to_string()))
        } else {
            Err(format!(
                "`{MARKER}` directive found without a reason (need at least 20 characters after the marker on the same line)"
            ))
        };
    }
    Ok(None)
}

/// Text of the `## [Unreleased]` section: from that heading up to the next `## [` heading.
fn unreleased_section(changelog: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in changelog.lines() {
        if line.starts_with("## [") {
            if inside {
                break;
            }
            inside = line.starts_with("## [Unreleased]");
            continue;
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Whether `after`'s Unreleased section contains at least one non-empty line absent from `before`'s.
fn unreleased_grew(before: &str, after: &str) -> bool {
    let old_text = unreleased_section(before);
    let old: Vec<&str> = old_text.lines().map(str::trim).collect();
    let new_text = unreleased_section(after);
    new_text
        .lines()
        .map(str::trim)
        .any(|l| !l.is_empty() && !l.starts_with("###") && !old.contains(&l))
}

/// Paths whose change means "code changed" for the documentation gate.
const CODE_PREFIXES: &[&str] = &["kernel/", "crates/", "services/", "userland/", "xtask/"];

/// The gate's rules, kept pure so they are unit-testable. Returns one message per violation.
fn docs_gate_violations(changed: &[Change], changelog_grew: bool) -> Vec<String> {
    let has = |p: &str| changed.iter().any(|c| c.path == p);
    let any_under = |prefixes: &[&str]| {
        changed
            .iter()
            .any(|c| prefixes.iter().any(|p| c.path.starts_with(p)))
    };
    let added_under = |prefix: &str| {
        changed
            .iter()
            .any(|c| c.status == 'A' && c.path.starts_with(prefix))
    };
    let mut v = Vec::new();

    if (any_under(CODE_PREFIXES) || has("Cargo.toml")) && !changelog_grew {
        v.push(
            "code changed (kernel/, crates/, services/, userland/, xtask/ or Cargo.toml) but nothing was added under `## [Unreleased]` in CHANGELOG.md".to_string(),
        );
    }
    if any_under(&["xtask/src/"]) && !(has("README.md") || has("CLAUDE.md") || has("docs/PLAN.md"))
    {
        v.push(
            "xtask/src/ changed but none of README.md, CLAUDE.md or docs/PLAN.md changed; at least one must describe the new behaviour".to_string(),
        );
    }
    if any_under(&[".github/workflows/"]) && !(has("docs/PLAN.md") || has("README.md")) {
        v.push(
            ".github/workflows/ changed but neither docs/PLAN.md (section 7) nor README.md was updated".to_string(),
        );
    }
    if has("docs/abi.md") && !added_under("docs/adr/") {
        v.push("docs/abi.md changed without a *new* ADR file under docs/adr/ (editing an old one does not count)".to_string());
    }
    v
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---------------------------------------------------------------- perf

/// Size budget for the release kernel's loaded segments (text + rodata + data + bss).
/// Generous today (the kernel is ~20 KiB); tighten as the design settles.
const KERNEL_LOAD_BYTES_MAX: u64 = 1024 * 1024;
/// Size budget for the bootable ISO (Limine + kernel + config).
const ISO_BYTES_MAX: u64 = 16 * 1024 * 1024;
/// Boot-to-banner budget (QEMU spawn to `CarvOS chisel v` on serial), enforced only under KVM
/// because TCG timings say more about the host than the kernel. OVMF alone costs ~2 s.
const BOOT_TO_BANNER_MAX: Duration = Duration::from_secs(8);
/// Kernel work between the banner and the final "halting" line, enforced under KVM.
const BANNER_TO_HALT_MAX: Duration = Duration::from_secs(2);

/// Loaded-segment sizes from an ELF64 file's program headers.
struct LoadSizes {
    text: u64,
    rodata: u64,
    data: u64,
    bss: u64,
}

impl LoadSizes {
    fn total(&self) -> u64 {
        self.text + self.rodata + self.data + self.bss
    }
}

/// Sums PT_LOAD segments by permission: RX → text, R → rodata, RW file bytes → data, RW
/// memsz beyond filesz → bss.
fn elf64_load_sizes(bytes: &[u8]) -> Result<LoadSizes, String> {
    const PT_LOAD: u32 = 1;
    const PF_X: u32 = 1;
    const PF_W: u32 = 2;
    elf64_entry(bytes)?; // validates the header
    let u16_at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
    let phoff = u64_at(32) as usize;
    let phentsize = u16_at(54) as usize;
    let phnum = u16_at(56) as usize;
    if phentsize < 56 || phoff + phnum * phentsize > bytes.len() {
        return Err("kernel ELF program headers are out of bounds".into());
    }
    let mut sizes = LoadSizes {
        text: 0,
        rodata: 0,
        data: 0,
        bss: 0,
    };
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        if u32_at(ph) != PT_LOAD {
            continue;
        }
        let flags = u32_at(ph + 4);
        let filesz = u64_at(ph + 32);
        let memsz = u64_at(ph + 40);
        if flags & PF_X != 0 {
            sizes.text += memsz;
        } else if flags & PF_W != 0 {
            sizes.data += filesz;
            sizes.bss += memsz.saturating_sub(filesz);
        } else {
            sizes.rodata += memsz;
        }
    }
    Ok(sizes)
}

/// Boot timing of one QEMU run: when the banner appeared and when the kernel reached "halting".
struct BootTiming {
    banner_after: Duration,
    halt_after_banner: Option<Duration>,
}

/// Measures release-kernel and ISO size plus boot timings (median of `--runs`, default 3) and
/// checks them against the budgets above. Writes `target/perf/report.md`, appends it to
/// `$GITHUB_STEP_SUMMARY` when set, and fails if any enforced budget is exceeded.
fn perf(args: &[String]) -> Result<(), String> {
    let mut runs = 3usize;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--runs" => {
                runs = it
                    .next()
                    .ok_or("--runs needs a number")?
                    .parse()
                    .map_err(|e| format!("bad --runs value: {e}"))?;
                if runs == 0 {
                    return Err("--runs must be at least 1".into());
                }
            }
            other => return Err(format!("unknown perf option `{other}`")),
        }
    }

    let iso = build_image(true, "")?;
    let kernel = target_dir()
        .join(KERNEL_TARGET)
        .join("release")
        .join(KERNEL_PACKAGE);
    let kernel_bytes =
        fs::read(&kernel).map_err(|e| format!("reading {}: {e}", kernel.display()))?;
    let sizes = elf64_load_sizes(&kernel_bytes)?;
    let iso_bytes = fs::metadata(&iso)
        .map_err(|e| format!("stat {}: {e}", iso.display()))?
        .len();
    let kvm = kvm_usable();

    let mut boot_ms: Vec<u128> = Vec::new();
    let mut work_ms: Vec<u128> = Vec::new();
    for _ in 0..runs {
        let opts = RunOpts {
            release: true,
            bios: false,
            debug: false,
            timeout: Some(BOOT_TO_BANNER_MAX * 4),
            cmdline: String::new(),
            iso: None,
        };
        let mut cmd = qemu_command(&iso, &opts)?;
        let (out, timing) = boot_timing(
            &mut cmd,
            BOOT_TO_BANNER_MAX * 4,
            "CarvOS chisel v",
            "halting",
        )?;
        let Some(t) = timing else {
            return Err(format!(
                "perf: banner never appeared within {:?}; serial:\n{out}",
                BOOT_TO_BANNER_MAX * 4
            ));
        };
        boot_ms.push(t.banner_after.as_millis());
        if let Some(w) = t.halt_after_banner {
            work_ms.push(w.as_millis());
        }
    }
    boot_ms.sort_unstable();
    work_ms.sort_unstable();
    let boot_med = boot_ms[boot_ms.len() / 2];
    let work_med = work_ms.get(work_ms.len() / 2).copied();

    let kib = |b: u64| format!("{:.1} KiB", b as f64 / 1024.0);
    let accel = if kvm { "KVM" } else { "TCG, not enforced" };
    // (metric, value, budget, within budget)
    let rows: Vec<(String, String, String, bool)> = vec![
        (
            "kernel loaded size (release)".into(),
            format!(
                "{} (text {}, rodata {}, data {}, bss {})",
                kib(sizes.total()),
                kib(sizes.text),
                kib(sizes.rodata),
                kib(sizes.data),
                kib(sizes.bss)
            ),
            format!("≤ {}", kib(KERNEL_LOAD_BYTES_MAX)),
            sizes.total() <= KERNEL_LOAD_BYTES_MAX,
        ),
        (
            "ISO size".into(),
            format!("{:.2} MiB", iso_bytes as f64 / (1024.0 * 1024.0)),
            format!("≤ {} MiB", ISO_BYTES_MAX >> 20),
            iso_bytes <= ISO_BYTES_MAX,
        ),
        (
            format!("boot to banner, median of {runs} ({accel})"),
            format!("{boot_med} ms"),
            format!("≤ {} ms", BOOT_TO_BANNER_MAX.as_millis()),
            !kvm || boot_med <= BOOT_TO_BANNER_MAX.as_millis(),
        ),
        (
            format!("banner to halt ({accel})"),
            work_med
                .map(|w| format!("{w} ms"))
                .unwrap_or_else(|| "n/a".into()),
            format!("≤ {} ms", BANNER_TO_HALT_MAX.as_millis()),
            !kvm || work_med.is_none_or(|w| w <= BANNER_TO_HALT_MAX.as_millis()),
        ),
    ];

    let mut md = String::from(
        "### CarvOS performance check\n\n| Metric | Value | Budget | |\n|---|---|---|---|\n",
    );
    for (m, v, b, ok) in &rows {
        let mark = if *ok { "✅" } else { "❌ over budget" };
        md.push_str(&format!("| {m} | {v} | {b} | {mark} |\n"));
    }
    let dir = target_dir().join("perf");
    fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    fs::write(dir.join("report.md"), &md).map_err(|e| format!("writing report: {e}"))?;
    print!("{md}");
    if let Ok(summary) = env::var("GITHUB_STEP_SUMMARY") {
        use std::io::Write;
        if let Ok(mut f) = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(summary)
        {
            let _ = f.write_all(md.as_bytes());
        }
    }

    let over: Vec<&str> = rows.iter().filter(|r| !r.3).map(|r| r.0.as_str()).collect();
    if over.is_empty() {
        println!("perf: all budgets met");
        Ok(())
    } else {
        Err(format!("perf: over budget: {over:?}"))
    }
}

/// Runs QEMU with the serial console captured and records when `banner` and then `halt`
/// appear. Returns the serial text and the timing (None if the banner never appeared).
fn boot_timing(
    cmd: &mut Command,
    timeout: Duration,
    banner: &str,
    halt: &str,
) -> Result<(String, Option<BootTiming>), String> {
    use std::io::Read;
    use std::sync::mpsc;

    cmd.stdin(Stdio::null()).stdout(Stdio::piped());
    let start = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to start qemu-system-x86_64: {e}"))?;
    let mut stdout = child.stdout.take().expect("stdout was requested as piped");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut out: Vec<u8> = Vec::new();
    let mut banner_at: Option<Instant> = None;
    let mut halt_at: Option<Instant> = None;
    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(chunk) => {
                out.extend_from_slice(&chunk);
                let text = String::from_utf8_lossy(&out);
                if banner_at.is_none() && text.contains(banner) {
                    banner_at = Some(Instant::now());
                }
                if banner_at.is_some() && text.contains(halt) {
                    halt_at = Some(Instant::now());
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let text = String::from_utf8_lossy(&out).into_owned();
    let timing = banner_at.map(|b| BootTiming {
        banner_after: b.duration_since(start),
        halt_after_banner: halt_at.map(|h| h.duration_since(b)),
    });
    Ok((text, timing))
}

// ---------------------------------------------------------------- test

/// `isa-debug-exit` turns a written byte `v` into QEMU exit status `(v << 1) | 1`; these match
/// `QemuExitCode` in `kernel/src/test.rs`.
const QEMU_EXIT_TESTS_PASSED: i32 = (0x10 << 1) | 1;
const QEMU_EXIT_TESTS_FAILED: i32 = (0x11 << 1) | 1;

struct TestOpts {
    host: bool,
    kernel: bool,
    integration: bool,
    release: bool,
    timeout: Duration,
}

fn parse_test_opts(args: &[String]) -> Result<TestOpts, String> {
    let mut o = TestOpts {
        host: false,
        kernel: false,
        integration: false,
        release: false,
        timeout: Duration::from_secs(120),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--host" => o.host = true,
            "--kernel" => o.kernel = true,
            "--integration" => o.integration = true,
            "--release" => o.release = true,
            "--timeout" => {
                let secs = it
                    .next()
                    .ok_or("--timeout needs a value in seconds")?
                    .parse::<u64>()
                    .map_err(|e| format!("bad --timeout value: {e}"))?;
                o.timeout = Duration::from_secs(secs);
            }
            other => return Err(format!("unknown test option `{other}`")),
        }
    }
    if !(o.host || o.kernel || o.integration) {
        o.host = true;
        o.kernel = true;
        o.integration = true;
    }
    Ok(o)
}

/// The one command every PR must pass. Layers (docs/PLAN.md §6): 1 host unit tests,
/// 2 in-kernel tests booted in QEMU, 3 boot smoke scenarios. Prints a summary and fails if any
/// selected layer failed.
fn test(args: &[String]) -> Result<(), String> {
    let opts = parse_test_opts(args)?;
    let mut results: Vec<(&str, Result<String, String>)> = Vec::new();

    if opts.host {
        results.push(("host unit tests", host_tests()));
    }
    if opts.kernel {
        results.push(("in-kernel tests (QEMU)", kernel_tests(&opts)));
    }
    if opts.integration {
        results.push((
            "boot smoke (UEFI, BIOS, panic, double fault, page fault, stack overflow)",
            run_smoke(opts.release, opts.timeout, None).map(|()| "6 scenarios passed".to_string()),
        ));
    }

    println!("\ntest summary:");
    let mut failed = 0;
    for (name, r) in &results {
        match r {
            Ok(msg) => println!("  PASS  {name}: {msg}"),
            Err(msg) => {
                failed += 1;
                println!("  FAIL  {name}: {msg}");
            }
        }
    }
    if failed == 0 {
        Ok(())
    } else {
        Err(format!(
            "{failed} of {} test layer(s) failed",
            results.len()
        ))
    }
}

/// Layer 1: `cargo test` for every workspace crate except the kernel (which has no host target).
fn host_tests() -> Result<String, String> {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(workspace_root())
        .args([
            "test",
            "--workspace",
            "--exclude",
            KERNEL_PACKAGE,
            "--locked",
        ])
        .status()
        .map_err(|e| format!("failed to run cargo: {e}"))?;
    if status.success() {
        Ok("passed".into())
    } else {
        Err("cargo test failed".into())
    }
}

/// Layer 2: build the kernel's test binary, boot it in QEMU, and read the `isa-debug-exit` code.
fn kernel_tests(opts: &TestOpts) -> Result<String, String> {
    let exe = kernel_test_executable(opts.release)?;
    let out_dir = target_dir().join("test");
    fs::create_dir_all(&out_dir).map_err(|e| format!("creating {}: {e}", out_dir.display()))?;
    let iso = assemble_iso(&exe, "", &out_dir, "carv-os-test.iso", opts.release)?;
    let run_opts = RunOpts {
        release: opts.release,
        bios: false,
        debug: false,
        timeout: Some(opts.timeout),
        cmdline: String::new(),
        iso: None,
    };
    let mut cmd = qemu_command(&iso, &run_opts)?;
    let (output, status) = run_qemu_until_exit(&mut cmd, opts.timeout)?;
    let log = out_dir.join("kernel-tests.log");
    fs::write(&log, &output).map_err(|e| format!("writing {}: {e}", log.display()))?;

    let summary = output
        .lines()
        .rev()
        .find(|l| l.starts_with("chisel-test:"))
        .unwrap_or("")
        .trim()
        .to_string();
    match status.and_then(|s| s.code()) {
        Some(QEMU_EXIT_TESTS_PASSED) => Ok(format!("{summary} (log: {})", log.display())),
        Some(QEMU_EXIT_TESTS_FAILED) => Err(format!(
            "a kernel test failed (log: {}):\n{}",
            log.display(),
            tail(&output, 12)
        )),
        Some(code) => Err(format!(
            "QEMU exited with unexpected status {code} (log: {}):\n{}",
            log.display(),
            tail(&output, 12)
        )),
        None => Err(format!(
            "kernel tests did not finish within {:?} (log: {}):\n{}",
            opts.timeout,
            log.display(),
            tail(&output, 12)
        )),
    }
}

/// Builds the kernel test binary with `cargo test --no-run` and returns its path from cargo's
/// JSON messages.
fn kernel_test_executable(release: bool) -> Result<PathBuf, String> {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(workspace_root()).args([
        "test",
        "--package",
        KERNEL_PACKAGE,
        "--target",
        KERNEL_TARGET,
        "--locked",
        "--no-run",
        "--message-format=json",
    ]);
    if release {
        cmd.arg("--release");
    }
    let out = cmd
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("failed to run cargo: {e}"))?;
    if !out.status.success() {
        return Err("building the kernel test binary failed".into());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    test_executable_from_messages(&stdout, KERNEL_PACKAGE)
        .ok_or_else(|| "cargo did not report a test executable for the kernel".to_string())
}

/// Finds the test executable for `package` in cargo's `--message-format=json` output (one JSON
/// object per line). Uses a real JSON parser, so paths containing quotes or backslashes (Windows)
/// come back exactly as cargo wrote them.
fn test_executable_from_messages(messages: &str, package: &str) -> Option<PathBuf> {
    messages
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|m| m["reason"] == "compiler-artifact")
        .filter(|m| m["target"]["name"] == package)
        .filter(|m| m["profile"]["test"] == true)
        .filter_map(|m| m["executable"].as_str().map(PathBuf::from))
        .next()
}

/// Joins a tool's captured stdout and stderr for an error message: both trimmed of surrounding
/// whitespace, separated by exactly one newline when both are non-empty.
fn join_tool_output(stdout: &str, stderr: &str) -> String {
    let (out, err) = (stdout.trim(), stderr.trim());
    match (out.is_empty(), err.is_empty()) {
        (true, true) => String::new(),
        (false, true) => out.to_string(),
        (true, false) => err.to_string(),
        (false, false) => format!("{out}\n{err}"),
    }
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// Runs QEMU with the serial console captured until it exits on its own or `timeout` elapses
/// (then it is killed). Returns the serial text and the exit status (None on timeout).
fn run_qemu_until_exit(
    cmd: &mut Command,
    timeout: Duration,
) -> Result<(String, Option<std::process::ExitStatus>), String> {
    use std::io::Read;
    use std::sync::mpsc;

    cmd.stdin(Stdio::null()).stdout(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to start qemu-system-x86_64: {e}"))?;
    let mut stdout = child.stdout.take().expect("stdout was requested as piped");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let start = Instant::now();
    let mut out: Vec<u8> = Vec::new();
    let status = loop {
        if let Some(s) = child
            .try_wait()
            .map_err(|e| format!("waiting for qemu: {e}"))?
        {
            break Some(s);
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => out.extend_from_slice(&chunk),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Some(child.wait().map_err(|e| format!("waiting for qemu: {e}"))?);
            }
        }
    };
    while let Ok(chunk) = rx.try_recv() {
        out.extend_from_slice(&chunk);
    }
    Ok((String::from_utf8_lossy(&out).into_owned(), status))
}

// ---------------------------------------------------------------- release-check

/// Verifies release consistency: the tag is `v<version>` (optionally `-<pre-release>`), where
/// `<version>` is `[workspace.package] version`, and `CHANGELOG.md` has a `## [<version>]`
/// section. Prints `version=` and `prerelease=`, also appending them to `$GITHUB_OUTPUT`.
fn release_check(args: &[String]) -> Result<(), String> {
    let mut tag: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--tag" => tag = Some(it.next().ok_or("--tag needs a value")?.clone()),
            other => return Err(format!("unknown release-check option `{other}`")),
        }
    }
    let tag = tag
        .or_else(|| env::var("GITHUB_REF_NAME").ok())
        .ok_or("release-check needs --tag vX.Y.Z (or GITHUB_REF_NAME)")?;

    let manifest = fs::read_to_string(workspace_root().join("Cargo.toml"))
        .map_err(|e| format!("reading Cargo.toml: {e}"))?;
    let version =
        workspace_version(&manifest).ok_or("no [workspace.package] version in Cargo.toml")?;
    let changelog = fs::read_to_string(workspace_root().join("CHANGELOG.md"))
        .map_err(|e| format!("reading CHANGELOG.md: {e}"))?;

    let info = check_release(&tag, &version, &changelog)?;
    let out = format!("version={version}\nprerelease={}\n", info.prerelease);
    print!("{out}");
    // The workflow decides --prerelease from this output, so failing to write it is an error,
    // not a warning (#26).
    if let Ok(path) = env::var("GITHUB_OUTPUT") {
        append_github_output(Path::new(&path), &out)?;
    }
    println!("release-check: OK ({tag} ↔ {version}, changelog section present)");
    Ok(())
}

struct ReleaseInfo {
    prerelease: bool,
}

/// Pure rule check behind `release_check`, unit-tested.
fn check_release(tag: &str, version: &str, changelog: &str) -> Result<ReleaseInfo, String> {
    let rest = tag
        .strip_prefix('v')
        .ok_or_else(|| format!("tag `{tag}` must start with `v`"))?;
    let (base, pre) = match rest.split_once('-') {
        Some((b, p)) => (b, Some(p)),
        None => (rest, None),
    };
    if base != version {
        return Err(format!(
            "tag `{tag}` does not match [workspace.package] version {version}; bump Cargo.toml in the release PR"
        ));
    }
    if let Some(p) = pre
        && (p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '.'))
    {
        return Err(format!(
            "pre-release suffix `{p}` must be alphanumeric/dots (e.g. rc.1)"
        ));
    }
    let heading = format!("## [{version}]");
    if !changelog.lines().any(|l| l.starts_with(&heading)) {
        return Err(format!(
            "CHANGELOG.md has no `{heading} - YYYY-MM-DD` section; move the Unreleased entries under it"
        ));
    }
    // 0.y.z is pre-1.0 and always published as a GitHub pre-release; so is any -suffix tag.
    let prerelease = pre.is_some() || version.starts_with("0.");
    Ok(ReleaseInfo { prerelease })
}

/// Removes one matching pair of basic (`"`) or literal (`'`) string delimiters, nothing more, so
/// a quote character inside the value survives.
fn strip_toml_quotes(v: &str) -> &str {
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return &v[1..v.len() - 1];
        }
    }
    v
}

/// Appends `text` to the `GITHUB_OUTPUT` file at `path`, failing loudly: the release workflow
/// decides `--prerelease` from this output, so a silent write failure must not pass (#26).
fn append_github_output(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|e| format!("opening GITHUB_OUTPUT ({}): {e}", path.display()))?;
    f.write_all(text.as_bytes())
        .map_err(|e| format!("writing GITHUB_OUTPUT ({}): {e}", path.display()))
}

/// Extracts `version = "..."` from the `[workspace.package]` table of a Cargo.toml. Trailing
/// `# comments` on the table header or the value are ignored (#27).
fn workspace_version(manifest: &str) -> Option<String> {
    /// Cuts a trailing `# comment`, but not a `#` inside a basic or literal string (#31).
    fn strip_comment(l: &str) -> &str {
        let mut quote: Option<char> = None;
        let mut escaped = false;
        for (i, c) in l.char_indices() {
            match quote {
                Some('"') if escaped => escaped = false,
                Some('"') if c == '\\' => escaped = true,
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None if c == '"' || c == '\'' => quote = Some(c),
                None if c == '#' => return l[..i].trim(),
                None => {}
            }
        }
        l.trim()
    }
    let mut in_table = false;
    for line in manifest.lines() {
        let t = strip_comment(line);
        if t.starts_with('[') {
            in_table = t == "[workspace.package]";
            continue;
        }
        if in_table && let Some(rest) = t.strip_prefix("version") {
            let rest = rest.trim_start();
            if let Some(v) = rest.strip_prefix('=') {
                // Basic ("...") or literal ('...') string; strip either kind of quote.
                return Some(strip_toml_quotes(v.trim()).to_string());
            }
        }
    }
    None
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
        let conf = limine_conf("");
        assert!(conf.contains("protocol: limine"));
        assert!(conf.contains(&format!("path: boot():/boot/{KERNEL_PACKAGE}")));
        assert!(conf.contains("serial: yes"));
        assert!(!conf.contains("cmdline:"));
        assert!(limine_conf("panic-test").contains("    cmdline: panic-test\n"));
    }

    #[test]
    fn elf_load_sizes_by_permission() {
        // Header + two program headers: RX text (memsz 0x1000) and RW data (filesz 0x100, memsz 0x300).
        let mut h = header(HIGHER_HALF_BASE);
        h[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        h[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        h[56..58].copy_from_slice(&2u16.to_le_bytes()); // e_phnum
        let ph = |ptype: u32, flags: u32, filesz: u64, memsz: u64| {
            let mut p = vec![0u8; 56];
            p[0..4].copy_from_slice(&ptype.to_le_bytes());
            p[4..8].copy_from_slice(&flags.to_le_bytes());
            p[32..40].copy_from_slice(&filesz.to_le_bytes());
            p[40..48].copy_from_slice(&memsz.to_le_bytes());
            p
        };
        h.extend(ph(1, 5, 0x1000, 0x1000));
        h.extend(ph(1, 6, 0x100, 0x300));
        let s = elf64_load_sizes(&h).unwrap();
        assert_eq!((s.text, s.rodata, s.data, s.bss), (0x1000, 0, 0x100, 0x200));
        assert_eq!(s.total(), 0x1300);
    }

    #[test]
    fn docs_gate_rules() {
        let v = |files: &[&str], grew: bool| {
            docs_gate_violations(
                &files.iter().map(|f| Change::m(f)).collect::<Vec<_>>(),
                grew,
            )
        };
        assert_eq!(
            v(&["kernel/src/main.rs", "CHANGELOG.md"], false).len(),
            1,
            "changelog touched but Unreleased did not grow"
        );
        assert!(v(&["kernel/src/main.rs", "CHANGELOG.md"], true).is_empty());
        assert_eq!(v(&["Cargo.toml"], false).len(), 1);
        assert_eq!(
            v(&["xtask/src/main.rs", "CHANGELOG.md"], true).len(),
            1,
            "xtask without README/CLAUDE/PLAN"
        );
        assert!(v(&["xtask/src/main.rs", "CHANGELOG.md", "README.md"], true).is_empty());
        assert_eq!(v(&[".github/workflows/ci.yml"], true).len(), 1);
        assert!(v(&[".github/workflows/ci.yml", "docs/PLAN.md"], true).is_empty());
        assert_eq!(
            v(&["docs/abi.md", "docs/adr/0001-old.md"], true).len(),
            1,
            "modified ADR does not count"
        );
        let added_adr = vec![
            Change::m("docs/abi.md"),
            Change {
                status: 'A',
                path: "docs/adr/0002-abi-v1.md".into(),
            },
        ];
        assert!(docs_gate_violations(&added_adr, true).is_empty());
        assert!(
            v(&["README.md", "docs/PLAN.md"], false).is_empty(),
            "docs-only change"
        );
        assert!(v(&[], false).is_empty());
    }

    #[test]
    fn changelog_unreleased_growth_is_content_based() {
        let before = "# C\n\n## [Unreleased]\n\n### Added\n- old line\n\n## [0.1.0] - 2026-09-27\n- released\n";
        let grown = "# C\n\n## [Unreleased]\n\n### Added\n- old line\n- new line\n\n## [0.1.0] - 2026-09-27\n- released\n";
        let only_history = "# C\n\n## [Unreleased]\n\n### Added\n- old line\n\n## [0.1.0] - 2026-09-27\n- released\n- edited history\n";
        assert!(!unreleased_grew(before, before));
        assert!(unreleased_grew(before, grown));
        assert!(
            !unreleased_grew(before, only_history),
            "touching a released section must not count"
        );
        assert!(unreleased_grew("", "## [Unreleased]\n- first\n"));
    }

    #[test]
    fn skip_marker_requires_a_reason() {
        assert_eq!(skip_reason("normal PR body").unwrap(), None);
        assert!(skip_reason("docs-gate: skip").is_err(), "bare directive");
        assert_eq!(
            skip_reason("please docs-gate: skip").unwrap(),
            None,
            "mid-line mention is not a directive"
        );
        assert!(
            skip_reason("docs-gate: skip — short").is_err(),
            "reason too short"
        );
        assert_eq!(
            skip_reason(
                "x\ndocs-gate: skip — internal refactor of a helper, no behaviour change\ny"
            )
            .unwrap()
            .as_deref(),
            Some("internal refactor of a helper, no behaviour change")
        );
        assert!(
            skip_reason("docs-gate: skip - hyphen separator also works fine here")
                .unwrap()
                .is_some()
        );
        // Not directives: a mention inside prose, or the marker glued to other text.
        assert_eq!(
            skip_reason("write `docs-gate: skip — reason` in the PR body if truly needed").unwrap(),
            None
        );
        assert_eq!(
            skip_reason("docs-gate: skipfoo this is a long sentence that is not it").unwrap(),
            None
        );
        assert!(
            skip_reason("- docs-gate: skip — list-item directive with a proper reason")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn name_status_lines_parse() {
        assert_eq!(Change::parse("M\tsrc/a.rs"), Some(Change::m("src/a.rs")));
        assert_eq!(
            Change::parse("A\tdocs/adr/0002.md"),
            Some(Change {
                status: 'A',
                path: "docs/adr/0002.md".into()
            })
        );
        assert_eq!(
            Change::parse("R100\told.rs\tnew.rs"),
            Some(Change {
                status: 'R',
                path: "new.rs".into()
            })
        );
        assert_eq!(Change::parse(""), None);
    }

    #[test]
    fn github_output_write_failures_are_errors() {
        let dir = std::env::temp_dir().join(format!("carv-xtask-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A directory is not writable as a file: must fail, not silently succeed (#26).
        assert!(append_github_output(&dir, "version=1\n").is_err());
        let file = dir.join("out");
        append_github_output(&file, "a=1\n").unwrap();
        append_github_output(&file, "b=2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a=1\nb=2\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toml_quote_stripping_keeps_inner_quotes() {
        assert_eq!(strip_toml_quotes("\"0.1.0\""), "0.1.0");
        assert_eq!(strip_toml_quotes("'0.1.0\"'"), "0.1.0\"");
        assert_eq!(strip_toml_quotes("\"a'b\""), "a'b");
        assert_eq!(strip_toml_quotes("plain"), "plain");
        assert_eq!(strip_toml_quotes("\""), "\"");
    }

    #[test]
    fn test_executable_parsed_from_cargo_json() {
        let msgs = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"limine","kind":["lib"]},"profile":{"test":false},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"chisel","kind":["bin"]},"profile":{"test":false},"executable":"/t/debug/chisel"}"#,
            "\n",
            r#"not json at all"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"chisel","kind":["bin"]},"profile":{"test":true},"executable":"C:\\tar\"get\\deps\\chisel-ab12"}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
            "\n",
        );
        let exe = test_executable_from_messages(msgs, "chisel").unwrap();
        // Escaped backslashes and the escaped quote survive intact (issues #13, #14).
        assert_eq!(exe.to_str().unwrap(), r#"C:\tar"get\deps\chisel-ab12"#);
        assert!(test_executable_from_messages(msgs, "xtask").is_none());
        assert!(test_executable_from_messages("", "chisel").is_none());
    }

    #[test]
    fn test_opts_default_to_all_layers() {
        let o = parse_test_opts(&[]).unwrap();
        assert!(o.host && o.kernel && o.integration && !o.release);
        let k = parse_test_opts(&["--kernel".into(), "--timeout".into(), "9".into()]).unwrap();
        assert!(k.kernel && !k.host && !k.integration);
        assert_eq!(k.timeout, Duration::from_secs(9));
        assert!(parse_test_opts(&["--nope".into()]).is_err());
    }

    #[test]
    fn qemu_exit_codes_match_kernel() {
        assert_eq!(QEMU_EXIT_TESTS_PASSED, 33);
        assert_eq!(QEMU_EXIT_TESTS_FAILED, 35);
    }

    #[test]
    fn release_rules() {
        let cl = "# Changelog\n\n## [Unreleased]\n\n## [0.1.0] - 2026-09-27\n### Added\n- stuff\n";
        // 0.y.z is pre-1.0, so it is always published as a pre-release.
        assert!(check_release("v0.1.0", "0.1.0", cl).unwrap().prerelease);
        assert!(
            check_release("v0.1.0-rc.1", "0.1.0", cl)
                .unwrap()
                .prerelease
        );
        assert!(
            check_release("v0.2.0", "0.1.0", cl).is_err(),
            "tag/version mismatch"
        );
        assert!(check_release("0.1.0", "0.1.0", cl).is_err(), "missing v");
        assert!(
            check_release("v0.1.0", "0.1.0", "## [Unreleased]\n").is_err(),
            "no changelog section"
        );
        assert!(
            check_release("v0.1.0-", "0.1.0", cl).is_err(),
            "empty suffix"
        );
        assert!(
            check_release("v0.1.0-rc 1", "0.1.0", cl).is_err(),
            "bad suffix"
        );
        assert!(
            !check_release("v1.2.3", "1.2.3", "## [1.2.3] - 2027-01-01\n")
                .unwrap()
                .prerelease
        );
    }

    #[test]
    fn workspace_version_parsed() {
        let m = "[workspace]\nmembers = [\"a\"]\n\n[workspace.package]\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[profile.dev]\npanic = \"abort\"\n";
        assert_eq!(workspace_version(m).as_deref(), Some("0.1.0"));
        assert_eq!(workspace_version("[package]\nversion = \"9.9.9\"\n"), None);
        // Comments after the header or the value are idiomatic TOML (#27).
        let c = "[workspace.package]  # shared metadata\nversion = \"0.2.0\" # bump per release\n";
        assert_eq!(workspace_version(c).as_deref(), Some("0.2.0"));
        assert_eq!(
            workspace_version("# [workspace.package]\nversion = \"1\"\n"),
            None
        );
        // `#` inside a quoted value is part of the value, not a comment (#31).
        let q = "[workspace.package]\nversion = \"0.1.0#build\" # trailing\n";
        assert_eq!(workspace_version(q).as_deref(), Some("0.1.0#build"));
        let lit = "[workspace.package]\nversion = '0.1.0#x'\n";
        assert_eq!(workspace_version(lit).as_deref(), Some("0.1.0#x"));
    }

    #[test]
    fn iso_timestamps_are_utc_civil() {
        assert_eq!(utc_civil(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(utc_civil(951_782_400), (2000, 2, 29, 0, 0, 0)); // leap day
        assert_eq!(iso_timestamp(1_790_493_059), "20260927071059");
        assert_eq!(iso_timestamp(4_102_444_799), "20991231235959");
    }

    #[test]
    fn tool_output_joined_with_single_separator() {
        assert_eq!(join_tool_output("hello\n", "\nerror\n"), "hello\nerror");
        assert_eq!(join_tool_output("a\nb\n", ""), "a\nb");
        assert_eq!(join_tool_output("  \n", "err"), "err");
        assert_eq!(join_tool_output("", "\n\n"), "");
    }

    #[test]
    fn run_opts_parse() {
        let o = parse_run_opts(&["--bios".into(), "--timeout".into(), "7".into()]).unwrap();
        assert!(o.bios && !o.release && !o.debug);
        assert!(o.cmdline.is_empty());
        let c = parse_run_opts(&["--cmdline".into(), "a b".into()]).unwrap();
        assert_eq!(c.cmdline, "a b");
        assert_eq!(o.timeout, Some(Duration::from_secs(7)));
        assert!(parse_run_opts(&["--timeout".into()]).is_err());
        assert!(parse_run_opts(&["--bogus".into()]).is_err());
    }
}
