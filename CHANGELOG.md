# Changelog

All notable changes to CarvOS are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow SemVer `0.y.z`.
Every PR adds a line under *Unreleased*; the release PR moves them under a version heading.

## [Unreleased]

### Added
- P2.9/P2.10: allocation-free `no_std` ELF64 validation and segment-mapping abstractions for a
  Limine root-task module (untested and not yet called: there is no module request or `init`
  binary), plus ADR-0002 and the frozen ABI v1 contract. Ring-3 address-space integration remains
  P2.5 work.
- P2.5/P2.6 (foundations only, nothing runs in ring 3 yet): kernel ring-3 selector and
  user-context abstractions, syscall MSR setup (STAR/LSTAR/SFMASK) and CR4 SMEP/SMAP. The syscall
  entry point only halts, EFER.SCE is not set, and there is no context switch. A budget-aware
  round-robin scheduler *model* is ticked by the LAPIC timer, but it holds no threads at runtime,
  never switches, and does its own refill math instead of using `carv-budget`.
- P2.7/P2.8 (foundations only, exercised by in-kernel tests rather than by user threads):
  non-blocking endpoint send/recv/call/reply queues, badge-bearing messages with capability
  transfer, notification signal/wait words, and a GSI-to-notification router that is not yet
  connected to real interrupt delivery. Capability transfer has a known bug (#141).
- P2.1: added the `carv-abi` crate with syscall numbers, IPC message layout, error codes, and
  capability-rights bits, documented in `docs/abi.md`.
- P2.4 (partial, #59 reopened): kernel object registry with budget-charged Thread, AddressSpace,
  Frame, Endpoint, Notification, Budget, and Reply objects, plus ABI-independent `invoke` dispatch
  abstractions. Objects are type tags without state, cannot yet be created via `invoke`, and the
  registry is not used by the rest of the kernel.
- P2.2: `carv-caps`, a pure `no_std` capability-space library with fixed slots, attenuating copy and
  badge minting, derivation tracking, descendant revocation, and proptest coverage.
- P2.3: `carv-budget`, a pure host-testable `no_std` crate for CPU period refills, memory charges,
  hierarchical child-limit carve-outs, and rate-limited token buckets.
- `nightly.yml`: a `report-status` job (`issues: write` only, `needs: [nightly, publish]`,
  `if: always()`) creates or updates a single pinned "Nightly is failing" issue with a link to the
  run when nightly fails, and closes it with a comment the next time nightly is green (PLAN.md
  §7.4).
- P1.7: PCI(e) enumeration — `platform::pci` walks every MCFG ECAM region (bus 0 plus the secondary buses of
  PCI-to-PCI bridges inside the region's bus range), reading each candidate's header through a single scratch
  mapping (no permanent config-space mappings), and records vendor/device/class and the config page's physical
  address per function; virtio devices are named (blk, net, rng, console, 9p). Boot prints
  the list, the UEFI/BIOS smoke scenarios expect `virtio-blk` and `virtio-net`, and an in-kernel test requires
  the q35 host bridge and both virtio devices with matching class codes.
- P1.6: ACPI tables — `platform::acpi` parses the RSDP Limine hands over with the `acpi` crate (5.x, `alloc`),
  reaching tables through the HHDM (a region with pages the direct map skips is mapped whole, read-only and
  cached, into a small window via the new `paging::map_reserved`), and keeps a
  `Summary`: table signatures, MADT local-APIC address, CPU APIC ids and I/O APICs, HPET base, and MCFG ECAM
  regions for P1.7. Boot prints all of it; the UEFI/BIOS smoke scenarios expect the HPET line; an in-kernel test
  checks the MADT, matches the boot CPU's APIC id against the running local APIC, and requires HPET and MCFG.
- P1.5: local APIC + timer — the legacy 8259 PICs are remapped and masked (`arch::x86_64::pic`), the xAPIC register page is
  mapped uncached at `paging::KERNEL_MMIO_BASE` (new `paging::map_mmio`; Limine's direct map does not cover device
  memory) and enabled (`arch::x86_64::apic`), its timer is calibrated against PIT channel 2
  (`arch::x86_64::pit::busy_wait_ms`) and runs periodic at 1 kHz on vector 32 with a spurious-vector handler;
  the PICs' 16 vectors (`0xE0..=0xEF`) get no-op handlers so a spurious IRQ7/IRQ15 is swallowed; boot enables
  interrupts, requires 90–110 ticks over a PIT-timed 100 ms (panics otherwise) and prints the success line the
  UEFI/BIOS smoke scenarios expect;
  an in-kernel test counts 100 ± 10 ticks over a PIT-timed 100 ms wait.
- P1.4: kernel heap — `kernel::mm::heap` maps 1 MiB of fresh frames at `KERNEL_DYNAMIC_BASE + 256 MiB` and
  installs `linked_list_allocator` (no default features) as the `#[global_allocator]` behind the kernel spin lock
  with interrupts off; `alloc` is enabled in `chisel`; boot prints the heap layout; in-kernel tests exercise
  `Vec`/`Box`/`BTreeMap` and prove memory returns to baseline after 200 allocate/free rounds. Bytes in use are
  counted for the Budget accounting that arrives with P2.4. An explicit `#[alloc_error_handler]` reports
  `kernel heap exhausted` with the request and heap state and takes the panic path; the `oom` smoke scenario
  (`oom-test` on the cmdline) proves it.

### Fixed
- P2.5 (#140): the GDT now places user data 8 bytes below user code, so `sysretq` loads SS from the
  user data descriptor instead of the upper half of the TSS descriptor; STAR is programmed with
  `Star::write`, which panics at boot on an incompatible layout, and an in-kernel test checks that
  STAR decodes back to the GDT selectors. LSTAR and SFMASK use the crate's typed writers too, which
  removes the `unsafe` block from `syscall::init`.
- P2.6: correct the scheduler kernel test to account for the initial dispatch not charging a thread;
  verify round-robin skips it only after its budget is exhausted.
- `cargo xtask image` pins the ISO's volume id (`CARVOS`), application id (`CarvOS`) and preparer id: without
  them xorriso writes its own version string into the volume descriptors, the one remaining difference (16 bytes)
  between a Fedora build and the runner's `v0.1.1-rc.1` image (kernel ELF and every file were already identical).
- P1.6: migrate ACPI table parsing to `acpi` 6.1.1, including the expanded `Handler` contract,
  revised table iteration and platform/MCFG APIs.
- ACPI MADT existence assertion in `kernel/src/test.rs` now prints the parsed table names
  (`s.tables`) instead of a table count, so a missing-MADT failure shows which tables were actually
  parsed (20bcd7d, #100).

### Changed
- P2.7: the kernel now depends on `carv-abi` and takes its IPC message limits from it
  (`MESSAGE_WORDS` = 6 inline words, `MESSAGE_CAPS` = 4 capabilities) instead of its own 4-word
  constant, so the kernel queues accept exactly what the frozen ABI v1 allows; an in-kernel test
  checks the limits. Conversion to the `repr(C)` wire `Message` happens with the syscall glue.
- Docs now match the tree: PLAN.md §4 marks directories and crates that do not exist yet as
  planned and lists `carv-frames`; CLAUDE.md no longer says `cargo xtask build` builds services
  and userland, marks `cargo xtask gdb` as not implemented, and notes that the kernel heap does not
  charge a Budget yet (P2.4).
- Kernel doc-comment cleanup, no behaviour change: `force_stack_overflow`'s doc comment (which had
  been misattached to `force_oom`) now sits on the right function; the module doc no longer opens
  with the stale "P1.1 state" wording and now describes the actual order (banner right after
  serial init, interrupts enabled and the LAPIC timer validated just before halting); the
  guard-page comment in `force_stack_overflow` is worded in terms of `base` instead of a
  nonexistent `TEST_STACK_BASE` constant; the `RSDP` request static moved up next to the other
  Limine requests for readability; and the `interrupts_are_disabled_at_boot` test comment now says
  interrupts stay disabled for the whole test run (a test build's `test::runner` exits QEMU
  directly and never reaches `kmain`'s later `enable_interrupts()` call), instead of claiming there
  is no IDT yet.
- `release.yml` retries `cosign sign-blob` up to four times with backoff on transient Sigstore network errors
  (the `v0.1.1-rc.2` publish job needed a manual re-run) (#76).

## [0.1.1] - 2026-09-27

Phase 1 under way: the kernel now has a GDT/TSS/IDT with a handler for every exception, a physical
frame allocator and a page-table mapper, and releases are byte-identical on any machine. Also the
review follow-ups and fixes from the first day of public development.

### Changed
- The Claude Code settings file (`.claude/settings.json`, a Stop hook running the docs gate) is no longer
  committed and `.claude/` is gitignored: a shared hook file runs shell commands on every contributor's machine.
  Documentation-with-code is enforced by the `docs` CI check alone; a local hook is an optional personal setting.
- Review follow-ups (threads on #7, #24, #25, #30, #32, #33, #37, #39): the docs gate runs from the base
  branch's `xtask`, checks that `## [Unreleased]` actually gained content, requires a *new* ADR for
  `docs/abi.md`, and only honours a skip marker with a ≥ 20-character reason; `cargo deny` treats duplicate
  dependency versions as errors; the nightly workflow has a concurrency group; SBOMs are included in the
  release provenance subjects; `release-check` output writing is a tested helper; TOML quote stripping only
  removes a matching delimiter pair; the IDT also covers `#CP`, `#HV` and `#VC`; the frame allocator
  tracks *allocated* frames separately so a stray `free` cannot release a reservation, initialises only its
  own storage, and takes its lock with interrupts disabled; the mapper rejects pages outside the kernel
  dynamic region, returns the frame on failure, and also locks with interrupts disabled.
- `stack-overflow-test` passes `depth = 0` in `rdi` explicitly instead of relying on a leftover register value (#40).
- Review round 2 on the follow-up PR: the docs workflow bootstraps when the base `xtask` predates `--repo`; the
  skip directive must start a line and be followed by a separator; `mark_used_range` refuses ranges containing
  allocated frames; `Frame` constructors are crate-private and `paging::map` refuses frames the allocator has
  not handed out (`FrameNotOwned`).
- Release and nightly workflows split into a read-only build job and a minimal-permission publish job; the SLSA
  provenance is now also published as `carv-os-<tag>.intoto.jsonl` and `carv-os-<tag>.provenance.sigstore.json`
  release assets (#35). Fuller `.gitignore`; `CONTRIBUTING.md` added (#34, #35).
- Reproducible builds across machines (#29): the release profile enables cargo's `trim-paths = "all"`, so no
  checkout, registry or sysroot path reaches the kernel ELF, and `cargo xtask image --release` strips debuginfo
  from the ISO's kernel copy with the toolchain's `llvm-objcopy`. The release workflow's second pass now builds
  from a different directory with a fresh `CARGO_HOME` and requires identical ISO and kernel hashes.
- Release workflow: the build job declares its read-only `contents: read` token explicitly (#35).

### Added
- P1.3: paging — `kernel::mm::paging` adopts Limine's page tables through the HHDM (`OffsetPageTable`) with
  `map`/`unmap`/`translate` and a kernel dynamic region at `0xffff9000_00000000`; in-kernel test maps, writes,
  reads back through the HHDM alias, translates and unmaps; new smoke scenarios `pfault` (unmapped access is
  reported by the `#PF` handler) and `sovflw` (recursion on a guard-paged stack ends in the double-fault
  handler — the test deferred from P1.1).
- P1.2: physical frame allocator — `crates/carv-frames` (pure bitmap allocator, 5 host tests) and
  `kernel::mm::frame` (bitmap carved from the first usable Limine region via the HHDM; frame 0 never handed
  out); boot prints free/total frames; in-kernel test allocates and frees 10 000 frames.
- P1.1: GDT with kernel code/data/TSS descriptors, a dedicated double-fault IST stack, and an IDT with a handler
  for every CPU exception (`#BP` resumes and is counted; `#DF` prints on its own stack and halts; everything
  else panics with the frame). `x86_64` 0.15 crate. Kernel cmdline `double-fault-test` and a `dfault` smoke
  scenario prove the double-fault path; in-kernel tests cover `int3` and the loaded selectors.

### Fixed
- In-kernel paging tests return their frame to the allocator before panicking when a `map` unexpectedly fails,
  matching the ownership discipline used everywhere else (#43).
- `carv-frames::free_range` guards its scan-hint update against out-of-range starts explicitly (the hint could
  never actually move past the bitmap; a test now proves the out-of-range case is harmless) (#38).
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
