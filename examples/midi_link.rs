//! Routing from inside the app: list the machine's MIDI destinations and link one of them
//! yourself, no patchbay involved. Run with `cargo run --example midi_link`; Ctrl-C to stop.
//!
//! Without arguments it prints what [`miau_core::io::Patchbay`] sees and keeps watching, so
//! switching a device on or off shows up in the list. With a key as argument — one of the keys
//! it printed — it also links `midi_out_0` to that destination and sends a note every second.
//!
//! Two things worth watching while it runs: the link is made by this process (check with
//! `pw-link -l`), and if you switch the target device off and on again the link comes back by
//! itself, because the wish is held on the key and not on an object id.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use miau_core::event::Event;
use miau_core::io::{ClientBuilder, Error, Patchbay};

fn main() -> Result<(), Error> {
    let wanted = std::env::args().nth(1);

    // A note is due: flipped by the main thread, read and cleared by the callback. The callback
    // runs on the realtime thread, so it may not lock, allocate or print (invariant 11).
    let due = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&due);
    let mut sounding = false;

    let client = ClientBuilder::new("miau midi link")
        .midi_out(1)
        .start(move |block| {
            if !flag.swap(false, Ordering::Relaxed) {
                return;
            }
            let event = if sounding {
                Event::NoteOff {
                    channel: 0,
                    key: 60,
                    velocity: 0,
                }
            } else {
                Event::NoteOn {
                    channel: 0,
                    key: 60,
                    velocity: 96,
                }
            };
            sounding = !sounding;
            block.midi_out(0, |sink| {
                sink.push(0, &event);
            });
        })?;

    let patchbay = Patchbay::start(client.node_id())?;
    // The registry answers in its own time; a moment is enough for the first sweep.
    std::thread::sleep(std::time::Duration::from_millis(500));

    if let Some(key) = &wanted {
        patchbay.set_links(vec![(0, key.clone())]);
        println!("linking midi_out_0 to {key}, one note per second. Ctrl-C to stop.\n");
    } else {
        println!("no destination given — watching only. Pass a key to link to it.\n");
    }

    let mut previous = Vec::new();
    loop {
        let now = patchbay.destinations();
        if now != previous {
            println!("{} destinations:", now.len());
            for dest in &now {
                let mark = if Some(&dest.key) == wanted.as_ref() {
                    "->"
                } else {
                    "  "
                };
                println!("{mark} {:<12} {:<10} {}", dest.device, dest.name, dest.key);
            }
            println!();
            previous = now;
        }
        if wanted.is_some() {
            due.store(true, Ordering::Relaxed);
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
