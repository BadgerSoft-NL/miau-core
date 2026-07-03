//! CLAP host tests. The event/param translation tests run everywhere (no plugin needed); the
//! instantiation and graph-render tests are gated on a plugin actually being installed, so CI (which
//! has none) skips them while a developer machine with e.g. Surge XT exercises the full path.

use std::io::Cursor;
use std::path::PathBuf;

use clack_extensions::state::PluginState;
use clack_host::entry::PluginEntry;
use clack_host::events::io::EventBuffer;
use clack_host::events::spaces::CoreEventSpace;
use clack_host::events::{Match, Pckn};
// Brings the `header()` accessor (from clack's `Event` trait) into scope; aliased to `_` so it does
// not collide with our own `Event` type.
use clack_host::prelude::Event as _;
use clack_host::prelude::{PluginAudioConfiguration, PluginInstance};

use super::host::{MiauHost, MiauShared, host_info};
use super::node::translate_in;
use super::{ClapPlugin, PluginInfo, scan, search_paths};
use crate::event::{Event, ParamId};
use crate::graph::{EventSourceNode, Graph, ProcessContext};
use crate::time::{SamplePos, Tick, TickRange};

#[test]
fn search_paths_include_the_standard_clap_dirs() {
    assert!(search_paths().contains(&PathBuf::from("/usr/lib/clap")));
}

// ---------------------------------------------------------------------------------------------
// Event translation (no plugin required).
// ---------------------------------------------------------------------------------------------

#[test]
fn note_on_translates_to_a_clap_note_event() {
    let mut buf = EventBuffer::new();
    translate_in(
        &mut buf,
        5,
        Event::NoteOn {
            channel: 2,
            key: 60,
            velocity: 127,
        },
    );
    assert_eq!(buf.len(), 1);
    match buf.get(0).unwrap().as_core_event().unwrap() {
        CoreEventSpace::NoteOn(e) => {
            assert_eq!(e.header().time(), 5);
            assert_eq!(e.pckn(), Pckn::new(0u16, 2u16, 60u16, Match::All));
            assert!((e.velocity() - 1.0).abs() < 1e-9, "velocity scaled to 0..1");
        }
        other => panic!("expected NoteOn, got {other:?}"),
    }
}

#[test]
fn note_off_translates_to_a_clap_note_off_event() {
    let mut buf = EventBuffer::new();
    translate_in(
        &mut buf,
        9,
        Event::NoteOff {
            channel: 0,
            key: 64,
            velocity: 0,
        },
    );
    match buf.get(0).unwrap().as_core_event().unwrap() {
        CoreEventSpace::NoteOff(e) => {
            assert_eq!(e.header().time(), 9);
            assert_eq!(e.pckn(), Pckn::new(0u16, 0u16, 64u16, Match::All));
        }
        other => panic!("expected NoteOff, got {other:?}"),
    }
}

#[test]
fn control_change_translates_to_raw_midi() {
    let mut buf = EventBuffer::new();
    translate_in(
        &mut buf,
        0,
        Event::Control {
            channel: 1,
            controller: 74,
            value: 100,
        },
    );
    match buf.get(0).unwrap().as_core_event().unwrap() {
        CoreEventSpace::Midi(e) => assert_eq!(e.data(), [0xB1, 74, 100]),
        other => panic!("expected Midi, got {other:?}"),
    }
}

#[test]
fn pitch_bend_center_translates_to_raw_midi() {
    let mut buf = EventBuffer::new();
    translate_in(
        &mut buf,
        0,
        Event::PitchBend {
            channel: 0,
            value: 0,
        },
    );
    match buf.get(0).unwrap().as_core_event().unwrap() {
        // Centered bend (0) is 14-bit 8192 = LSB 0, MSB 64.
        CoreEventSpace::Midi(e) => assert_eq!(e.data(), [0xE0, 0, 64]),
        other => panic!("expected Midi, got {other:?}"),
    }
}

#[test]
fn param_change_translates_to_a_param_value_event() {
    let mut buf = EventBuffer::new();
    translate_in(
        &mut buf,
        3,
        Event::ParamChange {
            param: ParamId(7),
            value: 0.25,
        },
    );
    match buf.get(0).unwrap().as_core_event().unwrap() {
        CoreEventSpace::ParamValue(e) => {
            assert_eq!(e.header().time(), 3);
            assert_eq!(e.param_id().unwrap().get(), 7);
            assert!((e.value() - 0.25).abs() < 1e-9);
        }
        other => panic!("expected ParamValue, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Live plugin (gated on an installed CLAP plugin).
// ---------------------------------------------------------------------------------------------

const SR: u32 = 48_000;
const BLOCK: usize = 512;

/// Prefer the Surge XT synth if present, otherwise any discovered plugin.
fn pick(plugins: &[PluginInfo]) -> Option<&PluginInfo> {
    plugins
        .iter()
        .find(|p| p.name == "Surge XT")
        .or_else(|| plugins.first())
}

fn ctx(block_start: u64) -> ProcessContext<'static> {
    ProcessContext {
        frames: BLOCK,
        sample_rate: SR,
        block_start: SamplePos::new(block_start),
        ticks: TickRange::new(Tick::new(0), Tick::new(BLOCK as u64)),
        tempo: None,
    }
}

/// An `EventSource` that emits a single sustained middle-C note-on on its first call, so a synth
/// keeps ringing across the render. It never releases the note (a test wants the sustain; the
/// engine's note tracker, not the source, would close it — invariant 8).
struct OneNote {
    fired: bool,
}

impl crate::event::EventSource for OneNote {
    fn events_in(&mut self, _range: TickRange, sink: &mut dyn crate::event::EventSink) {
        if !self.fired {
            self.fired = true;
            sink.push(
                Tick::new(0),
                Event::NoteOn {
                    channel: 0,
                    key: 60,
                    velocity: 100,
                },
            );
        }
    }
}

#[test]
fn clap_node_renders_a_note_and_round_trips_state() {
    let plugins = scan();
    let Some(info) = pick(&plugins) else {
        eprintln!("no CLAP plugin installed; skipping graph-render test");
        return;
    };
    eprintln!("using plugin {:?} from {}", info.name, info.path.display());

    let (mut plugin, node) = match ClapPlugin::open(info, SR, BLOCK) {
        Ok(pair) => pair,
        Err(e) => panic!("open {:?}: {e}", info.name),
    };

    // OneNote -> ClapNode, rendered through the graph.
    let mut graph = Graph::new(BLOCK);
    let src = graph.add_node(EventSourceNode::new(OneNote { fired: false }));
    let clap = graph.add_node(node);
    graph.connect_event((src, 0), (clap, 0)).unwrap();
    graph.commit();

    // A synth needs a few blocks for its envelope to ramp above zero.
    for b in 0..16 {
        graph.process(&ctx((b * BLOCK) as u64));
    }

    if let Some(out) = graph.output(clap, 0) {
        let audible = out[..BLOCK].iter().any(|&s| s.abs() > 1e-6);
        // Only a known synth is guaranteed to sound from a bare note-on; other plugins might be
        // effects or need parameters, so we only assert audio for Surge XT.
        if info.name == "Surge XT" {
            assert!(
                audible,
                "Surge XT should produce audio from a sustained note"
            );
        }
    }

    // State blob round trip: save, then load back into the same instance (invariant 14). Recreate
    // across instances is covered by `instantiate_activate_and_round_trip_state` below.
    if let Some(blob) = plugin.save_state() {
        assert!(!blob.is_empty(), "state blob should not be empty");
        assert!(plugin.load_state(&blob), "state blob should load back");
    }
}

#[test]
fn params_enumerate_lists_named_parameters() {
    let plugins = scan();
    let Some(info) = pick(&plugins) else {
        eprintln!("no CLAP plugin installed; skipping params enumeration test");
        return;
    };
    let (mut plugin, _node) = match ClapPlugin::open(info, SR, BLOCK) {
        Ok(pair) => pair,
        Err(e) => panic!("open {:?}: {e}", info.name),
    };

    let params = plugin.params();
    // Only a known synth is guaranteed to expose parameters; other plugins may legitimately have
    // none, so assert only for Surge XT.
    if info.name == "Surge XT" {
        assert!(!params.is_empty(), "Surge XT exposes parameters");
        assert!(params.iter().all(|p| p.min <= p.max), "ranges are ordered");
        assert!(
            params.iter().any(|p| !p.name.is_empty()),
            "params are named"
        );
    }
}

#[test]
fn instantiate_activate_and_round_trip_state() {
    let plugins = scan();
    let Some(info) = pick(&plugins) else {
        eprintln!("no CLAP plugin installed; skipping instantiation test");
        return;
    };

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

    // The get → destroy → recreate → set cycle from the phase-5 done-when.
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
