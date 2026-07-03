# IMPLEMENTATION_PLAN.md — miau-core

Phased build plan. Read `LLM.md` first; this file assumes you know the architecture.

**Ordering rationale:** pure logic → concurrency → hardware → plugins → persistence.
Each phase produces something testable without the phases after it; the first audible
result lands as early as the hardware allows (phase 4). Persistence comes late because it
needs every data structure's final shape, and blobs need CLAP.

Phases are sized so one focused agent session can complete one phase (phase 3 and 5 may
take two). Finish a phase — including its definition of done — before starting the next.

---

## Phase 0 — Scaffold

Repo skeleton: `cargo init --lib`, module tree from `LLM.md` as empty `mod`s with one-line
doc comments, `rustfmt.toml`, clippy config, `.gitignore`, MIT/Apache-2.0 dual license,
GitHub Actions CI (build + test + fmt + clippy `-D warnings`), `README.md` stub pointing at
`LLM.md`. Commit `LLM.md`, `AGENTS.md`, this file.

**Done when:** CI is green on an empty-but-structured crate.

## Phase 1 — `time/` + `event/` (pure logic, no threads)

- `Tick(u64)` (960 PPQ), `SamplePos(u64)`, `SampleOffset(u32)` newtypes with the arithmetic
  they need and nothing more.
- Stepwise `TempoMap` with precomputed cumulative sample positions; `tick_to_sample`,
  `sample_to_tick`, iteration over tempo segments within a block.
- `Event` enum: `NoteOn`, `NoteOff`, `Control`, `PitchBend`, `ParamChange` (more variants
  later; MPE/MIDI 2.0 deliberately deferred).
- `EventSource` trait (tick-range in, events out; realtime-safe contract documented on the
  trait itself).
- `NoteTracker`: records sounding notes, produces flush events for stop/locate/loop-wrap.

**Done when:** unit tests cover tempo changes mid-block, tick↔sample round trips at segment
boundaries, and note-tracker flush on all three interruption kinds.

## Phase 2 — `transport/` + minimal `arrange/`

- Transport commands (`Play`, `Stop`, `Locate`, `SetLoop`, …) in via `rtrb`; playhead and
  run-state out via atomics (single writer: the audio-side tick function).
- `Arc` snapshot swap for the tempo map (the pattern all later snapshots reuse).
- Block scheduling: given block size + sample rate, produce the block's tick range,
  including loop-wrap splits.
- Minimal `arrange/`: `Lane`, `Clip` (events at tick positions) and a `ClipPlayer`
  implementing `EventSource` — needed now so the transport has something real to test.

**Done when:** a fake-clock test drives the transport block by block through play, locate,
and a loop wrap, and the emitted events (including note-off flushes, invariant 8) match
expectations exactly.

## Phase 3 — `graph/`

- `Node` trait, `ProcessContext`, `NodeIO`; `AudioPort`/`EventPort` with typed connection
  rules (invariant 13).
- Node arena (engine-owned slab, stable runtime handles); topology as immutable snapshot;
  edits validated by topological sort (cycles rejected), baked into a flat schedule.
- Preallocated buffer pool with reuse; schedule compilation assigns buffers.
- Garbage chute: retired nodes/snapshots leave via a queue to a cleanup thread
  (invariant 11).
- EventSource wrapper node; a handful of utility nodes: `Sine` (test tone), `Gain`,
  `Capture` (writes its input to a buffer for tests).

**Done when:** an offline render test builds sine → gain → capture, processes N blocks,
and asserts byte-exact output; a topology edit mid-render swaps snapshots without a glitch
in the captured audio; cycle insertion is rejected.

## Phase 4 — `io/` 🔊 first sound

- PipeWire client (`pipewire-rs`): one client with ports; the engine is **pulled** from
  PipeWire's process callback — the core has no audio thread of its own.
- Hardware MIDI in → typed events at the io edge (invariant 6).
- Hardware routing stays external (qpwgraph); the core exposes ports, nothing more.

**Done when:** `examples/beep.rs` plays the sine node through the graph out of the
speakers, and `examples/midi_monitor.rs` prints typed events from a hardware keyboard.
*Milestone: the engine is audible.*

## Phase 5 — `clap/`

- Discovery: scan `~/.clap` and `/usr/lib/clap`; instantiation + activation on the main
  thread (never on the audio thread); a plugin enters the graph snapshot only once activated.
- `ClapNode` with two faces (RT part in the arena, main-thread part for GUI/state), joined
  per instance over the existing command channels.
- Event translation `(SampleOffset, Event)` ↔ CLAP events; parameters as `ParamChange`,
  sample-accurate within the block; GUI knob turns reported back out.
- State extension: opaque blob get/set. Placeholder node for missing plugins (blob and
  connections preserved).
- Deactivation exits via the garbage chute.

**Done when:** `examples/clap_synth.rs` plays a hardware MIDI keyboard live through a real
CLAP synth (e.g. Surge XT), an automation-style `ParamChange` stream audibly moves a
parameter, and a state blob survives a get→destroy→recreate→set cycle.
*Milestone: it's an instrument host.*

## Phase 6 — `persist/` + full `arrange/`

- `Document` types (serde): tempo map, lanes & clips, graph topology (registry type-IDs +
  settings, connections by `NodeId` + port index), blob references.
- Stable `NodeId(u64)` from a document-owned monotonic counter (invariant 15).
- ZIP container read/write: `core.json`, `blobs/`, `samples/` (relative path + BLAKE3 hash,
  optional embedding), `shell/<id>.json` preserved opaquely (invariant 14).
- `.miauclip` export/import with ID re-mapping.

**Done when:** save → restart → load reproduces an offline render byte-exactly; a load/save
round trip preserves an injected unknown field and a foreign `shell/` section; loading with
a plugin removed yields a placeholder that saves back losslessly; a `.miauclip` moves a clip
between two documents.
*Milestone: projects are durable.*

## Phase 7 — `dsp/` (minimal)

- `TimeStretcher` trait + registry (string IDs, graceful fallback per invariant 7).
- `RepitchStretcher` only. `BeatsStretcher`/`ComplexStretcher`, resampler trait, and disk
  streaming (creek) stay deferred (see design log note 06).

**Done when:** registry registration/fallback is tested; a clip referencing an unknown
stretcher ID loads with a warning and plays via repitch.

---

## After phase 7 → release prep (separate docs pass)

Rustdoc pass over the public API, `examples/` polish (examples double as agent context),
`LLM.md`/`AGENTS.md` reconciliation against the real code, crates.io metadata, publish.
This is deliberately its own step with fresh eyes — not part of the build phases.

## Standing rules across all phases

- Docs never trail the code: if reality diverges from `LLM.md` or this plan, update the
  file in the same change (see `AGENTS.md`).
- Every phase adds to `examples/` where it can; runnable examples are the cheapest
  documentation and the best agent context.
- Deferred items live in the design log (Nokta note 06), not as TODOs in code.
