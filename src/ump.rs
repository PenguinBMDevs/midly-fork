//! MIDI 2.0 UMP (Universal MIDI Packet) support.
//!
//! This module provides preprocessing to convert MIDI 2.0 UMP chunks within SMF files
//! into standard MTrk chunks, enabling the existing MIDI 1.0 parsers to understand them.
//!
//! # Supported UMP Message Types
//!
//! - **Type 0x2**: MIDI 1.0 Channel Voice over UMP (32-bit)
//!   - NoteOn / NoteOff with full 8-bit data bytes (0-255 key range)
//!   - ControlChange, ProgramChange, PitchBend
//! - **Type 0x4**: MIDI 2.0 Channel Voice (64-bit)
//!   - NoteOn / NoteOff with 16-bit note number → clamped to u8 (0-255)
//!   - 16-bit velocity → mapped to 8-bit (MSB)
//! - **Type 0x0**: Utility messages
//!   - Subtype 0x01: Timing (delta time in ticks)

use alloc::borrow::Cow;
use alloc::vec::Vec;

/// Preprocess raw SMF bytes, converting MIDI 2.0 UMP chunks to MTrk chunks.
///
/// If no UMP chunks are found, returns `Cow::Borrowed(&raw)` — zero allocation.
/// If UMP chunks are found, returns `Cow::Owned(Vec<u8>)` with all UMP chunks
/// converted to MTrk format. Non-UMP chunks (including regular MTrk chunks)
/// pass through unchanged.
///
/// # Example
///
/// ```rust,ignore
/// use midly::ump;
///
/// let raw = std::fs::read("midi2_file.mid").unwrap();
/// let processed = ump::preprocess_smf(&raw);
/// let smf = midly::Smf::parse(&processed).unwrap();
/// ```
pub fn preprocess_smf<'a>(raw: &'a [u8]) -> Cow<'a, [u8]> {
    if raw.len() < 14 {
        return Cow::Borrowed(raw);
    }

    // Quick scan: does this file have any UMP chunks?
    if !has_ump_chunks(raw) {
        return Cow::Borrowed(raw);
    }

    // Convert: replace UMP chunks with MTrk chunks
    let mut result = Vec::with_capacity(raw.len());
    // Copy MThd header
    result.extend_from_slice(&raw[..14]);

    let len = raw.len();
    let mut offset = 14;

    while offset + 8 <= len {
        let chunk_type = &raw[offset..offset + 4];
        let chunk_len =
            u32::from_be_bytes(raw[offset + 4..offset + 8].try_into().unwrap_or([0; 4])) as usize;
        let next = (offset + 8 + chunk_len).min(len);

        if chunk_type == b"UMP " {
            let ump_data = &raw[offset + 8..next];
            let mtrk_data = convert_ump_chunk_to_mtrk(ump_data);
            // Write as MTrk chunk
            result.extend_from_slice(b"MTrk");
            result.extend_from_slice(&(mtrk_data.len() as u32).to_be_bytes());
            result.extend_from_slice(&mtrk_data);
        } else {
            // Pass through other chunk types unchanged
            result.extend_from_slice(&raw[offset..next]);
        }

        offset = next;
    }

    // Append any trailing bytes
    if offset < len {
        result.extend_from_slice(&raw[offset..]);
    }

    Cow::Owned(result)
}

/// Check if the raw SMF data contains any UMP chunks.
fn has_ump_chunks(raw: &[u8]) -> bool {
    let mut offset: usize = 14;
    while offset + 8 <= raw.len() {
        let chunk_type = &raw[offset..offset + 4];
        let chunk_len =
            u32::from_be_bytes(raw[offset + 4..offset + 8].try_into().unwrap_or([0; 4])) as usize;

        if chunk_type == b"UMP " {
            return true;
        }
        offset = offset.saturating_add(8 + chunk_len);
    }
    false
}

/// Convert a single UMP chunk's data bytes into MTrk-format event bytes.
///
/// The output is a valid MTrk track data buffer containing delta-time + MIDI events,
/// suitable for creating a `TrackIter` or passing to `scan_track_notes_only`.
pub(crate) fn convert_ump_chunk_to_mtrk(ump_data: &[u8]) -> Vec<u8> {
    let mut mtrk = Vec::with_capacity(ump_data.len());
    let mut delta: u32 = 0;
    let mut last_status: Option<u8> = None;
    let mut offset = 0;

    while offset + 4 <= ump_data.len() {
        let word0 = u32::from_be_bytes(ump_data[offset..offset + 4].try_into().unwrap_or([0; 4]));
        // Message Type (upper nibble of the 32-bit word)
        let mt = ((word0 >> 28) & 0xF) as u8;

        match mt {
            0x0 => {
                // ── Utility Message (32-bit) ──
                // Bits 27-24: Group (usually 0)
                // Bits 23-20: Subtype
                // Bits 19-0:  Depends on subtype
                let subtype = ((word0 >> 20) & 0xF) as u8;
                if subtype == 0x01 {
                    // Timing message: delta time in ticks (lower 20 bits)
                    delta = word0 & 0xFFFFF;
                }
                // Other subtypes (NOF=0x00, etc.): ignored
                offset += 4;
            }

            0x2 => {
                // ── MIDI 1.0 Channel Voice over UMP (32-bit) ──
                // Byte 0 bits 3-0: Channel
                // Byte 1 bits 7-4: Status nibble (8=NoteOff, 9=NoteOn, etc.)
                // Byte 1 bits 3-0: Reserved
                // Byte 2: Data byte 1 (key / controller / program)
                // Byte 3: Data byte 2 (velocity / value)
                let channel = ((word0 >> 24) & 0xF) as u8;
                let status_nibble = ((word0 >> 20) & 0xF) as u8;
                // Bits 15-8 = Data byte 1, Bits 7-0 = Data byte 2
                let data1 = ((word0 >> 8) & 0xFF) as u8;
                let data2 = (word0 & 0xFF) as u8;

                if (0x8..=0xE).contains(&status_nibble) {
                    let status = (status_nibble << 4) | channel;
                    // 1-byte messages: ProgramChange (0xC), ChannelAftertouch (0xD)
                    let msg_len: usize = if status_nibble == 0xC || status_nibble == 0xD {
                        1
                    } else {
                        2
                    };

                    write_vlq(&mut mtrk, delta);
                    delta = 0;

                    if last_status != Some(status) {
                        mtrk.push(status);
                        last_status = Some(status);
                    }
                    mtrk.push(data1);
                    if msg_len == 2 {
                        mtrk.push(data2);
                    }
                }
                offset += 4;
            }

            0x4 => {
                // ── MIDI 2.0 Channel Voice (64-bit = 2 words) ──
                if offset + 8 <= ump_data.len() {
                    let word1 = u32::from_be_bytes(
                        ump_data[offset + 4..offset + 8]
                            .try_into()
                            .unwrap_or([0; 4]),
                    );

                    let channel = ((word0 >> 24) & 0xF) as u8;
                    let status_nibble = ((word0 >> 20) & 0xF) as u8;

                    // Note number: bytes 2-3 (16 bits)
                    // Velocity: bits 31-16 of word1 (16 bits)
                    let note_number = (word0 & 0xFFFF) as u16;
                    let velocity_full = ((word1 >> 16) & 0xFFFF) as u16;

                    // Map to u8 range
                    let key: u8 = if note_number > 255 {
                        255
                    } else {
                        note_number as u8
                    };
                    let vel: u8 = ((velocity_full >> 8) as u8).min(127);

                    if status_nibble == 0x8 || status_nibble == 0x9 {
                        let status_base = if status_nibble == 0x9 { 0x90 } else { 0x80 };
                        let status = status_base | channel;

                        write_vlq(&mut mtrk, delta);
                        delta = 0;

                        if last_status != Some(status) {
                            mtrk.push(status);
                            last_status = Some(status);
                        }
                        mtrk.push(key);
                        mtrk.push(vel);
                    }
                    // Skip unknown status nibbles (continue parsing)
                    offset += 8;
                } else {
                    break;
                }
            }

            _ => {
                // Skip other message types
                // Type 0x4 and 0x5 are 64-bit; others are 32-bit
                if mt == 0x4 || mt == 0x5 {
                    offset = offset.saturating_add(8);
                } else {
                    offset = offset.saturating_add(4);
                }
            }
        }
    }

    mtrk
}

/// Encode a 32-bit unsigned integer as MIDI variable-length quantity.
fn write_vlq(buf: &mut Vec<u8>, value: u32) {
    if value == 0 {
        buf.push(0);
        return;
    }
    // Collect 7-bit chunks (LSB first)
    let mut chunks = [0u8; 5];
    let mut len = 0usize;
    let mut v = value;
    while v > 0 {
        chunks[len] = (v & 0x7F) as u8;
        v >>= 7;
        len += 1;
    }
    // Write MSB first (reverse order)
    // Continuation bit (0x80) on all bytes except the very last (LSB chunk)
    for i in (0..len).rev() {
        if i == 0 {
            buf.push(chunks[i]);
        } else {
            buf.push(chunks[i] | 0x80);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_data() {
        let result = preprocess_smf(b"");
        assert!(matches!(result, Cow::Borrowed(_)));
    }

    #[test]
    fn test_no_ump_passthrough() {
        // Create minimal SMF with MTrk chunks only
        let mut smf = vec![];
        // MThd header
        smf.extend_from_slice(b"MThd");
        smf.extend_from_slice(&[0, 0, 0, 6]); // header length
        smf.extend_from_slice(&[0, 1]); // format 1
        smf.extend_from_slice(&[0, 2]); // 2 tracks
        smf.extend_from_slice(&[0, 120]); // 120 ticks/quarter

        // MTrk chunk 1 (empty)
        smf.extend_from_slice(b"MTrk");
        smf.extend_from_slice(&[0, 0, 0, 0]); // length 0

        // MTrk chunk 2 (empty)
        smf.extend_from_slice(b"MTrk");
        smf.extend_from_slice(&[0, 0, 0, 0]); // length 0

        let result = preprocess_smf(&smf);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(&*result, &smf);
    }

    #[test]
    fn test_write_vlq_zero() {
        let mut buf = Vec::new();
        write_vlq(&mut buf, 0);
        assert_eq!(buf, vec![0]);
    }

    #[test]
    fn test_write_vlq_single_byte() {
        let mut buf = Vec::new();
        write_vlq(&mut buf, 64);
        assert_eq!(buf, vec![64]);
    }

    #[test]
    fn test_write_vlq_two_byte() {
        let mut buf = Vec::new();
        write_vlq(&mut buf, 128);
        assert_eq!(buf, vec![0x81, 0x00]);
    }

    #[test]
    fn test_write_vlq_large() {
        let mut buf = Vec::new();
        write_vlq(&mut buf, 0x123456);
        let decoded = decode_vlq_for_test(&buf);
        assert_eq!(decoded, 0x123456);
    }

    #[test]
    fn test_convert_ump_type2_noteon() {
        // UMP Type 0x2: NoteOn ch0, key=60, vel=100
        // Byte 0: 0x20 (MT=2, ch=0)
        // Byte 1: 0x90 (NoteOn status nibble << 4)
        // Byte 2: 0x3C (key=60)
        // Byte 3: 0x64 (vel=100)
        let ump = vec![0x20, 0x90, 0x3C, 0x64];
        let mtrk = convert_ump_chunk_to_mtrk(&ump);

        // Expect: VLQ(0) + NoteOn ch0 key=60 vel=100
        assert_eq!(mtrk, vec![0x00, 0x90, 0x3C, 0x64]);
    }

    #[test]
    fn test_convert_ump_type2_noteon_256key() {
        // UMP Type 0x2: NoteOn ch0, key=200 (> 127), vel=100
        let ump = vec![0x20, 0x90, 0xC8, 0x64]; // 0xC8 = 200
        let mtrk = convert_ump_chunk_to_mtrk(&ump);

        assert_eq!(mtrk, vec![0x00, 0x90, 0xC8, 0x64]);
    }

    #[test]
    fn test_convert_ump_type2_program_change() {
        // UMP Type 0x2: ProgramChange ch5, program=42
        // Byte 0: 0x25 (MT=2, ch=5)
        // Byte 1: 0xC0 (PC status nibble << 4)
        // Byte 2: 42
        // Byte 3: ignored (but still 8 bits)
        let ump = vec![0x25, 0xC0, 42, 0];
        let mtrk = convert_ump_chunk_to_mtrk(&ump);

        // ProgramChange is 1-byte: VLQ(0) + status + program
        assert_eq!(mtrk, vec![0x00, 0xC5, 42]);
    }

    #[test]
    fn test_convert_ump_type2_with_running_status() {
        // Multiple NoteOn events with running status
        let ump = vec![
            0x20, 0x90, 0x3C, 0x64, // NoteOn ch0 key=60 vel=100
            0x20, 0x90, 0x40, 0x50, // NoteOn ch0 key=64 vel=80 (same status → running)
        ];
        let mtrk = convert_ump_chunk_to_mtrk(&ump);

        // First event: explicit status (VLQ(0) + 0x90 + key + vel)
        // Second event: running status (VLQ(0) + key + vel, no status)
        assert_eq!(
            mtrk,
            vec![
                0x00, 0x90, 0x3C, 0x64, // event 1
                0x00, 0x40, 0x50, // event 2 (no status byte, running from event 1)
            ]
        );
    }

    #[test]
    fn test_convert_ump_type4_noteon() {
        // UMP Type 0x4: MIDI 2.0 NoteOn
        // Word 0: 0x40 | channel=0, status nibble=0x9, note number=60
        // Word 1: velocity=32768 (50% = 0x8000), attr=0
        let ump = vec![
            0x40, 0x90, 0x00, 0x3C, // Word 0: ch=0, NoteOn, note=0x003C=60
            0x80, 0x00, 0x00, 0x00, // Word 1: vel=0x8000=32768 (MSB=0x80)
        ];
        let mtrk = convert_ump_chunk_to_mtrk(&ump);

        // Note: velocity 0x8000 = 32768, >> 8 = 128, clamped to 127
        assert_eq!(mtrk, vec![0x00, 0x90, 60, 127]);
    }

    #[test]
    fn test_convert_ump_type4_noteon_256key() {
        // UMP Type 0x4: NoteOn ch0, note=200 (>127)
        let ump = vec![
            0x40, 0x90, 0x00, 0xC8, // Word 0: ch=0, NoteOn, note=0x00C8=200
            0x7F, 0x00, 0x00, 0x00, // Word 1: vel=0x7F00, attr=0
        ];
        let mtrk = convert_ump_chunk_to_mtrk(&ump);

        assert_eq!(mtrk, vec![0x00, 0x90, 200, 0x7F]);
    }

    #[test]
    fn test_convert_ump_type4_note_number_clamping() {
        // Note number > 255 should be clamped to 255
        let ump = vec![
            0x40, 0x90, 0x01, 0x00, // Word 0: note=0x0100=256 → clamped to 255
            0x7F, 0x00, 0x00, 0x00,
        ];
        let mtrk = convert_ump_chunk_to_mtrk(&ump);
        assert_eq!(mtrk, vec![0x00, 0x90, 255, 0x7F]);
    }

    #[test]
    fn test_timing_message() {
        // Utility Timing message (delta=480) followed by NoteOn
        let ump = vec![
            // Utility: Type 0x0, subtype 0x1, delta=480
            0x00, 0x10, 0x01, 0xE0, // delta = 0x01E0 = 480
            // NoteOn ch0 key=60 vel=100
            0x20, 0x90, 0x3C, 0x64,
        ];
        let mtrk = convert_ump_chunk_to_mtrk(&ump);

        // Expect VLQ(480) + NoteOn
        let mut expected = vec![0x83, 0x60]; // VLQ(480) = 0x83 0x60
        expected.extend_from_slice(&[0x90, 0x3C, 0x64]);
        assert_eq!(mtrk, expected);
    }

    #[test]
    fn test_convert_ump_mixed_mtrk_and_ump() {
        // SMF with one MTrk track and one UMP track
        let mut raw = vec![];
        raw.extend_from_slice(b"MThd");
        raw.extend_from_slice(&[0, 0, 0, 6]); // header len
        raw.extend_from_slice(&[0, 1]); // format 1
        raw.extend_from_slice(&[0, 2]); // 2 tracks
        raw.extend_from_slice(&[0, 120]); // 120 ppq

        // MTrk track (empty)
        raw.extend_from_slice(b"MTrk");
        raw.extend_from_slice(&[0, 0, 0, 0]); // length 0

        // UMP track (one note)
        raw.extend_from_slice(b"UMP ");
        raw.extend_from_slice(&[0, 0, 0, 4]); // length 4
        raw.extend_from_slice(&[0x20, 0x90, 0x3C, 0x64]); // NoteOn ch0 key=60

        let processed = preprocess_smf(&raw);
        assert!(matches!(processed, Cow::Owned(_)));

        // Verify: MTrk should appear twice (original MTrk + converted UMP)
        let processed = processed.into_owned();
        assert!(processed.windows(4).filter(|w| w == b"MTrk").count() >= 2);
        assert!(!processed.windows(4).any(|w| w == b"UMP "));
    }

    // Helper: decode a VLQ for testing
    fn decode_vlq_for_test(data: &[u8]) -> u32 {
        let mut value = 0u32;
        for &byte in data {
            value = (value << 7) | (byte & 0x7F) as u32;
            if byte & 0x80 == 0 {
                break;
            }
        }
        value
    }
}
