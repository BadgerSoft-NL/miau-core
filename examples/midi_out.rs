//! Plays a looping arpeggio out of a PipeWire MIDI port — the smallest audible use of the engine
//! that never touches audio. Run with `cargo run --example midi_out`; press Ctrl-C to stop.
//!
//! The client exposes one MIDI output port. Route "miau midi out" to a synth externally (e.g. in
//! qpwgraph, or `pw-link` by port id) — the core does no routing itself; until it is routed,
//! nothing sounds. Watch it with `cargo run --example midi_monitor` in a second terminal if you
//! have no synth at hand.
//!
//! This is the whole MIDI-only path in one screen, and it is the path a MIDI shell walks:
//! PipeWire pulls a block → the transport schedules it into slices → the `ClipPlayer`
//! (an `EventSource`) emits the slice's ticks → the sink converts each tick to a sample offset
//! and writes it to the wire. The transport loops the clip, so every wrap flushes the
//! `NoteTracker` and no note is left hanging (invariant 8).
//!
//! It is also the clock master: `Start`, `Stop` and 24 pulses per quarter note ride the same port
//! as the notes, so another machine can follow along. That is why the block is staged and sorted
//! before it is written — a SPA sequence is ordered, and notes and clock are two ascending streams
//! that have to be merged into one.
//!
//! See `examples/midi_ports.rs` for the same idea with a port per voice.

use std::sync::Arc;

use miau_core::arrange::{Clip, ClipPlayer, Lane, Placement};
use miau_core::event::{Event, EventSink, EventSource, NoteTracker};
use miau_core::io::{ClientBuilder, Error};
use miau_core::time::{TempoMap, Tick, TickRange};
use miau_core::transport::{BlockStep, ClockMaster, SyncMessage, Transport};

const RATE: u32 = 48_000;
const BPM: f64 = 112.0;
/// One 4/4 bar at 960 PPQ — the loop length.
const BAR: u64 = 4 * Tick::PPQ;
/// Notes are quarter notes, released just short of the next one so the arpeggio articulates.
const GATE: u64 = Tick::PPQ - 60;

/// One thing to put on the wire, already placed at its sample offset in the block.
#[derive(Clone, Copy)]
enum Outgoing {
    Note(Event),
    Sync(SyncMessage),
}

/// Bridges the musical domain to the wire: takes events in ticks, stages them at their sample
/// offset so they can be merged with the clock before anything is written.
///
/// `at` / `start` are the origin of the slice being emitted — the transport hands those out with
/// every [`BlockStep::Play`], and an event's offset is its distance from `start` in samples added
/// to `at`. `tracker` is `Some` while playing (so the engine knows what is sounding) and `None`
/// while flushing, because [`NoteTracker::flush`] already owns the tracker at that point.
struct BlockSink<'a> {
    staged: &'a mut Vec<(u32, Outgoing)>,
    tracker: Option<&'a mut NoteTracker>,
    tempo: &'a TempoMap,
    frames: u32,
    at: u32,
    start: Tick,
}

impl BlockSink<'_> {
    /// The sample offset within the block for a tick in the current slice.
    fn offset_of(&self, tick: Tick) -> u32 {
        self.tempo
            .offset_in_block(self.start, self.at, self.frames, tick)
    }
}

impl EventSink for BlockSink<'_> {
    fn push(&mut self, at: Tick, event: Event) {
        if let Some(tracker) = self.tracker.as_deref_mut() {
            tracker.observe(&event);
        }
        let offset = self.offset_of(at);
        self.staged.push((offset, Outgoing::Note(event)));
    }
}

/// A one-bar arpeggio: four quarter notes, each with its own release.
fn arpeggio() -> Clip {
    let mut events = Vec::new();
    for (beat, key) in [60_u8, 64, 67, 71].into_iter().enumerate() {
        let on = beat as u64 * Tick::PPQ;
        events.push((
            Tick::new(on),
            Event::NoteOn {
                channel: 0,
                key,
                velocity: 96,
            },
        ));
        events.push((
            Tick::new(on + GATE),
            Event::NoteOff {
                channel: 0,
                key,
                velocity: 0,
            },
        ));
    }
    Clip::new(Tick::new(BAR), events)
}

fn main() -> Result<(), Error> {
    let tempo = Arc::new(TempoMap::constant(RATE, BPM).expect("a constant tempo map is valid"));

    let lane = Arc::new(Lane::new(vec![Placement::new(Tick::new(0), arpeggio())]));
    let mut player = ClipPlayer::new(lane);
    let mut tracker = NoteTracker::new();

    let (mut transport, mut handle) = Transport::new(Arc::clone(&tempo));
    handle
        .set_loop(Some(TickRange::new(Tick::new(0), Tick::new(BAR))))
        .expect("the command queue is empty at startup");
    handle
        .play()
        .expect("the command queue is empty at startup");

    let mut clock = ClockMaster::new();
    // Preallocated staging for one block: notes and clock merged, then sorted. The callback must
    // not allocate (invariant 11), so this is sized once and only ever cleared.
    let mut staged: Vec<(u32, Outgoing)> = Vec::with_capacity(1024);

    let client = ClientBuilder::new("miau midi out")
        .midi_out(1)
        .start(move |block| {
            let frames = block.frames();
            staged.clear();

            // The block's origin, updated by every Play slice so a Flush lands at the right offset.
            let mut at = 0_u32;
            let mut start = transport.playhead();
            let mut played = false;

            transport.process_block(frames, |step| match step {
                BlockStep::Play { at: slice, ticks } => {
                    at = slice.get();
                    start = ticks.start;
                    played = true;
                    let mut out = BlockSink {
                        staged: &mut staged,
                        tracker: Some(&mut tracker),
                        tempo: &tempo,
                        frames,
                        at,
                        start,
                    };
                    player.events_in(ticks, &mut out);
                    // Clock rides the same port, placed by the same tick→offset arithmetic.
                    clock.slice(true, ticks, &mut |tick, message| {
                        let offset = out.offset_of(tick);
                        out.staged.push((offset, Outgoing::Sync(message)));
                    });
                }
                // invariant 8: the engine closes hanging notes on every loop wrap, stop and
                // locate — the clip player never has to.
                BlockStep::Flush { tick } => {
                    let mut out = BlockSink {
                        staged: &mut staged,
                        tracker: None,
                        tempo: &tempo,
                        frames,
                        at,
                        start,
                    };
                    tracker.flush(tick, &mut out);
                }
            });

            // A stopped transport still owes the receiver a Stop.
            if !played {
                clock.slice(false, TickRange::new(start, start), &mut |_, message| {
                    staged.push((0, Outgoing::Sync(message)));
                });
            }

            // A SPA sequence is ordered. Notes and clock are two ascending streams, so the merged
            // result has to be sorted before it goes out. Stable, so same-offset items keep the
            // order they were staged in, and allocation-free on an already-sized buffer.
            staged.sort_by_key(|(offset, _)| *offset);

            block.midi_out(0, |sink| {
                for (offset, item) in staged.iter() {
                    match item {
                        Outgoing::Note(event) => sink.push(*offset, event),
                        Outgoing::Sync(message) => sink.push_sync(*offset, *message),
                    };
                }
            });
        })?;

    println!(
        "MIDI out at {BPM} BPM on node {} — route \"miau midi out\" to a synth (e.g. in qpwgraph). \
         Ctrl-C to stop.",
        client.node_id()
    );

    // The client runs on its own thread; parking here keeps it alive until Ctrl-C.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
