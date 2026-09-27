# Contributing to CarvOS

CarvOS is a solo hobby project built largely by AI coding agents following [`docs/PLAN.md`](docs/PLAN.md).
Outside contributions are welcome; the same workflow applies to everyone.

## Before you start
- Read [`docs/PLAN.md`](docs/PLAN.md) (design, roadmap, task IDs) and [`CLAUDE.md`](CLAUDE.md) (the rules agents
  follow — they are the project's engineering rules, not agent-specific).
- Pick a roadmap task or open an issue first. Design changes need an ADR (see [`docs/adr/`](docs/adr/README.md)).
- Set up the toolchain and QEMU as described in the [README](README.md#quick-start).

## Workflow
1. Branch from `main`, one task or fix per branch (`p1.2-frame-allocator`, `fix-123-…`).
2. Run everything CI runs before opening a PR — the list is in the README under *Testing*
   (`cargo xtask test`, `cargo xtask perf`, `cargo fmt`, both clippy invocations, `cargo deny check`,
   `cargo audit`).
3. Open a PR against `main` using the template. Required checks: `build`, `boot-smoke`, `security`, `docs`,
   `perf`. **Documentation travels with code** (`cargo xtask docs-gate` enforces it): a `CHANGELOG.md` line under
   *Unreleased*, and README / CLAUDE.md / PLAN updates when commands, workflow or design change.
4. Answer every review thread and resolve it; the `main` ruleset blocks merging otherwise. PRs merge with a
   **merge commit**.

## Code rules
- Rust, pinned nightly (`rust-toolchain.toml`). `cargo fmt` and clippy `-D warnings` must be clean.
- Every `unsafe` block carries a `// SAFETY:` comment (`#![deny(clippy::undocumented_unsafe_blocks)]`); every
  public item has a doc comment (`#![deny(missing_docs)]`).
- Logic that can live in a pure `crates/*` library must — it is tested natively on the host. The kernel and
  services are thin glue. New kernel behaviour gets a `#[test_case]`; behaviour that needs a fresh boot gets a
  smoke scenario in `xtask`.
- No ambient authority in the kernel: no global namespace, no root bypass, no syscall that skips a capability
  check. Every allocation path charges a budget (from Phase 2).

## Reporting bugs and security issues
- Bugs: use the *Bug* issue template and attach the serial log (`target/smoke/*.log` or `cargo xtask run --debug`).
- Security: **do not open a public issue** — follow [`SECURITY.md`](SECURITY.md) (private vulnerability reporting).

## Releases
Tagged `vX.Y.Z` releases are built by `.github/workflows/release.yml`: reproducible ISO, tests against the exact
artifact, `SHA256SUMS`, SBOMs, a Sigstore keyless signature and a SLSA build-provenance attestation. See the
README's *Installing* section for how to verify a download.
