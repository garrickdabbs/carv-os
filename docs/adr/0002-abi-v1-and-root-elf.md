# ADR-0002: Version-1 ABI and root-task ELF contract

- **Status:** Accepted (2026-09-30)
- **Deciders:** maintainer
- **Related:** P2.1, P2.9, P2.10; `docs/abi.md`

## Context

The root task is supplied as a Limine module, while the syscall and IPC types are being developed
by P2.1. The kernel needs a safe, allocation-free ELF validation boundary before ring-3 address
spaces and syscall glue are complete. Freezing the wire-level rules now prevents services and
`carv-rt` from silently depending on implementation details.

## Decision

`docs/abi.md` is the version-1 source of truth. Syscalls use the documented register convention,
fixed-width little-endian records, and capability rights; unknown numbers and malformed messages
are rejected. The root module must be ELF64, little-endian x86-64, `ET_EXEC` or already-relocated
`ET_DYN`, and contain page-compatible `PT_LOAD` segments. Writable and executable segments are
rejected. The loader maps zeroed pages, copies file bytes, and starts at an executable entry point.
The loader exposes a `SegmentMapper` boundary; P2.5 supplies the concrete user address-space
implementation and P2.1 supplies the generated ABI types.

## Alternatives considered

- Mapping the module as a flat blob: simpler, but violates W^X and gives user code file padding.
- Relocating arbitrary PIE images in the kernel: adds a dynamic linker and mutable policy to chisel.
- Freezing ABI after P3: would let independent runtime and service work drift.

## Consequences

The parser is `no_std`, allocation-free, and testable without booting. It does not yet start `init`
because the ring-3 context and address-space objects belong to P2.5. A future ABI change requires a
new ADR and a version bump; the P2.1 crate must implement the records described in `docs/abi.md`.
