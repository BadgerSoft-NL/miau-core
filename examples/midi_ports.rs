//! One client, many ports: a four-voice arpeggio where every voice gets its own MIDI output, plus
//! a MIDI input read in the same block. Run with `cargo run --example midi_ports`; Ctrl-C to stop.
//!
//! This is what [`miau_core::io::Client`] adds over the single-port `run_*` helpers. The node
//! "miau midi ports" exposes `midi_out_0..3` and `midi_in_0`; route each output to a different
//! synth in qpwgraph and the arpeggio spreads across four instruments. Everything happens in one
//! process callback, so input and output share a block — no ring buffer between two clients.
//!
//! The count of incoming events is published through an atomic and printed by the main thread:
//! the process callback runs on the realtime thread and must not print (invariant 11).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use miau_core::arrange::{Clip, ClipPlayer, Lane, Placement};
use miau_core::event::{Event, EventSink, EventSource, NoteTracker};
use miau_core::io::{ClientBuilder, Error, MidiSink};
use miau_core::time::{TempoMap, Tick, TickRange};
use miau_core::transport::{BlockStep, Transport};

const RATE: u32 = 48_000;
const BPM: f64 = 112.0;
/// One 4/4 bar at 960 PPQ — the loop length, and one beat per voice.
const BAR: u64 = 4 * Tick::PPQ;
const GATE: u64 = Tick::PPQ - 60;
/// One output port per voice.
const VOICES: usize = 4;

/// Bridges ticks to wire offsets for one port's sink. Same shape as `examples/midi_out.rs`.
struct BlockSink<'a, 'b> {
    out: &'a mut MidiSink<'b>,
    tracker: Option<&'a mut NoteTracker>,
    tempo: &'a TempoMap,
    at: u32,
    start: Tick,
}

impl EventSink for BlockSink<'_, '_> {
    fn push(&mut self, at: Tick, event: Event) {
        if let Some(tracker) = self.tracker.as_deref_mut() {
            tracker.observe(&event);
        }
        let offset = self
            .tempo
            .offset_in_block(self.start, self.at, self.out.frames(), at);
        self.out.push(offset, &event);
    }
}

/// Voice `beat`'s one-note clip: it sounds on its own beat of the bar and nowhere else.
fn voice(beat: u64, key: u8) -> Clip {
    let on = beat * Tick::PPQ;
    Clip::new(
        Tick::new(BAR),
        vec![
            (
                Tick::new(on),
                Event::NoteOn {
                    channel: 0,
                    key,
                    velocity: 96,
                },
            ),
            (
                Tick::new(on + GATE),
                Event::NoteOff {
                    channel: 0,
                    key,
                    velocity: 0,
                },
            ),
        ],
    )
}

fn main() -> Result<(), Error> {
    let tempo = Arc::new(TempoMap::constant(RATE, BPM).expect("a constant tempo map is valid"));

    // One player and one note tracker per voice — each drives its own output port.
    let keys = [60_u8, 64, 67, 71];
    let mut players: Vec<ClipPlayer> = (0..VOICES)
        .map(|beat| {
            let lane = Lane::new(vec![Placement::new(
                Tick::new(0),
                voice(beat as u64, keys[beat]),
            )]);
            ClipPlayer::new(Arc::new(lane))
        })
        .collect();
    let mut trackers = vec![NoteTracker::new(); VOICES];

    let (mut transport, mut handle) = Transport::new(Arc::clone(&tempo));
    handle
        .set_loop(Some(TickRange::new(Tick::new(0), Tick::new(BAR))))
        .expect("the command queue is empty at startup");
    handle
        .play()
        .expect("the command queue is empty at startup");

    let received = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&received);

    // Preallocated once, reused every block: the transport emits at most a handful of steps and
    // the callback must not allocate (invariant 11).
    let mut steps: Vec<BlockStep> = Vec::with_capacity(16);

    let client = ClientBuilder::new("miau midi ports")
        .midi_out(VOICES)
        .midi_in(1)
        .start(move |block| {
            // Input and output in the same block — the whole point of one client.
            block.midi_in(0, |control| {
                if control.event().is_some() {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            });

            // The transport is shared by every voice, so schedule once and fan the slices out.
            let mut at = 0_u32;
            let mut start = transport.playhead();
            steps.clear();
            transport.process_block(block.frames(), |step| steps.push(step));

            for port in 0..VOICES {
                let player = &mut players[port];
                let tracker = &mut trackers[port];
                let (mut at_v, mut start_v) = (at, start);
                block.midi_out(port, |sink| {
                    for step in &steps {
                        match *step {
                            BlockStep::Play { at: slice, ticks } => {
                                at_v = slice.get();
                                start_v = ticks.start;
                                let mut out = BlockSink {
                                    out: sink,
                                    tracker: Some(tracker),
                                    tempo: &tempo,
                                    at: at_v,
                                    start: start_v,
                                };
                                player.events_in(ticks, &mut out);
                            }
                            BlockStep::Flush { tick } => {
                                let mut out = BlockSink {
                                    out: sink,
                                    tracker: None,
                                    tempo: &tempo,
                                    at: at_v,
                                    start: start_v,
                                };
                                tracker.flush(tick, &mut out);
                            }
                        }
                    }
                });
                at = at_v;
                start = start_v;
            }
        })?;

    println!(
        "miau midi ports — node {} with midi_out_0..{} and midi_in_0.\n\
         Route each output to a synth (e.g. in qpwgraph). Ctrl-C to stop.",
        client.node_id(),
        VOICES - 1
    );

    loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        println!(
            "incoming events so far: {}",
            received.load(Ordering::Relaxed)
        );
    }
}
