//! The `project.miau` ZIP container and the `.miauclip` clip-exchange format.
//!
//! A container is a plain ZIP (design log note 10 §2): `core.json` is the [`Document`], `blobs/`
//! holds raw CLAP state keyed by [`NodeId`], `samples/` holds embedded audio (referenced by a
//! relative path and BLAKE3 hash), and `shell/` holds opaque per-shell sections. Anything a reader
//! doesn't recognise — foreign shell sections, unknown top-level entries — is carried through
//! untouched on save (invariant 14). Binary payloads are raw ZIP entries, never base64-in-JSON.

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::registry::NodeRegistry;
use super::{Document, FORMAT, Instantiated, NodeId, PersistError, VERSION};
use crate::arrange::{Clip, SampleRef};

const CORE_JSON: &str = "core.json";
const BLOBS_DIR: &str = "blobs/";
const BLOB_EXT: &str = ".state";
const SAMPLES_DIR: &str = "samples/";
const SHELL_DIR: &str = "shell/";
const SHELL_EXT: &str = ".json";
const CLIP_JSON: &str = "clip.json";

/// The BLAKE3 hash of `bytes`, as raw 32 bytes — the content address used by [`SampleRef`].
#[must_use]
pub fn hash(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

/// A loaded or in-memory `project.miau`: a [`Document`] plus the container's binary side-cars.
///
/// The maps are ordered so a save is reproducible. Losslessness (invariant 14) is structural:
/// every entry that isn't `core.json` is kept verbatim and re-emitted, so opening and re-saving a
/// foreign shell's project never damages it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Project {
    /// The core document.
    pub document: Document,
    /// Raw CLAP state blobs, keyed by the owning node's [`NodeId`] (`blobs/<id>.state`).
    pub blobs: BTreeMap<NodeId, Vec<u8>>,
    /// Embedded samples, keyed by their container-relative path (e.g. `samples/kick.wav`).
    pub samples: BTreeMap<String, Vec<u8>>,
    /// Opaque per-shell sections, keyed by shell id (`shell/<id>.json`) — never interpreted.
    pub shells: BTreeMap<String, Vec<u8>>,
    /// Any other ZIP entry, kept verbatim so nothing is lost on round trip (invariant 14).
    pub extra_entries: BTreeMap<String, Vec<u8>>,
}

impl Project {
    /// A project wrapping `document` with no side-cars.
    #[must_use]
    pub fn new(document: Document) -> Self {
        Self {
            document,
            ..Self::default()
        }
    }

    /// Embeds `bytes` under `path` (e.g. `"samples/kick.wav"`) and returns a content-addressed
    /// [`SampleRef`] pointing at them (design log note 10 §5).
    pub fn embed_sample(&mut self, path: impl Into<String>, bytes: Vec<u8>) -> SampleRef {
        let path = path.into();
        let sample = SampleRef {
            path: path.clone(),
            hash: hash(&bytes),
        };
        self.samples.insert(path, bytes);
        sample
    }

    /// Rebuilds a runtime graph from the project's document, feeding each node its blob
    /// (invariant 15). See [`Document::instantiate`](crate::persist::Document::instantiate).
    ///
    /// # Errors
    /// [`PersistError`] as for [`Document::instantiate`](crate::persist::Document::instantiate).
    pub fn instantiate(
        &self,
        registry: &NodeRegistry,
        sample_rate: u32,
        max_block: usize,
    ) -> Result<Instantiated, PersistError> {
        self.document
            .instantiate(registry, sample_rate, max_block, &self.blobs)
    }

    /// Verifies every embedded sample referenced by a clip matches its stored BLAKE3 hash.
    ///
    /// # Errors
    /// [`PersistError::HashMismatch`] for the first sample whose bytes don't match its reference.
    pub fn verify_samples(&self) -> Result<(), PersistError> {
        for lane in &self.document.lanes {
            for placement in lane.placements() {
                if let Some(sample) = placement.clip().sample() {
                    if let Some(bytes) = self.samples.get(&sample.path) {
                        if hash(bytes) != sample.hash {
                            return Err(PersistError::HashMismatch {
                                path: sample.path.clone(),
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Reads a `project.miau` container from `bytes`.
    ///
    /// # Errors
    /// [`PersistError::Format`] if `core.json` is missing or not a `miau-core` document;
    /// [`PersistError::Zip`]/[`PersistError::Json`] on malformed input.
    pub fn read(bytes: &[u8]) -> Result<Self, PersistError> {
        let mut archive = ZipArchive::new(Cursor::new(bytes))?;
        let mut project = Self::default();
        let mut seen_core = false;

        for i in 0..archive.len() {
            let mut file = archive.by_index(i)?;
            if file.is_dir() {
                continue;
            }
            let name = file.name().to_owned();
            let mut data = Vec::new();
            file.read_to_end(&mut data)?;

            if name == CORE_JSON {
                project.document = serde_json::from_slice(&data)?;
                if project.document.format != FORMAT {
                    return Err(PersistError::Format(format!(
                        "not a miau-core document (format = {:?})",
                        project.document.format
                    )));
                }
                seen_core = true;
            } else if let Some(id) = blob_id(&name) {
                project.blobs.insert(id, data);
            } else if let Some(rest) = name.strip_prefix(SHELL_DIR).and_then(shell_id) {
                project.shells.insert(rest, data);
            } else if name.starts_with(SAMPLES_DIR) {
                project.samples.insert(name, data);
            } else {
                // Unknown entry: preserve it verbatim (invariant 14).
                project.extra_entries.insert(name, data);
            }
        }

        if !seen_core {
            return Err(PersistError::Format("missing core.json".to_owned()));
        }
        Ok(project)
    }

    /// Writes the container to `project.miau` bytes. Entries are emitted in a fixed order so the
    /// output is reproducible.
    ///
    /// # Errors
    /// [`PersistError::Json`]/[`PersistError::Zip`] if serialization or ZIP writing fails.
    pub fn write(&self) -> Result<Vec<u8>, PersistError> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        let core = serde_json::to_vec_pretty(&self.document)?;
        write_entry(&mut zip, CORE_JSON, &core, options)?;
        for (id, blob) in &self.blobs {
            write_entry(
                &mut zip,
                &format!("{BLOBS_DIR}{}{BLOB_EXT}", id.0),
                blob,
                options,
            )?;
        }
        for (path, bytes) in &self.samples {
            write_entry(&mut zip, path, bytes, options)?;
        }
        for (shell_id, bytes) in &self.shells {
            write_entry(
                &mut zip,
                &format!("{SHELL_DIR}{shell_id}{SHELL_EXT}"),
                bytes,
                options,
            )?;
        }
        for (name, bytes) in &self.extra_entries {
            write_entry(&mut zip, name, bytes, options)?;
        }

        Ok(zip.finish()?.into_inner())
    }
}

/// The `.miauclip` cross-shell exchange unit: one [`Clip`] plus any samples it embeds.
///
/// The clip scope is deliberate — arrangement structure diverges too far between shells to move
/// wholesale, but a clip (events in ticks, length, loop, stretcher id + settings, optional sample)
/// is portable (design log note 10 §6). Because clips carry no document-relative ids — events are
/// tick-relative and samples are content-addressed — importing one into another document needs no
/// id remapping; a shell simply allocates a placement in the target lane.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MiauClip {
    /// The exchanged clip.
    pub clip: Clip,
    /// Samples embedded alongside the clip, keyed by container-relative path.
    pub samples: BTreeMap<String, Vec<u8>>,
    /// Any other entry, preserved verbatim (invariant 14).
    pub extra_entries: BTreeMap<String, Vec<u8>>,
}

/// The `clip.json` envelope: the format tag plus the clip, mirroring `core.json`.
#[derive(Serialize, Deserialize)]
struct ClipEnvelope {
    format: String,
    version: u32,
    clip: Clip,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl MiauClip {
    /// A `.miauclip` carrying `clip` with no embedded samples.
    #[must_use]
    pub fn new(clip: Clip) -> Self {
        Self {
            clip,
            ..Self::default()
        }
    }

    /// Reads a `.miauclip` from `bytes`.
    ///
    /// # Errors
    /// [`PersistError::Format`] if `clip.json` is missing or not a `miau-core` clip.
    pub fn read(bytes: &[u8]) -> Result<Self, PersistError> {
        let mut archive = ZipArchive::new(Cursor::new(bytes))?;
        let mut clip = None;
        let mut samples = BTreeMap::new();
        let mut extra_entries = BTreeMap::new();

        for i in 0..archive.len() {
            let mut file = archive.by_index(i)?;
            if file.is_dir() {
                continue;
            }
            let name = file.name().to_owned();
            let mut data = Vec::new();
            file.read_to_end(&mut data)?;

            if name == CLIP_JSON {
                let envelope: ClipEnvelope = serde_json::from_slice(&data)?;
                if envelope.format != FORMAT {
                    return Err(PersistError::Format(format!(
                        "not a miau-core clip (format = {:?})",
                        envelope.format
                    )));
                }
                clip = Some(envelope.clip);
            } else if name.starts_with(SAMPLES_DIR) {
                samples.insert(name, data);
            } else {
                extra_entries.insert(name, data);
            }
        }

        let clip = clip.ok_or_else(|| PersistError::Format("missing clip.json".to_owned()))?;
        Ok(Self {
            clip,
            samples,
            extra_entries,
        })
    }

    /// Writes the `.miauclip` bytes.
    ///
    /// # Errors
    /// [`PersistError::Json`]/[`PersistError::Zip`] if serialization or ZIP writing fails.
    pub fn write(&self) -> Result<Vec<u8>, PersistError> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        let envelope = ClipEnvelope {
            format: FORMAT.to_owned(),
            version: VERSION,
            clip: self.clip.clone(),
            extra: Map::new(),
        };
        let json = serde_json::to_vec_pretty(&envelope)?;
        write_entry(&mut zip, CLIP_JSON, &json, options)?;
        for (path, bytes) in &self.samples {
            write_entry(&mut zip, path, bytes, options)?;
        }
        for (name, bytes) in &self.extra_entries {
            write_entry(&mut zip, name, bytes, options)?;
        }
        Ok(zip.finish()?.into_inner())
    }
}

/// Writes one ZIP entry, translating the error into a [`PersistError`].
fn write_entry(
    zip: &mut ZipWriter<Cursor<Vec<u8>>>,
    name: &str,
    bytes: &[u8],
    options: SimpleFileOptions,
) -> Result<(), PersistError> {
    zip.start_file(name, options)?;
    zip.write_all(bytes)?;
    Ok(())
}

/// Parses `blobs/<id>.state` into its [`NodeId`], or `None` if the name isn't a well-formed blob.
fn blob_id(name: &str) -> Option<NodeId> {
    name.strip_prefix(BLOBS_DIR)?
        .strip_suffix(BLOB_EXT)?
        .parse()
        .ok()
        .map(NodeId)
}

/// Extracts the shell id from a `<id>.json` entry name (already stripped of the `shell/` prefix).
fn shell_id(rest: &str) -> Option<String> {
    // Only accept a flat `shell/<id>.json`; nested paths fall through to `extra_entries`.
    if rest.contains('/') {
        return None;
    }
    rest.strip_suffix(SHELL_EXT).map(str::to_owned)
}
