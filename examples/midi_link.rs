//! Routing from inside the app: list the machine's MIDI endpoints and link them yourself, no
//! external patchbay involved. Run with `cargo run --example midi_link`; Ctrl-C to stop.
//!
//! Without arguments it prints what [`miau_core::io::Patchbay`] sees — destinations and sources —
//! and keeps watching, so switching a device on or off shows up in the list.
//!
//! ```text
//! cargo run --example midi_link -- <destination-key> [source-key]
//! ```
//!
//! With a destination key it links `midi_out_0` to it and sends a note every second. With a source
//! key as well it links that source into `midi_in_0` and counts what arrives, which is the
//! cheapest proof that a link *into* this process carries traffic.
//!
//! Three things worth watching while it runs: the links are made by this process (check with
//! `pw-link -l`), if you switch a target device off and on again the link comes back by itself
//! because the wish is held on the key and not on an object id, and both directions travel in one
//! `set_links` call — passing half the picture would take the other half away.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use miau_core::event::Event;
use miau_core::io::{ClientBuilder, Error, LinkWish, MidiEndpoint, Patchbay};

fn print_list(title: &str, list: &[MidiEndpoint], marked: Option<&String>) {
    println!("{} {title}:", list.len());
    for endpoint in list {
        let mark = if Some(&endpoint.key) == marked {
            "->"
        } else {
            "  "
        };
        println!(
            "{mark} {:<12} {:<10} {}",
            endpoint.device, endpoint.name, endpoint.key
        );
    }
    println!();
}

fn main() -> Result<(), Error> {
    let mut args = std::env::args().skip(1);
    let destination = args.next();
    let source = args.next();

    // A note is due: flipped by the main thread, read and cleared by the callback. The callback
    // runs on the realtime thread, so it may not lock, allocate or print (invariant 11).
    let due = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&due);
    let mut sounding = false;

    // How much has come in. An atomic and not a ring buffer: this example is about the link
    // existing, not about what travels over it — `midi_monitor` prints the events themselves.
    let arrived = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&arrived);

    let client = ClientBuilder::new("miau midi link")
        .midi_out(1)
        .midi_in(1)
        .start(move |block| {
            let mut seen = 0_u64;
            block.midi_in(0, |_| seen += 1);
            if seen > 0 {
                counter.fetch_add(seen, Ordering::Relaxed);
            }

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

    // Both directions in one call: the patchbay holds a single wish list, so a second call with
    // only the outgoing link would drop the incoming one.
    let mut wishes = Vec::new();
    if let Some(key) = &destination {
        wishes.push(LinkWish::to_destination(0, key.clone()));
        println!("linking midi_out_0 to {key}, one note per second.");
    }
    if let Some(key) = &source {
        wishes.push(LinkWish::from_source(0, key.clone()));
        println!("linking {key} into midi_in_0, counting what arrives.");
    }
    if wishes.is_empty() {
        println!("no keys given — watching only. Pass a destination key to link to it.");
    }
    patchbay.set_links(wishes);
    println!("Ctrl-C to stop.\n");

    let mut previous = (Vec::new(), Vec::new());
    let mut reported = 0;
    loop {
        let now = (patchbay.destinations(), patchbay.sources());
        if now != previous {
            print_list("destinations", &now.0, destination.as_ref());
            print_list("sources", &now.1, source.as_ref());
            previous = now;
        }
        let total = arrived.load(Ordering::Relaxed);
        if total != reported {
            println!("{total} controls in");
            reported = total;
        }
        if destination.is_some() {
            due.store(true, Ordering::Relaxed);
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
