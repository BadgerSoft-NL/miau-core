//! `Tick` (960 PPQ), `SamplePos`, the stepwise tempo map and musical↔sample conversion.
//!
//! Pure logic: this module imports nothing from the rest of the crate (invariant 1). Musical
//! time lives here as ticks; sample positions are produced only by [`TempoMap`] at scheduling
//! time (invariant 4).

use core::fmt;

use serde::{Deserialize, Serialize};

/// A musical position or duration in ticks.
///
/// invariant 4: musical data lives in ticks at 960 PPQ ([`Tick::PPQ`]); sample positions appear
/// only at scheduling, via the [`TempoMap`]. Never use a bare integer for musical time.
///
/// Serializes transparently as its raw tick count — persistence keeps musical time in ticks
/// (invariant 4), so a document is human- and LLM-readable (design log note 10).
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, Serialize, Deserialize,
)]
pub struct Tick(u64);

impl Tick {
    /// Pulses (ticks) per quarter note. The whole engine is fixed at this resolution.
    pub const PPQ: u64 = 960;

    /// Wraps a raw tick count.
    #[must_use]
    pub const fn new(ticks: u64) -> Self {
        Self(ticks)
    }

    /// The raw tick count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// An absolute position on the audio timeline, in samples.
///
/// invariant 4: produced only at scheduling time, by converting a [`Tick`] through the
/// [`TempoMap`]. Musical logic never reasons in samples.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct SamplePos(u64);

impl SamplePos {
    /// Wraps a raw sample position.
    #[must_use]
    pub const fn new(samples: u64) -> Self {
        Self(samples)
    }

    /// The raw sample position.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// An offset within a single processing block, in samples (`0..block_size`).
///
/// invariant 4: this is where sample-domain numbers first appear — an event scheduled at a
/// [`Tick`] becomes a `(SampleOffset, Event)` pair inside the block that contains it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct SampleOffset(u32);

impl SampleOffset {
    /// Wraps a raw within-block sample offset.
    #[must_use]
    pub const fn new(offset: u32) -> Self {
        Self(offset)
    }

    /// The raw within-block sample offset.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A half-open range of ticks `[start, end)`.
///
/// One block's worth of musical time: the transport produces one per block and an
/// [`EventSource`](crate::event::EventSource) is asked to emit the events that fall inside it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TickRange {
    /// Inclusive start.
    pub start: Tick,
    /// Exclusive end.
    pub end: Tick,
}

impl TickRange {
    /// A half-open range `[start, end)`.
    #[must_use]
    pub const fn new(start: Tick, end: Tick) -> Self {
        Self { start, end }
    }

    /// True when the range covers no ticks.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start.0 >= self.end.0
    }

    /// Number of ticks the range spans.
    #[must_use]
    pub const fn len(self) -> u64 {
        self.end.0.saturating_sub(self.start.0)
    }

    /// True when `tick` falls in `[start, end)`.
    #[must_use]
    pub const fn contains(self, tick: Tick) -> bool {
        self.start.0 <= tick.0 && tick.0 < self.end.0
    }
}

/// One entry of a stepwise tempo map: a tempo that holds from `start` until the next entry.
///
/// This is the *persisted* form of the tempo map (invariant 9's input): a document stores the
/// segments and rebuilds the compiled [`TempoMap`] on load for its device sample rate.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct TempoSegment {
    /// Tick at which this tempo takes effect. The first segment must start at `Tick(0)`.
    pub start: Tick,
    /// Tempo in beats (quarter notes) per minute. Must be finite and positive.
    pub bpm: f64,
}

/// A compiled segment: the input tempo plus its precomputed cumulative sample position.
#[derive(Clone, Copy, Debug)]
struct CompiledSegment {
    start_tick: u64,
    /// Sample position at `start_tick`, accumulated across earlier segments (invariant 4).
    start_sample: u64,
    samples_per_tick: f64,
}

/// Why a [`TempoMap`] could not be built.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TempoMapError {
    /// No segments were supplied.
    Empty,
    /// The first segment did not start at `Tick(0)`.
    FirstSegmentNotAtZero,
    /// Segment start ticks were not strictly increasing.
    NonMonotonicStarts,
    /// A tempo was zero, negative, or not finite.
    InvalidTempo,
}

impl fmt::Display for TempoMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::Empty => "tempo map has no segments",
            Self::FirstSegmentNotAtZero => "first tempo segment must start at tick 0",
            Self::NonMonotonicStarts => "tempo segment starts must strictly increase",
            Self::InvalidTempo => "tempo must be finite and positive",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for TempoMapError {}

/// A stepwise tempo map with precomputed cumulative sample positions.
///
/// invariant 9: immutable during playback. A tempo edit builds a *new* map that the engine
/// swaps in as an `Arc` snapshot; a `TempoMap` is never mutated in place. The sample rate is
/// baked in at construction — a device sample-rate change rebuilds the map.
///
/// Conversions are exact at segment boundaries (a boundary tick maps to its stored sample
/// position and back); within a segment they round to the nearest sample. All conversions are
/// read-only and allocation-free, so they are safe to call on the audio thread.
#[derive(Clone, Debug)]
pub struct TempoMap {
    sample_rate: u32,
    segments: Vec<CompiledSegment>,
}

impl TempoMap {
    /// Builds a tempo map for `sample_rate` from stepwise `segments`.
    ///
    /// The first segment must start at `Tick(0)`, starts must strictly increase, and every
    /// tempo must be finite and positive; otherwise a [`TempoMapError`] is returned.
    ///
    /// # Errors
    /// See [`TempoMapError`] for the rejection cases.
    pub fn new(sample_rate: u32, segments: &[TempoSegment]) -> Result<Self, TempoMapError> {
        let first = segments.first().ok_or(TempoMapError::Empty)?;
        if first.start.get() != 0 {
            return Err(TempoMapError::FirstSegmentNotAtZero);
        }

        let sr = f64::from(sample_rate);
        let mut compiled = Vec::with_capacity(segments.len());
        let mut acc_samples = 0.0_f64;
        let mut prev: Option<(u64, f64)> = None; // (start_tick, samples_per_tick)

        for seg in segments {
            if !seg.bpm.is_finite() || seg.bpm <= 0.0 {
                return Err(TempoMapError::InvalidTempo);
            }
            let start = seg.start.get();
            if let Some((prev_start, prev_spt)) = prev {
                if start <= prev_start {
                    return Err(TempoMapError::NonMonotonicStarts);
                }
                acc_samples += (start - prev_start) as f64 * prev_spt;
            }
            let samples_per_tick = sr * 60.0 / (seg.bpm * Tick::PPQ as f64);
            compiled.push(CompiledSegment {
                start_tick: start,
                start_sample: acc_samples.round() as u64,
                samples_per_tick,
            });
            prev = Some((start, samples_per_tick));
        }

        Ok(Self {
            sample_rate,
            segments: compiled,
        })
    }

    /// Builds a single-tempo map. Convenience over [`TempoMap::new`].
    ///
    /// # Errors
    /// [`TempoMapError::InvalidTempo`] if `bpm` is not finite and positive.
    pub fn constant(sample_rate: u32, bpm: f64) -> Result<Self, TempoMapError> {
        Self::new(
            sample_rate,
            &[TempoSegment {
                start: Tick::new(0),
                bpm,
            }],
        )
    }

    /// The sample rate this map was compiled for.
    #[must_use]
    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Index of the segment whose tempo governs `tick` (the last one starting at or before it).
    fn segment_at_tick(&self, tick: u64) -> &CompiledSegment {
        let idx = self
            .segments
            .partition_point(|s| s.start_tick <= tick)
            .saturating_sub(1);
        &self.segments[idx]
    }

    /// Converts a musical position to an absolute sample position (invariant 4).
    #[must_use]
    pub fn tick_to_sample(&self, tick: Tick) -> SamplePos {
        let t = tick.get();
        let seg = self.segment_at_tick(t);
        let offset = (t - seg.start_tick) as f64 * seg.samples_per_tick;
        SamplePos::new(seg.start_sample + offset.round() as u64)
    }

    /// Converts an absolute sample position back to a musical position (invariant 4).
    #[must_use]
    pub fn sample_to_tick(&self, sample: SamplePos) -> Tick {
        let s = sample.get();
        let idx = self
            .segments
            .partition_point(|seg| seg.start_sample <= s)
            .saturating_sub(1);
        let seg = &self.segments[idx];
        let offset = (s - seg.start_sample) as f64 / seg.samples_per_tick;
        Tick::new(seg.start_tick + offset.round() as u64)
    }

    /// Iterates the tempo segments overlapping `range`, clipped to it.
    ///
    /// Each [`TempoSpan`] carries a single-tempo sub-range of the block, so a scheduler can
    /// convert event ticks piecewise without re-scanning the map. Allocation-free.
    #[must_use]
    pub fn segments_in(&self, range: TickRange) -> SegmentIter<'_> {
        SegmentIter {
            map: self,
            cursor: range.start,
            end: range.end,
        }
    }
}

/// One tempo segment clipped to a block, produced by [`TempoMap::segments_in`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TempoSpan {
    /// The portion of the block covered by a single tempo.
    pub ticks: TickRange,
    /// Sample position at `ticks.start`.
    pub start_sample: SamplePos,
    /// Samples per tick for this tempo — the block-local conversion factor.
    pub samples_per_tick: f64,
}

/// Iterator over the tempo segments overlapping a block. See [`TempoMap::segments_in`].
#[derive(Clone, Debug)]
pub struct SegmentIter<'a> {
    map: &'a TempoMap,
    cursor: Tick,
    end: Tick,
}

impl Iterator for SegmentIter<'_> {
    type Item = TempoSpan;

    fn next(&mut self) -> Option<Self::Item> {
        let cursor = self.cursor.get();
        if cursor >= self.end.get() {
            return None;
        }
        let idx = self
            .map
            .segments
            .partition_point(|s| s.start_tick <= cursor)
            .saturating_sub(1);
        let seg = &self.map.segments[idx];
        // This tempo runs until the next segment's start, or the block end, whichever is first.
        let next_start = self
            .map
            .segments
            .get(idx + 1)
            .map_or(u64::MAX, |n| n.start_tick);
        let span_end = next_start.min(self.end.get());
        let span = TempoSpan {
            ticks: TickRange::new(self.cursor, Tick::new(span_end)),
            start_sample: self.map.tick_to_sample(self.cursor),
            samples_per_tick: seg.samples_per_tick,
        };
        self.cursor = Tick::new(span_end);
        Some(span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: u64, bpm: f64) -> TempoSegment {
        TempoSegment {
            start: Tick::new(start),
            bpm,
        }
    }

    #[test]
    fn constant_tempo_quarter_note() {
        // 120 BPM @ 48 kHz → 25 samples per tick; a quarter note (960 ticks) = 24000 samples.
        let map = TempoMap::constant(48_000, 120.0).unwrap();
        assert_eq!(map.tick_to_sample(Tick::new(0)), SamplePos::new(0));
        assert_eq!(map.tick_to_sample(Tick::new(960)), SamplePos::new(24_000));
        assert_eq!(map.sample_to_tick(SamplePos::new(24_000)), Tick::new(960));
    }

    #[test]
    fn tempo_change_mid_block() {
        // 120 BPM until tick 960, then 60 BPM (50 samples/tick @ 48 kHz).
        let map = TempoMap::new(48_000, &[seg(0, 120.0), seg(960, 60.0)]).unwrap();
        // Boundary sample position = cumulative length of the first segment.
        assert_eq!(map.tick_to_sample(Tick::new(960)), SamplePos::new(24_000));
        // An eighth note (480 ticks) into the 60 BPM segment: 480 * 50 = 24000 more samples.
        assert_eq!(map.tick_to_sample(Tick::new(1440)), SamplePos::new(48_000));
        // And a position still inside the first segment stays on the 25 samples/tick rate.
        assert_eq!(map.tick_to_sample(Tick::new(480)), SamplePos::new(12_000));
    }

    #[test]
    fn round_trip_at_segment_boundaries() {
        let map =
            TempoMap::new(44_100, &[seg(0, 90.0), seg(1920, 140.0), seg(5000, 75.0)]).unwrap();
        for &boundary in &[0_u64, 1920, 5000] {
            let t = Tick::new(boundary);
            let s = map.tick_to_sample(t);
            assert_eq!(
                map.sample_to_tick(s),
                t,
                "boundary {boundary} must round-trip exactly"
            );
        }
    }

    #[test]
    fn segments_in_splits_a_block_at_the_change() {
        let map = TempoMap::new(48_000, &[seg(0, 120.0), seg(960, 60.0)]).unwrap();
        let spans: Vec<_> = map
            .segments_in(TickRange::new(Tick::new(720), Tick::new(1440)))
            .collect();
        assert_eq!(spans.len(), 2);
        assert_eq!(
            spans[0].ticks,
            TickRange::new(Tick::new(720), Tick::new(960))
        );
        assert_eq!(spans[0].start_sample, SamplePos::new(18_000)); // 720 * 25
        assert!((spans[0].samples_per_tick - 25.0).abs() < 1e-9);
        assert_eq!(
            spans[1].ticks,
            TickRange::new(Tick::new(960), Tick::new(1440))
        );
        assert_eq!(spans[1].start_sample, SamplePos::new(24_000));
        assert!((spans[1].samples_per_tick - 50.0).abs() < 1e-9);
    }

    #[test]
    fn segments_in_single_segment_and_empty_block() {
        let map = TempoMap::constant(48_000, 120.0).unwrap();
        let one: Vec<_> = map
            .segments_in(TickRange::new(Tick::new(0), Tick::new(480)))
            .collect();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].ticks, TickRange::new(Tick::new(0), Tick::new(480)));

        let none = map
            .segments_in(TickRange::new(Tick::new(100), Tick::new(100)))
            .count();
        assert_eq!(none, 0);
    }

    #[test]
    fn rejects_invalid_maps() {
        assert_eq!(
            TempoMap::new(48_000, &[]).unwrap_err(),
            TempoMapError::Empty
        );
        assert_eq!(
            TempoMap::new(48_000, &[seg(10, 120.0)]).unwrap_err(),
            TempoMapError::FirstSegmentNotAtZero
        );
        assert_eq!(
            TempoMap::new(48_000, &[seg(0, 120.0), seg(0, 60.0)]).unwrap_err(),
            TempoMapError::NonMonotonicStarts
        );
        assert_eq!(
            TempoMap::new(48_000, &[seg(0, 0.0)]).unwrap_err(),
            TempoMapError::InvalidTempo
        );
        assert_eq!(
            TempoMap::new(48_000, &[seg(0, f64::NAN)]).unwrap_err(),
            TempoMapError::InvalidTempo
        );
    }

    #[test]
    fn tick_range_helpers() {
        let r = TickRange::new(Tick::new(10), Tick::new(20));
        assert_eq!(r.len(), 10);
        assert!(!r.is_empty());
        assert!(r.contains(Tick::new(10)));
        assert!(!r.contains(Tick::new(20)));
        assert!(TickRange::new(Tick::new(5), Tick::new(5)).is_empty());
    }
}
