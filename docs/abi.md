# CarvOS ABI (P2.1)

This document is the source of truth for the shared types in `carv-abi`. It describes the
pre-freeze ABI; P2.10 will freeze version 1 and record the decision in an ADR.

## Syscalls

`syscall` receives its number in `rax` and arguments in `rdi, rsi, rdx, r10, r8, r9`.
The currently assigned numbers are:

| Number | Name | Purpose |
|---:|---|---|
| 0 | `debug_putc` | Write one debug byte (debug builds only) |
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
