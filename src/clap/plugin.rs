//! The main-thread face of a CLAP plugin: the [`ClapPlugin`] that owns the `!Send`
//! [`PluginInstance`], plus [`ClapPlugin::open`] which instantiates and activates a plugin and hands
//! back the paired [`ClapNode`] for the graph.
//!
//! Instantiation and activation happen here, on the main thread, never on the audio thread; a plugin
//! enters the graph only once activated. This handle stays alive for GUI, parameter enumeration and
//! opaque state blobs. Teardown is by drop: the [`ClapNode`]'s audio processor and this handle each
//! hold an `Arc` to the shared instance, and whichever drops last performs the plugin's
//! deactivate + destroy — so retiring the node through the graph's garbage chute deactivates off the
//! audio thread (invariant 11). Drop this handle once the node has been removed from the graph.

use std::fmt;
use std::io::Cursor;

use clack_extensions::audio_ports::{AudioPortInfoBuffer, PluginAudioPorts};
use clack_extensions::params::{ParamInfoBuffer, PluginParams};
use clack_extensions::state::PluginState;
use clack_host::entry::{PluginEntry, PluginEntryError};
use clack_host::prelude::{PluginAudioConfiguration, PluginInstance, PluginInstanceError};

use super::discovery::PluginInfo;
use super::host::{MiauHost, MiauShared, host_info};
use super::node::ClapNode;

/// Why [`ClapPlugin::open`] failed.
#[derive(Debug)]
pub enum ClapError {
    /// The `.clap` bundle could not be loaded.
    Load(PluginEntryError),
    /// The bundle exposes no plugin factory.
    NoFactory,
    /// No plugin with the requested id lives in the bundle.
    NoSuchPlugin,
    /// Instantiating the plugin failed.
    Instantiate(PluginInstanceError),
    /// Activating the plugin for audio processing failed.
    Activate(PluginInstanceError),
}

impl fmt::Display for ClapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(e) => write!(f, "failed to load CLAP bundle: {e}"),
            Self::NoFactory => f.write_str("CLAP bundle exposes no plugin factory"),
            Self::NoSuchPlugin => f.write_str("no plugin with that id in the bundle"),
            Self::Instantiate(e) => write!(f, "failed to instantiate plugin: {e}"),
            Self::Activate(e) => write!(f, "failed to activate plugin: {e}"),
        }
    }
}

impl std::error::Error for ClapError {}

/// One parameter of a hosted plugin, as enumerated by [`ClapPlugin::params`].
#[derive(Clone, Debug, PartialEq)]
pub struct ParamDesc {
    /// Stable CLAP parameter id — use it as the `param` of an
    /// [`Event::ParamChange`](crate::event::Event::ParamChange) to automate this parameter.
    pub id: u32,
    /// Display name, e.g. "Volume".
    pub name: String,
    /// Minimum plain value.
    pub min: f64,
    /// Maximum plain value.
    pub max: f64,
    /// Current plain value (falls back to the default if the plugin cannot report it).
    pub value: f64,
}

/// The main-thread half of a hosted CLAP plugin. Owns the `!Send` [`PluginInstance`]; the paired
/// [`ClapNode`] returned by [`open`](ClapPlugin::open) owns the `Send` audio processor.
pub struct ClapPlugin {
    instance: PluginInstance<MiauHost>,
    // The instance clones the entry internally to keep the DLL loaded; holding it here too makes
    // the ownership explicit and keeps the bundle alive for as long as this handle.
    _entry: PluginEntry,
}

impl ClapPlugin {
    /// Instantiates and activates the plugin described by `info`, returning the main-thread handle
    /// and the graph [`ClapNode`] to insert into the audio graph.
    ///
    /// `sample_rate` and `max_block` must match the graph the node will run in. The plugin is
    /// activated with `min_frames_count` 1 (some plugins reject 0) and `max_frames_count` `max_block`.
    ///
    /// # Errors
    /// See [`ClapError`].
    pub fn open(
        info: &PluginInfo,
        sample_rate: u32,
        max_block: usize,
    ) -> Result<(Self, ClapNode), ClapError> {
        // SAFETY: the path came from discovery, i.e. a `.clap` in a sanctioned CLAP directory.
        let entry = unsafe { PluginEntry::load(&info.path) }.map_err(ClapError::Load)?;
        let factory = entry.get_plugin_factory().ok_or(ClapError::NoFactory)?;
        let descriptor = factory
            .plugin_descriptors()
            .find(|d| d.id().map(|c| c.to_bytes()) == Some(info.id.as_bytes()))
            .ok_or(ClapError::NoSuchPlugin)?;
        let id = descriptor.id().ok_or(ClapError::NoSuchPlugin)?;
        let host = host_info();

        let mut instance =
            PluginInstance::<MiauHost>::new(|_| MiauShared, |_| (), &entry, id, &host)
                .map_err(ClapError::Instantiate)?;

        let channels = output_channel_count(&mut instance);

        let config = PluginAudioConfiguration {
            sample_rate: f64::from(sample_rate),
            min_frames_count: 1,
            max_frames_count: max_block as u32,
        };
        let stopped = instance
            .activate(|_, _| (), config)
            .map_err(ClapError::Activate)?;

        let node = ClapNode::new(stopped.into(), channels, max_block);
        Ok((
            Self {
                instance,
                _entry: entry,
            },
            node,
        ))
    }

    /// Enumerates the plugin's parameters (id, name, range and current value), or an empty list if
    /// the plugin exposes no params extension.
    ///
    /// Parameters are automated through the ordinary event stream (invariant 13): feed the graph an
    /// [`Event::ParamChange`](crate::event::Event::ParamChange) carrying a [`ParamDesc::id`] and the
    /// receiving [`ClapNode`] turns it into a CLAP `param_value` event.
    #[must_use]
    pub fn params(&mut self) -> Vec<ParamDesc> {
        let Some(params) = self
            .instance
            .plugin_handle()
            .get_extension::<PluginParams>()
        else {
            return Vec::new();
        };
        let mut handle = self.instance.plugin_handle();
        let count = params.count(&mut handle);
        let mut buffer = ParamInfoBuffer::new();
        let mut out = Vec::with_capacity(count as usize);
        for index in 0..count {
            let Some(info) = params.get_info(&mut handle, index, &mut buffer) else {
                continue;
            };
            let id = info.id;
            out.push(ParamDesc {
                id: id.get(),
                name: String::from_utf8_lossy(info.name).into_owned(),
                min: info.min_value,
                max: info.max_value,
                value: params
                    .get_value(&mut handle, id)
                    .unwrap_or(info.default_value),
            });
        }
        out
    }

    /// Saves the plugin's state to an opaque blob, or `None` if it exposes no state extension.
    ///
    /// The blob is preserved verbatim by persistence and round-trips through a
    /// get → destroy → recreate → [`load_state`](Self::load_state) cycle (invariant 14).
    #[must_use]
    pub fn save_state(&mut self) -> Option<Vec<u8>> {
        let state = self
            .instance
            .plugin_handle()
            .get_extension::<PluginState>()?;
        let mut blob = Vec::new();
        state
            .save(&mut self.instance.plugin_handle(), &mut blob)
            .ok()?;
        Some(blob)
    }

    /// Loads a previously [saved](Self::save_state) state blob. Returns `false` if the plugin
    /// exposes no state extension or rejects the blob.
    pub fn load_state(&mut self, blob: &[u8]) -> bool {
        let Some(state) = self.instance.plugin_handle().get_extension::<PluginState>() else {
            return false;
        };
        let mut cursor = Cursor::new(blob);
        state
            .load(&mut self.instance.plugin_handle(), &mut cursor)
            .is_ok()
    }
}

/// The channel count of the plugin's main audio output port, defaulting to stereo when the plugin
/// does not expose the audio-ports extension, and 0 for a plugin with no audio output (a MIDI FX).
fn output_channel_count(instance: &mut PluginInstance<MiauHost>) -> usize {
    let Some(ports) = instance.plugin_handle().get_extension::<PluginAudioPorts>() else {
        return 2;
    };
    let mut handle = instance.plugin_handle();
    if ports.count(&mut handle, false) == 0 {
        return 0;
    }
    let mut buffer = AudioPortInfoBuffer::new();
    match ports.get(&mut handle, 0, false, &mut buffer) {
        Some(info) => info.channel_count as usize,
        None => 2,
    }
}
