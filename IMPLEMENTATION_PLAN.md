# IMPLEMENTATION_PLAN.md — miau-core

Phased build plan. Read `LLM.md` first; this file assumes you know the architecture.

> **Status: all phases (0–7) are complete** — released as v0.1.0. This file is kept as the
> record of how the crate was built and what each phase's definition of done was. New work
> happens as scoped increments on feature branches, not as plan phases.

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

## Increments after v0.1.0

The phased build above is closed. Work now lands as scoped increments on feature branches;
each is recorded here so this file stays the record of what the crate is.

### `io/` MIDI output

Phase 4 only ever planned MIDI *in* — "hardware MIDI in → typed events at the io edge". That
left the core unable to *send* a note, so a MIDI-only shell could not reach a synth at all.
Two gaps, both filled symmetrically rather than with new machinery:

- `encode_midi` / `encode_ump` in `io/midi.rs`: the exact inverse of `parse_midi` /
  `parse_ump`, allocation-free (every modelled message is three bytes). `NoteOn` velocity is
  floored at 1 so it can never degrade into the velocity-0 note-off convention on the wire.
- `MidiSink` + a `run_midi_output` helper in `io/mod.rs` (the helper was superseded by
  `io::Client` in the next increment): the SPA sequence *writer*, mirror of the
  existing `for_each_control` reader, writing straight into the mapped PipeWire buffer.
  `MidiSink::frames()` comes from `Buffer::requested()` — a MIDI stream has no sample buffer
  to measure, and this is the block length the engine advances the transport by.
- `Graph::events_output`: the event mirror of `Graph::output`. Audio could always reach the
  io edge; events could not. This is the only way out of the graph for a MIDI-only shell.

**Done when:** encoder and parser round-trip across the whole field range; what `MidiSink`
writes, `for_each_control` reads back identically in both encodings; a full buffer truncates
instead of corrupting; and `examples/midi_out.rs` plays a looping arpeggio into a real synth
at the right tempo.
*Verified:* 37 note-ons in 20 s at 112 BPM (expected 37.3) on PipeWire 1.6.8, quantum 1024.

A note for shells: a MIDI-only client does not drive the PipeWire graph. With nothing else
driving, the server falls back to its dummy driver — timing measured accurate, but the block
rate is only as steady as that driver.

### Complete MIDI 1.0 channel voice

The `Event` enum stopped at note on/off, control change and pitch bend; poly pressure, program
change and channel pressure were dropped at the edge. For a *generic* core that is a hole — a
shell could not select a patch or pass aftertouch through. LLM.md always called `Event` a
growing enum, so this grows it rather than changing any contract:

- `Event::PolyPressure`, `Event::ProgramChange`, `Event::ChannelPressure`, with parse and
  encode on both sides. Purely additive, so existing documents deserialize unchanged.
- Program change and channel pressure are **two-byte** messages, so `encode_midi` now returns a
  `MidiMessage` (inline bytes + length) instead of a fixed `[u8; 3]`. A trailing zero byte would
  be read downstream as the start of a new message — the length is part of the format.
- `clap/node.rs::translate_in` now builds its raw-MIDI events through `io::encode_midi` instead
  of hand-rolling status bytes, so the CLAP path and the wire path cannot drift apart.

System messages (clock, transport, SysEx) stay out: they are synchronisation, not music, and
belong to a transport-sync layer the core does not have. That is the next real gap for a
generic core, and it touches `transport/`, not just `io/`.

**Done when:** every channel-voice event round-trips encode→parse across the full field range
in both encodings; the two-byte messages assert their exact wire length.

### `io/` one client, many ports

`run_audio_output`, `run_midi_input` and `run_midi_output` each owned a main loop, blocked until
Ctrl-C and exposed exactly one port. Enough for an example, not for an application: a GUI cannot
block, a MIDI tool wants a port per lane, and an engine wants input and output *in the same
block*. All three are **removed**; `io::Client` replaces them (invariant 10 — with both, one of
them would be wrong).

- `ClientBuilder` → `Client`: one `pw_filter` node with any mix of `midi_in`/`midi_out`/
  `audio_in`/`audio_out` ports, one process callback, its own thread, `stop()` on demand (also
  on drop).
- `Block` is what the callback gets: `frames()`, `midi_in(port, f)`, `midi_out(port, f)`,
  `audio_in/out(port)`. `midi_out` takes a closure so the sequence header is always patched and
  the size always reported — a half-written buffer is not something a caller can leave behind.
- `Graph::node_mut`: the mirror of `Graph::node`, so a driver that owns the graph can hand a
  node this block's live input directly. With one client the io edge and the graph share a
  thread, so routing events through a queue *to itself* is pure overhead. From another thread
  the queue is still the only way in (invariant 2).

`pipewire-rs` wraps `pw_stream` but not `pw_filter`, so the filter calls go through
`pipewire::sys` (already a transitive dependency — nothing new was added). The `unsafe` is
confined to `io/client.rs` and its invariants are documented at the top of that file. There is
**no `unsafe impl Send`**: every PipeWire object is created, used and destroyed on the client's
own thread, and `Client` holds only a channel sender and a join handle.

**Done when:** one node exposes several ports that carry independent material, input and output
are read and written in the same block, and start/stop is repeatable.

*Verified* on PipeWire 1.6.8:
- 4 MIDI outputs + 1 input on a single node; `midi_out_0` carried only its own voice (key 60)
  and `midi_out_2` only key 67, proving per-port independence.
- Input counted while the four outputs were writing — full duplex in one callback.
- Timing: 38 note-ons in 20 s at 112 BPM (expected 37.3).
- Audio: a captured `beep` window contained **only** 440 Hz (every other probed frequency
  measured 0.00000).
- Lifecycle: three start/stop rounds, 33 blocks per 700 ms (quantum 1024 @ 48 kHz), stop in
  ~0.4 ms, no hang.
- `clap_synth` was A/B'd against the pre-change build in a scratch worktree; both produce the
  same output character, so the port is behaviour-preserving.

Two findings worth keeping, both now handled:
- **An unlinked node is not scheduled at all.** Without `node.always-process` the process
  callback never ran, so a shell's transport would sit frozen until the user patched something.
- **Buffers exist only on linked ports.** `always-process` gets you the callback, not a buffer:
  measured 0 audio buffers while unlinked, then one per block the instant a link appeared. That
  is why `audio_out`/`midi_out` return `Option`/`bool` rather than pretending.

Also dropped: an `autoconnect` builder option. WirePlumber does not link a `pw_filter` node from
`node.autoconnect` + `media.class` alone, and shipping a switch that silently does nothing is
worse than not having it — routing stays external, which is what LLM.md says anyway.

### `transport/` MIDI clock — the core as clock master

The last thing keeping the core from being generally usable: it could not run alongside other
gear. System messages had no home, so a shell could neither send clock nor see incoming
start/stop.

- `transport::SyncMessage` (`Clock`, `Start`, `Continue`, `Stop`, `SongPosition`) is a **separate
  type from `Event`**, not another variant. An event is what a note port carries and a synth
  plays; these steer transports. Keeping them apart is what makes it impossible for a clock pulse
  to surface as a note — asserted directly by `music_and_timing_never_cross_over`.
- `io::parse_sync` / `encode_sync` (plus their UMP forms), `MidiControl::sync()` and
  `MidiSink::push_sync()`: the same edge treatment as channel voice, so raw bytes still stop at
  the edge (invariant 6).
- `transport::ClockMaster` turns the transport's own motion into sync messages: 24 pulses per
  quarter note, `Start` from the top, `SongPosition` + `Continue` when resuming elsewhere, `Stop`
  on halt. It *reads* the transport rather than living inside it, so nothing about clock leaks
  into scheduling and a shell that wants no clock never builds one.

**Done when:** the pulse rate is exactly 24 per quarter note; slicing a span at arbitrary points
— including exactly on a pulse — yields the identical pulse list as slicing it in one go (the
seam is where a clock master would otherwise stutter); and music and timing never parse into each
other's types.

*Verified* on PipeWire 1.6.8: 888 clock pulses received in 20 s at 112 BPM (expected 896), and a
scripted play → stop → locate → play produced exactly `Start`, `Stop`, `SongPosition(32)`,
`Continue`, `Stop` on the wire — 32 being bar 3 in sixteenths, as it should be.

Also closed here: `audio_in` on the client had been written but never measured. Fed from `beep`,
it reported peak **0.200** — exactly that example's `Gain::new(0.2)` on a unit sine — and, like
every other port, 0 buffers until linked.

### Still open: following an external clock

The core can now *drive* other gear but not *follow* it. That is not a missing function, it is a
collision with invariant 9: "the tempo map is immutable during playback". Slaving to an incoming
clock means continuously adjusting tempo, which the current design deliberately forbids — a
follower needs a PLL and a tempo representation that can bend without a snapshot swap per block.

Flagged rather than built (see `AGENTS.md`, "when the plan and reality diverge"). Incoming sync
messages *are* already delivered to a shell via `MidiControl::sync()`, so a shell can display or
act on them today; what it cannot do is hand the transport over to them.

## Standing rules across all phases

- Docs never trail the code: if reality diverges from `LLM.md` or this plan, update the
  file in the same change (see `AGENTS.md`).
- Every phase adds to `examples/` where it can; runnable examples are the cheapest
  documentation and the best agent context.
- Deferred items live in the design log (Nokta note 06), not as TODOs in code.
