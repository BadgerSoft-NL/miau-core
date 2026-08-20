//! Plays a 440 Hz sine through the graph — the smallest audible use of the engine.
//! Run with `cargo run --example beep`; press Ctrl-C to stop. Requires a running PipeWire session.
//!
//! Route "miau beep" to your speakers externally (e.g. in qpwgraph, or
//! `pw-link "miau beep:audio_out_0" <your sink>`). The core exposes ports and routes nothing
//! itself, so until it is patched you hear silence.
//!
//! The graph is `Sine → Gain`; PipeWire pulls one block at a time from the process callback,
//! which drives `graph.process` and copies the gain node's output into the hardware buffer.

use miau_core::graph::{Gain, Graph, ProcessContext, Sine};
use miau_core::io::{ClientBuilder, Error};
use miau_core::time::{SamplePos, Tick, TickRange};

const RATE: u32 = 48_000;

fn main() -> Result<(), Error> {
    let mut graph = Graph::new(16_384);
    let sine = graph.add_node(Sine::new(440.0));
    let gain = graph.add_node(Gain::new(0.2)); // keep it gentle on the ears
    graph.connect_audio((sine, 0), (gain, 0)).unwrap();
    graph.commit();

    let mut position: u64 = 0;

    let client = ClientBuilder::new("miau beep")
        .audio_out(1)
        .start(move |block| {
            let Some(out) = block.audio_out(0) else {
                return;
            };
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
        })?;

    println!(
        "playing 440 Hz on node {} — route \"miau beep\" to your speakers (e.g. in qpwgraph). \
         Ctrl-C to stop.",
        client.node_id()
    );

    // The client runs on its own thread; parking here keeps it alive until Ctrl-C.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
