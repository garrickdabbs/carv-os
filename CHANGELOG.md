# Changelog

All notable changes to CarvOS are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow SemVer `0.y.z`.
Every PR adds a line under *Unreleased*; the release PR moves them under a version heading.

## [Unreleased]

### Added
- P0.1: Cargo workspace, pinned nightly toolchain, higher-half `chisel` kernel link, `cargo xtask build`.
- P0.2: Limine boot (v11.4.1, every file SHA-256 pinned), `cargo xtask image` (UEFI + BIOS bootable ISO
  plus a blank virtio data disk), `cargo xtask run` (QEMU with OVMF, KVM auto-detect, `--bios`,
  `--debug`, `--timeout`). The kernel prints the Limine handoff details over serial and halts.
- P0.3: 16550 serial driver on COM1 with loopback self-test, `kprint!`/`kprintln!`, a panic handler that
  reports file:line and message over serial, `SpinLock`, port I/O primitives, and
  `cargo xtask run --cmdline STR` (Limine passes it to the kernel; `panic-test` triggers a test panic).
- Repository infrastructure (P0.5/P0.6 partial): README with build, test and install instructions; CI
  workflow (`lint`, `host-tests`, `boot-smoke`, `docs-gate`, `image`, `all-green` gate) with SHA-pinned
  actions and least-privilege permissions; `cargo deny` policy; Dependabot for crates and actions;
  CODEOWNERS, PR and issue templates; `SECURITY.md`.
- `cargo xtask smoke` (UEFI, BIOS and panic boot scenarios with serial-output checks) and
  `cargo xtask docs-gate` (code changes must carry documentation), the latter also wired as a Claude
  Code Stop hook in `.claude/settings.json`.
- Licensed under MIT (`LICENSE`; `license = "MIT"` on every workspace crate, checked by `cargo deny`).
- CI split into one workflow per README badge: Build, CI (boot smoke), Security (cargo-deny, cargo-audit weekly,
  documented-unsafe lint, action-pinning check), Docs (docs gate, rustdoc, link check), Performance
  (`cargo xtask perf`: kernel/ISO size and boot-time budgets), CodeQL (Rust + Actions), OpenSSF Scorecard.
  Crate roots now `#![deny(clippy::undocumented_unsafe_blocks)]` and `#![deny(missing_docs)]`. Repo now public
  with secret scanning + push protection, private vulnerability reporting and Dependabot security updates on.
- P0.4: in-kernel test framework (`kernel/src/test.rs`: `#[test_case]` runner, `isa-debug-exit` pass/fail codes)
  and `cargo xtask test` — host unit tests, the test kernel booted in QEMU, and the boot smoke scenarios in one
  command; CI's `boot-smoke` job now runs it. Host-side `cargo test`/`clippy` use `--workspace --exclude chisel`.
- xtask: cargo's JSON messages are parsed with `serde_json` (paths with quotes or backslashes now survive; #13, #14);
  xorriso and `limine bios-install` output is shown on failure and with `CARV_XTASK_VERBOSE=1` (#15).
- xtask: failed-tool errors separate captured stdout and stderr with a newline (#18); README pre-PR checklist
  lists every required command including `cargo xtask test` and `cargo audit` (#19).
