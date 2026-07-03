//! The `Node` trait, ports, the node arena, immutable topology snapshots, the compiled schedule,
//! the preallocated buffer pool and the garbage chute.
//!
//! The audio thread iterates the compiled schedule and calls [`Node::process`]; it never
//! traverses the graph (invariant 12) and never allocates or deallocates (invariant 11 — retired
//! nodes and snapshots leave via the [garbage chute](Chute), buffers come from a preallocated
//! pool). Topology is validated when edges are added (cycles rejected, invariant 13's typed
//! ports enforced) and baked into a flat, immutable [`Compiled`] snapshot on [`Graph::commit`].
//!
//! Phase-3 note: `commit` runs on the control thread (offline), so it may allocate and it swaps
//! the active snapshot directly. Phase 4 moves the swap onto the audio thread via a command +
//! the same retire-queue pattern the transport already uses.

use std::any::Any;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

use crate::event::Event;
use crate::time::{SampleOffset, SamplePos, TempoMap, TickRange};

/// Maximum audio/event ports of one kind on a single node.
const MAX_PORTS: usize = 32;
/// Preallocated capacity, in events, of each event buffer.
const MAX_EVENTS: usize = 4096;

/// The two port kinds. Port type is the only routing law (invariant 13): audio connects to
/// audio, events to events; everything else is shell convention.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PortType {
    /// A block of `f32` samples.
    Audio,
    /// A stream of timestamped [`Event`]s.
    Event,
}

/// How many ports of each kind a node exposes. Fixed for the lifetime of the node.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PortCounts {
    /// Number of audio input ports.
    pub audio_in: usize,
    /// Number of audio output ports.
    pub audio_out: usize,
    /// Number of event input ports.
    pub event_in: usize,
    /// Number of event output ports.
    pub event_out: usize,
}

/// A stable runtime handle to a node in the arena. Not persisted (invariant 15) — documents use
/// their own `NodeId` and the arena is rebuilt on load.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NodeKey {
    index: u32,
    generation: u32,
}

/// Per-block context handed to every [`Node::process`] call.
pub struct ProcessContext<'a> {
    /// Number of valid sample frames this block (`<=` the graph's max block size).
    pub frames: usize,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Absolute sample position of the block's first frame.
    pub block_start: SamplePos,
    /// Musical range covered by this block (for event sources).
    pub ticks: TickRange,
    /// Tempo map for tick↔sample conversion, when a transport is driving the graph.
    pub tempo: Option<&'a TempoMap>,
}

/// The routing contract: everything in the engine is a `Node` — utility DSP, CLAP plugins, the
/// PipeWire sink, hardware MIDI. `process` is a realtime path: no allocation, no locks, no I/O
/// (invariants 2 & 11). `Node: Any` enables downcasting a handle back to a concrete type.
pub trait Node: Any + Send {
    /// The node's fixed port layout.
    fn ports(&self) -> PortCounts;

    /// Processes one block, reading input ports and writing output ports through `io`.
    fn process(&mut self, ctx: &ProcessContext, io: &mut NodeIO);
}

/// A node's view of its port buffers for one block. Backed by the executor's reusable scratch;
/// slices are valid only for the duration of the `process` call.
pub struct NodeIO<'a> {
    frames: usize,
    block: usize,
    audio_in: &'a [f32],
    audio_out: &'a mut [f32],
    events_in: &'a [Vec<(SampleOffset, Event)>],
    events_out: &'a mut [Vec<(SampleOffset, Event)>],
}

impl NodeIO<'_> {
    /// Valid frame count for this block.
    #[must_use]
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Samples of audio input `port` (all zero if the port is unconnected).
    #[must_use]
    pub fn audio_in(&self, port: usize) -> &[f32] {
        &self.audio_in[port * self.block..][..self.frames]
    }

    /// Writable samples of audio output `port`.
    pub fn audio_out(&mut self, port: usize) -> &mut [f32] {
        &mut self.audio_out[port * self.block..][..self.frames]
    }

    /// Borrows an input and an output port at once (they are distinct buffers), for the common
    /// read-transform-write node shape.
    pub fn audio_in_out(&mut self, in_port: usize, out_port: usize) -> (&[f32], &mut [f32]) {
        let input = &self.audio_in[in_port * self.block..][..self.frames];
        let output = &mut self.audio_out[out_port * self.block..][..self.frames];
        (input, output)
    }

    /// Events arriving at event input `port`, ordered by sample offset.
    #[must_use]
    pub fn events_in(&self, port: usize) -> &[(SampleOffset, Event)] {
        &self.events_in[port]
    }

    /// The event output `port`'s buffer, to push `(offset, event)` pairs into.
    pub fn events_out(&mut self, port: usize) -> &mut Vec<(SampleOffset, Event)> {
        &mut self.events_out[port]
    }
}

/// Why [`Graph::connect_audio`]/[`Graph::connect_event`] refused an edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnectError {
    /// A referenced node is not in the arena.
    UnknownNode,
    /// A port index is out of range for its node.
    PortOutOfRange,
    /// The destination input port already has a source (one writer per input).
    InputAlreadyConnected,
    /// The edge would introduce a cycle; the graph must stay acyclic (invariant 12).
    WouldCycle,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            Self::UnknownNode => "node is not in the graph",
            Self::PortOutOfRange => "port index out of range",
            Self::InputAlreadyConnected => "input port already has a source",
            Self::WouldCycle => "edge would create a cycle",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for ConnectError {}

// ---------------------------------------------------------------------------------------------
// Internal: edges, arena, compiled schedule, buffer pool, garbage chute.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Edge {
    kind: PortType,
    src: NodeKey,
    src_port: u32,
    dst: NodeKey,
    dst_port: u32,
}

struct NodeEntry {
    node: Box<dyn Node>,
    counts: PortCounts,
}

/// Generational slab of nodes with stable handles.
#[derive(Default)]
struct Arena {
    slots: Vec<Option<NodeEntry>>,
    generations: Vec<u32>,
    free: Vec<u32>,
}

impl Arena {
    fn insert(&mut self, node: Box<dyn Node>, counts: PortCounts) -> NodeKey {
        let entry = NodeEntry { node, counts };
        if let Some(index) = self.free.pop() {
            let generation = self.generations[index as usize];
            self.slots[index as usize] = Some(entry);
            NodeKey { index, generation }
        } else {
            let index = self.slots.len() as u32;
            self.slots.push(Some(entry));
            self.generations.push(0);
            NodeKey {
                index,
                generation: 0,
            }
        }
    }

    fn remove(&mut self, key: NodeKey) -> Option<Box<dyn Node>> {
        if !self.contains(key) {
            return None;
        }
        let index = key.index as usize;
        self.generations[index] += 1;
        self.free.push(key.index);
        self.slots[index].take().map(|entry| entry.node)
    }

    fn contains(&self, key: NodeKey) -> bool {
        (key.index as usize) < self.slots.len()
            && self.generations[key.index as usize] == key.generation
            && self.slots[key.index as usize].is_some()
    }

    fn counts(&self, key: NodeKey) -> Option<PortCounts> {
        self.slots
            .get(key.index as usize)
            .and_then(|slot| slot.as_ref())
            .filter(|_| self.generations[key.index as usize] == key.generation)
            .map(|entry| entry.counts)
    }

    fn node_mut(&mut self, key: NodeKey) -> &mut dyn Node {
        self.slots[key.index as usize]
            .as_mut()
            .unwrap()
            .node
            .as_mut()
    }

    fn node(&self, key: NodeKey) -> Option<&dyn Node> {
        if !self.contains(key) {
            return None;
        }
        self.slots[key.index as usize]
            .as_ref()
            .map(|entry| entry.node.as_ref())
    }

    fn keys(&self) -> Vec<NodeKey> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slot.as_ref().map(|_| NodeKey {
                    index: index as u32,
                    generation: self.generations[index],
                })
            })
            .collect()
    }
}

/// One node's slot in the compiled schedule: the node plus the buffer ids feeding and receiving
/// each of its ports (buffer id 0 is the shared silence buffer for unconnected inputs).
struct Step {
    node: NodeKey,
    counts: PortCounts,
    audio_in: Vec<u32>,
    audio_out: Vec<u32>,
    event_in: Vec<u32>,
    event_out: Vec<u32>,
}

/// An immutable, executable topology snapshot (invariant 12). Retired via the chute on swap.
struct Compiled {
    steps: Vec<Step>,
}

impl Compiled {
    fn empty() -> Self {
        Self { steps: Vec::new() }
    }
}

/// The preallocated buffer pool plus per-node scratch. Grown only at commit (control thread);
/// `process` never resizes it (invariant 11).
struct Executor {
    block: usize,
    audio_pool: Vec<f32>,
    event_pool: Vec<Vec<(SampleOffset, Event)>>,
    ai_scratch: Vec<f32>,
    ao_scratch: Vec<f32>,
    ei_scratch: Vec<Vec<(SampleOffset, Event)>>,
    eo_scratch: Vec<Vec<(SampleOffset, Event)>>,
}

impl Executor {
    fn new(block: usize) -> Self {
        Self {
            block,
            audio_pool: Vec::new(),
            event_pool: Vec::new(),
            ai_scratch: vec![0.0; MAX_PORTS * block],
            ao_scratch: vec![0.0; MAX_PORTS * block],
            ei_scratch: (0..MAX_PORTS)
                .map(|_| Vec::with_capacity(MAX_EVENTS))
                .collect(),
            eo_scratch: (0..MAX_PORTS)
                .map(|_| Vec::with_capacity(MAX_EVENTS))
                .collect(),
        }
    }

    /// Ensures the pool holds at least `num_audio`/`num_event` buffers. Control-thread only.
    fn ensure(&mut self, num_audio: u32, num_event: u32) {
        let need = num_audio as usize * self.block;
        if self.audio_pool.len() < need {
            self.audio_pool.resize(need, 0.0);
        }
        while (self.event_pool.len() as u32) < num_event {
            self.event_pool.push(Vec::with_capacity(MAX_EVENTS));
        }
    }

    fn load_inputs(&mut self, step: &Step, frames: usize) {
        let block = self.block;
        let (pool, scratch) = (&self.audio_pool, &mut self.ai_scratch);
        for (port, &buf) in step.audio_in.iter().enumerate() {
            let src = &pool[buf as usize * block..][..frames];
            scratch[port * block..][..frames].copy_from_slice(src);
        }
        let (pool, scratch) = (&self.event_pool, &mut self.ei_scratch);
        for (port, &buf) in step.event_in.iter().enumerate() {
            scratch[port].clear();
            scratch[port].extend_from_slice(&pool[buf as usize]);
        }
    }

    fn begin(&mut self, counts: PortCounts, frames: usize) -> NodeIO<'_> {
        let block = self.block;
        self.ao_scratch[..counts.audio_out * block].fill(0.0);
        for buffer in &mut self.eo_scratch[..counts.event_out] {
            buffer.clear();
        }
        NodeIO {
            frames,
            block,
            audio_in: &self.ai_scratch[..counts.audio_in * block],
            audio_out: &mut self.ao_scratch[..counts.audio_out * block],
            events_in: &self.ei_scratch[..counts.event_in],
            events_out: &mut self.eo_scratch[..counts.event_out],
        }
    }

    fn store_outputs(&mut self, step: &Step, frames: usize) {
        let block = self.block;
        let (pool, scratch) = (&mut self.audio_pool, &self.ao_scratch);
        for (port, &buf) in step.audio_out.iter().enumerate() {
            let dst = &mut pool[buf as usize * block..][..frames];
            dst.copy_from_slice(&scratch[port * block..][..frames]);
        }
        let (pool, scratch) = (&mut self.event_pool, &self.eo_scratch);
        for (port, &buf) in step.event_out.iter().enumerate() {
            pool[buf as usize].clear();
            pool[buf as usize].extend_from_slice(&scratch[port]);
        }
    }
}

/// A retired node or snapshot on its way to the cleanup thread. The payload exists only to be
/// dropped there (never read), which is the whole point of the chute.
#[allow(dead_code)]
enum Garbage {
    Node(Box<dyn Node>),
    Compiled(Arc<Compiled>),
}

/// The garbage chute: retired nodes/snapshots leave the audio path here and are dropped on a
/// dedicated cleanup thread, never in `process` (invariant 11).
struct Chute {
    sender: Option<mpsc::Sender<Garbage>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Chute {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel::<Garbage>();
        let worker = thread::spawn(move || {
            for garbage in receiver {
                drop(garbage);
            }
        });
        Self {
            sender: Some(sender),
            worker: Some(worker),
        }
    }

    fn retire_node(&self, node: Box<dyn Node>) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(Garbage::Node(node));
        }
    }

    fn retire_compiled(&self, compiled: Arc<Compiled>) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(Garbage::Compiled(compiled));
        }
    }
}

impl Drop for Chute {
    fn drop(&mut self) {
        self.sender.take(); // close the channel so the worker loop ends
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The engine.
// ---------------------------------------------------------------------------------------------

/// The audio engine: owns the node arena, the current edges, the active compiled snapshot, the
/// buffer pool and the garbage chute.
pub struct Graph {
    arena: Arena,
    edges: Vec<Edge>,
    pending_remove: Vec<NodeKey>,
    active: Arc<Compiled>,
    executor: Executor,
    chute: Chute,
}

impl Graph {
    /// Creates an empty graph sized for blocks of up to `max_block` frames.
    #[must_use]
    pub fn new(max_block: usize) -> Self {
        let mut executor = Executor::new(max_block);
        executor.ensure(1, 1);
        Self {
            arena: Arena::default(),
            edges: Vec::new(),
            pending_remove: Vec::new(),
            active: Arc::new(Compiled::empty()),
            executor,
            chute: Chute::new(),
        }
    }

    /// Adds `node` and returns its handle. The new node is isolated until connected and does not
    /// take part in rendering until the next [`commit`](Graph::commit).
    pub fn add_node<N: Node>(&mut self, node: N) -> NodeKey {
        let counts = node.ports();
        debug_assert!(
            counts.audio_in <= MAX_PORTS
                && counts.audio_out <= MAX_PORTS
                && counts.event_in <= MAX_PORTS
                && counts.event_out <= MAX_PORTS,
            "node exceeds MAX_PORTS"
        );
        self.arena.insert(Box::new(node), counts)
    }

    /// Stages `key` for removal; it is dropped via the chute on the next [`commit`](Graph::commit).
    pub fn remove_node(&mut self, key: NodeKey) {
        if self.arena.contains(key) && !self.pending_remove.contains(&key) {
            self.pending_remove.push(key);
        }
    }

    /// Connects an audio output to an audio input (invariant 13).
    ///
    /// # Errors
    /// See [`ConnectError`].
    pub fn connect_audio(
        &mut self,
        from: (NodeKey, u32),
        to: (NodeKey, u32),
    ) -> Result<(), ConnectError> {
        self.connect(PortType::Audio, from, to)
    }

    /// Connects an event output to an event input (invariant 13).
    ///
    /// # Errors
    /// See [`ConnectError`].
    pub fn connect_event(
        &mut self,
        from: (NodeKey, u32),
        to: (NodeKey, u32),
    ) -> Result<(), ConnectError> {
        self.connect(PortType::Event, from, to)
    }

    /// Removes the audio edge between `from` and `to`, if present.
    pub fn disconnect_audio(&mut self, from: (NodeKey, u32), to: (NodeKey, u32)) {
        self.disconnect(PortType::Audio, from, to);
    }

    /// Removes the event edge between `from` and `to`, if present.
    pub fn disconnect_event(&mut self, from: (NodeKey, u32), to: (NodeKey, u32)) {
        self.disconnect(PortType::Event, from, to);
    }

    fn connect(
        &mut self,
        kind: PortType,
        from: (NodeKey, u32),
        to: (NodeKey, u32),
    ) -> Result<(), ConnectError> {
        let out_count = |c: PortCounts| match kind {
            PortType::Audio => c.audio_out,
            PortType::Event => c.event_out,
        };
        let in_count = |c: PortCounts| match kind {
            PortType::Audio => c.audio_in,
            PortType::Event => c.event_in,
        };
        let src_counts = self.arena.counts(from.0).ok_or(ConnectError::UnknownNode)?;
        let dst_counts = self.arena.counts(to.0).ok_or(ConnectError::UnknownNode)?;
        if from.1 as usize >= out_count(src_counts) || to.1 as usize >= in_count(dst_counts) {
            return Err(ConnectError::PortOutOfRange);
        }
        if self
            .edges
            .iter()
            .any(|e| e.kind == kind && e.dst == to.0 && e.dst_port == to.1)
        {
            return Err(ConnectError::InputAlreadyConnected);
        }
        if from.0 == to.0 || self.reachable(to.0, from.0) {
            return Err(ConnectError::WouldCycle);
        }
        self.edges.push(Edge {
            kind,
            src: from.0,
            src_port: from.1,
            dst: to.0,
            dst_port: to.1,
        });
        Ok(())
    }

    fn disconnect(&mut self, kind: PortType, from: (NodeKey, u32), to: (NodeKey, u32)) {
        self.edges.retain(|e| {
            !(e.kind == kind
                && e.src == from.0
                && e.src_port == from.1
                && e.dst == to.0
                && e.dst_port == to.1)
        });
    }

    /// Whether `target` is reachable from `origin` by following edges forward.
    fn reachable(&self, origin: NodeKey, target: NodeKey) -> bool {
        let mut stack = vec![origin];
        let mut seen = HashSet::new();
        while let Some(node) = stack.pop() {
            if node == target {
                return true;
            }
            if !seen.insert(node) {
                continue;
            }
            for edge in self.edges.iter().filter(|e| e.src == node) {
                stack.push(edge.dst);
            }
        }
        false
    }

    /// Validates and bakes the current topology into a new snapshot, swaps it in, and retires the
    /// old snapshot and any removed nodes via the chute. Runs on the control thread.
    pub fn commit(&mut self) {
        // Drop staged nodes from the working topology before compiling.
        if !self.pending_remove.is_empty() {
            let removed = &self.pending_remove;
            self.edges
                .retain(|e| !removed.contains(&e.src) && !removed.contains(&e.dst));
        }

        let nodes: Vec<NodeKey> = self
            .arena
            .keys()
            .into_iter()
            .filter(|k| !self.pending_remove.contains(k))
            .collect();
        let position = |key: NodeKey| nodes.iter().position(|&n| n == key).unwrap();
        let count = nodes.len();

        // Kahn topological sort. Edges are guaranteed acyclic by `connect`.
        let mut indegree = vec![0usize; count];
        let mut successors: Vec<Vec<usize>> = vec![Vec::new(); count];
        for edge in &self.edges {
            let (s, d) = (position(edge.src), position(edge.dst));
            successors[s].push(d);
            indegree[d] += 1;
        }
        let mut order: Vec<usize> = (0..count).filter(|&i| indegree[i] == 0).collect();
        let mut cursor = 0;
        while cursor < order.len() {
            let node = order[cursor];
            cursor += 1;
            for &next in &successors[node] {
                indegree[next] -= 1;
                if indegree[next] == 0 {
                    order.push(next);
                }
            }
        }
        debug_assert_eq!(order.len(), count, "cycle slipped past connect validation");

        // Assign each output port its own buffer; id 0 stays the shared silence buffer.
        let mut next_audio = 1u32;
        let mut next_event = 1u32;
        let mut audio_out_ids: Vec<Vec<u32>> = vec![Vec::new(); count];
        let mut event_out_ids: Vec<Vec<u32>> = vec![Vec::new(); count];
        for &node in &order {
            let counts = self.arena.counts(nodes[node]).unwrap();
            for _ in 0..counts.audio_out {
                audio_out_ids[node].push(next_audio);
                next_audio += 1;
            }
            for _ in 0..counts.event_out {
                event_out_ids[node].push(next_event);
                next_event += 1;
            }
        }

        let mut steps = Vec::with_capacity(count);
        for &node in &order {
            let key = nodes[node];
            let counts = self.arena.counts(key).unwrap();
            let mut audio_in = vec![0u32; counts.audio_in];
            let mut event_in = vec![0u32; counts.event_in];
            for edge in self.edges.iter().filter(|e| e.dst == key) {
                let src = position(edge.src);
                match edge.kind {
                    PortType::Audio => {
                        audio_in[edge.dst_port as usize] =
                            audio_out_ids[src][edge.src_port as usize];
                    }
                    PortType::Event => {
                        event_in[edge.dst_port as usize] =
                            event_out_ids[src][edge.src_port as usize];
                    }
                }
            }
            steps.push(Step {
                node: key,
                counts,
                audio_in,
                audio_out: audio_out_ids[node].clone(),
                event_in,
                event_out: event_out_ids[node].clone(),
            });
        }

        self.executor.ensure(next_audio, next_event);
        let compiled = Arc::new(Compiled { steps });
        let old = std::mem::replace(&mut self.active, compiled);
        self.chute.retire_compiled(old);

        // Now actually drop the removed nodes (via the chute) and forget them.
        for key in std::mem::take(&mut self.pending_remove) {
            if let Some(node) = self.arena.remove(key) {
                self.chute.retire_node(node);
            }
        }
    }

    /// Renders one block by iterating the active schedule.
    pub fn process(&mut self, ctx: &ProcessContext) {
        debug_assert!(
            ctx.frames <= self.executor.block,
            "frames exceed max block size"
        );
        let active = Arc::clone(&self.active);
        for step in &active.steps {
            self.executor.load_inputs(step, ctx.frames);
            {
                let mut io = self.executor.begin(step.counts, ctx.frames);
                self.arena.node_mut(step.node).process(ctx, &mut io);
            }
            self.executor.store_outputs(step, ctx.frames);
        }
    }

    /// Borrows a node as its concrete type `N`, if the handle is live and the type matches.
    #[must_use]
    pub fn node<N: Node>(&self, key: NodeKey) -> Option<&N> {
        (self.arena.node(key)? as &dyn Any).downcast_ref::<N>()
    }

    /// After [`process`](Graph::process), the samples on a node's audio output `port`. Returns a
    /// full block; take `[..frames]`. Lets the io layer pull the graph's result into a hardware
    /// buffer without exposing the buffer pool.
    #[must_use]
    pub fn output(&self, key: NodeKey, port: usize) -> Option<&[f32]> {
        let step = self.active.steps.iter().find(|s| s.node == key)?;
        let buffer = *step.audio_out.get(port)? as usize;
        let block = self.executor.block;
        Some(&self.executor.audio_pool[buffer * block..(buffer + 1) * block])
    }
}

mod nodes;
pub use nodes::{Capture, EventSourceNode, Gain, Sine};

#[cfg(test)]
mod tests;
