//! CLAP hosting via `clack`: `ClapNode` (a realtime face in the arena, a main-thread face for
//! GUI/state), plugin discovery and opaque state blobs. A missing plugin becomes a placeholder
//! node that preserves the blob and its connections (invariant 14). Filled in phase 5.
