# ADR-0001: Capability microkernel in Rust for x86_64, booted by Limine, tested in QEMU

- **Status:** Accepted (2026-09-25; recorded 2026-09-27)
- **Deciders:** maintainer
- **Related:** `docs/PLAN.md` §1–§3

## Context

CarvOS exists to make four things first-class that mainstream operating systems bolt on later:
explicit **capability authority**, **versioned storage** with provenance, **typed data** between
programs, and **resource budgets**. The project is a one-maintainer hobby built largely by AI coding
agents, tested only in virtual machines, with no compatibility obligations. The first decisions had to
pick a structure that lets the four pillars reinforce each other, a language and toolchain that catch
agent mistakes early, and a boot/test path that makes every change observable and automatable.

## Decision

1. **Kernel structure: a capability microkernel** ("chisel"). Drivers, the filesystem (`vstore`),
   networking and authentication run as isolated user-space services. The kernel owns only
   capabilities/CSpaces, IPC endpoints and notifications, threads and scheduling, address spaces,
   budgets, IRQ routing and the timer. Every operation on a kernel object is invoked through a
   capability the caller holds; the only capability-free syscalls are the scheduler's `yield` and the
   debug-build-only `debug_putc` (`docs/PLAN.md` §3.2). There is no global namespace and no root bypass.
2. **Language: Rust, pinned nightly**, `no_std` + `alloc`. Nightly is required for
   `custom_test_frameworks` and kernel-relevant features; the exact date is pinned in
   `rust-toolchain.toml` and bumped deliberately.
3. **Architecture: x86_64 only**, single core first (SMP designed for, implemented in Phase 10).
4. **Boot: Limine** (base revision 6, files pinned by commit and SHA-256), higher-half kernel at
   `0xffffffff80000000`, UEFI (OVMF) and BIOS both supported.
5. **Target platform: QEMU/KVM** with virtio devices only. No real-hardware drivers.
6. **Build orchestration: `cargo xtask`** (a Rust program), no Makefiles.

## Alternatives considered

- **Monolithic kernel** (Linux-style): faster path to a working shell, but capabilities would be an
  add-on rather than the boundary between components, and it splits poorly among parallel agents.
- **C**: the largest body of reference material (OSDev, xv6), but far weaker compiler help for the
  agents writing most of the code. **Zig**: attractive freestanding story, but a much smaller
  ecosystem and less training data.
- **RISC-V or aarch64**: cleaner ISAs, but x86_64 has the best QEMU/KVM support, runs natively on the
  maintainer's machine, and has the most documentation for the parts that hurt (APIC, ACPI, paging).
- **Writing our own bootloader** or **Multiboot2/GRUB**: Limine hands over a 64-bit higher-half
  kernel with memory map, HHDM and modules, and has a maintained Rust crate; a custom loader is weeks
  of work that teaches nothing about the four pillars.
- **Running on containers**: not possible — a container shares the host kernel.

## Consequences

- Each pillar becomes a small, separately testable component behind an IPC interface, which suits
  the one-task-per-PR agent workflow; the cost is IPC on every service call and a longer road to the
  first shell (Phase 6).
- Unsafe code is confined to the kernel and drivers; the pure `crates/*` libraries forbid it and are
  tested on the host. Every `unsafe` block must carry a `// SAFETY:` comment (enforced by clippy).
- The ABI (syscalls + IPC layout) must be frozen early (P2.10) so services and userland can proceed
  in parallel; changing it afterwards requires a new ADR and a version bump.
- The kernel's only observable output is the serial console. Logic that can live in a pure
  `crates/*` library is tested natively on the host; everything that needs the kernel — in-kernel
  tests, boot smoke and performance — is a QEMU boot whose serial output is asserted, and the
  in-kernel layer additionally reports pass/fail through the `isa-debug-exit` exit code.
- Portability to other architectures is deferred; the `arch/` module boundary exists so it is
  possible, not so it is easy.
