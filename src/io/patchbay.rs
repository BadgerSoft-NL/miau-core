//! Reading the PipeWire graph and making links in it.
//!
//! [`Client`](super::Client) owns one node and its ports; this owns the view *around* that node —
//! which MIDI endpoints the machine has, and which of them our ports are wired to. It is a second
//! connection to the server on a thread of its own, because a PipeWire loop is not `Send` and the
//! realtime callback must never wait on graph traffic.
//!
//! "Graph" here means PipeWire's, not [`crate::graph`]: this module is about wires between
//! processes on the machine, that one is about nodes inside the engine. Hence the name.
//!
//! Routing is a shell's decision, so this module carries no policy: it does not rank endpoints,
//! hide any, or connect anything on its own. It reports what exists and applies exactly the set of
//! links it is handed.
//!
//! # Both directions, one wish list
//!
//! A [`destination`](Patchbay::destinations) is an input belonging to something else — where our
//! `midi_out_N` and `audio_out_N` ports send. A [`source`](Patchbay::sources) is an output
//! belonging to something else — a keyboard, a sound card, another app — which our `midi_in_N` and
//! `audio_in_N` ports can be fed from. Both are [`Endpoint`]; which list it came from says which
//! way it faces, and its [`kind`](Endpoint::kind) says whether it carries notes or samples.
//!
//! Audio and MIDI are asked for separately ([`destinations`](Patchbay::destinations) takes a
//! [`PortKind`]) because a shell never wants them mixed: a menu of MIDI outputs with a sound card's
//! left channel in it is a menu with a bug in it.
//!
//! [`set_links`](Patchbay::set_links) speaks in [`LinkWish`], and takes **one list covering both
//! kinds and both directions**. Not four setters: the thread keeps a single map of the links it
//! owns, and two calls that each claim to be "the whole picture" would erase each other's half.
//!
//! # Where the speakers are
//!
//! Routing is a shell's decision, but "whatever this machine currently calls its speakers" is not
//! a decision, it is a fact the session manager keeps — so [`default_sink`](Patchbay::default_sink)
//! reports it. Without it every shell would have to make a device menu before it could make its
//! first sound, and a menu is the wrong first question to ask someone who pressed play.
//!
//! # What is stable and what is not
//!
//! Object ids in the graph are per session — a device that comes back after being switched off
//! gets new ones, descending and with gaps. An endpoint is therefore addressed by its
//! [`key`](Endpoint::key), taken from `port.alias`, which survives restarts and reconnections.
//! That is also what a [`LinkWish`] names: a wish for a device that is currently absent stays on
//! file, and the link is made the moment the device reappears.
//!
//! [`default_sink`](Patchbay::default_sink) is the exception and returns a node *id*: it answers
//! what is true right now, not what to remember, and a shell that saves it has saved the wrong
//! thing.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use pipewire as pw;
use pw::types::ObjectType;

use super::Error;

/// What PipeWire puts in `format.dsp` for a MIDI port. Two flavours, because a server can carry
/// either legacy bytes or UMP over the wire.
const DSP_MIDI: &str = "8 bit raw midi";
const DSP_UMP: &str = "32 bit raw UMP";
/// And for one channel of float audio, which is the only audio shape a DSP port has.
const DSP_AUDIO: &str = "32 bit float mono audio";

/// The metadata object the session manager keeps its defaults in, and the key for the sink.
const METADATA_DEFAULT: &str = "default";
const KEY_DEFAULT_SINK: &str = "default.audio.sink";

/// What a port carries. The same two kinds [`PortType`](crate::graph::PortType) knows about
/// inside the engine, seen from the machine's side of the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PortKind {
    /// Notes: `midi_out_N` / `midi_in_N` on our node.
    Midi,
    /// Samples: `audio_out_N` / `audio_in_N` on our node.
    Audio,
}

impl PortKind {
    /// The prefix our own ports of this kind and direction carry.
    fn prefix(self, direction: Direction) -> &'static str {
        match (self, direction) {
            (Self::Midi, Direction::ToDestination) => "midi_out_",
            (Self::Midi, Direction::FromSource) => "midi_in_",
            (Self::Audio, Direction::ToDestination) => "audio_out_",
            (Self::Audio, Direction::FromSource) => "audio_in_",
        }
    }

    /// Which kind a `format.dsp` string describes, if it is one we route.
    fn of_dsp(dsp: &str) -> Option<Self> {
        match dsp {
            DSP_MIDI | DSP_UMP => Some(Self::Midi),
            DSP_AUDIO => Some(Self::Audio),
            _ => None,
        }
    }
}

/// A port belonging to something else on this machine: a destination we can send to, or a source
/// we can be fed from. Which one it is follows from the list it came out of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// Object id in the graph. Used to make a link; never store it — see the module docs.
    pub id: u32,
    /// The node the port sits on. A link recipe wants both.
    pub node: u32,
    /// Whether this port carries notes or samples.
    pub kind: PortKind,
    /// What a saved choice is keyed on, for example `"mioXM:mioXM DIN 1"`.
    pub key: String,
    /// The device, to group by: `"mioXM"`, `"Fireface400"`.
    pub device: String,
    /// The port within that device: `"DIN 1"`, `"MIDI 2"`, `"playback_AUX0"`.
    pub name: String,
    /// For an audio port, the channel position it announces: `"FL"`, `"FR"`, `"MONO"`.
    ///
    /// This is how a stereo pair is told apart, and port order is not: a sound card lists its
    /// channels in whatever order its driver enumerated them. `None` for MIDI, and for an audio
    /// port that names no position.
    pub channel: Option<String>,
}

/// Which way a link runs, seen from our own node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Direction {
    /// One of our output ports into a destination's input.
    ToDestination,
    /// A source's output into one of our input ports.
    FromSource,
}

/// One link that should exist: which of our ports, of which kind, which way, and which endpoint.
///
/// Named by key rather than by object id, so it outlives the device being switched off — see the
/// module docs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LinkWish {
    /// The `N` in the port name `kind` and `direction` spell out.
    pub port: usize,
    /// Notes or samples — which of our two port families `port` indexes.
    pub kind: PortKind,
    /// Which way the link runs, and therefore which side of that family `port` indexes.
    pub direction: Direction,
    /// The [`key`](Endpoint::key) of the endpoint at the far end.
    pub key: String,
}

impl LinkWish {
    /// From our `midi_out_{port}` to the destination with this key.
    #[must_use]
    pub fn midi_out(port: usize, key: impl Into<String>) -> Self {
        Self::new(port, PortKind::Midi, Direction::ToDestination, key)
    }

    /// From the source with this key into our `midi_in_{port}`.
    #[must_use]
    pub fn midi_in(port: usize, key: impl Into<String>) -> Self {
        Self::new(port, PortKind::Midi, Direction::FromSource, key)
    }

    /// From our `audio_out_{port}` to the destination with this key.
    #[must_use]
    pub fn audio_out(port: usize, key: impl Into<String>) -> Self {
        Self::new(port, PortKind::Audio, Direction::ToDestination, key)
    }

    /// From the source with this key into our `audio_in_{port}`.
    #[must_use]
    pub fn audio_in(port: usize, key: impl Into<String>) -> Self {
        Self::new(port, PortKind::Audio, Direction::FromSource, key)
    }

    fn new(port: usize, kind: PortKind, direction: Direction, key: impl Into<String>) -> Self {
        Self {
            port,
            kind,
            direction,
            key: key.into(),
        }
    }
}

enum Command {
    /// The full set of wanted links, both directions together.
    Links(Vec<LinkWish>),
    Stop,
}

/// Everything as the thread last saw it. One mutex rather than three, so a reader can never catch
/// the parts a sweep apart.
#[derive(Default)]
struct Published {
    destinations: Vec<Endpoint>,
    sources: Vec<Endpoint>,
    default_sink: Option<u32>,
}

/// A live view of the graph around one node, and the links this client owns in it.
///
/// Dropping it stops the thread and takes every link it made with it: the link proxies die with
/// the struct, and the server removes a link whose proxy is gone.
pub struct Patchbay {
    tx: pw::channel::Sender<Command>,
    /// What the thread last saw. A mutex is fine here — this is the control side, not the
    /// realtime one.
    published: Arc<Mutex<Published>>,
    thread: Option<JoinHandle<()>>,
}

impl Patchbay {
    /// Connects to the server and starts watching, on a thread of its own.
    ///
    /// `own_node` is the node whose ports may be linked — in practice
    /// [`Client::node_id`](super::Client::node_id). Ports on that node named `midi_out_N` and
    /// `audio_out_N` are recognised as link sources, `midi_in_N` and `audio_in_N` as link targets,
    /// each indexed by `N`; every other MIDI or audio port on the machine becomes a
    /// [`destination`](Patchbay::destinations) or a [`source`](Patchbay::sources).
    ///
    /// Blocks until the connection stands, like [`ClientBuilder::start`](super::ClientBuilder),
    /// so do not call it from a UI thread.
    ///
    /// # Errors
    /// Returns a [`pipewire::Error`] if the thread cannot be spawned, the server cannot be
    /// reached, or it does not answer within five seconds.
    pub fn start(own_node: u32) -> Result<Self, Error> {
        let published = Arc::new(Mutex::new(Published::default()));
        let (tx, rx) = pw::channel::channel::<Command>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), Error>>();

        let shared = Arc::clone(&published);
        let thread = std::thread::Builder::new()
            .name("miau-patchbay".to_owned())
            .spawn(move || run(own_node, shared, rx, &ready_tx))
            .map_err(|_| Error::CreationFailed)?;

        match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                tx,
                published,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = tx.send(Command::Stop);
                let _ = thread.join();
                Err(Error::CreationFailed)
            }
        }
    }

    /// The destinations of one kind as they stand right now — inputs elsewhere, where our
    /// `midi_out_N` or `audio_out_N` ports can send. Ordered by device and then port name.
    ///
    /// A snapshot, not a subscription: the list changes when hardware is switched on or off,
    /// which is a few times a day, so asking now and then costs less than an event stream. The
    /// order is stable, so a port appearing does not reshuffle what is already on screen.
    #[must_use]
    pub fn destinations(&self, kind: PortKind) -> Vec<Endpoint> {
        self.of_kind(kind, |p| &p.destinations)
    }

    /// The sources of one kind as they stand right now — outputs elsewhere, which our `midi_in_N`
    /// or `audio_in_N` ports can be fed from. Same snapshot rules as
    /// [`destinations`](Patchbay::destinations).
    ///
    /// Several sources linked to one input port is a normal thing to want: the server merges
    /// them, so two keyboards arrive on one port as one stream.
    #[must_use]
    pub fn sources(&self, kind: PortKind) -> Vec<Endpoint> {
        self.of_kind(kind, |p| &p.sources)
    }

    fn of_kind(&self, kind: PortKind, pick: fn(&Published) -> &Vec<Endpoint>) -> Vec<Endpoint> {
        self.published
            .lock()
            .map(|p| {
                pick(&p)
                    .iter()
                    .filter(|e| e.kind == kind)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The node the session manager currently calls the default audio sink — the speakers.
    ///
    /// A node id and not a key, because this answers what is true now rather than what to
    /// remember (see the module docs). Filter [`destinations`](Patchbay::destinations) by it to
    /// find the ports to link to, and read each one's [`channel`](Endpoint::channel) to tell left
    /// from right.
    ///
    /// `None` while the graph is still being read, or on a machine with no sink at all.
    #[must_use]
    pub fn default_sink(&self) -> Option<u32> {
        self.published.lock().ok().and_then(|p| p.default_sink)
    }

    /// Sets which links should exist, in both directions at once.
    ///
    /// Always the whole picture, never a change: the thread lays it beside what is actually
    /// there and makes up the difference. Nothing can drift out of step after a link that failed
    /// to materialise, and a wish whose device is absent simply waits.
    ///
    /// One list for both directions on purpose — see the module docs. Passing only the outgoing
    /// wishes takes the incoming ones away.
    ///
    /// Returns immediately; the work happens on the graph thread.
    pub fn set_links(&self, links: Vec<LinkWish>) {
        let _ = self.tx.send(Command::Links(links));
    }
}

impl Drop for Patchbay {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Splits `"mioXM: DIN 1 (playback)"` into device and port name.
///
/// Both suffixes, because the same device shows up twice: PipeWire marks an input port
/// `(playback)` and an output port `(capture)`, and a source and a destination on one cable should
/// read the same in a menu.
fn split_name(port_name: &str, node_name: &str) -> (String, String) {
    let bare = port_name
        .strip_suffix(" (playback)")
        .or_else(|| port_name.strip_suffix(" (capture)"))
        .unwrap_or(port_name)
        .trim();
    match bare.split_once(": ") {
        Some((device, name)) => (device.to_owned(), name.to_owned()),
        // No device in the port name — Bluetooth does not put one there — so the node is the
        // device.
        None => (node_name.to_owned(), bare.to_owned()),
    }
}

/// Everything the thread keeps. Lives behind a `RefCell`: the registry callbacks and the command
/// queue both reach it.
struct State {
    own_node: u32,
    /// Node id to description, as a fallback for ports that carry no device in their own name.
    nodes: BTreeMap<u32, String>,
    /// Node id to `node.name` — the machine-readable one, which is what the session manager's
    /// default-sink metadata names. Two maps because the two names differ: `node.description`
    /// says "Fireface 400 Pro" where `node.name` says `alsa_output.firewire-0x…`.
    node_names: BTreeMap<u32, String>,
    /// Our own ports, keyed by what a wish names them: kind, direction and index.
    own: BTreeMap<(PortKind, Direction, usize), u32>,
    /// Inputs elsewhere, by object id.
    destinations: BTreeMap<u32, Endpoint>,
    /// Outputs elsewhere, by object id.
    sources: BTreeMap<u32, Endpoint>,
    /// The `node.name` the session manager last named as the default sink.
    default_sink: Option<String>,
    wanted: BTreeSet<LinkWish>,
    /// The links that exist. Holding the proxy *is* the link: drop it and the server takes the
    /// link away. That is what we want — when the client goes, so does its routing.
    links: BTreeMap<LinkWish, pw::link::Link>,
    published: Arc<Mutex<Published>>,
}

impl State {
    /// Takes one global from the registry. Returns whether something changed that the links
    /// depend on — only then is relinking needed.
    fn global(&mut self, g: &pw::registry::GlobalObject<&pw::spa::utils::dict::DictRef>) -> bool {
        let Some(props) = g.props else {
            return false;
        };
        match g.type_ {
            ObjectType::Node => {
                let name = props
                    .get(*pw::keys::NODE_DESCRIPTION)
                    .or_else(|| props.get(*pw::keys::NODE_NAME))
                    .unwrap_or_default();
                self.nodes.insert(g.id, name.to_owned());
                if let Some(raw) = props.get(*pw::keys::NODE_NAME) {
                    self.node_names.insert(g.id, raw.to_owned());
                }
                // The default sink is named, not numbered, so a node appearing can be the one
                // that answer was waiting for.
                self.publish();
                false
            }
            ObjectType::Port => {
                let dsp = props.get(*pw::keys::FORMAT_DSP).unwrap_or_default();
                let Some(kind) = PortKind::of_dsp(dsp) else {
                    return false;
                };
                let direction = props.get(*pw::keys::PORT_DIRECTION).unwrap_or_default();
                let node: u32 = props
                    .get(*pw::keys::NODE_ID)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let port_name = props.get(*pw::keys::PORT_NAME).unwrap_or_default();

                // One of ours: remember which index it is, on whichever side it sits.
                if node == self.own_node {
                    let ours = match direction {
                        "out" => Some(Direction::ToDestination),
                        "in" => Some(Direction::FromSource),
                        _ => None,
                    };
                    let Some(index) = ours.and_then(|way| {
                        port_name
                            .strip_prefix(kind.prefix(way))
                            .and_then(|n| n.parse::<usize>().ok())
                            .map(|index| (way, index))
                    }) else {
                        return false;
                    };
                    self.own.insert((kind, index.0, index.1), g.id);
                    return true;
                }

                let node_name = self.nodes.get(&node).cloned().unwrap_or_default();
                let (device, name) = split_name(port_name, &node_name);
                // The alias is usually already "device:port", which is exactly what we want.
                // Usually: the Bluetooth server calls its port just "in", and no choice can be
                // keyed on that because the next device may call its port the same. Without a
                // colon, build one.
                let key = props
                    .get(*pw::keys::PORT_ALIAS)
                    .filter(|alias| alias.contains(':'))
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{node_name}:{port_name}"));
                let endpoint = Endpoint {
                    id: g.id,
                    node,
                    kind,
                    key,
                    device,
                    name,
                    channel: match kind {
                        PortKind::Audio => props.get(*pw::keys::AUDIO_CHANNEL).map(str::to_owned),
                        PortKind::Midi => None,
                    },
                };
                // Someone else's input is a destination; their output is a source.
                match direction {
                    "in" => self.destinations.insert(g.id, endpoint),
                    "out" => self.sources.insert(g.id, endpoint),
                    _ => return false,
                };
                self.publish();
                true
            }
            _ => false,
        }
    }

    /// A global is gone. Ports and nodes disappear when a device is switched off; the server
    /// clears the links itself, but we still hold the proxies and those must go too — otherwise
    /// the link does not come back when the device does.
    fn removed(&mut self, id: u32) -> bool {
        let was_node = self.nodes.remove(&id).is_some();
        self.node_names.remove(&id);
        if self.destinations.remove(&id).is_some() || self.sources.remove(&id).is_some() {
            self.publish();
            return true;
        }
        if let Some(slot) = self
            .own
            .iter()
            .find(|(_, port)| **port == id)
            .map(|(slot, _)| *slot)
        {
            self.own.remove(&slot);
            return true;
        }
        if was_node {
            // The default sink may have been that node.
            self.publish();
        }
        false
    }

    /// The node id behind the session manager's default-sink name, if that node is here.
    fn default_sink_id(&self) -> Option<u32> {
        let wanted = self.default_sink.as_deref()?;
        self.node_names
            .iter()
            .find(|(_, name)| name.as_str() == wanted)
            .map(|(id, _)| *id)
    }

    fn publish(&self) {
        let Ok(mut out) = self.published.lock() else {
            return;
        };
        let order = |list: &mut Vec<Endpoint>| {
            list.sort_by(|a, b| (&a.device, &a.name).cmp(&(&b.device, &b.name)));
        };
        out.destinations = self.destinations.values().cloned().collect();
        out.sources = self.sources.values().cloned().collect();
        order(&mut out.destinations);
        order(&mut out.sources);
        out.default_sink = self.default_sink_id();
    }

    /// The endpoint at the far end of a wish, if it is switched on right now.
    fn endpoint(&self, wish: &LinkWish) -> Option<&Endpoint> {
        let list = match wish.direction {
            Direction::ToDestination => &self.destinations,
            Direction::FromSource => &self.sources,
        };
        // Kind as well as key: a cable can carry both, and linking a note port to a sample port
        // is a link the server would refuse anyway.
        list.values()
            .find(|e| e.key == wish.key && e.kind == wish.kind)
    }

    /// Our own port for a wish, if the node has come up that far.
    fn own_port(&self, wish: &LinkWish) -> Option<u32> {
        self.own
            .get(&(wish.kind, wish.direction, wish.port))
            .copied()
    }

    /// Lays the wish beside what exists and makes up the difference.
    fn apply(&mut self, core: &pw::core::CoreRc) {
        // Anything no longer wanted, or whose far end or own port has vanished: drop the proxy
        // and the link is gone.
        let stale: Vec<LinkWish> = self
            .links
            .keys()
            .filter(|wish| {
                !self.wanted.contains(*wish)
                    || self.endpoint(wish).is_none()
                    || self.own_port(wish).is_none()
            })
            .cloned()
            .collect();
        for wish in stale {
            self.links.remove(&wish);
        }

        for wish in self.wanted.clone() {
            if self.links.contains_key(&wish) {
                continue;
            }
            let (Some(ours), Some(theirs)) = (
                self.own_port(&wish),
                self.endpoint(&wish).map(|e| (e.node, e.id)),
            ) else {
                // The far end is switched off, or our node is not complete yet. The wish stays;
                // the next registry message tries again.
                continue;
            };
            // A link always reads output → input. Which of the two ends is ours is the whole
            // difference between the two directions.
            let (out_node, out_port, in_node, in_port) = match wish.direction {
                Direction::ToDestination => (self.own_node, ours, theirs.0, theirs.1),
                Direction::FromSource => (theirs.0, theirs.1, self.own_node, ours),
            };
            let props = pw::properties::properties! {
                *pw::keys::LINK_OUTPUT_NODE => out_node.to_string(),
                *pw::keys::LINK_OUTPUT_PORT => out_port.to_string(),
                *pw::keys::LINK_INPUT_NODE => in_node.to_string(),
                *pw::keys::LINK_INPUT_PORT => in_port.to_string(),
            };
            if let Ok(link) = core.create_object::<pw::link::Link>("link-factory", &props) {
                self.links.insert(wish, link);
            }
            // A link that cannot be created is not fatal: the rest of the routing still stands,
            // and the wish is retried on the next graph change.
        }
    }
}

/// Subscribes to the session manager's `default` metadata, where the default sink is named.
///
/// Kept separate from [`State::global`] because it is the one global we *bind* rather than read:
/// the registry only says a metadata object exists, and what is in it arrives afterwards, on its
/// own listener. The proxy and that listener go into `keep`, since dropping either unsubscribes.
fn watch_defaults(
    g: &pw::registry::GlobalObject<&pw::spa::utils::dict::DictRef>,
    registry: &pw::registry::RegistryRc,
    keep: &Rc<RefCell<Vec<(pw::metadata::Metadata, pw::metadata::MetadataListener)>>>,
    state: &Rc<RefCell<State>>,
    core: &pw::core::CoreRc,
) {
    // There are several metadata objects (`route-settings`, `settings`, …); only one holds the
    // defaults.
    if g.props.and_then(|p| p.get("metadata.name")) != Some(METADATA_DEFAULT) {
        return;
    }
    let Ok(meta) = registry.bind::<pw::metadata::Metadata, _>(g) else {
        return;
    };
    let listener = meta
        .add_listener_local()
        .property({
            let state = Rc::clone(state);
            let core = core.clone();
            move |_subject, key, _type, value| {
                if key == Some(KEY_DEFAULT_SINK) {
                    let mut s = state.borrow_mut();
                    s.default_sink = value.and_then(sink_name);
                    s.publish();
                    // A changed default sink does not move an existing wish — a wish names a key
                    // — but a shell that asked for "the speakers" will hand over a new list, and
                    // relinking now keeps the two from crossing.
                    s.apply(&core);
                }
                0
            }
        })
        .register();
    keep.borrow_mut().push((meta, listener));
}

/// Digs the node name out of a default-sink metadata value, which is JSON: `{"name":"alsa_…"}`.
///
/// A malformed value is not an error to shout about: it means this server says the default in a
/// way we do not read, and the honest answer is then "no default sink" rather than a guess.
fn sink_name(value: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(value)
        .ok()?
        .get("name")?
        .as_str()
        .map(str::to_owned)
}

/// The thread: connect, listen, link.
fn run(
    own_node: u32,
    published: Arc<Mutex<Published>>,
    rx: pw::channel::Receiver<Command>,
    ready: &mpsc::Sender<Result<(), Error>>,
) {
    pw::init();

    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(l) => l,
        Err(error) => return drop(ready.send(Err(error))),
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(c) => c,
        Err(error) => return drop(ready.send(Err(error))),
    };
    let core = match context.connect_rc(None) {
        Ok(c) => c,
        Err(error) => return drop(ready.send(Err(error))),
    };
    let registry = match core.get_registry_rc() {
        Ok(r) => r,
        Err(error) => return drop(ready.send(Err(error))),
    };

    let state = Rc::new(RefCell::new(State {
        own_node,
        nodes: BTreeMap::new(),
        node_names: BTreeMap::new(),
        own: BTreeMap::new(),
        destinations: BTreeMap::new(),
        sources: BTreeMap::new(),
        default_sink: None,
        wanted: BTreeSet::new(),
        links: BTreeMap::new(),
        published,
    }));

    // The metadata proxy and its listener have to outlive the callback that made them, or the
    // subscription dies the moment it is set up.
    let metadata: Rc<RefCell<Vec<(pw::metadata::Metadata, pw::metadata::MetadataListener)>>> =
        Rc::new(RefCell::new(Vec::new()));

    let _listener = registry
        .add_listener_local()
        .global({
            let state = Rc::clone(&state);
            let core = core.clone();
            let registry = registry.clone();
            let metadata = Rc::clone(&metadata);
            move |g| {
                if g.type_ == ObjectType::Metadata {
                    watch_defaults(g, &registry, &metadata, &state, &core);
                    return;
                }
                let mut s = state.borrow_mut();
                if s.global(g) {
                    s.apply(&core);
                }
            }
        })
        .global_remove({
            let state = Rc::clone(&state);
            let core = core.clone();
            move |id| {
                let mut s = state.borrow_mut();
                if s.removed(id) {
                    s.apply(&core);
                }
            }
        })
        .register();

    let _receiver = rx.attach(mainloop.loop_(), {
        let state = Rc::clone(&state);
        let core = core.clone();
        let quit = mainloop.clone();
        move |command| match command {
            Command::Links(links) => {
                let mut s = state.borrow_mut();
                s.wanted = links.into_iter().collect();
                s.apply(&core);
            }
            Command::Stop => quit.quit(),
        }
    });

    let _ = ready.send(Ok(()));
    mainloop.run();
}

#[cfg(test)]
mod tests {
    use super::{Direction, LinkWish, PortKind, sink_name, split_name};

    #[test]
    fn splits_device_from_port_name() {
        assert_eq!(
            split_name("mioXM: DIN 1 (playback)", "mioXM"),
            ("mioXM".to_owned(), "DIN 1".to_owned())
        );
        assert_eq!(
            split_name("Fireface400: MIDI 2", "Fireface400"),
            ("Fireface400".to_owned(), "MIDI 2".to_owned())
        );
    }

    /// The same cable shows up twice — `(playback)` for its input, `(capture)` for its output —
    /// and both should read the same in a menu.
    #[test]
    fn strips_either_side_suffix() {
        assert_eq!(
            split_name("mioXM: DIN 1 (capture)", "mioXM"),
            split_name("mioXM: DIN 1 (playback)", "mioXM")
        );
    }

    #[test]
    fn falls_back_to_the_node_when_the_port_names_no_device() {
        // The Bluetooth MIDI server names its port just "in".
        assert_eq!(
            split_name("in", "bluez_midi.server"),
            ("bluez_midi.server".to_owned(), "in".to_owned())
        );
    }

    /// Port index and key are not enough to tell two wishes apart: the same cable can be both a
    /// destination and a source, and `midi_out_0` and `midi_in_0` are different ports. If the
    /// direction dropped out of the identity, one of the two links would silently replace the
    /// other in the map.
    #[test]
    fn direction_is_part_of_a_wishs_identity() {
        let out = LinkWish::midi_out(0, "mioXM:mioXM DIN 1");
        let into = LinkWish::midi_in(0, "mioXM:mioXM DIN 1");
        assert_ne!(out, into);
        assert_eq!(out.direction, Direction::ToDestination);
        assert_eq!(into.direction, Direction::FromSource);
    }

    /// And so is the kind, for the same reason one step further: `midi_out_0` and `audio_out_0`
    /// are different ports on the same node, and without the kind in the identity one wish would
    /// silently take the other's place in the map.
    #[test]
    fn kind_is_part_of_a_wishs_identity_too() {
        let notes = LinkWish::midi_out(0, "Fireface400:MIDI 1");
        let samples = LinkWish::audio_out(0, "Fireface400:MIDI 1");
        assert_ne!(notes, samples);
        assert_eq!(notes.kind, PortKind::Midi);
        assert_eq!(samples.kind, PortKind::Audio);
        assert_eq!(notes.direction, samples.direction);
    }

    /// The four constructors name the four port families exactly, so a reader never has to work
    /// out which of our ports a wish indexes.
    #[test]
    fn each_constructor_names_its_own_port_family() {
        for (wish, kind, way) in [
            (
                LinkWish::midi_out(3, "k"),
                PortKind::Midi,
                Direction::ToDestination,
            ),
            (
                LinkWish::midi_in(3, "k"),
                PortKind::Midi,
                Direction::FromSource,
            ),
            (
                LinkWish::audio_out(3, "k"),
                PortKind::Audio,
                Direction::ToDestination,
            ),
            (
                LinkWish::audio_in(3, "k"),
                PortKind::Audio,
                Direction::FromSource,
            ),
        ] {
            assert_eq!((wish.kind, wish.direction, wish.port), (kind, way, 3));
        }
    }

    #[test]
    fn a_port_family_maps_to_its_prefix() {
        assert_eq!(PortKind::Midi.prefix(Direction::ToDestination), "midi_out_");
        assert_eq!(PortKind::Midi.prefix(Direction::FromSource), "midi_in_");
        assert_eq!(
            PortKind::Audio.prefix(Direction::ToDestination),
            "audio_out_"
        );
        assert_eq!(PortKind::Audio.prefix(Direction::FromSource), "audio_in_");
    }

    /// A server may carry MIDI as legacy bytes or as UMP; both are notes. Everything else — video,
    /// a control port — is not ours to route.
    #[test]
    fn only_the_two_kinds_we_route_are_recognised() {
        assert_eq!(PortKind::of_dsp("8 bit raw midi"), Some(PortKind::Midi));
        assert_eq!(PortKind::of_dsp("32 bit raw UMP"), Some(PortKind::Midi));
        assert_eq!(
            PortKind::of_dsp("32 bit float mono audio"),
            Some(PortKind::Audio)
        );
        assert_eq!(PortKind::of_dsp("32 bit float RGBA video"), None);
        assert_eq!(PortKind::of_dsp(""), None);
    }

    #[test]
    fn the_default_sink_is_read_out_of_its_json() {
        assert_eq!(
            sink_name(r#"{"name":"alsa_output.usb-Fireface","other":1}"#).as_deref(),
            Some("alsa_output.usb-Fireface")
        );
        // A value this server says in a way we do not read means "no default sink", not a guess.
        assert_eq!(sink_name("null"), None);
        assert_eq!(sink_name("{}"), None);
        assert_eq!(sink_name("not json at all"), None);
    }
}
