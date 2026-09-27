# Changelog

All notable changes to CarvOS are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow SemVer `0.y.z`.
Every PR adds a line under *Unreleased*; the release PR moves them under a version heading.

## [Unreleased]

### Changed
- Release and nightly workflows split into a read-only build job and a minimal-permission publish job; the SLSA
  provenance is now also published as `carv-os-<tag>.intoto.jsonl` and `carv-os-<tag>.provenance.sigstore.json`
  release assets (#35). Fuller `.gitignore`; `CONTRIBUTING.md` added (#34, #35).
- Reproducible builds: `.cargo/config.toml` remaps source paths to relative paths, and `cargo xtask image --release`
  strips debuginfo from the ISO kernel for byte-identical releases across machines (#29).
- Release workflow: build job permissions narrowed to read-only; publish job permissions scoped to specific steps
  instead of blanket `contents: write` (#35).

### Added
- P1.2: physical frame allocator — `crates/carv-frames` (pure bitmap allocator, 5 host tests) and
  `kernel::mm::frame` (bitmap carved from the first usable Limine region via the HHDM; frame 0 never handed
  out); boot prints free/total frames; in-kernel test allocates and frees 10 000 frames.
- P1.1: GDT with kernel code/data/TSS descriptors, a dedicated double-fault IST stack, and an IDT with a handler
  for every CPU exception (`#BP` resumes and is counted; `#DF` prints on its own stack and halts; everything
  else panics with the frame). `x86_64` 0.15 crate. Kernel cmdline `double-fault-test` and a `dfault` smoke
  scenario prove the double-fault path; in-kernel tests cover `int3` and the loaded selectors.

### Fixed
- `release-check` fails instead of silently continuing when `GITHUB_OUTPUT` cannot be written, and the release
  workflow refuses to publish without an explicit prerelease decision (#26); `[workspace.package]` parsing
  tolerates TOML comments (#27); the nightly stress loop checks `cargo xtask smoke`'s exit code rather than
  grepping one line (#28).
- `release-check`'s TOML comment stripping respects quoted strings (#31).

## [0.1.0] - 2026-09-27

Phase 0 complete: CarvOS boots under Limine on UEFI and BIOS, brings up a serial console, runs its
in-kernel tests, and ships through a reproducible, signed, attested release pipeline.

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
- xtask: captured tool stdout/stderr are trimmed consistently before being joined, so error output never has a
  blank line between them (#22).
- P0.8: release pipeline — `release.yml` (tag `v*` → `release-check`, reproducible ISO built twice and
  compared, full test suite, boot of the exact release ISO, `SHA256SUMS`, CycloneDX SBOM, Sigstore keyless
  signature, SLSA build provenance, GitHub pre-release with generated notes), `nightly.yml` (daily full tests,
  10-boot stress, 10-run perf, `cargo audit`, newest-nightly toolchain drift, rolling `nightly` pre-release),
  `cargo xtask release-check`, `run`/`smoke --iso PATH`, `SOURCE_DATE_EPOCH`-pinned ISO timestamps.
