//! Lanes and clips — the timeline content the transport plays, plus a [`ClipPlayer`] that
//! adapts them to the [`EventSource`] contract.
//!
//! Pure data + logic; uses [`crate::time`] and [`crate::event`] but nothing above them
//! (invariant 1). "Tracks" do not exist here — a track is a shell-level coupling of a lane and
//! a node chain (invariant 13). This is the *minimal* arrangement; clip looping, stretcher IDs
//! and the full document shape arrive in phase 6.

use std::sync::Arc;

use crate::event::{Event, EventSink, EventSource};
use crate::time::{Tick, TickRange};

/// A bounded piece of musical content: events at tick positions, relative to the clip's own
/// start (`0` = clip start), played once over its `length`.
#[derive(Clone, Debug, Default)]
pub struct Clip {
    length: Tick,
    /// Events sorted by tick. Positions at or past `length` are retained but never played.
    events: Vec<(Tick, Event)>,
}

impl Clip {
    /// Creates a clip of `length` from `events`, sorting them by tick (stable, so events at the
    /// same tick keep their given order). Positions outside `[0, length)` are kept but the
    /// player clips them away.
    #[must_use]
    pub fn new(length: Tick, mut events: Vec<(Tick, Event)>) -> Self {
        events.sort_by_key(|(tick, _)| tick.get());
        Self { length, events }
    }

    /// The clip's length in ticks.
    #[must_use]
    pub fn length(&self) -> Tick {
        self.length
    }

    /// The clip's events, sorted by tick.
    #[must_use]
    pub fn events(&self) -> &[(Tick, Event)] {
        &self.events
    }
}

/// A clip placed at a tick on the lane's timeline.
#[derive(Clone, Debug)]
pub struct Placement {
    start: Tick,
    clip: Clip,
}

impl Placement {
    /// Places `clip` so its local tick `0` sits at timeline tick `start`.
    #[must_use]
    pub fn new(start: Tick, clip: Clip) -> Self {
        Self { start, clip }
    }

    /// Timeline tick where the clip begins.
    #[must_use]
    pub fn start(&self) -> Tick {
        self.start
    }

    /// Timeline tick one past the clip's content (`start + clip.length`).
    #[must_use]
    pub fn end(&self) -> Tick {
        Tick::new(self.start.get() + self.clip.length.get())
    }

    /// The placed clip.
    #[must_use]
    pub fn clip(&self) -> &Clip {
        &self.clip
    }
}

/// A sequence of clip placements on a timeline.
///
/// Placements are sorted by start. This minimal phase assumes they do not overlap; the player
/// relies on that to find the relevant placements with a binary search.
#[derive(Clone, Debug, Default)]
pub struct Lane {
    placements: Vec<Placement>,
}

impl Lane {
    /// Builds a lane, sorting `placements` by start tick.
    #[must_use]
    pub fn new(mut placements: Vec<Placement>) -> Self {
        placements.sort_by_key(|p| p.start.get());
        Self { placements }
    }

    /// The lane's placements, sorted by start tick.
    #[must_use]
    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }
}

/// Plays a [`Lane`] as an [`EventSource`]: given a timeline tick range, emits the events of the
/// overlapping clips, translated to timeline ticks.
///
/// Holds the lane as an immutable `Arc` snapshot (invariant 2); an edit builds a new lane and
/// swaps it via [`set_lane`](ClipPlayer::set_lane). `events_in` is allocation-free and does work
/// bounded by the placements and events actually overlapping the range (invariant 5).
#[derive(Clone, Debug)]
pub struct ClipPlayer {
    lane: Arc<Lane>,
}

impl ClipPlayer {
    /// Creates a player over `lane`.
    #[must_use]
    pub fn new(lane: Arc<Lane>) -> Self {
        Self { lane }
    }

    /// Swaps the lane snapshot. The previous `Arc` is dropped by the caller — do this off the
    /// audio thread, or route the old snapshot through the garbage chute (phase 3).
    pub fn set_lane(&mut self, lane: Arc<Lane>) {
        self.lane = lane;
    }

    /// The current lane snapshot.
    #[must_use]
    pub fn lane(&self) -> &Arc<Lane> {
        &self.lane
    }
}

impl EventSource for ClipPlayer {
    fn events_in(&mut self, range: TickRange, sink: &mut dyn EventSink) {
        if range.is_empty() {
            return;
        }
        let placements = &self.lane.placements;
        // First placement that could overlap: the first whose end is past the range start.
        // `end` is sorted alongside `start` because placements do not overlap.
        let first = placements.partition_point(|p| p.end().get() <= range.start.get());
        for placement in &placements[first..] {
            let base = placement.start.get();
            if base >= range.end.get() {
                break; // placements are start-sorted: nothing further overlaps
            }
            // Intersect the range with this clip's local span `[0, length)`.
            let local_lo = range.start.get().saturating_sub(base);
            let local_hi = range
                .end
                .get()
                .saturating_sub(base)
                .min(placement.clip.length.get());
            if local_lo >= local_hi {
                continue;
            }
            let events = &placement.clip.events;
            let from = events.partition_point(|(tick, _)| tick.get() < local_lo);
            for (tick, event) in &events[from..] {
                if tick.get() >= local_hi {
                    break;
                }
                sink.push(Tick::new(base + tick.get()), *event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;

    fn on(key: u8) -> Event {
        Event::NoteOn {
            channel: 0,
            key,
            velocity: 100,
        }
    }

    #[derive(Default)]
    struct Rec(Vec<(Tick, Event)>);
    impl EventSink for Rec {
        fn push(&mut self, at: Tick, event: Event) {
            self.0.push((at, event));
        }
    }

    fn play(player: &mut ClipPlayer, lo: u64, hi: u64) -> Vec<(Tick, Event)> {
        let mut rec = Rec::default();
        player.events_in(TickRange::new(Tick::new(lo), Tick::new(hi)), &mut rec);
        rec.0
    }

    #[test]
    fn emits_events_within_a_single_clip_window() {
        let clip = Clip::new(
            Tick::new(100),
            vec![(Tick::new(0), on(60)), (Tick::new(50), on(64))],
        );
        let lane = Arc::new(Lane::new(vec![Placement::new(Tick::new(0), clip)]));
        let mut player = ClipPlayer::new(lane);

        // Half-open: tick 0 is included at the low edge, tick 50 excluded at the high edge.
        assert_eq!(play(&mut player, 0, 50), vec![(Tick::new(0), on(60))]);
        assert_eq!(play(&mut player, 50, 100), vec![(Tick::new(50), on(64))]);
        assert_eq!(
            play(&mut player, 0, 100),
            vec![(Tick::new(0), on(60)), (Tick::new(50), on(64))]
        );
    }

    #[test]
    fn translates_events_by_placement_start() {
        let clip = Clip::new(Tick::new(20), vec![(Tick::new(5), on(72))]);
        let lane = Arc::new(Lane::new(vec![Placement::new(Tick::new(1000), clip)]));
        let mut player = ClipPlayer::new(lane);

        // The event sits at local tick 5, i.e. timeline tick 1005.
        assert_eq!(
            play(&mut player, 1000, 1020),
            vec![(Tick::new(1005), on(72))]
        );
        // A window before the placement yields nothing.
        assert!(play(&mut player, 0, 1000).is_empty());
    }

    #[test]
    fn finds_the_right_placement_among_many() {
        let lane = Arc::new(Lane::new(vec![
            Placement::new(
                Tick::new(0),
                Clip::new(Tick::new(10), vec![(Tick::new(0), on(1))]),
            ),
            Placement::new(
                Tick::new(100),
                Clip::new(Tick::new(10), vec![(Tick::new(0), on(2))]),
            ),
            Placement::new(
                Tick::new(200),
                Clip::new(Tick::new(10), vec![(Tick::new(0), on(3))]),
            ),
        ]));
        let mut player = ClipPlayer::new(lane);
        assert_eq!(play(&mut player, 100, 110), vec![(Tick::new(100), on(2))]);
        assert!(play(&mut player, 50, 100).is_empty());
    }

    #[test]
    fn events_at_or_past_length_are_not_played() {
        let clip = Clip::new(
            Tick::new(40),
            vec![(Tick::new(40), on(60)), (Tick::new(10), on(61))],
        );
        let lane = Arc::new(Lane::new(vec![Placement::new(Tick::new(0), clip)]));
        let mut player = ClipPlayer::new(lane);
        // Tick 40 == length is out of `[0, 40)`, so only tick 10 plays over the full window.
        assert_eq!(play(&mut player, 0, 1000), vec![(Tick::new(10), on(61))]);
    }
}
