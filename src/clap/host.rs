//! The miau CLAP host handlers. Minimal by design: the plugin talks to the engine over the
//! existing command/event channels, not through rich host callbacks. The main-thread and
//! audio-processor handler types are `()`; only the shared handler is a real type (it must be
//! `Send + Sync`), and its request callbacks are no-ops for now — restart/process/callback
//! requests are handled by the engine's own scheduling.

use clack_host::prelude::{HostHandlers, HostInfo, SharedHandler};

/// Shared host state, visible from every thread. No-op request handlers for now.
pub(crate) struct MiauShared;

impl SharedHandler<'_> for MiauShared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

/// The miau host handler bundle.
pub(crate) struct MiauHost;

impl HostHandlers for MiauHost {
    type Shared<'a> = MiauShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

/// Host identification advertised to plugins.
pub(crate) fn host_info() -> HostInfo {
    HostInfo::new(
        "miau-core",
        "miau",
        "https://github.com/BadgerSoft-NL/miau-core",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("static host info strings contain no interior nul bytes")
}
