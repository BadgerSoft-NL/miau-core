//! CLAP host tests. The instantiation test is gated on a plugin actually being installed, so CI
//! (which has none) skips it while a developer machine with e.g. Surge XT exercises the full path.

use std::io::Cursor;
use std::path::PathBuf;

use clack_extensions::state::PluginState;
use clack_host::entry::PluginEntry;
use clack_host::prelude::{PluginAudioConfiguration, PluginInstance};

use super::host::{MiauHost, MiauShared, host_info};
use super::{PluginInfo, scan, search_paths};

#[test]
fn search_paths_include_the_standard_clap_dirs() {
    assert!(search_paths().contains(&PathBuf::from("/usr/lib/clap")));
}

/// Prefer the Surge XT synth if present, otherwise any discovered plugin.
fn pick(plugins: &[PluginInfo]) -> Option<&PluginInfo> {
    plugins
        .iter()
        .find(|p| p.name == "Surge XT")
        .or_else(|| plugins.first())
}

#[test]
fn instantiate_activate_and_round_trip_state() {
    let plugins = scan();
    let Some(info) = pick(&plugins) else {
        eprintln!("no CLAP plugin installed; skipping instantiation test");
        return;
    };
    eprintln!("using plugin {:?} from {}", info.name, info.path.display());

    // SAFETY: the path came from discovery, i.e. a `.clap` in a sanctioned CLAP directory.
    let entry = unsafe { PluginEntry::load(&info.path) }.expect("load bundle");
    let factory = entry.get_plugin_factory().expect("plugin factory");
    let descriptor = factory
        .plugin_descriptors()
        .find(|d| d.id().map(|c| c.to_bytes()) == Some(info.id.as_bytes()))
        .expect("descriptor for discovered id");
    let host = host_info();

    let mut instance = PluginInstance::<MiauHost>::new(
        |_| MiauShared,
        |_| (),
        &entry,
        descriptor.id().unwrap(),
        &host,
    )
    .expect("instantiate");

    let config = PluginAudioConfiguration {
        sample_rate: 48_000.0,
        min_frames_count: 1,
        max_frames_count: 512,
    };
    let processor = instance.activate(|_, _| (), config).expect("activate");

    // State blob round trip: get a blob from this instance, then load it into a fresh one
    // (the get → destroy → recreate → set cycle from the phase-5 done-when).
    let Some(state) = instance.plugin_handle().get_extension::<PluginState>() else {
        eprintln!("plugin has no state extension; skipping the round trip");
        instance.deactivate(processor);
        return;
    };
    let mut blob = Vec::new();
    state
        .save(&mut instance.plugin_handle(), &mut blob)
        .expect("save state");
    assert!(!blob.is_empty(), "state blob should not be empty");

    instance.deactivate(processor);
    drop(instance);

    let mut revived = PluginInstance::<MiauHost>::new(
        |_| MiauShared,
        |_| (),
        &entry,
        descriptor.id().unwrap(),
        &host,
    )
    .expect("re-instantiate");
    let revived_state = revived
        .plugin_handle()
        .get_extension::<PluginState>()
        .expect("state extension on revived instance");
    let mut cursor = Cursor::new(&blob[..]);
    revived_state
        .load(&mut revived.plugin_handle(), &mut cursor)
        .expect("load state into revived instance");
}
