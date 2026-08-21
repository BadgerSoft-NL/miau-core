//! Reading the PipeWire graph and making links in it.
//!
//! [`Client`](super::Client) owns one node and its ports; this owns the view *around* that node —
//! which MIDI destinations the machine has, and which of them our output ports are wired to. It is
//! a second connection to the server on a thread of its own, because a PipeWire loop is not
//! `Send` and the realtime callback must never wait on graph traffic.
//!
//! "Graph" here means PipeWire's, not [`crate::graph`]: this module is about wires between
//! processes on the machine, that one is about nodes inside the engine. Hence the name.
//!
//! Routing is a shell's decision, so this module carries no policy: it does not rank
//! destinations, hide any, or connect anything on its own. It reports what exists and applies
//! exactly the set of links it is handed.
//!
//! # What is stable and what is not
//!
//! Object ids in the graph are per session — a device that comes back after being switched off
//! gets new ones, descending and with gaps. A destination is therefore addressed by its
//! [`key`](MidiDestination::key), taken from `port.alias`, which survives restarts and
//! reconnections. That is also what [`set_links`](Patchbay::set_links) speaks in: a wish for a
//! device that is currently absent stays on file, and the link is made the moment the device
//! reappears.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

use pipewire as pw;
use pw::types::ObjectType;

use super::Error;

/// What PipeWire puts in `format.dsp` for a MIDI port. Two flavours, because a server can carry
/// either legacy bytes or UMP over the wire.
const DSP_MIDI: &str = "8 bit raw midi";
const DSP_UMP: &str = "32 bit raw UMP";

/// A MIDI destination: an input port belonging to something else on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MidiDestination {
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

enum Command {
    /// The full set of wanted links, as `(our output port index, destination key)`.
    Links(Vec<(usize, String)>),
    Stop,
}

/// A live view of the graph around one node, and the links this client owns in it.
///
/// Dropping it stops the thread and takes every link it made with it: the link proxies die with
/// the struct, and the server removes a link whose proxy is gone.
pub struct Patchbay {
    tx: pw::channel::Sender<Command>,
    /// What the thread last saw. A mutex is fine here — this is the control side, not the
    /// realtime one.
    destinations: Arc<Mutex<Vec<MidiDestination>>>,
    thread: Option<JoinHandle<()>>,
}

impl Patchbay {
    /// Connects to the server and starts watching, on a thread of its own.
    ///
    /// `own_node` is the node whose output ports may be linked from — in practice
    /// [`Client::node_id`](super::Client::node_id). Ports on that node named `midi_out_N` are
    /// recognised as link sources, indexed by `N`; every MIDI input port elsewhere becomes a
    /// destination.
    ///
    /// Blocks until the connection stands, like [`ClientBuilder::start`](super::ClientBuilder),
    /// so do not call it from a UI thread.
    ///
    /// # Errors
    /// Returns a [`pipewire::Error`] if the thread cannot be spawned, the server cannot be
    /// reached, or it does not answer within five seconds.
    pub fn start(own_node: u32) -> Result<Self, Error> {
        let destinations = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = pw::channel::channel::<Command>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), Error>>();

        let shared = Arc::clone(&destinations);
        let thread = std::thread::Builder::new()
            .name("miau-patchbay".to_owned())
            .spawn(move || run(own_node, shared, rx, &ready_tx))
            .map_err(|_| Error::CreationFailed)?;

        match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                tx,
                destinations,
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

    /// The MIDI destinations as they stand right now, ordered by device and then port name.
    ///
    /// A snapshot, not a subscription: the list changes when hardware is switched on or off,
    /// which is a few times a day, so asking now and then costs less than an event stream. The
    /// order is stable, so a port appearing does not reshuffle what is already on screen.
    #[must_use]
    pub fn destinations(&self) -> Vec<MidiDestination> {
        self.destinations
            .lock()
            .map(|list| list.clone())
            .unwrap_or_default()
    }

    /// Sets which links should exist, as `(output port index, destination key)` pairs.
    ///
    /// Always the whole picture, never a change: the thread lays it beside what is actually
    /// there and makes up the difference. Nothing can drift out of step after a link that failed
    /// to materialise, and a wish whose device is absent simply waits.
    ///
    /// Returns immediately; the work happens on the graph thread.
    pub fn set_links(&self, links: Vec<(usize, String)>) {
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
fn split_name(port_name: &str, node_name: &str) -> (String, String) {
    let bare = port_name
        .strip_suffix(" (playback)")
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
    /// The destinations, by object id.
    destinations: BTreeMap<u32, MidiDestination>,
    wanted: BTreeSet<(usize, String)>,
    /// The links that exist. Holding the proxy *is* the link: drop it and the server takes the
    /// link away. That is what we want — when the client goes, so does its routing.
    links: BTreeMap<(usize, String), pw::link::Link>,
    published: Arc<Mutex<Vec<MidiDestination>>>,
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

                // One of ours: remember which index it is.
                if node == self.own_node && direction == "out" {
                    if let Some(index) = port_name
                        .strip_prefix("midi_out_")
                        .and_then(|n| n.parse::<usize>().ok())
                    {
                        self.outputs.insert(index, g.id);
                        return true;
                    }
                    return false;
                }

                // A destination is a MIDI input belonging to someone else.
                if direction != "in" || node == self.own_node {
                    return false;
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
                self.destinations.insert(
                    g.id,
                    MidiDestination {
                        id: g.id,
                        node,
                        key,
                        device,
                        name,
                    },
                );
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
        if self.destinations.remove(&id).is_some() {
            self.publish();
            return true;
        }
        if let Some(index) = self
            .outputs
            .iter()
            .find(|(_, port)| **port == id)
            .map(|(index, _)| *index)
        {
            self.outputs.remove(&index);
            return true;
        }
        false
    }

    fn publish(&self) {
        if let Ok(mut out) = self.published.lock() {
            let mut list: Vec<MidiDestination> = self.destinations.values().cloned().collect();
            list.sort_by(|a, b| (&a.device, &a.name).cmp(&(&b.device, &b.name)));
            *out = list;
        }
    }

    fn destination(&self, key: &str) -> Option<&MidiDestination> {
        self.destinations.values().find(|d| d.key == key)
    }

    /// Lays the wish beside what exists and makes up the difference.
    fn apply(&mut self, core: &pw::core::CoreRc) {
        // Anything no longer wanted, or whose destination or source port has vanished: drop the
        // proxy and the link is gone.
        let stale: Vec<(usize, String)> = self
            .links
            .keys()
            .filter(|key| {
                !self.wanted.contains(*key)
                    || self.destination(&key.1).is_none()
                    || !self.outputs.contains_key(&key.0)
            })
            .cloned()
            .collect();
        for key in stale {
            self.links.remove(&key);
        }

        for (index, key) in self.wanted.clone() {
            if self.links.contains_key(&(index, key.clone())) {
                continue;
            }
            let (Some(from), Some(to)) = (
                self.outputs.get(&index).copied(),
                self.destination(&key).map(|d| (d.node, d.id)),
            ) else {
                // Destination is switched off, or our node is not complete yet. The wish stays;
                // the next registry message tries again.
                continue;
            };
            let props = pw::properties::properties! {
                *pw::keys::LINK_OUTPUT_NODE => self.own_node.to_string(),
                *pw::keys::LINK_OUTPUT_PORT => from.to_string(),
                *pw::keys::LINK_INPUT_NODE => to.0.to_string(),
                *pw::keys::LINK_INPUT_PORT => to.1.to_string(),
            };
            if let Ok(link) = core.create_object::<pw::link::Link>("link-factory", &props) {
                self.links.insert((index, key), link);
            }
            // A link that cannot be created is not fatal: the rest of the routing still stands,
            // and the wish is retried on the next graph change.
        }
    }
}

/// The thread: connect, listen, link.
fn run(
    own_node: u32,
    published: Arc<Mutex<Vec<MidiDestination>>>,
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
        destinations: BTreeMap::new(),
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
    use super::split_name;

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

    #[test]
    fn falls_back_to_the_node_when_the_port_names_no_device() {
        // The Bluetooth MIDI server names its port just "in".
        assert_eq!(
            split_name("in", "bluez_midi.server"),
            ("bluez_midi.server".to_owned(), "in".to_owned())
        );
    }
}
