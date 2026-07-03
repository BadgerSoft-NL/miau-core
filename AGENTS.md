# AGENTS.md — miau-core

Rules of engagement for coding agents (and humans behaving like them).

## Before anything else

1. Read `LLM.md` in full. It is the architectural contract; the 15 invariants are law.
2. The phased build in `IMPLEMENTATION_PLAN.md` is complete (v0.1.0); it remains the record
   of how the crate was built. Work now happens as scoped increments on feature branches —
   stay inside the increment you were asked for.

## Workflow

- Every change compiles with `cargo build`, passes `cargo test`, `cargo fmt --check` and
  `cargo clippy --all-targets -- -D warnings` before it is considered done.
- New behavior comes with tests. `time/`, `event/`, `arrange/`, `persist/` and the graph
  compiler are pure logic — they get unit tests, no excuses. Realtime paths get offline
  render tests (drive `process()` manually with a fake clock; assert on captured buffers).
- No new dependencies without stating why in the commit/PR description. The dependency list
  is part of the architecture.
- Public API gets doc comments written for a reader who has `LLM.md` in context — don't
  repeat the invariants, reference them (`// invariant 11: no dealloc here, use the chute`).

## Realtime-safety review checklist

Any code that can run on the audio thread (`graph/`, `io/`, node `process()` implementations,
`EventSource::events_in`) must be checked against:

- [ ] No allocation or deallocation (no `Box::new`, `Vec::push` beyond capacity, `String`,
      `format!`, no `drop` of heap-owning types)
- [ ] No locks (`Mutex`, `RwLock`, channels that can block), no syscalls, no I/O, no logging
- [ ] Bounded work per block (no unbounded loops over user data)
- [ ] Communication only via the sanctioned patterns: lock-free queue in, atomics out,
      `Arc` snapshot read

If a task seems to require breaking one of these, stop and flag it — do not work around it.

## When the plan and reality diverge

Implementation will surface things the design didn't foresee. When that happens:

1. Say so explicitly — name the invariant or plan section under pressure.
2. Propose the smallest change that resolves it.
3. Update `LLM.md` / `IMPLEMENTATION_PLAN.md` in the same change, so the docs never trail
   the code. The design log (why) lives in the owner's Nokta notebook; the repo carries the
   current truth (what/how).

## Style

- Newtypes over bare integers, always (`Tick`, `SamplePos`, `NodeId`, port indices).
- One way to do things; if two APIs can express the same operation, one of them is wrong.
- Prefer the boring solution. This project rejects speculative abstraction (no backend
  traits, no plugin-format abstraction, no crate splits).
