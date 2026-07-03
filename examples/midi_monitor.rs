//! Prints typed events from a hardware MIDI keyboard.
//! Run with `cargo run --example midi_monitor`; press Ctrl-C to stop.
//!
//! The stream exposes a MIDI input port. Route your keyboard to "miau midi monitor" externally
//! (e.g. in qpwgraph, or `pw-link` by port id) — the core does no routing itself; until a
//! source is routed, nothing prints. Each control is parsed to a typed
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
        // Skip the steady stream of MIDI clock / active-sensing (system real-time, >= 0xF8);
        // grooveboxes like the Circuit send it continuously. Show anything else the core does
        // not model yet as raw bytes.
        None if control.bytes.first().is_some_and(|&b| b >= 0xF8) => {}
        None => println!(
            "@{:<5} [raw {:?}] {:02x?}",
            control.offset, control.kind, control.bytes
        ),
    })
}
