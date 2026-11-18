//! 独立参考累加器：直接消费 `Smf::parse` 的轨事件，产出与 `TrackAllEvents`
//! 同构的期望值。**不依赖 loader 的 NoteAccumulator / ControlEventAccumulator**，
//! 配对语义按 MIDI 规范与快路径文档独立实现，用于交叉验证。
//!
//! 锁定的当前语义（0.6.5 基线，EOT 元数据尚未实现）：
//! - 单 (channel,key) 只保留一个活跃音符；重触发关闭旧音符，释放力度记 0；
//! - NoteOn vel=0 等价 NoteOff（释放力度 0）；
//! - 未闭合音符在**最后一个事件 tick**（含 EOT delta）收尾，释放力度 0，
//!   按 channel 升序 × key 升序追加（与 NoteAccumulator::finish 一致）；
//! - 音符按 NoteOff 到达顺序产出（非 start_tick 排序）；
//! - F0 SysEx 与 F7 Escape 均并入 sys_ex（快路径当前行为）；
//! - MidiPort 取首个出现值；TrackName(0x03) 不收录（下游 scan_track_names 负责）。

use midly::loader::TrackTiming;
use midly::{MetaMessage, MidiMessage, TrackEvent, TrackEventKind};

/// 参考音符（与 PackedNote 字段一一对应，不含 track）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefNote {
    pub start_tick: u32,
    pub end_tick: u32,
    pub key: u8,
    pub velocity: u8,
    pub release_velocity: u8,
    pub channel: u8,
}

/// 参考控制事件（与 PackedControlEvent 编码一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefControl {
    pub tick: u32,
    /// 0=CC, 1=PC, 2=PB, 3=ChannelAT, 4=PolyAT
    pub kind: u8,
    pub channel: u8,
    pub param: u16,
}

/// 单轨期望事件集合。
#[derive(Debug, Default)]
pub struct RefTrack {
    pub notes: Vec<RefNote>,
    pub tempo: Vec<(u32, f32)>,
    pub time_sigs: Vec<(u32, u8, u8)>,
    pub key_sigs: Vec<(u32, i8, bool)>,
    pub controls: Vec<RefControl>,
    pub lyrics: Vec<(u32, Vec<u8>)>,
    pub markers: Vec<(u32, Vec<u8>)>,
    /// (tick, meta_type, payload)
    pub texts: Vec<(u32, u8, Vec<u8>)>,
    pub sysex: Vec<(u32, Vec<u8>)>,
    pub midi_port: Option<u8>,
    /// EOT 结构元数据（指导书 §2.5），由参考链独立计算。
    pub timing: TrackTiming,
}

/// 独立实现的单轨累加。
pub fn accumulate_track(track: &[TrackEvent], _idx: usize) -> RefTrack {
    let mut out = RefTrack::default();
    // 活跃音符表：(channel, key) -> (start_tick, attack_velocity)
    let mut active: [[Option<(u32, u8)>; 256]; 16] = [[None; 256]; 16];
    let mut tick: u32 = 0;
    // EOT 结构元数据：首个 EOT、最后非 EOT 事件、EOT 后事件。
    // 0x2F 无论长度（Smf 分类为 EndOfTrack 或 Unknown）都按 EOT 处理，
    // 与快路径「event_type == 0x2F 即 EOT」的宽松契约一致。
    let mut eot_tick: Option<u32> = None;
    let mut content_end_tick: u32 = 0;
    let mut has_events_after_eot = false;

    for event in track {
        tick = tick.saturating_add(event.delta.as_int());
        let is_eot = matches!(&event.kind, TrackEventKind::Meta(MetaMessage::EndOfTrack))
            || matches!(
                &event.kind,
                TrackEventKind::Meta(MetaMessage::Unknown(0x2F, _))
            );
        if eot_tick.is_some() {
            has_events_after_eot = true;
        }
        if is_eot {
            if eot_tick.is_none() {
                eot_tick = Some(tick);
            }
        } else {
            content_end_tick = tick;
        }

        match &event.kind {
            TrackEventKind::Midi { channel, message } => {
                let ch = channel.as_int();
                match message {
                    MidiMessage::NoteOn { key, vel } => {
                        // 本 fork 的 NoteOn/NoteOff key 为 u8（支持 256 键黑乐谱），vel 为 u7。
                        let (key, vel) = (*key, vel.as_int());
                        if vel == 0 {
                            close_note(&mut out, &mut active, ch, key, tick, 0);
                        } else {
                            // 重触发：先关旧音符（释放力度 0），再开新音符。
                            close_note(&mut out, &mut active, ch, key, tick, 0);
                            active[ch as usize][key as usize] = Some((tick, vel));
                        }
                    }
                    MidiMessage::NoteOff { key, vel } => {
                        close_note(&mut out, &mut active, ch, *key, tick, vel.as_int());
                    }
                    MidiMessage::Controller { controller, value } => {
                        out.controls.push(RefControl {
                            tick,
                            kind: 0,
                            channel: ch,
                            param: ((controller.as_int() as u16) << 8) | value.as_int() as u16,
                        });
                    }
                    MidiMessage::ProgramChange { program } => {
                        out.controls.push(RefControl {
                            tick,
                            kind: 1,
                            channel: ch,
                            param: program.as_int() as u16,
                        });
                    }
                    MidiMessage::PitchBend { bend } => {
                        // PitchBend::as_int() 返回 [-8192,8191] 偏差；
                        // PackedControlEvent 文档编码为原始 14-bit（0..16383），
                        // 必须取 bend.0（u14）原值，与快路径 (msb<<7)|lsb 一致。
                        out.controls.push(RefControl {
                            tick,
                            kind: 2,
                            channel: ch,
                            param: bend.0.as_int(),
                        });
                    }
                    MidiMessage::ChannelAftertouch { vel } => {
                        out.controls.push(RefControl {
                            tick,
                            kind: 3,
                            channel: ch,
                            param: vel.as_int() as u16,
                        });
                    }
                    MidiMessage::Aftertouch { key, vel } => {
                        out.controls.push(RefControl {
                            tick,
                            kind: 4,
                            channel: ch,
                            param: ((key.as_int() as u16) << 8) | vel.as_int() as u16,
                        });
                    }
                }
            }
            TrackEventKind::Meta(meta) => match meta {
                MetaMessage::Tempo(us) => {
                    let us = us.as_int();
                    if us > 0 {
                        out.tempo.push((tick, 60_000_000.0 / us as f32));
                    }
                }
                MetaMessage::TimeSignature(num, den, _, _) => {
                    out.time_sigs.push((tick, *num, *den));
                }
                MetaMessage::KeySignature(sharps, minor) => {
                    out.key_sigs.push((tick, *sharps, *minor));
                }
                MetaMessage::Lyric(data) => out.lyrics.push((tick, data.to_vec())),
                MetaMessage::Marker(data) => out.markers.push((tick, data.to_vec())),
                MetaMessage::Text(data) => out.texts.push((tick, 0x01, data.to_vec())),
                MetaMessage::Copyright(data) => out.texts.push((tick, 0x02, data.to_vec())),
                MetaMessage::InstrumentName(data) => out.texts.push((tick, 0x04, data.to_vec())),
                MetaMessage::CuePoint(data) => out.texts.push((tick, 0x07, data.to_vec())),
                MetaMessage::ProgramName(data) => out.texts.push((tick, 0x08, data.to_vec())),
                MetaMessage::DeviceName(data) => out.texts.push((tick, 0x09, data.to_vec())),
                MetaMessage::MidiPort(port) => {
                    if out.midi_port.is_none() {
                        out.midi_port = Some(port.as_int());
                    }
                }
                // Smf 对长度不足的 meta 分类为 Unknown（如 0x58 要求 len>=4）；
                // 快路径契约更宽松（0x58 len>=2 / 0x51 len==3 / 0x59 len>=2 即保留）。
                // 参考实现独立建模快路径的宽松契约——保留数据而非静默丢弃。
                MetaMessage::Unknown(ty, data) => match ty {
                    0x51 if data.len() == 3 => {
                        let us =
                            ((data[0] as u32) << 16) | ((data[1] as u32) << 8) | data[2] as u32;
                        if us > 0 {
                            out.tempo.push((tick, 60_000_000.0 / us as f32));
                        }
                    }
                    0x58 if data.len() >= 2 => {
                        out.time_sigs.push((tick, data[0], data[1]));
                    }
                    0x59 if data.len() >= 2 => {
                        out.key_sigs.push((tick, data[0] as i8, data[1] != 0));
                    }
                    _ => {}
                },
                // TrackName / EndOfTrack / 其余 meta：快路径不收录。
                _ => {}
            },
            TrackEventKind::SysEx(data) | TrackEventKind::Escape(data) => {
                out.sysex.push((tick, data.to_vec()));
            }
        }
    }

    // 未闭合音符：在最后一个事件 tick 收尾，channel × key 升序，释放力度 0。
    for (ch, row) in active.iter_mut().enumerate() {
        for (key, slot) in row.iter_mut().enumerate() {
            if let Some((start, vel)) = slot.take() {
                out.notes.push(RefNote {
                    start_tick: start,
                    end_tick: tick,
                    key: key as u8,
                    velocity: vel,
                    release_velocity: 0,
                    channel: ch as u8,
                });
            }
        }
    }
    out.timing = TrackTiming {
        eot_tick,
        physical_end_tick: tick,
        content_end_tick,
        has_events_after_eot,
    };
    out
}

/// 关闭 (ch,key) 的活跃音符（若存在），按到达顺序追加。
fn close_note(
    out: &mut RefTrack,
    active: &mut [[Option<(u32, u8)>; 256]; 16],
    ch: u8,
    key: u8,
    tick: u32,
    release: u8,
) {
    let ch_idx = (ch & 0x0F) as usize;
    let key_idx = key as usize;
    if key_idx >= 256 {
        return;
    }
    if let Some((start, vel)) = active[ch_idx][key_idx].take() {
        out.notes.push(RefNote {
            start_tick: start,
            end_tick: tick,
            key,
            velocity: vel,
            release_velocity: release,
            channel: ch,
        });
    }
}
