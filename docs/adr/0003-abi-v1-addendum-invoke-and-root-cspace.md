# ADR-0003: ABI v1 addendum — syscall results, `invoke` methods, IPC capability transfer and the root CSpace

- **Status:** Proposed (2026-10-06)
- **Deciders:** maintainer
- **Related:** P2.4–P2.9 (#59–#64), bug #141, ADR-0002; `docs/abi.md`, `crates/carv-abi`

## Context

ADR-0002 froze syscall numbers, error codes, the message layout and rights bits, but left open
what the kernel returns and in which registers, which methods `invoke` offers on each kernel
object, how a capability moves through a message, and what the root task's initial CSpace holds.
Running code in ring 3 (P2.5–P2.9) needs all four. Bug #141 also showed that transferring a
capability by copying it into another CSpace without a parent link skips the `GRANT` check and
escapes revocation. Finally, `docs/abi.md` called `debug_putc` "debug builds only" while the only
output a user program has before the console service (P3.4) is that syscall, and release-build
smoke and perf runs need `init` to report it is running.

## Decision

Additive only — no existing number, code, layout or bit changes, so the ABI stays version 1:

1. **Results:** status in `rax`, up to two results in `rdx` and `rsi`; `rcx`/`r11` clobbered;
   a ring-3 fault kills only the faulting thread.
2. **`invoke` methods** 0–8 (`DESCRIBE`, `DESTROY`, `BUDGET_READ`, `BUDGET_CREATE`,
   `THREAD_START`, `ADDRESS_SPACE_MAP`, `IRQ_CONTROL_GET`, `IRQ_BIND`, `IRQ_UNBIND`) and object
   types 1–9 (adding `Irq` and `IrqControl`), as tabled in `docs/abi.md`. Every object is created
   through a Budget capability and charged to it (or, for `Irq`, to the caller's budget), so there
   is no allocation without an account and no authority without a capability.
3. **Capability transfer:** attaching a capability needs `GRANT` on it; the receiver's copy is
   installed in its first empty slots as a child of the sender's capability in a cross-CSpace
   derivation tree (`carv_caps::CapSpaces`), so revoking the sender's capability removes it. This
   fixes #141.
4. **Root CSpace:** 64 slots — 0 root budget (untyped authority), 1 own thread, 2 own address
   space, 3 IRQ control; 4 stack pages below `0x7fff_0000_0000`; `USER_TOP = 0x7fff_ffff_f000`.
5. **`debug_putc`** is available in all builds until the console service exists; P3.4 revisits it.

## Alternatives considered

- Bumping to ABI v2: unnecessary, nothing previously assigned changes meaning.
- Returning results through the IPC buffer only: costs a memory copy for every `invoke`.
- A separate `create` syscall per object type: grows the syscall table instead of reusing `invoke`.
- Copying transferred capabilities as roots in the receiver: simpler, but is exactly bug #141.

## Consequences

`carv-abi` gains `ObjectType`, `method`, `map`, `init` and `USER_TOP`; user code and the future
`carv-rt` can rely on them. The kernel needs a CSpace collection with cross-space derivation. The
`debug_putc` row in `docs/abi.md` now says what the kernel does. Acceptance of this ADR by the
maintainer is required before P2 is signed off; until then its status stays *Proposed*.
