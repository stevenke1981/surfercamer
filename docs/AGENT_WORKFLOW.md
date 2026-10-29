# Agent Team Workflow

How this repository was built (and how to extend it) with a team of AI agents.

## Roles

| Role | Model | Responsibility |
|------|-------|----------------|
| Lead / Architect ("brain") | Claude Opus 5.5 | Plan, spec, `ss-core` contracts, compiled stubs, task breakdown, integration, final verification, commit/push |
| Implementer ("hands") | Claude Haiku 5.5 | One crate per agent: replace every `todo!()` per SPEC, write unit tests, keep `cargo test -p <crate>` and clippy green |
| Reviewer | Claude Sonnet 5.5 | Read-only review of the full workspace against SPEC: correctness, concurrency, security, spec drift; ranked findings |

## Pipeline

```
Lead: PLAN.md + SPEC.md + ss-core + stubs (workspace compiles)
  │
  ├─▶ Phase 1 (parallel, Haiku): ss-db │ ss-camera │ ss-motion │ ss-recorder
  │       gate: each `cargo test -p <crate>` green
  ├─▶ Phase 2 (parallel, Haiku): ss-engine │ ss-server (+ web UI)
  │       gate: `cargo test --workspace` green
  ├─▶ Phase 3 (Sonnet): review → findings list
  ├─▶ Phase 4 (Haiku): fix findings   → Lead re-verifies
  └─▶ Lead: build/test/clippy, smoke-run `ssd`, commit, push
```

## Rules for implementers

1. Own exactly one crate; never edit `ss-core` or another crate. Contract
   problems are reported to the Lead, who changes `ss-core`.
2. Public signatures in stubs are fixed. Private items/modules are free.
3. Only `cargo … -p <your-crate>` while others work in parallel.
4. No network in tests (use local mock servers / temp dirs).
5. No `unwrap()` on I/O or external input in non-test code; map to `ss_core::Error`.
6. Done = no `todo!()` left, tests pass, `cargo clippy -p <crate> --all-targets` clean.

## Why stubs first
Parallel agents only stay consistent if the seams are fixed and *compiling*
before fan-out. The Lead therefore writes every cross-crate type and signature
up front; implementers fill bodies.
