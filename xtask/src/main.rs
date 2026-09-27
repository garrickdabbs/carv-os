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
//! - `docs-gate [--base REF]`: fail when code changed without matching documentation changes.
//! - `perf [--runs N]`: measure kernel/ISO size and boot-to-banner time against budgets.
//!
//! `test` arrives in P0.4.

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
    println!(
        "  docs-gate [--base REF]           Fail if code changed without documentation (default base: main)"
    );
    println!(
        "  perf [--runs N]                  Kernel/ISO size and boot timings vs budgets (report in target/perf/)"
    );
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
}

fn parse_run_opts(args: &[String]) -> Result<RunOpts, String> {
    let mut opts = RunOpts {
        release: false,
        bios: false,
        debug: false,
        timeout: None,
        cmdline: String::new(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--release" => opts.release = true,
            "--bios" => opts.bios = true,
            "--debug" => opts.debug = true,
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
    let iso = build_image(opts.release, &opts.cmdline)?;
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
];

/// Boots every [`SMOKE_SCENARIOS`] entry in QEMU and checks the serial output. Each run stops as
/// soon as all expected strings have appeared, so a passing suite takes seconds. Serial logs are
/// written to `target/smoke/<name>.log` for CI to upload.
fn smoke(args: &[String]) -> Result<(), String> {
    let mut release = false;
    let mut timeout = Duration::from_secs(60);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--release" => release = true,
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

    let log_dir = target_dir().join("smoke");
    fs::create_dir_all(&log_dir).map_err(|e| format!("creating {}: {e}", log_dir.display()))?;

    let mut failures = Vec::new();
    for sc in SMOKE_SCENARIOS {
        let iso = build_image(release, sc.cmdline)?;
        let opts = RunOpts {
            release,
            bios: sc.bios,
            debug: false,
            timeout: Some(timeout),
            cmdline: sc.cmdline.to_string(),
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
        println!("smoke: all {} scenarios passed", SMOKE_SCENARIOS.len());
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

/// Paths whose change means "code changed" for the documentation gate.
const CODE_PREFIXES: &[&str] = &["kernel/", "crates/", "services/", "userland/", "xtask/"];

/// Exit status when the gate finds violations (Claude Code treats hook exit 2 as "block").
const EXIT_DOCS_GATE: i32 = 2;

/// Compares the working tree against the merge-base with `--base` (default `main`) and fails if
/// code changed without the documentation that must travel with it. Runs in CI on every PR and
/// as a Claude Code stop hook (`.claude/settings.json`).
fn docs_gate(args: &[String]) -> Result<(), String> {
    let mut base = String::from("main");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--base" => base = it.next().ok_or("--base needs a git ref")?.clone(),
            other => return Err(format!("unknown docs-gate option `{other}`")),
        }
    }

    let root = workspace_root();
    let merge_base = git(&root, &["merge-base", &base, "HEAD"])?;
    let merge_base = merge_base.trim();
    // Committed and uncommitted changes since the merge-base, plus untracked files.
    let mut changed: Vec<String> = git(&root, &["diff", "--name-only", merge_base])?
        .lines()
        .map(str::to_string)
        .collect();
    changed.extend(
        git(&root, &["ls-files", "--others", "--exclude-standard"])?
            .lines()
            .map(str::to_string),
    );
    changed.sort();
    changed.dedup();

    let violations = docs_gate_violations(&changed);
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
        "docs-gate: documentation must travel with code. Add the missing docs; if a change truly needs none, put `docs-gate: skip` and the reason in the PR body."
    );
    std::process::exit(EXIT_DOCS_GATE);
}

/// The gate's rules, kept pure so they are unit-testable. Returns one message per violation.
fn docs_gate_violations(changed: &[String]) -> Vec<String> {
    let has = |p: &str| changed.iter().any(|c| c == p);
    let any_under = |prefixes: &[&str]| {
        changed
            .iter()
            .any(|c| prefixes.iter().any(|p| c.starts_with(p)))
    };
    let mut v = Vec::new();

    if (any_under(CODE_PREFIXES) || has("Cargo.toml")) && !has("CHANGELOG.md") {
        v.push(
            "code changed (kernel/, crates/, services/, userland/, xtask/ or Cargo.toml) but CHANGELOG.md was not updated; add a line under Unreleased".to_string(),
        );
    }
    if any_under(&["xtask/src/"]) && !(has("README.md") || has("CLAUDE.md") || has("docs/PLAN.md"))
    {
        v.push(
            "xtask/src/ changed but README.md, CLAUDE.md and docs/PLAN.md were not; the command reference must match the tool".to_string(),
        );
    }
    if any_under(&[".github/workflows/"]) && !(has("docs/PLAN.md") || has("README.md")) {
        v.push(
            ".github/workflows/ changed but neither docs/PLAN.md (section 7) nor README.md was updated".to_string(),
        );
    }
    if has("docs/abi.md") && !any_under(&["docs/adr/"]) {
        v.push("docs/abi.md changed without a new ADR in docs/adr/".to_string());
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
        let v = |files: &[&str]| {
            docs_gate_violations(&files.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        assert!(
            v(&["kernel/src/main.rs"]).len() == 1,
            "code without changelog"
        );
        assert!(v(&["kernel/src/main.rs", "CHANGELOG.md"]).is_empty());
        assert!(v(&["Cargo.toml"]).len() == 1);
        assert!(
            v(&["xtask/src/main.rs", "CHANGELOG.md"]).len() == 1,
            "xtask without README"
        );
        assert!(v(&["xtask/src/main.rs", "CHANGELOG.md", "README.md"]).is_empty());
        assert!(v(&[".github/workflows/ci.yml"]).len() == 1);
        assert!(v(&[".github/workflows/ci.yml", "docs/PLAN.md"]).is_empty());
        assert!(v(&["docs/abi.md"]).len() == 1);
        assert!(v(&["docs/abi.md", "docs/adr/0002-abi-v1.md"]).is_empty());
        assert!(
            v(&["README.md", "docs/PLAN.md"]).is_empty(),
            "docs-only change"
        );
        assert!(v(&[]).is_empty());
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
