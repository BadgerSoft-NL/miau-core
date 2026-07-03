# miau-core

A DAW-grade audio/MIDI engine in Rust. **miau** = **MI**DI + **AU**dio.

miau-core is the shared engine ("core") beneath a family of independent music applications
("shells"): a generative/phase-music MIDI tool, an Ableton-Live-inspired DAW, and an MPC-style
sampler. The core covers the complete DAW picture — time, events, arrangement, routing, plugin
hosting, audio I/O, persistence — and each shell takes the subset it needs. **The core knows
nothing about UI.**

## Platform stance

- **Linux only**, **PipeWire** as the only audio backend (via `pipewire-rs`).
- **CLAP** as the first-class plugin format (via `clack`). No VST/LV2 in core.
- Plugins run **in-process**; no sandboxing.

These are deliberate design decisions, not oversights. See below.

## Read this first

The architecture is a contract, not a suggestion:

- [`LLM.md`](LLM.md) — the architectural contract and its 15 invariants. **Read it in full
  before writing or reviewing any code.**
- [`IMPLEMENTATION_PLAN.md`](IMPLEMENTATION_PLAN.md) — the phased build plan.
- [`AGENTS.md`](AGENTS.md) — rules of engagement for coding agents (and humans behaving like
  them).

## Status

Early scaffold (phase 0). The module tree exists; the phases fill it in.

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your
option.
