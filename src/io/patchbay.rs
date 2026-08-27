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
//! A [`destination`](Patchbay::destinations) is a MIDI input belonging to something else — where
//! our `midi_out_N` ports send. A [`source`](Patchbay::sources) is a MIDI output belonging to
//! something else — a keyboard, another app — which our `midi_in_N` ports can be fed from. Both
//! are [`MidiEndpoint`]; which list it came from says which way it faces.
//!
//! [`set_links`](Patchbay::set_links) speaks in [`LinkWish`], and takes **one list covering both
//! directions**. Not two setters: the thread keeps a single map of the links it owns, and two
//! calls that each claim to be "the whole picture" would erase each other's half.
//!
//! # What is stable and what is not
//!
//! Object ids in the graph are per session — a device that comes back after being switched off
//! gets new ones, descending and with gaps. An endpoint is therefore addressed by its
//! [`key`](MidiEndpoint::key), taken from `port.alias`, which survives restarts and reconnections.
//! That is also what a [`LinkWish`] names: a wish for a device that is currently absent stays on
//! file, and the link is made the moment the device reappears.

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

/// A MIDI port belonging to something else on this machine: a destination we can send to, or a
/// source we can be fed from. Which one it is follows from the list it came out of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MidiEndpoint {
    /// Object id in the graph. Used to make a link; never store it — see the module docs.
    pub id: u32,
    /// The node the port sits on. A link recipe wants both.
    pub node: u32,
    /// What a saved choice is keyed on, for example `"mioXM:mioXM DIN 1"`.
    pub key: String,
    /// The device, to group by: `"mioXM"`, `"Fireface400"`.
    pub device: String,
    /// The port within that device: `"DIN 1"`, `"MIDI 2"`.
    pub name: String,
}

/// Which way a link runs, seen from our own node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Direction {
    /// One of our `midi_out_N` ports into a destination's input.
    ToDestination,
    /// A source's output into one of our `midi_in_N` ports.
    FromSource,
}

/// One link that should exist: which of our ports, which way, and which endpoint.
///
/// Named by key rather than by object id, so it outlives the device being switched off — see the
/// module docs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LinkWish {
    /// The `N` in `midi_out_N` or `midi_in_N`, depending on `direction`.
    pub port: usize,
    /// Which way the link runs, and therefore which of our port sets `port` indexes.
    pub direction: Direction,
    /// The [`key`](MidiEndpoint::key) of the endpoint at the far end.
    pub key: String,
}

impl LinkWish {
    /// From our `midi_out_{port}` to the destination with this key.
    #[must_use]
    pub fn to_destination(port: usize, key: impl Into<String>) -> Self {
        Self {
            port,
            direction: Direction::ToDestination,
            key: key.into(),
        }
    }

    /// From the source with this key into our `midi_in_{port}`.
    #[must_use]
    pub fn from_source(port: usize, key: impl Into<String>) -> Self {
        Self {
            port,
            direction: Direction::FromSource,
            key: key.into(),
        }
    }
}

enum Command {
    /// The full set of wanted links, both directions together.
    Links(Vec<LinkWish>),
    Stop,
}

/// Both lists as the thread last saw them. One mutex rather than two, so a reader can never catch
/// the two halves a sweep apart.
#[derive(Default)]
struct Published {
    destinations: Vec<MidiEndpoint>,
    sources: Vec<MidiEndpoint>,
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
    /// [`Client::node_id`](super::Client::node_id). Ports on that node named `midi_out_N` are
    /// recognised as link sources and `midi_in_N` as link targets, both indexed by `N`; every
    /// other MIDI port on the machine becomes a [`destination`](Patchbay::destinations) or a
    /// [`source`](Patchbay::sources).
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

    /// The MIDI destinations as they stand right now — inputs elsewhere, where our `midi_out_N`
    /// ports can send. Ordered by device and then port name.
    ///
    /// A snapshot, not a subscription: the list changes when hardware is switched on or off,
    /// which is a few times a day, so asking now and then costs less than an event stream. The
    /// order is stable, so a port appearing does not reshuffle what is already on screen.
    #[must_use]
    pub fn destinations(&self) -> Vec<MidiEndpoint> {
        self.published
            .lock()
            .map(|p| p.destinations.clone())
            .unwrap_or_default()
    }

    /// The MIDI sources as they stand right now — outputs elsewhere, which our `midi_in_N` ports
    /// can be fed from. Same snapshot rules as [`destinations`](Patchbay::destinations).
    ///
    /// Several sources linked to one input port is a normal thing to want: the server merges
    /// them, so two keyboards arrive on one port as one stream.
    #[must_use]
    pub fn sources(&self) -> Vec<MidiEndpoint> {
        self.published
            .lock()
            .map(|p| p.sources.clone())
            .unwrap_or_default()
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
    /// Node id to name, as a fallback for ports that carry no device in their own name.
    nodes: BTreeMap<u32, String>,
    /// Our own output ports: the `N` from `midi_out_N` to the port's object id.
    outputs: BTreeMap<usize, u32>,
    /// Our own input ports: the `N` from `midi_in_N` to the port's object id.
    inputs: BTreeMap<usize, u32>,
    /// Inputs elsewhere, by object id.
    destinations: BTreeMap<u32, MidiEndpoint>,
    /// Outputs elsewhere, by object id.
    sources: BTreeMap<u32, MidiEndpoint>,
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
                false
            }
            ObjectType::Port => {
                let dsp = props.get(*pw::keys::FORMAT_DSP).unwrap_or_default();
                if dsp != DSP_MIDI && dsp != DSP_UMP {
                    return false;
                }
                let direction = props.get(*pw::keys::PORT_DIRECTION).unwrap_or_default();
                let node: u32 = props
                    .get(*pw::keys::NODE_ID)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let port_name = props.get(*pw::keys::PORT_NAME).unwrap_or_default();

                // One of ours: remember which index it is, on whichever side it sits.
                if node == self.own_node {
                    let ours = match direction {
                        "out" => port_name
                            .strip_prefix("midi_out_")
                            .and_then(|n| n.parse::<usize>().ok())
                            .map(|index| (&mut self.outputs, index)),
                        "in" => port_name
                            .strip_prefix("midi_in_")
                            .and_then(|n| n.parse::<usize>().ok())
                            .map(|index| (&mut self.inputs, index)),
                        _ => None,
                    };
                    let Some((map, index)) = ours else {
                        return false;
                    };
                    map.insert(index, g.id);
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
                let endpoint = MidiEndpoint {
                    id: g.id,
                    node,
                    key,
                    device,
                    name,
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
        self.nodes.remove(&id);
        if self.destinations.remove(&id).is_some() || self.sources.remove(&id).is_some() {
            self.publish();
            return true;
        }
        for map in [&mut self.outputs, &mut self.inputs] {
            if let Some(index) = map
                .iter()
                .find(|(_, port)| **port == id)
                .map(|(index, _)| *index)
            {
                map.remove(&index);
                return true;
            }
        }
        false
    }

    fn publish(&self) {
        let Ok(mut out) = self.published.lock() else {
            return;
        };
        let order = |list: &mut Vec<MidiEndpoint>| {
            list.sort_by(|a, b| (&a.device, &a.name).cmp(&(&b.device, &b.name)));
        };
        out.destinations = self.destinations.values().cloned().collect();
        out.sources = self.sources.values().cloned().collect();
        order(&mut out.destinations);
        order(&mut out.sources);
    }

    /// The endpoint at the far end of a wish, if it is switched on right now.
    fn endpoint(&self, wish: &LinkWish) -> Option<&MidiEndpoint> {
        let list = match wish.direction {
            Direction::ToDestination => &self.destinations,
            Direction::FromSource => &self.sources,
        };
        list.values().find(|e| e.key == wish.key)
    }

    /// Our own port for a wish, if the node has come up that far.
    fn own_port(&self, wish: &LinkWish) -> Option<u32> {
        let map = match wish.direction {
            Direction::ToDestination => &self.outputs,
            Direction::FromSource => &self.inputs,
        };
        map.get(&wish.port).copied()
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
        outputs: BTreeMap::new(),
        inputs: BTreeMap::new(),
        destinations: BTreeMap::new(),
        sources: BTreeMap::new(),
        wanted: BTreeSet::new(),
        links: BTreeMap::new(),
        published,
    }));

    let _listener = registry
        .add_listener_local()
        .global({
            let state = Rc::clone(&state);
            let core = core.clone();
            move |g| {
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
    use super::{Direction, LinkWish, split_name};

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
        let out = LinkWish::to_destination(0, "mioXM:mioXM DIN 1");
        let into = LinkWish::from_source(0, "mioXM:mioXM DIN 1");
        assert_ne!(out, into);
        assert_eq!(out.direction, Direction::ToDestination);
        assert_eq!(into.direction, Direction::FromSource);
    }
}
