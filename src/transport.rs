//! Transport — play/stop/locate/loop, per-block scheduling, and the tempo-map snapshot swap.
//!
//! This is the first module with concurrency, so it is where the threading rules bite. Commands
//! travel control → audio over a lock-free `rtrb` queue; the playhead and run-state travel
//! audio → control via atomics with a single writer, the audio-side [`Transport::process_block`]
//! (invariants 2 & 3). Shared data (the tempo map) is an immutable `Arc` snapshot swapped
//! atomically; the retired snapshot is handed back over a return queue rather than dropped on
//! the audio thread (invariants 9 & 11 — a miniature of phase 3's garbage chute).
//!
//! The transport only *schedules*: it emits [`BlockStep`]s. It never pulls events or tracks
//! notes itself — a driver (the graph, in phase 3) interprets the steps, asking the
//! [`EventSource`](crate::event::EventSource)s for each range and flushing the
//! [`NoteTracker`](crate::event::NoteTracker) on each [`BlockStep::Flush`] (invariant 8).

use core::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rtrb::{Consumer, Producer, RingBuffer};

use crate::time::{SampleOffset, SamplePos, TempoMap, Tick, TickRange};

/// Command queue depth. One block drains at most this many commands.
const COMMAND_CAPACITY: usize = 256;
/// Retire queue depth. Kept `>= COMMAND_CAPACITY` so a full block of tempo swaps can always hand
/// its retired snapshots back without dropping any on the audio thread (invariant 11).
const RETIRE_CAPACITY: usize = COMMAND_CAPACITY;

/// A control-thread command to the transport, delivered over the lock-free queue (invariant 2).
enum Command {
    Play,
    Stop,
    Locate(Tick),
    SetLoop(Option<TickRange>),
    SetTempoMap(Arc<TempoMap>),
}

/// What the transport asks the driver to do for one contiguous slice of a processing block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockStep {
    /// Render `ticks`, beginning `at` this sample offset within the block.
    Play {
        /// Sample offset within the block where this slice begins.
        at: SampleOffset,
        /// The musical range to render.
        ticks: TickRange,
    },
    /// Close every hanging note at `tick` — on a loop wrap, stop, or locate (invariant 8).
    Flush {
        /// The tick at which notes are flushed.
        tick: Tick,
    },
}

/// The command queue was full, so the command was dropped — the audio thread is not draining
/// (typically an xrun). Retry after the next block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct QueueFull;

impl fmt::Display for QueueFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("transport command queue is full")
    }
}

impl std::error::Error for QueueFull {}

/// Atomics written only by the audio thread and read by the control thread (invariant 3).
#[derive(Debug)]
struct Shared {
    /// Current playhead, in ticks.
    playhead: AtomicU64,
    /// Whether the transport is playing.
    playing: AtomicBool,
}

/// The audio-thread half of the transport. Driven once per block by [`process_block`], which is
/// the sole writer of the shared atomics.
///
/// [`process_block`]: Transport::process_block
pub struct Transport {
    commands: Consumer<Command>,
    retired: Producer<Arc<TempoMap>>,
    shared: Arc<Shared>,
    tempo: Arc<TempoMap>,
    /// Playback position as the audio-domain source of truth; ticks are derived from it.
    pos: SamplePos,
    playing: bool,
    loop_range: Option<TickRange>,
}

/// The control-thread half of the transport: sends commands and reads back the playhead.
pub struct TransportHandle {
    commands: Producer<Command>,
    retired: Consumer<Arc<TempoMap>>,
    shared: Arc<Shared>,
}

impl Transport {
    /// Creates a transport starting stopped at tick 0 with the given tempo map, returning the
    /// audio-thread [`Transport`] and the control-thread [`TransportHandle`] that drives it.
    #[must_use]
    pub fn new(tempo: Arc<TempoMap>) -> (Self, TransportHandle) {
        let (command_tx, command_rx) = RingBuffer::new(COMMAND_CAPACITY);
        let (retire_tx, retire_rx) = RingBuffer::new(RETIRE_CAPACITY);
        let shared = Arc::new(Shared {
            playhead: AtomicU64::new(0),
            playing: AtomicBool::new(false),
        });
        let transport = Self {
            commands: command_rx,
            retired: retire_tx,
            shared: Arc::clone(&shared),
            tempo,
            pos: SamplePos::new(0),
            playing: false,
            loop_range: None,
        };
        let handle = TransportHandle {
            commands: command_tx,
            retired: retire_rx,
            shared,
        };
        (transport, handle)
    }

    /// Advances one processing block of `block_size` samples, emitting a [`BlockStep`] for each
    /// slice. Drains pending commands first, then schedules playback (splitting the block at
    /// loop wraps), then publishes the playhead. The single writer of the shared atomics.
    pub fn process_block<F: FnMut(BlockStep)>(&mut self, block_size: u32, mut emit: F) {
        self.drain_commands(&mut emit);
        if self.playing && block_size > 0 {
            self.schedule(block_size, &mut emit);
        }
        self.publish();
    }

    /// Current playback position in ticks (derived from the sample-domain truth).
    #[must_use]
    pub fn playhead(&self) -> Tick {
        self.tempo.sample_to_tick(self.pos)
    }

    /// Whether the transport is currently playing.
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    fn drain_commands<F: FnMut(BlockStep)>(&mut self, emit: &mut F) {
        while let Ok(command) = self.commands.pop() {
            match command {
                Command::Play => self.playing = true,
                Command::Stop => {
                    emit(BlockStep::Flush {
                        tick: self.playhead(),
                    });
                    self.playing = false;
                }
                Command::Locate(tick) => {
                    emit(BlockStep::Flush {
                        tick: self.playhead(),
                    });
                    self.pos = self.tempo.tick_to_sample(tick);
                }
                Command::SetLoop(range) => self.loop_range = range,
                Command::SetTempoMap(map) => {
                    let old = core::mem::replace(&mut self.tempo, map);
                    // invariant 11: hand the retired snapshot back instead of dropping it here.
                    // The queue is sized so this never fails under normal draining.
                    let _ = self.retired.push(old);
                }
            }
        }
    }

    fn schedule<F: FnMut(BlockStep)>(&mut self, block_size: u32, emit: &mut F) {
        let mut remaining = u64::from(block_size);
        let mut offset: u32 = 0;
        let mut start_tick = self.playhead();

        while remaining > 0 {
            let block_end = self.pos.get() + remaining;

            // Does an active loop end inside this slice? If so, the slice stops at the loop end.
            let wrap = self.loop_range.and_then(|loop_range| {
                if loop_range.is_empty() {
                    return None;
                }
                let end_sample = self.tempo.tick_to_sample(loop_range.end).get();
                (self.pos.get() < end_sample && end_sample <= block_end)
                    .then_some((loop_range, end_sample))
            });

            let (span_end, end_tick) = match wrap {
                // Pin the boundary to the exact loop-end tick (avoids sample round-trip drift).
                Some((loop_range, end_sample)) => (end_sample, loop_range.end),
                None => (
                    block_end,
                    self.tempo.sample_to_tick(SamplePos::new(block_end)),
                ),
            };

            let ticks = TickRange::new(start_tick, end_tick);
            if !ticks.is_empty() {
                emit(BlockStep::Play {
                    at: SampleOffset::new(offset),
                    ticks,
                });
            }

            let span_samples = span_end - self.pos.get();
            offset += span_samples as u32;
            remaining -= span_samples;

            match wrap {
                Some((loop_range, _)) => {
                    emit(BlockStep::Flush {
                        tick: loop_range.end,
                    });
                    self.pos = self.tempo.tick_to_sample(loop_range.start);
                    start_tick = loop_range.start; // exact restart at the loop start
                }
                None => self.pos = SamplePos::new(span_end),
            }
        }
    }

    fn publish(&self) {
        self.shared
            .playhead
            .store(self.playhead().get(), Ordering::Relaxed);
        self.shared.playing.store(self.playing, Ordering::Relaxed);
    }
}

impl TransportHandle {
    /// Starts playback from the current position.
    ///
    /// # Errors
    /// [`QueueFull`] if the command queue is saturated.
    pub fn play(&mut self) -> Result<(), QueueFull> {
        self.send(Command::Play)
    }

    /// Stops playback; the transport flushes hanging notes at the current position.
    ///
    /// # Errors
    /// [`QueueFull`] if the command queue is saturated.
    pub fn stop(&mut self) -> Result<(), QueueFull> {
        self.send(Command::Stop)
    }

    /// Moves the playhead to `tick`; hanging notes are flushed at the old position first.
    ///
    /// # Errors
    /// [`QueueFull`] if the command queue is saturated.
    pub fn locate(&mut self, tick: Tick) -> Result<(), QueueFull> {
        self.send(Command::Locate(tick))
    }

    /// Sets or clears the loop region. An empty range is ignored by the scheduler.
    ///
    /// # Errors
    /// [`QueueFull`] if the command queue is saturated.
    pub fn set_loop(&mut self, range: Option<TickRange>) -> Result<(), QueueFull> {
        debug_assert!(
            range.is_none_or(|r| !r.is_empty()),
            "loop range must be non-empty"
        );
        self.send(Command::SetLoop(range))
    }

    /// Swaps in a new tempo map as a snapshot (invariant 9). The old map is returned to this
    /// handle's garbage queue; call [`collect_garbage`](TransportHandle::collect_garbage) to
    /// reclaim it off the audio thread.
    ///
    /// # Errors
    /// [`QueueFull`] if the command queue is saturated.
    pub fn set_tempo_map(&mut self, map: Arc<TempoMap>) -> Result<(), QueueFull> {
        self.send(Command::SetTempoMap(map))
    }

    /// The last published playhead position, in ticks.
    #[must_use]
    pub fn playhead(&self) -> Tick {
        Tick::new(self.shared.playhead.load(Ordering::Relaxed))
    }

    /// Whether the transport last reported that it is playing.
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    /// Drops any tempo-map snapshots the audio thread has retired. Call periodically from the
    /// control thread; this is where the retired `Arc`s are actually deallocated (invariant 11).
    pub fn collect_garbage(&mut self) {
        while let Ok(retired) = self.retired.pop() {
            drop(retired);
        }
    }

    fn send(&mut self, command: Command) -> Result<(), QueueFull> {
        self.commands.push(command).map_err(|_| QueueFull)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrange::{Clip, ClipPlayer, Lane, Placement};
    use crate::event::{Event, EventSink, EventSource, NoteTracker};

    // 120 BPM @ 48 kHz → 25 samples/tick. A 1200-sample block is exactly 48 ticks.
    const SR: u32 = 48_000;
    const BLOCK: u32 = 1200;

    fn tempo(bpm: f64) -> Arc<TempoMap> {
        Arc::new(TempoMap::constant(SR, bpm).unwrap())
    }

    fn on(key: u8) -> Event {
        Event::NoteOn {
            channel: 0,
            key,
            velocity: 100,
        }
    }

    fn off(key: u8) -> Event {
        Event::NoteOff {
            channel: 0,
            key,
            velocity: 100,
        }
    }

    fn off0(key: u8) -> Event {
        Event::NoteOff {
            channel: 0,
            key,
            velocity: 0,
        }
    }

    fn t(tick: u64) -> Tick {
        Tick::new(tick)
    }

    /// Collected `(tick, event)` output, doubling as an [`EventSink`].
    #[derive(Default)]
    struct Rec(Vec<(Tick, Event)>);
    impl EventSink for Rec {
        fn push(&mut self, at: Tick, event: Event) {
            self.0.push((at, event));
        }
    }

    /// A sink that both records output and feeds the note tracker (for `Play` steps).
    struct Tracking<'a> {
        tracker: &'a mut NoteTracker,
        out: &'a mut Rec,
    }
    impl EventSink for Tracking<'_> {
        fn push(&mut self, at: Tick, event: Event) {
            self.tracker.observe(&event);
            self.out.0.push((at, event));
        }
    }

    /// Drives one block: interprets the transport's steps against a clip player + note tracker,
    /// exactly as the phase-3 graph will. Returns this block's emitted events.
    fn drive(
        transport: &mut Transport,
        player: &mut ClipPlayer,
        tracker: &mut NoteTracker,
        block: u32,
    ) -> Vec<(Tick, Event)> {
        let mut out = Rec::default();
        transport.process_block(block, |step| match step {
            BlockStep::Play { ticks, .. } => {
                let mut sink = Tracking {
                    tracker: &mut *tracker,
                    out: &mut out,
                };
                player.events_in(ticks, &mut sink);
            }
            BlockStep::Flush { tick } => tracker.flush(tick, &mut out),
        });
        out.0
    }

    fn player(placements: Vec<Placement>) -> ClipPlayer {
        ClipPlayer::new(Arc::new(Lane::new(placements)))
    }

    #[test]
    fn play_advances_the_playhead_and_emits_clip_events() {
        let (mut tr, mut h) = Transport::new(tempo(120.0));
        let mut cp = player(vec![Placement::new(
            t(0),
            Clip::new(t(96), vec![(t(0), on(60)), (t(24), off(60))]),
        )]);
        let mut nt = NoteTracker::new();

        h.play().unwrap();
        let block1 = drive(&mut tr, &mut cp, &mut nt, BLOCK);

        assert_eq!(block1, vec![(t(0), on(60)), (t(24), off(60))]);
        assert_eq!(tr.playhead(), t(48)); // 1200 samples / 25 = 48 ticks
        assert_eq!(h.playhead(), t(48)); // published to the control side via the atomic
        assert!(h.is_playing());
    }

    #[test]
    fn loop_wrap_splits_the_block_and_flushes_hanging_notes() {
        let (mut tr, mut h) = Transport::new(tempo(120.0));
        // A note turns on at tick 40 and is never explicitly released — the wrap must close it.
        let mut cp = player(vec![Placement::new(
            t(0),
            Clip::new(
                t(96),
                vec![(t(0), on(60)), (t(24), off(60)), (t(40), on(67))],
            ),
        )]);
        let mut nt = NoteTracker::new();

        h.play().unwrap();
        h.set_loop(Some(TickRange::new(t(0), t(60)))).unwrap(); // loop end at 60 ticks = 1500 smp

        // Block 1 covers ticks [0,48): note 67 is left sounding.
        let block1 = drive(&mut tr, &mut cp, &mut nt, BLOCK);
        assert_eq!(
            block1,
            vec![(t(0), on(60)), (t(24), off(60)), (t(40), on(67))]
        );
        assert_eq!(nt.sounding_count(), 1);

        // Block 2 wraps mid-block at tick 60: play [48,60) (no events), flush 67, then [0,36).
        let block2 = drive(&mut tr, &mut cp, &mut nt, BLOCK);
        assert_eq!(
            block2,
            vec![
                (t(60), off0(67)), // note-off flush at the loop end (invariant 8)
                (t(0), on(60)),    // resumed from the loop start
                (t(24), off(60)),
            ]
        );
        assert!(nt.is_empty());
        // After the wrap: 2400 - 1500 = 900 samples past the loop start = 36 ticks.
        assert_eq!(tr.playhead(), t(36));
    }

    #[test]
    fn locate_flushes_hanging_notes_then_jumps() {
        let (mut tr, mut h) = Transport::new(tempo(120.0));
        let mut cp = player(vec![Placement::new(
            t(0),
            Clip::new(t(10_000), vec![(t(0), on(60))]), // on, never released within a block
        )]);
        let mut nt = NoteTracker::new();

        h.play().unwrap();
        let _ = drive(&mut tr, &mut cp, &mut nt, BLOCK); // note 60 now sounding, playhead at 48
        assert_eq!(nt.sounding_count(), 1);

        h.locate(t(0)).unwrap();
        let after = drive(&mut tr, &mut cp, &mut nt, BLOCK);
        // Flush at the old position (tick 48) comes first, then playback resumes from tick 0.
        assert_eq!(after, vec![(t(48), off0(60)), (t(0), on(60))]);
        assert_eq!(tr.playhead(), t(48));
    }

    #[test]
    fn stop_flushes_hanging_notes_and_halts() {
        let (mut tr, mut h) = Transport::new(tempo(120.0));
        let mut cp = player(vec![Placement::new(
            t(0),
            Clip::new(t(10_000), vec![(t(0), on(60))]),
        )]);
        let mut nt = NoteTracker::new();

        h.play().unwrap();
        let _ = drive(&mut tr, &mut cp, &mut nt, BLOCK);
        assert_eq!(nt.sounding_count(), 1);

        h.stop().unwrap();
        let after = drive(&mut tr, &mut cp, &mut nt, BLOCK);
        assert_eq!(after, vec![(t(48), off0(60))]); // flushed at the stop position
        assert!(nt.is_empty());
        assert!(!tr.is_playing());
        assert!(!h.is_playing());

        // While stopped, further blocks emit nothing and the playhead does not move.
        let idle = drive(&mut tr, &mut cp, &mut nt, BLOCK);
        assert!(idle.is_empty());
        assert_eq!(tr.playhead(), t(48));
    }

    #[test]
    fn tempo_map_swap_takes_effect_and_can_be_collected() {
        let (mut tr, mut h) = Transport::new(tempo(120.0));
        let mut cp = player(vec![]);
        let mut nt = NoteTracker::new();

        h.play().unwrap();
        let _ = drive(&mut tr, &mut cp, &mut nt, BLOCK);
        assert_eq!(tr.playhead(), t(48)); // 120 BPM: 1200/25 = 48

        // Halve the tempo to 60 BPM (50 samples/tick). The swap happens on the audio thread.
        h.set_tempo_map(tempo(60.0)).unwrap();
        let _ = drive(&mut tr, &mut cp, &mut nt, BLOCK);
        // Position advanced another 1200 samples (2400 total) → 2400/50 = 48 ticks at 60 BPM.
        assert_eq!(tr.playhead(), t(48));

        // The retired 120 BPM map is waiting in the garbage queue; reclaim it off-thread.
        h.collect_garbage();
    }

    #[test]
    fn fake_clock_drives_play_locate_and_loop_wrap() {
        // The phase-2 done-when: one fake-clock run through play, locate and a loop wrap, with
        // every emitted event — note-off flushes included — asserted exactly.
        let (mut tr, mut h) = Transport::new(tempo(120.0));
        let mut cp = player(vec![Placement::new(
            t(0),
            Clip::new(t(96), vec![(t(0), on(60)), (t(40), on(67))]), // 67 never released
        )]);
        let mut nt = NoteTracker::new();

        h.play().unwrap();
        h.set_loop(Some(TickRange::new(t(0), t(96)))).unwrap(); // loop end 96 ticks = 2400 smp

        // Block 1 — plain play over [0,48): both notes on, nothing released.
        assert_eq!(
            drive(&mut tr, &mut cp, &mut nt, BLOCK),
            vec![(t(0), on(60)), (t(40), on(67))]
        );

        // Locate back to 0 while two notes hang: flush both at tick 48, then replay from 0.
        h.locate(t(0)).unwrap();
        assert_eq!(
            drive(&mut tr, &mut cp, &mut nt, BLOCK),
            vec![
                (t(48), off0(60)),
                (t(48), off0(67)),
                (t(0), on(60)),
                (t(40), on(67)),
            ]
        );

        // Block 3 — wrap exactly at the block/loop end (tick 96): flush both, restart at 0.
        assert_eq!(
            drive(&mut tr, &mut cp, &mut nt, BLOCK),
            vec![(t(96), off0(60)), (t(96), off0(67))]
        );
        assert!(nt.is_empty());
        assert_eq!(tr.playhead(), t(0)); // sitting back at the loop start
    }
}
