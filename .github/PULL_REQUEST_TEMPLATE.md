## Task
<!-- Task ID and title from docs/PLAN.md, e.g. "P1.2: Physical frame allocator". Infra/docs PRs: say so. -->

**Acceptance criteria** (copied from the plan) and whether each is met:
- [ ] …

## What changed
<!-- Design choices, anything left undone, anything a reviewer should look at first. -->

## Documentation
<!-- The docs gate requires these; tick what applies. -->
- [ ] `CHANGELOG.md` line under *Unreleased* (release PR: the new `## [x.y.z]` heading instead)
- [ ] README / CLAUDE.md / PLAN.md updated if commands, workflow, or design changed
- [ ] `docs/PLAN.md` §11 status table updated
- [ ] ADR added if an architectural decision or `docs/abi.md` changed

## Verification
<!-- Paste real output. -->
```
$ cargo xtask test
$ cargo xtask perf
$ cargo clippy --workspace --exclude chisel --all-targets -- -D warnings
$ cargo clippy -p chisel --target x86_64-unknown-none -- -D warnings
$ cargo fmt --all --check
```

## Safety
- [ ] Every new `unsafe` block has a `// SAFETY:` comment
- [ ] No new path bypasses capability checks; every allocation path charges a Budget (from P2 on)
