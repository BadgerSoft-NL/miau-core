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

**v0.1.0** — the full planned core is implemented: musical time & tempo maps, typed events,
transport, arrangement (lanes/clips), the realtime audio graph, PipeWire I/O, CLAP hosting,
document persistence (ZIP container + `.miauclip` exchange format) and offline time-stretching.
MIDI 1.0 channel voice is covered completely in both directions, one `io::Client` carries any
mix of MIDI and audio ports on a single node with a start/stop lifecycle, `io::Patchbay` reads
the surrounding PipeWire graph and makes links on request in both directions, and the transport
is a MIDI clock master. 92 tests; realtime paths are lock- and allocation-free per the
invariants.

## Examples

Six examples double as a guided tour (all need a running PipeWire session; all but the last
expose ports you route yourself — the core decides no routing of its own):

- `cargo run --example beep` — a sine through the graph; route "miau beep" to your speakers.
- `cargo run --example midi_monitor` — typed events from a hardware MIDI keyboard; route your
  keyboard to "miau midi monitor" (e.g. in qpwgraph).
- `cargo run --example midi_out` — a looping arpeggio out of a MIDI port, driven by the
  transport, with MIDI clock on the same port. The whole MIDI-only path in one screen.
- `cargo run --example midi_ports` — the same arpeggio with **a port per voice**, plus a MIDI
  input read in the same block: what one client with many ports buys you.
- `cargo run --example clap_synth` — play a CLAP synth (default: Surge XT) live via MIDI, with
  a parameter-automation LFO; needs a CLAP plugin in `~/.clap` or `/usr/lib/clap`.
- `cargo run --example midi_link` — list the machine's MIDI destinations *and* sources and link
  them from inside the process instead of reaching for a patchbay. Pass a destination key to
  send to it, a source key as well to be fed from it.

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your
option.
