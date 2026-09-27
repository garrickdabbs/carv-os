# Tally OS — Agent Instructions

Hobby capability-microkernel OS in Rust for x86_64, tested in QEMU. The full plan is in `docs/PLAN.md`.
Read it before starting any task.

## Commands
- `cargo xtask build` — build kernel, services, userland
- `cargo xtask run [--debug]` — boot in QEMU (serial on stdio); `--debug` adds QEMU int/reset logging
- `cargo xtask test` — host unit tests + in-kernel tests + QEMU integration tests. **Must pass before any PR.**
- `cargo xtask gdb` — boot paused with gdb stub attached

## Rules
- One task ID per branch/PR (`p2.7-ipc`). Stay inside the directories your task names.
- Put logic in pure `crates/*` libraries (host-testable) whenever possible; the kernel and services are thin glue.
- Every `unsafe` block needs a `// SAFETY:` comment explaining why it is sound.
- No changes to `docs/abi.md` or `crates/tally-abi` after the ABI freeze (P2.10) without a new ADR in `docs/adr/`.
- No ambient authority: never add a global path namespace, a "root bypass", or a syscall that skips capability checks.
- Every allocation path must charge a Budget.
- Every QEMU test run has a timeout; a hang counts as a failure.
- Don't mark a task done without pasting `cargo xtask test` output. If blocked on a design question, write it up and stop.
- Update the status table in `docs/PLAN.md` §11 when you open a PR.
- clippy with `-D warnings` and `cargo fmt` must be clean.
- Add a line under *Unreleased* in `CHANGELOG.md` for any user-visible change.
- Workflows: pin every action to a full commit SHA, keep top-level `permissions: contents: read`, never add secrets. Never edit the `main` ruleset or bypass CI. See PLAN.md §7.
