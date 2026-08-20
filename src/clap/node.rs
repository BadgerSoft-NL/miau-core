//! The audio-thread face of a CLAP plugin — the [`ClapNode`] that lives in the graph arena. A
//! missing plugin is stood in for by the generic [`Placeholder`](crate::graph::Placeholder),
//! which lives in `graph/` so `persist/` can build one without depending on `clap/` (invariant 1).
//!
//! A [`ClapNode`] owns the plugin's `Send` audio processor (activation and the `!Send`
//! [`PluginInstance`](super::ClapPlugin) stay on the main thread). Each block it translates the
//! incoming `(SampleOffset, Event)` stream into CLAP events, runs the plugin, copies the plugin's
//! audio output into the node's audio ports, and translates the plugin's output events (e.g. a GUI
//! knob turn) back into [`Event::ParamChange`]s (the core has no parameter system separate from the
//! event stream). All the CLAP-side buffers are preallocated at construction so `process` never
//! allocates (invariant 11); the plugin's own internal allocations are outside our control and
//! accepted (in-process plugins, see `LLM.md`).

use clack_host::events::event_types::{MidiEvent, NoteOffEvent, NoteOnEvent, ParamValueEvent};
use clack_host::events::io::{EventBuffer, InputEvents, OutputEvents};
use clack_host::events::spaces::CoreEventSpace;
use clack_host::events::{Match, Pckn};
// Brings the `header()` accessor (from clack's `Event` trait) into scope without colliding with our
// own `Event` type.
use clack_host::prelude::Event as _;
use clack_host::prelude::{
    AudioPortBuffer, AudioPortBufferType, AudioPorts, InputAudioBuffers, PluginAudioProcessor,
};
use clack_host::utils::{ClapId, Cookie};

use super::host::MiauHost;
use crate::event::{Event, ParamId};
use crate::graph::{Node, NodeIO, PortCounts, ProcessContext};
use crate::time::SampleOffset;

/// Upper bound on the audio output channels a `ClapNode` exposes, matching the graph's per-node
/// port cap. A plugin reporting more is clamped (and only the first channels are wired).
const MAX_AUDIO_OUT: usize = 32;
/// Preallocated event-buffer capacity, in standard events, for one block each way.
const EVENT_CAPACITY: usize = 4096;

/// A CLAP plugin as a graph node: the realtime face of the two-face split.
///
/// One event input port carries notes and parameter changes into the plugin; the audio output
/// ports expose the plugin's main audio output (one graph port per channel); one event output port
/// carries parameter changes the plugin reports back out. Instruments have no audio input, so the
/// node exposes none.
pub struct ClapNode {
    processor: PluginAudioProcessor<MiauHost>,
    audio_out_channels: usize,
    max_block: usize,
    in_events: EventBuffer,
    out_events: EventBuffer,
    out_ports: AudioPorts,
    /// Channel-major plugin output scratch: channel `c` occupies `[c * max_block ..][.. max_block]`.
    out_scratch: Vec<f32>,
}

impl ClapNode {
    /// Wraps an activated plugin's audio `processor`. `audio_out_channels` is the channel count of
    /// the plugin's main audio output (0 for a MIDI effect); `max_block` is the graph's block size.
    #[must_use]
    pub(super) fn new(
        processor: PluginAudioProcessor<MiauHost>,
        audio_out_channels: usize,
        max_block: usize,
    ) -> Self {
        let channels = audio_out_channels.min(MAX_AUDIO_OUT);
        Self {
            processor,
            audio_out_channels: channels,
            max_block,
            in_events: EventBuffer::with_capacity(EVENT_CAPACITY),
            out_events: EventBuffer::with_capacity(EVENT_CAPACITY),
            // A synth has one audio output port; `with_capacity` preallocates so the per-block
            // `with_output_buffers` call below never reallocates.
            out_ports: AudioPorts::with_capacity(channels.max(1), 1),
            out_scratch: vec![0.0; channels * max_block],
        }
    }
}

impl Node for ClapNode {
    fn ports(&self) -> PortCounts {
        PortCounts {
            audio_in: 0,
            audio_out: self.audio_out_channels,
            event_in: 1,
            event_out: 1,
        }
    }

    fn process(&mut self, ctx: &ProcessContext, io: &mut NodeIO) {
        let frames = io.frames();
        let channels = self.audio_out_channels;
        let block = self.max_block;

        // 1. Translate our input events into the plugin's CLAP input buffer (already in
        //    nondecreasing sample-offset order, which CLAP requires).
        self.in_events.clear();
        for &(offset, event) in io.events_in(0) {
            translate_in(&mut self.in_events, offset.get(), event);
        }
        self.out_events.clear();

        // 2. Run the plugin for this block, writing into the channel-major scratch. Zero the used
        //    region first so a plugin that only overwrites active voices still yields clean silence.
        self.out_scratch[..channels * block].fill(0.0);
        let ran = {
            let input_events = InputEvents::from_buffer(&self.in_events);
            let mut output_events = OutputEvents::from_buffer(&mut self.out_events);
            let input_audio = InputAudioBuffers::empty();
            let mut output_audio = self.out_ports.with_output_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(
                    self.out_scratch[..channels * block]
                        .chunks_mut(block)
                        .map(|chan| &mut chan[..frames]),
                ),
            }]);
            match self.processor.ensure_processing_started() {
                Ok(started) => started
                    .process(
                        &input_audio,
                        &mut output_audio,
                        &input_events,
                        &mut output_events,
                        Some(ctx.block_start.get()),
                        None,
                    )
                    .is_ok(),
                Err(_) => false,
            }
        };
        // On a processing failure leave the node's outputs as the executor's zeroed silence.
        if !ran {
            return;
        }

        // 3. Copy the plugin's audio output into the node's audio ports (one channel per port).
        for chan in 0..channels {
            let src = &self.out_scratch[chan * block..chan * block + frames];
            io.audio_out(chan).copy_from_slice(src);
        }

        // 4. Translate the plugin's output parameter events back into the event stream (invariant
        //    13: a `ParamChange` rides an ordinary event port). Other output event kinds are
        //    ignored — nodes ignore variants they don't model.
        for idx in 0..self.out_events.len() {
            let Some(event) = self.out_events.get(idx) else {
                continue;
            };
            if let Some(CoreEventSpace::ParamValue(pv)) = event.as_core_event() {
                if let Some(id) = pv.param_id() {
                    let at = pv.header().time().min(frames.saturating_sub(1) as u32);
                    io.events_out(0).push((
                        SampleOffset::new(at),
                        Event::ParamChange {
                            param: ParamId(id.get()),
                            value: pv.value(),
                        },
                    ));
                }
            }
        }
    }
}

/// Translates one core [`Event`] into the equivalent CLAP event and pushes it at sample `time`.
///
/// Notes become CLAP note events (the port-0, all-note-ids mapping a plain MIDI keyboard implies);
/// a [`Event::ParamChange`] becomes a `param_value` event; CC and pitch-bend become raw MIDI 1.0
/// events, since CLAP has no dedicated controller event. Velocities are scaled to CLAP's `0.0..=1.0`.
pub(super) fn translate_in(buf: &mut EventBuffer, time: u32, event: Event) {
    match event {
        Event::NoteOn {
            channel,
            key,
            velocity,
        } => buf.push(&NoteOnEvent::new(
            time,
            Pckn::new(0u16, u16::from(channel), u16::from(key), Match::All),
            f64::from(velocity) / 127.0,
        )),
        Event::NoteOff {
            channel,
            key,
            velocity,
        } => buf.push(&NoteOffEvent::new(
            time,
            Pckn::new(0u16, u16::from(channel), u16::from(key), Match::All),
            f64::from(velocity) / 127.0,
        )),
        // Everything else that has a wire form goes as a raw MIDI 1.0 event; CLAP has no
        // dedicated controller event. The bytes come from the io encoder rather than being
        // rebuilt here, so the two can never drift apart (invariant 1 allows `clap/` to use
        // `io/`). Two-byte messages are zero-padded, which is what CLAP's fixed [u8; 3] wants.
        Event::PolyPressure { .. }
        | Event::Control { .. }
        | Event::ProgramChange { .. }
        | Event::ChannelPressure { .. }
        | Event::PitchBend { .. } => {
            if let Some(message) = crate::io::encode_midi(&event) {
                let mut bytes = [0_u8; 3];
                let encoded = message.as_bytes();
                bytes[..encoded.len()].copy_from_slice(encoded);
                buf.push(&MidiEvent::new(time, 0, bytes));
            }
        }
        Event::ParamChange { param, value } => {
            // `ClapId` can represent every `u32` except `u32::MAX`; skip that one rather than panic.
            if let Some(id) = ClapId::from_raw(param.0) {
                buf.push(&ParamValueEvent::new(
                    time,
                    id,
                    Pckn::match_all(),
                    value,
                    Cookie::empty(),
                ));
            }
        }
    }
}
