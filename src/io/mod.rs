//! The PipeWire edge: the engine is *pulled* from PipeWire's process callback — the core has no
//! audio thread of its own. This module exposes ports and converts at the edge; hardware routing
//! stays external (qpwgraph). Raw MIDI becomes typed events here and nowhere deeper (invariant 6,
//! see [`parse_midi`]/[`parse_ump`]).
//!
//! [`Client`] is the way in and out: one PipeWire node with any mix of MIDI and audio ports,
//! driven by a single process callback on a thread of its own. [`MidiSink`] writes a block's
//! events to a port; [`MidiControl`] reads them back.

use pipewire as pw;
use pw::spa;

mod client;
mod midi;
pub use client::{Block, Client, ClientBuilder};
pub use midi::{
    MidiMessage, encode_midi, encode_sync, encode_sync_ump, encode_ump, parse_midi, parse_sync,
    parse_sync_ump, parse_ump,
};

/// The PipeWire error type, surfaced from setup and connection.
pub use pipewire::Error;

// ---------------------------------------------------------------------------------------------
// MIDI input.
// ---------------------------------------------------------------------------------------------

// Stable `spa_control_type` ABI values. Hardcoded rather than taken from `pw::spa::sys`, because
// `SPA_CONTROL_UMP` only exists in newer libspa headers (PipeWire 1.x) and CI builds against an
// older `libspa-0.2-dev`. The values themselves are fixed by the SPA ABI.
const SPA_CONTROL_MIDI: u32 = 2; // legacy raw MIDI 1.0 bytes
const SPA_CONTROL_UMP: u32 = 4; // MIDI 2.0 Universal MIDI Packet

/// How a MIDI control's payload is encoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MidiKind {
    /// Legacy raw MIDI 1.0 bytes (`SPA_CONTROL_Midi`).
    Legacy,
    /// MIDI 2.0 Universal MIDI Packet words in host byte order (`SPA_CONTROL_UMP`).
    Ump,
}

/// One control read from a PipeWire MIDI buffer: its within-block sample offset, its encoding, and
/// the raw payload bytes. This is the io edge — [`event`](MidiControl::event) does the byte→typed
/// conversion so raw bytes go no deeper (invariant 6).
pub struct MidiControl<'a> {
    /// Sample offset of the event within the block.
    pub offset: u32,
    /// How `bytes` is encoded.
    pub kind: MidiKind,
    /// The raw payload (legacy MIDI bytes or UMP words).
    pub bytes: &'a [u8],
}

impl MidiControl<'_> {
    /// Converts the payload to a typed [`Event`](crate::event::Event), or `None` for messages
    /// the core does not model — including every system message, which is
    /// [`sync`](MidiControl::sync)'s business.
    #[must_use]
    pub fn event(&self) -> Option<crate::event::Event> {
        match self.kind {
            MidiKind::Legacy => parse_midi(self.bytes),
            MidiKind::Ump => parse_ump(self.bytes),
        }
    }

    /// Converts the payload to a typed [`SyncMessage`](crate::transport::SyncMessage), or `None`
    /// when this control is music rather than timing. Exactly one of
    /// [`event`](MidiControl::event) and this can be `Some`.
    #[must_use]
    pub fn sync(&self) -> Option<crate::transport::SyncMessage> {
        match self.kind {
            MidiKind::Legacy => parse_sync(self.bytes),
            MidiKind::Ump => parse_sync_ump(self.bytes),
        }
    }
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Walks a SPA POD sequence of controls, invoking `f` for each MIDI/UMP control. The buffer layout
/// is: an 8-byte pod header (`size`, `type`), an 8-byte sequence body header (`unit`, `pad`), then
/// controls, each an 8-byte header (`offset`, `type`) + an 8-byte value header (`size`, `type`) +
/// `size` value bytes padded up to 8. All integers are host byte order. Bounds-checked, no unsafe.
fn for_each_control(pod: &[u8], mut f: impl FnMut(&MidiControl)) {
    let (Some(body_size), Some(pod_type)) = (read_u32(pod, 0), read_u32(pod, 4)) else {
        return;
    };
    if pod_type != pw::spa::sys::SPA_TYPE_Sequence {
        return;
    }
    let end = (8 + body_size as usize).min(pod.len());
    let mut cursor = 16; // skip pod header (8) + sequence body header (8)
    while cursor + 16 <= end {
        let (Some(offset), Some(ctype), Some(value_size)) = (
            read_u32(pod, cursor),
            read_u32(pod, cursor + 4),
            read_u32(pod, cursor + 8),
        ) else {
            break;
        };
        let value_size = value_size as usize;
        let value_start = cursor + 16;
        if value_start + value_size > end {
            break;
        }
        let kind = if ctype == SPA_CONTROL_MIDI {
            Some(MidiKind::Legacy)
        } else if ctype == SPA_CONTROL_UMP {
            Some(MidiKind::Ump)
        } else {
            None
        };
        if let Some(kind) = kind {
            f(&MidiControl {
                offset,
                kind,
                bytes: &pod[value_start..value_start + value_size],
            });
        }
        cursor = value_start + value_size.next_multiple_of(8);
    }
}

// ---------------------------------------------------------------------------------------------
// MIDI output.
// ---------------------------------------------------------------------------------------------

/// Byte length of a sequence pod's fixed prelude: the 8-byte pod header (`size`, `type`) plus the
/// 8-byte sequence body header (`unit`, `pad`). Controls start here. Mirrors the cursor start in
/// [`for_each_control`].
const SEQUENCE_PRELUDE: usize = 16;

/// The `spa_control_type` a [`MidiKind`] is carried as on the wire.
const fn control_type(kind: MidiKind) -> u32 {
    match kind {
        MidiKind::Legacy => SPA_CONTROL_MIDI,
        MidiKind::Ump => SPA_CONTROL_UMP,
    }
}

/// The sink one output block is written into: appends MIDI controls to a PipeWire buffer.
///
/// Realtime-safe by construction (invariant 11): it writes straight into the mapped buffer the
/// process callback handed over, never allocates, and *refuses* a control that would not fit
/// rather than growing. It is the exact inverse of the reader behind [`Block::midi_in`] — what
/// this writes, that reads back.
///
/// Controls must be pushed in **nondecreasing** sample-offset order: a SPA sequence is ordered,
/// and so is the engine's own output ([`EventSource::events_in`](crate::event::EventSource)
/// emits in nondecreasing tick order, which the scheduler preserves).
pub struct MidiSink<'a> {
    buf: &'a mut [u8],
    cursor: usize,
    kind: MidiKind,
    frames: u32,
    last_offset: u32,
    dropped: u32,
}

impl<'a> MidiSink<'a> {
    /// Writes the sequence headers into `buf` and returns a sink positioned at the first control.
    /// `None` when the buffer cannot even hold the prelude.
    fn new(buf: &'a mut [u8], kind: MidiKind, frames: u32) -> Option<Self> {
        if buf.len() < SEQUENCE_PRELUDE {
            return None;
        }
        // Pod header: body size is patched by `finish` once the controls are known.
        buf[0..4].copy_from_slice(&0_u32.to_ne_bytes());
        buf[4..8].copy_from_slice(&spa::sys::SPA_TYPE_Sequence.to_ne_bytes());
        // Sequence body header: unit and pad, both unused for MIDI.
        buf[8..16].fill(0);
        Some(Self {
            buf,
            cursor: SEQUENCE_PRELUDE,
            kind,
            frames,
            last_offset: 0,
            dropped: 0,
        })
    }

    /// The block length in samples. Event offsets are relative to the block start and must fall
    /// in `0..frames` — this is the engine's cue for how far to advance the transport.
    #[must_use]
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// The encoding this sink writes. Fixed for the lifetime of the stream.
    #[must_use]
    pub fn kind(&self) -> MidiKind {
        self.kind
    }

    /// How many controls were refused in this block — because the buffer filled up, or because
    /// the event has no MIDI representation. Realtime code cannot log; a shell reads this after
    /// the block and reports it off the audio thread.
    #[must_use]
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    /// Appends `event` at `offset` samples into the block, encoded per [`kind`](Self::kind).
    ///
    /// Returns `false` when the event was **not** written: either the buffer is full, or the
    /// event has no wire representation ([`Event::ParamChange`](crate::event::Event::ParamChange)
    /// — see [`encode_midi`]). Both cases bump [`dropped`](Self::dropped).
    pub fn push(&mut self, offset: u32, event: &crate::event::Event) -> bool {
        let mut payload = [0_u8; 4];
        let len = match self.kind {
            // Legacy messages are two or three bytes; UMP is always one 32-bit word.
            MidiKind::Legacy => match encode_midi(event) {
                Some(message) => {
                    let bytes = message.as_bytes();
                    payload[..bytes.len()].copy_from_slice(bytes);
                    bytes.len()
                }
                None => {
                    self.dropped += 1;
                    return false;
                }
            },
            MidiKind::Ump => match encode_ump(event) {
                Some(word) => {
                    payload.copy_from_slice(&word);
                    4
                }
                None => {
                    self.dropped += 1;
                    return false;
                }
            },
        };
        self.push_control(offset, &payload[..len])
    }

    /// Appends a transport-sync message at `offset` samples into the block.
    ///
    /// The timing counterpart of [`push`](MidiSink::push): clock and start/stop ride the same
    /// port as the notes, which is how MIDI has always carried them. Returns `false` when the
    /// buffer is full, bumping [`dropped`](MidiSink::dropped).
    pub fn push_sync(&mut self, offset: u32, message: crate::transport::SyncMessage) -> bool {
        let mut payload = [0_u8; 4];
        let len = match self.kind {
            MidiKind::Legacy => {
                let encoded = encode_sync(message);
                let bytes = encoded.as_bytes();
                payload[..bytes.len()].copy_from_slice(bytes);
                bytes.len()
            }
            MidiKind::Ump => {
                payload.copy_from_slice(&encode_sync_ump(message));
                4
            }
        };
        self.push_control(offset, &payload[..len])
    }

    /// Writes one control: an 8-byte header (`offset`, `type`), an 8-byte value header (`size`,
    /// `type`), then the payload padded up to 8. Host byte order throughout.
    fn push_control(&mut self, offset: u32, payload: &[u8]) -> bool {
        debug_assert!(
            offset >= self.last_offset,
            "a SPA sequence is ordered: pushed offset {offset} after {}",
            self.last_offset
        );
        debug_assert!(
            offset < self.frames || self.frames == 0,
            "offset {offset} lies past the end of a {}-sample block",
            self.frames
        );
        let padded = payload.len().next_multiple_of(8);
        let Some(end) = self.cursor.checked_add(16 + padded) else {
            self.dropped += 1;
            return false;
        };
        if end > self.buf.len() {
            self.dropped += 1;
            return false;
        }
        let at = self.cursor;
        self.buf[at..at + 4].copy_from_slice(&offset.to_ne_bytes());
        self.buf[at + 4..at + 8].copy_from_slice(&control_type(self.kind).to_ne_bytes());
        self.buf[at + 8..at + 12].copy_from_slice(&(payload.len() as u32).to_ne_bytes());
        self.buf[at + 12..at + 16].copy_from_slice(&spa::sys::SPA_TYPE_Bytes.to_ne_bytes());
        self.buf[at + 16..at + 16 + payload.len()].copy_from_slice(payload);
        self.buf[at + 16 + payload.len()..end].fill(0); // padding
        self.cursor = end;
        self.last_offset = offset;
        true
    }

    /// Patches the pod's body size and returns the total byte length to report in the chunk.
    ///
    /// The body size counts everything after the 8-byte pod header — the sequence body header
    /// plus every control — which is exactly what [`for_each_control`] uses to find the end.
    fn finish(self) -> usize {
        let body = self.cursor - 8;
        self.buf[0..4].copy_from_slice(&(body as u32).to_ne_bytes());
        self.cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;

    /// Builds a one-control SPA sequence POD (host byte order) for testing the walker.
    fn sequence(ctype: u32, payload: &[u8]) -> Vec<u8> {
        let padded = payload.len().next_multiple_of(8);
        let body_size = 8 + 16 + padded; // seq body header + control header + padded value
        let mut buf = Vec::new();
        buf.extend_from_slice(&(body_size as u32).to_ne_bytes()); // pod size
        buf.extend_from_slice(&(pw::spa::sys::SPA_TYPE_Sequence).to_ne_bytes()); // pod type
        buf.extend_from_slice(&0u32.to_ne_bytes()); // unit
        buf.extend_from_slice(&0u32.to_ne_bytes()); // pad
        buf.extend_from_slice(&7u32.to_ne_bytes()); // control offset (7 frames)
        buf.extend_from_slice(&ctype.to_ne_bytes()); // control type
        buf.extend_from_slice(&(payload.len() as u32).to_ne_bytes()); // value size
        buf.extend_from_slice(&(pw::spa::sys::SPA_TYPE_Bytes).to_ne_bytes()); // value type
        buf.extend_from_slice(payload);
        buf.resize(buf.len() + (padded - payload.len()), 0);
        buf
    }

    #[test]
    fn walks_a_ump_sequence_into_a_typed_event() {
        let word = 0x2090_3C64_u32.to_ne_bytes(); // note-on ch0 key60 vel100
        let pod = sequence(SPA_CONTROL_UMP, &word);

        let mut events = Vec::new();
        for_each_control(&pod, |control| {
            assert_eq!(control.offset, 7);
            assert_eq!(control.kind, MidiKind::Ump);
            events.push(control.event());
        });
        assert_eq!(
            events,
            vec![Some(Event::NoteOn {
                channel: 0,
                key: 60,
                velocity: 100
            })]
        );
    }

    #[test]
    fn walks_a_legacy_midi_sequence() {
        let pod = sequence(SPA_CONTROL_MIDI, &[0xB0, 7, 127]);
        let mut events = Vec::new();
        for_each_control(&pod, |control| events.push((control.kind, control.event())));
        assert_eq!(
            events,
            vec![(
                MidiKind::Legacy,
                Some(Event::Control {
                    channel: 0,
                    controller: 7,
                    value: 127
                })
            )]
        );
    }

    #[test]
    fn ignores_garbage_and_short_buffers() {
        for_each_control(&[], |_| panic!("no controls in an empty buffer"));
        for_each_control(&[0, 1, 2, 3], |_| {
            panic!("no controls in a truncated buffer")
        });
    }

    /// Drives a sink over a scratch buffer and reads the result back with the input-side walker.
    fn round_trip(kind: MidiKind, events: &[(u32, Event)]) -> Vec<(u32, MidiKind, Option<Event>)> {
        let mut buf = vec![0_u8; 4096];
        let mut sink = MidiSink::new(&mut buf, kind, 512).expect("4096 bytes fits the prelude");
        for (offset, event) in events {
            assert!(sink.push(*offset, event), "scratch buffer is large enough");
        }
        assert_eq!(sink.dropped(), 0);
        let size = sink.finish();

        let mut out = Vec::new();
        for_each_control(&buf[..size], |control| {
            out.push((control.offset, control.kind, control.event()));
        });
        out
    }

    /// The load-bearing test: what [`MidiSink`] writes is exactly what [`for_each_control`]
    /// reads. Output and input are two halves of one wire format — if they drift, this breaks.
    #[test]
    fn what_the_sink_writes_the_walker_reads_back() {
        let events = [
            (
                0,
                Event::NoteOn {
                    channel: 0,
                    key: 60,
                    velocity: 100,
                },
            ),
            (
                7,
                Event::Control {
                    channel: 1,
                    controller: 7,
                    value: 127,
                },
            ),
            (
                128,
                Event::PitchBend {
                    channel: 2,
                    value: -4096,
                },
            ),
            (
                511,
                Event::NoteOff {
                    channel: 0,
                    key: 60,
                    velocity: 0,
                },
            ),
        ];

        for kind in [MidiKind::Ump, MidiKind::Legacy] {
            let read_back = round_trip(kind, &events);
            let expected: Vec<_> = events
                .iter()
                .map(|(offset, event)| (*offset, kind, Some(*event)))
                .collect();
            assert_eq!(read_back, expected, "round trip for {kind:?}");
        }
    }

    #[test]
    fn an_empty_block_is_a_valid_empty_sequence() {
        let mut buf = vec![0_u8; 64];
        let sink = MidiSink::new(&mut buf, MidiKind::Ump, 512).unwrap();
        let size = sink.finish();
        assert_eq!(size, SEQUENCE_PRELUDE);
        // The walker must accept it and find nothing — a silent block is not a malformed one.
        for_each_control(&buf[..size], |_| panic!("no controls were pushed"));
    }

    #[test]
    fn a_full_buffer_refuses_further_controls_instead_of_overrunning() {
        // Prelude (16) + one UMP control (16 header + 8 padded payload) = 40 bytes; 48 leaves
        // room for exactly one.
        let mut buf = vec![0_u8; 48];
        let mut sink = MidiSink::new(&mut buf, MidiKind::Ump, 512).unwrap();
        let note = Event::NoteOn {
            channel: 0,
            key: 60,
            velocity: 100,
        };
        assert!(sink.push(0, &note), "the first control fits");
        assert!(!sink.push(1, &note), "the second does not");
        assert!(!sink.push(2, &note), "and it stays refused");
        assert_eq!(sink.dropped(), 2);

        // What did fit is still a well-formed sequence — a full buffer truncates, never corrupts.
        let size = sink.finish();
        let mut count = 0;
        for_each_control(&buf[..size], |control| {
            assert_eq!(control.event(), Some(note));
            count += 1;
        });
        assert_eq!(count, 1);
    }

    #[test]
    fn events_without_a_wire_representation_are_dropped_not_written() {
        let mut buf = vec![0_u8; 4096];
        let mut sink = MidiSink::new(&mut buf, MidiKind::Ump, 512).unwrap();
        assert!(!sink.push(
            0,
            &Event::ParamChange {
                param: crate::event::ParamId(1),
                value: 0.25,
            }
        ));
        assert_eq!(sink.dropped(), 1);
        let size = sink.finish();
        for_each_control(&buf[..size], |_| {
            panic!("a ParamChange never reaches the wire")
        });
    }

    #[test]
    fn a_buffer_too_small_for_the_prelude_yields_no_sink() {
        let mut buf = [0_u8; SEQUENCE_PRELUDE - 1];
        assert!(MidiSink::new(&mut buf, MidiKind::Ump, 512).is_none());
    }
}
