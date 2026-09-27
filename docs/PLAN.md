# CarvOS — Master Build Plan

> A hobby operating system in Rust for x86_64 that treats **authority, history, data, and resources**
> as built-in parts of the system, not add-ons.
> Target: QEMU/KVM virtual machines. Audience for this document: Claude / AI coding agents and the
> human maintainer.

**CARV** stands for **Capability Authority & Resource Versioning**: every action needs an explicit
capability (*authority*), every resource is budgeted and every change is kept (*resource versioning*).

### Naming conventions
| Form | Use it for |
|---|---|
| **CARV** | The acronym and the design itself ("the CARV model", "CARV capabilities"). |
| **CarvOS** | The operating system as a product: docs, banners, release names, the README title. |
| `carv-os` | The GitHub repository (`github.com/garrickdabbs/carv-os`), ISO and artifact file names. |
| `carv` | Code identifiers: crate prefix (`carv-abi`, `carv-rt`), ELF section (`.carv_schema`), CLI/tool names. |
| **chisel** | The kernel (crate and binary). A chisel is what you carve with; the kernel is the one tool that shapes capabilities. |

---

## 1. Problem Statement

Linux, Unix, and Windows were designed when one machine meant one trusted user running programs
they wrote or bought. That assumption causes four problems that today are patched with add-ons
instead of being fixed in the base design:

| # | Problem in Linux/Unix/Windows | Today's add-on patch | CARV's built-in answer |
|---|---|---|---|
| 1 | **Ambient authority.** Every program you run can read your SSH keys, delete your files, and open any network connection. Supply-chain attacks exploit this. | SELinux, AppArmor, sandboxes, Flatpak, UAC | **Capability security**: a process starts with *nothing* and can use only the handles it was explicitly given. Handles can be narrowed and revoked. |
| 2 | **Destructive, unexplained mutation.** Overwrites are permanent; nobody can answer "what changed, when, and which program did it?" Ransomware and config drift thrive. | btrfs/ZFS snapshots, backups, git for /etc, auditd | **Versioned object store**: every write is a transaction in a content-addressed history, and every commit records *who* (principal) and *what* (program hash) made it. Undo any change. |
| 3 | **Text-stream plumbing.** Tools talk in ad-hoc text that must be re-parsed (`ls | awk '{print $5}'`). This is fragile for people and worse for AI agents. | PowerShell, Nushell, `--json` flags | **Typed records everywhere**: IPC and shell pipelines carry schema-described structured values. Every command publishes a machine-readable schema. |
| 4 | **Resource usage is bolted on and hard to see.** Limits (cgroups, job objects) are optional and separate from identity; energy use is invisible. | cgroups, systemd slices, quotas, tc | **Budgets as kernel objects**: CPU time, memory, network bandwidth, storage, and estimated energy are always charged to a budget, and every process runs against one. |

**Prior art, stated honestly:** each idea exists somewhere (seL4/Fuchsia/KeyKOS for capabilities;
ZFS/NixOS/git for versioned state; PowerShell/Nushell for structured shells; cgroups/seL4-MCS for
budgets). CARV's contribution is **combining all four under one identity model**: a single
capability ties an action to a *principal*, the *budget* it's charged to, and the *provenance* stored
with the data it changes. Each pillar strengthens the others:

- Capabilities make provenance trustworthy (the kernel knows exactly which process, holding which
  authority, made each request).
- Budgets are themselves capabilities (delegating work = delegating a slice of your budget).
- Typed records make the history, budgets, and capabilities *queryable* (`log /etc | where program != "pkg"`).
- Versioned storage makes capability-granted writes *reversible*, so granting access is less scary.

### Non-goals (for now)
- POSIX compatibility or running Linux binaries.
- GUI, audio, USB, real hardware drivers (virtio only).
- SMP / multicore (design for it, implement single-core first; see Phase 10).
- Production-grade security claims. This is for fun and learning.

---

## 2. Key Technical Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Language | Rust (nightly, `no_std` + `alloc`) | Memory safety, strong compiler checks help agents catch their own mistakes, best hobby-OS ecosystem. |
| Architecture | x86_64 | Best QEMU/KVM support, runs natively on the Fedora host. |
| Kernel structure | **Capability microkernel** (seL4-inspired, simplified) | Capabilities are the core idea; drivers, filesystem, and network run as isolated user-space services; small components are easy for parallel agents to own and test. |
| Boot | **Limine** bootloader (UEFI + BIOS), `limine` crate | Hands over a 64-bit higher-half kernel with a memory map and framebuffer; skips real-mode boot code. |
| Firmware in VM | OVMF (UEFI) under QEMU | Modern path; BIOS kept as fallback. |
| Console | Serial (COM1, 16550 UART) first, framebuffer text later | Serial is scriptable, so automated tests read it. |
| Devices | virtio-blk, virtio-net, virtio-rng via the `virtio-drivers` crate (PCI transport) | Standard QEMU paravirtual devices, simple and well documented. |
| TCP/IP | `smoltcp` inside a user-space `netd` service | Mature `no_std` stack. Writing our own TCP is out of scope. |
| Hashing | BLAKE3 (`blake3` crate, `no_std`) | Fast content addressing for the object store. |
| Password hashing | Argon2id (`argon2` crate, `alloc` feature) | Current best practice. |
| Structured data wire format | CBOR via `minicbor` | Self-describing, compact, `no_std`. |
| Build orchestration | `cargo xtask` pattern (a Rust binary in the workspace) | One command to build, make the image, run, and test. No Makefile sprawl. |
| Image | ISO via `xorriso` + Limine; data disk as raw `.img` | Standard Limine workflow. |
| CI | GitHub Actions, QEMU (KVM if `/dev/kvm` exists, otherwise TCG) | Every PR boots the OS and runs the tests. |
| License | **MIT** (chosen 2026-09-27) | Simple, permissive, matches the hobby nature of the project. |
| Release artifacts | ISO + data image + kernel ELF, `SHA256SUMS`, CycloneDX SBOM, Sigstore keyless signature, SLSA provenance attestation | Verifiable downloads with no long-lived signing keys to protect. See §7.5. |
| Supply chain | Dependabot, `cargo deny`/`audit`/`vet`, SHA-pinned actions, least-privilege workflow permissions | An OS that fixes ambient authority shouldn't ship with ambient authority in its build pipeline. See §7.3. |

---

## 3. Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│ User space                                                           │
│                                                                      │
│  sh (typed shell)   ls cat cp mv rm mkdir log undo ps top caps       │
│  ping fetch netstat  whoami login passwd useradd grant budget ...    │
│           │  typed-record IPC (CBOR, schema-tagged)                  │
│  ─────────┼────────────────────────────────────────────────────────  │
│  Services: init │ svcmgr │ authd │ vstore (FS) │ netd │ blkd │ cons  │
│            (each is a separate process with only the caps it needs)  │
├──────────────────────────────────────────────────────────────────────┤
│ Kernel (ring 0) — "chisel"                                           │
│  capabilities & CSpaces │ IPC endpoints │ threads & scheduler        │
│  address spaces (paging) │ budgets (CPU/mem accounting)              │
│  IRQ → notification routing │ timer │ minimal ELF loader for init    │
└──────────────────────────────────────────────────────────────────────┘
          QEMU/KVM: OVMF, virtio-blk, virtio-net, 16550 serial, APIC, HPET
```

### 3.1 Kernel objects (all reached only through capabilities)

| Object | Purpose |
|---|---|
| `Thread` | Execution context (registers, state, bound Budget). |
| `AddressSpace` | Page tables. Map/unmap frames. |
| `Frame` | A range of physical memory (4 KiB / 2 MiB). |
| `Endpoint` | Synchronous IPC rendezvous (call/reply). |
| `Notification` | Async signal word (IRQs, events). |
| `CSpace` | A process's capability table. |
| `Budget` | CPU time (budget/period), memory byte limit, child-budget limits, counters. |
| `Irq` | Right to receive a specific hardware interrupt (drivers only). |
| `IoPort` / `Mmio` | Device access rights (drivers only). |
| `Reply` | One-shot reply capability made by `call`. |

A **capability** = (object ref, rights bitmask, badge). Rights: `READ, WRITE, GRANT, EXEC, DERIVE, REVOKE`.
Operations: `copy` (with rights ⊆ source), `mint` (add badge), `revoke` (destroy all derived caps,
tracked in a **derivation tree**), `delete`.

### 3.2 System call ABI (deliberately tiny)

Invoked with `syscall`; number in `rax`, args in `rdi, rsi, rdx, r10, r8, r9`; small messages passed in
registers, large ones via a per-thread IPC buffer page.

| # | Syscall | Notes |
|---|---|---|
| 0 | `debug_putc` | Debug builds only; removed later. |
| 1 | `send(cap, msg)` | |
| 2 | `recv(cap) -> (badge, msg)` | |
| 3 | `call(cap, msg) -> msg` | send + wait for reply |
| 4 | `reply_recv(reply, msg, cap)` | server fast path |
| 5 | `signal(ntfn)` / 6 `wait(ntfn)` | |
| 7 | `yield` | |
| 8 | `invoke(cap, method, args)` | All kernel-object operations (map frame, spawn thread, copy cap, read budget counters, ...) go through this: the object type decides what methods exist. |

**Every** operation that allocates kernel memory or frames charges the caller's `Budget`, and fails
with `E_BUDGET` when the budget is used up.

### 3.3 Pillar designs

#### A. Capability security & users
- **No global namespace.** A process does not "open /etc/passwd"; it is *handed* a directory
  capability from `vstore` and resolves paths relative to it. The shell *presents* the user's root
  directory capability as `/`.
- **Spawn = explicit grant.** `spawn(program, caps=[...], budget=slice)`. The shell passes a
  default set (stdin/stdout console caps, cwd dir cap) plus anything named with `--grant`.
  Example: `fetch https://x --grant net:tcp:443` gives it network access; without the flag it has none.
- **Attenuation.** Services support narrowing caps: a directory cap can become read-only, or limited
  to a subtree; a net cap can become "TCP to host H port P only" (a per-program firewall for free).
- **Users are principals.** A user = `{uid, name, argon2id hash, home dir cap, default budget, groups}`
  stored in `vstore` under `/system/users` (versioned, so account changes are auditable and
  reversible). `authd` holds the only cap to that data.
- **Login** = `authd` checks the password, then spawns the user's shell with that user's bundle
  (home cap, console cap, budget slice, badge = uid). The badge travels on every IPC, so services
  know the principal without trusting the client.
- **No all-powerful root.** "Admin" = holding specific caps (user DB write, system budget, service
  manager). `grant` (a sudo equivalent) asks `authd` for a time-limited, attenuated cap; each grant is
  recorded in the versioned log.

#### B. Versioned object store (`vstore`)
- **On-disk format (v1):** log-structured, copy-on-write, content-addressed.
  - Two superblocks (A/B, sector 0 and 1 MiB) with a sequence number and checksum; the newer valid
    one wins, so commits are atomic.
  - Objects: `Blob` (file data chunk, fixed 64 KiB in v1; content-defined chunking later),
    `Tree` (sorted entries: name → {kind, object hash, metadata}), `Commit`
    `{parent, root_tree, timestamp, principal uid, program hash, message, budget id}`.
  - Object ID = BLAKE3 hash. Identical data is stored once (deduplication).
  - Free space: append-only segments plus a garbage collector that keeps everything reachable from
    retained commits (retention policy: keep all for N days, then daily snapshots).
- **Transactions:** the client API offers `begin / write / commit` explicitly. For simple tools,
  each file close auto-commits. Multi-file changes (e.g. `useradd`) run in one transaction.
- **Provenance:** because the kernel badge identifies the principal and `svcmgr` records the
  program's hash at spawn, every commit is signed-off automatically: `log <path>` shows who and what.
- **Undo:** `undo <path> [--to <commit>|--before <time>]` for a file or tree, done as a new
  forward commit (history is never rewritten). `snapshot` names a commit.
- **Storage quota** is charged to the writing principal's budget.

#### C. Typed records & shell (`sh`)
- **Value model** (`carv-value` crate): `Null, Bool, Int, Float, Str, Bytes, Time, Duration, Size,
  Path, CapRef, List, Record, Table`.
- **Schema:** every program embeds a schema section in its ELF (`.carv_schema`): name, args, input
  type, output type, required caps. `help <cmd>` and `describe <cmd> --json` read it. This also
  gives AI agents a tool manifest for free.
- **Pipelines** pass CBOR-encoded Values over IPC channels, not bytes. The shell renders values at
  the end (table, tree, or `--json`). Byte streams still exist as `Bytes` for `cat` and friends.
- **Syntax sketch:**
  ```
  ls /home | where size > 1MB | sort-by modified -r | take 5
  ps | where cpu > 10% | get name,pid,budget
  log /system/users | first 3
  undo notes.txt --before 10m
  fetch http://10.0.2.2:8000/ --grant net:tcp:10.0.2.2:8000 | get status
  budget show | where kind == "net"
  ```
- Built-in operators: `where, get, sort-by, take, first, count, to-json, from-json, each, select`.

#### D. Budgets & accounting
- **CPU:** each Thread binds to a Budget with `(budget_ns, period_ns)`. The scheduler is
  round-robin across ready threads that have budget left, refilled every period (sporadic-server
  style, simplified). Counters: consumed ns, throttle events.
- **Memory:** frames and kernel objects are charged to a byte limit, with a hard fail at the limit
  (no OOM killer, since allocation is explicit).
- **Hierarchy:** budgets form a tree. A child's limits are carved out of its parent's. Delegating
  work to a service can pass a budget slice so the service's work is billed to *you* (this fixes the
  "who is really using the CPU" problem for daemons).
- **Network:** `netd` charges bytes to the budget named by the requesting cap's badge, with token-bucket
  rate limits per budget.
- **Storage:** `vstore` charges bytes stored.
- **Energy (estimate):** `energy_mJ ≈ cpu_ns·k_cpu + net_bytes·k_net + disk_bytes·k_disk` with
  configurable coefficients. It is clearly labeled an *estimate* (a VM can't measure real power), but
  every principal can see it.
- **Observability:** `top`, `ps`, `budget show` return typed tables; the kernel exposes counters via
  `invoke(budget_cap, READ_STATS)`.

---

## 4. Repository Layout (Cargo workspace)

```
carv-os/
├── .github/
│   ├── workflows/
│   │   ├── ci.yml             # per-PR: lint, host tests, kernel + integration tests in QEMU, all-green gate
│   │   ├── nightly.yml        # fuzz, Miri, audit, geiger, stress loops, toolchain drift, nightly pre-release
│   │   ├── release.yml        # tag v* → reproducible ISO, tests, SBOM, Sigstore signature, attestation, Release
│   │   ├── codeql.yml         # code scanning (public repo)
│   │   └── scorecard.yml      # OpenSSF Scorecard (public repo)
│   ├── dependabot.yml         # cargo + github-actions, weekly, grouped
│   ├── release.yml            # release-notes categories by task prefix
│   ├── CODEOWNERS
│   ├── PULL_REQUEST_TEMPLATE.md
│   └── ISSUE_TEMPLATE/        # task.yml, bug.yml, adr.yml
├── CLAUDE.md                  # agent rules (see that file)
├── README.md
├── CHANGELOG.md               # Keep-a-Changelog; every PR adds an Unreleased line
├── SECURITY.md                # disclosure process; "hobby OS, no guarantees"
├── LICENSE
├── deny.toml                  # cargo-deny: advisories, license allow-list, bans, sources
├── docs/
│   ├── PLAN.md                # this document
│   ├── adr/                   # Architecture Decision Records: NNNN-title.md
│   ├── abi.md                 # syscall + IPC ABI (source of truth once frozen)
│   └── vstore-format.md       # on-disk format spec
├── rust-toolchain.toml        # pinned nightly + rust-src, llvm-tools
├── .cargo/config.toml
├── xtask/                     # cargo xtask build|image|run|test|gdb|release-check
├── kernel/                    # "chisel": the microkernel (bin, x86_64-unknown-none)
├── crates/                    # no_std libraries, host-testable with `cargo test`
│   ├── carv-abi/             # syscall numbers, message layouts, error codes (shared)
│   ├── carv-caps/            # capability derivation tree logic (pure)
│   ├── carv-value/           # Value model + CBOR codec + schema types
│   ├── carv-vstore-core/     # object store format, tree/commit logic (pure, on a BlockDevice trait)
│   ├── carv-budget/          # accounting math, token buckets (pure)
│   └── carv-rt/              # user-space runtime: _start, syscalls, allocator, IPC helpers, spawn
├── services/
│   ├── init/                  # first user process; starts svcmgr
│   ├── svcmgr/                # spawns & supervises services, records program hashes
│   ├── cons/                  # serial/framebuffer console server
│   ├── blkd/                  # virtio-blk driver
│   ├── vstore/                # filesystem service over blkd
│   ├── netd/                  # virtio-net + smoltcp
│   └── authd/                 # users, login, grants
├── userland/
│   ├── sh/                    # typed shell
│   └── coreutils/             # ls cat cp mv rm mkdir stat log undo ps top caps whoami ...
└── tests/
    ├── integration/           # host-side harness that boots QEMU and drives serial
    └── fixtures/
```

Rule: **logic that can live in a pure `crates/*` library must**. That keeps most code testable
on the host at full speed, with the kernel and services as thin glue.

---

## 5. Development Environment (Fedora host)

```bash
sudo dnf install -y qemu-system-x86 edk2-ovmf xorriso git python3-pexpect
curl https://sh.rustup.rs -sSf | sh     # if rustup not installed
# rust-toolchain.toml pins nightly; components: rust-src, llvm-tools-preview, rustfmt, clippy
cargo install cargo-fuzz               # later phases
```

`cargo xtask run` → builds everything, makes `target/carv-os.iso` (UEFI + BIOS, Limine 11.4.1 with every
file SHA-256 pinned in `xtask`) plus a blank `target/data.img`, and starts:
```
qemu-system-x86_64 -machine q35 -cpu max -m 512M -accel kvm \
  -drive if=pflash,unit=0,format=raw,readonly=on,file=/usr/share/edk2/ovmf/OVMF_CODE.fd \
  -drive if=pflash,unit=1,format=raw,file=target/OVMF_VARS.fd \
  -cdrom target/carv-os.iso -boot d \
  -drive file=target/data.img,if=none,id=d0,format=raw -device virtio-blk-pci,drive=d0 \
  -netdev user,id=n0,hostfwd=tcp::5555-:23 -device virtio-net-pci,netdev=n0 \
  -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
  -serial stdio -display none -no-reboot
```
Flags: `--release`, `--bios` (SeaBIOS instead of OVMF), `--debug` (`-d int,cpu_reset,guest_errors -D target/qemu.log`),
`--timeout SECS` (kill QEMU and exit 124; every automated run uses this). `xtask` probes the usual OVMF
locations (override with `CARV_OVMF_CODE`/`CARV_OVMF_VARS`) and falls back to `-accel tcg` when
`/dev/kvm` is not usable, e.g. in CI.

---

## 6. Testing Strategy (four layers)

1. **Host unit tests**: `cargo test -p carv-caps -p carv-value -p carv-vstore-core -p carv-budget`.
   Pure logic, run in milliseconds. Property tests with `proptest` for the cap derivation tree
   (revoke removes every descendant), vstore (crash at any write, then recover to the last commit),
   and CBOR round-trips.
2. **In-kernel tests**: a custom test framework (`#![feature(custom_test_frameworks)]`) boots the
   kernel in QEMU, runs `#[test_case]` functions, and exits through `isa-debug-exit` with a pass or
   fail code. Covers paging, allocator, IPC, scheduler, and budget enforcement.
3. **System integration tests**: `tests/integration` boots the full image, talks to the serial shell
   (pexpect in Python, or Rust `rexpect`), sends commands, and checks the output. The shell's
   `--json` mode makes these checks exact rather than regex-based. Each phase adds scenarios (see
   the acceptance criteria below).
4. **Fuzzing (Phase 9+)**: `cargo fuzz` targets for the CBOR decoder, vstore image parser, and the
   syscall-argument validator (host-compiled).

**Crash-consistency test:** the harness kills QEMU at random points during write-heavy
workloads, reboots, and checks that vstore mounts at a consistent commit.

**CI:** every layer runs on GitHub Actions; the workflows, security controls, nightly jobs, and
release pipeline are specified in §7.

**Single command:** `cargo xtask test` runs layers 1–3. Agents must run it before claiming a task is done.

---

## 7. GitHub Build, Security & Release Infrastructure

Everything the OS needs from GitHub to go from a PR to a signed, bootable release. This is
infrastructure, so it is delivered as Phase 0 tasks (P0.5, P0.7, P0.8) and hardened in Phase 9
(P9.6, P9.7). All workflow files live under `.github/` (layout in §4).

### 7.1 Repository governance
- **Ruleset on `main`** (Settings → Rules): require a pull request, require the `all-green` status
  check, block force-pushes and deletion, require conversation resolution. *Not* linear history:
  PRs merge with **merge commits** so stacked PRs retarget cleanly (learned in Phase 0; squashing a
  stacked base makes the next PR conflict).
  Set **"do not allow bypass"** so even the maintainer merges through CI. Solo maintainer: approvals
  are *not* required (you'd be approving your own PRs), CI is the gate.
- **Agent credentials.** Agents run on the maintainer's machine using the maintainer's `gh` login,
  which has admin rights. Give agents a **fine-grained PAT** (or a GitHub App) scoped to this repo
  with only `contents: write` and `pull_requests: write`, no admin. Rulesets then bind agents
  mechanically: they can push branches and open PRs but can never merge to `main` without CI.
- **`CODEOWNERS`:** `kernel/`, `crates/carv-abi/`, `docs/abi.md`, `docs/adr/`, `.github/`, `deny.toml`
  → maintainer. Changes there get a review request automatically.
- **Templates:** PR template (task ID, AC checklist, pasted `cargo xtask test` output, unsafe-block
  count delta); issue templates for *task*, *bug (with serial log)*, and *ADR proposal*.
- **Labels & board:** `phase:P0`…`phase:P10`, `area:kernel|services|userland|crates|infra`,
  `kind:task|bug|adr|security`. One GitHub Issue per task ID, mirrored on a Project board with
  columns *Backlog → In progress → In review → Done*. The §11 status table summarizes the board.
- **PR titles** are `<TaskID>: <title>` (e.g. `P2.7: IPC send/recv/call`); the release notes
  generator groups by that prefix.

### 7.2 Build CI — `.github/workflows/ci.yml`
Triggers: `pull_request`, `push` to `main`, `merge_group`, `workflow_dispatch`.
`concurrency: { group: ci-${{ github.ref }}, cancel-in-progress: true }` for PRs.

| Job | What it does | Timeout |
|---|---|---|
| `lint` | `cargo fmt --check`; `cargo clippy --all-targets -D warnings` for host crates *and* `-p chisel --target x86_64-unknown-none`; `cargo doc --no-deps` with `RUSTDOCFLAGS=-D warnings`; `cargo deny check` | 10 min |
| `host-tests` | `cargo nextest run --workspace --locked` (layer 1), JUnit report uploaded and summarized in `$GITHUB_STEP_SUMMARY` | 15 min |
| `boot-smoke` | install `qemu-system-x86 ovmf xorriso`; enable KVM (below); `cargo xtask smoke --timeout 120` boots UEFI, BIOS and `panic-test` scenarios and checks the serial output; uploads `target/smoke/*.log` | 30 min |
| `docs-gate` (PRs only) | `cargo xtask docs-gate --base origin/<base>`: code changes must carry `CHANGELOG.md`; `xtask` changes must update README/CLAUDE.md/PLAN; workflow changes must update PLAN §7/README; `docs/abi.md` changes need an ADR. Skipped for Dependabot PRs (the bump PR is its own record) and when the PR body contains `docs-gate: skip` (with a reason). | 10 min |
| `kernel-tests` (from P0.4) | `cargo xtask test --kernel` (layer 2, `isa-debug-exit`) | 20 min |
| `integration` (from P0.4) | `cargo xtask test --integration` (layer 3); uploads `target/serial.log`, `qemu.log`, and the ISO on failure | 30 min |
| `image` (push to `main` only) | `cargo xtask image --release`; uploads `carv-<sha>.iso` as a 7-day artifact so the maintainer can boot the latest `main` | 15 min |
| `all-green` | `needs:` every gate job, `if: always()`, fails unless each is `success` (`docs-gate` may be `skipped`). **This is the only required status check**, so adding jobs never means touching the ruleset. | 1 min |

Details:
- **Toolchain** comes from `rust-toolchain.toml`: CI runs plain `rustup toolchain install`, which
  installs exactly the pinned nightly, components and target, so CI and laptops always agree and no
  third-party toolchain action is needed. `--locked` everywhere; `Cargo.lock` is committed.
- **Caching:** `Swatinem/rust-cache` keyed on the lockfile and toolchain; Limine binaries are fetched
  by **pinned release tag + SHA-256** that `xtask` verifies before use (never "latest").
- **KVM on GitHub-hosted Linux runners** (nested virtualization is available on `ubuntu-latest`):
  ```
  echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' | sudo tee /etc/udev/rules.d/99-kvm4all.rules
  sudo udevadm control --reload-rules && sudo udevadm trigger --name-match=kvm
  ```
  `xtask` still falls back to TCG (`-accel tcg`) when `/dev/kvm` is missing and scales test
  timeouts ×4, so the suite also passes on forks and other CI.
- **Every QEMU run has a hard timeout** (`timeout` in xtask *and* `timeout-minutes` on the job).
  A hang is a failure, never a retry. No automatic re-runs: flaky tests get fixed, not retried.
- **Reproducible builds** (P9.6): release profile sets `strip = "debuginfo"` on the ISO copy,
  `--remap-path-prefix`, `SOURCE_DATE_EPOCH` from the commit date, and `xorriso`
  `-volume_date all =<epoch>`; a CI job builds twice on different runners and asserts identical
  ISO hashes.
- **Self-hosted runner:** *not* used. Your Fedora box would be faster (real KVM), but a self-hosted
  runner on a public repo executes PR code from anyone who forks. Stay on GitHub-hosted runners.

### 7.3 Security
| Control | Tool / setting | Where |
|---|---|---|
| Dependency updates | **Dependabot** for `cargo` and `github-actions`, weekly, grouped minor/patch | `.github/dependabot.yml` |
| Vulnerable / unlicensed / duplicate deps | **`cargo deny`** (`advisories`, `licenses` allow-list: MIT, Apache-2.0, BSD-2/3, ISC, Zlib, Unicode-3.0; `bans` on duplicate versions; `sources` = crates.io only) | `deny.toml`, `lint` job |
| Fresh advisories without a code change | **`cargo audit`** on the nightly schedule (advisory DB moves even when code doesn't) | `nightly.yml` |
| Unsafe-code discipline (mechanizes the CLAUDE.md rule) | clippy `#![deny(clippy::undocumented_unsafe_blocks)]` + `#![deny(unsafe_op_in_unsafe_fn)]` in every crate; `cargo geiger` unsafe-count report posted to the PR summary; `unsafe` forbidden (`#![forbid(unsafe_code)]`) in pure `crates/*` except `carv-rt` | crate roots, `lint` job |
| Undefined behaviour in host-testable code | **Miri**: `cargo miri test` on the pure crates (`carv-caps`, `carv-value`, `carv-vstore-core`, `carv-budget`) | `nightly.yml` |
| Fuzzing (P9.3) | `cargo fuzz` for 20 min per target nightly; corpus cached as an artifact; a crash uploads the input and opens/updates an issue labeled `kind:security` | `nightly.yml` |
| Actions supply chain | Every action pinned to a **full commit SHA** (Dependabot bumps them); top-level `permissions: contents: read`, elevated per job only (`id-token: write`, `attestations: write`, `contents: write` for release); no long-lived secrets anywhere, signing is keyless via OIDC | all workflows |
| Static analysis | **CodeQL** code scanning with the Rust extractor; **OpenSSF Scorecard** workflow with a README badge, target ≥ 7 by Phase 9 | `codeql.yml`, `scorecard.yml` |
| Secrets & reporting | Secret scanning + push protection; Dependabot alerts; **private vulnerability reporting** enabled; `SECURITY.md` with the disclosure process and a plain statement that this is a hobby OS with no security guarantees | repo settings, `SECURITY.md` |
| Kernel hardening tests | W^X, guard pages, SMEP/SMAP, user/kernel isolation each have an in-kernel test that CI runs (P9.4) | `kernel-tests` job |
| Threat model | `docs/threat-model.md` (P9.5) lists assets, trust boundaries (kernel ↔ services ↔ user programs ↔ network), and which controls above cover each threat | `docs/` |

**Visibility note:** CodeQL, secret scanning push protection, and Scorecard are **free only for
public repositories**. While the repo is private, Dependabot and `cargo deny`/`audit` still work;
the rest activates when the repo goes public (recommended once Phase 0 lands).

### 7.4 Test infrastructure on GitHub
- **Documentation gate.** `cargo xtask docs-gate` compares the change set against the merge-base
  and fails (exit 2) when code moved without its documentation (rules in the `docs-gate` row
  above; the logic is a pure, unit-tested function in `xtask`). It runs twice: in CI on every PR,
  and as a **Claude Code Stop hook** (`.claude/settings.json`) so an agent cannot end a task with
  undocumented code. Escape hatch: `docs-gate: skip` plus a reason in the PR body.
- **Per-PR:** layers 1–3 (§6) via `ci.yml`. Integration tests talk to the shell in `--json` mode so
  assertions are exact. Failures always upload the serial log, QEMU log, and the ISO that failed.
- **Nightly — `.github/workflows/nightly.yml`** (`schedule: cron '0 6 * * *'` + `workflow_dispatch`):
  long-running and drift-detecting jobs that are too slow or too noisy for PRs:
  - crash-consistency loop: 200 randomized kill-and-recover cycles against `vstore`
  - fuzzing (20 min/target), Miri, `cargo audit`, `cargo geiger`
  - **toolchain drift:** build and test with `nightly` (unpinned) and `beta` so a future toolchain
    bump has no surprises; failures here are informational (`continue-on-error`)
  - a stress boot: 50 consecutive boots must all reach the shell (catches races)
  - On failure the workflow creates or updates a single pinned issue **"Nightly is failing"**
    with links to the run, and closes it when green again.
- **Test reporting:** `cargo-nextest` emits JUnit for host tests; the QEMU harness emits the same
  format for kernel and integration tests; both are rendered into the job summary and kept as
  artifacts for 30 days so regressions can be diffed.
- **Coverage:** `cargo llvm-cov` on the pure crates only (kernel coverage isn't practical); reported
  in the summary, no hard threshold, trend tracked in the nightly issue.
- **Hermetic tests:** integration tests that need internet (`P8.4` DNS) are marked
  `#[ignore = "needs-network"]` and run only in nightly with `--include-ignored`.

### 7.5 Releases — `.github/workflows/release.yml`
- **Versioning:** SemVer `0.y.z` while pre-1.0; the single source is `[workspace.package] version`.
  `cargo xtask release-check` fails if the git tag ≠ Cargo version or `CHANGELOG.md` lacks an entry.
- **Cadence:** tagged releases at milestones: `v0.1.0` boots to serial banner (end of Phase 0),
  `v0.2.0` MVP 1 "Hello, CarvOS" (Phase 6), `v0.3.0` MVP 2 (Phase 8), `v0.4.0` hardening (Phase 9).
  Plus a rolling **`nightly` pre-release** re-tagged by `nightly.yml` when it is green, so there is
  always a bootable image of the latest `main`.
- **Changelog:** `CHANGELOG.md` in Keep-a-Changelog format. Each PR adds a line under
  *Unreleased*; the release PR moves it under the version heading. Release notes are generated
  from merged PR titles grouped by task prefix (`gh release create --generate-notes` +
  `.github/release.yml` categories).
- **Release flow** (triggered by pushing a `v*` tag, which only the maintainer can do because tag
  creation is covered by the ruleset):
  1. `release-check`, then a **reproducible** `cargo xtask image --release`
  2. run the **full test suite against the exact release artifacts** (not a rebuild)
  3. boot-smoke: boot the release ISO under QEMU and require the banner and a shell prompt
  4. produce artifacts: `carv-os-v0.y.z-x86_64.iso`, `carv-os-v0.y.z-data.img.zst`,
     `chisel-v0.y.z.elf` (unstripped kernel with symbols, for debugging crash reports),
     `SHA256SUMS`, SBOM `carv-os-v0.y.z.cdx.json` (`cargo cyclonedx`)
  5. **sign & attest:** Sigstore keyless signature of `SHA256SUMS` (`cosign sign-blob` via OIDC,
     `.sigstore` bundle) and **SLSA build provenance** via `actions/attest-build-provenance` for
     every artifact, so `gh attestation verify carv-os-*.iso -R garrickdabbs/carv-os` proves it came
     from this repo's workflow at that commit
  6. `gh release create` with generated notes, marked *pre-release* while `0.y.z`
- **Verifying a download** (documented in README):
  `sha256sum -c SHA256SUMS`, `cosign verify-blob --bundle SHA256SUMS.sigstore ...`,
  `gh attestation verify`.
- **Hotfixes:** tag from `main`; no release branches before 1.0. A bad release is *yanked* by
  marking it as such in the release notes and publishing the fixed version, never by deleting or
  re-uploading artifacts (published hashes are immutable).

---

## 8. Phased Roadmap

Each task has an ID, a deliverable, and **acceptance criteria (AC)** a machine can check.
Phases are ordered by dependency. Tasks marked ∥ can run in parallel with other agents once their
phase's interfaces are frozen.

### Phase 0 — Scaffolding & Boot (target: "hello" over serial)
| ID | Task | AC |
|---|---|---|
| P0.1 | Workspace, `rust-toolchain.toml`, `.cargo/config.toml`, target `x86_64-unknown-none`, kernel linker script (higher half) | `cargo xtask build` succeeds |
| P0.2 | `xtask`: build, fetch/pin Limine, make ISO, run QEMU, detect KVM | `cargo xtask run` boots to the Limine menu, then the kernel |
| P0.3 | Serial driver (16550) + `kprintln!` macro + panic handler printing to serial | Serial shows `CarvOS chisel v0.0.1 booting` |
| P0.4 | `isa-debug-exit` + in-kernel test framework | `cargo xtask test` runs a trivial test, QEMU exits with the pass code |
| P0.5 | `ci.yml` per §7.2: lint (fmt, clippy both targets, doc, `cargo deny`), host tests (nextest + JUnit), kernel tests and integration tests in QEMU with KVM enabled, `all-green` gate; `deny.toml`, `dependabot.yml`; all actions SHA-pinned with least-privilege `permissions` | PR shows a single required `all-green` check; a deliberately failing kernel test uploads `serial.log`; Dependabot opens its first PR |
| P0.6 | `README.md` (build, run, verify-a-release sections), `docs/adr/0001-microkernel-rust-x86_64.md`, `CHANGELOG.md`, `SECURITY.md`, `LICENSE` | Files exist; README badges for CI and Scorecard render |
| P0.7 | Repo governance per §7.1: `main` ruleset (PR + `all-green` required, linear history, no bypass), `CODEOWNERS`, PR and issue templates, labels, Project board, fine-grained PAT for agents | Direct push to `main` is rejected; a PR without green CI can't merge; agent token can't merge |
| P0.8 | `release.yml` per §7.5 + `cargo xtask release-check`: reproducible ISO, tests against artifacts, boot-smoke, SHA256SUMS, SBOM, Sigstore signature, build provenance attestation, GitHub Release with generated notes. `nightly.yml` skeleton (toolchain drift, `cargo audit`, rolling `nightly` pre-release) | Tag `v0.1.0` produces a release whose ISO boots to the banner and passes `gh attestation verify`; two runs produce identical ISO hashes |

### Phase 1 — Kernel Core
| ID | Task | AC |
|---|---|---|
| P1.1 | GDT + TSS (with IST stacks for double fault), IDT, exception handlers | Test triggers `int3`, handler runs; forced stack overflow → double-fault message, not triple fault |
| P1.2 | Physical frame allocator from the Limine memory map (bitmap or buddy) | Kernel test: allocate/free 10k frames, no duplicates, count restored |
| P1.3 | Paging: kernel mapper using the HHDM offset, map/unmap/translate | Test maps a fresh page, writes, reads back, unmaps → page fault handler catches access |
| P1.4 | Kernel heap (`linked_list_allocator` or custom slab) + `alloc` | `Vec`/`Box`/`BTreeMap` work in kernel tests |
| P1.5 | Local APIC + timer (calibrated against HPET/PIT), disable legacy PIC | Timer ticks at 1 kHz; test counts ticks over a busy wait |
| P1.6 | ACPI table parsing (`acpi` crate) for MADT/HPET/MCFG | Boot log lists APIC ID(s) and HPET address |
| P1.7 | PCI(e) enumeration via ECAM | Boot log lists virtio-blk and virtio-net devices |

### Phase 2 — Capabilities, Threads, IPC, Budgets (the heart; freeze the ABI at the end)
| ID | Task | AC |
|---|---|---|
| P2.1 | `carv-abi` crate: syscall numbers, error enum, message layout, rights bits | Host-compiles; documented in `docs/abi.md` |
| P2.2 | `carv-caps` (pure): CSpace, capability slots, copy/mint/revoke, derivation tree | Proptest: after revoke(c), no descendant of c is reachable; rights never grow |
| P2.3 | `carv-budget` (pure): CPU budget/period refill, memory limit, hierarchical carve-out, token bucket | Host tests: child limits never exceed parent; refill math correct over simulated time |
| P2.4 | Kernel objects: Thread, AddressSpace, Frame, Endpoint, Notification, Budget, Reply | Kernel tests create and destroy each object via `invoke`; memory charged/credited to the budget |
| P2.5 | Context switch, ring-3 entry (`sysretq`/`iretq`), `syscall`/`sysret` setup (STAR/LSTAR/SFMASK), SMAP/SMEP on | User-mode test thread calls `debug_putc`; user access to kernel page faults |
| P2.6 | Scheduler: round-robin within budgets, preemption on timer, throttling on budget use-up | Test: two spinning threads with 30%/70% budgets measure within ±5% of that |
| P2.7 | IPC: send/recv/call/reply_recv, badges, cap transfer in messages | Ping-pong test between two user threads; cap passed over IPC is usable by the receiver; round-trip latency printed |
| P2.8 | IRQ → Notification routing, `Irq` caps | Timer or serial IRQ delivered to a user-space waiter |
| P2.9 | Minimal ELF loader for the root task (`init`) from a Limine module | `init` runs in ring 3 with its initial CSpace (all "untyped" authority) |
| P2.10 | **ABI freeze**: ADR-0002, `docs/abi.md` marked v1 | Maintainer approves; later changes need a new ADR |

### Phase 3 — User-Space Runtime & Service Manager
| ID | Task | AC |
|---|---|---|
| P3.1 | `carv-rt`: `_start`, syscall wrappers, user heap (grows by mapping frames from the budget), panic → exit, `println!` via a console cap | Hello-world user program built separately from the kernel |
| P3.2 | IPC helper layer: typed request/response (`#[derive]`-based or hand-rolled), server loop macro | Echo service + client test |
| P3.3 | `spawn(elf, caps, budget)` in `carv-rt` + `svcmgr` (loads ELFs from a Limine-module initrd for now, records BLAKE3 of each binary, restarts crashed services) | `svcmgr` starts `cons`; killing `cons` → restarted, logged |
| P3.4 | `cons` console service: serial input line discipline, output; each client gets its own console cap | Two processes print through `cons` without garbled output |

### Phase 4 — Drivers ∥
| ID | Task | AC |
|---|---|---|
| P4.1 ∥ | `blkd`: virtio-blk over PCI via `virtio-drivers`, DMA frames from its budget, block read/write IPC API | Integration test: write pattern to sector N, reboot, read it back |
| P4.2 ∥ | `netd` driver half: virtio-net RX/TX | Receives packets (ARP from QEMU's user net seen in the log) |
| P4.3 ∥ | virtio-rng → `rand` service (seeds for Argon2 salts and TCP ISNs) | 1 KiB of random output, not all zero, differs across boots |
| P4.4 ∥ | RTC (CMOS) → wall-clock time service | `date` shows a value within 1 minute of the host |

### Phase 5 — Versioned Object Store ∥ (starts once P2.10 is done; `-core` can start right after Phase 0)
| ID | Task | AC |
|---|---|---|
| P5.1 | `docs/vstore-format.md` spec + ADR | Reviewed |
| P5.2 | `carv-vstore-core` (pure, on a `BlockDevice` trait with a RAM implementation for tests): objects, trees, commits, A/B superblocks, mkfs, open, path lookup | Host tests: create/read/rename/delete; dedup of identical blobs |
| P5.3 | Crash-consistency: a simulated device drops writes after a random point; recovery | Proptest: after any simulated crash, open() gives exactly the last fully committed state |
| P5.4 | History API: log(path), read_at(path, commit), undo (forward commit), snapshots, GC with retention | Host tests for each; GC never deletes reachable objects |
| P5.5 | `vstore` service: directory/file capabilities, attenuation (read-only, subtree), transactions, provenance from badge + svcmgr hash, storage charged to budget | Integration: file written by user A shows `principal=A program=<hash of cp>` in `log` |
| P5.6 | `mkfs` via xtask on the host (reuses `-core` compiled for std) to prepare `data.img` with `/system`, `/home` | Fresh image boots and mounts |

### Phase 6 — Typed Shell & Core Commands
| ID | Task | AC |
|---|---|---|
| P6.1 | `carv-value`: Value enum, CBOR codec, schema types, table renderer | Host round-trip proptests; golden-file render tests |
| P6.2 | ELF `.carv_schema` section: macro to embed, reader in the shell | `describe ls --json` prints the schema |
| P6.3 | `sh`: line editing (history, backspace), parser (pipes, strings, flags, `--grant`), spawner, pipeline wiring with typed channels, render + `--json` | Integration: `echo 1 2 3 | count` → `3` |
| P6.4 | Built-in operators: `where get select sort-by take first count each to-json from-json` | Golden tests per operator |
| P6.5 | File coreutils: `ls cd pwd cat mkdir rm cp mv stat touch write` | Integration script: create tree, list with filters, copy, remove |
| P6.6 | History coreutils: `log undo snapshot diff` | Integration: write, overwrite, `undo --before`, content restored; `log` shows 3 commits |
| P6.7 | Process coreutils: `ps kill caps help clear echo date uptime` | `caps` lists the shell's own capabilities as a table |

**Milestone "Hello, CarvOS" (MVP 1):** boot to a shell over serial; manage files with full history.

### Phase 7 — Users & Authentication
| ID | Task | AC |
|---|---|---|
| P7.1 | `authd`: user records in `/system/users` (one transaction per change), Argon2id hashes, first-boot setup creates the `admin` user (password from the kernel cmdline in test builds, prompt otherwise) | Record created; hash verifies |
| P7.2 | `login` flow owned by `cons`/`authd`: prompt, verify, spawn shell with the user's bundle and badge; lockout after N failures | Integration: bad password rejected, good one gives a shell; `whoami` correct |
| P7.3 | `useradd userdel passwd groups whoami logout` | Integration: admin adds `alice`; alice can't read `/home/admin`; alice's writes show `principal=alice` |
| P7.4 | `grant`: time-limited attenuated capability from `authd` per policy file (`/system/policy`), recorded in history | Integration: alice is denied writing `/system/policy` until admin policy allows `grant`; the grant expires |
| P7.5 | Multiple sessions: a second login over a TCP port (after Phase 8) or a second serial port | Two users logged in at once, isolated |

### Phase 8 — Networking
| ID | Task | AC |
|---|---|---|
| P8.1 | `netd` + smoltcp: interface, DHCP client (QEMU user net: 10.0.2.15/24, gw .2, DNS .3) | `ifconfig` shows the leased address |
| P8.2 | Socket IPC API as capabilities: `NetCap` with attenuation (proto, host, port ranges, listen vs connect) | Program without a NetCap gets `E_NOCAP`; with `tcp:10.0.2.2:8000` can't reach port 22 |
| P8.3 | `ping` (ICMP echo) returning a typed table (seq, ttl, rtt) | `ping 10.0.2.2 -c 3 | count` → 3 |
| P8.4 | DNS resolver (UDP) + `resolve` command | `resolve example.com` returns an address (needs host internet; skip-marked in offline CI) |
| P8.5 | `fetch` (minimal HTTP/1.1 client) → record `{status, headers, body}` | Host runs `python3 -m http.server`; `fetch` gets status 200 |
| P8.6 | `listen`/`telnetd`-style remote shell on port 23 (plaintext, test only, off by default) | Host `nc localhost 5555` gets a login prompt |
| P8.7 | `netstat` (typed: sockets, owner principal, bytes, budget) and network token-bucket limits per budget | Budget limited to 50 KB/s; a download measures ≤ 55 KB/s |

**Milestone MVP 2:** multi-user, networked, versioned, capability-secured system.

### Phase 9 — Accounting UX, Hardening, Fuzzing
| ID | Task | AC |
|---|---|---|
| P9.1 | `top` (refreshing typed table), `budget show/create/give/limit`, energy estimate per principal | Integration: a CPU hog under a 10% budget stays ≤ 12% |
| P9.2 | Budget delegation over IPC (a client passes a budget slice with a request, and the service's work is billed to the client) | `vstore` CPU time appears under the caller's budget |
| P9.3 | Fuzz targets (CBOR, vstore parser, syscall validator); fix all crashes | 1 hour of fuzzing per target with no crashes in CI nightly |
| P9.4 | Kernel hardening: guard pages, W^X for all mappings, stack canaries where supported, KASLR-lite via Limine | Tests confirm W^X (write to a code page faults) |
| P9.5 | Security review against a threat model doc (`docs/threat-model.md`) | Review doc + issues filed |
| P9.6 | Reproducible-build verification job (two runners, identical ISO hash), `cargo llvm-cov` on pure crates, Miri + `cargo geiger` in nightly, crash-consistency and 50-boot stress loops in nightly | Nightly green for 7 consecutive days; geiger count tracked in the summary |
| P9.7 | Supply-chain hardening: CodeQL and OpenSSF Scorecard workflows (repo public), `cargo vet` audits for kernel and driver dependencies, `SECURITY.md` process exercised once with a mock report | Scorecard ≥ 7; `cargo vet` passes with no unaudited kernel deps |

### Phase 10 — Stretch Ideas (pick for fun)
- SMP: per-CPU run queues, IPIs, TLB shootdown.
- Framebuffer console with a font, then a tiny compositor.
- Content-defined chunking plus `vstore` sync between two CarvOS VMs (replication = pushing commits, like git).
- Package manager where each package declares the capabilities it needs (install shows a
  "this program wants: net:tcp:*:443, read ~/Documents" prompt).
- A WASM runtime as the user-program format (portable and sandboxed by design).
- An agent API: expose command schemas plus a capability-scoped session so an AI agent can operate
  the OS safely: every action is typed, limited by budget, recorded, and reversible.

---

## 9. Parallelization Map for Multiple Agents

```
P0 ──► P1 ──► P2 (ABI freeze) ──┬──► P3 ──┬──► P4.1 blkd ──► P5.5 vstore svc ──► P6.5/6.6 ──► P7 ──► P9
                                 │         ├──► P4.2 netd ──────────────────────► P8 ────────┘
                                 │         └──► P3.4 cons ──► P6.3 sh ──► P6.4/6.7
 (from P0) P5.2–P5.4 vstore-core ┘  (pure crates can start early, host-tested)
 (from P0) P6.1 carv-value, P2.2 carv-caps, P2.3 carv-budget
```

Recommended team shape (max ~3–4 concurrent agents to limit merge conflicts):
- **Agent K (kernel):** P0 → P1 → P2 → P9.4, in order. Owns `kernel/`, `carv-abi`.
- **Agent L (libraries):** P2.2, P2.3, P5.2–P5.4, P6.1 as pure crates, starting on day one.
- **Agent S (services):** P3 → P4 → P5.5 → P8, after the ABI freeze.
- **Agent U (userland):** P6.2–P6.7 → P7, after P3.
- **Reviewer agent:** runs `/code-review` on each PR before the maintainer merges.

Each agent works on a branch named `p<phase>.<task>-short-name` and opens one PR per task ID.

---

## 10. Agent Handoff Prompt Template

Paste this to a Claude agent for each task:

```
You are working on CarvOS (repo: github.com/garrickdabbs/carv-os). Read CLAUDE.md and docs/PLAN.md first.

Task: <ID> — <title>
Acceptance criteria: <copy AC from PLAN.md>
Scope: only touch <dirs>. Do not change docs/abi.md without an ADR.

Deliver:
1. Implementation with doc comments on public items.
2. Tests proving each acceptance criterion (host tests where possible).
3. `cargo xtask test` passing locally — paste the summary output in the PR.
4. Update docs/PLAN.md task status table (Section 11) to "review".
5. Open a PR titled "<ID>: <title>" describing design choices and anything left undone.
If blocked by a design question, write it up in the PR and stop rather than guessing.
```

---

## 11. Status Tracker

| Phase | Status | Notes |
|---|---|---|
| P0 Scaffolding & Boot | in progress | P0.1–P0.2 done; P0.3 in review; P0.5 (CI, deny, Dependabot, docs gate) and P0.6 (README, CHANGELOG, SECURITY, LICENSE done; ADR-0001 pending); P0.7 ruleset pending repo visibility; P0.8 not started |
| P1 Kernel Core | not started | |
| P2 Caps/IPC/Budgets | not started | |
| P3 Runtime & svcmgr | not started | |
| P4 Drivers | not started | |
| P5 Versioned Store | not started | |
| P6 Shell & Coreutils | not started | |
| P7 Users & Auth | not started | |
| P8 Networking | not started | |
| P9 Hardening | not started | |

(Better: mirror these as GitHub Issues with a Project board; one issue per task ID, labeled by phase.)

---

## 12. Risks & Mitigations

| Risk | Mitigation |
|---|---|
| Triple faults / silent hangs are hard for agents to debug | Serial logging from the first instruction; QEMU `-d int,cpu_reset -D qemu.log` in `xtask run --debug`; gdb stub (`-s -S`) and an `xtask gdb` helper; timeout on every QEMU test run |
| Scope creep: four pillars is a lot | MVP 1 needs only caps + vstore + shell; budgets can start as "count only, no enforcement" and gain enforcement in P9 |
| ABI churn breaks parallel work | Freeze at P2.10; changes require an ADR and a version bump in `carv-abi` |
| Unsafe Rust sprawl | `#![deny(unsafe_op_in_unsafe_fn)]`; every `unsafe` block needs a `// SAFETY:` comment; keep unsafe inside the kernel and driver modules |
| Filesystem corruption | Pure core with proptested crash consistency *before* it touches a disk; A/B superblocks; checksums on every object |
| Agents claim "done" without proof | AC are machine-checkable; CI must be green; PR template requires pasted test output |
| Nightly Rust breakage | Pin an exact nightly date in `rust-toolchain.toml`; bump deliberately |
| Compromised dependency or GitHub Action | `cargo deny` sources = crates.io only, `cargo vet` for kernel deps, actions pinned to commit SHAs, Dependabot reviews every bump, least-privilege `permissions` |
| Agent merges or tags without CI | Ruleset with no bypass on `main` and `v*` tags; agents use a fine-grained PAT that cannot merge or tag |
| Tampered or mistaken release download | Reproducible builds, `SHA256SUMS`, Sigstore signature, SLSA provenance; releases are never re-uploaded, only superseded |
| CI slow or flaky under TCG emulation | KVM enabled on GitHub Linux runners; TCG fallback scales timeouts; no retries, so flakes surface and get fixed |

---

## 13. Reference Material for Agents
- Philipp Oppermann, *Writing an OS in Rust* (blog_os): boot, paging, interrupts, allocators, testing.
- OSDev Wiki: APIC, ACPI, PCI, virtio, 16550 UART.
- seL4 manual and whitepaper: capability derivation, IPC, MCS scheduling contexts.
- Fuchsia Zircon docs: handles and rights (a capability model close to CARV's).
- Limine protocol spec (`limine` crate docs).
- virtio 1.2 specification (OASIS).
- smoltcp examples; Nushell documentation (structured pipelines); git internals (object model).
