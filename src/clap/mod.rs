//! CLAP hosting via `clack`: discovery, the `ClapNode` (a realtime audio-processor face in the
//! arena, a main-thread face for GUI/state), event translation `(SampleOffset, Event)` ↔ CLAP,
//! parameters as [`ParamChange`](crate::event::Event::ParamChange), opaque state blobs, and a
//! placeholder node for missing plugins (invariant 14). Instantiation and activation happen on
//! the main thread; a plugin enters the graph snapshot only once activated. Deactivation exits
//! via the garbage chute (invariant 11).
//!
//! Phase 5 — in progress. Landed so far: discovery + the miau host handlers. The `ClapNode`
//! graph integration, event/param translation and the placeholder node follow.

mod discovery;
mod host;

pub use discovery::{PluginInfo, scan, search_paths};

#[cfg(test)]
mod tests;
