//! Prints typed events from a hardware MIDI keyboard.
//! Run with `cargo run --example midi_monitor`; press Ctrl-C to stop.
//!
//! The client exposes a MIDI input port. Route your keyboard to "miau midi monitor" externally
//! (e.g. in qpwgraph, or `pw-link` by port id) — the core does no routing itself; until a source
//! is routed, nothing prints. Each control is parsed to a typed [`miau_core::event::Event`] at the
//! io edge; controls the core doesn't model are shown raw so the encoding (legacy MIDI vs UMP) is
//! visible while wiring things up.
//!
//! Printing happens on the **main** thread. The process callback runs on the realtime thread,
//! where `println!` would allocate and lock (invariant 11), so events cross over through a
//! lock-free ring buffer — the same pattern any shell uses to get realtime data to its UI.

use miau_core::io::{ClientBuilder, Error, MidiControl, MidiKind};
use miau_core::transport::SyncMessage;

/// Ring capacity in messages — deep enough that a busy keyboard never overruns one block.
const RING: usize = 1024;

/// One observed control, flattened into a `Copy` value so it can cross the ring buffer without
/// allocating: the typed event when the core models it, the raw bytes when it does not.
#[derive(Clone, Copy)]
struct Observed {
    offset: u32,
    kind: MidiKind,
    event: Option<miau_core::event::Event>,
    sync: Option<SyncMessage>,
    bytes: [u8; 4],
    len: u8,
}

impl Observed {
    fn new(control: &MidiControl) -> Self {
        let mut bytes = [0_u8; 4];
        let len = control.bytes.len().min(bytes.len());
        bytes[..len].copy_from_slice(&control.bytes[..len]);
        Self {
            offset: control.offset,
            kind: control.kind,
            event: control.event(),
            sync: control.sync(),
            bytes,
            len: len as u8,
        }
    }
}

fn main() -> Result<(), Error> {
    let (mut tx, mut rx) = rtrb::RingBuffer::<Observed>::new(RING);

    let client = ClientBuilder::new("miau midi monitor")
        .midi_in(1)
        .start(move |block| {
            block.midi_in(0, |control| {
                // A full ring means the main thread is behind; drop rather than block (invariant 2).
                let _ = tx.push(Observed::new(control));
            });
        })?;

    println!(
        "MIDI monitor on node {} — route your keyboard to \"miau midi monitor\" (e.g. in \
         qpwgraph). Ctrl-C to stop.",
        client.node_id()
    );

    let mut clocks: u64 = 0;
    loop {
        while let Ok(observed) = rx.pop() {
            match (observed.event, observed.sync) {
                (Some(event), _) => println!("@{:<5} {event:?}", observed.offset),
                // Clock pulses arrive 24 times a quarter note; counting them keeps the output
                // readable while still proving they are there. Everything else is shown.
                (_, Some(SyncMessage::Clock)) => {
                    clocks += 1;
                    if clocks % 24 == 0 {
                        println!("@{:<5} Clock x{clocks}", observed.offset);
                    }
                }
                (_, Some(message)) => println!("@{:<5} {message:?}", observed.offset),
                // Anything the core does not model yet, shown raw so the encoding is visible.
                (None, None) => println!(
                    "@{:<5} [raw {:?}] {:02x?}",
                    observed.offset,
                    observed.kind,
                    &observed.bytes[..observed.len as usize]
                ),
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
