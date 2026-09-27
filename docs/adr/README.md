# Architecture Decision Records

One file per decision, numbered, never edited after acceptance except to change `Status`
(e.g. to *Superseded by ADR-000N*). A new ADR is required for any change to `docs/abi.md` after the
ABI freeze (P2.10), to a pillar design in `docs/PLAN.md` §3.3, or to a key technical decision in §2.
Propose one with the *Architecture decision proposal* issue template.

Format (Michael Nygard's): **Context** — what forces the decision; **Decision** — what we do;
**Alternatives considered**; **Consequences** — what gets easier, harder, or must change.

| ADR | Title | Status |
|---|---|---|
| [0001](0001-microkernel-rust-x86_64.md) | Capability microkernel in Rust for x86_64, booted by Limine, tested in QEMU | Accepted |
