//! The typed [`Event`] enum, the realtime-safe [`EventSource`] trait and the [`NoteTracker`].
//!
//! Pure logic. This module uses [`crate::time`] (an [`EventSource`] is asked for a tick range)
//! but nothing above it (invariant 1). Raw MIDI bytes never reach here — they become typed
//! events at the io edge (invariant 6).

use serde::{Deserialize, Serialize};

use crate::time::{Tick, TickRange};

/// A typed musical event.
///
/// invariant 6: raw MIDI exists only at the io edge; everywhere inside the core, music is this
/// enum. invariant 13: automation is not a separate system — a [`Event::ParamChange`] rides an
/// ordinary event port like any other event, and a node ignores variants it doesn't understand.
/// MPE / MIDI 2.0 are deliberately deferred, so note events carry a plain channel and key.
///
/// The channel-voice variants cover MIDI 1.0 completely, so nothing a keyboard or sequencer can
/// send is lost at the edge. System messages (clock, transport, SysEx) are *not* here: they are
/// not music, they are synchronisation, and they belong to a transport-sync layer the core does
/// not have yet.
///
/// Serde-serializable so clips persist their events verbatim (invariant 4: still in ticks).
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub enum Event {
    /// Start a note. A `NoteOn` always means *on*: the io edge rewrites the running-status
    /// "note-on, velocity 0" convention into [`Event::NoteOff`] before it reaches the core.
    NoteOn {
        /// MIDI channel, `0..16`.
        channel: u8,
        /// Note number, `0..128`.
        key: u8,
        /// Attack velocity, `0..128`.
        velocity: u8,
    },
    /// Stop a note.
    NoteOff {
        /// MIDI channel, `0..16`.
        channel: u8,
        /// Note number, `0..128`.
        key: u8,
        /// Release velocity, `0..128`.
        velocity: u8,
    },
    /// Pressure applied to one held key after it sounded (polyphonic aftertouch).
    PolyPressure {
        /// MIDI channel, `0..16`.
        channel: u8,
        /// Note number the pressure applies to, `0..128`.
        key: u8,
        /// Pressure, `0..128`.
        pressure: u8,
    },
    /// A continuous-controller change.
    Control {
        /// MIDI channel, `0..16`.
        channel: u8,
        /// Controller number, `0..128`.
        controller: u8,
        /// Controller value, `0..128`.
        value: u8,
    },
    /// A patch/preset selection on a channel.
    ProgramChange {
        /// MIDI channel, `0..16`.
        channel: u8,
        /// Program number, `0..128`.
        program: u8,
    },
    /// Pressure applied to every held key on a channel at once (channel aftertouch).
    ChannelPressure {
        /// MIDI channel, `0..16`.
        channel: u8,
        /// Pressure, `0..128`.
        pressure: u8,
    },
    /// A pitch-bend change, centered at zero (`-8192..=8191`, 14-bit).
    PitchBend {
        /// MIDI channel, `0..16`.
        channel: u8,
        /// Signed bend amount.
        value: i16,
    },
    /// A parameter change destined for a node's event port (e.g. a CLAP plugin parameter).
    ParamChange {
        /// Target parameter, interpreted by the receiving node.
        param: ParamId,
        /// New value, in the parameter's own domain.
        value: f64,
    },
}

/// Identifies a parameter on the node that receives a [`Event::ParamChange`].
///
/// The value matches a CLAP `clap_id`; the receiving node is selected by the event-port
/// connection, so the id only has to be unique within that node.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ParamId(pub u32);

/// The sink an [`EventSource`] emits into.
///
/// The engine owns the concrete sink — on the audio thread it is backed by a preallocated
/// buffer, so `push` never allocates there. Sources only call [`push`](EventSink::push);
/// offline code and tests may supply any sink, including an allocating one.
pub trait EventSink {
    /// Record `event` at musical position `at`.
    fn push(&mut self, at: Tick, event: Event);
}

/// The musical contract: given a tick range, emit the events that fall inside it.
///
/// Clip playback and generative modules are the *same thing* to the engine — both are
/// `EventSource`s that know ticks, not the graph. The engine wraps a source in a node that
/// fetches the block's tick range from the transport and converts each event's tick to a
/// sample offset.
///
/// invariant 5: `events_in` is realtime-safe — no allocation, no locks, and work bounded by
/// the range, never by unbounded user data. invariant 8: a source must **not** close its own
/// hanging notes on stop / locate / loop-wrap; that is the engine's [`NoteTracker`]'s job.
pub trait EventSource {
    /// Emit every event whose position lies in `range`, in nondecreasing tick order.
    fn events_in(&mut self, range: TickRange, sink: &mut dyn EventSink);
}

/// Tracks which notes are sounding so the engine can close them cleanly.
///
/// invariant 8: note-off hygiene lives here, not in sources. On stop, locate or loop-wrap the
/// engine calls [`flush`](NoteTracker::flush) to emit a `NoteOff` for every note still held,
/// preventing stuck notes. Fixed-size (a bit per channel/key) and allocation-free, so it is
/// safe to drive from the audio thread.
#[derive(Clone, Debug)]
pub struct NoteTracker {
    /// One bit per (channel, key): 16 channels × 128 keys.
    sounding: [u128; 16],
}

impl Default for NoteTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl NoteTracker {
    /// A tracker with no notes sounding.
    #[must_use]
    pub const fn new() -> Self {
        Self { sounding: [0; 16] }
    }

    /// Updates the sounding set from an observed event; non-note events are ignored.
    ///
    /// A `NoteOn` marks a note sounding, a `NoteOff` clears it. The velocity-0 note-on
    /// convention is already normalized away at the io edge (invariant 6), so it is not
    /// special-cased here.
    pub fn observe(&mut self, event: &Event) {
        match *event {
            Event::NoteOn { channel, key, .. } => {
                self.sounding[(channel & 0x0F) as usize] |= 1_u128 << (key & 0x7F);
            }
            Event::NoteOff { channel, key, .. } => {
                self.sounding[(channel & 0x0F) as usize] &= !(1_u128 << (key & 0x7F));
            }
            _ => {}
        }
    }

    /// Emits a `NoteOff` (release velocity 0) at `at` for every sounding note, then clears them.
    ///
    /// invariant 8: the single place hanging notes are closed — on stop, locate and loop-wrap.
    /// Work is bounded by the number of sounding notes, never by user data (invariant 5).
    pub fn flush(&mut self, at: Tick, sink: &mut dyn EventSink) {
        for (channel, slot) in self.sounding.iter_mut().enumerate() {
            let mut bits = *slot;
            while bits != 0 {
                let key = bits.trailing_zeros() as u8;
                sink.push(
                    at,
                    Event::NoteOff {
                        channel: channel as u8,
                        key,
                        velocity: 0,
                    },
                );
                bits &= bits - 1; // clear the lowest set bit
            }
            *slot = 0;
        }
    }

    /// True when no notes are sounding.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sounding.iter().all(|&b| b == 0)
    }

    /// How many notes are currently sounding.
    #[must_use]
    pub fn sounding_count(&self) -> u32 {
        self.sounding.iter().map(|b| b.count_ones()).sum()
    }

    /// Whether this exact note is sounding right now.
    ///
    /// The per-note counterpart of [`sounding_count`](Self::sounding_count),
    /// added for callers that gate note-ons on what is already held — a delay
    /// line refusing to double a key it is already playing, say. Without it
    /// such a caller has to mirror this bitmask on its own, and a second copy
    /// of "what is sounding" is exactly the state that drifts apart on the
    /// first missed note-off.
    ///
    /// Channel and key are masked the same way [`observe`](Self::observe)
    /// masks them, so an out-of-range argument answers about the note that
    /// would actually have been recorded rather than panicking.
    #[must_use]
    pub fn is_sounding(&self, channel: u8, key: u8) -> bool {
        self.sounding[(channel & 0x0F) as usize] & (1_u128 << (key & 0x7F)) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An allocating sink — fine off the audio thread (invariant 5 binds sources, not sinks).
    #[derive(Default)]
    struct Collector(Vec<(Tick, Event)>);

    impl EventSink for Collector {
        fn push(&mut self, at: Tick, event: Event) {
            self.0.push((at, event));
        }
    }

    fn note_on(channel: u8, key: u8) -> Event {
        Event::NoteOn {
            channel,
            key,
            velocity: 100,
        }
    }

    #[test]
    fn is_sounding_answers_per_note() {
        let mut tracker = NoteTracker::new();
        tracker.observe(&note_on(0, 60));
        tracker.observe(&note_on(1, 60));

        assert!(tracker.is_sounding(0, 60));
        assert!(tracker.is_sounding(1, 60));
        // Same key, a channel that never played it.
        assert!(!tracker.is_sounding(2, 60));
        // Same channel, a key that never played.
        assert!(!tracker.is_sounding(0, 61));

        tracker.observe(&Event::NoteOff {
            channel: 0,
            key: 60,
            velocity: 0,
        });
        assert!(!tracker.is_sounding(0, 60));
        assert!(tracker.is_sounding(1, 60));
    }

    #[test]
    fn is_sounding_masks_like_observe_does() {
        // Out of range is not a panic but the note that would have been
        // recorded: channel 17 is channel 1, key 200 is key 72.
        let mut tracker = NoteTracker::new();
        tracker.observe(&note_on(17, 200));
        assert!(tracker.is_sounding(17, 200));
        assert!(tracker.is_sounding(1, 72));
    }

    #[test]
    fn flush_leaves_nothing_sounding() {
        let mut tracker = NoteTracker::new();
        tracker.observe(&note_on(0, 60));
        let mut out = Collector::default();
        tracker.flush(Tick::new(0), &mut out);
        assert!(!tracker.is_sounding(0, 60));
        assert!(tracker.is_empty());
    }

    #[test]
    fn flush_on_stop_closes_all_sounding_notes() {
        let mut tracker = NoteTracker::new();
        tracker.observe(&note_on(0, 60));
        tracker.observe(&note_on(0, 64));
        tracker.observe(&note_on(1, 40));
        assert_eq!(tracker.sounding_count(), 3);

        let mut out = Collector::default();
        tracker.flush(Tick::new(1000), &mut out);

        assert_eq!(
            out.0,
            vec![
                (
                    Tick::new(1000),
                    Event::NoteOff {
                        channel: 0,
                        key: 60,
                        velocity: 0
                    }
                ),
                (
                    Tick::new(1000),
                    Event::NoteOff {
                        channel: 0,
                        key: 64,
                        velocity: 0
                    }
                ),
                (
                    Tick::new(1000),
                    Event::NoteOff {
                        channel: 1,
                        key: 40,
                        velocity: 0
                    }
                ),
            ]
        );
        assert!(tracker.is_empty());
    }

    #[test]
    fn flush_on_locate_ignores_already_released_notes() {
        let mut tracker = NoteTracker::new();
        tracker.observe(&note_on(2, 50));
        tracker.observe(&note_on(2, 55));
        tracker.observe(&Event::NoteOff {
            channel: 2,
            key: 50,
            velocity: 0,
        });

        let mut out = Collector::default();
        tracker.flush(Tick::new(480), &mut out); // the locate point
        assert_eq!(
            out.0,
            vec![(
                Tick::new(480),
                Event::NoteOff {
                    channel: 2,
                    key: 55,
                    velocity: 0
                }
            )]
        );
        assert!(tracker.is_empty());
    }

    #[test]
    fn flush_on_loop_wrap_then_resumes_empty() {
        let mut tracker = NoteTracker::new();
        tracker.observe(&note_on(0, 36));

        let mut out = Collector::default();
        tracker.flush(Tick::new(3840), &mut out); // the loop-end tick
        assert_eq!(out.0.len(), 1);
        assert!(tracker.is_empty());

        // After the wrap a fresh note is tracked independently of the flushed one.
        tracker.observe(&note_on(0, 38));
        assert_eq!(tracker.sounding_count(), 1);
    }

    #[test]
    fn non_note_events_do_not_affect_tracking() {
        let mut tracker = NoteTracker::new();
        tracker.observe(&Event::Control {
            channel: 0,
            controller: 7,
            value: 100,
        });
        tracker.observe(&Event::PitchBend {
            channel: 0,
            value: -2048,
        });
        tracker.observe(&Event::ParamChange {
            param: ParamId(3),
            value: 0.5,
        });
        assert!(tracker.is_empty());
    }

    #[test]
    fn event_source_emits_only_within_range() {
        struct OneShot;
        impl EventSource for OneShot {
            fn events_in(&mut self, range: TickRange, sink: &mut dyn EventSink) {
                let hit = Tick::new(100);
                if range.contains(hit) {
                    sink.push(
                        hit,
                        Event::NoteOn {
                            channel: 0,
                            key: 72,
                            velocity: 80,
                        },
                    );
                }
            }
        }

        let mut src = OneShot;
        let mut inside = Collector::default();
        src.events_in(TickRange::new(Tick::new(0), Tick::new(200)), &mut inside);
        assert_eq!(inside.0.len(), 1);

        let mut outside = Collector::default();
        src.events_in(TickRange::new(Tick::new(200), Tick::new(400)), &mut outside);
        assert!(outside.0.is_empty());
    }
}
