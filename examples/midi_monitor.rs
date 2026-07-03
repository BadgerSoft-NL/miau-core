//! Prints typed events from a hardware MIDI keyboard. Phase-4 milestone (MIDI half).
//! Run with `cargo run --example midi_monitor`; press Ctrl-C to stop.
//!
//! The stream exposes a MIDI input port. Route your keyboard to "miau midi monitor" externally
//! (e.g. in qpwgraph) — the core does no routing itself. Each control is parsed to a typed
//! [`miau_core::event::Event`] at the io edge; controls the core doesn't model are shown raw so
//! the encoding (legacy MIDI vs UMP) is visible while wiring things up.

use miau_core::io::{Error, run_midi_input};

fn main() -> Result<(), Error> {
    println!(
        "MIDI monitor — route your keyboard to \"miau midi monitor\" (e.g. in qpwgraph). \
         Ctrl-C to stop."
    );

    run_midi_input("miau midi monitor", |control| match control.event() {
        Some(event) => println!("@{:<5} {event:?}", control.offset),
        None => println!(
            "@{:<5} [raw {:?}] {:02x?}",
            control.offset, control.kind, control.bytes
        ),
    })
}
