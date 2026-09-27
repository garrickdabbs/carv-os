# CarvOS — Agent Instructions

CarvOS (CARV: Capability Authority & Resource Versioning) is a hobby capability-microkernel OS in Rust for x86_64, tested in QEMU. Kernel crate: `chisel`. Naming conventions are in PLAN.md (top). The full plan is in `docs/PLAN.md`.
Read it before starting any task.

## Commands
- `cargo xtask build` — build kernel, services, userland
- `cargo xtask image [--release]` — build `target/carv-os.iso` (UEFI + BIOS) and `target/data.img`
- `cargo xtask run [--release] [--bios] [--debug] [--timeout SECS]` — boot in QEMU (serial on stdio); `--debug` logs int/reset to `target/qemu.log`; always pass `--timeout` in scripts
- `cargo xtask smoke [--release] [--timeout SECS]` — boot UEFI, BIOS and panic scenarios in QEMU and check serial output; logs in `target/smoke/`
- `cargo xtask docs-gate [--base REF]` — fail (exit 2) if code changed without documentation; runs in CI and as the Stop hook in `.claude/settings.json`
- `cargo xtask test` — host unit tests + in-kernel tests + QEMU integration tests (arrives in P0.4). **Must pass before any PR.** Until then: `cargo test --workspace`, `cargo xtask smoke`, clippy, fmt, `cargo deny check`.
- `cargo xtask gdb` — boot paused with gdb stub attached

## Rules
- One task ID per branch/PR (`p2.7-ipc`). Stay inside the directories your task names.
- Put logic in pure `crates/*` libraries (host-testable) whenever possible; the kernel and services are thin glue.
- Every `unsafe` block needs a `// SAFETY:` comment explaining why it is sound.
- No changes to `docs/abi.md` or `crates/carv-abi` after the ABI freeze (P2.10) without a new ADR in `docs/adr/`.
- No ambient authority: never add a global path namespace, a "root bypass", or a syscall that skips capability checks.
- Every allocation path must charge a Budget.
- Every QEMU test run has a timeout; a hang counts as a failure.
- Don't mark a task done without pasting `cargo xtask test` output. If blocked on a design question, write it up and stop.
- Update the status table in `docs/PLAN.md` §11 when you open a PR.
- clippy with `-D warnings` and `cargo fmt` must be clean.
- **Documentation travels with code** (enforced by `cargo xtask docs-gate` in CI and on Stop): any change under `kernel/`, `crates/`, `services/`, `userland/`, `xtask/` or `Cargo.toml` needs a `CHANGELOG.md` line under *Unreleased*; `xtask` changes need README.md/CLAUDE.md/PLAN.md updated; workflow changes need PLAN.md §7 or README updated; `docs/abi.md` changes need an ADR. Do not disable the hook or use `docs-gate: skip` without a stated reason.
- Workflows: pin every action to a full commit SHA, keep top-level `permissions: contents: read`, never add secrets. Never edit the `main` ruleset or bypass CI. See PLAN.md §7.
