//! Graph tests: byte-exact offline render, mid-render snapshot swap, cycle rejection, the
//! garbage chute, and the event path through an `EventSourceNode`.

use std::f64::consts::TAU;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::arrange::{Clip, ClipPlayer, Lane, Placement};
use crate::event::Event;
use crate::time::{SamplePos, TempoMap, Tick, TickRange};

const SR: u32 = 48_000;
const BLOCK: usize = 256;

fn ctx(frames: usize, block_start: u64) -> ProcessContext<'static> {
    ProcessContext {
        frames,
        sample_rate: SR,
        block_start: SamplePos::new(block_start),
        ticks: TickRange::new(Tick::new(0), Tick::new(0)),
        tempo: None,
    }
}

fn sine(freq: f64, n: usize) -> f32 {
    (TAU * freq * n as f64 / f64::from(SR)).sin() as f32
}

#[test]
fn offline_render_is_byte_exact() {
    let freq = 440.0;
    let gain = 0.5_f32;
    let mut graph = Graph::new(BLOCK);
    let s = graph.add_node(Sine::new(freq));
    let g = graph.add_node(Gain::new(gain));
    let c = graph.add_node(Capture::new());
    graph.connect_audio((s, 0), (g, 0)).unwrap();
    graph.connect_audio((g, 0), (c, 0)).unwrap();
    graph.commit();

    let blocks = 4;
    for b in 0..blocks {
        graph.process(&ctx(BLOCK, (b * BLOCK) as u64));
    }

    let expected: Vec<f32> = (0..BLOCK * blocks).map(|n| sine(freq, n) * gain).collect();
    assert_eq!(
        graph.node::<Capture>(c).unwrap().captured(),
        expected.as_slice()
    );
}

#[test]
fn topology_swap_mid_render_keeps_phase_continuous() {
    let freq = 220.0;
    let gain = 0.25_f32;
    let mut graph = Graph::new(BLOCK);
    let s = graph.add_node(Sine::new(freq));
    let g = graph.add_node(Gain::new(gain));
    let c = graph.add_node(Capture::new());
    graph.connect_audio((s, 0), (g, 0)).unwrap();
    graph.connect_audio((g, 0), (c, 0)).unwrap();
    graph.commit();

    // Render two blocks through the gain.
    graph.process(&ctx(BLOCK, 0));
    graph.process(&ctx(BLOCK, BLOCK as u64));

    // Swap the topology: bypass the gain, feeding the sine straight into capture.
    graph.disconnect_audio((g, 0), (c, 0));
    graph.connect_audio((s, 0), (c, 0)).unwrap();
    graph.commit();

    // Render two more blocks after the snapshot swap.
    graph.process(&ctx(BLOCK, (2 * BLOCK) as u64));
    graph.process(&ctx(BLOCK, (3 * BLOCK) as u64));

    let expected: Vec<f32> = (0..4 * BLOCK)
        .map(|n| {
            if n < 2 * BLOCK {
                sine(freq, n) * gain
            } else {
                sine(freq, n)
            }
        })
        .collect();
    // The sine node's phase carried across the swap: sample 2*BLOCK is sine(2*BLOCK), not a reset.
    assert_eq!(
        graph.node::<Capture>(c).unwrap().captured(),
        expected.as_slice()
    );
}

#[test]
fn cycles_and_bad_ports_are_rejected() {
    let mut graph = Graph::new(64);
    let a = graph.add_node(Gain::new(1.0));
    let b = graph.add_node(Gain::new(1.0));
    graph.connect_audio((a, 0), (b, 0)).unwrap();

    assert_eq!(
        graph.connect_audio((b, 0), (a, 0)),
        Err(ConnectError::WouldCycle)
    );
    assert_eq!(
        graph.connect_audio((a, 0), (a, 0)),
        Err(ConnectError::WouldCycle)
    );

    // A second source into an occupied input is refused (one writer per input).
    let s = graph.add_node(Sine::new(1.0));
    assert_eq!(
        graph.connect_audio((s, 0), (b, 0)),
        Err(ConnectError::InputAlreadyConnected)
    );

    // Out-of-range port and unknown node.
    assert_eq!(
        graph.connect_audio((s, 1), (a, 0)),
        Err(ConnectError::PortOutOfRange)
    );
    let ghost = a;
    graph.remove_node(ghost);
    graph.commit();
    assert_eq!(
        graph.connect_audio((s, 0), (ghost, 0)),
        Err(ConnectError::UnknownNode)
    );
}

/// A node whose drop bumps a shared counter — used to observe the chute actually dropping it.
struct DropCounter(Arc<AtomicUsize>);

impl Node for DropCounter {
    fn ports(&self) -> PortCounts {
        PortCounts::default()
    }
    fn process(&mut self, _ctx: &ProcessContext, _io: &mut NodeIO) {}
}

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn chute_drops_retired_nodes_on_the_cleanup_thread() {
    let counter = Arc::new(AtomicUsize::new(0));
    {
        let chute = Chute::new();
        chute.retire_node(Box::new(DropCounter(Arc::clone(&counter))));
        // Dropping the chute closes the channel and joins the worker, which drains and drops.
    }
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

/// A sink node that records the events arriving at its event input, for assertions.
#[derive(Default)]
struct EventLog(Vec<(crate::time::SampleOffset, Event)>);

impl EventLog {
    fn events(&self) -> &[(crate::time::SampleOffset, Event)] {
        &self.0
    }
}

impl Node for EventLog {
    fn ports(&self) -> PortCounts {
        PortCounts {
            event_in: 1,
            ..PortCounts::default()
        }
    }
    fn process(&mut self, _ctx: &ProcessContext, io: &mut NodeIO) {
        self.0.extend_from_slice(io.events_in(0));
    }
}

#[test]
fn event_source_node_emits_sample_accurate_events() {
    use crate::time::SampleOffset;

    // 120 BPM @ 48 kHz → 25 samples/tick.
    let tempo = TempoMap::constant(SR, 120.0).unwrap();
    let clip = Clip::new(
        Tick::new(1000),
        vec![
            (
                Tick::new(0),
                Event::NoteOn {
                    channel: 0,
                    key: 60,
                    velocity: 100,
                },
            ),
            (
                Tick::new(10),
                Event::NoteOn {
                    channel: 0,
                    key: 64,
                    velocity: 100,
                },
            ),
        ],
    );
    let lane = Arc::new(Lane::new(vec![Placement::new(Tick::new(0), clip)]));

    let mut graph = Graph::new(512);
    let src = graph.add_node(EventSourceNode::new(ClipPlayer::new(lane)));
    let log = graph.add_node(EventLog::default());
    graph.connect_event((src, 0), (log, 0)).unwrap();
    graph.commit();

    // One 500-sample block covering ticks [0, 20).
    graph.process(&ProcessContext {
        frames: 500,
        sample_rate: SR,
        block_start: SamplePos::new(0),
        ticks: TickRange::new(Tick::new(0), Tick::new(20)),
        tempo: Some(&tempo),
    });

    assert_eq!(
        graph.node::<EventLog>(log).unwrap().events(),
        &[
            (
                SampleOffset::new(0),
                Event::NoteOn {
                    channel: 0,
                    key: 60,
                    velocity: 100
                }
            ),
            (
                SampleOffset::new(250),
                Event::NoteOn {
                    channel: 0,
                    key: 64,
                    velocity: 100
                }
            ),
        ]
    );
}

/// The io edge reads events out of the graph the same way it reads audio — this is the only way
/// out for a MIDI-only shell, which has no audio buffer to pull.
#[test]
fn events_output_exposes_an_event_port_to_the_io_edge() {
    use crate::time::SampleOffset;

    let tempo = TempoMap::constant(SR, 120.0).unwrap(); // 25 samples/tick
    let clip = Clip::new(
        Tick::new(1000),
        vec![(
            Tick::new(10),
            Event::NoteOn {
                channel: 3,
                key: 48,
                velocity: 90,
            },
        )],
    );
    let lane = Arc::new(Lane::new(vec![Placement::new(Tick::new(0), clip)]));

    let mut graph = Graph::new(512);
    let src = graph.add_node(EventSourceNode::new(ClipPlayer::new(lane)));
    graph.commit();

    graph.process(&ProcessContext {
        frames: 500,
        sample_rate: SR,
        block_start: SamplePos::new(0),
        ticks: TickRange::new(Tick::new(0), Tick::new(20)),
        tempo: Some(&tempo),
    });

    assert_eq!(
        graph.events_output(src, 0),
        Some(
            &[(
                SampleOffset::new(250),
                Event::NoteOn {
                    channel: 3,
                    key: 48,
                    velocity: 90
                }
            )][..]
        )
    );

    // An audio port, an out-of-range port and a dead handle all say "nothing here", never panic.
    assert_eq!(graph.events_output(src, 1), None);
    let gone = graph.add_node(Gain::new(1.0));
    graph.remove_node(gone);
    graph.commit();
    assert_eq!(graph.events_output(gone, 0), None);
}
