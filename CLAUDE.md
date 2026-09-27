# CarvOS — Agent Instructions

CarvOS (CARV: Capability Authority & Resource Versioning) is a hobby capability-microkernel OS in Rust for x86_64, tested in QEMU. Kernel crate: `chisel`. Naming conventions are in PLAN.md (top). The full plan is in `docs/PLAN.md`.
Read it before starting any task.

## Commands
- `cargo xtask build` — build kernel, services, userland
- `cargo xtask image [--release]` — build `target/carv-os.iso` (UEFI + BIOS) and `target/data.img`; `--release` strips debuginfo from the ISO's kernel copy (reproducible builds, #29)
- `cargo xtask run [--release] [--bios] [--debug] [--timeout SECS]` — boot in QEMU (serial on stdio); `--debug` logs int/reset to `target/qemu.log`; always pass `--timeout` in scripts
- `cargo xtask smoke [--release] [--timeout SECS]` — boot UEFI, BIOS and panic scenarios in QEMU and check serial output; logs in `target/smoke/`
- `cargo xtask docs-gate [--base REF]` — fail (exit 2) if code changed without documentation; CI runs it on every PR from the base branch's xtask. Run it before opening a PR. (Optional, local only: a Claude Code Stop hook in your own `.claude/settings.local.json` — `.claude/` is gitignored and never committed, because a shared settings file would run shell commands on every contributor's machine.)
- `cargo xtask perf [--runs N]` — size and boot-time budgets (release build, median of N boots); CI fails a PR that exceeds them, so if you grow the kernel deliberately, raise the budget constants in `xtask` in the same PR and say why
- `cargo xtask test [--host] [--kernel] [--integration]` — host unit tests + in-kernel `#[test_case]` tests booted in QEMU (`isa-debug-exit`) + boot smoke. **Must pass before any PR.** Also run `cargo xtask perf`, clippy (host: `--workspace --exclude chisel`; kernel: `-p chisel --target x86_64-unknown-none`), fmt, `cargo deny check`.
- `cargo xtask gdb` — boot paused with gdb stub attached

## Rules
- One task ID per branch/PR (`p2.7-ipc`). Stay inside the directories your task names.
- Several agent sessions may run on this machine at once. Work in your **own git worktree** (`git worktree add ../carv-os-<name> origin/main`), never in a checkout another session uses; never use bare `git stash`; background scripts read state via `gh` and never `checkout`/`pull`.
- Put logic in pure `crates/*` libraries (host-testable) whenever possible; the kernel and services are thin glue.
- Every `unsafe` block needs a `// SAFETY:` comment explaining why it is sound (enforced: `#![deny(clippy::undocumented_unsafe_blocks)]` in every crate root). Every public item needs a doc comment (`#![deny(missing_docs)]`).
- No changes to `docs/abi.md` or `crates/carv-abi` after the ABI freeze (P2.10) without a new ADR in `docs/adr/`.
- No ambient authority: never add a global path namespace, a "root bypass", or a syscall that skips capability checks.
- Every allocation path must charge a Budget.
- Every QEMU test run has a timeout; a hang counts as a failure. New kernel behaviour gets a `#[test_case]` in `kernel/src/test.rs` (or a `test` module next to the code); tests that need a fresh boot or a panic go in the `xtask` smoke scenarios.
- Don't mark a task done without pasting `cargo xtask test` output. If blocked on a design question, write it up and stop.
- Update the status table in `docs/PLAN.md` §11 when you open a PR.
- Review comments and threads on your PR are yours to close: reply to each, fix or explain, and resolve the thread. **Wait at least 15 minutes after opening a PR before merging** so the automated reviewers can post, then validate and resolve every thread first; never enable auto-merge on a fresh PR. The `main` ruleset refuses to merge with unresolved threads. Issues filed against your commits are fixed in a follow-up PR that says `Closes #N`.
- clippy with `-D warnings` and `cargo fmt` must be clean.
- **Documentation travels with code** (enforced by `cargo xtask docs-gate` in CI; run it locally before every PR): any change under `kernel/`, `crates/`, `services/`, `userland/`, `xtask/` or `Cargo.toml` needs a `CHANGELOG.md` line under *Unreleased* (a release PR instead adds the new `## [x.y.z]` heading); `xtask` changes need README.md/CLAUDE.md/PLAN.md updated; workflow changes need PLAN.md §7 or README updated; `docs/abi.md` changes need an ADR. A skip needs `docs-gate: skip — <reason ≥ 20 chars>` in the PR body, and CI runs the base branch's gate, so changing the rules in a PR does not help.
- Workflows: one workflow per badge (`build`, `ci`, `security`, `docs`, `perf`, `codeql`, `scorecard`); pin every action to a full commit SHA, keep top-level `permissions: contents: read`, never add secrets. Never edit the `main` ruleset or bypass CI. See PLAN.md §7.
