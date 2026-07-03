//! The `Node` trait, ports, immutable topology snapshots, the compiled schedule and the
//! preallocated buffer pool. The audio thread iterates the schedule, never the graph
//! (invariants 11 & 12). Filled in phase 3.
