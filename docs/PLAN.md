# CarvOS — Master Build Plan

> A hobby operating system in Rust for x86_64 that treats **authority, history, data, and resources**
> as built-in parts of the system, not add-ons.
> Target: QEMU/KVM virtual machines. Audience for this document: Claude / AI coding agents and the
> human maintainer.

**CARV** stands for **Capability Authority & Resource Versioning**: every action needs an explicit
capability (*authority*), every resource is budgeted and every change is kept (*resource versioning*).

> **Status (2026-10-01):** Phases 0 and 1 complete (`v0.1.1` released, reproducible across machines);
> Phase 2 in progress: the pure crates P2.1–P2.3 are done; P2.4–P2.10 are partial (kernel-side
> foundations only, nothing runs in ring 3 yet; P2.4 and P2.10 were reopened after review). Epics
> exist for every phase (#46, #55, #66, #78, #83, #90, #104, #110, #118). Details in §11.

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
│   ├── workflows/              # one workflow per README badge (§7.2)
│   │   ├── build.yml          # `build`: fmt, clippy (host + kernel), debug/release build, host tests, ISO artifact
│   │   ├── ci.yml              # `boot-smoke`: cargo xtask test — host + in-kernel tests, UEFI/BIOS/panic smoke in QEMU
│   │   ├── security.yml       # `security`: cargo deny, cargo audit, unsafe inventory, SHA-pinned-action check
│   │   ├── docs.yml            # `docs`: docs-gate, rustdoc -D warnings, lychee markdown link check
│   │   ├── perf.yml            # `perf`: cargo xtask perf size/boot-time budgets
│   │   ├── codeql.yml         # code scanning (public repo), advisory only
│   │   ├── scorecard.yml      # OpenSSF Scorecard (public repo), advisory only
│   │   ├── nightly.yml        # stress loops, perf budgets, cargo audit, toolchain drift, nightly pre-release, pinned "Nightly is failing" issue (§7.4); fuzz/Miri/geiger planned but not yet wired in (§7.3, P9.6)
│   │   └── release.yml        # tag v* → reproducible ISO, tests, SBOM, Sigstore signature, attestation, Release
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
2. **In-kernel tests**: a custom test framework (`#![feature(custom_test_frameworks)]`, `kernel/src/test.rs`)
   boots the kernel in QEMU, runs `#[test_case]` functions, and exits through `isa-debug-exit` with a pass
   (33) or fail (35) code that `cargo xtask test --kernel` maps to success/failure. Will cover paging,
   allocator, IPC, scheduler, and budget enforcement as they land.
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

**Single command:** `cargo xtask test` runs layers 1–3 (`--host`, `--kernel`, `--integration` select a subset). Agents must run it before claiming a task is done.

---

## 7. GitHub Build, Security & Release Infrastructure

Everything the OS needs from GitHub to go from a PR to a signed, bootable release. This is
infrastructure, so it is delivered as Phase 0 tasks (P0.5, P0.7, P0.8) and hardened in Phase 9
(P9.6, P9.7). All workflow files live under `.github/` (layout in §4).

### 7.1 Repository governance
- **Ruleset on `main`** (Settings → Rules): require a pull request, require the status checks
  `build`, `boot-smoke`, `security`, `docs` and `perf` (one per README badge), block force-pushes
  and deletion, require conversation resolution. *Not* linear history:
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
  columns *Backlog → Ready → In progress → In review → Done*. The §11 status table summarizes the board.
- **Board mechanics (set up 2026-09-27):** the *CarvOS Prototype* user project (#3) with Status
  *Backlog / Ready / In progress / In review / Done*, 14-day iterations (Iteration 1 from 2026-09-26;
  Phase N's tasks sit in Iteration N, when that iteration exists — only Iterations 1–5 are
  configured as of 2026-10-01 (through 2026-12-05), so Phase 6's epic (#90) is already Backlog with
  no iteration, and Phase 7–9 epics/tasks are Backlog with no iteration too, until Iterations 6–9
  are added). Phase epics #46 (P1), #55 (P2), #66 (P3), #78 (P4), #83 (P5), #90 (P6), #104 (P7), #110
  (P8) and #118 (P9) hold one sub-issue per task ID with the AC copied from §8; the epic's sub-issue
  counter is the phase progress. Built-in project workflows do the automatic moves: auto-add on creation,
  item closed → Done, PR merged → Done, PR linked to issue, auto-close issue when set to Done,
  auto-add sub-issues. Agents set *In progress* and *In review* with `gh` and link PRs with
  `Closes #<task>` + `task: <ID>` in the body. No repository workflow touches the board: `GITHUB_TOKEN`
  cannot write a user project and no secrets are stored (the logging-only `project-link.yml` attempt
  was dropped in #45). All work is assigned to the maintainer until other contributors join.
- **PR titles** are `<TaskID>: <title>` (e.g. `P2.7: IPC send/recv/call`); the release notes
  generator groups by that prefix.
- **Review flow (learned 2026-09-27).** Two reviewers post *after* a PR opens or is pushed: GitHub
  Copilot code review (inline threads, typically within 5–10 minutes) and the maintainer's
  monitoring loop, which files issues against fresh commits (#38, #40, #43). An agent therefore
  waits **at least 15 minutes** after opening or pushing a PR, answers/fixes/resolves every thread
  (the ruleset refuses to merge with unresolved conversations) and only then merges with a merge
  commit. **Never enable auto-merge on a fresh PR**: it once fired before 26 review threads existed;
  they were audited and resolved afterwards in #41.
- **Concurrent agent sessions** on one machine each work in their own `git worktree`
  (`~/carv-os-<name>`), never in another session's checkout; no bare `git stash`; background
  watchers only read state through `gh` and act through the API (see CLAUDE.md).

### 7.2 Build, test and quality workflows — one per README badge
Each concern is its own workflow so it has its own badge, its own required check, and its own
history. All trigger on `pull_request`, `push` to `main`, `merge_group` and `workflow_dispatch`;
`concurrency` cancels superseded PR runs; top-level `permissions: contents: read`.

| Workflow (badge) | Job (required check) | What it does | Timeout |
|---|---|---|---|
| **Build** `build.yml` | `build` | `cargo fmt --check`; clippy `-D warnings` for host crates *and* `-p chisel --target x86_64-unknown-none`; `cargo xtask build` debug and `--release` (higher-half layout verified); `cargo test --workspace --locked`; `cargo xtask image --release`; ISO uploaded as a 7-day artifact on `main` | 20 min |
| **CI** `ci.yml` | `boot-smoke` | install `qemu-system-x86 ovmf xorriso`; enable KVM (below); `cargo xtask test --timeout 120`: host unit tests, the in-kernel test kernel booted in QEMU (`isa-debug-exit` 33/35), and the UEFI/BIOS/`panic-test` smoke scenarios with serial output asserted; uploads `target/test/*.log` and `target/smoke/*.log`. Job name stays `boot-smoke` (required check) | 30 min |
| **Security** `security.yml` | `security` | `cargo deny check` (advisories, licenses, bans, sources); `cargo audit --deny warnings` with a fresh RustSec DB; clippy with only `undocumented_unsafe_blocks` as an error; unsafe inventory in the job summary; every `uses:` pinned to a 40-hex SHA; no `write-all` permissions. Also runs **weekly** so new advisories surface without a code change | 15 min |
| **Docs** `docs.yml` | `docs` | `docs-gate` run from the **base branch's** xtask against the PR checkout (a PR cannot weaken the gate it is judged by); requires *new content under Unreleased* in CHANGELOG.md, a *new* ADR file for `docs/abi.md`, and a skip marker only with a ≥ 20-character reason (PRs; Dependabot exempt); rustdoc with `RUSTDOCFLAGS=-D warnings` (crate roots deny `missing_docs`); `lychee --offline` checks every local Markdown link and anchor — or, in a release PR, a new `## [x.y.z]` version heading | 15 min |
| **Performance** `perf.yml` | `perf` | `cargo xtask perf --runs 5`: release-kernel loaded size, ISO size, boot-to-banner and banner-to-halt (median, KVM) against budgets in `xtask`; over budget fails; report to the job summary | 30 min |
| **CodeQL** `codeql.yml` | `codeql (rust)`, `codeql (actions)` | GitHub code scanning of the Rust sources and of the workflow files; PRs, `main`, weekly. Advisory (not a required check) | 45 min |
| **Scorecard** `scorecard.yml` | `scorecard` | OpenSSF Scorecard on `main` and weekly; publishes results for the README badge and to code scanning. Advisory | 15 min |

The docs-gate rules: code changes must add content under `## [Unreleased]` in `CHANGELOG.md` (a release
PR satisfies this by adding the new `## [x.y.z]` heading the entries moved under); `xtask`
changes must update README/CLAUDE.md/PLAN; workflow changes must update PLAN §7/README; `docs/abi.md`
changes need a *new* ADR file. Skipped for Dependabot PRs (the bump PR is its own record) or with
`docs-gate: skip — <reason ≥ 20 chars>` in the PR body.

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
- **Reproducible builds** (P9.6, landed early for #29): the release profile sets cargo's
  `trim-paths = "all"` (no checkout, registry or sysroot path in the ELF), `cargo xtask image
  --release` strips debuginfo from the ISO's kernel copy (`llvm-objcopy` from `llvm-tools-preview`),
  and `SOURCE_DATE_EPOCH` from the commit date pins every xorriso timestamp. The release job builds
  twice — the second time from a different directory with a fresh `CARGO_HOME` — and asserts
  identical ISO and kernel hashes.
- **Self-hosted runner:** *not* used. Your Fedora box would be faster (real KVM), but a self-hosted
  runner on a public repo executes PR code from anyone who forks. Stay on GitHub-hosted runners.

### 7.3 Security
| Control | Tool / setting | Where |
|---|---|---|
| Dependency updates | **Dependabot** for `cargo` and `github-actions`, weekly, grouped minor/patch | `.github/dependabot.yml` |
| Vulnerable / unlicensed / duplicate deps | **`cargo deny`** (`advisories`, `licenses` allow-list: MIT, Apache-2.0, BSD-2/3, ISC, Zlib, Unicode-3.0; `bans`: duplicate versions are an *error*; `sources` = crates.io only) | `deny.toml`, `security` job |
| Fresh advisories without a code change | **`cargo audit`** on the nightly schedule (advisory DB moves even when code doesn't) | `nightly.yml` |
| Unsafe-code discipline (mechanizes the CLAUDE.md rule) — *partially live* | clippy `#![deny(clippy::undocumented_unsafe_blocks)]` + `#![deny(missing_docs)]` in every crate root — *live*; `#![deny(unsafe_op_in_unsafe_fn)]` in the kernel crate root (`kernel/src/main.rs`), the only crate with `unsafe fn`s today — *live*; `unsafe` forbidden (`#![forbid(unsafe_code)]`) in pure `crates/*` except `carv-rt` — *live*; `cargo geiger` unsafe-count report posted to the PR summary — **deferred to P9.6** (not in any workflow yet, consistent with §11 "deferred to later tasks") | crate roots, `build.yml` (clippy) and `security.yml` (unsafe inventory) |
| Undefined behaviour in host-testable code | **Miri**: `cargo miri test` on the pure crates (`carv-caps`, `carv-value`, `carv-vstore-core`, `carv-budget`) | `nightly.yml` |
| Fuzzing (P9.3) | `cargo fuzz` for 20 min per target nightly; corpus cached as an artifact; a crash uploads the input and opens/updates an issue labeled `kind:security` | `nightly.yml` |
| Actions supply chain | Every action pinned to a **full commit SHA** (Dependabot bumps them); top-level `permissions: contents: read`, elevated per job only (`id-token: write`, `attestations: write`, `contents: write` for release); no long-lived secrets anywhere, signing is keyless via OIDC | all workflows |
| Static analysis | **CodeQL** code scanning (Rust + Actions) on PRs, `main` and weekly; **OpenSSF Scorecard** weekly with a README badge, target ≥ 7 by Phase 9 — *both live* | `codeql.yml`, `scorecard.yml` |
| Secrets & reporting | Secret scanning + push protection, Dependabot alerts + security updates, **private vulnerability reporting** — *all enabled 2026-09-27 when the repo went public*; `SECURITY.md` with the disclosure process and a plain statement that this is a hobby OS with no security guarantees | repo settings, `SECURITY.md` |
| Kernel hardening tests | W^X, guard pages, SMEP/SMAP, user/kernel isolation each have an in-kernel test that CI runs (P9.4) | `kernel-tests` job |
| Threat model | `docs/threat-model.md` (P9.5) lists assets, trust boundaries (kernel ↔ services ↔ user programs ↔ network), and which controls above cover each threat | `docs/` |

**Visibility:** the repository has been public since 2026-09-27, so CodeQL, secret-scanning push
protection, Scorecard and rulesets are all available at no cost.

### 7.4 Test infrastructure on GitHub
- **Documentation gate.** `cargo xtask docs-gate` compares the change set against the merge-base
  and fails (exit 2) when code moved without its documentation (rules in the `docs-gate` row
  above; the logic is a pure, unit-tested function in `xtask`; a release PR satisfies the CHANGELOG
  rule by adding the new `## [x.y.z]` heading instead of an Unreleased line). CI runs it on every PR from the
  *base branch's* xtask; agents run it locally before opening a PR. A Claude Code Stop hook that runs
  the gate is an optional, **uncommitted** personal setting (`.claude/settings.local.json`;
  `.claude/` is gitignored): a committed settings file would run shell commands on every
  contributor's machine, so repository policy lives in CI only (decided 2026-09-27). Escape hatch:
  `docs-gate: skip` plus a reason in the PR body.
- **Per-PR:** layer 1 via `build.yml` and `ci.yml`, layers 2–3 via `ci.yml` (`cargo xtask test`), plus the Security, Docs and Performance workflows. Integration tests talk to the shell in `--json` mode so
  assertions are exact. Failures always upload the serial log, QEMU log, and the ISO that failed.
- **Nightly — `.github/workflows/nightly.yml`** (`schedule: cron '0 6 * * *'` + `workflow_dispatch`):
  long-running and drift-detecting jobs that are too slow or too noisy for PRs:
  - crash-consistency loop: 200 randomized kill-and-recover cycles against `vstore`
  - fuzzing (20 min/target), Miri, `cargo audit`, `cargo geiger`
  - **toolchain drift:** build and test with `nightly` (unpinned) and `beta` so a future toolchain
    bump has no surprises; failures here are informational (`continue-on-error`)
  - a stress boot: 10 consecutive UEFI+BIOS rounds today (50 boots to a shell prompt from Phase 6);
    the loop checks `cargo xtask smoke`'s exit code directly, never through `grep`
  - On failure the workflow creates or updates a single pinned issue **"Nightly is failing"**
    with links to the run, and closes it when green again. Implemented as a separate
    `report-status` job (`needs: [nightly, publish]`, `if: always()`) that holds only
    `issues: write` — never `contents` — so it can run even when the test jobs fail.
- **Test reporting (deferred):** host tests run with plain `cargo test` today; `cargo-nextest` with
  JUnit output for host, kernel and integration tests, rendered into the job summary and kept as
  artifacts for 30 days, is planned for P9.6.
- **Coverage:** `cargo llvm-cov` on the pure crates only (kernel coverage isn't practical); reported
  in the summary, no hard threshold, trend tracked in the nightly issue.
- **Hermetic tests:** integration tests that need internet (`P8.4` DNS) are marked
  `#[ignore = "needs-network"]` and run only in nightly with `--include-ignored`.

### 7.5 Releases — `.github/workflows/release.yml`
- **Versioning:** SemVer `0.y.z` while pre-1.0; the single source is `[workspace.package] version`.
  `cargo xtask release-check` fails if the git tag ≠ Cargo version or `CHANGELOG.md` lacks an entry.
- **Cadence:** tagged releases at milestones: `v0.1.0` boots to serial banner (end of Phase 0),
  `v0.2.0` MVP 1 "Hello, CarvOS" (Phase 6), `v0.3.0` MVP 2 (Phase 8), `v0.4.0` hardening (Phase 9).
  Interim `v0.1.z` releases are cut on request between milestones (`v0.1.1`, 2026-09-27: Phase 1
  progress and reproducible builds); the milestone numbers stay reserved.
  Plus a rolling **`nightly` pre-release** re-tagged by `nightly.yml` when it is green, so there is
  always a bootable image of the latest `main`.
- **Changelog:** `CHANGELOG.md` in Keep-a-Changelog format. Each PR adds a line under
  *Unreleased*; the release PR moves it under the version heading. Release notes are generated
  from merged PR titles grouped by task prefix (`gh release create --generate-notes` +
  `.github/release.yml` categories).
- **Release flow** (triggered by pushing a `v*` tag; in practice only the maintainer does this today
  because no `v*` tag ruleset exists yet — see §11 "Open maintainer decisions" — and agents never
  create rulesets):
  1. `release-check`, then a **reproducible** `cargo xtask image --release` — `xtask` sets
     `SOURCE_DATE_EPOCH` to the commit time for xorriso, and the workflow builds twice — the second
     time from another directory with a fresh `CARGO_HOME` — and requires identical SHA-256s
  2. run the full test suite (`cargo xtask test --release`) at the tagged commit; the in-kernel tests
     necessarily use a separate test kernel, so "against the exact artifact" means step 3
  3. boot-smoke: `cargo xtask smoke --iso <release iso>` boots the exact artifact under UEFI and BIOS
     and requires the banner (and, from Phase 6, a shell prompt)
  4. produce artifacts: `carv-os-v0.y.z-x86_64.iso`, `carv-os-v0.y.z-data.img.zst`,
     `chisel-v0.y.z.elf` (unstripped kernel with symbols, for debugging crash reports),
     `SHA256SUMS`, SBOMs `carv-os-v0.y.z-<crate>.cdx.json`, one CycloneDX document per workspace
     crate (`cargo cyclonedx --describe crate`)
  5. **sign & attest:** Sigstore keyless signature of `SHA256SUMS` (`cosign sign-blob` via OIDC,
     retried up to 4 times with backoff because Fulcio/Rekor occasionally reset the connection, #76;
     `.sigstore.json` bundle) and **SLSA build provenance** via `actions/attest-build-provenance` for
     every artifact, so `gh attestation verify carv-os-*.iso -R garrickdabbs/carv-os` proves it came
     from this repo's workflow at that commit. The provenance is *also* published as release assets —
     `carv-os-vX.intoto.jsonl` (the DSSE envelope) and `carv-os-vX.provenance.sigstore.json` — for
     offline verifiers and for OpenSSF Scorecard, which only looks at assets.
     The workflow is split into a read-only `build` job (compilers, QEMU, third-party tools) and a
     `publish` job that alone holds `contents/id-token/attestations: write`; `nightly.yml` is split the
     same way.
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
| P0.1 | Workspace, `rust-toolchain.toml`, `.cargo/config.toml`, target `x86_64-unknown-none`, kernel linker script (higher half) | `cargo xtask build` succeeds — **done 2026-09-27** |
| P0.2 | `xtask`: build, fetch/pin Limine, make ISO, run QEMU, detect KVM | `cargo xtask run` boots to the Limine menu, then the kernel — **done 2026-09-27** |
| P0.3 | Serial driver (16550) + `kprintln!` macro + panic handler printing to serial | Serial shows `CarvOS chisel v0.0.1 booting` — **done 2026-09-27** |
| P0.4 | `isa-debug-exit` + in-kernel test framework | `cargo xtask test` runs a trivial test, QEMU exits with the pass code — **done 2026-09-27**: 5 `#[test_case]`s, exit 33/35 mapped by `xtask`, `test` runs host + kernel + smoke layers |
| P0.5 | Lint (fmt, clippy both targets, doc, `cargo deny`), host tests (`cargo test`; nextest + JUnit deferred to P9.6), kernel tests and integration tests in QEMU with KVM enabled, five required checks — now delivered as `build.yml`, `ci.yml`, `security.yml`, `docs.yml` and `perf.yml` per §7.2 rather than one `ci.yml`; `deny.toml`, `dependabot.yml`; all actions SHA-pinned with least-privilege `permissions` | PR shows the five required checks (`build`, `boot-smoke`, `security`, `docs`, `perf`) green; a deliberately failing kernel test uploads `serial.log`; Dependabot opens its first PR — **done 2026-09-27**. History: an initial single required `all-green` check existed briefly before the split into one workflow per README badge (§7.2) |
| P0.6 | `README.md` (build, run, verify-a-release sections), `docs/adr/0001-microkernel-rust-x86_64.md`, `CHANGELOG.md`, `SECURITY.md`, `LICENSE` | Files exist; README badges for CI and Scorecard render — **done 2026-09-27** |
| P0.7 | Repo governance per §7.1: `main` ruleset (PR + the five required checks, merge commits only, no bypass), `CODEOWNERS`, PR and issue templates, labels, Project board, fine-grained PAT for agents | Direct push to `main` is rejected; a PR without all five checks green can't merge; agent token can't merge — **partial, 2026-09-27**: the live `main` ruleset (`gh api repos/garrickdabbs/carv-os/rulesets`) requires `build`, `boot-smoke`, `security`, `docs` and `perf`, allows merge commits only (not linear history/squash), requires review-thread resolution and has no bypass, matching §7.1; `CODEOWNERS`, templates, labels and the Project board are in place. **Not done:** the fine-grained agent PAT — agents still run under the maintainer's own admin `gh` login, so the "agent token can't merge" AC is unverified. Tracked in §11 "Open maintainer decisions" |
| P0.8 | `release.yml` per §7.5 + `cargo xtask release-check`: reproducible ISO, tests against artifacts, boot-smoke, SHA256SUMS, SBOM, Sigstore signature, build provenance attestation, GitHub Release with generated notes. `nightly.yml` (daily tests, stress, perf, audit, toolchain drift, rolling `nightly` pre-release) | Tag `v0.1.0` produces a release whose ISO boots to the banner and passes `gh attestation verify`; two runs produce identical ISO hashes — **pipeline landed 2026-09-27; validated with `v0.1.0-rc.1`, then `v0.1.0`** |

### Phase 1 — Kernel Core
| ID | Task | AC |
|---|---|---|
| P1.1 | GDT + TSS (with IST stacks for double fault), IDT, exception handlers | Test triggers `int3`, handler runs; forced stack overflow → double-fault message, not triple fault — **done 2026-09-27** (`x86_64` crate; `#BP` test; `double-fault-test` smoke scenario forces an unusable stack, since a recursion overflow is only deterministic once P1.3 adds a kernel-stack guard page — that test is added then) |
| P1.2 | Physical frame allocator from the Limine memory map (bitmap or buddy) | Kernel test: allocate/free 10k frames, no duplicates, count restored — **done 2026-09-27** (`crates/carv-frames` bitmap core, host-tested; kernel wrapper carves the bitmap from usable RAM via the HHDM) |
| P1.3 | Paging: kernel mapper using the HHDM offset, map/unmap/translate | Test maps a fresh page, writes, reads back, unmaps → page fault handler catches access — **done 2026-09-27** (`x86_64::OffsetPageTable` over Limine's CR3 tables; dynamic region at `0xffff9000_00000000`; `pfault` smoke scenario; the deferred recursion-overflow → `#DF` test now runs via a guard-paged stack, `sovflw`) |
| P1.4 | Kernel heap (`linked_list_allocator` or custom slab) + `alloc` | `Vec`/`Box`/`BTreeMap` work in kernel tests — **done 2026-09-27** (`kernel::mm::heap`: 1 MiB `linked_list_allocator` heap in the dynamic region behind the kernel `SpinLock`, bytes-in-use counter for future budgets) |
| P1.5 | Local APIC + timer (calibrated against HPET/PIT), disable legacy PIC | Timer ticks at 1 kHz; test counts ticks over a busy wait — **done 2026-09-27** (xAPIC via the HHDM, PIT-calibrated, vector 32; PICs remapped + masked; HPET calibration deferred to issue #126, filed now that P1.6 has landed and parses the HPET base address) |
| P1.6 | ACPI table parsing (`acpi` crate) for MADT/HPET/MCFG | Boot log lists APIC ID(s) and HPET address — **done 2026-09-27** (`platform::acpi` summary via the HHDM; ECAM regions kept for P1.7) |
| P1.7 | PCI(e) enumeration via ECAM | Boot log lists virtio-blk and virtio-net devices — **done 2026-09-27** (`platform::pci` over the MCFG regions from P1.6; headers read through one scratch mapping, drivers map their own device pages later) |

### Phase 2 — Capabilities, Threads, IPC, Budgets (the heart; freeze the ABI at the end)
| ID | Task | AC |
|---|---|---|
| P2.1 | `carv-abi` crate: syscall numbers, error enum, message layout, rights bits | Host-compiles; documented in `docs/abi.md` — **done 2026-09-30** (#56) |
| P2.2 | `carv-caps` (pure): CSpace, capability slots, copy/mint/revoke, derivation tree | Proptest: after revoke(c), no descendant of c is reachable; rights never grow — **done 2026-09-29** (#57; derivation is per-CSpace, cross-CSpace revocation is #141, node reclamation #142) |
| P2.3 | `carv-budget` (pure): CPU budget/period refill, memory limit, hierarchical carve-out, token bucket | Host tests: child limits never exceed parent; refill math correct over simulated time — **done 2026-09-29** (#58; the §3.3 D energy estimate is not implemented yet) |
| P2.4 | Kernel objects: Thread, AddressSpace, Frame, Endpoint, Notification, Budget, Reply | Kernel tests create and destroy each object via `invoke`; memory charged/credited to the budget — **partial (#59, reopened 2026-10-01):** `kernel/src/objects.rs` has a registry where each object type is charged to and credited back to its budget, and objects can be destroyed via `invoke`. Remaining: objects created via `invoke`; the registry actually used by the kernel (objects are type tags with no state); child budgets carved out of their parent via `carv-budget` (today a child's limit can exceed its parent's) |
| P2.5 | Context switch, ring-3 entry (`sysretq`/`iretq`), `syscall`/`sysret` setup (STAR/LSTAR/SFMASK), SMAP/SMEP on | User-mode test thread calls `debug_putc`; user access to kernel page faults — **partial (#60):** ring-3 selectors, `UserContext`, STAR/LSTAR/SFMASK and CR4 SMEP/SMAP landed; `syscall_entry` only halts. Remaining: entry stub and kernel-stack switch, dispatcher, `sysretq`/`iretq` return, context switch, the ring-3 `debug_putc` test and the kernel-page fault test. Blocked by #140 |
| P2.6 | Scheduler: round-robin within budgets, preemption on timer, throttling on budget use-up | Test: two spinning threads with 30%/70% budgets measure within ±5% of that — **partial (#61):** `kernel/src/scheduler.rs` is a budget-aware round-robin *model* ticked by the LAPIC timer that never switches threads, and it reimplements the refill math instead of using `carv-budget`. Remaining: real preemptive switching and the 30/70 measurement |
| P2.7 | IPC: send/recv/call/reply_recv, badges, cap transfer in messages | Ping-pong test between two user threads; cap passed over IPC is usable by the receiver; round-trip latency printed — **partial (#62):** `kernel/src/ipc.rs` has non-blocking endpoint queues, badges and cap transfer, tested in-kernel. Remaining: blocking through the scheduler, the user-thread ping-pong and latency; the message layout must match `carv-abi` (#65); transfer bugs in #141 |
| P2.8 | IRQ → Notification routing, `Irq` caps | Timer or serial IRQ delivered to a user-space waiter — **partial (#63):** `Notification` and an `IrqRouter` (GSI → notification) landed, tested by calling `dispatch` directly. Remaining: real IDT/I/O-APIC delivery, `Irq` as a capability, and a user-space waiter |
| P2.9 | Minimal ELF loader for the root task (`init`) from a Limine module | `init` runs in ring 3 with its initial CSpace (all "untyped" authority) — **partial (#64):** `kernel/src/elf.rs` validates ELF64 images (rejects segments that are both writable and executable) behind a `SegmentMapper` trait; it has no tests and nothing calls it. Remaining: a Limine module request, an `init` binary, a user address-space mapper, and `init` actually running in ring 3 |
| P2.10 | **ABI freeze**: ADR-0002, `docs/abi.md` marked v1 | Maintainer approves; later changes need a new ADR — **partial (#65, reopened 2026-10-01):** ADR-0002 marks `docs/abi.md` v1 frozen, but that happened before P2.5–P2.9 exist, and the kernel doesn't follow it yet: `kernel/src/ipc.rs::Message` has 4 words and no label/info, while the frozen ABI specifies 6 words plus label and info; `Rights` is defined separately in both `carv-caps` and `carv-abi`. Signed off once the kernel's IPC/syscall path uses the `carv-abi` types; any layout correction needs a new ADR |

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
| P9.6 | Reproducible-build verification job (two runners, identical ISO hash), `cargo llvm-cov` on pure crates, Miri + `cargo geiger` in nightly, crash-consistency and 50-boot stress loops in nightly | Nightly green for 7 consecutive days; geiger count tracked in the summary — **reproducible-build part done 2026-09-27** (`trim-paths`, stripped ISO kernel, cross-directory second pass in `release.yml`; coverage/Miri/geiger/stress loops still open) |
| P9.7 | Supply-chain hardening: CodeQL and OpenSSF Scorecard workflows (repo public — **landed in Phase 0; Scorecard 7/10 on 2026-09-27, #35**), `cargo vet` audits for kernel and driver dependencies, `SECURITY.md` process exercised once with a mock report, OpenSSF Best Practices badge (needs maintainer registration) | Scorecard ≥ 7; `cargo vet` passes with no unaudited kernel deps |

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
| P0 Scaffolding & Boot | **done** | P0.1–P0.8 complete 2026-09-27; released as `v0.1.0`; `v0.1.1` (2026-09-27) adds P1.1–P1.3 and reproducible builds |
| P1 Kernel Core | **done** | P1.1–P1.7 done 2026-09-27 (GDT/IDT, `carv-frames` + frame allocator, paging over Limine's tables, kernel heap, LAPIC timer at 1 kHz, ACPI summary, PCI(e) enumeration over ECAM); **Phase 1 complete** |
| P2 Caps/IPC/Budgets | **in progress** | Epic #55. **Done:** P2.1 `carv-abi` (#56), P2.2 `carv-caps` (#57), P2.3 `carv-budget` (#58). **Partial (reopened 2026-10-01):** P2.4 kernel objects (#59) and P2.10 ABI freeze (#65), see their rows. **Partial:** P2.5–P2.9 (#60–#64), foundations only, with no code running in ring 3 yet. **Bugs:** #140 (STAR/GDT order gives the wrong user SS on `sysret`), #141 (IPC cap transfer skips GRANT and escapes revocation), #142 (`carv-caps` never reclaims nodes). The kernel depends on `carv-caps` but not yet on `carv-abi` or `carv-budget`. Merge order: P2.5 → P2.6 → P2.7/P2.8 → P2.9 → P2.4/P2.10 sign-off |
| P3 Runtime & svcmgr | not started | Epic #66 |
| P4 Drivers | not started | Epic #78 |
| P5 Versioned Store | not started | Epic #83 |
| P6 Shell & Coreutils | not started | Epic #90; no iteration assigned yet — only Iterations 1–5 exist |
| P7 Users & Auth | not started | Epic #104 (created 2026-09-29); no iteration assigned yet — only Iterations 1–5 exist |
| P8 Networking | not started | Epic #110 (created 2026-09-29); no iteration assigned yet — only Iterations 1–5 exist |
| P9 Hardening | partial | Epic #118 (created 2026-09-29); no iteration assigned yet — only Iterations 1–5 exist. Landed early: CodeQL + Scorecard workflows (P9.7, Phase 0), reproducible builds across machines (P9.6 part, #42, 2026-09-27). Rest not started |

(Better: mirror these as GitHub Issues with a Project board; one issue per task ID, labeled by phase.)

**Open maintainer decisions (2026-09-29, from #35 plus this review):**
- Branch-protection approval tiers (required approver / CODEOWNERS review / last-push approval).
  With one maintainer this blocks the autonomous loop; options: leave as is, add a second reviewer
  account and require one approval, or enable with admin bypass.
- OpenSSF Best Practices badge: registration at bestpractices.dev needs the maintainer's login; the
  questionnaire can then be answered from README/SECURITY/CONTRIBUTING.
- **Fine-grained agent PAT (P0.7):** §7.1 calls for agents to run under a fine-grained PAT scoped to
  `contents: write` + `pull_requests: write` with no admin rights; today agents use the maintainer's
  own `gh` login, which has admin rights. Creating and distributing that PAT is a maintainer action.
- **`v*` tag ruleset:** no ruleset protects tag creation/deletion today (only the `main` branch
  ruleset exists); §7.5's release flow and §12's risk mitigation assumed one. Agents never create or
  edit rulesets, so adding a tag ruleset is a maintainer action.
- **Iterations 6–9:** the board's Iteration field only has Iterations 1–5 configured (through
  2026-12-05). The Phase 6–9 epics (#90, #104, #110, #118), their sub-issues and #126 have no
  iteration until the maintainer adds Iterations 6–9.
- **Phase 2 sign-off (2026-10-01):** P2.4 (#59) and P2.10 (#65) were reopened because their AC are
  unmet (see their §8 rows). Approving the ABI freeze stays a maintainer decision; it happens once
  the kernel's IPC/syscall path uses the `carv-abi` types.

**Deferred to later tasks:** nextest + JUnit output (P9.6), `cargo vet` (P9.7), fuzzing once parsers
exist (P9.3), Miri/geiger/coverage/50-boot stress in nightly (P9.6), `cargo geiger` in CI (P9.6).

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
| Agent merges or tags without CI | `main` ruleset with no bypass; agents use `gh` under the maintainer's login (a scoped fine-grained PAT is still a pending maintainer action, §11) and are instructed never to tag or edit rulesets. **No `v*` tag ruleset exists yet** — tag protection is a pending maintainer action (§11); until then, tagging is a purely human/process control, not a mechanical one |
| Tampered or mistaken release download | Reproducible builds, `SHA256SUMS`, Sigstore signature, SLSA provenance; releases are never re-uploaded, only superseded |
| CI slow or flaky under TCG emulation | KVM enabled on GitHub Linux runners; TCG fallback scales timeouts; no retries, so flakes surface and get fixed |
| Review comments land after the merge (auto-merge races Copilot and the monitoring loop) | No auto-merge on fresh PRs; ≥ 15-minute review window after every push; every thread answered and resolved before merging; periodic audit that all merged PRs have zero unresolved threads (§7.1) |
| Two agent sessions share one checkout (branch switches, stashes, stray commits on `main`) | One `git worktree` per session; no bare `git stash`; watchers read via `gh` only; `main` is protected so a stray local commit cannot be pushed (§7.1, CLAUDE.md) |
| A PR weakens the gate it is judged by | `docs.yml` runs `docs-gate` from the *base* branch's xtask (bootstrap fallback only when the base lacks `--repo`); the `main` ruleset has no bypass |

---

## 13. Reference Material for Agents
- Philipp Oppermann, *Writing an OS in Rust* (blog_os): boot, paging, interrupts, allocators, testing.
- OSDev Wiki: APIC, ACPI, PCI, virtio, 16550 UART.
- seL4 manual and whitepaper: capability derivation, IPC, MCS scheduling contexts.
- Fuchsia Zircon docs: handles and rights (a capability model close to CARV's).
- Limine protocol spec (`limine` crate docs).
- virtio 1.2 specification (OASIS).
- smoltcp examples; Nushell documentation (structured pipelines); git internals (object model).

### Toolchain and tooling notes learned so far
- **Reproducible kernel ELF:** `-C remap-path-prefix` is *not* a codegen option; use cargo's
  `trim-paths = "all"` in the release profile (`cargo-features = ["trim-paths"]`, nightly). Only the
  `/rustc/<commit>` and `/cargo/registry/<index-hash>` prefixes remain, identical on every machine
  with the pinned toolchain. The ISO's kernel copy is stripped with the toolchain's `llvm-objcopy`
  (`llvm-tools-preview`), never a system binutils.
- **Reproducible ISO:** `SOURCE_DATE_EPOCH` alone is not enough for xorriso — pass
  `--modification-date=<stamp>00` and `--set_all_file_dates @<epoch>` *after* the `-as mkisofs`
  arguments, and overwrite the MBR disk id at offset `0x1B8` (`limine bios-install` seeds it from
  `time()`). Also pass `-V`, `-A` and `-p`: xorriso otherwise stamps its own version
  (`XORRISO-1.5.8 …, LIBISOBURN-…`) into the preparer field of the primary and Joliet volume
  descriptors, so builds differ by xorriso version even when every file inside matches (found by
  comparing Fedora's 1.5.8 with Ubuntu's 1.5.6 on `v0.1.1-rc.1`).
- **GitHub Actions:** `id-token` accepts only `write` (or absent), never `read`; job `permissions`
  blocks that run compilers stay `contents: read`. Plain-scalar colons in `if:`/`name:` break YAML,
  so quote them and validate with PyYAML. `dtolnay/rust-toolchain` needs an explicit toolchain input;
  `rustup toolchain install` reads `rust-toolchain.toml` directly. `cargo-deny-action` installs its
  own musl build of the pinned nightly from `rust-toolchain.toml`.
- **Kernel flags:** a global `RUSTFLAGS` (env or `build.rustflags`) silently replaces
  `target.x86_64-unknown-none.rustflags`; keep kernel flags only in `.cargo/config.toml`.
- **x86_64 0.15:** `Cr2::read()` returns `Result`; use `is_multiple_of` for alignment checks
  (clippy); `#[allow(unconditional_recursion)]` is needed on the stack-overflow test.
- **Kernel test layer in CI** needs the blank data disk too (`ensure_data_disk()` runs from
  `qemu_command`), and every QEMU scenario must carry a timeout.
