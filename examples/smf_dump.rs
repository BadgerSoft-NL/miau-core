//! Reads a standard MIDI file and prints what the core made of it, then writes it back out and
//! reads that — so a real-world file, not the test suite's own bytes, proves the round trip.
//!
//! Usage:
//!   cargo run --example smf_dump -- <file.mid> [out.mid]
//!
//! Needs no audio server: this is the file half of the io edge and touches no hardware.

use std::error::Error;

use miau_core::event::Event;
use miau_core::io::{SmfFile, read_smf, write_smf};
use miau_core::time::{TempoMap, Tick};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: smf_dump <file.mid> [out.mid]");
        return Ok(());
    };

    let file = read_smf(&std::fs::read(&path)?)?;
    report(&path, &file);

    // A map is what a transport actually wants; building one here is the proof that the segments
    // a file yields are always valid input (see `io/smf.rs`).
    let map = TempoMap::new(48_000, &file.tempo)?;
    let last = file
        .tracks
        .iter()
        .filter_map(|t| t.events.last().map(|(tick, _)| *tick))
        .max()
        .unwrap_or(Tick::new(0));
    let seconds = map.tick_to_sample(last).get() as f64 / f64::from(map.sample_rate());
    println!(
        "  length      {} ticks ({seconds:.1} s at 48 kHz)",
        last.get()
    );

    let bytes = write_smf(&file)?;
    let back = read_smf(&bytes)?;
    // Track 0 of the written file is the tempo track, so the music lines up one further along.
    let same = file
        .tracks
        .iter()
        .zip(back.tracks.iter().skip(1))
        .all(|(a, b)| a.events == b.events);
    println!(
        "\nrewritten: {} bytes, {} tracks, events identical: {same}",
        bytes.len(),
        back.tracks.len()
    );

    if let Some(out) = args.next() {
        std::fs::write(&out, &bytes)?;
        println!("written to {out}");
    }
    Ok(())
}

fn report(path: &str, file: &SmfFile) {
    println!("{path}");
    println!("  tempo       {} segment(s)", file.tempo.len());
    for segment in file.tempo.iter().take(8) {
        println!(
            "    tick {:>10}  {:.2} BPM",
            segment.start.get(),
            segment.bpm
        );
    }
    if file.tempo.len() > 8 {
        println!("    … {} more", file.tempo.len() - 8);
    }
    println!("  tracks      {}", file.tracks.len());
    for (i, track) in file.tracks.iter().enumerate() {
        let notes = track
            .events
            .iter()
            .filter(|(_, e)| matches!(e, Event::NoteOn { .. }))
            .count();
        let name = track.name.as_deref().unwrap_or("—");
        println!(
            "    {i:>3}  {:>6} events, {notes:>6} notes  {name}",
            track.events.len()
        );
    }
}
