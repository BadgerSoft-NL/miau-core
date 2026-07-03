# LLM.md — miau-core

**Read this file first, in full, before writing or reviewing any code in this repository.**
It is the architectural contract. Everything here was decided deliberately; if the code and this
file disagree, that is a bug in one of them — say so, don't silently pick a side.

## What this is

**miau-core** (MIDI + AUdio) is a DAW-grade audio/MIDI engine in Rust. It is the shared
foundation of a family of independent music applications ("shells"): a generative/phase-music
MIDI tool (Sparkle), an Ableton-Live-inspired DAW, and an MPC-style sampler. The core covers
the complete DAW picture — time, events, arrangement, routing, plugin hosting, audio I/O,
persistence — and each shell takes the subset it needs.

The core knows nothing about UI technology. No Tauri, no webview, no widget toolkit.
**Core = engine, transport, events, routing, I/O, persistence. Shell = everything a human
sees and touches.**

## Platform stance (deliberate, not an oversight)

- **Linux only. PipeWire is the only audio backend** (via `pipewire-rs`). On modern distros
  PipeWire *is* the JACK server, so desktop and pro-audio users work with one backend.
  No ALSA-direct, no PulseAudio, no legacy JACK, no backend abstraction trait.
- **CLAP is the first-class plugin format** (hosted via `clack`). No VST/LV2 hosting in core.
- Plugins run **in-process**. A crashing plugin takes the app down; accepted. Sandboxing is
  deliberately off the table — not deferred, rejected.

Do not add portability layers, backend traits, or plugin-format abstractions "to be safe."
That is exactly the ceremony this project rejects.

## Module map

```
miau-core/src/
  time/     — Tick, SamplePos, tempo map, musical↔sample conversion
  event/    — typed Event enum, EventSource trait, note tracker
  arrange/  — lanes, clips, ClipPlayer (EventSource); "tracks" do not exist here
  transport/— play/stop/locate/loop, block scheduling, tempo-map snapshot swap
  graph/    — Node trait, ports, topology snapshots, schedule, buffer pool
  dsp/      — TimeStretcher trait + default implementations (registry pattern)
  persist/  — Document types, ZIP container, .miauclip exchange format
  io/       — PipeWire client, hardware MIDI
  clap/     — CLAP hosting (ClapNode, discovery, state blobs)
```

Dependency direction is law (invariant 1): `time/` imports from nobody, `event/` and
`arrange/` only from `time/`, `transport/` from `time/`/`event/`/`arrange/`; `io/` and `clap/`
may know everything. Rust doesn't enforce this — you must.

## The invariants

An agent (or human) must never violate these. When a change seems to require breaking one,
stop and flag it instead of working around it.

1. **Module dependencies flow one way.** `time/` and `event/` import nothing from `graph/`,
   `io/`, `clap/`. `io/` and `clap/` may know everything.
2. **The audio thread never shares locks with other threads.** Communication is: commands in
   via lock-free queues, state out via atomics, data in as immutable `Arc` snapshots swapped
   atomically.
3. **Single writer per atomic.** The playhead is written only by the audio thread; the UI
   changes position via a `Locate` command, never by writing the atomic.
4. **Musical data lives in ticks (`Tick(u64)`, 960 PPQ); sample positions appear only at
   scheduling.** Newtypes everywhere — never bare integers for time.
5. **EventSources are realtime-safe.** No allocations, no locks, no unbounded work inside
   `events_in`.
6. **Raw MIDI bytes exist only at the io edge.** Inside the core, events are the typed enum.
7. **The core defines contracts and ships defaults; shells register alternatives under string
   IDs.** Unknown IDs degrade gracefully (fallback + warning), never crash.
8. **Note-off hygiene is central.** The engine's note tracker flushes hanging notes on
   stop/locate/loop-wrap; sources must not attempt this themselves.
9. **The tempo map is immutable during playback.** Edits produce a new map swapped in as a
   snapshot.
10. **One way to do things.** Predictable naming, no hidden ordering dependencies, types that
    make misuse impossible.
11. **Zero allocations and zero deallocations on the audio thread during playback.** Buffers
    come from the preallocated pool; removed nodes leave via the garbage chute (queue to a
    cleanup thread), never by dropping in `process`.
12. **The graph is acyclic.** Topology edits are validated by the topological sort; the audio
    thread only iterates the precompiled schedule list — never traverses the graph.
13. **Port types are the only routing law.** Audio ports connect to audio ports, event ports
    to event ports; everything else (tracks, chains, mixer structure) is shell convention.
    Nodes ignore event variants they don't understand.
14. **Persistence never discards what it doesn't understand.** Unknown fields, foreign shell
    sections and missing-plugin blobs survive a load/save round trip intact.
15. **Runtime indices are never persisted.** Documents reference stable `NodeId`s; the runtime
    arena is rebuilt on load.

## Threading model

Three worlds, with strict traffic rules between them:

- **Audio thread** (owned by PipeWire's callback — the core has no audio thread of its own):
  iterates the precompiled schedule, calls `Node::process`, writes the playhead atomic.
  Never locks, never allocates, never deallocates, never does I/O.
- **Main thread**: topology edits, CLAP GUI/state, document load/save, snapshot construction.
- **Cleanup thread**: receives retired nodes/snapshots through the garbage chute and drops them.

Traffic between worlds: lock-free command queues (`rtrb`) inward, atomics outward, `Arc`
snapshots for shared data (tempo map, graph topology). Snapshots are immutable; an edit builds
a new one and swaps it atomically.

## Key contracts

- **`Tick(u64)`** at 960 PPQ. The tempo map is stepwise with precomputed cumulative sample
  positions; conversion to `SamplePos` happens only at scheduling time.
- **`Event`** is a typed enum (`NoteOn`, `NoteOff`, `Control`, `PitchBend`, `ParamChange`, …).
  Automation is not a separate system: a `ParamChange` travels through an ordinary event port.
- **`EventSource`** is the musical contract: given a tick range, emit events. Clip playback and
  generative modules are *the same thing* to the engine — this is the core's key departure from
  traditional sequencer architecture. EventSources know ticks, not the graph.
- **`Node`** is the routing contract: `fn process(&mut self, ctx: &ProcessContext,
  io: &mut NodeIO)`. Everything is a node — CLAP plugins, gain, the PipeWire sink, hardware
  MIDI in. The core wraps an EventSource in a node that fetches the block's tick range from
  the transport and emits `(SampleOffset, Event)`.
- **Ports**: exactly two types, `AudioPort` (f32 buffer per block) and `EventPort`
  (timestamped messages). Don't confuse the three layers: port type (2 kinds, routing law),
  event variant (growing enum, node behavior), track nature (emergent shell label).
- **Registry pattern**: `StretcherRegistry::register("paulstretch", factory)` (and the parallel
  `NodeRegistry` for graph nodes). Compile-time extension, not runtime plugins. Clips reference
  implementations by string ID; unknown IDs fall back with a warning (`dsp/` falls back to
  `repitch`, `persist/` to a placeholder node).
- **CLAP**: threading model adopted wholesale. `ClapNode` has two faces — a realtime part in
  the node arena (the `Send` audio processor) and a main-thread part (GUI, state, parameter
  enumeration) — joined per
  instance by a shared plugin handle. Instantiation/activation happen on the main thread; the
  realtime face is retired through the garbage chute, where the plugin is deactivated off the
  audio thread (invariant 11). Parameters are just `ParamChange` events, translated sample-
  accurately. Plugin state is an opaque blob; a missing plugin becomes a **placeholder node**
  that preserves the blob and all connections.

## Persistence

The core persists **documents**, not projects. A `Document` (serde) holds: tempo map, lanes &
clips, graph topology (nodes as registry type-ID + settings, connections by `NodeId` + port
index), plugin blobs. Track names, mixer structure, UI layout are shell data.

Container `project.miau` is a plain ZIP:

```
core.json           — the Document (serde_json), { "format": "miau-core", "version": 1 }
blobs/<node>.state  — CLAP state blobs, raw binary
samples/…           — embedded audio (optional; referenced by relative path + BLAKE3 hash)
shell/<id>.json     — one entry per shell, opaque to the core, preserved on round trip
```

`.miauclip` is the clip-level cross-shell exchange format: same machinery, one clip
(`clip.json`: events in ticks, length, loop, stretcher ID + settings) plus optional sample.
Import re-maps IDs.

Not persisted: playhead, transport runtime state, buffer pools, compiled schedules — all
derived. A document describes *what the music is*, not what the engine is doing.

## What the core is not

- There is no `Track` type, no `enum TrackType`, no mixer. A "track" is a shell-level coupling
  of a lane (content) and a node chain (routing) plus a name. A "MIDI track" is just a chain
  ending in an event port.
- There is no UI, no settings storage, no file browser, no undo stack (shells own undo over
  the command stream).
- There is no parameter system separate from the event stream.

When you're tempted to add one of these "because every DAW has it" — that's the shell's job.
The core has no opinion where none is needed.
