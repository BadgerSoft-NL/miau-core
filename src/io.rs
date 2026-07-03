//! The PipeWire client and hardware MIDI. The engine is *pulled* from PipeWire's process
//! callback — the core has no audio thread of its own. Raw MIDI becomes typed events at this
//! edge (invariant 6). Filled in phase 4.
