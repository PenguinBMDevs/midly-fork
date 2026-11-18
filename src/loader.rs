//! High-performance MIDI note extraction and streaming loader.
//!
//! This module provides advanced MIDI loading capabilities learned from memory-efficient
//! MIDI visualization applications, including:
//!
//! - **Compact Note Representation**: 12-byte packed note structure (vs 24+ bytes standard)
//! - **Spatial Indexing**: Fast range queries for visible notes using chunk-based indexing
//! - **Streaming Loader**: Memory-mapped, lazy-loaded note extraction with bounded memory
//! - **Parallel Extraction**: Multi-threaded note parsing using rayon
//!
//! # Example
//!
//! ```rust,no_run
//! use midly::loader::{StreamingNoteLoader, NoteIndex};
//! use std::path::Path;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Stream notes from a large MIDI file
//! let mut loader = StreamingNoteLoader::open(Path::new("large_song.mid"))?;
//!
//! // Load notes visible in current window
//! let current_tick = 10000.0f32;
//! let ticks_per_screen = 5000.0f32;
//! let (notes, active_keys) = loader.prepare_frame(current_tick, ticks_per_screen);
//!
//! println!("Visible notes: {}", notes.len());
//! # Ok(())
//! # }
//! ```

use crate::event::{MetaMessage, MidiMessage, TrackEvent, TrackEventKind};

#[cfg(all(feature = "std", feature = "memmap"))]
use crate::{Header, Timing};

#[cfg(feature = "alloc")]
use alloc::vec::Vec;

#[cfg(feature = "alloc")]
use crate::fast_midi::{self, MidiEvent, TrackIter as FastTrackIter};

#[cfg(all(feature = "std", feature = "memmap"))]
use std::{cmp::Reverse, collections::BinaryHeap};

#[cfg(all(feature = "std", feature = "memmap"))]
use memmap2::Mmap;

#[cfg(all(feature = "std", feature = "memmap"))]
use std::{collections::VecDeque, fs::File, path::Path};

#[cfg(feature = "parallel")]
use rayon::prelude::*;

/// A compact, packed representation of a MIDI note.
///
/// This structure uses only **14 bytes** compared to 24+ bytes for a typical
/// `(start_tick, end_tick, key, velocity, channel, track)` tuple.
///
/// The `#[repr(C, packed)]` layout ensures optimal cache efficiency when
/// processing large MIDI files with millions of notes.
///
/// `release_velocity` (NoteOff vel) defaults to 0 — 黑乐谱场景绝大多数为 0，
/// 如需极致内存可由调用方稀疏存储非零值。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(C, packed)]
pub struct PackedNote {
    /// Start time in MIDI ticks (4 bytes)
    pub start_tick: u32,
    /// End time in MIDI ticks (4 bytes)
    pub end_tick: u32,
    /// MIDI key number (0-127) (1 byte)
    pub key: u8,
    /// Note attack velocity / NoteOn velocity (0-127) (1 byte)
    pub velocity: u8,
    /// Note release velocity / NoteOff velocity (0-127) (1 byte)
    pub release_velocity: u8,
    /// MIDI channel (0-15) (1 byte)
    pub channel: u8,
    /// Track index, supports up to 65535 tracks (2 bytes)
    pub track: u16,
}

/// A compact, packed representation of a MIDI control event (CC / PC / PB).
///
/// Uses **10 bytes** to store control changes, program changes, and pitch bend events.
/// The `#[repr(C, packed)]` layout ensures no padding overhead.
///
/// # Encoding
/// - `kind`: 0 = ControlChange, 1 = ProgramChange, 2 = PitchBend
/// - `param` encoding:
///   - CC: `(controller << 8) | value`
///   - PC: `program`
///   - PB: raw 14-bit value (0..16383)
///   - ChannelAT: `velocity`
///   - PolyAT: `(key << 8) | velocity`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(C, packed)]
pub struct PackedControlEvent {
    /// Event time in MIDI ticks (4 bytes)
    pub tick: u32,
    /// Track index, supports up to 65535 tracks (2 bytes)
    pub track: u16,
    /// Event kind: 0=CC, 1=PC, 2=PB, 3=ChannelAT, 4=PolyAT (1 byte)
    pub kind: u8,
    /// MIDI channel (0-15) (1 byte)
    pub channel: u8,
    /// Parameter value (2 bytes) — see struct docs for encoding
    pub param: u16,
}

impl PackedControlEvent {
    /// Create a new packed control change event.
    #[inline]
    pub fn control_change(tick: u32, track: u16, channel: u8, controller: u8, value: u8) -> Self {
        Self {
            tick,
            track,
            kind: 0,
            channel: channel & 0x0F,
            param: ((controller as u16) << 8) | (value as u16),
        }
    }

    /// Create a new packed program change event.
    #[inline]
    pub fn program_change(tick: u32, track: u16, channel: u8, program: u8) -> Self {
        Self {
            tick,
            track,
            kind: 1,
            channel: channel & 0x0F,
            param: program as u16,
        }
    }

    /// Create a new packed pitch bend event.
    #[inline]
    pub fn pitch_bend(tick: u32, track: u16, channel: u8, bend: u16) -> Self {
        Self {
            tick,
            track,
            kind: 2,
            channel: channel & 0x0F,
            param: bend & 0x3FFF,
        }
    }

    /// Create a new packed channel aftertouch event.
    #[inline]
    pub fn channel_aftertouch(tick: u32, track: u16, channel: u8, velocity: u8) -> Self {
        Self {
            tick,
            track,
            kind: 3,
            channel: channel & 0x0F,
            param: (velocity & 0x7F) as u16,
        }
    }

    /// Create a new packed polyphonic aftertouch event.
    #[inline]
    pub fn poly_aftertouch(tick: u32, track: u16, channel: u8, key: u8, velocity: u8) -> Self {
        Self {
            tick,
            track,
            kind: 4,
            channel: channel & 0x0F,
            param: (((key & 0x7F) as u16) << 8) | ((velocity & 0x7F) as u16),
        }
    }

    /// Decode parameter as CC controller and value.
    #[inline]
    pub fn as_control_change(&self) -> (u8, u8) {
        ((self.param >> 8) as u8, (self.param & 0xFF) as u8)
    }

    /// Decode parameter as program number.
    #[inline]
    pub fn as_program_change(&self) -> u8 {
        (self.param & 0x7F) as u8
    }

    /// Decode parameter as pitch bend value (normalized to -1.0..1.0).
    #[inline]
    pub fn as_pitch_bend(&self) -> f32 {
        let raw = self.param as i16 - 8192;
        raw as f32 / 8192.0
    }

    /// Decode parameter as channel aftertouch velocity.
    #[inline]
    pub fn as_channel_aftertouch(&self) -> u8 {
        (self.param & 0x7F) as u8
    }

    /// Decode parameter as polyphonic aftertouch (key, velocity).
    #[inline]
    pub fn as_poly_aftertouch(&self) -> (u8, u8) {
        ((self.param >> 8) as u8, (self.param & 0xFF) as u8)
    }
}

/// A compact, packed representation of a MIDI time signature meta event.
///
/// Uses **6 bytes** to store time signature events. The denominator is kept in its raw
/// MIDI form (a power-of-two exponent), allowing callers to apply their own conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(C, packed)]
pub struct PackedTimeSignatureEvent {
    /// Event time in MIDI ticks (4 bytes)
    pub tick: u32,
    /// Time signature numerator (1 byte)
    pub numerator: u8,
    /// Time signature denominator as a power-of-two exponent (1 byte).
    /// For example, a denominator of `2` means 1/4, `3` means 1/8.
    pub denominator: u8,
}

impl PackedTimeSignatureEvent {
    /// Create a new packed time signature event.
    #[inline]
    pub fn new(tick: u32, numerator: u8, denominator: u8) -> Self {
        Self {
            tick,
            numerator,
            denominator,
        }
    }
}

/// A compact, packed representation of a MIDI key signature meta event.
///
/// Uses **6 bytes** to store key signature events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(C, packed)]
pub struct PackedKeySignatureEvent {
    /// Event time in MIDI ticks (4 bytes)
    pub tick: u32,
    /// Number of sharps (positive) or flats (negative), range -7..=7 (1 byte signed)
    pub sharps: i8,
    /// True if minor, false if major (1 byte)
    pub is_minor: bool,
}

impl PackedKeySignatureEvent {
    /// Create a new packed key signature event.
    #[inline]
    pub fn new(tick: u32, sharps: i8, is_minor: bool) -> Self {
        Self {
            tick,
            sharps,
            is_minor,
        }
    }
}

/// A lightweight borrowed text meta event (lyric / marker / text / track name / ...).
///
/// The text payload is **borrowed** from the original MIDI byte slice to avoid copies
/// during the parse pass. Callers that need to persist the data must copy it out in
/// the callback. This keeps the 100 GB MIDI path from allocating gigabytes of text
/// unless the consumer explicitly asks for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PackedTextEvent<'a> {
    /// Event time in MIDI ticks
    pub tick: u32,
    /// Track index
    pub track: u16,
    /// Raw text payload (encoding depends on the source file)
    pub text: &'a [u8],
}

impl<'a> PackedTextEvent<'a> {
    /// Create a new borrowed text event.
    #[inline]
    pub fn new(tick: u32, track: u16, text: &'a [u8]) -> Self {
        Self { tick, track, text }
    }
}

/// A lightweight borrowed text meta event with its MIDI meta type.
///
/// Covers text-class metas that previously fell into `_ => {}` and were
/// silently dropped: `0x01` Text, `0x02` Copyright, `0x04` InstrumentName,
/// `0x07` CuePoint, `0x08` ProgramName, `0x09` DeviceName.
/// Excluded by design: `0x03` TrackName (handled by `scan_track_names`
/// downstream), `0x05`/`0x06` (lyrics/markers keep their own channels).
///
/// Payload is **borrowed** like [`PackedTextEvent`]: callers copy only what
/// they persist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PackedTextMetaEvent<'a> {
    /// Event time in MIDI ticks
    pub tick: u32,
    /// Track index
    pub track: u16,
    /// MIDI meta type (0x01 / 0x02 / 0x04 / 0x07 / 0x08 / 0x09)
    pub meta_type: u8,
    /// Raw text payload (encoding depends on the source file)
    pub text: &'a [u8],
}

impl<'a> PackedTextMetaEvent<'a> {
    /// Create a new borrowed typed text event.
    #[inline]
    pub fn new(tick: u32, track: u16, meta_type: u8, text: &'a [u8]) -> Self {
        Self {
            tick,
            track,
            meta_type,
            text,
        }
    }
}

/// A lightweight borrowed System Exclusive event.
///
/// The data payload is **borrowed** from the original MIDI byte slice. SysEx events can
/// be large, so this is critical for the 100 GB MIDI path: the caller decides whether to
/// copy, stream to disk, or ignore the payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PackedSysExEvent<'a> {
    /// Event time in MIDI ticks
    pub tick: u32,
    /// Track index
    pub track: u16,
    /// Raw SysEx payload (does not include the 0xF0 prefix)
    pub data: &'a [u8],
}

impl<'a> PackedSysExEvent<'a> {
    /// Create a new borrowed SysEx event.
    #[inline]
    pub fn new(tick: u32, track: u16, data: &'a [u8]) -> Self {
        Self { tick, track, data }
    }
}

impl PackedNote {
    /// Create a new packed note (release velocity defaults to 0).
    ///
    /// 保持 6 参签名兼容历史调用方；需要释放力度时用
    /// [`Self::new_with_release`] 或 [`Self::with_release_velocity`]。
    #[inline]
    pub fn new(
        start_tick: u32,
        end_tick: u32,
        key: u8,
        velocity: u8,
        channel: u8,
        track: u16,
    ) -> Self {
        Self {
            start_tick,
            end_tick,
            key,
            velocity: velocity & 0x7F,
            release_velocity: 0,
            channel: channel & 0x0F,
            track,
        }
    }

    /// Create a new packed note with explicit release (NoteOff) velocity.
    #[inline]
    pub fn new_with_release(
        start_tick: u32,
        end_tick: u32,
        key: u8,
        velocity: u8,
        release_velocity: u8,
        channel: u8,
        track: u16,
    ) -> Self {
        Self {
            start_tick,
            end_tick,
            key,
            velocity: velocity & 0x7F,
            release_velocity: release_velocity & 0x7F,
            channel: channel & 0x0F,
            track,
        }
    }

    /// Builder: attach a release (NoteOff) velocity.
    #[inline]
    pub fn with_release_velocity(mut self, release_velocity: u8) -> Self {
        self.release_velocity = release_velocity & 0x7F;
        self
    }

    /// Get note duration in ticks.
    #[inline]
    pub fn duration(&self) -> u32 {
        self.end_tick.saturating_sub(self.start_tick)
    }

    /// Check if this note overlaps with the given tick range.
    #[inline]
    pub fn overlaps(&self, start: f32, end: f32) -> bool {
        let s = self.start_tick as f32;
        let e = self.end_tick as f32;
        s <= end && e >= start
    }
}

/// Spatial index for fast note range queries.
///
/// `NoteIndex` organizes notes into chunks based on their start tick,
/// enabling O(1) lookup of notes that may be visible in a given time range.
///
/// This is particularly useful for MIDI visualization applications that need
/// to quickly find notes visible on screen without scanning the entire file.
#[derive(Clone, Debug)]
#[cfg(feature = "alloc")]
pub struct NoteIndex {
    /// Notes sorted by start_tick
    notes: Vec<PackedNote>,
    /// Chunk index: chunk_starts[i] = first note index with start_tick >= i * chunk_ticks
    chunk_starts: Vec<usize>,
    /// Size of each chunk in ticks
    chunk_ticks: u32,
    /// Maximum end_tick across all notes
    max_end_tick: u32,
}

#[cfg(feature = "alloc")]
impl NoteIndex {
    /// Build a spatial index from a collection of notes.
    ///
    /// # Arguments
    ///
    /// * `notes` - Notes to index (will be sorted by start_tick)
    /// * `chunk_ticks` - Size of each chunk in ticks (e.g., 1000 for 1000 ticks per chunk)
    ///
    /// # Example
    ///
    /// ```rust
    /// use midly::loader::{PackedNote, NoteIndex};
    ///
    /// let notes = vec![
    ///     PackedNote::new(0, 100, 60, 100, 0, 0),
    ///     PackedNote::new(50, 150, 64, 100, 0, 0),
    ///     PackedNote::new(200, 300, 67, 100, 0, 1),
    /// ];
    ///
    /// let index = NoteIndex::build(notes, 100);
    /// ```
    pub fn build(mut notes: Vec<PackedNote>, chunk_ticks: u32) -> Self {
        // Prevent division by zero - clamp to at least 1
        let chunk_ticks = chunk_ticks.max(1);

        // Sort notes by start_tick for efficient range queries
        notes.sort_unstable_by_key(|n| n.start_tick);

        let max_end_tick = notes.iter().map(|n| n.end_tick).max().unwrap_or(0);
        let num_chunks = if max_end_tick > 0 {
            ((max_end_tick / chunk_ticks) + 2) as usize
        } else {
            1
        };

        // Initialize with "past end" sentinel
        let mut chunk_starts = vec![notes.len(); num_chunks];

        // One-pass index building
        for (i, note) in notes.iter().enumerate() {
            let chunk = (note.start_tick / chunk_ticks) as usize;
            if chunk < chunk_starts.len() && i < chunk_starts[chunk] {
                chunk_starts[chunk] = i;
            }
        }

        // Backfill to ensure each chunk points to a valid position
        let mut last = notes.len();
        for i in (0..chunk_starts.len()).rev() {
            if chunk_starts[i] > last {
                chunk_starts[i] = last;
            } else {
                last = chunk_starts[i];
            }
        }

        Self {
            notes,
            chunk_starts,
            chunk_ticks,
            max_end_tick,
        }
    }

    /// Get the starting index for notes that might overlap with the given tick range.
    ///
    /// This returns a conservative estimate - some notes before the returned index
    /// may also overlap (due to long notes that started earlier).
    #[inline]
    pub fn get_start_index(&self, min_tick: f32) -> usize {
        if min_tick <= 0.0 {
            return 0;
        }
        let chunk = (min_tick as u32 / self.chunk_ticks) as usize;
        if chunk >= self.chunk_starts.len() {
            self.notes.len()
        } else {
            // Look back 2 chunks to ensure we don't miss long notes
            let safe_chunk = chunk.saturating_sub(2);
            self.chunk_starts[safe_chunk]
        }
    }

    /// Get all notes in the index.
    #[inline]
    pub fn notes(&self) -> &[PackedNote] {
        &self.notes
    }

    /// Get the number of notes.
    #[inline]
    pub fn len(&self) -> usize {
        self.notes.len()
    }

    /// Check if the index is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }

    /// Get the maximum end tick across all notes.
    #[inline]
    pub fn max_end_tick(&self) -> u32 {
        self.max_end_tick
    }

    /// Query notes that overlap with the given tick range.
    ///
    /// Returns an iterator over notes that may be visible in [start_tick, end_tick].
    pub fn query_range(&self, start_tick: f32, end_tick: f32) -> impl Iterator<Item = &PackedNote> {
        let start_idx = self.get_start_index(start_tick);
        self.notes[start_idx..]
            .iter()
            .take_while(move |n| (n.start_tick as f32) <= end_tick)
            .filter(move |n| n.overlaps(start_tick, end_tick))
    }
}

/// Temporary storage for notes being built during parsing.
#[cfg(feature = "alloc")]
#[derive(Clone, Copy, Debug)]
struct ActiveNote {
    start_tick: u32,
    velocity: u8,
}

#[cfg(feature = "alloc")]
#[derive(Debug)]
struct NoteAccumulator {
    notes: Vec<PackedNote>,
    tempo_changes: Vec<(u32, f32)>,
    active_notes: [[Option<(u32, u8)>; 256]; 16],
    current_tick: u32,
    track_idx: u16,
}

#[cfg(feature = "alloc")]
impl NoteAccumulator {
    fn new(track_idx: u16) -> Self {
        Self {
            notes: Vec::with_capacity(512),
            tempo_changes: Vec::new(),
            active_notes: [[None; 256]; 16],
            current_tick: 0,
            track_idx,
        }
    }

    #[inline]
    fn advance(&mut self, delta: u32) {
        self.current_tick = self.current_tick.saturating_add(delta);
    }

    fn note_on(&mut self, key: u8, velocity: u8, channel: u8) {
        let ch = channel.min(15) as usize;
        let key_idx = key as usize;
        if key_idx >= 256 {
            return;
        }

        if let Some((start_tick, prev_velocity)) = self.active_notes[ch][key_idx].take() {
            // 重触发关闭旧音符：新 NoteOn 的 velocity 是新音符的攻击力度，
            // 不是旧音符的释放力度；旧音符释放力度记 0。
            // 只有 velocity == 0（即 NoteOn-as-NoteOff）时才透传释放力度（恒为 0）。
            let release = if velocity == 0 { velocity } else { 0 };
            self.notes.push(PackedNote::new_with_release(
                start_tick,
                self.current_tick,
                key,
                prev_velocity,
                release,
                channel,
                self.track_idx,
            ));
        }

        if velocity > 0 {
            self.active_notes[ch][key_idx] = Some((self.current_tick, velocity));
        }
    }

    fn note_off(&mut self, key: u8, channel: u8, release_velocity: u8) {
        let ch = channel.min(15) as usize;
        let key_idx = key as usize;
        if key_idx >= 256 {
            return;
        }
        if let Some((start_tick, velocity)) = self.active_notes[ch][key_idx].take() {
            self.notes.push(PackedNote::new_with_release(
                start_tick,
                self.current_tick,
                key,
                velocity,
                release_velocity,
                channel,
                self.track_idx,
            ));
        }
    }

    fn tempo_microseconds(&mut self, microseconds: u32) {
        if microseconds > 0 {
            let bpm = 60_000_000.0 / microseconds as f32;
            self.tempo_changes.push((self.current_tick, bpm));
        }
    }

    fn finish(mut self) -> (Vec<PackedNote>, Vec<(u32, f32)>) {
        for ch in 0..16 {
            for key in 0..256 {
                if let Some((start_tick, velocity)) = self.active_notes[ch][key].take() {
                    self.notes.push(PackedNote::new(
                        start_tick,
                        self.current_tick,
                        key as u8,
                        velocity,
                        ch as u8,
                        self.track_idx,
                    ));
                }
            }
        }
        (self.notes, self.tempo_changes)
    }
}

/// Accumulator for MIDI control events (CC / PC / PB).
#[cfg(feature = "alloc")]
#[derive(Debug)]
struct ControlEventAccumulator {
    events: Vec<PackedControlEvent>,
    current_tick: u32,
    track_idx: u16,
}

#[cfg(feature = "alloc")]
impl ControlEventAccumulator {
    fn new(track_idx: u16) -> Self {
        Self {
            events: Vec::with_capacity(64),
            current_tick: 0,
            track_idx,
        }
    }

    #[inline]
    fn advance(&mut self, delta: u32) {
        self.current_tick = self.current_tick.saturating_add(delta);
    }

    #[inline]
    fn control_change(&mut self, channel: u8, controller: u8, value: u8) {
        self.events.push(PackedControlEvent::control_change(
            self.current_tick,
            self.track_idx,
            channel,
            controller,
            value,
        ));
    }

    #[inline]
    fn program_change(&mut self, channel: u8, program: u8) {
        self.events.push(PackedControlEvent::program_change(
            self.current_tick,
            self.track_idx,
            channel,
            program,
        ));
    }

    #[inline]
    fn pitch_bend(&mut self, channel: u8, bend: u16) {
        self.events.push(PackedControlEvent::pitch_bend(
            self.current_tick,
            self.track_idx,
            channel,
            bend,
        ));
    }

    #[inline]
    fn channel_aftertouch(&mut self, channel: u8, velocity: u8) {
        self.events.push(PackedControlEvent::channel_aftertouch(
            self.current_tick,
            self.track_idx,
            channel,
            velocity,
        ));
    }

    #[inline]
    fn poly_aftertouch(&mut self, channel: u8, key: u8, velocity: u8) {
        self.events.push(PackedControlEvent::poly_aftertouch(
            self.current_tick,
            self.track_idx,
            channel,
            key,
            velocity,
        ));
    }

    fn finish(self) -> Vec<PackedControlEvent> {
        self.events
    }
}

/// A track cursor for streaming note extraction.
///
/// This provides a low-level iterator over MIDI events from a single track,
/// tracking active notes and emitting `PackedNote` objects when notes end.
#[cfg(feature = "alloc")]
#[derive(Debug)]
pub struct NoteTrackCursor<'a> {
    #[allow(dead_code)]
    events: crate::smf::EventIter<'a>,
    active_notes: [[Option<ActiveNote>; 256]; 16],
    current_tick: u32,
    track_idx: u16,
}

#[cfg(feature = "alloc")]
impl<'a> NoteTrackCursor<'a> {
    /// Create a new note cursor from an event iterator.
    pub fn new(events: crate::smf::EventIter<'a>, track_idx: u16) -> Self {
        Self {
            events,
            active_notes: [[None; 256]; 16],
            current_tick: 0,
            track_idx,
        }
    }

    /// Get the current tick position.
    #[inline]
    pub fn current_tick(&self) -> u32 {
        self.current_tick
    }

    /// Close all active notes, returning them as completed notes.
    pub fn close_all_notes(&mut self) -> Vec<PackedNote> {
        let mut completed = Vec::new();
        for ch in 0..16 {
            for key in 0..256 {
                if let Some(active) = self.active_notes[ch][key].take() {
                    completed.push(PackedNote::new(
                        active.start_tick,
                        self.current_tick,
                        key as u8,
                        active.velocity,
                        ch as u8,
                        self.track_idx,
                    ));
                }
            }
        }
        completed
    }
}

/// Extract all notes from a parsed SMF using parallel processing.
///
/// This function processes all tracks in parallel using rayon (if the `parallel`
/// feature is enabled), making it efficient for large MIDI files.
///
/// # Arguments
///
/// * `smf` - The parsed Standard MIDI File
///
/// Returns a tuple of `(notes, tempo_changes)` where:
/// - `notes` is a `Vec<PackedNote>` of all notes in the file
/// - `tempo_changes` is a `Vec<(tick, bpm)>` of tempo change events
#[cfg(feature = "alloc")]
pub fn extract_notes(smf: &crate::Smf) -> (Vec<PackedNote>, Vec<(u32, f32)>) {
    extract_notes_internal(smf)
}

/// Extract all notes and build a spatial index.
///
/// This is a convenience function that extracts notes and builds a `NoteIndex`
/// for efficient range queries.
#[cfg(feature = "alloc")]
pub fn extract_notes_indexed(smf: &crate::Smf, chunk_ticks: u32) -> (NoteIndex, Vec<(u32, f32)>) {
    let (notes, tempo_changes) = extract_notes_internal(smf);
    let index = NoteIndex::build(notes, chunk_ticks);
    (index, tempo_changes)
}

#[cfg(feature = "alloc")]
fn extract_notes_internal(smf: &crate::Smf) -> (Vec<PackedNote>, Vec<(u32, f32)>) {
    let track_results: Vec<(Vec<PackedNote>, Vec<(u32, f32)>)> = {
        #[cfg(feature = "parallel")]
        {
            smf.tracks
                .par_iter()
                .enumerate()
                .map(|(track_idx, track)| parse_track_notes(track, track_idx as u16))
                .collect()
        }
        #[cfg(not(feature = "parallel"))]
        {
            smf.tracks
                .iter()
                .enumerate()
                .map(|(track_idx, track)| parse_track_notes(track, track_idx as u16))
                .collect()
        }
    };

    // Merge results
    let total_notes: usize = track_results.iter().map(|(n, _)| n.len()).sum();
    let mut all_notes = Vec::with_capacity(total_notes);
    let mut all_tempo_changes: Vec<(u32, f32)> = Vec::new();

    for (notes, tempo_changes) in track_results {
        all_notes.extend(notes);
        for tempo in tempo_changes {
            if !all_tempo_changes.iter().any(|(t, _)| *t == tempo.0) {
                all_tempo_changes.push(tempo);
            }
        }
    }

    // MIDI default is 120 BPM; only insert default if no tempo at tick 0 exists
    if !all_tempo_changes.iter().any(|(t, _)| *t == 0) {
        all_tempo_changes.push((0u32, 120.0f32));
    }

    all_tempo_changes.sort_unstable_by_key(|&(t, _)| t);
    (all_notes, all_tempo_changes)
}

#[cfg(feature = "alloc")]
fn parse_track_notes(track: &[TrackEvent], track_idx: u16) -> (Vec<PackedNote>, Vec<(u32, f32)>) {
    let mut acc = NoteAccumulator::new(track_idx);

    for event in track {
        acc.advance(event.delta.as_int());

        match &event.kind {
            TrackEventKind::Midi { channel, message } => {
                let ch = channel.as_int();
                match message {
                    MidiMessage::NoteOn { key, vel } => {
                        acc.note_on(*key, vel.as_int(), ch);
                    }
                    MidiMessage::NoteOff { key, vel } => acc.note_off(*key, ch, vel.as_int()),
                    _ => {}
                }
            }
            TrackEventKind::Meta(MetaMessage::Tempo(tempo)) => {
                acc.tempo_microseconds(tempo.as_int());
            }
            _ => {}
        }
    }

    acc.finish()
}

#[cfg(feature = "alloc")]
fn parse_track_notes_and_control_events(
    track: &[TrackEvent],
    track_idx: u16,
) -> (Vec<PackedNote>, Vec<(u32, f32)>, Vec<PackedControlEvent>) {
    let mut note_acc = NoteAccumulator::new(track_idx);
    let mut ctrl_acc = ControlEventAccumulator::new(track_idx);

    for event in track {
        let delta = event.delta.as_int();
        note_acc.advance(delta);
        ctrl_acc.advance(delta);

        match &event.kind {
            TrackEventKind::Midi { channel, message } => {
                let ch = channel.as_int();
                match message {
                    MidiMessage::NoteOn { key, vel } => {
                        note_acc.note_on(*key, vel.as_int(), ch);
                    }
                    MidiMessage::NoteOff { key, vel } => note_acc.note_off(*key, ch, vel.as_int()),
                    MidiMessage::Controller { controller, value } => {
                        ctrl_acc.control_change(ch, controller.as_int(), value.as_int());
                    }
                    MidiMessage::ProgramChange { program } => {
                        ctrl_acc.program_change(ch, program.as_int());
                    }
                    MidiMessage::PitchBend { bend } => {
                        // PitchBend::as_int() 是 [-8192,8191] 偏差语义；
                        // PackedControlEvent 编码契约是原始 14-bit（0..16383），
                        // 与快路径 (msb<<7)|lsb 一致。误用 as_int() 会让负偏差
                        // wrap 成 u16 再被 &0x3FFF 掩码，造成 Smf/快路径分裂。
                        let bend_u16 = bend.0.as_int();
                        ctrl_acc.pitch_bend(ch, bend_u16);
                    }
                    MidiMessage::Aftertouch { key, vel } => {
                        ctrl_acc.poly_aftertouch(ch, key.as_int(), vel.as_int());
                    }
                    MidiMessage::ChannelAftertouch { vel } => {
                        ctrl_acc.channel_aftertouch(ch, vel.as_int());
                    }
                    _ => {}
                }
            }
            TrackEventKind::Meta(MetaMessage::Tempo(tempo)) => {
                note_acc.tempo_microseconds(tempo.as_int());
            }
            _ => {}
        }
    }

    let (notes, tempo_changes) = note_acc.finish();
    let control_events = ctrl_acc.finish();
    (notes, tempo_changes, control_events)
}

/// Extract notes AND control events from a parsed `Smf`.
///
/// This is the `Smf`-based variant of [`extract_notes_and_control_events_from_bytes`].
#[cfg(feature = "alloc")]
pub fn extract_notes_and_control_events(
    smf: &crate::Smf,
) -> (Vec<PackedNote>, Vec<(u32, f32)>, Vec<PackedControlEvent>) {
    let track_results: Vec<(Vec<PackedNote>, Vec<(u32, f32)>, Vec<PackedControlEvent>)> = {
        #[cfg(feature = "parallel")]
        {
            smf.tracks
                .par_iter()
                .enumerate()
                .map(|(track_idx, track)| {
                    parse_track_notes_and_control_events(track, track_idx as u16)
                })
                .collect()
        }
        #[cfg(not(feature = "parallel"))]
        {
            smf.tracks
                .iter()
                .enumerate()
                .map(|(track_idx, track)| {
                    parse_track_notes_and_control_events(track, track_idx as u16)
                })
                .collect()
        }
    };

    let total_notes: usize = track_results.iter().map(|(n, _, _)| n.len()).sum();
    let total_ctrl: usize = track_results.iter().map(|(_, _, c)| c.len()).sum();
    let mut all_notes = Vec::with_capacity(total_notes);
    let mut all_tempo_changes: Vec<(u32, f32)> = Vec::new();
    let mut all_control_events = Vec::with_capacity(total_ctrl);

    for (notes, tempo_changes, control_events) in track_results {
        all_notes.extend(notes);
        all_control_events.extend(control_events);
        for tempo in tempo_changes {
            if !all_tempo_changes.iter().any(|(t, _)| *t == tempo.0) {
                all_tempo_changes.push(tempo);
            }
        }
    }

    if !all_tempo_changes.iter().any(|(t, _)| *t == 0) {
        all_tempo_changes.push((0u32, 120.0f32));
    }

    all_tempo_changes.sort_unstable_by_key(|&(t, _)| t);
    all_control_events.sort_unstable_by_key(|e| e.tick);
    (all_notes, all_tempo_changes, all_control_events)
}

/// Extract notes directly from raw MIDI bytes without creating an intermediate `Smf`.
///
/// This function provides the most memory-efficient way to extract notes from a MIDI file.
/// Unlike `extract_notes(&smf)` which requires the `Smf` to store all events in memory,
/// this function parses tracks lazily and only keeps note data.
///
/// # Memory Efficiency
///
/// - `extract_notes(&smf)`: Events (2-3GB) + Notes (550MB) = ~4GB total
/// - `extract_notes_from_bytes(bytes)`: Notes only (550MB) = ~550MB total
///
/// # Example
///
/// ```rust,no_run
/// use midly::loader::extract_notes_from_bytes;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let bytes = std::fs::read("large_song.mid")?;
/// let (notes, tempo_changes) = extract_notes_from_bytes(&bytes)?;
/// println!("Extracted {} notes", notes.len());
/// # Ok(())
/// # }
/// ```
#[cfg(feature = "alloc")]
pub fn extract_notes_from_bytes(bytes: &[u8]) -> crate::Result<(Vec<PackedNote>, Vec<(u32, f32)>)> {
    // Convert MIDI 2.0 UMP chunks to MTrk before parsing
    let data = crate::ump::preprocess_smf(bytes);
    let (_header, tracks_count, _division, raw) = fast_midi::parse_header(&data)?;
    let tracks = fast_midi::iter_tracks_from_data(raw, tracks_count);

    // Use parallel extraction if available
    let track_results: Vec<(Vec<PackedNote>, Vec<(u32, f32)>)> = {
        #[cfg(feature = "parallel")]
        {
            use rayon::prelude::*;
            tracks
                .into_par_iter()
                .enumerate()
                .map(|(track_idx, events)| parse_fast_track_notes(events, track_idx as u16))
                .collect()
        }
        #[cfg(not(feature = "parallel"))]
        {
            tracks
                .into_iter()
                .enumerate()
                .map(|(track_idx, events)| parse_fast_track_notes(events, track_idx as u16))
                .collect()
        }
    };

    // Merge results
    let total_notes: usize = track_results.iter().map(|(n, _)| n.len()).sum();
    let mut all_notes = Vec::with_capacity(total_notes);
    let mut all_tempo_changes: Vec<(u32, f32)> = Vec::new();

    for (notes, tempo_changes) in track_results {
        all_notes.extend(notes);
        for tempo in tempo_changes {
            if !all_tempo_changes.iter().any(|(t, _)| *t == tempo.0) {
                all_tempo_changes.push(tempo);
            }
        }
    }

    // MIDI default is 120 BPM; only insert default if no tempo at tick 0 exists
    if !all_tempo_changes.iter().any(|(t, _)| *t == 0) {
        all_tempo_changes.push((0u32, 120.0f32));
    }

    all_tempo_changes.sort_unstable_by_key(|&(t, _)| t);
    Ok((all_notes, all_tempo_changes))
}

/// Extract notes AND control events (CC / PC / PB) from raw MIDI bytes.
///
/// This is the control-event-aware variant of [`extract_notes_from_bytes`].
/// It returns an additional `Vec<PackedControlEvent>` containing all
/// ControlChange, ProgramChange and PitchBend events found in the file.
///
/// # Returns
///
/// A tuple of `(notes, tempo_changes, control_events)` where:
/// - `notes` is a `Vec<PackedNote>` of all notes in the file
/// - `tempo_changes` is a `Vec<(tick, bpm)>` of tempo change events
/// - `control_events` is a `Vec<PackedControlEvent>` of CC/PC/PB events
#[cfg(feature = "alloc")]
pub fn extract_notes_and_control_events_from_bytes(
    bytes: &[u8],
) -> crate::Result<(Vec<PackedNote>, Vec<(u32, f32)>, Vec<PackedControlEvent>)> {
    // Convert MIDI 2.0 UMP chunks to MTrk before parsing
    let data = crate::ump::preprocess_smf(bytes);
    let (_header, tracks_count, _division, raw) = fast_midi::parse_header(&data)?;
    let tracks = fast_midi::iter_tracks_from_data(raw, tracks_count);

    // Use parallel extraction if available
    let track_results: Vec<(Vec<PackedNote>, Vec<(u32, f32)>, Vec<PackedControlEvent>)> = {
        #[cfg(feature = "parallel")]
        {
            use rayon::prelude::*;
            tracks
                .into_par_iter()
                .enumerate()
                .map(|(track_idx, events)| {
                    parse_fast_track_notes_and_control_events(events, track_idx as u16)
                })
                .collect()
        }
        #[cfg(not(feature = "parallel"))]
        {
            tracks
                .into_iter()
                .enumerate()
                .map(|(track_idx, events)| {
                    parse_fast_track_notes_and_control_events(events, track_idx as u16)
                })
                .collect()
        }
    };

    // Merge results
    let total_notes: usize = track_results.iter().map(|(n, _, _)| n.len()).sum();
    let total_ctrl: usize = track_results.iter().map(|(_, _, c)| c.len()).sum();
    let mut all_notes = Vec::with_capacity(total_notes);
    let mut all_tempo_changes: Vec<(u32, f32)> = Vec::new();
    let mut all_control_events = Vec::with_capacity(total_ctrl);

    for (notes, tempo_changes, control_events) in track_results {
        all_notes.extend(notes);
        all_control_events.extend(control_events);
        for tempo in tempo_changes {
            if !all_tempo_changes.iter().any(|(t, _)| *t == tempo.0) {
                all_tempo_changes.push(tempo);
            }
        }
    }

    // MIDI default is 120 BPM; only insert default if no tempo at tick 0 exists
    if !all_tempo_changes.iter().any(|(t, _)| *t == 0) {
        all_tempo_changes.push((0u32, 120.0f32));
    }

    all_tempo_changes.sort_unstable_by_key(|&(t, _)| t);
    all_control_events.sort_unstable_by_key(|e| e.tick);
    Ok((all_notes, all_tempo_changes, all_control_events))
}

/// 按轨提取结果，用于避免全局 `Vec<PackedNote>` 大数组的随机访问开销。
///
/// 与全局合并函数不同，该结构保留每轨数据的独立性，使得下游消费者
/// 可以为每轨分配独立的连续 Arena，而不是在数 GB 的全局 notes 上构建索引。
#[cfg(feature = "alloc")]
#[derive(Clone, Debug)]
pub struct TrackExtractResult {
    /// 该轨解析出的音符，按开始 tick 有序。
    pub notes: Vec<PackedNote>,
    /// 该轨解析出的速度变化事件。
    pub tempo_changes: Vec<(u32, f32)>,
    /// 该轨解析出的控制 / 音色 / 弯音事件。
    pub control_events: Vec<PackedControlEvent>,
}

/// 直接从 MIDI 字节流按轨提取音符与控制事件。
///
/// 这是 [`extract_notes_and_control_events_from_bytes`] 的按轨版本。
/// 它避免了将所有音轨合并到单个全局 `Vec<PackedNote>` 所带来的内存与缓存开销；
/// 调用方可以独立处理每个音轨，只在最后合并较小的速度/控制事件列表。
#[cfg(feature = "alloc")]
pub fn extract_notes_and_control_events_per_track_from_bytes(
    bytes: &[u8],
) -> crate::Result<Vec<TrackExtractResult>> {
    let data = crate::ump::preprocess_smf(bytes);
    let (_header, tracks_count, _division, raw) = fast_midi::parse_header(&data)?;
    let tracks = fast_midi::iter_tracks_from_data(raw, tracks_count);

    let track_results: Vec<(Vec<PackedNote>, Vec<(u32, f32)>, Vec<PackedControlEvent>)> = {
        #[cfg(feature = "parallel")]
        {
            use rayon::prelude::*;
            tracks
                .into_par_iter()
                .enumerate()
                .map(|(track_idx, events)| {
                    parse_fast_track_notes_and_control_events(events, track_idx as u16)
                })
                .collect()
        }
        #[cfg(not(feature = "parallel"))]
        {
            tracks
                .into_iter()
                .enumerate()
                .map(|(track_idx, events)| {
                    parse_fast_track_notes_and_control_events(events, track_idx as u16)
                })
                .collect()
        }
    };

    let results: Vec<TrackExtractResult> = track_results
        .into_iter()
        .map(
            |(notes, tempo_changes, control_events)| TrackExtractResult {
                notes,
                tempo_changes,
                control_events,
            },
        )
        .collect();

    Ok(results)
}

/// 直接从 MIDI 字节流按轨提取音符、控制事件与时间签名，通过回调逐轨处理。
///
/// 与 [`extract_notes_and_control_events_per_track_from_bytes`] 不同，该函数不会
/// 把所有音轨的 `Vec<PackedNote>` 同时保留在内存中；每解析完一轨就立即调用
/// `track_handler`，下游可立即转换并释放该轨数据，从而降低大文件加载的峰值内存。
///
/// 该版本使用 `rayon` 并行解析各轨，并通过 `Mutex` 保护回调，因此回调可能被
/// 以任意顺序、多线程并发调用。调用方应通过 `track_idx` 自行写入对应槽位，
/// 不依赖回调顺序。
///
/// 回调参数为 `(track_idx, notes, tempo_changes, control_events, time_signatures)`。
#[cfg(feature = "alloc")]
pub fn extract_notes_and_control_events_per_track_streaming_from_bytes<F>(
    bytes: &[u8],
    track_handler: F,
) -> crate::Result<()>
where
    F: FnMut(
            usize,
            Vec<PackedNote>,
            Vec<(u32, f32)>,
            Vec<PackedControlEvent>,
            Vec<PackedTimeSignatureEvent>,
        ) + Send,
{
    // parallel 构建：委托给无锁并行 API，再以 Mutex 把 `FnMut` 语义串行化
    // （与原实现等价：解析在锁外，回调在锁内）。
    #[cfg(feature = "parallel")]
    {
        let handler = std::sync::Mutex::new(track_handler);
        extract_notes_and_control_events_per_track_parallel_from_bytes(
            bytes,
            |track_idx, notes, tempo_changes, control_events, time_signatures| {
                let mut h = handler.lock().unwrap_or_else(|e| e.into_inner());
                h(
                    track_idx,
                    notes,
                    tempo_changes,
                    control_events,
                    time_signatures,
                );
            },
        )
    }
    #[cfg(not(feature = "parallel"))]
    {
        let mut handler = track_handler;
        let data = crate::ump::preprocess_smf(bytes);
        let (_header, tracks_count, _division, raw) = fast_midi::parse_header(&data)?;
        let tracks = fast_midi::iter_tracks_from_data(raw, tracks_count);
        for (track_idx, events) in tracks.into_iter().enumerate() {
            let (notes, tempo_changes, control_events, time_signatures) =
                parse_fast_track_notes_control_and_time_signature_events(events, track_idx as u16);
            handler(
                track_idx,
                notes,
                tempo_changes,
                control_events,
                time_signatures,
            );
        }
        Ok(())
    }
}

/// 单轨时间边界；EOT 是结构信息，不进入音频事件流（指导书 §2.5）。
///
/// 三个时间必须分离：
/// - [`eot_tick`](Self::eot_tick)：文件显式声明的轨道结束位置（首个 `FF 2F`）；
/// - [`content_end_tick`](Self::content_end_tick)：最后一个**非 EOT** 事件位置；
/// - [`physical_end_tick`](Self::physical_end_tick)：物理读取到的最后事件位置。
///
/// 解析器只报告边界；`audible_end_tick`（release / 效果器尾音）由音频引擎计算，
/// 不由解析器决定。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackTiming {
    /// 文件显式声明的 EOT 位置；未出现时为 `None`（缺 EOT 诊断）。
    pub eot_tick: Option<u32>,
    /// 物理读取到的最后事件位置，包括 EOT 之后发现的异常事件。
    pub physical_end_tick: u32,
    /// 最后一个非 EOT 事件的位置，用于有效内容范围。
    pub content_end_tick: u32,
    /// 是否在首个 EOT 后仍发现事件（兼容模式保留这些事件，仅作诊断；
    /// 重复 EOT 也计入）。
    pub has_events_after_eot: bool,
}

impl TrackTiming {
    /// 文件声明结尾；缺失 EOT 时退化为物理末尾。
    ///
    /// 调用方若需要区分「声明结尾」与「fallback」，应同时检查
    /// [`eot_tick`](Self::eot_tick) 是否为 `None`。
    pub fn declared_end_tick(&self) -> u32 {
        self.eot_tick.unwrap_or(self.physical_end_tick)
    }

    /// Strict 模式门控：`false` 表示该轨在 EOT 后仍有事件，严格消费者应拒绝。
    ///
    /// 解析器自身始终按兼容模式保留数据（不得静默丢数据），是否拒绝属产品策略。
    pub fn is_strict_clean(&self) -> bool {
        !self.has_events_after_eot
    }
}

/// All events extracted from a single track, using borrowed slices for text/SysEx data.
///
/// This is the most complete extraction interface in `midly`. It passes **borrowed**
/// byte slices for lyrics, markers, text-class metas and SysEx so that a 100 GB MIDI file with large
/// SysEx payloads does not force midly to allocate. The consumer copies only what it
/// needs to persist.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug)]
pub struct TrackAllEvents<'a> {
    /// Notes found in this track, sorted by start tick.
    pub notes: Vec<PackedNote>,
    /// Tempo changes found in this track.
    pub tempo_changes: Vec<(u32, f32)>,
    /// Time signatures found in this track.
    pub time_signatures: Vec<PackedTimeSignatureEvent>,
    /// Key signatures found in this track.
    pub key_signatures: Vec<PackedKeySignatureEvent>,
    /// Control / program / pitch-bend events found in this track.
    pub control_events: Vec<PackedControlEvent>,
    /// Lyric meta events (type 0x05), borrowed from the original byte slice.
    pub lyrics: Vec<PackedTextEvent<'a>>,
    /// Marker meta events (type 0x06), borrowed from the original byte slice.
    pub markers: Vec<PackedTextEvent<'a>>,
    /// Text-class meta events (0x01 Text / 0x02 Copyright / 0x04 InstrumentName /
    /// 0x07 CuePoint / 0x08 ProgramName / 0x09 DeviceName), borrowed.
    /// Each entry carries its own `meta_type` so callers can route or write back
    /// without a second parse pass.
    pub text_events: Vec<PackedTextMetaEvent<'a>>,
    /// System Exclusive events (0xF0 / 0xF7), borrowed from the original byte slice.
    pub sys_ex: Vec<PackedSysExEvent<'a>>,
    /// MIDI port meta event (FF 21) of this track — the **first** occurrence, if any.
    ///
    /// The MIDI 1.0 spec allows multiple MidiPort metas per track; the first one
    /// wins (matches the historical `Smf::parse`-based extraction which returned
    /// the first occurrence). `None` when the track has no MidiPort meta
    /// (callers should default to port 0).
    pub midi_port: Option<u8>,
    /// TrackName meta event (FF 03) of this track — the **first** occurrence, if any.
    ///
    /// 原始字节（未解码）：调用方自行按需解码（UTF-8/Shift-JIS/GBK…）。
    /// 这里直接带出可以免掉下游的第二次全量扫描——对没有 TrackName 的轨，
    /// 「扫描找名字」必须走完整条轨（1.25GB 素材实测占端到端墙钟 ~57%）。
    /// 空名称仍返回 `Some(&[])`，是否存在与是否为空由调用方区分。
    pub track_name: Option<&'a [u8]>,
    /// 轨道时间边界（EOT / 内容末尾 / 物理末尾），见 [`TrackTiming`]。
    ///
    /// EOT 本身不进入上述音频事件流：它只声明轨道结束位置。兼容模式下
    /// EOT 后事件照常保留（`has_events_after_eot` 标记），不静默丢数据。
    pub timing: TrackTiming,
}

/// 直接从 MIDI 字节流按轨提取全部事件（音符 + 速度 + 拍号 + 调号 + 控制事件 +
/// 歌词 + 标记 + SysEx），通过回调逐轨处理。
///
/// 与只提取部分事件的函数不同，该版本把所有 meta 文本/SysEx 以 **借用切片** 形式
/// 交给回调：调用方决定复制、丢弃或写入磁盘。对 100 GB 极端 MIDI 文件，避免在
/// 解析器内部无差别分配大块 SysEx 内存。
///
/// 该版本使用 `rayon` 并行解析各轨，并通过 `Mutex` 保护回调。调用方应通过
/// `track_idx` 自行写入对应槽位，不依赖回调顺序。
#[cfg(feature = "alloc")]
pub fn extract_all_events_per_track_streaming_from_bytes<F>(
    bytes: &[u8],
    track_handler: F,
) -> crate::Result<()>
where
    F: for<'a> FnMut(usize, TrackAllEvents<'a>) + Send,
{
    // parallel 构建：委托给无锁并行 API，再以 Mutex 把 `FnMut` 语义串行化
    // （与原实现等价：每轨解析在锁外，回调在锁内依次执行）。
    #[cfg(feature = "parallel")]
    {
        let handler = std::sync::Mutex::new(track_handler);
        extract_all_events_per_track_parallel_from_bytes(bytes, |track_idx, events| {
            let mut h = handler.lock().unwrap_or_else(|e| e.into_inner());
            h(track_idx, events);
        })
    }
    #[cfg(not(feature = "parallel"))]
    {
        let mut handler = track_handler;
        let data = crate::ump::preprocess_smf(bytes);
        let (_header, tracks_count, _division, raw) = fast_midi::parse_header(&data)?;
        let tracks = fast_midi::iter_tracks_from_data(raw, tracks_count);
        for (track_idx, events) in tracks.into_iter().enumerate() {
            let events = parse_fast_track_all_events(events, track_idx as u16);
            handler(track_idx, events);
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// P3：无锁逐轨并行提取 API（指导书 §P3）
//
// 背景：上面的 `*_streaming_from_bytes` 用 `FnMut + Send` + 全局 `Mutex` 调用
// 回调，下游逐轨工作（排序 / ChunkedList 构建 / 文本复制 / 全局归并）被强制串行。
// P2 实测（Pi.mid，24MB/30 轨/3.14M 音符）该串行段占墙钟 ~80%，
// Amdahl 拟合 wall≈S+P/N（S≈12.8ms 串行回调，P≈25.8ms 可并行解析，误差<1ms）。
//
// 下面三个 API 把回调约束改为 `Fn + Sync`，由 rayon 直接并发调用，库内部不持锁：
// - `*_parallel_from_bytes`：流式逐轨回调，适合「本轨处理完立即释放」的消费者；
// - `*_collect_from_bytes`：并行 map 后按轨序收集为 `Vec<T>`，即「每轨独立槽位 +
//   按轨序轻量归并」的零锁实现，适合结果整体保留到解析结束的消费者（如主链路）。
//
// 旧 API 保留且语义不变（parallel 构建内部委托到此处，serial 构建退化为顺序调用）。
// ─────────────────────────────────────────────────────────────────────────────

/// 逐轨并行提取音符/控制事件/拍号（无锁回调）。
///
/// 产出与 [`extract_notes_and_control_events_per_track_streaming_from_bytes`] 完全
/// 一致；回调为 `Fn + Sync`，由工作线程**并发**调用，库内部不持全局锁。
/// 回调顺序不保证（与轨序无关），但每轨恰好调用一次。
/// 关闭 `parallel` 特性时退化为顺序调用，契约不变。
#[cfg(feature = "alloc")]
pub fn extract_notes_and_control_events_per_track_parallel_from_bytes<F>(
    bytes: &[u8],
    track_handler: F,
) -> crate::Result<()>
where
    F: Fn(
            usize,
            Vec<PackedNote>,
            Vec<(u32, f32)>,
            Vec<PackedControlEvent>,
            Vec<PackedTimeSignatureEvent>,
        ) + Sync,
{
    let data = crate::ump::preprocess_smf(bytes);
    let (_header, tracks_count, _division, raw) = fast_midi::parse_header(&data)?;
    let tracks = fast_midi::iter_tracks_from_data(raw, tracks_count);

    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        // 以 `&F` 捕获：闭包只需 `F: Sync` 即满足 Send + Sync，无需 `F: Send`。
        let handler = &track_handler;
        tracks
            .into_par_iter()
            .enumerate()
            .for_each(|(track_idx, events)| {
                let (notes, tempo_changes, control_events, time_signatures) =
                    parse_fast_track_notes_control_and_time_signature_events(
                        events,
                        track_idx as u16,
                    );
                handler(
                    track_idx,
                    notes,
                    tempo_changes,
                    control_events,
                    time_signatures,
                );
            });
    }
    #[cfg(not(feature = "parallel"))]
    {
        for (track_idx, events) in tracks.into_iter().enumerate() {
            let (notes, tempo_changes, control_events, time_signatures) =
                parse_fast_track_notes_control_and_time_signature_events(events, track_idx as u16);
            track_handler(
                track_idx,
                notes,
                tempo_changes,
                control_events,
                time_signatures,
            );
        }
    }

    Ok(())
}

/// 逐轨并行提取全部事件（无锁回调）。
///
/// 事件内容与 [`extract_all_events_per_track_streaming_from_bytes`] 完全一致；
/// 回调为 `Fn + Sync`，由工作线程**并发**调用，库内部不持全局锁。
/// 调用方需要汇总多轨结果时应写每轨独立槽位（各槽位独立加锁，无全局竞争），
/// 或直接使用 [`extract_all_events_per_track_collect_from_bytes`]。
///
/// 回调顺序不保证（与轨序无关），但每轨恰好调用一次，`track_idx` 覆盖 `0..轨数`。
/// 关闭 `parallel` 特性时退化为顺序调用，契约不变。
#[cfg(feature = "alloc")]
pub fn extract_all_events_per_track_parallel_from_bytes<F>(
    bytes: &[u8],
    track_handler: F,
) -> crate::Result<()>
where
    F: for<'a> Fn(usize, TrackAllEvents<'a>) + Sync,
{
    let data = crate::ump::preprocess_smf(bytes);
    let (_header, tracks_count, _division, raw) = fast_midi::parse_header(&data)?;
    let tracks = fast_midi::iter_tracks_from_data(raw, tracks_count);

    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        // 以 `&F` 捕获：闭包只需 `F: Sync` 即满足 Send + Sync，无需 `F: Send`。
        let handler = &track_handler;
        tracks
            .into_par_iter()
            .enumerate()
            .for_each(|(track_idx, events)| {
                handler(
                    track_idx,
                    parse_fast_track_all_events(events, track_idx as u16),
                );
            });
    }
    #[cfg(not(feature = "parallel"))]
    {
        for (track_idx, events) in tracks.into_iter().enumerate() {
            track_handler(
                track_idx,
                parse_fast_track_all_events(events, track_idx as u16),
            );
        }
    }

    Ok(())
}

/// 逐轨并行提取全部事件，并按**轨序**收集回调产物（无锁）。
///
/// rayon 的有序收集保证 `results[i]` 一定来自 `track_idx == i`，
/// 即「每轨独立槽位 + 按轨序轻量归并」的零锁实现；调用方可在回调内完成
/// 排序 / 结构转换 / 文本复制等逐轨工作，互不阻塞。
///
/// 与流式回调 API 的取舍：本 API 会在解析结束前保留全部 `T`（内存 = 调用方
/// 本身的产物大小），适合结果本来就要整体保留的消费者；需要逐轨释放时用
/// [`extract_all_events_per_track_parallel_from_bytes`]。
///
/// 关闭 `parallel` 特性时按轨序顺序执行，返回值与并行构建完全一致。
#[cfg(feature = "alloc")]
pub fn extract_all_events_per_track_collect_from_bytes<F, T>(
    bytes: &[u8],
    track_handler: F,
) -> crate::Result<Vec<T>>
where
    F: for<'a> Fn(usize, TrackAllEvents<'a>) -> T + Sync,
    T: Send,
{
    let data = crate::ump::preprocess_smf(bytes);
    let (_header, tracks_count, _division, raw) = fast_midi::parse_header(&data)?;
    let tracks = fast_midi::iter_tracks_from_data(raw, tracks_count);

    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        let handler = &track_handler;
        // `IndexedParallelIterator::collect` 按索引保序：results[i] ← track_idx == i。
        let results: Vec<T> = tracks
            .into_par_iter()
            .enumerate()
            .map(|(track_idx, events)| {
                handler(
                    track_idx,
                    parse_fast_track_all_events(events, track_idx as u16),
                )
            })
            .collect();
        Ok(results)
    }
    #[cfg(not(feature = "parallel"))]
    {
        let results: Vec<T> = tracks
            .into_iter()
            .enumerate()
            .map(|(track_idx, events)| {
                track_handler(
                    track_idx,
                    parse_fast_track_all_events(events, track_idx as u16),
                )
            })
            .collect();
        Ok(results)
    }
}

/// Parse all events (notes, tempo, signatures, controls, lyrics, markers,
/// text-class metas, SysEx)
/// without allocating intermediate `TrackEvent` structs. Text/SysEx payloads are borrowed
/// from the raw track slice to keep the 100 GB path allocation-free in the parser.
#[cfg(feature = "alloc")]
fn parse_fast_track_all_events(mut events: FastTrackIter, track_idx: u16) -> TrackAllEvents<'_> {
    let mut note_acc = NoteAccumulator::new(track_idx);
    let mut ctrl_acc = ControlEventAccumulator::new(track_idx);
    let mut time_signatures = Vec::new();
    let mut key_signatures = Vec::new();
    let mut lyrics = Vec::new();
    let mut markers = Vec::new();
    let mut text_events = Vec::new();
    let mut sys_ex = Vec::new();
    let mut midi_port: Option<u8> = None;
    let mut track_name: Option<&[u8]> = None;
    // EOT 结构元数据（指导书 §2）：EOT 不进入事件流，只记录时间边界。
    // 全部记录都发生在 EOT 分支（每轨最多几次），热循环零额外开销。
    let mut eot_tick: Option<u32> = None;
    // 首个 EOT 解码后是否仍有未消费字节（即 EOT 后还有事件/尾部数据）。
    let mut eot_has_bytes_after = false;
    // 最后一个 EOT 是否恰好是轨道最后一条事件。
    let mut last_eot_ends_track = false;
    // 最后一个 EOT 之前那条事件的 tick（= EOT tick - EOT delta）。
    let mut last_eot_prev_tick: u32 = 0;

    while let Some((delta, event)) = events.next_event() {
        note_acc.advance(delta);
        ctrl_acc.advance(delta);
        let tick = note_acc.current_tick;

        match event {
            MidiEvent::NoteOn {
                channel,
                key,
                velocity,
            } => note_acc.note_on(key, velocity, channel),
            MidiEvent::NoteOff {
                channel,
                key,
                velocity,
            } => note_acc.note_off(key, channel, velocity),
            MidiEvent::ControlChange {
                channel,
                controller,
                value,
            } => ctrl_acc.control_change(channel, controller, value),
            MidiEvent::ProgramChange { channel, program } => {
                ctrl_acc.program_change(channel, program)
            }
            MidiEvent::PitchBend { channel, bend } => ctrl_acc.pitch_bend(channel, bend),
            MidiEvent::PolyAftertouch {
                channel,
                key,
                velocity,
            } => ctrl_acc.poly_aftertouch(channel, key, velocity),
            MidiEvent::ChannelAftertouch { channel, velocity } => {
                ctrl_acc.channel_aftertouch(channel, velocity)
            }
            MidiEvent::SysEx { data } => {
                sys_ex.push(PackedSysExEvent::new(tick, track_idx, data));
            }
            MidiEvent::Meta { event_type, data } => {
                // EOT 只在这里处理：元事件占比极低，不给普通事件增加分支。
                if event_type == 0x2F {
                    if eot_tick.is_none() {
                        eot_tick = Some(tick);
                        eot_has_bytes_after = events.remaining_len() > 0;
                    }
                    last_eot_ends_track = events.remaining_len() == 0;
                    last_eot_prev_tick = tick.saturating_sub(delta);
                }
                match event_type {
                    0x51 if data.len() == 3 => {
                        let microseconds =
                            ((data[0] as u32) << 16) | ((data[1] as u32) << 8) | (data[2] as u32);
                        note_acc.tempo_microseconds(microseconds);
                    }
                    0x58 if data.len() >= 2 => {
                        time_signatures.push(PackedTimeSignatureEvent::new(tick, data[0], data[1]));
                    }
                    0x59 if data.len() >= 2 => {
                        key_signatures.push(PackedKeySignatureEvent::new(
                            tick,
                            data[0] as i8,
                            data[1] != 0,
                        ));
                    }
                    0x05 => {
                        lyrics.push(PackedTextEvent::new(tick, track_idx, data));
                    }
                    0x06 => {
                        markers.push(PackedTextEvent::new(tick, track_idx, data));
                    }
                    // 文本类元事件：全量保留（此前静默丢弃）。
                    0x01 | 0x02 | 0x04 | 0x07 | 0x08 | 0x09 => {
                        text_events
                            .push(PackedTextMetaEvent::new(tick, track_idx, event_type, data));
                    }
                    // TrackName：单独带出（首个生效），供下游免去第二次全量扫描。
                    0x03 => {
                        if track_name.is_none() {
                            track_name = Some(data);
                        }
                    }
                    0x21 if data.len() >= 1 => {
                        if midi_port.is_none() {
                            midi_port = Some(data[0]);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let physical_end_tick = note_acc.current_tick;
    // EOT 结构元数据在循环后一次性推导（热循环零额外 store）：
    // - 无 EOT：全部事件都是内容 → 内容末尾 = 物理末尾；
    // - EOT 之后仍有数据：内容末尾 = 物理末尾（末尾为重复 EOT 时回退到其前一事件）；
    // - EOT 即最后事件：内容末尾 = EOT 前一事件的 tick。
    //
    // 已知边界（畸形输入，已由 `has_events_after_eot` 标记）：连续重复 EOT
    // 且都在轨道末尾时，`content_end_tick` 回退到首个 EOT 的 tick，而非更早的内容末尾。
    let has_events_after_eot = eot_has_bytes_after;
    let content_end_tick = match eot_tick {
        None => physical_end_tick,
        Some(_) if last_eot_ends_track => last_eot_prev_tick,
        Some(_) => physical_end_tick,
    };

    let physical_end_tick = note_acc.current_tick;
    let (notes, tempo_changes) = note_acc.finish();
    let control_events = ctrl_acc.finish();

    TrackAllEvents {
        notes,
        tempo_changes,
        time_signatures,
        key_signatures,
        control_events,
        lyrics,
        markers,
        text_events,
        sys_ex,
        midi_port,
        track_name,
        timing: TrackTiming {
            eot_tick,
            physical_end_tick,
            content_end_tick,
            has_events_after_eot,
        },
    }
}

/// Parse events directly to notes without allocating intermediate TrackEvent structs.
#[cfg(feature = "alloc")]
fn parse_fast_track_notes(
    mut events: FastTrackIter,
    track_idx: u16,
) -> (Vec<PackedNote>, Vec<(u32, f32)>) {
    let mut acc = NoteAccumulator::new(track_idx);

    while let Some((delta, event)) = events.next_event() {
        acc.advance(delta);

        match event {
            MidiEvent::NoteOn {
                channel,
                key,
                velocity,
            } => acc.note_on(key, velocity, channel),
            MidiEvent::NoteOff {
                channel,
                key,
                velocity,
            } => acc.note_off(key, channel, velocity),
            MidiEvent::Meta {
                event_type: 0x51,
                data,
            } => {
                if data.len() == 3 {
                    let microseconds =
                        ((data[0] as u32) << 16) | ((data[1] as u32) << 8) | (data[2] as u32);
                    acc.tempo_microseconds(microseconds);
                }
            }
            _ => {}
        }
    }

    acc.finish()
}

/// Parse events to notes AND control events (CC / PC / PB) without allocating intermediate TrackEvent structs.
#[cfg(feature = "alloc")]
fn parse_fast_track_notes_and_control_events(
    mut events: FastTrackIter,
    track_idx: u16,
) -> (Vec<PackedNote>, Vec<(u32, f32)>, Vec<PackedControlEvent>) {
    let mut note_acc = NoteAccumulator::new(track_idx);
    let mut ctrl_acc = ControlEventAccumulator::new(track_idx);

    while let Some((delta, event)) = events.next_event() {
        note_acc.advance(delta);
        ctrl_acc.advance(delta);

        match event {
            MidiEvent::NoteOn {
                channel,
                key,
                velocity,
            } => note_acc.note_on(key, velocity, channel),
            MidiEvent::NoteOff {
                channel,
                key,
                velocity,
            } => note_acc.note_off(key, channel, velocity),
            MidiEvent::ControlChange {
                channel,
                controller,
                value,
            } => ctrl_acc.control_change(channel, controller, value),
            MidiEvent::ProgramChange { channel, program } => {
                ctrl_acc.program_change(channel, program)
            }
            MidiEvent::PitchBend { channel, bend } => ctrl_acc.pitch_bend(channel, bend),
            MidiEvent::PolyAftertouch {
                channel,
                key,
                velocity,
            } => ctrl_acc.poly_aftertouch(channel, key, velocity),
            MidiEvent::ChannelAftertouch { channel, velocity } => {
                ctrl_acc.channel_aftertouch(channel, velocity)
            }
            MidiEvent::Meta {
                event_type: 0x51,
                data,
            } => {
                if data.len() == 3 {
                    let microseconds =
                        ((data[0] as u32) << 16) | ((data[1] as u32) << 8) | (data[2] as u32);
                    note_acc.tempo_microseconds(microseconds);
                }
            }
            _ => {}
        }
    }

    let (notes, tempo_changes) = note_acc.finish();
    let control_events = ctrl_acc.finish();
    (notes, tempo_changes, control_events)
}

/// Parse events to notes AND control events (CC / PC / PB) AND time signature events
/// without allocating intermediate TrackEvent structs.
#[cfg(feature = "alloc")]
fn parse_fast_track_notes_control_and_time_signature_events(
    mut events: FastTrackIter,
    track_idx: u16,
) -> (
    Vec<PackedNote>,
    Vec<(u32, f32)>,
    Vec<PackedControlEvent>,
    Vec<PackedTimeSignatureEvent>,
) {
    let mut note_acc = NoteAccumulator::new(track_idx);
    let mut ctrl_acc = ControlEventAccumulator::new(track_idx);
    let mut time_signatures = Vec::new();

    while let Some((delta, event)) = events.next_event() {
        note_acc.advance(delta);
        ctrl_acc.advance(delta);

        match event {
            MidiEvent::NoteOn {
                channel,
                key,
                velocity,
            } => note_acc.note_on(key, velocity, channel),
            MidiEvent::NoteOff {
                channel,
                key,
                velocity,
            } => note_acc.note_off(key, channel, velocity),
            MidiEvent::ControlChange {
                channel,
                controller,
                value,
            } => ctrl_acc.control_change(channel, controller, value),
            MidiEvent::ProgramChange { channel, program } => {
                ctrl_acc.program_change(channel, program)
            }
            MidiEvent::PitchBend { channel, bend } => ctrl_acc.pitch_bend(channel, bend),
            MidiEvent::PolyAftertouch {
                channel,
                key,
                velocity,
            } => ctrl_acc.poly_aftertouch(channel, key, velocity),
            MidiEvent::ChannelAftertouch { channel, velocity } => {
                ctrl_acc.channel_aftertouch(channel, velocity)
            }
            MidiEvent::Meta {
                event_type: 0x51,
                data,
            } => {
                if data.len() == 3 {
                    let microseconds =
                        ((data[0] as u32) << 16) | ((data[1] as u32) << 8) | (data[2] as u32);
                    note_acc.tempo_microseconds(microseconds);
                }
            }
            MidiEvent::Meta {
                event_type: 0x58,
                data,
            } => {
                if data.len() >= 2 {
                    time_signatures.push(PackedTimeSignatureEvent::new(
                        note_acc.current_tick,
                        data[0],
                        data[1],
                    ));
                }
            }
            _ => {}
        }
    }

    let (notes, tempo_changes) = note_acc.finish();
    let control_events = ctrl_acc.finish();
    (notes, tempo_changes, control_events, time_signatures)
}

// ============================================================================
// Streaming Note Loader
// ============================================================================

/// Heap item for multi-track merging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(all(feature = "std", feature = "memmap"))]
struct TrackHeapItem {
    tick: u32,
    cursor_idx: usize,
}

#[cfg(all(feature = "std", feature = "memmap"))]
impl Ord for TrackHeapItem {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        // Reverse for min-heap
        other
            .tick
            .cmp(&self.tick)
            .then_with(|| other.cursor_idx.cmp(&self.cursor_idx))
    }
}

#[cfg(all(feature = "std", feature = "memmap"))]
impl PartialOrd for TrackHeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(all(feature = "std", feature = "memmap"))]
#[derive(Debug)]
struct TrackCursor<'a> {
    iter: FastTrackIter<'a>,
    abs_tick: u32,
    next_event: Option<MidiEvent<'a>>,
}

#[cfg(all(feature = "std", feature = "memmap"))]
impl<'a> TrackCursor<'a> {
    fn new(iter: FastTrackIter<'a>) -> Self {
        let mut cursor = Self {
            iter,
            abs_tick: 0,
            next_event: None,
        };
        cursor.pull_next();
        cursor
    }

    fn pull_next(&mut self) {
        if let Some((delta, ev)) = self.iter.next_event() {
            self.abs_tick = self.abs_tick.saturating_add(delta);
            self.next_event = Some(ev);
        } else {
            self.next_event = None;
        }
    }
}

/// A streaming note loader with memory-mapped file support and bounded memory usage.
///
/// `StreamingNoteLoader` provides efficient, lazy loading of MIDI notes suitable for
/// large files and real-time visualization. It uses:
///
/// - **Memory mapping**: Zero-copy file access via `memmap2`
/// - **Lazy parsing**: Only parses notes in the current view window
/// - **Bounded memory**: Configurable limit on cached finished notes
/// - **Multi-track merge**: Efficient merging of notes from multiple tracks
///
/// # Memory Safety
///
/// The loader uses unsafe transmute to extend lifetimes of track cursors. This is safe
/// because the memory-mapped file (`Mmap`) is owned by the loader and kept alive for
/// its entire lifetime.
#[derive(Debug)]
#[cfg(all(feature = "std", feature = "memmap"))]
pub struct StreamingNoteLoader {
    _mmap: Mmap,
    header: Header,
    division: u16,

    // Event streaming
    cursors: Vec<TrackCursor<'static>>,
    heap: BinaryHeap<Reverse<TrackHeapItem>>,
    parsed_until: u32,

    // State tracking
    active_notes: Vec<[[Option<ActiveNote>; 256]; 16]>,

    // Finished notes cache with bounds
    finished_notes: VecDeque<PackedNote>,
    max_finished_notes: usize,

    // Reusable frame output
    frame_notes: Vec<PackedNote>,
    active_keys: [Option<u16>; 256],
}

#[cfg(all(feature = "std", feature = "memmap"))]
/// Memory statistics for debugging.
#[derive(Debug, Clone, Copy)]
pub struct LoaderMemoryStats {
    /// Number of finished notes currently cached.
    pub finished_notes_count: usize,
    /// Capacity of the finished notes buffer.
    pub finished_notes_capacity: usize,
    /// Capacity of the frame notes buffer.
    pub frame_notes_capacity: usize,
    /// Number of tracks.
    pub num_tracks: usize,
    /// Size of the heap.
    pub heap_size: usize,
}

#[cfg(all(feature = "std", feature = "memmap"))]
impl StreamingNoteLoader {
    /// Open a MIDI file for streaming note extraction.
    pub fn open(path: &Path) -> crate::Result<Self> {
        let file = File::open(path).map_err(|_| {
            crate::Error::new(&crate::ErrorKind::Invalid("failed to open midi file"))
        })?;

        // SAFETY: Read-only mapping is safe
        let mmap = unsafe {
            Mmap::map(&file).map_err(|_| {
                crate::Error::new(&crate::ErrorKind::Invalid("failed to memory map file"))
            })?
        };

        Self::from_mmap(mmap)
    }

    /// Create a streaming loader from an existing memory map.
    ///
    /// This is useful when you already have a memory-mapped file and want to
    /// avoid re-mapping it.
    pub fn from_mmap(mmap: Mmap) -> crate::Result<Self> {
        let (header, tracks_count, _division_raw, raw) = fast_midi::parse_header(&mmap)?;

        // Extract division from header
        let division = match header.timing {
            Timing::Metrical(ticks) => ticks.as_int(),
            Timing::Timecode(_, subframe) => subframe as u16,
        };

        // Convert tracks to cursors
        let tracks = fast_midi::iter_tracks_from_data(raw, tracks_count);
        let num_tracks = tracks.len();

        // SAFETY: We transmute to 'static because mmap lives as long as Self
        let cursors: Vec<TrackCursor<'static>> = unsafe {
            tracks
                .into_iter()
                .map(|events| {
                    std::mem::transmute::<TrackCursor<'_>, TrackCursor<'static>>(TrackCursor::new(
                        events,
                    ))
                })
                .collect()
        };

        // Initialize heap with first event from each cursor
        let mut heap = BinaryHeap::new();
        for (i, cursor) in cursors.iter().enumerate() {
            if cursor.next_event.is_some() {
                heap.push(Reverse(TrackHeapItem {
                    tick: cursor.abs_tick,
                    cursor_idx: i,
                }));
            }
        }

        Ok(Self {
            _mmap: mmap,
            header,
            division,
            cursors,
            heap,
            parsed_until: 0,
            active_notes: vec![[[None; 256]; 16]; num_tracks],
            finished_notes: VecDeque::new(),
            max_finished_notes: 32_768,
            frame_notes: Vec::with_capacity(2048),
            active_keys: [None; 256],
        })
    }

    /// Set the maximum number of finished notes to keep in memory.
    #[inline]
    pub fn set_max_finished_notes(&mut self, max: usize) {
        self.max_finished_notes = max;
    }

    /// Get the number of finished notes currently cached.
    #[inline]
    pub fn finished_notes_count(&self) -> usize {
        self.finished_notes.len()
    }

    /// Get memory statistics for debugging.
    pub fn memory_stats(&self) -> LoaderMemoryStats {
        LoaderMemoryStats {
            finished_notes_count: self.finished_notes.len(),
            finished_notes_capacity: self.finished_notes.capacity(),
            frame_notes_capacity: self.frame_notes.capacity(),
            num_tracks: self.cursors.len(),
            heap_size: self.heap.len(),
        }
    }

    /// Get the MIDI file header.
    #[inline]
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Get the division (ticks per quarter note or timecode).
    #[inline]
    pub fn division(&self) -> u16 {
        self.division
    }

    /// Parse all remaining events in the file.
    ///
    /// Returns the number of finished notes currently cached.
    pub fn parse_all(&mut self) -> usize {
        self.advance_to(u32::MAX);
        self.finished_notes.len()
    }

    /// Prepare notes for the current frame/view window.
    pub fn prepare_frame(
        &mut self,
        current_tick: f32,
        ticks_per_screen: f32,
    ) -> (&[PackedNote], &[Option<u16>; 256]) {
        let screen_end = current_tick + ticks_per_screen;
        let parse_target = (screen_end + ticks_per_screen).max(0.0) as u32;

        // Advance parsing
        self.advance_to(parse_target);

        // Prune old notes
        let lookback = ticks_per_screen * 2.0;
        let min_end = (current_tick - lookback).max(0.0);
        self.prune_old_notes(min_end as u32);

        // Collect visible notes with bounded capacity
        self.frame_notes.clear();
        const MAX_FRAME_NOTES: usize = 10000;

        // Add finished notes in range (bounded)
        for note in &self.finished_notes {
            if self.frame_notes.len() >= MAX_FRAME_NOTES {
                break;
            }
            if (note.start_tick as f32) > screen_end + ticks_per_screen {
                continue;
            }
            if (note.end_tick as f32) < min_end {
                continue;
            }
            self.frame_notes.push(*note);
        }

        // Add active notes (bounded)
        let provisional_end = parse_target as f32;
        for (track_idx, track_channels) in self.active_notes.iter().enumerate() {
            if self.frame_notes.len() >= MAX_FRAME_NOTES {
                break;
            }
            for (ch, track_notes) in track_channels.iter().enumerate() {
                if self.frame_notes.len() >= MAX_FRAME_NOTES {
                    break;
                }
                for (key, active) in track_notes.iter().enumerate() {
                    if self.frame_notes.len() >= MAX_FRAME_NOTES {
                        break;
                    }
                    if let Some(note) = active {
                        let start = note.start_tick as f32;
                        if start > screen_end + ticks_per_screen {
                            continue;
                        }
                        self.frame_notes.push(PackedNote::new(
                            note.start_tick,
                            provisional_end as u32,
                            key as u8,
                            note.velocity,
                            ch as u8,
                            track_idx as u16,
                        ));
                    }
                }
            }
        }

        // Compute active keys
        self.active_keys = [None; 256];
        for note in &self.frame_notes {
            let start = note.start_tick as f32;
            let end = note.end_tick as f32;
            if start <= current_tick && end >= current_tick {
                let k = note.key as usize;
                if k < 256 {
                    self.active_keys[k] = Some(note.track);
                }
            }
        }

        (&self.frame_notes, &self.active_keys)
    }

    fn advance_to(&mut self, target_tick: u32) {
        while let Some(Reverse(item)) = self.heap.peek().copied() {
            if item.tick > target_tick {
                break;
            }

            let item = match self.heap.pop() {
                Some(Reverse(it)) => it,
                None => break,
            };
            let cursor_idx = item.cursor_idx;

            if let Some(ev) = self.cursors[cursor_idx].next_event.take() {
                let tick = self.cursors[cursor_idx].abs_tick;
                self.process_event(tick, cursor_idx, ev);

                self.cursors[cursor_idx].pull_next();
                if self.cursors[cursor_idx].next_event.is_some() {
                    self.heap.push(Reverse(TrackHeapItem {
                        tick: self.cursors[cursor_idx].abs_tick,
                        cursor_idx,
                    }));
                }

                self.parsed_until = self.parsed_until.max(tick);
            }
        }
    }

    fn process_event(&mut self, tick: u32, track_idx: usize, event: MidiEvent<'_>) {
        match event {
            MidiEvent::NoteOn {
                channel,
                key,
                velocity,
            } => {
                let ch = (channel.min(15)) as usize;
                let key_idx = key as usize;
                if key_idx < 256 && track_idx < self.active_notes.len() {
                    if let Some(active) = self.active_notes[track_idx][ch][key_idx].take() {
                        self.finish_note(track_idx, ch, key_idx, active, tick, channel);
                    }
                    if velocity > 0 {
                        self.active_notes[track_idx][ch][key_idx] = Some(ActiveNote {
                            start_tick: tick,
                            velocity,
                        });
                    }
                }
            }
            MidiEvent::NoteOff {
                channel,
                key,
                velocity: _,
            } => {
                let ch = (channel.min(15)) as usize;
                let key_idx = key as usize;
                if key_idx < 256 && track_idx < self.active_notes.len() {
                    if let Some(active) = self.active_notes[track_idx][ch][key_idx].take() {
                        self.finish_note(track_idx, ch, key_idx, active, tick, channel);
                    }
                }
            }
            _ => {}
        }
    }

    fn finish_note(
        &mut self,
        track_idx: usize,
        ch: usize,
        key: usize,
        active: ActiveNote,
        end_tick: u32,
        channel: u8,
    ) {
        self.finished_notes.push_back(PackedNote::new(
            active.start_tick,
            end_tick,
            key as u8,
            active.velocity,
            channel,
            track_idx as u16,
        ));

        // Aggressive cleanup when approaching memory limit
        if self.finished_notes.len() > self.max_finished_notes {
            // Remove 25% of oldest notes at once to avoid frequent pruning
            let remove_count = self.max_finished_notes / 4;
            for _ in 0..remove_count {
                self.finished_notes.pop_front();
            }
        }
    }

    fn prune_old_notes(&mut self, min_end_tick: u32) {
        while let Some(front) = self.finished_notes.front() {
            if front.end_tick < min_end_tick {
                self.finished_notes.pop_front();
            } else {
                break;
            }
        }
    }
}

// ============================================================================
// Sequential File Scanner (bounded memory)
// ============================================================================

/// Result of scanning a MIDI file with [`scan_midi_file`].
///
/// Contains summary statistics without storing individual notes.
#[cfg(feature = "std")]
#[derive(Debug, Clone)]
pub struct MidiScanResult {
    /// Total number of notes in the file.
    pub note_count: u64,
    /// Number of tracks found.
    pub track_count: u16,
    /// Tempo changes as `(tick, bpm)` pairs, sorted by tick.
    pub tempo_changes: Vec<(u32, f32)>,
    /// Maximum tick across all tracks.
    pub max_tick: u32,
    /// Division (ticks per quarter note) from the MIDI header.
    pub division: u16,
}

/// Scan a MIDI file sequentially with bounded memory usage.
///
/// This function reads the file track-by-track using buffered I/O,
/// never loading the entire file into memory. It counts notes,
/// collects tempo changes, and determines timing information.
///
/// # Memory Usage
///
/// Peak memory is `O(largest_track_size)` plus a 256 KB read buffer.
/// For a typical MIDI file with hundreds of tracks, this is usually
/// well under 10 MB even for very large files (354+ MB).
///
/// # Performance
///
/// Sequential disk I/O with a 256 KB buffer provides near-maximum
/// throughput on SSDs. Parsing uses the lightweight `fast_midi::TrackIter`
/// which avoids allocating intermediate event structures.
///
/// # Example
///
/// ```rust,no_run
/// use midly::loader::scan_midi_file;
/// use std::path::Path;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let result = scan_midi_file(Path::new("large_song.mid"))?;
/// println!("Notes: {}, Tracks: {}", result.note_count, result.track_count);
/// # Ok(())
/// # }
/// ```
/// Single buffer size for sequential read; tracks are parsed in parallel from slices of this buffer.
#[cfg(all(feature = "std", feature = "parallel"))]
const PARALLEL_BATCH_BYTES: usize = 20 * 1024 * 1024;

#[cfg(feature = "std")]
pub fn scan_midi_file(path: &std::path::Path) -> crate::Result<MidiScanResult> {
    use std::io::{BufReader, Read, Seek, SeekFrom};

    let file = std::fs::File::open(path)
        .map_err(|_| crate::Error::new(&crate::ErrorKind::Invalid("failed to open midi file")))?;
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(4096);
    let mut reader = BufReader::with_capacity(4 * 1024 * 1024, file);

    let mut magic = [0u8; 4];
    reader
        .read_exact(&mut magic)
        .map_err(|_| crate::Error::new(&crate::ErrorKind::Invalid("file too short")))?;

    if &magic == b"RIFF" {
        reader
            .seek(SeekFrom::Start(0))
            .map_err(|_| crate::Error::new(&crate::ErrorKind::Invalid("seek failed")))?;
        let scan_size = file_len.min(4096) as usize;
        let mut scan_buf = vec![0u8; scan_size];
        reader.read_exact(&mut scan_buf).map_err(|_| {
            crate::Error::new(&crate::ErrorKind::Invalid("failed to scan RIFF header"))
        })?;

        let mthd_pos = scan_buf
            .windows(4)
            .position(|w| w == b"MThd")
            .ok_or_else(|| crate::Error::new(&crate::ErrorKind::Invalid("no MThd in RIFF")))?;

        reader
            .seek(SeekFrom::Start(mthd_pos as u64))
            .map_err(|_| crate::Error::new(&crate::ErrorKind::Invalid("seek to MThd failed")))?;
    } else if &magic == b"MThd" {
        reader
            .seek(SeekFrom::Start(0))
            .map_err(|_| crate::Error::new(&crate::ErrorKind::Invalid("seek failed")))?;
    } else {
        return Err(crate::Error::new(&crate::ErrorKind::Invalid(
            "not a MIDI file",
        )));
    }

    let mut header_buf = [0u8; 14];
    reader
        .read_exact(&mut header_buf)
        .map_err(|_| crate::Error::new(&crate::ErrorKind::Invalid("truncated MIDI header")))?;

    if &header_buf[0..4] != b"MThd" {
        return Err(crate::Error::new(&crate::ErrorKind::Invalid(
            "invalid MIDI header",
        )));
    }

    let header_len =
        u32::from_be_bytes([header_buf[4], header_buf[5], header_buf[6], header_buf[7]]);
    if header_len < 6 {
        return Err(crate::Error::new(&crate::ErrorKind::Invalid(
            "MIDI header too short",
        )));
    }

    if header_len > 6 {
        let skip = (header_len - 6) as i64;
        reader.seek(SeekFrom::Current(skip)).map_err(|_| {
            crate::Error::new(&crate::ErrorKind::Invalid("failed to skip header padding"))
        })?;
    }

    let tracks_count = u16::from_be_bytes([header_buf[10], header_buf[11]]);
    let division = u16::from_be_bytes([header_buf[12], header_buf[13]]);

    let mut result = MidiScanResult {
        note_count: 0,
        track_count: 0,
        tempo_changes: Vec::new(),
        max_tick: 0,
        division,
    };

    #[cfg(feature = "parallel")]
    {
        // Single buffer: sequential read, parallel parse per batch (no copy, no extra thread).
        let mut buf = vec![0u8; PARALLEL_BATCH_BYTES];
        let mut buf_used = 0usize;
        let mut batch: Vec<(usize, usize)> = Vec::new();

        let mut chunk_header = [0u8; 8];
        loop {
            if result.track_count >= tracks_count {
                break;
            }
            if reader.read_exact(&mut chunk_header).is_err() {
                break;
            }
            let chunk_len = u32::from_be_bytes([
                chunk_header[4],
                chunk_header[5],
                chunk_header[6],
                chunk_header[7],
            ]);
            if &chunk_header[0..4] != b"MTrk" {
                if &chunk_header[0..4] == b"UMP " {
                    let ump_len = chunk_len as usize;
                    let mut ump_buf = vec![0u8; ump_len];
                    if reader.read_exact(&mut ump_buf).is_err() {
                        break;
                    }
                    let mtrk_data = crate::ump::convert_ump_chunk_to_mtrk(&ump_buf);
                    result.track_count += 1;
                    let (notes, max_tick, tempos) = fast_midi::scan_track_notes_only(&mtrk_data);
                    result.note_count += notes;
                    result.max_tick = result.max_tick.max(max_tick);
                    result.tempo_changes.extend(tempos);
                } else {
                    let _ = reader.seek(SeekFrom::Current(chunk_len as i64));
                }
                continue;
            }
            let track_len = chunk_len as usize;
            if track_len > PARALLEL_BATCH_BYTES {
                if !batch.is_empty() {
                    let results: Vec<(u64, u32, Vec<(u32, f32)>)> = batch
                        .par_iter()
                        .map(|&(start, len)| {
                            fast_midi::scan_track_notes_only(&buf[start..start + len])
                        })
                        .collect();
                    for (notes, max_tick, tempos) in results {
                        result.note_count += notes;
                        result.max_tick = result.max_tick.max(max_tick);
                        result.tempo_changes.extend(tempos);
                    }
                    batch.clear();
                    buf_used = 0;
                }
                let mut big = vec![0u8; track_len];
                if reader.read_exact(&mut big).is_err() {
                    break;
                }
                result.track_count += 1;
                let (notes, max_tick, tempos) = fast_midi::scan_track_notes_only(&big);
                result.note_count += notes;
                result.max_tick = result.max_tick.max(max_tick);
                result.tempo_changes.extend(tempos);
                continue;
            }
            if buf_used + track_len > buf.len() {
                let results: Vec<(u64, u32, Vec<(u32, f32)>)> = batch
                    .par_iter()
                    .map(|&(start, len)| fast_midi::scan_track_notes_only(&buf[start..start + len]))
                    .collect();
                for (notes, max_tick, tempos) in results {
                    result.note_count += notes;
                    result.max_tick = result.max_tick.max(max_tick);
                    result.tempo_changes.extend(tempos);
                }
                batch.clear();
                buf_used = 0;
            }
            if reader
                .read_exact(&mut buf[buf_used..buf_used + track_len])
                .is_err()
            {
                break;
            }
            batch.push((buf_used, track_len));
            buf_used += track_len;
            result.track_count += 1;
        }

        if !batch.is_empty() {
            let results: Vec<(u64, u32, Vec<(u32, f32)>)> = batch
                .par_iter()
                .map(|&(start, len)| fast_midi::scan_track_notes_only(&buf[start..start + len]))
                .collect();
            for (notes, max_tick, tempos) in results {
                result.note_count += notes;
                result.max_tick = result.max_tick.max(max_tick);
                result.tempo_changes.extend(tempos);
            }
        }
    }

    #[cfg(not(feature = "parallel"))]
    {
        let mut track_buf: Vec<u8> = Vec::new();
        loop {
            if result.track_count >= tracks_count {
                break;
            }
            let mut chunk_header = [0u8; 8];
            if reader.read_exact(&mut chunk_header).is_err() {
                break;
            }
            let chunk_len = u32::from_be_bytes([
                chunk_header[4],
                chunk_header[5],
                chunk_header[6],
                chunk_header[7],
            ]);
            if &chunk_header[0..4] != b"MTrk" {
                if &chunk_header[0..4] == b"UMP " {
                    let ump_len = chunk_len as usize;
                    track_buf.clear();
                    track_buf.resize(ump_len, 0);
                    if reader.read_exact(&mut track_buf).is_err() {
                        break;
                    }
                    let mtrk_data = crate::ump::convert_ump_chunk_to_mtrk(&track_buf);
                    result.track_count += 1;
                    let (notes, max_tick, tempos) = fast_midi::scan_track_notes_only(&mtrk_data);
                    result.note_count += notes;
                    result.max_tick = result.max_tick.max(max_tick);
                    result.tempo_changes.extend(tempos);
                } else {
                    let _ = reader.seek(SeekFrom::Current(chunk_len as i64));
                }
                continue;
            }
            let track_len = chunk_len as usize;
            track_buf.clear();
            track_buf.resize(track_len, 0);
            if reader.read_exact(&mut track_buf).is_err() {
                break;
            }
            let (track_notes, track_max_tick, track_tempos) =
                fast_midi::scan_track_notes_only(&track_buf);
            result.note_count += track_notes;
            result.max_tick = result.max_tick.max(track_max_tick);
            result.tempo_changes.extend(track_tempos);
            result.track_count += 1;
        }
    }

    // MIDI default is 120 BPM; only insert default if no tempo at tick 0 exists
    if !result.tempo_changes.iter().any(|(t, _)| *t == 0) {
        result.tempo_changes.push((0, 120.0f32));
    }

    result.tempo_changes.sort_unstable_by_key(|&(t, _)| t);
    result.tempo_changes.dedup_by_key(|(t, _)| *t);

    Ok(result)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::num::{u4, u7, u15, u28};
    use crate::{Format, Timing};

    #[test]
    fn test_packed_note_size() {
        // Packed size is 14 bytes (13 + 1 release_velocity; was 12 before adding channel)
        assert_eq!(core::mem::size_of::<PackedNote>(), 14);
    }

    #[test]
    fn test_packed_note_new() {
        let note = PackedNote::new(100, 200, 60, 100, 5, 0);
        // Copy fields to avoid unaligned reference issues with packed struct
        let start_tick = note.start_tick;
        let end_tick = note.end_tick;
        let key = note.key;
        let velocity = note.velocity;
        let release_velocity = note.release_velocity;
        let channel = note.channel;
        let track = note.track;
        assert_eq!(start_tick, 100);
        assert_eq!(end_tick, 200);
        assert_eq!(key, 60);
        assert_eq!(velocity, 100);
        // new() 保持兼容：释放力度默认 0
        assert_eq!(release_velocity, 0);
        assert_eq!(channel, 5);
        assert_eq!(track, 0);
    }

    #[test]
    fn test_packed_note_release_velocity() {
        let note = PackedNote::new_with_release(100, 200, 60, 100, 64, 5, 0);
        let velocity = note.velocity;
        let release_velocity = note.release_velocity;
        assert_eq!(velocity, 100);
        assert_eq!(release_velocity, 64);

        // vel 掩码：>127 的输入截断到 7 位
        let masked = PackedNote::new_with_release(0, 10, 60, 200, 200, 0, 0);
        assert_eq!(masked.velocity, 200 & 0x7F);
        assert_eq!(masked.release_velocity, 200 & 0x7F);

        // builder 形式
        let built = PackedNote::new(0, 10, 60, 90, 0, 0).with_release_velocity(77);
        assert_eq!(built.release_velocity, 77);
        // 原 velocity 不受影响
        assert_eq!(built.velocity, 90);
    }

    #[test]
    fn test_packed_note_duration() {
        let note = PackedNote::new(100, 250, 60, 100, 0, 0);
        assert_eq!(note.duration(), 150);
    }

    #[test]
    fn test_packed_note_overlaps() {
        let note = PackedNote::new(100, 200, 60, 100, 0, 0);

        // Overlapping ranges
        assert!(note.overlaps(50.0, 150.0));
        assert!(note.overlaps(150.0, 250.0));
        assert!(note.overlaps(120.0, 180.0));
        assert!(note.overlaps(100.0, 200.0));

        // Non-overlapping ranges
        assert!(!note.overlaps(0.0, 50.0));
        assert!(!note.overlaps(250.0, 300.0));
    }

    #[test]
    fn test_note_index_build() {
        let notes = vec![
            PackedNote::new(0, 100, 60, 100, 0, 0),
            PackedNote::new(50, 150, 64, 100, 0, 0),
            PackedNote::new(200, 300, 67, 100, 0, 1),
        ];

        let index = NoteIndex::build(notes, 100);

        assert_eq!(index.len(), 3);
        assert_eq!(index.max_end_tick(), 300);
        assert!(!index.is_empty());
    }

    #[test]
    fn test_note_index_query_range() {
        let notes = vec![
            PackedNote::new(0, 100, 60, 100, 0, 0),
            PackedNote::new(50, 150, 64, 100, 0, 0),
            PackedNote::new(200, 300, 67, 100, 0, 1),
            PackedNote::new(400, 500, 72, 100, 0, 0),
        ];

        let index = NoteIndex::build(notes, 100);

        // Query range 25-175 should find first 2 notes
        let results: Vec<_> = index.query_range(25.0, 175.0).collect();
        assert_eq!(results.len(), 2);
        // Access key field through local copy
        let key0 = results[0].key;
        let key1 = results[1].key;
        assert_eq!(key0, 60);
        assert_eq!(key1, 64);

        // Query range 150-250 should find note 2 (spans 50-150) and note 3 (200-300)
        let results: Vec<_> = index.query_range(150.0, 250.0).collect();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_note_index_empty() {
        let index = NoteIndex::build(vec![], 100);
        assert!(index.is_empty());
        assert_eq!(index.len(), 0);
        assert_eq!(index.max_end_tick(), 0);
    }

    #[test]
    #[cfg(feature = "alloc")]
    fn test_extract_notes_smf() {
        // Create a simple SMF with one track containing note events
        let smf = crate::Smf {
            header: crate::smf::Header {
                format: Format::Parallel,
                timing: Timing::Metrical(u15::new(480)),
            },
            tracks: vec![vec![
                // Note on at tick 0
                TrackEvent {
                    delta: u28::new(0),
                    kind: TrackEventKind::Midi {
                        channel: u4::new(0),
                        message: MidiMessage::NoteOn {
                            key: 60u8,
                            vel: u7::new(100),
                        },
                    },
                },
                // Note off at tick 480
                TrackEvent {
                    delta: u28::new(480),
                    kind: TrackEventKind::Midi {
                        channel: u4::new(0),
                        message: MidiMessage::NoteOff {
                            key: 60u8,
                            vel: u7::new(0),
                        },
                    },
                },
            ]],
        };

        let (notes, tempo_changes) = extract_notes(&smf);

        assert_eq!(notes.len(), 1);
        // Copy fields to avoid unaligned reference
        let start_tick = notes[0].start_tick;
        let end_tick = notes[0].end_tick;
        let key = notes[0].key;
        let velocity = notes[0].velocity;
        let channel = notes[0].channel;
        let track = notes[0].track;
        assert_eq!(start_tick, 0);
        assert_eq!(end_tick, 480);
        assert_eq!(key, 60);
        assert_eq!(velocity, 100);
        assert_eq!(channel, 0);
        assert_eq!(track, 0);

        // Should have default tempo
        assert_eq!(tempo_changes.len(), 1);
        assert_eq!(tempo_changes[0], (0, 120.0));
    }

    #[test]
    fn test_256_key_roundtrip() {
        // Build a track with all 256 keys (0-255), each with NoteOn + NoteOff
        let mut events = Vec::with_capacity(512);
        for key in 0..=255u8 {
            events.push(TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Midi {
                    channel: u4::new(0),
                    message: MidiMessage::NoteOn {
                        key,
                        vel: u7::new(100),
                    },
                },
            });
            events.push(TrackEvent {
                delta: u28::new(480),
                kind: TrackEventKind::Midi {
                    channel: u4::new(0),
                    message: MidiMessage::NoteOff {
                        key,
                        vel: u7::new(0),
                    },
                },
            });
        }

        let smf = crate::Smf {
            header: crate::smf::Header {
                format: Format::Parallel,
                timing: Timing::Metrical(u15::new(480)),
            },
            tracks: vec![events],
        };

        // Write to bytes
        let mut bytes = Vec::new();
        smf.write(&mut bytes).expect("write failed");

        // Parse back via Smf::parse
        let parsed = crate::Smf::parse(&bytes).expect("parse failed");

        // Collect all NoteOn keys
        let mut parsed_keys: Vec<u8> = Vec::new();
        for track in &parsed.tracks {
            for event in track {
                if let TrackEventKind::Midi {
                    message: MidiMessage::NoteOn { key, .. },
                    ..
                } = &event.kind
                {
                    parsed_keys.push(*key);
                }
            }
        }

        // Verify all 256 keys are present and correct
        assert_eq!(parsed_keys.len(), 256, "should have 256 NoteOn events");
        let mut sorted = parsed_keys.clone();
        sorted.sort();
        for (i, &key) in sorted.iter().enumerate() {
            assert_eq!(key, i as u8, "key mismatch at position {}", i);
        }

        // Verify via extract_notes_from_bytes (fast path)
        let (notes, _tempo) = crate::loader::extract_notes_from_bytes(&bytes)
            .expect("extract_notes_from_bytes failed");
        assert_eq!(notes.len(), 256, "fast path should extract 256 notes");
        let mut fast_keys: Vec<u8> = notes.iter().map(|n| n.key).collect();
        fast_keys.sort();
        for (i, &key) in fast_keys.iter().enumerate() {
            assert_eq!(key, i as u8, "fast path key mismatch at position {}", i);
        }

        // Verify keys > 127 are preserved (not masked to 7-bit)
        let has_extended = parsed_keys.iter().any(|&k| k > 127);
        assert!(has_extended, "should contain keys > 127 in 256-key mode");
        let has_extended_fast = fast_keys.iter().any(|&k| k > 127);
        assert!(has_extended_fast, "fast path should contain keys > 127");
    }

    #[test]
    #[cfg(feature = "alloc")]
    fn test_note_off_release_velocity_roundtrip() {
        // NoteOff(0x80) 携带非零释放力度 64：Smf 路径与 fast-bytes 路径都必须透传。
        let track = vec![
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Midi {
                    channel: u4::new(0),
                    message: MidiMessage::NoteOn {
                        key: 60u8,
                        vel: u7::new(100),
                    },
                },
            },
            TrackEvent {
                delta: u28::new(480),
                kind: TrackEventKind::Midi {
                    channel: u4::new(0),
                    message: MidiMessage::NoteOff {
                        key: 60u8,
                        vel: u7::new(64),
                    },
                },
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
            },
        ];
        let smf = crate::Smf {
            header: crate::smf::Header {
                format: Format::Parallel,
                timing: Timing::Metrical(u15::new(480)),
            },
            tracks: vec![track],
        };

        // Smf 路径
        let (notes, _) = extract_notes(&smf);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].velocity, 100);
        assert_eq!(notes[0].release_velocity, 64);

        // fast-bytes 路径（0x80 NoteOff）
        let mut bytes = Vec::new();
        smf.write(&mut bytes).expect("write failed");
        let mut fast_releases = Vec::new();
        extract_all_events_per_track_streaming_from_bytes(&bytes, |_idx, events| {
            fast_releases.extend(events.notes.iter().map(|n| n.release_velocity));
        })
        .expect("extract all events failed");
        assert_eq!(fast_releases, vec![64]);

        // NoteOn(vel=0) 等价 NoteOff：释放力度为 0，不断言失败只断言透传语义。
        let track_zero = vec![
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Midi {
                    channel: u4::new(1),
                    message: MidiMessage::NoteOn {
                        key: 62u8,
                        vel: u7::new(90),
                    },
                },
            },
            TrackEvent {
                delta: u28::new(240),
                kind: TrackEventKind::Midi {
                    channel: u4::new(1),
                    message: MidiMessage::NoteOn {
                        key: 62u8,
                        vel: u7::new(0),
                    },
                },
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
            },
        ];
        let smf_zero = crate::Smf {
            header: crate::smf::Header {
                format: Format::Parallel,
                timing: Timing::Metrical(u15::new(480)),
            },
            tracks: vec![track_zero],
        };
        let (notes_zero, _) = extract_notes(&smf_zero);
        assert_eq!(notes_zero.len(), 1);
        assert_eq!(notes_zero[0].velocity, 90);
        assert_eq!(notes_zero[0].release_velocity, 0);
    }

    #[test]
    #[cfg(feature = "alloc")]
    fn test_extract_all_events_per_track_streaming() {
        let track = vec![
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::Tempo(crate::primitive::u24::new(500_000))),
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::TimeSignature(4, 2, 24, 8)),
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::KeySignature(2, false)),
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Midi {
                    channel: u4::new(0),
                    message: MidiMessage::NoteOn {
                        key: 60,
                        vel: u7::new(100),
                    },
                },
            },
            TrackEvent {
                delta: u28::new(480),
                kind: TrackEventKind::Meta(MetaMessage::Lyric(b"la")),
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::Marker(b"Chorus")),
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::SysEx(b"\x01\x02\x03\xF7"),
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::Text(b"hello")),
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::Copyright(b"(c) test")),
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::InstrumentName(b"Piano")),
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::CuePoint(b"cue1")),
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Midi {
                    channel: u4::new(0),
                    message: MidiMessage::NoteOff {
                        key: 60,
                        vel: u7::new(0),
                    },
                },
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
            },
        ];
        let smf = crate::Smf {
            header: crate::smf::Header {
                format: Format::Parallel,
                timing: Timing::Metrical(u15::new(480)),
            },
            tracks: vec![track],
        };

        let mut bytes = Vec::new();
        smf.write(&mut bytes).expect("write failed");

        // 回调内借用数据不可逃逸，因此把需要断言的数据复制到 owned 容器。
        let mut note_keys = Vec::new();
        let mut tempos = Vec::new();
        let mut time_sigs = Vec::new();
        let mut key_sigs = Vec::new();
        let mut lyrics = Vec::new();
        let mut markers = Vec::new();
        let mut text_events = Vec::new();
        let mut sys_exes = Vec::new();
        extract_all_events_per_track_streaming_from_bytes(&bytes, |_track_idx, events| {
            note_keys.extend(events.notes.iter().map(|n| n.key));
            tempos.extend(events.tempo_changes.iter().copied());
            time_sigs.extend(
                events
                    .time_signatures
                    .iter()
                    .map(|e| (e.tick, e.numerator, e.denominator)),
            );
            key_sigs.extend(
                events
                    .key_signatures
                    .iter()
                    .map(|e| (e.tick, e.sharps, e.is_minor)),
            );
            lyrics.extend(
                events
                    .lyrics
                    .iter()
                    .map(|e| (e.tick, e.track, e.text.to_vec())),
            );
            markers.extend(
                events
                    .markers
                    .iter()
                    .map(|e| (e.tick, e.track, e.text.to_vec())),
            );
            text_events.extend(
                events
                    .text_events
                    .iter()
                    .map(|e| (e.tick, e.track, e.meta_type, e.text.to_vec())),
            );
            sys_exes.extend(
                events
                    .sys_ex
                    .iter()
                    .map(|e| (e.tick, e.track, e.data.to_vec())),
            );
        })
        .expect("extract all events failed");

        assert_eq!(note_keys, vec![60]);
        assert_eq!(tempos.len(), 1);
        assert!((tempos[0].1 - 120.0).abs() < 0.001);
        assert_eq!(time_sigs, vec![(0, 4, 2)]);
        assert_eq!(key_sigs, vec![(0, 2, false)]);
        assert_eq!(lyrics, vec![(480, 0, b"la".to_vec())]);
        assert_eq!(markers, vec![(480, 0, b"Chorus".to_vec())]);
        // 文本类元事件全量保留：(tick, track, meta_type, bytes)
        assert_eq!(
            text_events,
            vec![
                (480, 0, 0x01, b"hello".to_vec()),
                (480, 0, 0x02, b"(c) test".to_vec()),
                (480, 0, 0x04, b"Piano".to_vec()),
                (480, 0, 0x07, b"cue1".to_vec()),
            ]
        );
        assert_eq!(sys_exes, vec![(480, 0, b"\x01\x02\x03\xF7".to_vec())]);
    }

    #[test]
    fn test_packed_control_event_size() {
        // 4 + 2 + 1 + 1 + 2 = 10 字节；新增 kind 3/4 不改变布局
        assert_eq!(core::mem::size_of::<PackedControlEvent>(), 10);
    }

    #[test]
    fn test_packed_control_aftertouch_roundtrip() {
        let ch_at = PackedControlEvent::channel_aftertouch(10, 2, 5, 64);
        assert_eq!(ch_at.kind, 3);
        assert_eq!(ch_at.as_channel_aftertouch(), 64);

        let poly = PackedControlEvent::poly_aftertouch(20, 1, 3, 60, 80);
        assert_eq!(poly.kind, 4);
        assert_eq!(poly.as_poly_aftertouch(), (60, 80));

        // 旧 kinds 不受影响
        let cc = PackedControlEvent::control_change(0, 0, 0, 7, 100);
        assert_eq!(cc.kind, 0);
        assert_eq!(cc.as_control_change(), (7, 100));
    }

    #[test]
    #[cfg(feature = "alloc")]
    fn test_fast_aftertouch_parse_and_no_desync() {
        // MThd(format 0, 1 track, division 128) + MTrk(19 bytes):
        // ChAT(0xD0,1字节数据) → PolyAT(0xA0) → NoteOn → NoteOff → EOT。
        // 回归：0xD0 此前误跳 2 字节导致后续事件错位。
        let bytes: Vec<u8> = vec![
            0x4D, 0x54, 0x68, 0x64, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x01, 0x00, 0x80,
            0x4D, 0x54, 0x72, 0x6B, 0x00, 0x00, 0x00, 0x13, 0x00, 0xD0, 0x40, 0x00, 0xA0, 0x3C,
            0x50, 0x00, 0x90, 0x3C, 0x64, 0x60, 0x80, 0x3C, 0x40, 0x00, 0xFF, 0x2F, 0x00,
        ];
        let (notes, _, controls) =
            extract_notes_and_control_events_from_bytes(&bytes).expect("parse failed");

        let kinds: Vec<(u8, u32)> = controls.iter().map(|e| (e.kind, e.tick)).collect();
        assert_eq!(kinds, vec![(3, 0), (4, 0)]);

        let ch_at = controls.iter().find(|e| e.kind == 3).expect("无通道触后");
        assert_eq!(ch_at.as_channel_aftertouch(), 64);
        let poly = controls.iter().find(|e| e.kind == 4).expect("无复音触后");
        assert_eq!(poly.as_poly_aftertouch(), (60, 80));

        // 0xD0 之后 NoteOn 必须在正确 tick/键/力度解析（错位即失败）
        assert_eq!(notes.len(), 1);
        // packed struct：先拷贝到局部变量再断言，避免非对齐引用
        let start_tick = notes[0].start_tick;
        let end_tick = notes[0].end_tick;
        let key = notes[0].key;
        let velocity = notes[0].velocity;
        assert_eq!(start_tick, 0);
        assert_eq!(end_tick, 96);
        assert_eq!(key, 60);
        assert_eq!(velocity, 100);
    }
}
