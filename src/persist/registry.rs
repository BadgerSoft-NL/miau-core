//! The node registry: the compile-time extension point that turns a persisted `(kind, settings)`
//! back into a live [`Node`] (invariant 7 — the core ships defaults, shells register alternatives
//! under string ids, unknown ids fall back gracefully).

use std::collections::HashMap;
use std::fmt;

use serde::Deserialize;
use serde_json::Value;

use crate::graph::{Gain, Node, Sine};

/// A factory that rebuilds a node from its opaque settings.
type Builder = Box<dyn Fn(&Value) -> Result<Box<dyn Node>, BuildError> + Send + Sync>;

/// A registered `kind` rejected the settings it was handed.
///
/// This is distinct from an *unknown* kind: an unknown kind degrades to a
/// [`Placeholder`](crate::graph::Placeholder) (invariant 7), whereas a known builder given
/// malformed settings is a hard error the caller must see.
#[derive(Debug)]
pub struct BuildError {
    kind: String,
    reason: String,
}

impl BuildError {
    /// A build error for `kind` explaining `reason`.
    #[must_use]
    pub fn new(kind: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "node kind `{}`: {}", self.kind, self.reason)
    }
}

impl std::error::Error for BuildError {}

/// Maps registry type-ids to node factories.
///
/// [`with_builtins`](NodeRegistry::with_builtins) registers the core's own nodes; a shell adds its
/// nodes (and its CLAP factory) with [`register`](NodeRegistry::register) before loading a
/// document. A `kind` with no factory is not an error — the loader stands it in with a
/// [`Placeholder`](crate::graph::Placeholder).
pub struct NodeRegistry {
    builders: HashMap<String, Builder>,
}

impl NodeRegistry {
    /// An empty registry — no kinds known, so every node loads as a placeholder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            builders: HashMap::new(),
        }
    }

    /// A registry with the core's built-in nodes registered: `"sine"` and `"gain"`.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register("sine", |settings| {
            let s = SineSettings::parse(settings)?;
            Ok(Box::new(Sine::new(s.freq)))
        });
        registry.register("gain", |settings| {
            let s = GainSettings::parse(settings)?;
            Ok(Box::new(Gain::new(s.gain)))
        });
        registry
    }

    /// Registers `builder` under `kind`, replacing any previous factory for it.
    pub fn register(
        &mut self,
        kind: impl Into<String>,
        builder: impl Fn(&Value) -> Result<Box<dyn Node>, BuildError> + Send + Sync + 'static,
    ) {
        self.builders.insert(kind.into(), Box::new(builder));
    }

    /// Whether a factory is registered for `kind`.
    #[must_use]
    pub fn knows(&self, kind: &str) -> bool {
        self.builders.contains_key(kind)
    }

    /// Builds a node for `kind` from `settings`. Returns `None` when `kind` is unknown (the caller
    /// makes a placeholder), `Some(Err)` when a known builder rejects the settings.
    pub(super) fn build(
        &self,
        kind: &str,
        settings: &Value,
    ) -> Option<Result<Box<dyn Node>, BuildError>> {
        self.builders.get(kind).map(|builder| builder(settings))
    }
}

impl Default for NodeRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

/// `null` settings deserialize as the type's `Default`, so a node saved with no settings rebuilds
/// at its defaults instead of erroring.
fn parse_settings<'a, T: Deserialize<'a> + Default>(
    kind: &str,
    settings: &'a Value,
) -> Result<T, BuildError> {
    if settings.is_null() {
        return Ok(T::default());
    }
    T::deserialize(settings).map_err(|e| BuildError::new(kind, e.to_string()))
}

#[derive(Deserialize)]
struct SineSettings {
    freq: f64,
}

impl Default for SineSettings {
    fn default() -> Self {
        Self { freq: 440.0 }
    }
}

impl SineSettings {
    fn parse(settings: &Value) -> Result<Self, BuildError> {
        parse_settings("sine", settings)
    }
}

#[derive(Deserialize)]
struct GainSettings {
    gain: f32,
}

impl Default for GainSettings {
    fn default() -> Self {
        Self { gain: 1.0 }
    }
}

impl GainSettings {
    fn parse(settings: &Value) -> Result<Self, BuildError> {
        parse_settings("gain", settings)
    }
}
