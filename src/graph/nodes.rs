//! A handful of utility nodes: a test-tone [`Sine`], a [`Gain`], a [`Capture`] sink and the
//! [`EventSourceNode`] that adapts an [`EventSource`] into the graph (invariant: clip playback
//! and generative modules are the same thing to the engine — both enter here).

use std::f64::consts::TAU;

use super::{Node, NodeIO, PortCounts, ProcessContext};
use crate::event::{Event, EventSink, EventSource};
use crate::time::{SampleOffset, TempoMap, Tick};

/// A pure-tone oscillator, for tests and beeps. Deterministic: sample `n` is
/// `sin(2π · freq · n / sample_rate)`, so a render is byte-exactly reproducible.
#[derive(Clone, Debug)]
pub struct Sine {
    freq: f64,
    frame: u64,
}

impl Sine {
    /// A sine at `freq` Hz, starting at phase 0.
    #[must_use]
    pub fn new(freq: f64) -> Self {
        Self { freq, frame: 0 }
    }
}

impl Node for Sine {
    fn ports(&self) -> PortCounts {
        PortCounts {
            audio_out: 1,
            ..PortCounts::default()
        }
    }

    fn process(&mut self, ctx: &ProcessContext, io: &mut NodeIO) {
        let rate = f64::from(ctx.sample_rate);
        let out = io.audio_out(0);
        for (i, sample) in out.iter_mut().enumerate() {
            let n = self.frame + i as u64;
            *sample = (TAU * self.freq * n as f64 / rate).sin() as f32;
        }
        self.frame += ctx.frames as u64;
    }
}

/// A scalar gain applied to one audio channel.
#[derive(Clone, Debug)]
pub struct Gain {
    gain: f32,
}

impl Gain {
    /// A gain node with linear multiplier `gain`.
    #[must_use]
    pub fn new(gain: f32) -> Self {
        Self { gain }
    }

    /// Sets the linear gain. Node-local state, not a topology change — no recommit needed.
    pub fn set_gain(&mut self, gain: f32) {
        self.gain = gain;
    }
}

impl Node for Gain {
    fn ports(&self) -> PortCounts {
        PortCounts {
            audio_in: 1,
            audio_out: 1,
            ..PortCounts::default()
        }
    }

    fn process(&mut self, _ctx: &ProcessContext, io: &mut NodeIO) {
        let (input, output) = io.audio_in_out(0, 0);
        for (out, sample) in output.iter_mut().zip(input) {
            *out = *sample * self.gain;
        }
    }
}

/// A sink that appends its input to a growing buffer, for offline render tests.
///
/// This node allocates as it grows, so it is a test/offline utility, not a realtime node.
#[derive(Clone, Debug, Default)]
pub struct Capture {
    captured: Vec<f32>,
}

impl Capture {
    /// A capture sink with an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything captured so far.
    #[must_use]
    pub fn captured(&self) -> &[f32] {
        &self.captured
    }

    /// Discards the captured buffer.
    pub fn clear(&mut self) {
        self.captured.clear();
    }
}

impl Node for Capture {
    fn ports(&self) -> PortCounts {
        PortCounts {
            audio_in: 1,
            ..PortCounts::default()
        }
    }

    fn process(&mut self, _ctx: &ProcessContext, io: &mut NodeIO) {
        self.captured.extend_from_slice(io.audio_in(0));
    }
}

/// Adapts an [`EventSource`] into the graph: each block it asks the source for the block's tick
/// range and emits `(SampleOffset, Event)` on its single event output, converting ticks to
/// sample offsets through the context's tempo map.
pub struct EventSourceNode {
    source: Box<dyn EventSource + Send>,
}

impl EventSourceNode {
    /// Wraps `source` (e.g. a `ClipPlayer` or a generative module) as a node.
    #[must_use]
    pub fn new<S: EventSource + Send + 'static>(source: S) -> Self {
        Self {
            source: Box::new(source),
        }
    }
}

impl Node for EventSourceNode {
    fn ports(&self) -> PortCounts {
        PortCounts {
            event_out: 1,
            ..PortCounts::default()
        }
    }

    fn process(&mut self, ctx: &ProcessContext, io: &mut NodeIO) {
        let frames = io.frames();
        let mut sink = OffsetSink {
            out: io.events_out(0),
            tempo: ctx.tempo,
            block_start: ctx.block_start.get(),
            frames,
        };
        self.source.events_in(ctx.ticks, &mut sink);
    }
}

/// Converts each event's tick to a within-block sample offset and drops events outside the block.
struct OffsetSink<'a> {
    out: &'a mut Vec<(SampleOffset, Event)>,
    tempo: Option<&'a TempoMap>,
    block_start: u64,
    frames: usize,
}

impl EventSink for OffsetSink<'_> {
    fn push(&mut self, at: Tick, event: Event) {
        let offset = match self.tempo {
            Some(tempo) => tempo
                .tick_to_sample(at)
                .get()
                .saturating_sub(self.block_start),
            None => 0,
        };
        if (offset as usize) < self.frames {
            self.out.push((SampleOffset::new(offset as u32), event));
        }
    }
}
