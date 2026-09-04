//! Standard MIDI files ↔ typed events: [`read_smf`] and [`write_smf`].
//!
//! A `.mid` file is raw MIDI bytes, so it belongs at the io edge and nowhere deeper (invariant 6)
//! — the same rule that puts [`parse_midi`]/[`encode_midi`] next door. This module handles only
//! the *file* layer: chunks, delta times, the tempo meta-message and the PPQ the file was written
//! at. What a status byte means stays in [`midi`](super::midi), and both directions route through
//! it, so there is one place that decides that a note-on with velocity zero is a note-off.
//!
//! Two conversions happen here and are worth naming.
//!
//! **Ticks are rescaled to 960 PPQ** (invariant 4). A file at 480 PPQ has its positions doubled;
//! at 384 they are multiplied by 2.5 and rounded. The rescale happens once per event, on the
//! absolute tick — never on the delta — so a file's rounding error cannot accumulate down the
//! track.
//!
//! **Tempo becomes [`TempoSegment`]s**, not a [`TempoMap`](crate::time::TempoMap): a map bakes in
//! a sample rate and the file does not have one. That is the same split `persist/` makes — the
//! segments are the stored form, the map is compiled from them for a device.
//!
//! What is *dropped*: SysEx, time and key signatures, lyrics, markers, SMPTE offsets, and every
//! other meta-message except tempo and track name. This is deliberate and it does not contradict
//! invariant 14 — that invariant binds `persist/`, which must round-trip a document it authored.
//! An SMF is an interchange format written by someone else; carrying an opaque tail of another
//! sequencer's metadata through the engine would be pretending we understood it.

use midly::num::{u4, u7, u15, u24, u28};
use midly::{Format, Header, MetaMessage, Smf, Timing, TrackEvent, TrackEventKind};

use super::midi::{encode_midi, parse_midi};
use crate::event::Event;
use crate::time::{TempoSegment, Tick};

/// Tempo a file is played at when it carries no tempo meta-message, per the MIDI specification.
const DEFAULT_BPM: f64 = 120.0;
/// Microseconds in a minute — the numerator of the tempo-meta ↔ BPM conversion.
const MICROS_PER_MINUTE: f64 = 60_000_000.0;

/// One track of a standard MIDI file: a name and typed events at absolute ticks.
///
/// Absolute, not delta: delta times are a storage detail of the file format, and every consumer
/// inside the engine — [`Clip`](crate::arrange::Clip), the tempo map, a lane — reasons in absolute
/// ticks. Events are sorted by tick on the way in.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SmfTrack {
    /// The track's name meta-message, if it had one.
    pub name: Option<String>,
    /// The track's events at absolute ticks (960 PPQ), sorted.
    pub events: Vec<(Tick, Event)>,
}

/// A standard MIDI file in core terms: its tempo map as segments, plus its tracks.
///
/// The tempo is always usable: a file without a tempo meta-message yields a single 120 BPM segment
/// at tick 0, so [`TempoMap::new`](crate::time::TempoMap::new) accepts the segments unconditionally.
#[derive(Clone, Debug, PartialEq)]
pub struct SmfFile {
    /// Stepwise tempo, first segment at `Tick(0)`, starts strictly increasing.
    pub tempo: Vec<TempoSegment>,
    /// The file's tracks, in file order.
    pub tracks: Vec<SmfTrack>,
}

impl SmfFile {
    /// A file of `tracks` at one constant tempo — the shape a shell exports.
    #[must_use]
    pub fn new(bpm: f64, tracks: Vec<SmfTrack>) -> Self {
        Self {
            tempo: vec![TempoSegment {
                start: Tick::new(0),
                bpm,
            }],
            tracks,
        }
    }
}

/// Why a standard MIDI file could not be read or written.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SmfError {
    /// The bytes are not a standard MIDI file, or are truncated. Carries the parser's reason.
    Malformed(String),
    /// The file is timed in SMPTE timecode (frames per second) rather than in ticks per beat.
    ///
    /// Rejected rather than guessed at: such a file pins its events to wall-clock time and has no
    /// beat to rescale against, so every tick the core produced from it would be a fiction
    /// (invariant 4). Rare in practice — sequencers write metrical timing.
    Timecode,
    /// A gap between two consecutive events is longer than a delta time can express
    /// (2²⁸ − 1 ticks, about 77 hours at 120 BPM).
    GapTooLong,
}

impl std::fmt::Display for SmfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "not a standard MIDI file: {why}"),
            Self::Timecode => f.write_str("SMPTE-timed MIDI file has no musical time to read"),
            Self::GapTooLong => f.write_str("gap between events exceeds a MIDI delta time"),
        }
    }
}

impl std::error::Error for SmfError {}

/// Reads a standard MIDI file into typed events at 960 PPQ.
///
/// Bytes in, not a path: an SMF arrives from a file, a drop target or a download, and choosing
/// which is the shell's business, not the core's.
///
/// Every track is returned, in file order, whatever the file's format claims. Format 0 has one
/// track carrying all sixteen channels; format 2's tracks are separate songs that a caller must
/// play one after another rather than together — the file says which, and the caller knows what it
/// asked for.
///
/// # Errors
/// See [`SmfError`]: malformed bytes and SMPTE timing.
pub fn read_smf(bytes: &[u8]) -> Result<SmfFile, SmfError> {
    let smf = Smf::parse(bytes).map_err(|e| SmfError::Malformed(e.to_string()))?;
    let ppq = match smf.header.timing {
        Timing::Metrical(ppq) => u64::from(ppq.as_int()),
        Timing::Timecode(..) => return Err(SmfError::Timecode),
    };
    if ppq == 0 {
        return Err(SmfError::Malformed(
            "timing says zero ticks per beat".into(),
        ));
    }

    let mut tempo_at: Vec<(u64, f64)> = Vec::new();
    let mut tracks = Vec::with_capacity(smf.tracks.len());

    for track in &smf.tracks {
        // Absolute position in the file's own ticks; rescaled per event, never per delta, so
        // rounding cannot accumulate.
        let mut raw: u64 = 0;
        let mut events = Vec::new();
        let mut name = None;

        for event in track {
            raw += u64::from(event.delta.as_int());
            let at = rescale(raw, ppq);
            match event.kind {
                TrackEventKind::Midi { channel, message } => {
                    if let Some(event) = to_event(channel, message) {
                        events.push((Tick::new(at), event));
                    }
                }
                TrackEventKind::Meta(MetaMessage::Tempo(micros)) => {
                    let micros = f64::from(micros.as_int());
                    if micros > 0.0 {
                        tempo_at.push((at, MICROS_PER_MINUTE / micros));
                    }
                }
                TrackEventKind::Meta(MetaMessage::TrackName(text)) if name.is_none() => {
                    name = Some(String::from_utf8_lossy(text).into_owned());
                }
                // Everything else is either not music or not ours to carry; see the module doc.
                _ => {}
            }
        }

        // A file may hold its events out of order across running-status runs; the rest of the
        // engine assumes sorted. Stable, so same-tick events keep the file's order — a program
        // change written just before the note it colours must stay in front of it.
        events.sort_by_key(|(tick, _)| tick.get());
        tracks.push(SmfTrack { name, events });
    }

    Ok(SmfFile {
        tempo: segments(tempo_at),
        tracks,
    })
}

/// Writes a standard MIDI file at 960 PPQ.
///
/// Always format 1, always with the tempo on its own leading track, even when there is a single
/// tempo and a single music track (invariant 10: one way to do things). A reader that only
/// understands format 0 is a reader from the 1980s; a shell that has to guess which shape it will
/// get back from its own exporter is a bug waiting to happen.
///
/// [`Event::ParamChange`] has no place on a MIDI wire and is skipped, exactly as
/// [`encode_midi`] skips it.
///
/// # Errors
/// [`SmfError::GapTooLong`] when two consecutive events lie further apart than a delta time can
/// express; [`SmfError::Malformed`] if the bytes could not be assembled.
pub fn write_smf(file: &SmfFile) -> Result<Vec<u8>, SmfError> {
    let mut smf = Smf::new(Header::new(
        Format::Parallel,
        Timing::Metrical(u15::from_int_lossy(Tick::PPQ as u16)),
    ));

    // Track 0 carries the tempo and nothing else, so a reader that stops after the first track
    // still learns how fast the piece goes.
    let mut tempo_track = Vec::with_capacity(file.tempo.len() + 1);
    let mut previous = 0_u64;
    for segment in &file.tempo {
        let micros = MICROS_PER_MINUTE / segment.bpm.max(f64::MIN_POSITIVE);
        tempo_track.push(TrackEvent {
            delta: delta(&mut previous, segment.start.get())?,
            kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::from_int_lossy(micros as u32))),
        });
    }
    tempo_track.push(end_of_track());
    smf.tracks.push(tempo_track);

    for track in &file.tracks {
        let mut out = Vec::with_capacity(track.events.len() + 2);
        if let Some(name) = &track.name {
            out.push(TrackEvent {
                delta: u28::from_int_lossy(0),
                kind: TrackEventKind::Meta(MetaMessage::TrackName(name.as_bytes())),
            });
        }
        // The caller's order is not to be trusted; the file format is delta-encoded and a
        // backwards step is unrepresentable.
        let mut sorted: Vec<&(Tick, Event)> = track.events.iter().collect();
        sorted.sort_by_key(|(tick, _)| tick.get());

        let mut previous = 0_u64;
        for (tick, event) in sorted {
            let Some((channel, message)) = to_midly(event) else {
                continue;
            };
            out.push(TrackEvent {
                delta: delta(&mut previous, tick.get())?,
                kind: TrackEventKind::Midi { channel, message },
            });
        }
        out.push(end_of_track());
        smf.tracks.push(out);
    }

    let mut bytes = Vec::new();
    smf.write_std(&mut bytes)
        .map_err(|e| SmfError::Malformed(e.to_string()))?;
    Ok(bytes)
}

/// A file tick at `ppq` pulses per beat, expressed at the engine's 960 (invariant 4).
///
/// In 128-bit arithmetic: at 960 PPQ a `u64` tick and a four-digit PPQ overflow 64 bits long
/// before the music runs out.
fn rescale(raw: u64, ppq: u64) -> u64 {
    ((u128::from(raw) * u128::from(Tick::PPQ)) / u128::from(ppq)) as u64
}

/// The tempo events found across the file, as a valid stepwise map.
///
/// Sorted, one entry per tick (a later event at the same tick wins, as a sequencer's own reader
/// would have it), and always opening at tick 0 — [`TempoMap::new`](crate::time::TempoMap::new)
/// rejects anything else, and a caller should never have to repair what it was handed.
fn segments(mut found: Vec<(u64, f64)>) -> Vec<TempoSegment> {
    found.sort_by_key(|(tick, _)| *tick);
    let mut out: Vec<TempoSegment> = Vec::with_capacity(found.len() + 1);
    for (tick, bpm) in found {
        if out.last().is_some_and(|last| last.start.get() == tick) {
            out.pop();
        }
        out.push(TempoSegment {
            start: Tick::new(tick),
            bpm,
        });
    }
    if out.first().is_none_or(|first| first.start.get() != 0) {
        out.insert(
            0,
            TempoSegment {
                start: Tick::new(0),
                bpm: DEFAULT_BPM,
            },
        );
    }
    out
}

/// The delta from `previous` to `tick`, advancing `previous`.
fn delta(previous: &mut u64, tick: u64) -> Result<u28, SmfError> {
    let gap = tick.saturating_sub(*previous);
    *previous = tick;
    u32::try_from(gap)
        .ok()
        .and_then(u28::try_from)
        .ok_or(SmfError::GapTooLong)
}

/// The obligatory end-of-track meta, at zero delta.
fn end_of_track() -> TrackEvent<'static> {
    TrackEvent {
        delta: u28::from_int_lossy(0),
        kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
    }
}

/// A file's channel message as a typed [`Event`], through the same bytes the wire uses.
///
/// The detour via bytes is the point: [`parse_midi`] is the one place that knows what a status
/// byte means, and a second mapping written against another crate's enum is a second place for
/// note-on-velocity-zero to be handled differently. Off the audio thread this costs three bytes of
/// stack.
fn to_event(channel: u4, message: midly::MidiMessage) -> Option<Event> {
    let mut bytes = [0_u8; 3];
    let mut out = &mut bytes[..];
    midly::live::LiveEvent::Midi { channel, message }
        .write(&mut out)
        .ok()?;
    parse_midi(&bytes)
}

/// The inverse of [`to_event`]: a typed event as a file channel message.
fn to_midly(event: &Event) -> Option<(u4, midly::MidiMessage)> {
    let encoded = encode_midi(event)?;
    let bytes = encoded.as_bytes();
    let status = *bytes.first()?;
    let channel = u4::from_int_lossy(status & 0x0F);
    let data = |i: usize| u7::from_int_lossy(bytes.get(i).copied().unwrap_or(0));
    let message = match status & 0xF0 {
        0x80 => midly::MidiMessage::NoteOff {
            key: data(1),
            vel: data(2),
        },
        0x90 => midly::MidiMessage::NoteOn {
            key: data(1),
            vel: data(2),
        },
        0xA0 => midly::MidiMessage::Aftertouch {
            key: data(1),
            vel: data(2),
        },
        0xB0 => midly::MidiMessage::Controller {
            controller: data(1),
            value: data(2),
        },
        0xC0 => midly::MidiMessage::ProgramChange { program: data(1) },
        0xD0 => midly::MidiMessage::ChannelAftertouch { vel: data(1) },
        0xE0 => midly::MidiMessage::PitchBend {
            bend: midly::PitchBend(midly::num::u14::from_int_lossy(
                u16::from(bytes.get(1).copied().unwrap_or(0))
                    | (u16::from(bytes.get(2).copied().unwrap_or(0)) << 7),
            )),
        },
        _ => return None,
    };
    Some((channel, message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::TempoMap;

    fn on(key: u8) -> Event {
        Event::NoteOn {
            channel: 0,
            key,
            velocity: 100,
        }
    }

    fn off(key: u8) -> Event {
        Event::NoteOff {
            channel: 0,
            key,
            velocity: 64,
        }
    }

    /// A minimal metrical file built by hand, so the reader is tested against bytes and not
    /// against the writer's idea of them.
    fn handmade(ppq: u16, track: &[(u32, &[u8])]) -> Vec<u8> {
        let mut events = Vec::new();
        for (delta, bytes) in track {
            // Variable-length delta.
            let mut buf = [0_u8; 4];
            let mut n = 0;
            let mut v = *delta;
            loop {
                buf[n] = (v & 0x7F) as u8;
                n += 1;
                v >>= 7;
                if v == 0 {
                    break;
                }
            }
            for i in (0..n).rev() {
                events.push(if i == 0 { buf[i] } else { buf[i] | 0x80 });
            }
            events.extend_from_slice(bytes);
        }
        let mut out = b"MThd".to_vec();
        out.extend_from_slice(&6_u32.to_be_bytes());
        out.extend_from_slice(&0_u16.to_be_bytes()); // format 0
        out.extend_from_slice(&1_u16.to_be_bytes()); // one track
        out.extend_from_slice(&ppq.to_be_bytes());
        out.extend_from_slice(b"MTrk");
        out.extend_from_slice(&(events.len() as u32).to_be_bytes());
        out.extend_from_slice(&events);
        out
    }

    #[test]
    fn a_written_file_reads_back_as_it_went_in() {
        let file = SmfFile::new(
            96.0,
            vec![SmfTrack {
                name: Some("cantus".into()),
                events: vec![
                    (Tick::new(0), on(62)),
                    (Tick::new(3840), off(62)),
                    (Tick::new(3840), on(65)),
                    (Tick::new(7680), off(65)),
                ],
            }],
        );
        let back = read_smf(&write_smf(&file).unwrap()).unwrap();
        // Track 0 is the tempo track the writer always puts in front; the music follows it.
        assert_eq!(&back.tracks[1..], &file.tracks[..]);
        assert_eq!(back.tempo.len(), 1);
        assert_eq!(back.tempo[0].start, Tick::new(0));
        // The tempo survives a trip through microseconds-per-beat, which is integral and so not
        // exact for every BPM; a hundredth is far below anything audible.
        assert!((back.tempo[0].bpm - 96.0).abs() < 0.01);
    }

    #[test]
    fn ticks_are_rescaled_to_the_engines_ppq() {
        // A 480 PPQ file: one beat is 480 there and 960 here.
        let bytes = handmade(
            480,
            &[
                (0, &[0x90, 60, 100]),
                (480, &[0x80, 60, 0]),
                (240, &[0x90, 64, 100]),
            ],
        );
        let file = read_smf(&bytes).unwrap();
        let ticks: Vec<u64> = file.tracks[0].events.iter().map(|(t, _)| t.get()).collect();
        assert_eq!(ticks, vec![0, 960, 1440]);
    }

    #[test]
    fn an_odd_ppq_does_not_drift_down_the_track() {
        // 384 PPQ: a beat is 2.5 engine ticks per file tick, so every other position rounds. The
        // rescale is on the absolute tick, so the hundredth beat still lands exactly on 96000 —
        // rescaling deltas instead would have lost half a tick a hundred times over.
        let mut track: Vec<(u32, &[u8])> = Vec::new();
        for _ in 0..100 {
            track.push((384, &[0x90, 60, 100]));
        }
        let bytes = handmade(384, &track);
        let file = read_smf(&bytes).unwrap();
        let last = file.tracks[0].events.last().unwrap().0;
        assert_eq!(last, Tick::new(100 * 960));
    }

    #[test]
    fn note_on_with_velocity_zero_arrives_as_a_note_off() {
        // The running-status convention, decided once in `parse_midi` and inherited here.
        let bytes = handmade(960, &[(0, &[0x90, 60, 100]), (960, &[0x90, 60, 0])]);
        let file = read_smf(&bytes).unwrap();
        assert_eq!(
            file.tracks[0].events[1].1,
            Event::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0
            }
        );
    }

    #[test]
    fn a_file_without_a_tempo_plays_at_the_specified_default() {
        let bytes = handmade(960, &[(0, &[0x90, 60, 100])]);
        let file = read_smf(&bytes).unwrap();
        assert_eq!(
            file.tempo,
            vec![TempoSegment {
                start: Tick::new(0),
                bpm: DEFAULT_BPM
            }]
        );
    }

    #[test]
    fn tempo_changes_become_segments_a_map_accepts() {
        // 500000 µs/beat = 120 BPM at tick 0, 1000000 = 60 BPM one beat later.
        let bytes = handmade(
            960,
            &[
                (0, &[0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20]),
                (960, &[0xFF, 0x51, 0x03, 0x0F, 0x42, 0x40]),
            ],
        );
        let file = read_smf(&bytes).unwrap();
        assert_eq!(file.tempo.len(), 2);
        assert!((file.tempo[0].bpm - 120.0).abs() < 1e-9);
        assert_eq!(file.tempo[1].start, Tick::new(960));
        assert!((file.tempo[1].bpm - 60.0).abs() < 1e-9);
        // The point of the shape: it is exactly what the tempo map takes.
        assert!(TempoMap::new(48_000, &file.tempo).is_ok());
    }

    #[test]
    fn a_tempo_after_the_start_still_opens_the_map_at_zero() {
        let bytes = handmade(960, &[(480, &[0xFF, 0x51, 0x03, 0x0F, 0x42, 0x40])]);
        let file = read_smf(&bytes).unwrap();
        assert_eq!(file.tempo[0].start, Tick::new(0));
        assert!((file.tempo[0].bpm - DEFAULT_BPM).abs() < 1e-9);
        assert_eq!(file.tempo[1].start, Tick::new(480));
        assert!(TempoMap::new(48_000, &file.tempo).is_ok());
    }

    #[test]
    fn two_tempi_on_one_tick_leave_one_segment() {
        // Strictly increasing starts is the map's law; the later event wins, as a sequencer's
        // own reader would have it.
        let bytes = handmade(
            960,
            &[
                (0, &[0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20]),
                (0, &[0xFF, 0x51, 0x03, 0x0F, 0x42, 0x40]),
            ],
        );
        let file = read_smf(&bytes).unwrap();
        assert_eq!(file.tempo.len(), 1);
        assert!((file.tempo[0].bpm - 60.0).abs() < 1e-9);
        assert!(TempoMap::new(48_000, &file.tempo).is_ok());
    }

    #[test]
    fn every_channel_voice_message_survives_the_round_trip() {
        // The full MIDI 1.0 channel-voice set, so the file layer loses nothing the wire keeps.
        let events = vec![
            (
                Tick::new(0),
                Event::NoteOn {
                    channel: 3,
                    key: 60,
                    velocity: 100,
                },
            ),
            (
                Tick::new(10),
                Event::NoteOff {
                    channel: 3,
                    key: 60,
                    velocity: 64,
                },
            ),
            (
                Tick::new(20),
                Event::PolyPressure {
                    channel: 4,
                    key: 61,
                    pressure: 77,
                },
            ),
            (
                Tick::new(30),
                Event::Control {
                    channel: 5,
                    controller: 64,
                    value: 127,
                },
            ),
            (
                Tick::new(40),
                Event::ProgramChange {
                    channel: 6,
                    program: 42,
                },
            ),
            (
                Tick::new(50),
                Event::ChannelPressure {
                    channel: 7,
                    pressure: 12,
                },
            ),
            (
                Tick::new(60),
                Event::PitchBend {
                    channel: 8,
                    value: -2048,
                },
            ),
            (
                Tick::new(70),
                Event::PitchBend {
                    channel: 8,
                    value: 8191,
                },
            ),
        ];
        let file = SmfFile::new(
            120.0,
            vec![SmfTrack {
                name: None,
                events: events.clone(),
            }],
        );
        let back = read_smf(&write_smf(&file).unwrap()).unwrap();
        assert_eq!(back.tracks[1].events, events);
    }

    #[test]
    fn a_param_change_has_no_place_in_a_file() {
        // Same rule as `encode_midi`: it addresses a node's parameter, not a channel.
        let file = SmfFile::new(
            120.0,
            vec![SmfTrack {
                name: None,
                events: vec![
                    (
                        Tick::new(0),
                        Event::ParamChange {
                            param: crate::event::ParamId(7),
                            value: 0.5,
                        },
                    ),
                    (Tick::new(10), on(60)),
                ],
            }],
        );
        let back = read_smf(&write_smf(&file).unwrap()).unwrap();
        assert_eq!(back.tracks[1].events, vec![(Tick::new(10), on(60))]);
    }

    #[test]
    fn tracks_keep_their_names_and_their_order() {
        let file = SmfFile::new(
            120.0,
            vec![
                SmfTrack {
                    name: Some("cantus firmus".into()),
                    events: vec![(Tick::new(0), on(50))],
                },
                SmfTrack {
                    name: Some("contrapunt".into()),
                    events: vec![(Tick::new(0), on(69))],
                },
            ],
        );
        let back = read_smf(&write_smf(&file).unwrap()).unwrap();
        // The written tempo track carries no notes and no name, so it is not mistaken for music.
        assert_eq!(back.tracks.len(), 3);
        assert_eq!(back.tracks[0].name, None);
        assert!(back.tracks[0].events.is_empty());
        assert_eq!(back.tracks[1].name.as_deref(), Some("cantus firmus"));
        assert_eq!(back.tracks[2].name.as_deref(), Some("contrapunt"));
    }

    #[test]
    fn events_come_back_sorted_however_they_were_handed_over() {
        let file = SmfFile::new(
            120.0,
            vec![SmfTrack {
                name: None,
                events: vec![(Tick::new(960), on(64)), (Tick::new(0), on(60))],
            }],
        );
        let back = read_smf(&write_smf(&file).unwrap()).unwrap();
        assert_eq!(
            back.tracks[1].events,
            vec![(Tick::new(0), on(60)), (Tick::new(960), on(64))]
        );
    }

    #[test]
    fn smpte_timing_is_refused_rather_than_guessed_at() {
        let mut bytes = handmade(960, &[(0, &[0x90, 60, 100])]);
        // Overwrite the division field with 25 fps / 40 subframes (high bit set).
        bytes[12] = 0xE7;
        bytes[13] = 40;
        assert_eq!(read_smf(&bytes), Err(SmfError::Timecode));
    }

    #[test]
    fn rubbish_is_refused_with_a_reason() {
        assert!(matches!(
            read_smf(b"this is not a midi file"),
            Err(SmfError::Malformed(_))
        ));
        assert!(matches!(read_smf(&[]), Err(SmfError::Malformed(_))));
    }

    #[test]
    fn a_gap_no_delta_time_can_express_is_an_error_and_not_a_wrap() {
        let file = SmfFile::new(
            120.0,
            vec![SmfTrack {
                name: None,
                events: vec![(Tick::new(0), on(60)), (Tick::new(1 << 28), on(61))],
            }],
        );
        assert_eq!(write_smf(&file), Err(SmfError::GapTooLong));
    }
}
