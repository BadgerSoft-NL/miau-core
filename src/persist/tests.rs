//! Phase 6 definition of done: a render survives save → restart → load byte-exactly; unknown
//! fields and foreign shell sections round-trip intact; a missing plugin becomes a placeholder
//! that saves back losslessly; and a `.miauclip` moves a clip between two documents.

use std::collections::BTreeMap;

use serde_json::json;

use super::*;
use crate::arrange::{Clip, Lane, Placement, SampleRef};
use crate::graph::{Node, NodeKey, Placeholder, PortCounts, ProcessContext};
use crate::time::{SamplePos, TempoSegment, Tick, TickRange};

const SR: u32 = 48_000;
const BLOCK: usize = 64;

fn seg(bpm: f64) -> TempoSegment {
    TempoSegment {
        start: Tick::new(0),
        bpm,
    }
}

/// A document: `sine(freq) -> gain(g)`, with the gain node's id returned so tests can read its
/// output. Exercises the tempo map, two builtin node kinds, an audio connection and the counter.
fn sine_gain_doc(freq: f64, g: f32) -> (Document, NodeId) {
    let mut doc = Document::new();
    doc.tempo.push(seg(120.0));
    let sine = doc.add_node(
        "sine",
        json!({ "freq": freq }),
        PortCounts {
            audio_out: 1,
            ..PortCounts::default()
        },
    );
    let gain = doc.add_node(
        "gain",
        json!({ "gain": g }),
        PortCounts {
            audio_in: 1,
            audio_out: 1,
            ..PortCounts::default()
        },
    );
    doc.connections.push(Connection {
        kind: crate::graph::PortType::Audio,
        from: sine,
        from_port: 0,
        to: gain,
        to_port: 0,
    });
    (doc, gain)
}

/// Renders `blocks` blocks and returns the output-node's port-0 samples concatenated.
fn render(inst: &mut Instantiated, output: NodeId, blocks: usize) -> Vec<f32> {
    let key: NodeKey = inst.keys[&output];
    let mut out = Vec::new();
    for b in 0..blocks {
        let ctx = ProcessContext {
            frames: BLOCK,
            sample_rate: SR,
            block_start: SamplePos::new((b * BLOCK) as u64),
            ticks: TickRange::new(Tick::new(0), Tick::new(0)),
            tempo: inst.tempo.as_ref(),
        };
        inst.graph.process(&ctx);
        out.extend_from_slice(&inst.graph.output(key, 0).unwrap()[..BLOCK]);
    }
    out
}

#[test]
fn render_round_trips_byte_exactly() {
    let registry = NodeRegistry::with_builtins();
    let (doc, gain) = sine_gain_doc(440.0, 0.5);

    // Reference render straight from the in-memory document.
    let mut reference = doc
        .instantiate(&registry, SR, BLOCK, &BTreeMap::new())
        .unwrap();
    let expected = render(&mut reference, gain, 32);

    // Save → "restart" (drop everything) → load → render again.
    let bytes = Project::new(doc).write().unwrap();
    let project = Project::read(&bytes).unwrap();
    let mut loaded = project.instantiate(&registry, SR, BLOCK).unwrap();
    let actual = render(&mut loaded, gain, 32);

    assert_eq!(
        actual, expected,
        "reloaded render must match sample-for-sample"
    );
    // Non-trivial signal: the sine actually produced sound, so the match means something.
    assert!(actual.iter().any(|&s| s.abs() > 0.1));
}

#[test]
fn unknown_field_and_foreign_shell_section_survive_round_trip() {
    let (mut doc, _gain) = sine_gain_doc(220.0, 1.0);
    // Arrangement content, so lanes/clips serialize through the container too.
    doc.lanes.push(Lane::new(vec![Placement::with_span(
        Tick::new(0),
        Tick::new(3840),
        Clip::new(
            Tick::new(960),
            vec![(
                Tick::new(0),
                crate::event::Event::NoteOn {
                    channel: 0,
                    key: 64,
                    velocity: 90,
                },
            )],
        )
        .with_looping(true),
    )]));
    // An unknown top-level field an older/foreign writer might have added.
    doc.extra
        .insert("shellHint".to_owned(), json!({ "color": "teal" }));
    // An unknown field nested on a node.
    doc.nodes[0]
        .extra
        .insert("uiPos".to_owned(), json!([12, 34]));

    let mut project = Project::new(doc.clone());
    project
        .shells
        .insert("ableton".to_owned(), br#"{"tracks":["drums"]}"#.to_vec());
    project
        .extra_entries
        .insert("thumbnail.png".to_owned(), vec![0x89, b'P', b'N', b'G']);

    let bytes = project.write().unwrap();
    let read = Project::read(&bytes).unwrap();

    assert_eq!(
        read.document, doc,
        "document (incl. unknown fields) preserved"
    );
    assert_eq!(read.document.extra["shellHint"], json!({ "color": "teal" }));
    assert_eq!(read.document.nodes[0].extra["uiPos"], json!([12, 34]));
    assert_eq!(
        read.shells, project.shells,
        "foreign shell section preserved"
    );
    assert_eq!(
        read.extra_entries, project.extra_entries,
        "unknown container entry preserved"
    );
}

#[test]
fn missing_plugin_becomes_placeholder_and_saves_losslessly() {
    let mut doc = Document::new();
    // A plugin the builtin registry has never heard of.
    let ports = PortCounts {
        event_in: 1,
        audio_out: 2,
        ..PortCounts::default()
    };
    let synth = doc.add_node("com.example.synth", json!({ "preset": 3 }), ports);

    let mut project = Project::new(doc);
    project.blobs.insert(synth, vec![0xDE, 0xAD, 0xBE, 0xEF]);

    // Instantiating with the builtin registry stands the plugin in with a placeholder that
    // preserves its ports and blob (invariants 7 & 14).
    let registry = NodeRegistry::with_builtins();
    let inst = project.instantiate(&registry, SR, BLOCK).unwrap();
    assert_eq!(inst.missing, vec![synth]);
    let placeholder = inst
        .graph
        .node::<Placeholder>(inst.keys[&synth])
        .expect("missing plugin should be a Placeholder");
    assert_eq!(placeholder.kind(), "com.example.synth");
    assert_eq!(placeholder.blob(), &[0xDE, 0xAD, 0xBE, 0xEF]);
    assert_eq!(placeholder.ports(), ports);

    // And it saves back exactly as it loaded.
    let bytes = project.write().unwrap();
    let round_tripped = Project::read(&bytes).unwrap();
    assert_eq!(
        round_tripped, project,
        "missing-plugin project round-trips losslessly"
    );
}

#[test]
fn miauclip_moves_a_clip_between_documents() {
    // Document A owns a clip on a lane.
    let mut doc_a = Document::new();
    let clip = Clip::new(
        Tick::new(960),
        vec![
            (
                Tick::new(0),
                crate::event::Event::NoteOn {
                    channel: 0,
                    key: 60,
                    velocity: 100,
                },
            ),
            (
                Tick::new(480),
                crate::event::Event::NoteOff {
                    channel: 0,
                    key: 60,
                    velocity: 0,
                },
            ),
        ],
    )
    .with_looping(true);
    doc_a
        .lanes
        .push(Lane::new(vec![Placement::new(Tick::new(0), clip.clone())]));

    // Export it as a .miauclip and re-import into a fresh document B.
    let exported = MiauClip::new(clip.clone()).write().unwrap();
    let imported = MiauClip::read(&exported).unwrap();
    assert_eq!(imported.clip, clip, "clip survives the exchange format");

    let mut doc_b = Document::new();
    doc_b.lanes.push(Lane::new(vec![Placement::new(
        Tick::new(1920),
        imported.clip,
    )]));
    assert_eq!(
        doc_b.lanes[0].placements()[0].clip().events(),
        doc_a.lanes[0].placements()[0].clip().events(),
        "the same music now lives in document B"
    );
}

#[test]
fn miauclip_carries_a_sample_and_verifies_its_hash() {
    let bytes = b"RIFF....fake wav data".to_vec();
    let sample = SampleRef {
        path: "samples/kick.wav".to_owned(),
        hash: container_hash(&bytes),
    };
    let clip = Clip::new(Tick::new(480), Vec::new()).with_sample(sample.clone());

    let mut miau = MiauClip::new(clip);
    miau.samples
        .insert("samples/kick.wav".to_owned(), bytes.clone());

    let read = MiauClip::read(&miau.write().unwrap()).unwrap();
    assert_eq!(read.samples["samples/kick.wav"], bytes);
    assert_eq!(read.clip.sample(), Some(&sample));
}

#[test]
fn embedded_sample_hash_mismatch_is_detected() {
    let mut project = Project::new(Document::new());
    let good = project.embed_sample("samples/kick.wav", b"real audio".to_vec());
    // A clip referencing the sample by its correct hash verifies clean...
    project.document.lanes.push(Lane::new(vec![Placement::new(
        Tick::new(0),
        Clip::new(Tick::new(10), Vec::new()).with_sample(good.clone()),
    )]));
    project.verify_samples().unwrap();

    // ...but corrupt the stored bytes and verification catches it (design log note 10 §5).
    project
        .samples
        .insert("samples/kick.wav".to_owned(), b"tampered".to_vec());
    assert!(matches!(
        project.verify_samples(),
        Err(PersistError::HashMismatch { .. })
    ));
}

// Re-exported so tests can compute the same hash the container uses.
use super::container::hash as container_hash;
