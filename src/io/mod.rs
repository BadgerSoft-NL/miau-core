//! The PipeWire edge: the engine is *pulled* from PipeWire's process callback — the core has no
//! audio thread of its own. This module exposes ports and converts at the edge; hardware routing
//! stays external (qpwgraph). Raw MIDI becomes typed events here and nowhere deeper (invariant 6,
//! see [`parse_midi`]).
//!
//! Phase 4 provides mono audio output ([`run_audio_output`]) and the MIDI-byte parser. The
//! PipeWire MIDI *input* stream lands next.

use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;
use pw::{properties::properties, stream::StreamFlags};

mod midi;
pub use midi::parse_midi;

/// The PipeWire error type, surfaced from setup and connection.
pub use pipewire::Error;

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
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
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
        spa::utils::Direction::Output,
        None,
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    mainloop.run();

    Ok(())
}
