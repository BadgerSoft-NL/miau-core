//! Lanes and clips — the timeline content the transport plays, plus a [`ClipPlayer`] that
//! adapts them to the [`EventSource`] contract.
//!
//! Pure data + logic; uses [`crate::time`] and [`crate::event`] but nothing above them
//! (invariant 1). "Tracks" do not exist here — a track is a shell-level coupling of a lane and
//! a node chain (invariant 13). Clips loop, carry an optional time-stretch reference (invariant
//! 7, resolved through [`crate::dsp`]) and an optional sample reference; this is the shape
//! `persist/` serializes (design log note 10).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::{Event, EventSink, EventSource};
use crate::time::{Tick, TickRange};

/// A reference to a time-stretch implementation and its opaque settings.
///
/// `id` selects a stretcher registered in [`crate::dsp`] (invariant 7 — unknown ids fall back
/// with a warning, they never crash). `settings` is arbitrary JSON the core carries but never
/// interprets; [`crate::dsp::StretcherRegistry::resolve`] hands it to the stretcher's factory.
/// Storing it as [`Value`] keeps a document readable
/// (design log note 10) without `arrange/` having to understand any stretcher's parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stretch {
    /// Registered stretcher id.
    pub id: String,
    /// Opaque, stretcher-defined settings, passed through untouched.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub settings: Value,
}

/// A content-addressed reference to an audio sample: a relative path plus a BLAKE3 hash.
///
/// The hash drives relinking after the file moves and lets a shell verify an embedded copy
/// (design log note 10 §5). The bytes may live in the container under `samples/` or stay
/// external; the reference is the same either way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleRef {
    /// Path relative to the document, e.g. `samples/kick.wav`.
    pub path: String,
    /// BLAKE3 hash of the referenced bytes.
    pub hash: [u8; 32],
}

/// A bounded piece of musical content: events at tick positions, relative to the clip's own
/// start (`0` = clip start).
///
/// Over its `length` a clip plays once; when `looping` is set the `[0, length)` content tiles
/// across the placement's span (see [`Placement`]). A clip may also reference an audio sample
/// and a time-stretch to apply to it — both carried for persistence and honored by later
/// phases, ignored by the event-only [`ClipPlayer`] today.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    length: Tick,
    #[serde(default)]
    looping: bool,
    /// Events sorted by tick. Positions at or past `length` are retained but never played.
    events: Vec<(Tick, Event)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stretch: Option<Stretch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sample: Option<SampleRef>,
}

impl Clip {
    /// Creates a one-shot clip of `length` from `events`, sorting them by tick (stable, so events
    /// at the same tick keep their given order). Positions outside `[0, length)` are kept but the
    /// player clips them away.
    #[must_use]
    pub fn new(length: Tick, mut events: Vec<(Tick, Event)>) -> Self {
        events.sort_by_key(|(tick, _)| tick.get());
        Self {
            length,
            events,
            ..Self::default()
        }
    }

    /// Sets whether the clip loops (its `[0, length)` content tiles across the placement span).
    #[must_use]
    pub fn with_looping(mut self, looping: bool) -> Self {
        self.looping = looping;
        self
    }

    /// Attaches a time-stretch reference (invariant 7), carried through persistence.
    #[must_use]
    pub fn with_stretch(mut self, stretch: Stretch) -> Self {
        self.stretch = Some(stretch);
        self
    }

    /// Attaches a sample reference, carried through persistence.
    #[must_use]
    pub fn with_sample(mut self, sample: SampleRef) -> Self {
        self.sample = Some(sample);
        self
    }

    /// The clip's length in ticks.
    #[must_use]
    pub fn length(&self) -> Tick {
        self.length
    }

    /// Whether the clip loops across its placement span.
    #[must_use]
    pub fn is_looping(&self) -> bool {
        self.looping
    }

    /// The clip's events, sorted by tick.
    #[must_use]
    pub fn events(&self) -> &[(Tick, Event)] {
        &self.events
    }

    /// The clip's time-stretch reference, if any.
    #[must_use]
    pub fn stretch(&self) -> Option<&Stretch> {
        self.stretch.as_ref()
    }

    /// The clip's sample reference, if any.
    #[must_use]
    pub fn sample(&self) -> Option<&SampleRef> {
        self.sample.as_ref()
    }
}

/// A clip placed at a tick on the lane's timeline, occupying a `span` of ticks.
///
/// The span defaults to the clip's length (a one-shot placement). A longer span on a looping
/// clip repeats the content to fill it; on a non-looping clip the tail past the content is
/// silence. Events only ever come from the clip's own `[0, length)` window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    start: Tick,
    span: Tick,
    clip: Clip,
}

impl Placement {
    /// Places `clip` so its local tick `0` sits at timeline tick `start`, spanning the clip's
    /// own length (one shot).
    #[must_use]
    pub fn new(start: Tick, clip: Clip) -> Self {
        let span = clip.length;
        Self { start, span, clip }
    }

    /// Places `clip` at `start` occupying `span` ticks of the timeline — longer than the clip to
    /// loop or pad it, shorter to truncate it.
    #[must_use]
    pub fn with_span(start: Tick, span: Tick, clip: Clip) -> Self {
        Self { start, span, clip }
    }

    /// Timeline tick where the placement begins.
    #[must_use]
    pub fn start(&self) -> Tick {
        self.start
    }

    /// Timeline tick one past the placement (`start + span`).
    #[must_use]
    pub fn end(&self) -> Tick {
        Tick::new(self.start.get() + self.span.get())
    }

    /// The placement's span in ticks.
    #[must_use]
    pub fn span(&self) -> Tick {
        self.span
    }

    /// The placed clip.
    #[must_use]
    pub fn clip(&self) -> &Clip {
        &self.clip
    }
}

/// A sequence of clip placements on a timeline.
///
/// Placements are sorted by start and assumed not to overlap; the player relies on that to find
/// the relevant placements with a binary search.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
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
            emit_placement(placement, base, range, sink);
        }
    }
}

/// Emits the events of one placement overlapping `range`, translated to timeline ticks.
///
/// A one-shot placement resolves to a single content window; a looping clip tiles its
/// `[0, length)` content across the placement span, one iteration at a time. Work is bounded by
/// the loop iterations in the block (never by user data), so this stays realtime-safe
/// (invariant 5) as long as the block is bounded.
fn emit_placement(placement: &Placement, base: u64, range: TickRange, sink: &mut dyn EventSink) {
    let clip = &placement.clip;
    let length = clip.length.get();
    // The timeline window this placement can sound over: its span, clipped to the query range.
    let span_end = base + placement.span.get();
    let lo = range.start.get().max(base);
    let hi = range.end.get().min(span_end);
    if lo >= hi {
        return;
    }
    if !clip.looping || length == 0 {
        // One shot: events come from `[0, min(length, span))` at their given timeline positions.
        emit_iteration(clip, base, length, lo, hi, sink);
        return;
    }
    // Looping: walk the loop iterations that overlap `[lo, hi)`.
    let mut iter_start = base + (lo - base) / length * length;
    while iter_start < hi {
        // The last iteration may be cut short by the placement span.
        let iter_len = length.min(span_end - iter_start);
        emit_iteration(clip, iter_start, iter_len, lo, hi, sink);
        iter_start += length;
    }
}

/// Emits the clip events falling in the timeline window `[lo, hi)` for a single content
/// iteration whose local tick `0` sits at `iter_start` and whose content spans `iter_len` ticks.
fn emit_iteration(
    clip: &Clip,
    iter_start: u64,
    iter_len: u64,
    lo: u64,
    hi: u64,
    sink: &mut dyn EventSink,
) {
    let local_lo = lo.saturating_sub(iter_start);
    let local_hi = hi.saturating_sub(iter_start).min(iter_len);
    if local_lo >= local_hi {
        return;
    }
    let from = clip
        .events
        .partition_point(|(tick, _)| tick.get() < local_lo);
    for (tick, event) in &clip.events[from..] {
        if tick.get() >= local_hi {
            break;
        }
        sink.push(Tick::new(iter_start + tick.get()), *event);
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

    #[test]
    fn looping_clip_tiles_its_content_across_the_span() {
        // A 10-tick clip with one event at local tick 2, looped across a 35-tick placement.
        let clip = Clip::new(Tick::new(10), vec![(Tick::new(2), on(60))]).with_looping(true);
        let lane = Arc::new(Lane::new(vec![Placement::with_span(
            Tick::new(100),
            Tick::new(35),
            clip,
        )]));
        let mut player = ClipPlayer::new(lane);
        // Iterations start at 100, 110, 120, 130; the event lands at +2 in each whole one. The
        // fourth iteration is cut at 135 but local tick 2 (== 132) still fits.
        assert_eq!(
            play(&mut player, 0, 1000),
            vec![
                (Tick::new(102), on(60)),
                (Tick::new(112), on(60)),
                (Tick::new(122), on(60)),
                (Tick::new(132), on(60)),
            ]
        );
        // A sub-window only reports the iterations it overlaps, still block-bounded.
        assert_eq!(play(&mut player, 115, 125), vec![(Tick::new(122), on(60))]);
    }

    #[test]
    fn looping_last_iteration_is_cut_by_the_span() {
        // Event at local tick 7; the final partial iteration (130..135) ends before tick 7 hits.
        let clip = Clip::new(Tick::new(10), vec![(Tick::new(7), on(64))]).with_looping(true);
        let lane = Arc::new(Lane::new(vec![Placement::with_span(
            Tick::new(100),
            Tick::new(35),
            clip,
        )]));
        let mut player = ClipPlayer::new(lane);
        assert_eq!(
            play(&mut player, 0, 1000),
            vec![
                (Tick::new(107), on(64)),
                (Tick::new(117), on(64)),
                (Tick::new(127), on(64)),
                // 137 would be the fourth, but the span ends at 135 — dropped.
            ]
        );
    }

    #[test]
    fn non_looping_span_longer_than_content_pads_silence() {
        let clip = Clip::new(Tick::new(10), vec![(Tick::new(3), on(60))]);
        let lane = Arc::new(Lane::new(vec![Placement::with_span(
            Tick::new(0),
            Tick::new(50),
            clip,
        )]));
        let mut player = ClipPlayer::new(lane);
        // Content plays once at tick 3; the rest of the 50-tick span is silent.
        assert_eq!(play(&mut player, 0, 50), vec![(Tick::new(3), on(60))]);
    }
}
