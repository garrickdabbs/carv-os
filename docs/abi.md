# CarvOS ABI — version 1

**Status: frozen by ADR-0002 (2026-09-30).** This document is the source of truth for the shared
types in `carv-abi`. P2.1 provides the corresponding representation-stable Rust definitions;
future changes must update this document and the crate together.

## Syscalls

`syscall` receives its number in `rax` and arguments in `rdi, rsi, rdx, r10, r8, r9`.
The currently assigned numbers are:

| Number | Name | Purpose |
|---:|---|---|
| 0 | `debug_putc` | Write one debug byte to the kernel serial console (all builds until the console service, P3.4; see ADR-0003) |
| 1 | `send` | Send an IPC message |
| 2 | `recv` | Receive an IPC message |
| 3 | `call` | Send and wait for a reply |
| 4 | `reply_recv` | Reply and receive (server fast path) |
| 5 | `signal` | Signal a notification |
| 6 | `wait` | Wait for a notification |
| 7 | `yield` | Yield the current thread |
| 8 | `invoke` | Invoke a method on a kernel object |

## Errors

Errors are returned as nonzero `u64` values. Zero is reserved for success. `Error` currently
assigns codes 1–9 to invalid operation, invalid capability, permission denied, invalid argument,
would block, closed endpoint, budget exhausted, out of memory, and message too large, respectively.
Unknown values must be handled as an unassigned error.

## IPC message layout

`Message` is `#[repr(C)]` and 96 bytes, aligned to 8 bytes:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | operation-specific `label` |
| 8 | 8 | `info`: word count in bits 0–7, capability count in bits 8–15 |
| 16 | 48 | six `u64` payload words |
| 64 | 32 | four `u64` capability handles |

Only the counts encoded in `info` are valid. Small messages use registers; larger protocol data
will use the per-thread IPC buffer in a later task.

## Capability rights

Rights are a six-bit `u8` mask:

| Bit | Name | Meaning |
|---:|---|---|
| 0 | `READ` | Read an object |
| 1 | `WRITE` | Modify an object |
| 2 | `GRANT` | Grant a capability |
| 3 | `EXEC` | Execute an object |
| 4 | `DERIVE` | Copy/derive a capability |
| 5 | `REVOKE` | Revoke descendants |

Undefined bits are rejected by `Rights::from_bits`; rights can only be attenuated by later
capability operations.

## Root task ELF contract

Limine supplies `init` as a module. The kernel accepts ELF64, little-endian, current-version,
x86-64 `ET_EXEC` or already-relocated `ET_DYN` images. Each `PT_LOAD` must fit in the module,
have `p_filesz <= p_memsz`, use page-compatible offset/address alignment, and not be both writable
and executable. `p_memsz - p_filesz` is zero-filled. The entry must lie in an executable segment.
The initial CSpace contains the system's untyped authority; subsequent authority is transferred
only through capabilities.

## Addendum: syscall results, `invoke` methods and the root CSpace (ADR-0003)

This addendum is additive: it assigns meaning to registers and values that v1 left unspecified
and changes none of the numbers, codes, layouts or bits above.

**Results.** Every syscall returns its status in `rax` (0 or an `Error` code) and up to two
results in `rdx` and `rsi`. `rcx` and `r11` are clobbered by `syscall`/`sysret`; every other
register is preserved. A thread that faults in ring 3 is killed; the kernel keeps running.

**IPC registers.** `send`/`call` take the endpoint slot in `rdi` and a pointer to a `Message` in
user memory in `rsi`; `recv` takes the endpoint slot and a pointer to a `Message` to fill;
`reply_recv` takes the endpoint slot, a pointer to the reply `Message` (the reply is sent to the
thread whose `call` was last received) and the same buffer receives the next message. `recv` and
`reply_recv` return the sender's badge in `rdx` and the label in `rsi`; `call` returns the reply
in its buffer and the reply's label in `rsi`. `signal` takes a notification slot and counts one
signal; `wait` blocks until the count is nonzero, returns it in `rdx` and resets it to zero.
Interrupts bound to a notification signal it the same way. Rights: `send`/`call`/`signal` need `WRITE`, `recv`/`wait` need `READ`, and
attaching capabilities to a message needs `GRANT` on each of them. Transferred capabilities land
in the receiver's first empty slots as *children* of the sender's capabilities (revoking the
sender's capability revokes them), and the received message's capability words name those slots.

**`invoke`.** `invoke(cap, method, a0, a1, a2, a3)` takes the slot in `rdi`, the method in `rsi`
and arguments in `rdx, r10, r8, r9`.

| Method | Name | Object | Effect |
|---:|---|---|---|
| 0 | `DESCRIBE` | any | `rdx` = object type, `rsi` = rights bits |
| 1 | `DESTROY` | any but the root budget | destroys the object and credits its budget (`WRITE`) |
| 2 | `BUDGET_READ` | Budget | `rdx` = memory used, `rsi` = memory limit (`READ`) |
| 3 | `BUDGET_CREATE` | Budget | creates object type `a0` in empty slot `a1`, charged to this budget; a child budget gets CPU `a2` ns per period and `a3` bytes, carved out of this one (`WRITE`) |
| 4 | `THREAD_START` | Thread | starts at `rip = a0`, `rsp = a1`, `rdi = a2` in the address space in slot `a3` (`u64::MAX` = the caller's), sharing the caller's CSpace (`WRITE`) |
| 5 | `ADDRESS_SPACE_MAP` | AddressSpace | maps a zeroed frame at page `a0` with flags `a1` (bit 0 write, bit 1 exec; both together rejected) (`WRITE`) |
| 6 | `IRQ_CONTROL_GET` | IrqControl | creates an `Irq` capability for GSI `a0` in empty slot `a1`, charged to the caller's budget (`WRITE`) |
| 7 | `IRQ_BIND` | Irq | delivers the line to the notification in slot `a0` and unmasks it (`WRITE`) |
| 8 | `IRQ_UNBIND` | Irq | masks the line and drops the binding (`WRITE`) |

Object types: 1 Thread, 2 AddressSpace, 3 Frame, 4 Endpoint, 5 Notification, 6 Budget, 7 Reply,
8 Irq, 9 IrqControl. The kernel refuses to destroy a budget that still pays for objects or an
address space a live thread uses.

**Root CSpace.** `init` starts with a 64-slot CSpace: slot 0 the root budget (all CPU and the
memory left after boot — the system's untyped authority), slot 1 its own thread, slot 2 its
address space, slot 3 the IRQ control capability; slots 4–63 are empty. Its stack is 4 pages
below `0x7fff_0000_0000`; user pages lie below `0x7fff_ffff_f000`.

## Compatibility

This is v1. Any change to syscall numbers, error values, message layout, rights bits, or the root
task contract requires a new ADR and a version bump.
