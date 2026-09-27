# CarvOS

[![Build](https://github.com/garrickdabbs/carv-os/actions/workflows/build.yml/badge.svg?branch=main)](https://github.com/garrickdabbs/carv-os/actions/workflows/build.yml)
[![CI](https://github.com/garrickdabbs/carv-os/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/garrickdabbs/carv-os/actions/workflows/ci.yml)
[![Security](https://github.com/garrickdabbs/carv-os/actions/workflows/security.yml/badge.svg?branch=main)](https://github.com/garrickdabbs/carv-os/actions/workflows/security.yml)
[![Docs](https://github.com/garrickdabbs/carv-os/actions/workflows/docs.yml/badge.svg?branch=main)](https://github.com/garrickdabbs/carv-os/actions/workflows/docs.yml)
[![Performance](https://github.com/garrickdabbs/carv-os/actions/workflows/perf.yml/badge.svg?branch=main)](https://github.com/garrickdabbs/carv-os/actions/workflows/perf.yml)
[![CodeQL](https://github.com/garrickdabbs/carv-os/actions/workflows/codeql.yml/badge.svg?branch=main)](https://github.com/garrickdabbs/carv-os/security/code-scanning)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/garrickdabbs/carv-os/badge)](https://scorecard.dev/viewer/?uri=github.com/garrickdabbs/carv-os)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**CARV: Capability Authority & Resource Versioning.** A hobby operating system, written from scratch
in Rust for x86_64, that treats four things Linux, Unix and Windows bolt on afterwards as parts of
the base design:

| Pillar | What it means in CarvOS |
|---|---|
| **Capability authority** | A program starts with *no* access and can use only the handles it is explicitly given. No ambient authority, no all-powerful root. |
| **Versioned storage** | Every write is a transaction in a content-addressed history that records *who* and *which program* made it. Any change can be undone. |
| **Typed data** | Programs and shell pipelines exchange schema-described records, not text to be re-parsed. |
| **Resource budgets** | CPU, memory, network, storage and an energy estimate are always charged to a budget; every process runs against one. |

The kernel is a capability microkernel called **chisel**. The full design and phased roadmap live in
[`docs/PLAN.md`](docs/PLAN.md). This is a learning project — see [Status](#status) before expecting
anything to work.

> **Naming:** *CARV* is the design, *CarvOS* is the product, `carv-os` is this repository and its
> artifacts, `carv` prefixes crates and identifiers (`carv-abi`, `.carv_schema`), and `chisel` is the kernel.

## Status

Phase 0 (scaffolding and boot) is in progress. Today CarvOS boots under Limine on UEFI and BIOS,
brings up a serial console, prints what the bootloader handed over, and halts. There is no
scheduler, no user space, and no filesystem yet. Progress is tracked in
[`docs/PLAN.md` §11](docs/PLAN.md#11-status-tracker) and [`CHANGELOG.md`](CHANGELOG.md).

## Quick start

CarvOS runs in a virtual machine. You need Rust, QEMU, UEFI firmware, and `xorriso`.

**Fedora / RHEL**
```bash
sudo dnf install -y qemu-system-x86 edk2-ovmf xorriso gcc git
```

**Debian / Ubuntu**
```bash
sudo apt-get install -y qemu-system-x86 ovmf xorriso build-essential git
```

**Rust** (any platform) — `rust-toolchain.toml` pins the exact nightly and installs the components
and `x86_64-unknown-none` target automatically the first time you run `cargo`:
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

**Build and boot**
```bash
git clone https://github.com/garrickdabbs/carv-os.git
cd carv-os
cargo xtask run
```
You should see the Limine menu, then:
```
CarvOS chisel v0.0.1 booting
  bootloader: Limine 11.4.1 (base revision 6)
  hhdm offset: 0xffff800000000000
  memory map: 31 entries, 466 MiB usable
chisel: nothing more to do yet; halting
```
Quit QEMU with `Ctrl-A` then `X`.

## Building

Everything goes through `cargo xtask` (a small Rust program in [`xtask/`](xtask/); no Makefiles).

| Command | What it does |
|---|---|
| `cargo xtask build [--release]` | Build the `chisel` kernel for `x86_64-unknown-none` and verify it is linked in the higher half. |
| `cargo xtask image [--release]` | Build a bootable ISO (`target/carv-os.iso`, UEFI + BIOS) and a blank 64 MiB virtio disk (`target/data.img`). |
| `cargo xtask limine` | Fetch and verify the pinned Limine bootloader files (done automatically by `image`). |
| `cargo xtask run [flags]` | Build the image and boot it in QEMU with the serial console on your terminal. |
| `cargo xtask test [--host] [--kernel] [--integration] [--release] [--timeout SECS]` | **The one command every PR must pass**: host unit tests, the in-kernel test binary booted in QEMU, and the boot smoke scenarios. No selector runs all three. |
| `cargo xtask smoke [--release] [--timeout SECS]` | Boot the image under UEFI, BIOS, and with a deliberate panic, and check the serial output (also part of `test`). |
| `cargo xtask docs-gate [--base REF]` | Fail if code changed without matching documentation changes (see [Contributing](#contributing)). |
| `cargo xtask perf [--runs N]` | Measure release-kernel and ISO size and boot timings (median of N boots) against budgets; report in `target/perf/report.md`. |
| `cargo xtask help` | List commands. |

`run` flags:

| Flag | Effect |
|---|---|
| `--release` | Optimised kernel. |
| `--bios` | Boot with SeaBIOS instead of OVMF/UEFI. |
| `--debug` | Log interrupts, CPU resets and guest errors to `target/qemu.log`. |
| `--timeout SECS` | Kill QEMU after `SECS` seconds and exit with status 124. Use this in scripts. |
| `--cmdline STR` | Kernel command line, passed through Limine. `panic-test` triggers a test panic. |

Notes:
- **KVM** is used when `/dev/kvm` is readable and writable; otherwise `xtask` falls back to TCG
  emulation (slow) and says so. On Fedora, add yourself to the `kvm` group if needed.
- **UEFI firmware** is probed at the usual Fedora, Debian and Arch paths. Override with
  `CARV_OVMF_CODE` and `CARV_OVMF_VARS`, or use `--bios`.
- **Limine** (v11.4.1) is downloaded once from a pinned commit; every file's SHA-256 is checked
  against the values in `xtask/src/main.rs` before it is used. A mismatch fails the build.
- Plain `cargo build` / `cargo test` at the workspace root build only host code (`xtask`, and later
  the `crates/*` libraries); the kernel is built only through `xtask`.

## Testing

```bash
cargo xtask test            # everything a PR must pass: host tests, in-kernel tests in QEMU, boot smoke
cargo xtask test --kernel   # just the in-kernel tests (kernel/src/test.rs, `#[test_case]` functions)
```
In-kernel tests are ordinary functions marked `#[test_case]` in the kernel crate. `cargo xtask test`
builds them into a bootable test kernel, boots it under Limine in QEMU, and reads the result from the
`isa-debug-exit` exit code, so a hang, a triple fault or a panic all count as failures.

Before opening a PR, also run `cargo fmt --all --check`, `cargo clippy --workspace --exclude chisel --all-targets -- -D warnings`,
`cargo clippy -p chisel --target x86_64-unknown-none -- -D warnings`, `cargo deny check` and `cargo xtask perf`.
The same checks run in CI; the workflows in [`.github/workflows/`](.github/workflows/) are their definition.

## Installing

CarvOS is a VM operating system. "Installing" means booting the ISO somewhere.

**QEMU (recommended)** — `cargo xtask run`, or by hand:
```bash
cargo xtask image
qemu-system-x86_64 -machine q35 -cpu max -m 512M -accel kvm \
  -drive if=pflash,unit=0,format=raw,readonly=on,file=/usr/share/edk2/ovmf/OVMF_CODE.fd \
  -cdrom target/carv-os.iso -boot d -serial stdio -display none
```

**libvirt / virt-manager, VirtualBox, VMware** — create an x86_64 VM (UEFI or BIOS firmware, 512 MiB
RAM is plenty), attach `target/carv-os.iso` as a CD-ROM, and add a serial console; all output goes
to the serial port. Only virtio block and network devices will be supported when drivers arrive.

**Real hardware** — not supported. The ISO is a hybrid image so `dd if=target/carv-os.iso of=/dev/sdX`
will boot Limine on a real machine, but nothing beyond the serial banner is exercised there.

**Releases** — none yet. When they exist (see [`docs/PLAN.md` §7.5](docs/PLAN.md#75-releases--githubworkflowsreleaseyml)),
each will ship `carv-os-vX.Y.Z-x86_64.iso`, `SHA256SUMS`, an SBOM, a Sigstore signature and a build
provenance attestation, verifiable with `sha256sum -c` and `gh attestation verify`.

## Repository layout

```
kernel/          chisel, the microkernel (no_std, x86_64-unknown-none)
xtask/           cargo xtask: build, image, run, smoke, docs-gate
docs/PLAN.md     design, roadmap with task IDs, GitHub infrastructure, status
docs/adr/        architecture decision records (as they are written)
.github/         CI workflows, Dependabot, CODEOWNERS, PR and issue templates
CLAUDE.md        rules for AI coding agents working in this repo
CHANGELOG.md     Keep-a-Changelog; every PR adds a line under Unreleased
SECURITY.md      how to report a vulnerability
deny.toml        cargo-deny policy
```
`crates/` (host-testable libraries), `services/` (user-space drivers and servers) and `userland/`
(shell and tools) appear in later phases.

## Contributing

This is a solo hobby project built largely by AI coding agents following [`docs/PLAN.md`](docs/PLAN.md),
but the workflow is the same for anyone:

1. One task ID per branch and PR (`p1.2-frame-allocator`), titled `P1.2: …`.
2. Open a PR against `main`. All required checks must pass. Merge with a merge commit.
3. **Documentation travels with code.** The docs gate (`cargo xtask docs-gate`) fails a PR that
   changes code under `kernel/`, `crates/`, `services/`, `userland/`, `xtask/` or `Cargo.toml`
   without a `CHANGELOG.md` entry; changes `xtask` without updating this README, `CLAUDE.md` or the
   plan; changes CI workflows without updating the plan or README; or changes `docs/abi.md` without
   an ADR. Dependabot PRs are exempt. If a change genuinely needs no docs, write `docs-gate: skip` and the
   reason in the PR body. The same gate runs as a Claude Code stop hook ([`.claude/settings.json`](.claude/settings.json)),
   so agents cannot finish a task with undocumented code.
4. Every `unsafe` block carries a `// SAFETY:` comment. Kernel code never adds a way around
   capability checks.

## Security

CarvOS is a hobby OS with **no security guarantees**. See [`SECURITY.md`](SECURITY.md) for how to
report a problem anyway, and [`docs/PLAN.md` §7.3](docs/PLAN.md#73-security) for the supply-chain
controls on this repository.

## License

[MIT](LICENSE). Copyright (c) 2026 Garrick Dabbs.
