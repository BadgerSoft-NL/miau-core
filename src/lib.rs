//! miau-core — a DAW-grade audio/MIDI engine in Rust.
//!
//! Read `LLM.md` first: it is the architectural contract and its 15 invariants are law.
//! This crate is the shared engine ("core") beneath a family of music applications
//! ("shells"); it knows nothing about UI. Core = engine, transport, events, routing, I/O,
//! persistence. Shell = everything a human sees and touches.
//!
//! ## Module map
//!
//! Dependency direction is law (invariant 1): `time` and `event` import from nobody;
//! `io` and `clap` may know everything. Rust does not enforce this — we do.

// Pure logic. `time` imports from nobody; `event` and `arrange` may use `time` but nothing
// above them (invariant 1).
pub mod arrange;
pub mod event;
pub mod time;

// Concurrency, routing and the data-processing extension points.
pub mod dsp;
pub mod graph;
pub mod persist;

// The outermost layer — hardware and plugins may know everything.
pub mod clap;
pub mod io;
