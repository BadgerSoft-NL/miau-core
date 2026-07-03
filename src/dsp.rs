//! The [`TimeStretcher`] contract, its registry, and the core's one built-in implementation,
//! [`RepitchStretcher`].
//!
//! This is the registry extension point (invariant 7): the core defines the contract and ships a
//! default, shells register alternatives (`paulstretch`, elastique, …) under string ids, and an
//! unknown id degrades gracefully to repitch with a warning — it never crashes. A clip carries a
//! [`Stretch`] reference (id + opaque settings); [`StretcherRegistry::resolve`] turns that
//! reference into a live stretcher.
//!
//! Stretching is an *offline / preparation-time* operation: [`TimeStretcher::stretch`] allocates
//! its output and must never run on the audio thread (invariant 11). The core has no sample-
//! playback node yet (disk streaming stays deferred — design-log note 06), so a stretcher is
//! exercised as a standalone buffer transform; a node that plays a stretched sample is future
//! work.

use std::collections::HashMap;
use std::fmt;

use serde_json::Value;

use crate::arrange::Stretch;

/// The string id of the core's built-in stretcher, used as the graceful fallback (invariant 7).
pub const REPITCH_ID: &str = "repitch";

/// The time-stretch contract: reshape a block of mono audio in time.
///
/// `stretch` maps `input` to roughly `input.len() * ratio` output frames. `ratio > 1` lengthens
/// (slows down), `ratio < 1` shortens (speeds up). Implementations decide what that does to pitch
/// — [`RepitchStretcher`] resamples, so time and pitch move together (the varispeed behaviour);
/// a pitch-preserving stretcher would hold pitch constant. Realtime-agnostic on purpose: this
/// runs off the audio thread (invariant 11).
pub trait TimeStretcher: Send {
    /// The registered id this stretcher was built for (for logging and round-tripping).
    fn id(&self) -> &str;

    /// Stretches `input` by `ratio`, returning a freshly allocated output buffer of about
    /// `input.len() * ratio` frames. Empty input or a non-positive `ratio` yields an empty buffer.
    fn stretch(&self, input: &[f32], ratio: f64) -> Vec<f32>;
}

/// A known stretcher rejected the settings it was handed.
///
/// Unlike [`crate::persist`]'s node registry — where a known builder given bad settings is a hard
/// error the caller must see, because substituting a node would silently lose routing — a bad
/// stretcher setting is *recoverable*: repitch is always an acoustically valid stand-in, so
/// [`StretcherRegistry::resolve`] turns this into a fallback + warning rather than propagating it.
#[derive(Debug)]
pub struct StretchError {
    id: String,
    reason: String,
}

impl StretchError {
    /// A settings-rejection error for stretcher `id` explaining `reason`.
    #[must_use]
    pub fn new(id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for StretchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "stretcher `{}`: {}", self.id, self.reason)
    }
}

impl std::error::Error for StretchError {}

/// Builds a stretcher from its opaque settings.
type Factory = Box<dyn Fn(&Value) -> Result<Box<dyn TimeStretcher>, StretchError> + Send + Sync>;

/// The core's built-in stretcher: a linear-interpolating resampler.
///
/// Repitch is the naive stretch — it plays the source faster or slower, so changing the length
/// changes the pitch (varispeed). It takes no settings. It is the graceful fallback for any
/// unknown or unbuildable stretcher id (invariant 7): resampling always produces valid audio.
#[derive(Clone, Copy, Debug, Default)]
pub struct RepitchStretcher;

impl TimeStretcher for RepitchStretcher {
    fn id(&self) -> &str {
        REPITCH_ID
    }

    fn stretch(&self, input: &[f32], ratio: f64) -> Vec<f32> {
        if input.is_empty() || ratio <= 0.0 {
            return Vec::new();
        }
        let out_len = (input.len() as f64 * ratio).round() as usize;
        let last = input.len() - 1;
        let mut out = Vec::with_capacity(out_len);
        for i in 0..out_len {
            // Where this output frame reads from in the source: the inverse of the time ratio.
            let src = i as f64 / ratio;
            let i0 = src.floor() as usize;
            if i0 >= last {
                out.push(input[last]); // hold the last sample rather than read past the end
            } else {
                let frac = (src - i0 as f64) as f32;
                out.push(input[i0] + (input[i0 + 1] - input[i0]) * frac);
            }
        }
        out
    }
}

/// The outcome of [`StretcherRegistry::resolve`]: a stretcher that is always usable, plus an
/// optional warning when the requested id could not be honoured and repitch was substituted.
///
/// The core does not log (it owns no UI); the caller — a shell — surfaces `warning` however it
/// likes (invariant 7's "fallback + warning").
pub struct Resolved {
    /// The stretcher to use. Never absent: an unresolved request falls back to repitch.
    pub stretcher: Box<dyn TimeStretcher>,
    /// Set when the requested stretcher was unknown or rejected its settings and repitch stood in.
    pub warning: Option<String>,
}

/// Maps stretcher ids to factories. [`with_builtins`](StretcherRegistry::with_builtins) registers
/// `"repitch"`; a shell adds its own before resolving a document's clips.
///
/// This is the mechanism behind invariant 7: string-id extension, compile-time (not runtime
/// plugins), with a guaranteed fallback so a missing stretcher never stops the music.
pub struct StretcherRegistry {
    factories: HashMap<String, Factory>,
}

impl StretcherRegistry {
    /// An empty registry — no stretcher registered, so every request falls back to repitch.
    #[must_use]
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// A registry with the core's built-in `"repitch"` stretcher registered.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(REPITCH_ID, |_settings| Ok(Box::new(RepitchStretcher)));
        registry
    }

    /// Registers `factory` under `id`, replacing any previous factory for it.
    pub fn register(
        &mut self,
        id: impl Into<String>,
        factory: impl Fn(&Value) -> Result<Box<dyn TimeStretcher>, StretchError> + Send + Sync + 'static,
    ) {
        self.factories.insert(id.into(), Box::new(factory));
    }

    /// Whether a factory is registered for `id`.
    #[must_use]
    pub fn knows(&self, id: &str) -> bool {
        self.factories.contains_key(id)
    }

    /// Resolves a clip's [`Stretch`] reference to a live stretcher, always succeeding.
    ///
    /// `None` (a clip with no stretch) resolves to repitch with no warning. An id with a registered
    /// factory that accepts its settings resolves to that stretcher with no warning. An **unknown**
    /// id, or a known factory that **rejects** its settings, resolves to repitch and carries a
    /// warning describing what was substituted (invariant 7).
    #[must_use]
    pub fn resolve(&self, stretch: Option<&Stretch>) -> Resolved {
        let Some(stretch) = stretch else {
            return Resolved {
                stretcher: Box::new(RepitchStretcher),
                warning: None,
            };
        };
        match self.factories.get(&stretch.id) {
            None => Resolved {
                stretcher: Box::new(RepitchStretcher),
                warning: Some(format!(
                    "unknown time-stretcher `{}`; falling back to `{REPITCH_ID}`",
                    stretch.id
                )),
            },
            Some(factory) => match factory(&stretch.settings) {
                Ok(stretcher) => Resolved {
                    stretcher,
                    warning: None,
                },
                Err(err) => Resolved {
                    stretcher: Box::new(RepitchStretcher),
                    warning: Some(format!("{err}; falling back to `{REPITCH_ID}`")),
                },
            },
        }
    }
}

impl Default for StretcherRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_know_repitch() {
        let registry = StretcherRegistry::with_builtins();
        assert!(registry.knows(REPITCH_ID));
        assert!(!registry.knows("paulstretch"));
        // An empty registry knows nothing but still resolves (to repitch).
        assert!(!StretcherRegistry::new().knows(REPITCH_ID));
    }

    #[test]
    fn repitch_doubles_length_by_interpolation() {
        let out = RepitchStretcher.stretch(&[0.0, 1.0, 2.0, 3.0], 2.0);
        // Linear interpolation at the half-frame positions; the tail holds the last sample.
        assert_eq!(out, vec![0.0, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.0]);
    }

    #[test]
    fn repitch_identity_ratio_returns_input() {
        let input = [0.0, 1.0, 2.0, 3.0];
        assert_eq!(RepitchStretcher.stretch(&input, 1.0), input);
    }

    #[test]
    fn repitch_compression_and_edges() {
        assert_eq!(
            RepitchStretcher.stretch(&[0.0, 1.0, 2.0, 3.0], 0.5),
            vec![0.0, 2.0]
        );
        // Degenerate inputs never panic and never allocate output.
        assert!(RepitchStretcher.stretch(&[], 2.0).is_empty());
        assert!(RepitchStretcher.stretch(&[1.0, 2.0], 0.0).is_empty());
        assert!(RepitchStretcher.stretch(&[1.0, 2.0], -1.0).is_empty());
    }

    #[test]
    fn no_stretch_resolves_to_repitch_without_warning() {
        let resolved = StretcherRegistry::with_builtins().resolve(None);
        assert_eq!(resolved.stretcher.id(), REPITCH_ID);
        assert!(resolved.warning.is_none());
    }

    #[test]
    fn unknown_id_falls_back_to_repitch_with_warning() {
        // A clip referencing a stretcher this build doesn't have: loads with a warning, plays
        // via repitch (the done-when for phase 7).
        let clip_stretch = Stretch {
            id: "paulstretch".into(),
            settings: Value::Null,
        };
        let resolved = StretcherRegistry::with_builtins().resolve(Some(&clip_stretch));
        assert_eq!(resolved.stretcher.id(), REPITCH_ID);
        let warning = resolved.warning.expect("unknown id must warn");
        assert!(warning.contains("paulstretch") && warning.contains(REPITCH_ID));
        // And it actually stretches.
        assert_eq!(
            resolved.stretcher.stretch(&[0.0, 1.0], 2.0),
            vec![0.0, 0.5, 1.0, 1.0]
        );
    }

    #[test]
    fn registered_stretcher_is_used_over_the_fallback() {
        // A trivial stand-in stretcher, distinguishable from repitch by its all-zero output.
        struct Silence;
        impl TimeStretcher for Silence {
            fn id(&self) -> &str {
                "silence"
            }
            fn stretch(&self, input: &[f32], ratio: f64) -> Vec<f32> {
                vec![0.0; (input.len() as f64 * ratio).round() as usize]
            }
        }

        let mut registry = StretcherRegistry::with_builtins();
        registry.register("silence", |_settings| Ok(Box::new(Silence)));
        assert!(registry.knows("silence"));

        let stretch = Stretch {
            id: "silence".into(),
            settings: Value::Null,
        };
        let resolved = registry.resolve(Some(&stretch));
        assert!(resolved.warning.is_none());
        assert_eq!(resolved.stretcher.id(), "silence");
        assert_eq!(
            resolved.stretcher.stretch(&[1.0, 2.0, 3.0, 4.0], 0.5),
            vec![0.0, 0.0]
        );
    }

    #[test]
    fn known_stretcher_with_bad_settings_falls_back_with_warning() {
        // A stretcher that insists on a settings object; hand it null and it rejects.
        let mut registry = StretcherRegistry::new();
        registry.register("picky", |settings| {
            if settings.is_object() {
                Ok(Box::new(RepitchStretcher))
            } else {
                Err(StretchError::new("picky", "expected a settings object"))
            }
        });

        let stretch = Stretch {
            id: "picky".into(),
            settings: Value::Null,
        };
        let resolved = registry.resolve(Some(&stretch));
        assert_eq!(resolved.stretcher.id(), REPITCH_ID);
        let warning = resolved.warning.expect("rejected settings must warn");
        assert!(warning.contains("picky") && warning.contains("settings object"));
    }
}
