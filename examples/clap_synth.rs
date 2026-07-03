//! Live instrument host: a hardware MIDI keyboard plays a CLAP synth (e.g. Surge XT) out of the
//! speakers, while an automation-style `ParamChange` LFO audibly sweeps one of the plugin's
//! parameters. Phase-5 milestone: miau-core is an instrument host.
//!
//! Usage:
//!   cargo run --example clap_synth -- [plugin-substring] [param-id]
//!     plugin-substring   case-insensitive match against a discovered plugin's name or id
//!                        (default: "surge")
//!     param-id           CLAP parameter id to sweep; omit to auto-pick a likely-audible one
//!
//! Two PipeWire streams run, each on its own main loop and thread: the MIDI capture stream feeds
//! typed events across a lock-free ring buffer into the graph's `LiveInput` node (invariant 11: the
//! audio thread never blocks or allocates), and the audio playback stream *pulls* the graph. Route
//! your keyboard to "miau clap synth" and the output to your speakers externally (e.g. qpwgraph).
//! Ctrl-C to stop.
//!
//! io audio output is mono while a synth emits N (usually stereo) channels, so this monitors the
//! plugin's channel 0 (left).

use std::error::Error;

use miau_core::clap::{ClapPlugin, ParamDesc, PluginInfo, scan};
use miau_core::event::{Event, ParamId};
use miau_core::graph::{Graph, Node, NodeIO, PortCounts, ProcessContext};
use miau_core::io::{run_audio_output, run_midi_input};
use miau_core::time::{SampleOffset, SamplePos, Tick, TickRange};

const RATE: u32 = 48_000;
/// Max graph block; also the plugin's `max_frames_count`. Generous so any negotiated PipeWire
/// quantum fits (the audio callback renders `frames <= MAX_BLOCK`).
const MAX_BLOCK: usize = 16_384;
/// Ring-buffer capacity in events — deep enough that a busy keyboard never overruns one audio block.
const RING: usize = 1024;
/// Sweep rate of the automation LFO, in Hz.
const LFO_HZ: f64 = 0.15;

/// The graph's live-input node: it drains hardware MIDI events from the ring buffer and, if enabled,
/// emits an automation `ParamChange` each block. Both ride the single event output into the synth.
struct LiveInput {
    rx: rtrb::Consumer<Event>,
    automation: Option<Automation>,
}

/// A parameter sweep: a triangle LFO between `min` and `max` on parameter `param`.
struct Automation {
    param: u32,
    min: f64,
    max: f64,
}

impl Node for LiveInput {
    fn ports(&self) -> PortCounts {
        PortCounts {
            event_out: 1,
            ..PortCounts::default()
        }
    }

    fn process(&mut self, ctx: &ProcessContext, io: &mut NodeIO) {
        let out = io.events_out(0);

        // Live MIDI has no meaningful offset within *this* audio block (it arrived asynchronously on
        // the capture stream), so place every drained event at the block start. FIFO order is kept,
        // which satisfies the nondecreasing-offset order the synth requires.
        while let Ok(event) = self.rx.pop() {
            out.push((SampleOffset::new(0), event));
        }

        if let Some(a) = &self.automation {
            let t = ctx.block_start.get() as f64 / f64::from(ctx.sample_rate);
            let phase = (t * LFO_HZ).fract(); // 0.0..1.0
            let tri = if phase < 0.5 {
                phase * 2.0
            } else {
                2.0 - phase * 2.0
            }; // 0 -> 1 -> 0
            let value = a.min + (a.max - a.min) * tri;
            out.push((
                SampleOffset::new(0),
                Event::ParamChange {
                    param: ParamId(a.param),
                    value,
                },
            ));
        }
    }
}

/// Case-insensitive match of `query` against a plugin's name or id.
fn matches(query: &str, info: &PluginInfo) -> bool {
    let q = query.to_lowercase();
    info.name.to_lowercase().contains(&q) || info.id.to_lowercase().contains(&q)
}

/// Picks a default parameter to sweep: the first whose name hints at pitch/timbre (so the ear can
/// hear it move), else the first parameter of all.
fn default_param(params: &[ParamDesc]) -> Option<&ParamDesc> {
    const HINTS: [&str; 4] = ["cutoff", "freq", "filter", "pitch"];
    params
        .iter()
        .find(|p| HINTS.iter().any(|h| p.name.to_lowercase().contains(h)))
        .or_else(|| params.first())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    let query = args.get(1).map(String::as_str).unwrap_or("surge");

    let plugins = scan();
    if plugins.is_empty() {
        eprintln!("no CLAP plugins found in ~/.clap or /usr/lib/clap");
        return Ok(());
    }
    let Some(info) = plugins.iter().find(|p| matches(query, p)) else {
        eprintln!("no plugin matching {query:?}. Discovered:");
        for p in &plugins {
            eprintln!("  {}  ({})", p.name, p.id);
        }
        return Ok(());
    };
    println!("hosting {:?} from {}", info.name, info.path.display());

    let (mut plugin, node) = ClapPlugin::open(info, RATE, MAX_BLOCK)?;
    let channels = node.ports().audio_out;
    if channels == 0 {
        eprintln!(
            "warning: {:?} has no audio output — you will hear silence",
            info.name
        );
    }

    // Enumerate parameters and choose the automation target.
    let params = plugin.params();
    let chosen = match args.get(2).map(|s| s.parse::<u32>()) {
        Some(Ok(id)) => match params.iter().find(|p| p.id == id) {
            Some(p) => Some(Automation {
                param: p.id,
                min: p.min,
                max: p.max,
            }),
            None => {
                eprintln!("param id {id} not enumerated; sweeping 0.0..1.0");
                Some(Automation {
                    param: id,
                    min: 0.0,
                    max: 1.0,
                })
            }
        },
        Some(Err(_)) => {
            eprintln!("could not parse param id argument; automation disabled");
            None
        }
        None => default_param(&params).map(|p| Automation {
            param: p.id,
            min: p.min,
            max: p.max,
        }),
    };
    println!("plugin exposes {} parameter(s)", params.len());
    match &chosen {
        Some(a) => {
            let name = params
                .iter()
                .find(|p| p.id == a.param)
                .map_or("<unknown>", |p| p.name.as_str());
            println!(
                "sweeping param {} ({name}) over {:.3}..{:.3} at {LFO_HZ} Hz",
                a.param, a.min, a.max
            );
        }
        None => println!("automation disabled"),
    }

    // Opaque state blob (invariant 14): save it and load it straight back to show the plugin accepts
    // its own blob. The full get -> destroy -> recreate -> set cycle is covered by the gated tests.
    if let Some(blob) = plugin.save_state() {
        let ok = plugin.load_state(&blob);
        println!("state blob: {} bytes, reload accepted: {ok}", blob.len());
    }

    // Build the graph: LiveInput (MIDI + automation) -> ClapNode -> speakers (channel 0).
    let (tx, rx) = rtrb::RingBuffer::<Event>::new(RING);
    let mut graph = Graph::new(MAX_BLOCK);
    let input = graph.add_node(LiveInput {
        rx,
        automation: chosen,
    });
    let clap = graph.add_node(node);
    graph.connect_event((input, 0), (clap, 0))?;
    graph.commit();

    // Hardware MIDI on its own thread: parse to typed events at the edge, push across the ring. A
    // dropped event (ring full) is better than blocking the capture callback.
    std::thread::spawn(move || {
        let mut tx = tx;
        let result = run_midi_input("miau clap synth", move |control| {
            if let Some(event) = control.event() {
                let _ = tx.push(event);
            }
        });
        if let Err(e) = result {
            eprintln!("MIDI input stopped: {e}");
        }
    });

    // Audio pull on the main thread: drive the graph and copy the synth's channel 0 to the speakers.
    // `plugin` stays alive here on the main thread (its `!Send` handle is the plugin's other face);
    // dropping the retired node through the graph's chute performs teardown off the audio thread.
    println!("playing — Ctrl-C to stop");
    let mut position: u64 = 0;
    run_audio_output("miau clap synth", RATE, move |out| {
        let frames = out.len();
        let ctx = ProcessContext {
            frames,
            sample_rate: RATE,
            block_start: SamplePos::new(position),
            ticks: TickRange::new(Tick::new(0), Tick::new(0)),
            tempo: None,
        };
        graph.process(&ctx);
        if let Some(rendered) = graph.output(clap, 0) {
            out.copy_from_slice(&rendered[..frames]);
        }
        position += frames as u64;
    })?;

    Ok(())
}
