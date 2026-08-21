//! One PipeWire client with as many ports as a shell needs.
//!
//! This replaced three single-purpose helpers (`run_audio_output`, `run_midi_input`,
//! `run_midi_output`) that each owned a main loop, blocked until the process was signalled, and
//! exposed exactly one port. Enough for an example and not for an application: a GUI cannot
//! block, a MIDI tool wants a port per lane, and an engine wants to see input and output *in the
//! same block*.
//!
//! [`Client`] is the answer to all three. It is one PipeWire node (`pw_filter`) carrying any mix
//! of MIDI and audio ports, driven by a single process callback, running on a thread of its own
//! with a real start/stop lifecycle.
//!
//! ## Threading
//!
//! Every PipeWire object is created, used and destroyed on the client's own thread — nothing is
//! shared across threads, so there is no `unsafe impl Send` anywhere in this module. [`Client`]
//! holds only a [`pw::channel::Sender`] and a join handle, both `Send` on their own merits, which
//! is what lets a shell park the client in application state.
//!
//! ## Safety
//!
//! `pipewire-rs` wraps `pw_stream` but not `pw_filter`, so the filter calls go through
//! `pipewire::sys` directly. That FFI is confined to this file. The invariants it rests on:
//!
//! - `Inner` lives in a `Box` for as long as the filter does, so the `user_data` pointer PipeWire
//!   hands back to [`on_process`] always points at a live, uniquely-borrowed `Inner`.
//! - The `pw_filter_events` struct is boxed too: `pw_filter_new_simple` keeps the pointer rather
//!   than copying the struct.
//! - `on_process` runs on the realtime thread and touches nothing but `Inner` and the mapped
//!   buffers — no allocation, no locks, no I/O (invariant 11).
//! - Buffers are dequeued before the user closure and queued after it, exactly once each.

use std::ffi::{CString, c_void};
use std::ptr;
use std::sync::mpsc;
use std::thread::JoinHandle;

use pipewire as pw;
use pw::spa;
use pw::sys as pw_sys;

use super::{Error, MidiControl, MidiKind, MidiSink, for_each_control};

/// What a [`Client`]'s thread is being asked to do.
enum Command {
    /// Leave the main loop and tear the filter down.
    Stop,
    /// Ask the server for a different block size by updating `node.latency`.
    Latency { frames: u32, rate: u32 },
}

/// A MIDI port's realtime state for the current block.
struct MidiPort {
    /// The handle `pw_filter_add_port` returned; identifies the port to every other filter call.
    data: *mut c_void,
    /// The buffer dequeued for this block, or null when none was available.
    buffer: *mut pw_sys::pw_buffer,
    /// Mapped payload for this block, or null when there is nothing to read or write.
    ptr: *mut u8,
    /// Bytes readable (input) or writable (output) at `ptr`.
    len: usize,
    /// Bytes written this block; reported to PipeWire as the chunk size (output only).
    written: usize,
}

/// An audio port's realtime state for the current block.
struct AudioPort {
    data: *mut c_void,
    /// The block's samples, or null when PipeWire had no buffer for us.
    ptr: *mut f32,
}

/// Everything the realtime callback needs, owned by the client thread and reached through the
/// `user_data` pointer PipeWire carries for us.
struct Inner {
    process: Box<dyn FnMut(&mut Block) + Send>,
    midi_in: Vec<MidiPort>,
    midi_out: Vec<MidiPort>,
    audio_in: Vec<AudioPort>,
    audio_out: Vec<AudioPort>,
    kind: MidiKind,
    /// Set right after the filter is created, so [`on_state_changed`] can ask for the node id.
    filter: *mut pw_sys::pw_filter,
    /// Reports the outcome of startup to [`ClientBuilder::start`], exactly once.
    ready: Option<mpsc::Sender<Result<u32, Error>>>,
}

/// One processing block: the ports, and how many samples long it is.
///
/// Handed to the process closure once per PipeWire cycle. Every accessor is realtime-safe; the
/// closure that receives it must be too (invariant 11 — no allocation, no locks, no I/O).
pub struct Block<'a> {
    frames: u32,
    rate: u32,
    nsec: u64,
    kind: MidiKind,
    midi_in: &'a mut [MidiPort],
    midi_out: &'a mut [MidiPort],
    audio_in: &'a mut [AudioPort],
    audio_out: &'a mut [AudioPort],
}

impl Block<'_> {
    /// The block length in samples — how far the engine advances the transport this cycle.
    #[must_use]
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// The graph's sample rate in Hz, as reported by the driver's clock.
    ///
    /// This is the rate [`frames`](Block::frames) is counted in, and the rate a [`TempoMap`]
    /// must be built with for musical time to line up with wall time. May be `0` on a server
    /// that does not fill the clock's rate in; a caller should treat that as "unknown" rather
    /// than divide by it.
    ///
    /// [`TempoMap`]: crate::time::TempoMap
    #[must_use]
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// When this cycle started, in nanoseconds on `CLOCK_MONOTONIC`.
    ///
    /// A real instant in the recent past, straight from the driver's clock. A shell that
    /// timestamps live input against the same clock can turn "how long ago was the key hit"
    /// into a sample offset within the block instead of placing everything at offset 0.
    #[must_use]
    pub fn nsec(&self) -> u64 {
        self.nsec
    }

    /// The wire encoding this client's MIDI ports use.
    #[must_use]
    pub fn midi_kind(&self) -> MidiKind {
        self.kind
    }

    /// How many MIDI input ports the client has.
    #[must_use]
    pub fn midi_in_ports(&self) -> usize {
        self.midi_in.len()
    }

    /// How many MIDI output ports the client has.
    #[must_use]
    pub fn midi_out_ports(&self) -> usize {
        self.midi_out.len()
    }

    /// How many audio input ports the client has.
    #[must_use]
    pub fn audio_in_ports(&self) -> usize {
        self.audio_in.len()
    }

    /// How many audio output ports the client has.
    #[must_use]
    pub fn audio_out_ports(&self) -> usize {
        self.audio_out.len()
    }

    /// Calls `f` for every MIDI control that arrived on input `port` this block, in ascending
    /// sample-offset order. Returns `false` for an unknown port or a block with no input buffer.
    ///
    /// Controls carry raw bytes; [`MidiControl::event`] does the byte→typed conversion, so raw
    /// MIDI still goes no deeper than this edge (invariant 6).
    pub fn midi_in(&self, port: usize, f: impl FnMut(&MidiControl)) -> bool {
        let Some(p) = self.midi_in.get(port) else {
            return false;
        };
        if p.ptr.is_null() {
            return false;
        }
        // SAFETY: `ptr`/`len` were set from this block's mapped buffer in `prepare_midi_in` and
        // bounds-checked against `maxsize` there; the buffer stays queued until after `process`.
        let pod = unsafe { std::slice::from_raw_parts(p.ptr, p.len) };
        for_each_control(pod, f);
        true
    }

    /// Gives `f` a [`MidiSink`] writing to output `port` for this block. Returns `false` for an
    /// unknown port or a block with no output buffer, in which case `f` is not called.
    ///
    /// Takes a closure rather than handing out the sink so the sequence header is always patched
    /// and the size always reported — a half-written buffer is not a thing a caller can leave
    /// behind. Calling it twice for one port replaces what the first call wrote.
    pub fn midi_out(&mut self, port: usize, f: impl FnOnce(&mut MidiSink)) -> bool {
        let frames = self.frames;
        let kind = self.kind;
        let Some(p) = self.midi_out.get_mut(port) else {
            return false;
        };
        if p.ptr.is_null() {
            return false;
        }
        // SAFETY: as above — `ptr`/`len` describe this block's mapped output buffer, which is
        // ours exclusively until it is queued after `process`.
        let buf = unsafe { std::slice::from_raw_parts_mut(p.ptr, p.len) };
        let Some(mut sink) = MidiSink::new(buf, kind, frames) else {
            return false;
        };
        f(&mut sink);
        p.written = sink.finish();
        true
    }

    /// The samples that arrived on audio input `port` this block.
    #[must_use]
    pub fn audio_in(&self, port: usize) -> Option<&[f32]> {
        let p = self.audio_in.get(port)?;
        if p.ptr.is_null() {
            return None;
        }
        // SAFETY: `pw_filter_get_dsp_buffer` returned a buffer of at least `frames` samples for
        // this block; it is not touched again until the next cycle.
        Some(unsafe { std::slice::from_raw_parts(p.ptr, self.frames as usize) })
    }

    /// The block's samples on audio output `port`, to be filled. **Not** pre-zeroed: PipeWire
    /// hands back a recycled buffer, so write every sample or clear it first.
    #[must_use]
    pub fn audio_out(&mut self, port: usize) -> Option<&mut [f32]> {
        let frames = self.frames as usize;
        let p = self.audio_out.get_mut(port)?;
        if p.ptr.is_null() {
            return None;
        }
        // SAFETY: as in `audio_in`.
        Some(unsafe { std::slice::from_raw_parts_mut(p.ptr, frames) })
    }
}

/// Dequeues this block's buffer for each MIDI input port and records the readable payload.
///
/// # Safety
/// `ports` must hold live `port_data` handles from a connected filter, called on the realtime
/// thread inside `process`.
unsafe fn prepare_midi_in(ports: &mut [MidiPort]) {
    for port in ports {
        port.ptr = ptr::null_mut();
        port.len = 0;
        port.buffer = unsafe { pw_sys::pw_filter_dequeue_buffer(port.data) };
        if port.buffer.is_null() {
            continue;
        }
        // SAFETY: a dequeued pw_buffer owns a live spa_buffer with at least one data block.
        unsafe {
            let spa_buf = (*port.buffer).buffer;
            if spa_buf.is_null() || (*spa_buf).n_datas == 0 {
                continue;
            }
            let data = &*(*spa_buf).datas;
            if data.data.is_null() || data.chunk.is_null() {
                continue;
            }
            let chunk = &*data.chunk;
            let (offset, size) = (chunk.offset as usize, chunk.size as usize);
            // The producer is another process; treat its chunk as untrusted and clamp.
            if offset.saturating_add(size) > data.maxsize as usize {
                continue;
            }
            port.ptr = data.data.cast::<u8>().add(offset);
            port.len = size;
        }
    }
}

/// Dequeues this block's buffer for each MIDI output port and seeds it with an empty sequence,
/// so a port the closure never writes still emits something well-formed rather than stale bytes.
///
/// # Safety
/// As [`prepare_midi_in`].
unsafe fn prepare_midi_out(ports: &mut [MidiPort], kind: MidiKind, frames: u32) {
    for port in ports {
        port.ptr = ptr::null_mut();
        port.len = 0;
        port.written = 0;
        port.buffer = unsafe { pw_sys::pw_filter_dequeue_buffer(port.data) };
        if port.buffer.is_null() {
            continue;
        }
        // SAFETY: as in `prepare_midi_in`; an output buffer is ours to fill up to `maxsize`.
        unsafe {
            let spa_buf = (*port.buffer).buffer;
            if spa_buf.is_null() || (*spa_buf).n_datas == 0 {
                continue;
            }
            let data = &*(*spa_buf).datas;
            if data.data.is_null() || data.chunk.is_null() {
                continue;
            }
            port.ptr = data.data.cast::<u8>();
            port.len = data.maxsize as usize;

            let buf = std::slice::from_raw_parts_mut(port.ptr, port.len);
            port.written = MidiSink::new(buf, kind, frames).map_or(0, MidiSink::finish);
        }
    }
}

/// Reports each output port's byte count and returns every MIDI buffer to PipeWire.
///
/// # Safety
/// As [`prepare_midi_in`]. Each buffer is queued exactly once per block.
unsafe fn finish_midi(ports: &mut [MidiPort], output: bool) {
    for port in ports {
        if port.buffer.is_null() {
            continue;
        }
        // SAFETY: `buffer` was dequeued this block and not yet queued.
        unsafe {
            if output {
                let spa_buf = (*port.buffer).buffer;
                if !spa_buf.is_null() && (*spa_buf).n_datas > 0 {
                    let data = &*(*spa_buf).datas;
                    if !data.chunk.is_null() {
                        let chunk = &mut *data.chunk;
                        chunk.offset = 0;
                        chunk.stride = 1;
                        chunk.size = port.written as u32;
                    }
                }
            }
            pw_sys::pw_filter_queue_buffer(port.data, port.buffer);
        }
        port.buffer = ptr::null_mut();
        port.ptr = ptr::null_mut();
        port.len = 0;
    }
}

/// Points each audio port at this block's samples. `pw_filter_get_dsp_buffer` dequeues and queues
/// internally, so audio ports need no matching finish step.
///
/// # Safety
/// As [`prepare_midi_in`].
unsafe fn prepare_audio(ports: &mut [AudioPort], frames: u32) {
    for port in ports {
        port.ptr = unsafe { pw_sys::pw_filter_get_dsp_buffer(port.data, frames) }.cast::<f32>();
    }
}

/// Publishes the outcome of startup once the filter settles.
///
/// The node id does not exist until the filter is actually connected — asking for it right after
/// `pw_filter_connect` yields `SPA_ID_INVALID`. Reporting from here is what lets
/// [`ClientBuilder::start`] promise a *running* client with a real id.
///
/// # Safety
/// `data` must be the `Box<Inner>` pointer handed to `pw_filter_new_simple`. Runs on the client
/// thread's main loop, never on the realtime thread.
unsafe extern "C" fn on_state_changed(
    data: *mut c_void,
    _old: pw_sys::pw_filter_state,
    state: pw_sys::pw_filter_state,
    _error: *const std::os::raw::c_char,
) {
    if data.is_null() {
        return;
    }
    // SAFETY: as in `on_process`; state changes are delivered on the loop thread, and the
    // realtime callback never touches `ready` or `filter`.
    let inner = unsafe { &mut *data.cast::<Inner>() };

    let outcome = match state {
        pw_sys::pw_filter_state_PW_FILTER_STATE_ERROR => Err(Error::CreationFailed),
        pw_sys::pw_filter_state_PW_FILTER_STATE_PAUSED
        | pw_sys::pw_filter_state_PW_FILTER_STATE_STREAMING => {
            if inner.filter.is_null() {
                return;
            }
            // SAFETY: the filter is live and now connected, so the id is assigned.
            Ok(unsafe { pw_sys::pw_filter_get_node_id(inner.filter) })
        }
        // UNCONNECTED and CONNECTING are steps on the way; nothing to report yet.
        _ => return,
    };

    if let Some(ready) = inner.ready.take() {
        let _ = ready.send(outcome);
    }
}

/// The realtime entry point: PipeWire calls this once per cycle on the data thread.
///
/// # Safety
/// `data` must be the `Box<Inner>` pointer handed to `pw_filter_new_simple`, and this must run
/// as the filter's `process` callback so the port handles are live.
unsafe extern "C" fn on_process(data: *mut c_void, position: *mut spa::sys::spa_io_position) {
    if data.is_null() {
        return;
    }
    // SAFETY: `data` is the `Inner` the client thread boxed and kept alive for the filter's whole
    // life; PipeWire never calls `process` re-entrantly, so this borrow is unique.
    let inner = unsafe { &mut *data.cast::<Inner>() };

    // The quantum comes from the driver's clock. Without a position area there is no block to
    // render — bail rather than guess a length.
    if position.is_null() {
        return;
    }
    // SAFETY: PipeWire guarantees a valid position area for the duration of the callback.
    let clock = unsafe { &(*position).clock };
    let frames = clock.duration as u32;
    if frames == 0 {
        return;
    }
    // The clock's rate is a fraction; for audio it is 1/rate, so the denominator is the Hz.
    let rate = clock.rate.denom;
    let nsec = clock.nsec;

    let kind = inner.kind;
    // SAFETY: we are inside `process` on a connected filter; all port handles are live.
    unsafe {
        prepare_midi_in(&mut inner.midi_in);
        prepare_midi_out(&mut inner.midi_out, kind, frames);
        prepare_audio(&mut inner.audio_in, frames);
        prepare_audio(&mut inner.audio_out, frames);
    }

    {
        // Split the borrow: the closure and the ports are disjoint fields of `Inner`.
        let Inner {
            process,
            midi_in,
            midi_out,
            audio_in,
            audio_out,
            ..
        } = inner;
        let mut block = Block {
            frames,
            rate,
            nsec,
            kind,
            midi_in,
            midi_out,
            audio_in,
            audio_out,
        };
        process(&mut block);
    }

    // SAFETY: every buffer prepared above is queued back exactly once.
    unsafe {
        finish_midi(&mut inner.midi_in, false);
        finish_midi(&mut inner.midi_out, true);
    }
}

/// Describes the client to build: its name, its ports and its MIDI encoding.
///
/// ```no_run
/// use miau_core::io::ClientBuilder;
///
/// let client = ClientBuilder::new("my shell")
///     .midi_out(4)
///     .midi_in(1)
///     .start(|block| {
///         block.midi_out(0, |sink| {
///             let _ = sink.frames();
///         });
///     })?;
/// # Ok::<(), miau_core::io::Error>(())
/// ```
pub struct ClientBuilder {
    name: String,
    midi_in: usize,
    midi_out: usize,
    audio_in: usize,
    audio_out: usize,
    kind: MidiKind,
    latency: Option<u32>,
}

impl ClientBuilder {
    /// A client named `name` with no ports yet. The name is what a shell's user sees in a patchbay
    /// such as qpwgraph.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            midi_in: 0,
            midi_out: 0,
            audio_in: 0,
            audio_out: 0,
            kind: MidiKind::Ump,
            latency: None,
        }
    }

    /// Gives the client `n` MIDI input ports, named `midi_in_0`, `midi_in_1`, …
    #[must_use]
    pub fn midi_in(mut self, n: usize) -> Self {
        self.midi_in = n;
        self
    }

    /// Gives the client `n` MIDI output ports, named `midi_out_0`, `midi_out_1`, …
    ///
    /// One port per lane is the point: routing to a different synth per lane is then a patchbay
    /// operation, which is where routing belongs — the core does none itself.
    #[must_use]
    pub fn midi_out(mut self, n: usize) -> Self {
        self.midi_out = n;
        self
    }

    /// Gives the client `n` audio input ports, named `audio_in_0`, …
    #[must_use]
    pub fn audio_in(mut self, n: usize) -> Self {
        self.audio_in = n;
        self
    }

    /// Gives the client `n` audio output ports, named `audio_out_0`, …
    #[must_use]
    pub fn audio_out(mut self, n: usize) -> Self {
        self.audio_out = n;
        self
    }

    /// Sets the wire encoding for every MIDI port. Defaults to [`MidiKind::Ump`], which is what
    /// PipeWire 1.x carries natively; [`MidiKind::Legacy`] is for older servers.
    #[must_use]
    pub fn midi_kind(mut self, kind: MidiKind) -> Self {
        self.kind = kind;
        self
    }

    /// Asks the server for blocks of `frames` samples by setting `node.latency`.
    ///
    /// A request, not a guarantee: the server weighs it against every other node and its
    /// quantum limits, and `node.latency` is a fraction it scales to the graph's rate — the
    /// request is written against 48 kHz here, so on a 44.1 kHz graph the resulting block is
    /// proportionally smaller. What was actually granted is whatever [`Block::frames`] says.
    /// To adjust at runtime — when the real rate is known — use [`Client::set_latency`].
    #[must_use]
    pub fn latency(mut self, frames: u32) -> Self {
        self.latency = Some(frames);
        self
    }

    /// Starts the client on its own thread and returns once it is connected and running.
    ///
    /// `process` is called once per PipeWire cycle on the **realtime thread**: it must not
    /// allocate, lock, log or block (invariant 11). It is where the engine renders a block —
    /// [`Block::frames`] says how far to advance the transport.
    ///
    /// # Errors
    /// Returns a [`pipewire::Error`] if the loop, filter, ports or connection fail, or if the
    /// client thread cannot be spawned.
    pub fn start<F>(self, process: F) -> Result<Client, Error>
    where
        F: FnMut(&mut Block) + Send + 'static,
    {
        let (tx, rx) = pw::channel::channel::<Command>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, Error>>();

        let thread = std::thread::Builder::new()
            .name("miau-pipewire".to_owned())
            .spawn(move || run(self, Box::new(process), rx, &ready_tx))
            .map_err(|_| Error::CreationFailed)?;

        // The thread reports once the filter reaches Paused, so a successful return means the
        // node really exists. The timeout covers a server that never answers; without it a
        // missing daemon would hang the caller forever.
        let outcome = ready_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap_or(Err(Error::CreationFailed));

        match outcome {
            Ok(node_id) => Ok(Client {
                tx,
                thread: Some(thread),
                node_id,
            }),
            Err(error) => {
                // Wake the thread if it is sitting in its loop, then let it finish.
                let _ = tx.send(Command::Stop);
                let _ = thread.join();
                Err(error)
            }
        }
    }
}

/// A running PipeWire client: one node, its ports, and the thread driving them.
///
/// Dropping it stops the client and joins its thread, so a shell can simply let it fall out of
/// scope; [`stop`](Client::stop) is the same thing said out loud.
pub struct Client {
    tx: pw::channel::Sender<Command>,
    thread: Option<JoinHandle<()>>,
    node_id: u32,
}

impl Client {
    /// The PipeWire node id, for a shell that wants to make links itself instead of leaving it to
    /// a patchbay.
    #[must_use]
    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    /// Asks the server for blocks of `frames` samples at `rate` Hz, replacing any earlier
    /// request.
    ///
    /// `rate` should be the graph's real rate as reported by [`Block::rate`], so the fraction
    /// `frames/rate` means exactly `frames` samples. The request is applied on the client
    /// thread; the granted size shows up in [`Block::frames`] a few cycles later.
    pub fn set_latency(&self, frames: u32, rate: u32) {
        let _ = self.tx.send(Command::Latency { frames, rate });
    }

    /// Stops the client and waits for its thread to finish.
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Adds one port to the filter and returns its handle.
///
/// # Safety
/// `filter` must be a live, not-yet-connected filter.
unsafe fn add_port(
    filter: *mut pw_sys::pw_filter,
    direction: spa::utils::Direction,
    dsp_format: &str,
    name: &str,
) -> Option<*mut c_void> {
    let mut props = pw::properties::properties! {
        *pw::keys::FORMAT_DSP => dsp_format,
        *pw::keys::PORT_NAME => name,
    };
    // Label audio ports with a channel position so patchbays show them meaningfully.
    if dsp_format == DSP_AUDIO {
        props.insert(*pw::keys::AUDIO_CHANNEL, "MONO");
    }
    // SAFETY: the filter is live and the properties are handed over (PipeWire takes ownership).
    let data = unsafe {
        pw_sys::pw_filter_add_port(
            filter,
            direction.as_raw(),
            pw_sys::pw_filter_port_flags_PW_FILTER_PORT_FLAG_MAP_BUFFERS,
            std::mem::size_of::<usize>(),
            props.into_raw(),
            ptr::null_mut(),
            0,
        )
    };
    (!data.is_null()).then_some(data)
}

/// PipeWire's DSP format string for a MIDI port.
const DSP_MIDI: &str = "8 bit raw midi";
/// PipeWire's DSP format string for a mono float audio port.
const DSP_AUDIO: &str = "32 bit float mono audio";

/// The client thread: build the filter, connect it, run the loop, tear it all down.
fn run(
    spec: ClientBuilder,
    process: Box<dyn FnMut(&mut Block) + Send>,
    rx: pw::channel::Receiver<Command>,
    ready: &mpsc::Sender<Result<u32, Error>>,
) {
    pw::init();

    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(loop_) => loop_,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    let Ok(name) = CString::new(spec.name.as_str()) else {
        let _ = ready.send(Err(Error::CreationFailed));
        return;
    };

    // Boxed because `pw_filter_new_simple` keeps the pointer rather than copying the struct.
    let events = Box::new(pw_sys::pw_filter_events {
        version: pw_sys::PW_VERSION_FILTER_EVENTS,
        process: Some(on_process),
        state_changed: Some(on_state_changed),
        ..unsafe { std::mem::zeroed() }
    });

    // Boxed and kept alive for the filter's whole life: this is what `on_process` dereferences.
    let mut inner = Box::new(Inner {
        process,
        midi_in: Vec::with_capacity(spec.midi_in),
        midi_out: Vec::with_capacity(spec.midi_out),
        audio_in: Vec::with_capacity(spec.audio_in),
        audio_out: Vec::with_capacity(spec.audio_out),
        kind: spec.kind,
        filter: ptr::null_mut(),
        ready: Some(ready.clone()),
    });

    // Describe the node the way a patchbay expects, so it groups and sorts sensibly.
    let media_type = if spec.midi_in + spec.midi_out == 0 {
        "Audio"
    } else {
        "Midi"
    };
    let has_in = spec.midi_in + spec.audio_in > 0;
    let has_out = spec.midi_out + spec.audio_out > 0;
    let category = match (has_in, has_out) {
        (true, true) => "Duplex",
        (true, false) => "Capture",
        _ => "Playback",
    };
    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => media_type,
        *pw::keys::MEDIA_CATEGORY => category,
        *pw::keys::MEDIA_ROLE => "Music",
        // Without this PipeWire does not schedule a node that has no links, and the engine would
        // sit frozen until the user patched something — a transport that only runs once you route
        // it is a bug, not a feature.
        *pw::keys::NODE_ALWAYS_PROCESS => "true",
    };
    if let Some(frames) = spec.latency {
        // Written against 48 kHz because the real rate is not known before the first block; the
        // server scales the fraction to the graph's rate. `set_latency` can correct it later.
        props.insert(*pw::keys::NODE_LATENCY, format!("{frames}/48000"));
    }

    // SAFETY: the loop outlives the filter (both are dropped at the end of this function), the
    // events box is kept alive alongside it, and `inner` is a stable heap address.
    let filter = unsafe {
        pw_sys::pw_filter_new_simple(
            mainloop.loop_().as_raw_ptr(),
            name.as_ptr(),
            props.into_raw(),
            events.as_ref(),
            std::ptr::from_mut(inner.as_mut()).cast::<c_void>(),
        )
    };
    if filter.is_null() {
        let _ = ready.send(Err(Error::CreationFailed));
        return;
    }
    inner.filter = filter;

    // Ports, in the order the `Block` accessors index them.
    let mut failed = false;
    for (count, direction, format, prefix, sink) in [
        (
            spec.midi_in,
            spa::utils::Direction::Input,
            DSP_MIDI,
            "midi_in",
            &mut inner.midi_in,
        ),
        (
            spec.midi_out,
            spa::utils::Direction::Output,
            DSP_MIDI,
            "midi_out",
            &mut inner.midi_out,
        ),
    ] {
        for index in 0..count {
            // SAFETY: the filter is live and not yet connected.
            match unsafe { add_port(filter, direction, format, &format!("{prefix}_{index}")) } {
                Some(data) => sink.push(MidiPort {
                    data,
                    buffer: ptr::null_mut(),
                    ptr: ptr::null_mut(),
                    len: 0,
                    written: 0,
                }),
                None => failed = true,
            }
        }
    }
    for (count, direction, prefix, sink) in [
        (
            spec.audio_in,
            spa::utils::Direction::Input,
            "audio_in",
            &mut inner.audio_in,
        ),
        (
            spec.audio_out,
            spa::utils::Direction::Output,
            "audio_out",
            &mut inner.audio_out,
        ),
    ] {
        for index in 0..count {
            // SAFETY: as above.
            match unsafe { add_port(filter, direction, DSP_AUDIO, &format!("{prefix}_{index}")) } {
                Some(data) => sink.push(AudioPort {
                    data,
                    ptr: ptr::null_mut(),
                }),
                None => failed = true,
            }
        }
    }

    if failed {
        // SAFETY: the filter is live and never connected.
        unsafe { pw_sys::pw_filter_destroy(filter) };
        let _ = ready.send(Err(Error::CreationFailed));
        return;
    }

    // SAFETY: the filter is live with all its ports added.
    let connected = unsafe {
        pw_sys::pw_filter_connect(
            filter,
            pw_sys::pw_filter_flags_PW_FILTER_FLAG_RT_PROCESS,
            ptr::null_mut(),
            0,
        )
    };
    if connected < 0 {
        // SAFETY: the filter is live; connecting failed, so there is nothing to disconnect.
        unsafe { pw_sys::pw_filter_destroy(filter) };
        let _ = ready.send(Err(Error::CreationFailed));
        return;
    }

    let quit = mainloop.clone();
    // Captured as an address, not a pointer: a raw pointer would keep this closure from being
    // moved across the attach boundary on some toolchains, and the loop thread is the only
    // place it is turned back into one. The filter outlives the loop, so it is live here.
    let filter_addr = filter as usize;
    let _receiver = rx.attach(mainloop.loop_(), move |command| match command {
        Command::Stop => quit.quit(),
        Command::Latency { frames, rate } => {
            let props = pw::properties::properties! {
                *pw::keys::NODE_LATENCY => format!("{frames}/{rate}"),
            };
            // SAFETY: this runs on the loop thread while the loop runs, and the filter is not
            // destroyed until after `mainloop.run()` returns.
            unsafe {
                pw_sys::pw_filter_update_properties(
                    filter_addr as *mut pw_sys::pw_filter,
                    ptr::null_mut(),
                    props.dict().as_raw_ptr(),
                );
            }
        }
    });

    // Startup is reported from `on_state_changed` once the filter actually reaches Paused — the
    // node id does not exist before that. Running the loop is what makes that happen.
    mainloop.run();

    // SAFETY: the loop has returned, so `on_process` is no longer running and `inner` and
    // `events` are still alive — destroying the filter here is the last use of both.
    unsafe {
        pw_sys::pw_filter_disconnect(filter);
        pw_sys::pw_filter_destroy(filter);
    }
    drop(events);
    drop(inner);
}
