# Security Policy

## Scope and expectations

CarvOS is a **hobby operating system** built for learning. Although its whole design is about
security (capability authority, versioned storage, budgets), it has had no independent review,
runs only in virtual machines, and comes with **no security guarantees of any kind**. Do not run it
on anything you care about, and do not expose a CarvOS VM to an untrusted network.

That said, the project takes its own claims seriously and wants to hear when it falls short of them.

## Reporting a vulnerability

Please report privately through **GitHub Security Advisories**:
<https://github.com/garrickdabbs/carv-os/security/advisories/new>

Do **not** open a public issue for security problems.

Include what you can of: the affected commit or release, a description of the issue, steps or a
program that reproduces it in QEMU (`cargo xtask run --cmdline ... --timeout 60`), and the serial
log. A capability bypass, a way for one process to reach memory or objects it holds no capability
for, a way to escape or exceed a budget, or a way to alter history in `vstore` without a recorded
commit are all in scope even while the project is pre-1.0.

## What to expect

- Acknowledgement within 7 days. This is a one-person project; if that slips, a follow-up message
  is welcome.
- A fix (or a documented decision not to fix) tracked in the advisory, then a `CHANGELOG.md` entry
  and, once releases exist, a release with the fix.
- Credit in the advisory and changelog if you want it.

## Supported versions

Only the `main` branch and the latest release (when releases exist) receive fixes.

## Supply-chain controls on this repository

Documented in [`docs/PLAN.md` §7.3](docs/PLAN.md#73-security): `cargo deny` on every PR, weekly
Dependabot for crates and GitHub Actions, every Action pinned to a commit SHA, least-privilege
workflow permissions, SHA-256 verification of the pinned bootloader binaries, and a `main` branch
that accepts only pull requests with green CI.
