//! Document types, the `project.miau` ZIP container and the `.miauclip` exchange format.
//!
//! The core persists **documents, not projects** (design log note 10): a [`Document`] is the
//! serde model of the musical content — tempo map, lanes & clips, graph topology (nodes as a
//! registry type-id plus settings, connections by [`NodeId`] + port), plugin blobs. Track names,
//! mixer structure and UI layout are shell data, carried opaquely in `shell/` sections.
//!
//! Two invariants govern this module. **14 — persistence never discards what it doesn't
//! understand:** unknown JSON fields (via a flattened `extra` map), foreign `shell/` sections and
//! missing-plugin blobs all survive a load/save round trip. **15 — runtime indices are never
//! persisted:** documents reference stable [`NodeId`]s from a document-owned monotonic counter,
//! and the runtime [`Graph`](crate::graph::Graph) arena is rebuilt on load via
//! [`Document::instantiate`]. Unknown registry ids fall back to a
//! [`Placeholder`](crate::graph::Placeholder) with a warning, never a crash (invariant 7).

mod container;
mod document;
mod registry;

pub use container::{MiauClip, Project};
pub use document::{Connection, Document, Instantiated, NodeSpec};
pub use registry::{BuildError, NodeRegistry};

use std::fmt;

/// The container format tag written into every `core.json` / `clip.json`.
pub const FORMAT: &str = "miau-core";
/// The current document format version. Additive changes don't bump it (design log note 10 §3).
pub const VERSION: u32 = 1;

/// A stable, document-owned node identity (invariant 15).
///
/// Allocated from the [`Document`]'s monotonic counter and never reused; connections reference it
/// plus a port index. The runtime arena's `NodeKey` is a separate, non-persisted handle rebuilt on
/// load. Serializes transparently as its raw `u64`.
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, serde::Serialize, serde::Deserialize,
)]
pub struct NodeId(pub u64);

/// Everything that can go wrong loading, building or saving a document.
#[derive(Debug)]
pub enum PersistError {
    /// The stored tempo segments did not form a valid [`TempoMap`](crate::time::TempoMap).
    Tempo(crate::time::TempoMapError),
    /// A known registry `kind` rejected its stored settings.
    Build(BuildError),
    /// A connection could not be made in the rebuilt graph (bad port, cycle, …).
    Connect(crate::graph::ConnectError),
    /// A connection referenced a [`NodeId`] absent from the document.
    UnknownNodeRef(NodeId),
    /// The container was not a `miau-core` document, or a required entry was missing/misshaped.
    Format(String),
    /// An embedded sample's bytes did not match its stored BLAKE3 hash (design log note 10 §5).
    HashMismatch {
        /// The sample path whose contents failed verification.
        path: String,
    },
    /// JSON (de)serialization failed.
    Json(serde_json::Error),
    /// The ZIP container could not be read or written.
    Zip(zip::result::ZipError),
    /// Underlying I/O failed.
    Io(std::io::Error),
}

impl fmt::Display for PersistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tempo(e) => write!(f, "invalid tempo map: {e}"),
            Self::Build(e) => write!(f, "node build failed: {e}"),
            Self::Connect(e) => write!(f, "connection rejected: {e}"),
            Self::UnknownNodeRef(id) => write!(f, "connection references unknown node {}", id.0),
            Self::Format(msg) => write!(f, "malformed document: {msg}"),
            Self::HashMismatch { path } => write!(f, "sample hash mismatch for {path}"),
            Self::Json(e) => write!(f, "json error: {e}"),
            Self::Zip(e) => write!(f, "zip error: {e}"),
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for PersistError {}

impl From<serde_json::Error> for PersistError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

impl From<zip::result::ZipError> for PersistError {
    fn from(e: zip::result::ZipError) -> Self {
        Self::Zip(e)
    }
}

impl From<std::io::Error> for PersistError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
mod tests;
