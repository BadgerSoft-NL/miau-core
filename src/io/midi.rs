//! Raw MIDI 1.0 channel-voice bytes ↔ typed [`Event`]. This is the io edge where invariant 6
//! holds: raw bytes exist here and nowhere deeper in the core.
//!
//! Both directions live here and are exact inverses: [`parse_midi`]/[`parse_ump`] on the way in,
//! [`encode_midi`]/[`encode_ump`] on the way out.
//!
//! MIDI 1.0 **channel voice is covered completely** — note on/off, poly pressure, control change,
//! program change, channel pressure and pitch bend — so nothing a keyboard or sequencer plays is
//! lost at the edge. MIDI 2.0 / MPE remain deliberately deferred.
//!
//! System messages are timing, not music, so they take a separate road:
//! [`parse_sync`]/[`encode_sync`] convert them to and from
//! [`SyncMessage`](crate::transport::SyncMessage). A clock pulse can therefore never be mistaken
//! for a note. SysEx, tune request, active sensing and reset are still dropped at the edge.

use crate::event::Event;
use crate::transport::SyncMessage;

/// Parses one MIDI channel-voice message into an [`Event`], or `None` for messages the core does
/// not model yet.
///
/// The running-status "note-on with velocity 0" convention is normalized to [`Event::NoteOff`]
/// here, so the rest of the engine never has to special-case it (see the note on
/// [`Event::NoteOn`]).
#[must_use]
pub fn parse_midi(bytes: &[u8]) -> Option<Event> {
    let status = *bytes.first()?;
    let channel = status & 0x0F;
    match status & 0xF0 {
        0x80 => Some(Event::NoteOff {
            channel,
            key: *bytes.get(1)?,
            velocity: *bytes.get(2)?,
        }),
        0x90 => {
            let key = *bytes.get(1)?;
            let velocity = *bytes.get(2)?;
            if velocity == 0 {
                Some(Event::NoteOff {
                    channel,
                    key,
                    velocity: 0,
                })
            } else {
                Some(Event::NoteOn {
                    channel,
                    key,
                    velocity,
                })
            }
        }
        0xA0 => Some(Event::PolyPressure {
            channel,
            key: *bytes.get(1)?,
            pressure: *bytes.get(2)?,
        }),
        0xB0 => Some(Event::Control {
            channel,
            controller: *bytes.get(1)?,
            value: *bytes.get(2)?,
        }),
        // Program change and channel pressure are two-byte messages: no data2 to read.
        0xC0 => Some(Event::ProgramChange {
            channel,
            program: *bytes.get(1)?,
        }),
        0xD0 => Some(Event::ChannelPressure {
            channel,
            pressure: *bytes.get(1)?,
        }),
        0xE0 => {
            // 14-bit little-endian, centered at 8192 → signed.
            let lsb = i16::from(*bytes.get(1)?);
            let msb = i16::from(*bytes.get(2)?);
            Some(Event::PitchBend {
                channel,
                value: ((msb << 7) | lsb) - 8192,
            })
        }
        _ => None,
    }
}

/// Parses one Universal MIDI Packet (MIDI 2.0 transport) into an [`Event`], for the MIDI 1.0
/// channel-voice message type only (UMP message type `0x2`, a single 32-bit word in host byte
/// order). MIDI 2.0 channel voice (wider resolution) and other packet types are deferred.
#[must_use]
pub fn parse_ump(bytes: &[u8]) -> Option<Event> {
    let word = u32::from_ne_bytes(bytes.get(0..4)?.try_into().ok()?);
    // High nibble is the message type; 0x2 is a one-word MIDI 1.0 channel-voice message whose
    // low three bytes are exactly a legacy status/data1/data2 triple.
    if (word >> 28) & 0xF != 0x2 {
        return None;
    }
    let status = ((word >> 16) & 0xFF) as u8;
    let data1 = ((word >> 8) & 0xFF) as u8;
    let data2 = (word & 0xFF) as u8;
    parse_midi(&[status, data1, data2])
}

/// One encoded MIDI 1.0 channel-voice message: two or three bytes, held inline.
///
/// A fixed-size value rather than a slice or a `Vec`, so encoding allocates nothing and can run
/// on the audio thread (invariant 11).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MidiMessage {
    bytes: [u8; 3],
    len: u8,
}

impl MidiMessage {
    /// The message's bytes: status first, then its one or two data bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

/// Encodes one [`Event`] as a MIDI 1.0 channel-voice message — the inverse of [`parse_midi`].
///
/// Returns `None` for events with no place on a MIDI wire: [`Event::ParamChange`] addresses a
/// node's own parameter (invariant 13), it is not a channel message.
///
/// Fields are masked to their MIDI widths rather than rejected, so a shell that computes a key
/// or channel arithmetically can never emit a malformed status byte. `NoteOn` velocity is
/// additionally floored at 1: a `NoteOn` always means *on* (see [`Event::NoteOn`]), so it must
/// not degrade into the running-status velocity-0 note-off convention on the wire.
#[must_use]
pub fn encode_midi(event: &Event) -> Option<MidiMessage> {
    let (bytes, len) = match *event {
        Event::NoteOn {
            channel,
            key,
            velocity,
        } => (
            [
                0x90 | (channel & 0x0F),
                key & 0x7F,
                (velocity & 0x7F).max(1),
            ],
            3,
        ),
        Event::NoteOff {
            channel,
            key,
            velocity,
        } => ([0x80 | (channel & 0x0F), key & 0x7F, velocity & 0x7F], 3),
        Event::PolyPressure {
            channel,
            key,
            pressure,
        } => ([0xA0 | (channel & 0x0F), key & 0x7F, pressure & 0x7F], 3),
        Event::Control {
            channel,
            controller,
            value,
        } => (
            [0xB0 | (channel & 0x0F), controller & 0x7F, value & 0x7F],
            3,
        ),
        // Two-byte messages: the third slot stays zero and is never read.
        Event::ProgramChange { channel, program } => {
            ([0xC0 | (channel & 0x0F), program & 0x7F, 0], 2)
        }
        Event::ChannelPressure { channel, pressure } => {
            ([0xD0 | (channel & 0x0F), pressure & 0x7F, 0], 2)
        }
        Event::PitchBend { channel, value } => {
            // 14-bit little-endian, centered at 8192 — the inverse of the parse side.
            let bend = (value.clamp(-8192, 8191) + 8192) as u16;
            (
                [
                    0xE0 | (channel & 0x0F),
                    (bend & 0x7F) as u8,
                    ((bend >> 7) & 0x7F) as u8,
                ],
                3,
            )
        }
        Event::ParamChange { .. } => return None,
    };
    Some(MidiMessage { bytes, len })
}

/// Encodes one [`Event`] as a Universal MIDI Packet — the inverse of [`parse_ump`].
///
/// Emits UMP message type `0x2` (MIDI 1.0 channel voice, group 0): a single 32-bit word whose
/// low three bytes are the [`encode_midi`] message, zero-padded for the two-byte ones exactly as
/// the UMP spec requires. Returned in host byte order, which is what `SPA_CONTROL_UMP` carries.
#[must_use]
pub fn encode_ump(event: &Event) -> Option<[u8; 4]> {
    let message = encode_midi(event)?;
    let [status, data1, data2] = message.bytes;
    let word = 0x2000_0000 | (u32::from(status) << 16) | (u32::from(data1) << 8) | u32::from(data2);
    Some(word.to_ne_bytes())
}

/// Parses one MIDI system message into a [`SyncMessage`] — timing rather than music.
///
/// The counterpart to [`parse_midi`], which handles channel voice and returns `None` here. System
/// messages the core does not model (SysEx, tune request, active sensing, reset) also yield
/// `None`: they are dropped at the edge like any other unmodelled byte.
#[must_use]
pub fn parse_sync(bytes: &[u8]) -> Option<SyncMessage> {
    match *bytes.first()? {
        0xF2 => {
            // Song position pointer: 14-bit little-endian, in MIDI beats (sixteenth notes).
            let lsb = u16::from(*bytes.get(1)? & 0x7F);
            let msb = u16::from(*bytes.get(2)? & 0x7F);
            Some(SyncMessage::SongPosition((msb << 7) | lsb))
        }
        0xF8 => Some(SyncMessage::Clock),
        0xFA => Some(SyncMessage::Start),
        0xFB => Some(SyncMessage::Continue),
        0xFC => Some(SyncMessage::Stop),
        _ => None,
    }
}

/// Parses one Universal MIDI Packet carrying a system message into a [`SyncMessage`].
///
/// UMP message type `0x1` is system real-time and system common; its low three bytes are the
/// legacy status/data1/data2 triple, so this defers to [`parse_sync`].
#[must_use]
pub fn parse_sync_ump(bytes: &[u8]) -> Option<SyncMessage> {
    let word = u32::from_ne_bytes(bytes.get(0..4)?.try_into().ok()?);
    if (word >> 28) & 0xF != 0x1 {
        return None;
    }
    let status = ((word >> 16) & 0xFF) as u8;
    let data1 = ((word >> 8) & 0xFF) as u8;
    let data2 = (word & 0xFF) as u8;
    parse_sync(&[status, data1, data2])
}

/// Encodes one [`SyncMessage`] — the inverse of [`parse_sync`].
///
/// Real-time messages are a single status byte; a song-position pointer is three. Allocation-free,
/// like [`encode_midi`], because this runs on the audio thread (invariant 11).
#[must_use]
pub fn encode_sync(message: SyncMessage) -> MidiMessage {
    let (bytes, len) = match message {
        SyncMessage::Clock => ([0xF8, 0, 0], 1),
        SyncMessage::Start => ([0xFA, 0, 0], 1),
        SyncMessage::Continue => ([0xFB, 0, 0], 1),
        SyncMessage::Stop => ([0xFC, 0, 0], 1),
        SyncMessage::SongPosition(beats) => {
            let beats = beats & 0x3FFF;
            ([0xF2, (beats & 0x7F) as u8, ((beats >> 7) & 0x7F) as u8], 3)
        }
    };
    MidiMessage { bytes, len }
}

/// Encodes one [`SyncMessage`] as a Universal MIDI Packet — the inverse of [`parse_sync_ump`].
///
/// Emits UMP message type `0x1` (system real time and common, group 0), zero-padded for the
/// one-byte messages exactly as the UMP spec requires.
#[must_use]
pub fn encode_sync_ump(message: SyncMessage) -> [u8; 4] {
    let [status, data1, data2] = encode_sync(message).bytes;
    let word = 0x1000_0000 | (u32::from(status) << 16) | (u32::from(data1) << 8) | u32::from(data2);
    word.to_ne_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_note_on_and_off() {
        assert_eq!(
            parse_midi(&[0x90, 60, 100]),
            Some(Event::NoteOn {
                channel: 0,
                key: 60,
                velocity: 100
            })
        );
        assert_eq!(
            parse_midi(&[0x85, 64, 40]),
            Some(Event::NoteOff {
                channel: 5,
                key: 64,
                velocity: 40
            })
        );
    }

    #[test]
    fn note_on_velocity_zero_becomes_note_off() {
        assert_eq!(
            parse_midi(&[0x92, 48, 0]),
            Some(Event::NoteOff {
                channel: 2,
                key: 48,
                velocity: 0
            })
        );
    }

    #[test]
    fn parses_control_change() {
        assert_eq!(
            parse_midi(&[0xB0, 7, 127]),
            Some(Event::Control {
                channel: 0,
                controller: 7,
                value: 127
            })
        );
    }

    #[test]
    fn parses_pitch_bend_center_and_extremes() {
        assert_eq!(
            parse_midi(&[0xE0, 0x00, 0x40]),
            Some(Event::PitchBend {
                channel: 0,
                value: 0
            })
        );
        assert_eq!(
            parse_midi(&[0xE3, 0x00, 0x00]),
            Some(Event::PitchBend {
                channel: 3,
                value: -8192
            })
        );
        assert_eq!(
            parse_midi(&[0xE0, 0x7F, 0x7F]),
            Some(Event::PitchBend {
                channel: 0,
                value: 8191
            })
        );
    }

    #[test]
    fn parses_the_rest_of_midi_1_channel_voice() {
        assert_eq!(
            parse_midi(&[0xA1, 60, 100]),
            Some(Event::PolyPressure {
                channel: 1,
                key: 60,
                pressure: 100
            })
        );
        assert_eq!(
            parse_midi(&[0xC0, 5]),
            Some(Event::ProgramChange {
                channel: 0,
                program: 5
            })
        );
        assert_eq!(
            parse_midi(&[0xD7, 64]),
            Some(Event::ChannelPressure {
                channel: 7,
                pressure: 64
            })
        );
    }

    #[test]
    fn unmodelled_and_truncated_messages_are_dropped() {
        // System messages are synchronisation, not music — no home in `Event` yet.
        assert_eq!(parse_midi(&[0xF8]), None); // system real-time (clock)
        assert_eq!(parse_midi(&[0xF0, 0x7E]), None); // sysex
        assert_eq!(parse_midi(&[0x90, 60]), None); // truncated note-on
        assert_eq!(parse_midi(&[0xC0]), None); // truncated program change
        assert_eq!(parse_midi(&[]), None);
    }

    #[test]
    fn parses_ump_midi1_channel_voice() {
        // UMP word 0x2090_3C64: type 0x2, group 0, status 0x90 (note-on ch0), key 60, vel 100.
        let word = 0x2090_3C64_u32.to_ne_bytes();
        assert_eq!(
            parse_ump(&word),
            Some(Event::NoteOn {
                channel: 0,
                key: 60,
                velocity: 100
            })
        );
    }

    #[test]
    fn ignores_non_midi1_ump_and_short_packets() {
        // Type 0x4 is MIDI 2.0 channel voice (deferred).
        assert_eq!(parse_ump(&0x4090_3C00_u32.to_ne_bytes()), None);
        assert_eq!(parse_ump(&[0x00, 0x11]), None); // too short
    }

    /// Encodes `event` and returns its wire bytes, failing the test if it has no wire form.
    fn wire(event: &Event) -> Vec<u8> {
        encode_midi(event)
            .expect("a channel-voice event encodes")
            .as_bytes()
            .to_vec()
    }

    #[test]
    fn encodes_the_bytes_the_parser_expects() {
        assert_eq!(
            wire(&Event::NoteOn {
                channel: 0,
                key: 60,
                velocity: 100
            }),
            [0x90, 60, 100]
        );
        assert_eq!(
            wire(&Event::NoteOff {
                channel: 5,
                key: 64,
                velocity: 40
            }),
            [0x85, 64, 40]
        );
        assert_eq!(
            wire(&Event::Control {
                channel: 0,
                controller: 7,
                value: 127
            }),
            [0xB0, 7, 127]
        );
        assert_eq!(
            wire(&Event::PitchBend {
                channel: 0,
                value: 0
            }),
            [0xE0, 0x00, 0x40] // center
        );
    }

    #[test]
    fn program_change_and_channel_pressure_are_two_byte_messages() {
        // The wire length is part of the format: a trailing zero byte here would be read as the
        // start of a new message by anything downstream.
        assert_eq!(
            wire(&Event::ProgramChange {
                channel: 2,
                program: 42
            }),
            [0xC2, 42]
        );
        assert_eq!(
            wire(&Event::ChannelPressure {
                channel: 9,
                pressure: 64
            }),
            [0xD9, 64]
        );
        assert_eq!(
            wire(&Event::PolyPressure {
                channel: 0,
                key: 60,
                pressure: 100
            }),
            [0xA0, 60, 100] // poly pressure keeps all three
        );
    }

    /// The round trip that matters: every event the core models survives a trip to the wire and
    /// back, across the whole field range — the encoder and parser cannot drift apart.
    #[test]
    fn every_modelled_event_round_trips_through_both_encodings() {
        let mut cases = Vec::new();
        for channel in 0..16_u8 {
            for key in (0..128_u8).step_by(7) {
                cases.push(Event::NoteOn {
                    channel,
                    key,
                    velocity: 1 + key % 127,
                });
                cases.push(Event::NoteOff {
                    channel,
                    key,
                    velocity: key % 128,
                });
                cases.push(Event::Control {
                    channel,
                    controller: key,
                    value: 127 - key % 128,
                });
                cases.push(Event::PolyPressure {
                    channel,
                    key,
                    pressure: key % 128,
                });
                cases.push(Event::ProgramChange {
                    channel,
                    program: key,
                });
                cases.push(Event::ChannelPressure {
                    channel,
                    pressure: key,
                });
            }
            for value in [-8192_i16, -4096, -1, 0, 1, 4096, 8191] {
                cases.push(Event::PitchBend { channel, value });
            }
        }

        for event in cases {
            let midi = encode_midi(&event).expect("modelled events encode");
            assert_eq!(
                parse_midi(midi.as_bytes()),
                Some(event),
                "legacy round trip: {event:?}"
            );
            let ump = encode_ump(&event).expect("modelled events encode");
            assert_eq!(parse_ump(&ump), Some(event), "ump round trip: {event:?}");
        }
    }

    #[test]
    fn note_on_never_degrades_into_the_velocity_zero_note_off() {
        // A NoteOn always means *on*; velocity 0 would parse back as NoteOff at the far end, so
        // the encoder floors it at 1 rather than emitting a note-off in disguise.
        let silent = Event::NoteOn {
            channel: 3,
            key: 48,
            velocity: 0,
        };
        assert_eq!(wire(&silent), [0x93, 48, 1]);
        assert_eq!(
            parse_midi(&wire(&silent)),
            Some(Event::NoteOn {
                channel: 3,
                key: 48,
                velocity: 1
            })
        );
    }

    #[test]
    fn out_of_range_fields_are_masked_to_midi_widths() {
        // A shell computing a key or channel arithmetically must never produce a malformed
        // status byte; masking keeps the wire valid instead of rejecting the event.
        assert_eq!(
            wire(&Event::NoteOff {
                channel: 0xFF,
                key: 200,
                velocity: 255
            }),
            [0x8F, 200 & 0x7F, 127]
        );
        assert_eq!(
            wire(&Event::ProgramChange {
                channel: 0xFF,
                program: 200
            }),
            [0xCF, 200 & 0x7F]
        );
        // Pitch bend is clamped, not wrapped — an out-of-range bend saturates at the extreme.
        assert_eq!(
            wire(&Event::PitchBend {
                channel: 0,
                value: i16::MAX
            }),
            [0xE0, 0x7F, 0x7F]
        );
        assert_eq!(
            wire(&Event::PitchBend {
                channel: 0,
                value: i16::MIN
            }),
            [0xE0, 0x00, 0x00]
        );
    }

    #[test]
    fn sync_messages_round_trip_through_both_encodings() {
        let cases = [
            SyncMessage::Clock,
            SyncMessage::Start,
            SyncMessage::Continue,
            SyncMessage::Stop,
            SyncMessage::SongPosition(0),
            SyncMessage::SongPosition(1),
            SyncMessage::SongPosition(127),
            SyncMessage::SongPosition(128),
            SyncMessage::SongPosition(16_383),
        ];
        for message in cases {
            let legacy = encode_sync(message);
            assert_eq!(
                parse_sync(legacy.as_bytes()),
                Some(message),
                "legacy: {message:?}"
            );
            assert_eq!(
                parse_sync_ump(&encode_sync_ump(message)),
                Some(message),
                "ump: {message:?}"
            );
        }
    }

    #[test]
    fn real_time_messages_are_a_single_status_byte() {
        // Length is part of the format: a padded clock pulse would be read as a stray data byte.
        assert_eq!(encode_sync(SyncMessage::Clock).as_bytes(), [0xF8]);
        assert_eq!(encode_sync(SyncMessage::Start).as_bytes(), [0xFA]);
        assert_eq!(encode_sync(SyncMessage::Continue).as_bytes(), [0xFB]);
        assert_eq!(encode_sync(SyncMessage::Stop).as_bytes(), [0xFC]);
        // A song position is three, 14-bit little-endian.
        assert_eq!(
            encode_sync(SyncMessage::SongPosition(200)).as_bytes(),
            [0xF2, 200 & 0x7F, 200 >> 7]
        );
    }

    /// The separation that matters: timing and music never answer to the same parser, so a clock
    /// pulse can never surface as a note or vice versa.
    #[test]
    fn music_and_timing_never_cross_over() {
        for message in [SyncMessage::Clock, SyncMessage::Start, SyncMessage::Stop] {
            let bytes = encode_sync(message);
            assert_eq!(
                parse_midi(bytes.as_bytes()),
                None,
                "{message:?} is not music"
            );
        }
        let note = Event::NoteOn {
            channel: 0,
            key: 60,
            velocity: 100,
        };
        let bytes = encode_midi(&note).unwrap();
        assert_eq!(parse_sync(bytes.as_bytes()), None, "a note is not timing");

        // The same must hold through UMP, where the message type is what separates them.
        assert_eq!(parse_ump(&encode_sync_ump(SyncMessage::Clock)), None);
        assert_eq!(parse_sync_ump(&encode_ump(&note).unwrap()), None);
    }

    #[test]
    fn unmodelled_system_messages_are_still_dropped() {
        assert_eq!(parse_sync(&[0xF0, 0x7E]), None); // sysex
        assert_eq!(parse_sync(&[0xF6]), None); // tune request
        assert_eq!(parse_sync(&[0xFE]), None); // active sensing
        assert_eq!(parse_sync(&[0xFF]), None); // reset
        assert_eq!(parse_sync(&[0xF2, 0x10]), None); // truncated song position
        assert_eq!(parse_sync(&[]), None);
    }

    #[test]
    fn param_change_has_no_wire_representation() {
        // invariant 13: a ParamChange addresses the receiving node's own parameter; it is not a
        // channel message and must never reach a MIDI cable.
        let param = Event::ParamChange {
            param: crate::event::ParamId(3),
            value: 0.5,
        };
        assert_eq!(encode_midi(&param), None);
        assert_eq!(encode_ump(&param), None);
    }
}
