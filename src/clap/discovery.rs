//! CLAP plugin discovery: scan the standard directories and enumerate the plugins each bundle
//! exposes. Instantiation happens later, on the main thread (a plugin enters the graph snapshot
//! only once activated).

use std::path::{Path, PathBuf};

use clack_host::entry::PluginEntry;

/// One plugin found during discovery: which bundle it lives in, its CLAP id, and display name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginInfo {
    /// Path to the `.clap` bundle that exposes this plugin.
    pub path: PathBuf,
    /// The plugin's unique CLAP id (used to instantiate it).
    pub id: String,
    /// Human-readable plugin name.
    pub name: String,
}

/// The directories scanned by [`scan`]: `~/.clap` and `/usr/lib/clap` (invariant: Linux-only).
#[must_use]
pub fn search_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join(".clap"));
    }
    dirs.push(PathBuf::from("/usr/lib/clap"));
    dirs
}

/// Scans [`search_paths`] and returns every plugin found. Bundles that fail to load are skipped.
#[must_use]
pub fn scan() -> Vec<PluginInfo> {
    let mut found = Vec::new();
    for dir in search_paths() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue; // directory absent or unreadable — nothing to scan here
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "clap") {
                scan_bundle(&path, &mut found);
            }
        }
    }
    found
}

fn scan_bundle(path: &Path, found: &mut Vec<PluginInfo>) {
    // SAFETY: loading a `.clap` runs the bundle's entry init. We only load files from the
    // sanctioned CLAP directories; this is the same trust model as any plugin host.
    let Ok(entry) = (unsafe { PluginEntry::load(path) }) else {
        return;
    };
    let Some(factory) = entry.get_plugin_factory() else {
        return;
    };
    for descriptor in factory.plugin_descriptors() {
        if let (Some(id), Some(name)) = (descriptor.id(), descriptor.name()) {
            found.push(PluginInfo {
                path: path.to_path_buf(),
                id: id.to_string_lossy().into_owned(),
                name: name.to_string_lossy().into_owned(),
            });
        }
    }
}
