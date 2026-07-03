//! CLAP hosting via `clack`: discovery, the `ClapNode` (a realtime audio-processor face in the
//! arena, a main-thread face for GUI/state), event translation `(SampleOffset, Event)` ↔ CLAP,
//! parameters as [`ParamChange`](crate::event::Event::ParamChange), opaque state blobs, and a
//! placeholder node for missing plugins (invariant 14 — the generic
//! [`Placeholder`] lives in `graph/`, re-exported here for
//! convenience). Instantiation and activation happen on the main thread; a plugin enters the
//! graph snapshot only once activated. Deactivation exits via the garbage chute (invariant 11).
//!
//! Landed: discovery, the miau host handlers, the [`ClapNode`] graph integration with event/param
//! translation, the main-thread [`ClapPlugin`] handle, and the live `clap_synth` example. Missing
//! plugins round-trip through persistence as a [`Placeholder`].

mod discovery;
mod host;
mod node;
mod plugin;

pub use crate::graph::Placeholder;
pub use discovery::{PluginInfo, scan, search_paths};
pub use node::ClapNode;
pub use plugin::{ClapError, ClapPlugin, ParamDesc};

#[cfg(test)]
mod tests;
