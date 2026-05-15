use clap::Parser;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to input MIDI file
    input: PathBuf,

    /// Path to output MIDI file
    #[arg(short, long)]
    output: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();
    let input_path = cli.input;
    let output_path = cli.output.clone().unwrap_or_else(|| {
        let mut p = input_path.clone();
        if let Some(stem) = p.file_stem() {
            let mut new_stem = stem.to_os_string();
            new_stem.push("_fixed");
            p.set_file_name(new_stem);
            if let Some(ext) = input_path.extension() {
                p.set_extension(ext);
            }
        } else {
            p.set_extension("fixed.mid");
        }
        p
    });

    println!("Loading MIDI from: {:?}", input_path);
    let input_file = match File::open(&input_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error opening file: {}", e);
            return;
        }
    };

    // Use BufReader with a large buffer for performance
    let mut reader = BufReader::with_capacity(1024 * 1024, input_file);

    let out_file = match File::create(&output_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error creating output file: {}", e);
            return;
        }
    };
    let mut out_writer = std::io::BufWriter::new(out_file);

    println!("Processing to: {:?}", output_path);

    if let Err(e) = process_midi(&mut reader, &mut out_writer) {
        eprintln!("Error processing MIDI: {}", e);
    } else {
        println!("Done.");
    }
}

fn process_midi<R: Read + Seek, W: Write + Seek>(reader: &mut R, writer: &mut W) -> io::Result<()> {
    // 1. Read Header
    let mut header_chunk = [0u8; 14];
    reader.read_exact(&mut header_chunk)?;

    if &header_chunk[0..4] != b"MThd" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Not a valid MIDI file (missing MThd)",
        ));
    }

    let header_len = u32::from_be_bytes(header_chunk[4..8].try_into().unwrap());
    if header_len != 6 {
        // Some files might have larger headers, skip extra
        if header_len < 6 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid MThd length",
            ));
        }
    }

    // Write Header to output
    writer.write_all(b"MThd")?;
    writer.write_all(&6u32.to_be_bytes())?;
    writer.write_all(&header_chunk[8..14])?; // format, tracks, division

    // If header_len > 6, skip remaining
    if header_len > 6 {
        reader.seek(SeekFrom::Current((header_len - 6) as i64))?;
    }

    // 2. Process Tracks
    loop {
        // Read Chunk Header
        let mut chunk_head = [0u8; 8];
        if let Err(_) = reader.read_exact(&mut chunk_head) {
            // EOF likely
            break;
        }

        let tag = &chunk_head[0..4];
        let len = u32::from_be_bytes(chunk_head[4..8].try_into().unwrap());

        if tag == b"MTrk" {
            process_track(reader, writer, len)?;
        } else {
            // Unknown chunk, skip
            reader.seek(SeekFrom::Current(len as i64))?;
        }
    }

    writer.flush()?;
    Ok(())
}

fn process_track<R: Read, W: Write + Seek>(
    reader: &mut R,
    writer: &mut W,
    len: u32,
) -> io::Result<()> {
    writer.write_all(b"MTrk")?;
    let len_pos = writer.stream_position()?;
    writer.write_all(&0u32.to_be_bytes())?; // Placeholder for length
    let content_start = writer.stream_position()?;

    let mut parser = TrackParser::new(reader.take(len as u64));
    let mut writer_state = TrackWriterState::new();

    // Need to track bytes read to ensure we don't go past `len`?
    // `reader.take(len)` handles that!

    while let Some(event_res) = parser.next_event() {
        let event = event_res?;

        // Logic:
        // 1. Accumulate Delta.
        // 2. Check Event Type.
        // 3. Filter Low Velocity NoteOn.
        // 4. Handle Overlap.

        writer_state.accumulate_delta(event.delta);

        match event.kind {
            RawEventKind::Midi {
                status,
                data1,
                data2,
            } => {
                let msg_type = status & 0xF0;
                let ch = (status & 0x0F) as usize;

                if msg_type == 0x90 {
                    // NoteOn
                    let vel = data2.unwrap_or(0);
                    let key = data1;
                    if vel > 10 {
                        // Filter quiet notes (vel <= 10)
                        writer_state.handle_note_on(ch, key, vel, status, writer)?;
                    } else if vel == 0 {
                        // NoteOn(0) is NoteOff
                        writer_state.handle_note_off(ch, key, 0, writer)?;
                    } else {
                        // vel is 1..10, skip (filtered)
                    }
                } else if msg_type == 0x80 {
                    // NoteOff
                    let vel = data2.unwrap_or(0);
                    let key = data1;
                    writer_state.handle_note_off(ch, key, vel, writer)?;
                } else {
                    // Other Midi
                    writer_state.write_pending_delta(writer)?;
                    writer_state.write_midi(status, data1, data2, writer)?;
                }
            }
            RawEventKind::SysEx { data } => {
                writer_state.write_pending_delta(writer)?;
                writer.write_all(&[0xF0])?;
                write_varlen(data.len() as u32, writer)?;
                writer.write_all(&data)?;
                writer_state.running_status = None;
            }
            RawEventKind::Escape { data } => {
                writer_state.write_pending_delta(writer)?;
                writer.write_all(&[0xF7])?;
                write_varlen(data.len() as u32, writer)?;
                writer.write_all(&data)?;
                writer_state.running_status = None;
            }
            RawEventKind::Meta { type_byte, data } => {
                writer_state.write_pending_delta(writer)?;
                writer.write_all(&[0xFF, type_byte])?;
                write_varlen(data.len() as u32, writer)?;
                writer.write_all(&data)?;
                writer_state.running_status = None;
            }
        }
    }

    let content_end = writer.stream_position()?;
    let new_len = (content_end - content_start) as u32;
    writer.seek(SeekFrom::Start(len_pos))?;
    writer.write_all(&new_len.to_be_bytes())?;
    writer.seek(SeekFrom::Start(content_end))?;

    Ok(())
}

struct TrackWriterState {
    pending_delta: u32,
    running_status: Option<u8>,
    active_notes: [[bool; 128]; 16],
    // We don't track start time, just active state
}

impl TrackWriterState {
    fn new() -> Self {
        Self {
            pending_delta: 0,
            running_status: None,
            active_notes: [[false; 128]; 16],
        }
    }

    fn accumulate_delta(&mut self, delta: u32) {
        self.pending_delta += delta;
    }

    fn write_pending_delta<W: Write>(&mut self, writer: &mut W) -> io::Result<()> {
        write_varlen(self.pending_delta, writer)?;
        self.pending_delta = 0;
        Ok(())
    }

    fn handle_note_on<W: Write>(
        &mut self,
        ch: usize,
        key: u8,
        vel: u8,
        _status: u8,
        writer: &mut W,
    ) -> io::Result<()> {
        if self.active_notes[ch][key as usize] {
            // Overlap! Cut previous note.
            self.write_pending_delta(writer)?;

            // Write NoteOff(key, 0) - Prefer NoteOn(0)
            let status_on = 0x90 | (ch as u8);
            if self.running_status == Some(status_on) {
                writer.write_all(&[key, 0])?;
            } else {
                writer.write_all(&[status_on, key, 0])?;
                self.running_status = Some(status_on);
            }

            // New NoteOn
            // Delta is 0 now
            write_varlen(0, writer)?;
            // Write data
            if self.running_status == Some(status_on) {
                writer.write_all(&[key, vel])?;
            } else {
                writer.write_all(&[status_on, key, vel])?;
                self.running_status = Some(status_on);
            }
        } else {
            // No overlap, just write
            self.active_notes[ch][key as usize] = true;
            self.write_pending_delta(writer)?;

            let status_on = 0x90 | (ch as u8);
            if self.running_status == Some(status_on) {
                writer.write_all(&[key, vel])?;
            } else {
                writer.write_all(&[status_on, key, vel])?;
                self.running_status = Some(status_on);
            }
        }
        Ok(())
    }

    fn handle_note_off<W: Write>(
        &mut self,
        ch: usize,
        key: u8,
        vel: u8,
        writer: &mut W,
    ) -> io::Result<()> {
        if self.active_notes[ch][key as usize] {
            self.active_notes[ch][key as usize] = false;
            self.write_pending_delta(writer)?;

            // Write NoteOff
            if vel == 0 {
                let status_on = 0x90 | (ch as u8);
                if self.running_status == Some(status_on) {
                    writer.write_all(&[key, 0])?;
                } else {
                    writer.write_all(&[status_on, key, 0])?;
                    self.running_status = Some(status_on);
                }
            } else {
                let status_off = 0x80 | (ch as u8);
                if self.running_status == Some(status_off) {
                    writer.write_all(&[key, vel])?;
                } else {
                    writer.write_all(&[status_off, key, vel])?;
                    self.running_status = Some(status_off);
                }
            }
        } else {
            // Orphan NoteOff. Skip.
            // But delta Accumulation happens before this call!
        }
        Ok(())
    }

    fn write_midi<W: Write>(
        &mut self,
        status: u8,
        data1: u8,
        maybe_data2: Option<u8>,
        writer: &mut W,
    ) -> io::Result<()> {
        if self.running_status != Some(status) {
            writer.write_all(&[status])?;
            self.running_status = Some(status);
        }
        writer.write_all(&[data1])?;
        if let Some(d2) = maybe_data2 {
            writer.write_all(&[d2])?;
        }
        Ok(())
    }
}

// Streaming Parser
struct TrackParser<R> {
    reader: R,
    running_status: Option<u8>,
}

struct RawEvent {
    delta: u32,
    kind: RawEventKind,
}

enum RawEventKind {
    Midi {
        status: u8,
        data1: u8,
        data2: Option<u8>,
    },
    SysEx {
        data: Vec<u8>,
    },
    Escape {
        data: Vec<u8>,
    },
    Meta {
        type_byte: u8,
        data: Vec<u8>,
    },
}

impl<R: Read> TrackParser<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            running_status: None,
        }
    }

    fn next_event(&mut self) -> Option<io::Result<RawEvent>> {
        // Read Delta
        let delta = match read_varlen(&mut self.reader) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return None,
            Err(e) => return Some(Err(e)),
        };

        // Read Status
        let mut status_byte = [0u8];
        if let Err(e) = self.reader.read_exact(&mut status_byte) {
            return Some(Err(e));
        }

        let mut status = status_byte[0];

        // Handle Running Status
        if status < 0x80 {
            // Data Byte -> Use RS
            if let Some(rs) = self.running_status {
                return Some(self.parse_event_with_rs(delta, rs, Some(status)));
            } else {
                return Some(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Data byte without Running Status",
                )));
            }
        } else {
            // Status Byte
            if status < 0xF0 {
                self.running_status = Some(status);
                // We need to read data bytes
                return Some(self.parse_event_with_rs(delta, status, None));
            } else if status == 0xF0 {
                self.running_status = None;
                return Some(self.parse_sysex(delta));
            } else if status == 0xF7 {
                self.running_status = None;
                return Some(self.parse_escape(delta));
            } else if status == 0xFF {
                self.running_status = None;
                return Some(self.parse_meta(delta));
            } else {
                // System Common F1-F6
                self.running_status = None;
                return Some(self.parse_system_common(delta, status));
            }
        }
    }

    fn parse_event_with_rs(
        &mut self,
        delta: u32,
        status: u8,
        first_byte: Option<u8>,
    ) -> io::Result<RawEvent> {
        let high = status & 0xF0;
        let takes_2 = high != 0xC0 && high != 0xD0;

        let data1 = if let Some(b) = first_byte {
            b
        } else {
            let mut b = [0u8];
            self.reader.read_exact(&mut b)?;
            b[0]
        };

        let data2 = if takes_2 {
            let mut b = [0u8];
            self.reader.read_exact(&mut b)?;
            Some(b[0])
        } else {
            None
        };

        Ok(RawEvent {
            delta,
            kind: RawEventKind::Midi {
                status,
                data1,
                data2,
            },
        })
    }

    fn parse_sysex(&mut self, delta: u32) -> io::Result<RawEvent> {
        let len = read_varlen(&mut self.reader)?;
        let mut data = vec![0u8; len as usize];
        self.reader.read_exact(&mut data)?;
        Ok(RawEvent {
            delta,
            kind: RawEventKind::SysEx { data },
        })
    }

    fn parse_escape(&mut self, delta: u32) -> io::Result<RawEvent> {
        let len = read_varlen(&mut self.reader)?;
        let mut data = vec![0u8; len as usize];
        self.reader.read_exact(&mut data)?;
        Ok(RawEvent {
            delta,
            kind: RawEventKind::Escape { data },
        })
    }

    fn parse_meta(&mut self, delta: u32) -> io::Result<RawEvent> {
        let mut type_b = [0u8];
        self.reader.read_exact(&mut type_b)?;
        let len = read_varlen(&mut self.reader)?;
        let mut data = vec![0u8; len as usize];
        self.reader.read_exact(&mut data)?;
        Ok(RawEvent {
            delta,
            kind: RawEventKind::Meta {
                type_byte: type_b[0],
                data,
            },
        })
    }

    fn parse_system_common(&mut self, delta: u32, status: u8) -> io::Result<RawEvent> {
        let data1: u8;
        let data2: Option<u8>;

        if status == 0xF1 || status == 0xF3 {
            let mut b = [0u8];
            self.reader.read_exact(&mut b)?;
            data1 = b[0];
            data2 = None;
        } else if status == 0xF2 {
            let mut b = [0u8; 2];
            self.reader.read_exact(&mut b)?;
            data1 = b[0];
            data2 = Some(b[1]);
        } else {
            // F6, F8..FE: 0 data
            return Ok(RawEvent {
                delta,
                kind: RawEventKind::Midi {
                    status,
                    data1: 0,
                    data2: None,
                },
            });
        }

        Ok(RawEvent {
            delta,
            kind: RawEventKind::Midi {
                status,
                data1,
                data2,
            },
        })
    }
}

fn read_varlen<R: Read>(reader: &mut R) -> io::Result<u32> {
    let mut val = 0u32;
    for _ in 0..4 {
        let mut b = [0u8];
        reader.read_exact(&mut b)?;
        val = (val << 7) | (b[0] & 0x7F) as u32;
        if b[0] & 0x80 == 0 {
            return Ok(val);
        }
    }
    // If we hit here, 4th byte exceeded or loop finished
    Ok(val)
}

fn write_varlen<W: Write>(mut value: u32, out: &mut W) -> io::Result<()> {
    if value == 0 {
        return out.write_all(&[0]);
    }
    value &= 0x0FFFFFFF; // clamp

    let mut buf = [0u8; 4];
    if value < 0x80 {
        buf[0] = value as u8;
        out.write_all(&buf[0..1])
    } else if value < 0x4000 {
        buf[0] = ((value >> 7) & 0x7F) as u8 | 0x80;
        buf[1] = (value & 0x7F) as u8;
        out.write_all(&buf[0..2])
    } else if value < 0x200000 {
        buf[0] = ((value >> 14) & 0x7F) as u8 | 0x80;
        buf[1] = ((value >> 7) & 0x7F) as u8 | 0x80;
        buf[2] = (value & 0x7F) as u8;
        out.write_all(&buf[0..3])
    } else {
        buf[0] = ((value >> 21) & 0x7F) as u8 | 0x80;
        buf[1] = ((value >> 14) & 0x7F) as u8 | 0x80;
        buf[2] = ((value >> 7) & 0x7F) as u8 | 0x80;
        buf[3] = (value & 0x7F) as u8;
        out.write_all(&buf[0..4])
    }
}
