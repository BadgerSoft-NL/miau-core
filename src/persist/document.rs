//! The [`Document`] — the serde model of a piece of music — and [`Document::instantiate`], which
//! rebuilds a runtime [`Graph`] from it (invariant 15: the arena is derived, never persisted).

use std::collections::BTreeMap;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::registry::NodeRegistry;
use super::{FORMAT, NodeId, PersistError, VERSION};
use crate::arrange::Lane;
use crate::graph::{Graph, NodeKey, Placeholder, PortCounts, PortType};
use crate::time::{TempoMap, TempoSegment};

/// One node in the persisted topology: a stable [`NodeId`], a registry `kind`, its opaque
/// `settings`, and the port layout needed to preserve connections if the `kind` can't be rebuilt.
///
/// `settings` is `kind`-specific JSON interpreted by the registry builder; `extra` catches any
/// unknown fields so a foreign or future writer's additions survive a round trip (invariant 14).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeSpec {
    /// Stable document identity (invariant 15).
    pub id: NodeId,
    /// Registry type-id selecting how to rebuild the node (e.g. `"sine"`, or a CLAP plugin id).
    pub kind: String,
    /// Builder-specific settings, carried through untouched when unknown.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub settings: Value,
    /// Port layout — lets a [`Placeholder`] preserve the node's connections when `kind` is missing.
    pub ports: PortCounts,
    /// Unknown fields, preserved verbatim (invariant 14).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A typed edge in the persisted topology: `from`'s output port to `to`'s input port
/// (invariant 13 — audio to audio, events to events).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    /// Which port kind this edge connects.
    pub kind: PortType,
    /// Source node.
    pub from: NodeId,
    /// Source output-port index.
    pub from_port: u32,
    /// Destination node.
    pub to: NodeId,
    /// Destination input-port index.
    pub to_port: u32,
}

/// The persisted description of a piece of music (design log note 10).
///
/// Holds the musical content and routing but nothing the engine derives at runtime (playhead,
/// buffers, schedules — invariant 15). Shell conventions (track names, mixer) live in the
/// container's `shell/` sections, not here. Unknown top-level fields land in `extra` and are
/// re-emitted on save (invariant 14).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Document {
    /// Format tag; always [`FORMAT`] for a core document.
    pub format: String,
    /// Format version ([`VERSION`]).
    pub version: u32,
    /// Next id the monotonic counter will hand out (invariant 15; design log note 10 §4).
    #[serde(default)]
    pub next_node_id: u64,
    /// Stepwise tempo map input; the compiled [`TempoMap`] is rebuilt on load (invariant 9).
    #[serde(default)]
    pub tempo: Vec<TempoSegment>,
    /// Timeline content.
    #[serde(default)]
    pub lanes: Vec<Lane>,
    /// Graph nodes.
    #[serde(default)]
    pub nodes: Vec<NodeSpec>,
    /// Graph edges.
    #[serde(default)]
    pub connections: Vec<Connection>,
    /// Unknown fields, preserved verbatim (invariant 14).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            format: FORMAT.to_owned(),
            version: VERSION,
            next_node_id: 0,
            tempo: Vec::new(),
            lanes: Vec::new(),
            nodes: Vec::new(),
            connections: Vec::new(),
            extra: Map::new(),
        }
    }
}

impl Document {
    /// An empty `miau-core` document at the current [`VERSION`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocates the next stable [`NodeId`] from the document's monotonic counter (invariant 15).
    /// Ids are never reused, so a connection can outlive the node it names without aliasing a new
    /// one.
    pub fn allocate_id(&mut self) -> NodeId {
        let id = NodeId(self.next_node_id);
        self.next_node_id += 1;
        id
    }

    /// Adds a node of registry `kind` with `settings` and `ports`, allocating and returning its id.
    pub fn add_node(
        &mut self,
        kind: impl Into<String>,
        settings: Value,
        ports: PortCounts,
    ) -> NodeId {
        let id = self.allocate_id();
        self.nodes.push(NodeSpec {
            id,
            kind: kind.into(),
            settings,
            ports,
            extra: Map::new(),
        });
        id
    }

    /// True if this document declares itself a `miau-core` document the reader understands.
    #[must_use]
    pub fn is_supported(&self) -> bool {
        self.format == FORMAT && self.version <= VERSION
    }

    /// Rebuilds a runtime [`Graph`] from the document (invariant 15).
    ///
    /// Each node is rebuilt through `registry`; an unknown `kind` degrades to a
    /// [`Placeholder`] carrying the node's `blobs` entry and port layout (invariants 7 & 14) and
    /// its id is reported in [`Instantiated::missing`]. The tempo map is compiled for
    /// `sample_rate`, and the graph is sized for `max_block`-frame blocks and committed ready to
    /// render. `blobs` supplies opaque per-node state (typically from the container); pass an
    /// empty map when there is none.
    ///
    /// # Errors
    /// [`PersistError`] if the tempo map is invalid, a known builder rejects its settings, or a
    /// connection is impossible in the rebuilt graph.
    pub fn instantiate(
        &self,
        registry: &NodeRegistry,
        sample_rate: u32,
        max_block: usize,
        blobs: &BTreeMap<NodeId, Vec<u8>>,
    ) -> Result<Instantiated, PersistError> {
        let tempo = if self.tempo.is_empty() {
            None
        } else {
            Some(TempoMap::new(sample_rate, &self.tempo).map_err(PersistError::Tempo)?)
        };

        let mut graph = Graph::new(max_block);
        let mut keys: HashMap<NodeId, NodeKey> = HashMap::with_capacity(self.nodes.len());
        let mut missing = Vec::new();

        for spec in &self.nodes {
            let key = match registry.build(&spec.kind, &spec.settings) {
                Some(result) => graph.add_node_boxed(result.map_err(PersistError::Build)?),
                None => {
                    // Unknown kind: stand in with a placeholder that preserves the blob and shape.
                    let blob = blobs.get(&spec.id).cloned().unwrap_or_default();
                    missing.push(spec.id);
                    graph.add_node(Placeholder::new(spec.kind.clone(), blob, spec.ports))
                }
            };
            keys.insert(spec.id, key);
        }

        for edge in &self.connections {
            let from = *keys
                .get(&edge.from)
                .ok_or(PersistError::UnknownNodeRef(edge.from))?;
            let to = *keys
                .get(&edge.to)
                .ok_or(PersistError::UnknownNodeRef(edge.to))?;
            let result = match edge.kind {
                PortType::Audio => graph.connect_audio((from, edge.from_port), (to, edge.to_port)),
                PortType::Event => graph.connect_event((from, edge.from_port), (to, edge.to_port)),
            };
            result.map_err(PersistError::Connect)?;
        }

        graph.commit();
        Ok(Instantiated {
            graph,
            tempo,
            keys,
            missing,
        })
    }
}

/// The runtime products of [`Document::instantiate`]: a committed graph plus the maps to drive it.
pub struct Instantiated {
    /// The rebuilt, committed graph, ready to [`process`](Graph::process).
    pub graph: Graph,
    /// The compiled tempo map, if the document carried one.
    pub tempo: Option<TempoMap>,
    /// Document [`NodeId`] → runtime [`NodeKey`], for locating nodes after the rebuild.
    pub keys: HashMap<NodeId, NodeKey>,
    /// Nodes whose `kind` was unknown and became a [`Placeholder`] (invariant 7).
    pub missing: Vec<NodeId>,
}
