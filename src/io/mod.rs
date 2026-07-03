//! The PipeWire edge: the engine is *pulled* from PipeWire's process callback — the core has no
//! audio thread of its own. This module exposes ports and converts at the edge; hardware routing
//! stays external (qpwgraph). Raw MIDI becomes typed events here and nowhere deeper (invariant 6,
//! see [`parse_midi`]/[`parse_ump`]).
//!
//! Currently covered: mono audio output ([`run_audio_output`]) and MIDI input
//! ([`run_midi_input`]).

use pipewire as pw;
use pw::spa;
use pw::spa::pod::{Object, Pod, Property, PropertyFlags, Value};
use pw::spa::utils::{Direction, Id};
use pw::{properties::properties, stream::StreamFlags};

mod midi;
pub use midi::{parse_midi, parse_ump};

/// The PipeWire error type, surfaced from setup and connection.
pub use pipewire::Error;

// ---------------------------------------------------------------------------------------------
// Audio output.
// ---------------------------------------------------------------------------------------------

/// Reusable per-callback render state: the caller's render closure plus a scratch buffer sized to
/// the negotiated quantum (preallocated, so the process callback never allocates).
struct RenderState<R> {
    render: R,
    scratch: Vec<f32>,
}

/// Opens a mono F32 playback stream at `rate` Hz and runs the PipeWire main loop, driving `render`
/// from the realtime process callback once per block. `render` fills its slice with one block of
/// mono samples; the engine writes them to the hardware buffer.
///
/// The core has no audio thread of its own — this *is* the pull. The call blocks running the loop
/// until the process is signalled (Ctrl-C); the stream autoconnects to the default sink, and
/// hardware routing is left to an external tool such as qpwgraph.
///
/// # Errors
/// Returns a [`pipewire::Error`] if the loop, context, stream or format negotiation fails.
pub fn run_audio_output<R>(name: &str, rate: u32, render: R) -> Result<(), Error>
where
    R: FnMut(&mut [f32]) + 'static,
{
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let stream = pw::stream::StreamBox::new(
        &core,
        name,
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::AUDIO_CHANNELS => "1",
        },
    )?;

    let state = RenderState {
        render,
        scratch: Vec::with_capacity(16_384),
    };

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .process(|stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let data = &mut datas[0];
            let stride = std::mem::size_of::<f32>();
            let Some(slice) = data.data() else {
                return;
            };
            let frames = slice.len() / stride;

            state.scratch.resize(frames, 0.0);
            let block = &mut state.scratch[..frames];
            block.fill(0.0);
            (state.render)(block);

            for (i, &sample) in block.iter().enumerate() {
                slice[i * stride..i * stride + stride].copy_from_slice(&sample.to_le_bytes());
            }

            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as _;
            *chunk.size_mut() = (stride * frames) as _;
        })
        .register()?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(rate);
    audio_info.set_channels(1);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_MONO;
    audio_info.set_position(position);

    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: pw::spa::sys::SPA_TYPE_OBJECT_Format,
            id: pw::spa::sys::SPA_PARAM_EnumFormat,
            properties: audio_info.into(),
        }),
    )
    .unwrap()
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).unwrap()];

    stream.connect(
        Direction::Output,
        None,
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    mainloop.run();

    Ok(())
}

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
    /// the core does not model.
    #[must_use]
    pub fn event(&self) -> Option<crate::event::Event> {
        match self.kind {
            MidiKind::Legacy => parse_midi(self.bytes),
            MidiKind::Ump => parse_ump(self.bytes),
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

/// Opens a PipeWire MIDI capture stream and runs the main loop, invoking `on_control` for every
/// control in every block (from the realtime process callback).
///
/// The stream exposes a MIDI input port; route your hardware keyboard to it externally (e.g. in
/// qpwgraph) — the core does routing nowhere itself. The call blocks running the loop until the
/// process is signalled (Ctrl-C).
///
/// # Errors
/// Returns a [`pipewire::Error`] if the loop, context, stream or format negotiation fails.
pub fn run_midi_input<F>(name: &str, on_control: F) -> Result<(), Error>
where
    F: FnMut(&MidiControl) + 'static,
{
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let stream = pw::stream::StreamBox::new(
        &core,
        name,
        properties! {
            *pw::keys::MEDIA_TYPE => "Midi",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Music",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(on_control)
        .process(|stream, on_control| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            for data in buffer.datas_mut() {
                let size = data.chunk().size() as usize;
                let offset = data.chunk().offset() as usize;
                let Some(bytes) = data.data() else {
                    continue;
                };
                if offset + size > bytes.len() {
                    continue;
                }
                for_each_control(&bytes[offset..offset + size], |control| on_control(control));
            }
        })
        .register()?;

    // MIDI format: application/control (a SPA_TYPE_Sequence of controls).
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: pw::spa::sys::SPA_TYPE_OBJECT_Format,
            id: pw::spa::sys::SPA_PARAM_EnumFormat,
            properties: vec![
                Property {
                    key: pw::spa::sys::SPA_FORMAT_mediaType,
                    flags: PropertyFlags::empty(),
                    value: Value::Id(Id(pw::spa::sys::SPA_MEDIA_TYPE_application)),
                },
                Property {
                    key: pw::spa::sys::SPA_FORMAT_mediaSubtype,
                    flags: PropertyFlags::empty(),
                    value: Value::Id(Id(pw::spa::sys::SPA_MEDIA_SUBTYPE_control)),
                },
            ],
        }),
    )
    .unwrap()
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).unwrap()];

    stream.connect(
        Direction::Input,
        None,
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    mainloop.run();

    Ok(())
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
}
