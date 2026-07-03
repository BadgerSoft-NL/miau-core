//! Plays a 440 Hz sine through the graph out of the speakers. Phase-4 milestone: the engine is
//! audible. Run with `cargo run --example beep`; press Ctrl-C to stop.
//!
//! The graph is `Sine → Gain`; PipeWire pulls one block at a time from the process callback,
//! which drives `graph.process` and copies the gain node's output into the hardware buffer.

use miau_core::graph::{Gain, Graph, ProcessContext, Sine};
use miau_core::io::{Error, run_audio_output};
use miau_core::time::{SamplePos, Tick, TickRange};

const RATE: u32 = 48_000;

fn main() -> Result<(), Error> {
    let mut graph = Graph::new(16_384);
    let sine = graph.add_node(Sine::new(440.0));
    let gain = graph.add_node(Gain::new(0.2)); // keep it gentle on the ears
    graph.connect_audio((sine, 0), (gain, 0)).unwrap();
    graph.commit();

    let mut position: u64 = 0;
    println!("playing 440 Hz — Ctrl-C to stop");

    run_audio_output("miau beep", RATE, move |out| {
        let frames = out.len();
        let ctx = ProcessContext {
            frames,
            sample_rate: RATE,
            block_start: SamplePos::new(position),
            ticks: TickRange::new(Tick::new(0), Tick::new(0)),
            tempo: None,
        };
        graph.process(&ctx);
        if let Some(rendered) = graph.output(gain, 0) {
            out.copy_from_slice(&rendered[..frames]);
        }
        position += frames as u64;
    })
}
