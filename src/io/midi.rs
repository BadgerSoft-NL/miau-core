//! Raw MIDI 1.0 channel-voice bytes → typed [`Event`]. This is the io edge where invariant 6
//! holds: raw bytes exist here and nowhere deeper in the core.
//!
//! MIDI 2.0 / MPE are deliberately deferred, so system, poly-aftertouch, program-change and
//! channel-pressure messages have no typed home yet and parse to `None` (dropped at the edge).

use crate::event::Event;

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
        0xB0 => Some(Event::Control {
            channel,
            controller: *bytes.get(1)?,
            value: *bytes.get(2)?,
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
    fn unmodelled_and_truncated_messages_are_dropped() {
        assert_eq!(parse_midi(&[0xC0, 5]), None); // program change
        assert_eq!(parse_midi(&[0xD0, 64]), None); // channel pressure
        assert_eq!(parse_midi(&[0xF8]), None); // system real-time
        assert_eq!(parse_midi(&[0x90, 60]), None); // truncated note-on
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
}
