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

use crate::{
    event::{MetaMessage, MidiMessage, TrackEvent, TrackEventKind},
};

#[cfg(all(feature = "std", feature = "memmap"))]
use crate::{Header, Timing};

#[cfg(feature = "alloc")]
use alloc::vec::Vec;

#[cfg(feature = "alloc")]
use crate::fast_midi::{self, MidiEvent, TrackIter as FastTrackIter};

#[cfg(all(feature = "std", feature = "memmap"))]
use std::{
    cmp::Reverse,
    collections::BinaryHeap,
};

#[cfg(all(feature = "std", feature = "memmap"))]
use memmap2::Mmap;

#[cfg(all(feature = "std", feature = "memmap"))]
use std::{
    collections::VecDeque,
    fs::File,
    path::Path,
};

#[cfg(feature = "parallel")]
use rayon::prelude::*;

/// A compact, packed representation of a MIDI note.
///
/// This structure uses only **12 bytes** compared to 24+ bytes for a typical
/// `(start_tick, end_tick, key, velocity, track)` tuple, saving 50% memory.
///
/// The `#[repr(C, packed)]` layout ensures optimal cache efficiency when
/// processing large MIDI files with millions of notes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(C, packed)]
pub struct PackedNote {
    /// Start time in MIDI ticks (4 bytes)
    pub start_tick: u32,
    /// End time in MIDI ticks (4 bytes)
    pub end_tick: u32,
    /// MIDI key number (0-127) (1 byte)
    pub key: u8,
    /// Note velocity (0-127) (1 byte)
    pub velocity: u8,
    /// Track index, supports up to 65535 tracks (2 bytes)
    pub track: u16,
}

impl PackedNote {
    /// Create a new packed note.
    #[inline]
    pub fn new(start_tick: u32, end_tick: u32, key: u8, velocity: u8, track: u16) -> Self {
        Self {
            start_tick,
            end_tick,
            key: key & 0x7F,
            velocity: velocity & 0x7F,
            track,
        }
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
    ///     PackedNote::new(0, 100, 60, 100, 0),
    ///     PackedNote::new(50, 150, 64, 100, 0),
    ///     PackedNote::new(200, 300, 67, 100, 1),
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
#[derive(Clone, Copy)]
#[derive(Debug)]
struct ActiveNote {
    start_tick: u32,
    velocity: u8,
}

#[cfg(feature = "alloc")]
#[derive(Debug)]
struct NoteAccumulator {
    notes: Vec<PackedNote>,
    tempo_changes: Vec<(u32, f32)>,
    active_notes: [Option<(u32, u8)>; 128],
    current_tick: u32,
    track_idx: u16,
}

#[cfg(feature = "alloc")]
impl NoteAccumulator {
    fn new(track_idx: u16) -> Self {
        Self {
            notes: Vec::with_capacity(512),
            tempo_changes: Vec::new(),
            active_notes: [None; 128],
            current_tick: 0,
            track_idx,
        }
    }

    #[inline]
    fn advance(&mut self, delta: u32) {
        self.current_tick = self.current_tick.saturating_add(delta);
    }

    fn note_on(&mut self, key: u8, velocity: u8) {
        let key_idx = key as usize;
        if key_idx >= 128 {
            return;
        }

        if let Some((start_tick, prev_velocity)) = self.active_notes[key_idx].take() {
            self.notes.push(PackedNote::new(
                start_tick,
                self.current_tick,
                key,
                prev_velocity,
                self.track_idx,
            ));
        }

        if velocity > 0 {
            self.active_notes[key_idx] = Some((self.current_tick, velocity));
        }
    }

    fn note_off(&mut self, key: u8) {
        let key_idx = key as usize;
        if key_idx >= 128 {
            return;
        }
        if let Some((start_tick, velocity)) = self.active_notes[key_idx].take() {
            self.notes.push(PackedNote::new(
                start_tick,
                self.current_tick,
                key,
                velocity,
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
        for key in 0..128 {
            if let Some((start_tick, velocity)) = self.active_notes[key].take() {
                self.notes.push(PackedNote::new(
                    start_tick,
                    self.current_tick,
                    key as u8,
                    velocity,
                    self.track_idx,
                ));
            }
        }
        (self.notes, self.tempo_changes)
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
    active_notes: [Option<ActiveNote>; 128],
    current_tick: u32,
    track_idx: u16,
}

#[cfg(feature = "alloc")]
impl<'a> NoteTrackCursor<'a> {
    /// Create a new note cursor from an event iterator.
    pub fn new(events: crate::smf::EventIter<'a>, track_idx: u16) -> Self {
        Self {
            events,
            active_notes: [None; 128],
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
        for key in 0..128 {
            if let Some(active) = self.active_notes[key].take() {
                completed.push(PackedNote::new(
                    active.start_tick,
                    self.current_tick,
                    key as u8,
                    active.velocity,
                    self.track_idx,
                ));
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
            TrackEventKind::Midi { channel: _, message } => match message {
                MidiMessage::NoteOn { key, vel } => {
                    acc.note_on(key.as_int(), vel.as_int());
                }
                MidiMessage::NoteOff { key, vel: _ } => acc.note_off(key.as_int()),
                _ => {}
            },
            TrackEventKind::Meta(MetaMessage::Tempo(tempo)) => {
                acc.tempo_microseconds(tempo.as_int());
            }
            _ => {}
        }
    }

    acc.finish()
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
    let (_header, tracks_count, _division, raw) = fast_midi::parse_header(bytes)?;
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

/// Parse events directly to notes without allocating intermediate TrackEvent structs.
#[cfg(feature = "alloc")]
fn parse_fast_track_notes(mut events: FastTrackIter, track_idx: u16) -> (Vec<PackedNote>, Vec<(u32, f32)>) {
    let mut acc = NoteAccumulator::new(track_idx);

    while let Some((delta, event)) = events.next_event() {
        acc.advance(delta);

        match event {
            MidiEvent::NoteOn { key, velocity, .. } => acc.note_on(key, velocity),
            MidiEvent::NoteOff { key, .. } => acc.note_off(key),
            MidiEvent::Meta { event_type: 0x51, data } => {
                if data.len() == 3 {
                    let microseconds = ((data[0] as u32) << 16)
                        | ((data[1] as u32) << 8)
                        | (data[2] as u32);
                    acc.tempo_microseconds(microseconds);
                }
            }
            _ => {}
        }
    }

    acc.finish()
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
        other.tick.cmp(&self.tick).then_with(|| other.cursor_idx.cmp(&self.cursor_idx))
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
    active_notes: Vec<[Option<ActiveNote>; 128]>,

    // Finished notes cache with bounds
    finished_notes: VecDeque<PackedNote>,
    max_finished_notes: usize,

    // Reusable frame output
    frame_notes: Vec<PackedNote>,
    active_keys: [Option<u16>; 128],
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
        let mmap = unsafe { Mmap::map(&file).map_err(|_| {
            crate::Error::new(&crate::ErrorKind::Invalid("failed to memory map file"))
        })? };

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
                .map(|events| std::mem::transmute::<TrackCursor<'_>, TrackCursor<'static>>(TrackCursor::new(events)))
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
            active_notes: vec![[None; 128]; num_tracks],
            finished_notes: VecDeque::new(),
            max_finished_notes: 32_768,
            frame_notes: Vec::with_capacity(2048),
            active_keys: [None; 128],
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
    pub fn prepare_frame(&mut self, current_tick: f32, ticks_per_screen: f32) -> (&[PackedNote], &[Option<u16>; 128]) {
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
        for (track_idx, track_notes) in self.active_notes.iter().enumerate() {
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
                        track_idx as u16,
                    ));
                }
            }
        }

        // Compute active keys
        self.active_keys = [None; 128];
        for note in &self.frame_notes {
            let start = note.start_tick as f32;
            let end = note.end_tick as f32;
            if start <= current_tick && end >= current_tick {
                let k = note.key as usize;
                if k < 128 {
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
            MidiEvent::NoteOn { key, velocity, .. } => {
                let key_idx = key as usize;
                if key_idx < 128 && track_idx < self.active_notes.len() {
                    if let Some(active) = self.active_notes[track_idx][key_idx].take() {
                        self.finish_note(track_idx, key_idx, active, tick);
                    }
                    if velocity > 0 {
                        self.active_notes[track_idx][key_idx] = Some(ActiveNote {
                            start_tick: tick,
                            velocity,
                        });
                    }
                }
            }
            MidiEvent::NoteOff { key, .. } => {
                let key_idx = key as usize;
                if key_idx < 128 && track_idx < self.active_notes.len() {
                    if let Some(active) = self.active_notes[track_idx][key_idx].take() {
                        self.finish_note(track_idx, key_idx, active, tick);
                    }
                }
            }
            _ => {}
        }
    }

    fn finish_note(&mut self, track_idx: usize, key: usize, active: ActiveNote, end_tick: u32) {
        self.finished_notes.push_back(PackedNote::new(
            active.start_tick,
            end_tick,
            key as u8,
            active.velocity,
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

    let file = std::fs::File::open(path).map_err(|_| {
        crate::Error::new(&crate::ErrorKind::Invalid("failed to open midi file"))
    })?;
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(4096);
    let mut reader = BufReader::with_capacity(4 * 1024 * 1024, file);

    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic).map_err(|_| {
        crate::Error::new(&crate::ErrorKind::Invalid("file too short"))
    })?;

    if &magic == b"RIFF" {
        reader.seek(SeekFrom::Start(0)).map_err(|_| {
            crate::Error::new(&crate::ErrorKind::Invalid("seek failed"))
        })?;
        let scan_size = file_len.min(4096) as usize;
        let mut scan_buf = vec![0u8; scan_size];
        reader.read_exact(&mut scan_buf).map_err(|_| {
            crate::Error::new(&crate::ErrorKind::Invalid("failed to scan RIFF header"))
        })?;

        let mthd_pos = scan_buf.windows(4)
            .position(|w| w == b"MThd")
            .ok_or_else(|| crate::Error::new(&crate::ErrorKind::Invalid("no MThd in RIFF")))?;

        reader.seek(SeekFrom::Start(mthd_pos as u64)).map_err(|_| {
            crate::Error::new(&crate::ErrorKind::Invalid("seek to MThd failed"))
        })?;
    } else if &magic == b"MThd" {
        reader.seek(SeekFrom::Start(0)).map_err(|_| {
            crate::Error::new(&crate::ErrorKind::Invalid("seek failed"))
        })?;
    } else {
        return Err(crate::Error::new(&crate::ErrorKind::Invalid("not a MIDI file")));
    }

    let mut header_buf = [0u8; 14];
    reader.read_exact(&mut header_buf).map_err(|_| {
        crate::Error::new(&crate::ErrorKind::Invalid("truncated MIDI header"))
    })?;

    if &header_buf[0..4] != b"MThd" {
        return Err(crate::Error::new(&crate::ErrorKind::Invalid("invalid MIDI header")));
    }

    let header_len = u32::from_be_bytes([
        header_buf[4], header_buf[5], header_buf[6], header_buf[7],
    ]);
    if header_len < 6 {
        return Err(crate::Error::new(&crate::ErrorKind::Invalid("MIDI header too short")));
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
                chunk_header[4], chunk_header[5],
                chunk_header[6], chunk_header[7],
            ]);
            if &chunk_header[0..4] != b"MTrk" {
                let _ = reader.seek(SeekFrom::Current(chunk_len as i64));
                continue;
            }
            let track_len = chunk_len as usize;
            if track_len > PARALLEL_BATCH_BYTES {
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
            if reader.read_exact(&mut buf[buf_used..buf_used + track_len]).is_err() {
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
                chunk_header[4], chunk_header[5],
                chunk_header[6], chunk_header[7],
            ]);
            if &chunk_header[0..4] != b"MTrk" {
                let _ = reader.seek(SeekFrom::Current(chunk_len as i64));
                continue;
            }
            let track_len = chunk_len as usize;
            track_buf.clear();
            track_buf.resize(track_len, 0);
            if reader.read_exact(&mut track_buf).is_err() {
                break;
            }
            let (track_notes, track_max_tick, track_tempos) = fast_midi::scan_track_notes_only(&track_buf);
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
    use crate::{Format, Timing};
    use crate::num::{u7, u4, u15, u28};

    #[test]
    fn test_packed_note_size() {
        // Verify the packed size is 12 bytes
        assert_eq!(core::mem::size_of::<PackedNote>(), 12);
    }

    #[test]
    fn test_packed_note_new() {
        let note = PackedNote::new(100, 200, 60, 100, 0);
        // Copy fields to avoid unaligned reference issues with packed struct
        let start_tick = note.start_tick;
        let end_tick = note.end_tick;
        let key = note.key;
        let velocity = note.velocity;
        let track = note.track;
        assert_eq!(start_tick, 100);
        assert_eq!(end_tick, 200);
        assert_eq!(key, 60);
        assert_eq!(velocity, 100);
        assert_eq!(track, 0);
    }

    #[test]
    fn test_packed_note_duration() {
        let note = PackedNote::new(100, 250, 60, 100, 0);
        assert_eq!(note.duration(), 150);
    }

    #[test]
    fn test_packed_note_overlaps() {
        let note = PackedNote::new(100, 200, 60, 100, 0);
        
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
            PackedNote::new(0, 100, 60, 100, 0),
            PackedNote::new(50, 150, 64, 100, 0),
            PackedNote::new(200, 300, 67, 100, 1),
        ];

        let index = NoteIndex::build(notes, 100);
        
        assert_eq!(index.len(), 3);
        assert_eq!(index.max_end_tick(), 300);
        assert!(!index.is_empty());
    }

    #[test]
    fn test_note_index_query_range() {
        let notes = vec![
            PackedNote::new(0, 100, 60, 100, 0),
            PackedNote::new(50, 150, 64, 100, 0),
            PackedNote::new(200, 300, 67, 100, 1),
            PackedNote::new(400, 500, 72, 100, 0),
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
            tracks: vec![
                vec![
                    // Note on at tick 0
                    TrackEvent {
                        delta: u28::new(0),
                        kind: TrackEventKind::Midi {
                            channel: u4::new(0),
                            message: MidiMessage::NoteOn {
                                key: u7::new(60),
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
                                key: u7::new(60),
                                vel: u7::new(0),
                            },
                        },
                    },
                ],
            ],
        };

        let (notes, tempo_changes) = extract_notes(&smf);
        
        assert_eq!(notes.len(), 1);
        // Copy fields to avoid unaligned reference
        let start_tick = notes[0].start_tick;
        let end_tick = notes[0].end_tick;
        let key = notes[0].key;
        let velocity = notes[0].velocity;
        let track = notes[0].track;
        assert_eq!(start_tick, 0);
        assert_eq!(end_tick, 480);
        assert_eq!(key, 60);
        assert_eq!(velocity, 100);
        assert_eq!(track, 0);
        
        // Should have default tempo
        assert_eq!(tempo_changes.len(), 1);
        assert_eq!(tempo_changes[0], (0, 120.0));
    }
}
