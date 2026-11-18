//! P2 素材合成：补齐性能矩阵缺失的 4 类形态（指导书 §P2 素材要求）。
//!
//! 直接写 SMF 字节，不依赖 `Smf::write`（避免编码器与解析器共享 bug 互相掩盖）。
//! 生成物写入 `test-asset/generated/`（不入库）：生成器是确定性的，重跑即得
//! 同样字节；`tests` 模块用 `Smf::parse` + 快路径双重校验生成结果，防止
//! 「素材本身畸形导致性能结论失真」。
//!
//! 四类形态：
//! - `gen-single-small.mid`：单轨小文件（1 轨 / 120 音符 / ~1KB）；
//! - `gen-single-giant.mid`：单巨轨（1 轨 / 1,000,000 音符）——P5 轨内并行素材；
//! - `gen-control-dense.mid`：控制密集（8 轨 / 12.8 万音符 / 128 万控制事件）；
//! - `gen-sysex-dense.mid`：SysEx 密集（4 轨 / 800 条 8KB SysEx ≈ 6.4MB）。

use std::fs;
use std::path::Path;

/// 生成全部素材到 `out_dir`，返回 `(文件名, 字节数)`。
pub fn generate_all(out_dir: &Path) -> std::io::Result<Vec<(String, usize)>> {
    fs::create_dir_all(out_dir)?;
    let items: Vec<(&str, Vec<u8>)> = vec![
        ("gen-single-small.mid", single_track_small()),
        ("gen-single-giant.mid", single_giant_track(1_000_000)),
        ("gen-control-dense.mid", control_dense(8, 16_000)),
        ("gen-sysex-dense.mid", sysex_dense(4, 200, 8 * 1024)),
    ];
    let mut written = Vec::new();
    for (name, bytes) in items {
        fs::write(out_dir.join(name), &bytes)?;
        written.push((name.to_string(), bytes.len()));
    }
    Ok(written)
}

// ── 生成规格（供测试与文档引用）──────────────────────────────────────────────

/// 单巨轨音符数。
pub const GIANT_NOTES: u32 = 1_000_000;
/// 控制密集：每轨音符数。
pub const CONTROL_NOTES_PER_TRACK: u32 = 16_000;
/// 控制密集：轨数。
pub const CONTROL_TRACKS: usize = 8;
/// SysEx 密集：每轨 SysEx 条数。
pub const SYSEX_PER_TRACK: usize = 200;
/// SysEx 密集：单条 SysEx 载荷字节数（含结尾 0xF7）。
pub const SYSEX_PAYLOAD: usize = 8 * 1024;
/// SysEx 密集：轨数。
pub const SYSEX_TRACKS: usize = 4;

// ── 四类素材 ────────────────────────────────────────────────────────────────

/// 单轨小文件：1 轨、120 个不重叠音符、1KB 级。用来测派发开销与 serial 阈值。
pub fn single_track_small() -> Vec<u8> {
    let mut t = Vec::with_capacity(1024);
    ev_meta(&mut t, 0, 0x03, b"gen-single-small");
    // 120 BPM、4/4。
    ev_meta(&mut t, 0, 0x51, &[0x07, 0xA1, 0x20]);
    ev_meta(&mut t, 0, 0x58, &[4, 2, 24, 8]);
    for i in 0..120u32 {
        let key = 60 + (i % 12) as u8;
        ev_note_on(&mut t, 0, 0, key, 100);
        ev_note_off(&mut t, 120, 0, key, 64);
    }
    ev_meta(&mut t, 0, 0x2F, &[]);
    smf(0, 480, vec![t])
}

/// 单巨轨：format 0、1 轨、`notes` 个音符（默认 100 万）。P5 轨内并行的主素材。
pub fn single_giant_track(notes: u32) -> Vec<u8> {
    let mut t = Vec::with_capacity(notes as usize * 8 + 64);
    ev_meta(&mut t, 0, 0x03, b"gen-single-giant");
    ev_meta(&mut t, 0, 0x51, &[0x07, 0xA1, 0x20]);
    for i in 0..notes {
        let key = 21 + (i % 88) as u8;
        ev_note_on(&mut t, if i == 0 { 0 } else { 1 }, 0, key, 90);
        ev_note_off(&mut t, 0, 0, key, 40);
    }
    ev_meta(&mut t, 0, 0x2F, &[]);
    smf(0, 480, vec![t])
}

/// 控制密集：format 1、`tracks` 轨、每轨 `notes_per_track` 音符，
/// 每个音符配 4 CC + 1 PitchBend + 1 ChannelAT + 1 PolyAT（控制:音符 = 7:1）。
pub fn control_dense(tracks: usize, notes_per_track: u32) -> Vec<u8> {
    let mut bodies = Vec::with_capacity(tracks);
    for tr in 0..tracks {
        let ch = (tr % 16) as u8;
        let mut t = Vec::with_capacity(notes_per_track as usize * 40 + 64);
        ev_meta(&mut t, 0, 0x03, format!("gen-control-{tr}").as_bytes());
        ev_meta(&mut t, 0, 0x51, &[0x07, 0xA1, 0x20]);
        for i in 0..notes_per_track {
            let key = 30 + (i % 60) as u8;
            let val = (i % 128) as u8;
            for c in 0..4u8 {
                ev_cc(&mut t, 0, ch, 1 + c, val);
            }
            ev_pitch_bend(&mut t, 0, ch, (i % 16384) as u16);
            ev_channel_at(&mut t, 0, ch, val);
            ev_poly_at(&mut t, 0, ch, key, val);
            ev_note_on(&mut t, 1, ch, key, 100);
            ev_note_off(&mut t, 1, ch, key, 0);
        }
        ev_meta(&mut t, 0, 0x2F, &[]);
        bodies.push(t);
    }
    smf(1, 480, bodies)
}

/// SysEx 密集：format 1、`tracks` 轨、每轨 `count` 条 `payload_len` 字节 SysEx，
/// 间插少量音符与 F7 Escape（快路径把两者都并入 sys_ex）。
pub fn sysex_dense(tracks: usize, count: usize, payload_len: usize) -> Vec<u8> {
    let mut bodies = Vec::with_capacity(tracks);
    let payload_len = payload_len.max(2);
    for tr in 0..tracks {
        let ch = (tr % 16) as u8;
        let mut t = Vec::with_capacity(count * (payload_len + 8) + 64);
        ev_meta(&mut t, 0, 0x03, format!("gen-sysex-{tr}").as_bytes());
        for i in 0..count {
            // 载荷：厂商 ID 0x7D（教育/非商业）+ 递增填充 + 结尾 0xF7。
            let mut payload = vec![0x7Du8; payload_len];
            payload[0] = 0x7D;
            payload[payload_len - 2] = (i % 128) as u8;
            payload[payload_len - 1] = 0xF7;
            ev_sysex(&mut t, 0, &payload);
            // 每 10 条插入一个 F7 Escape，覆盖「F7 并入 sys_ex」路径。
            if i % 10 == 0 {
                ev_escape(&mut t, 0, &[0x7D, 0x01, 0x02, 0xF7]);
            }
            // 每 20 条 SysEx 插入一对音符，保证音符/控制路径也有覆盖。
            if i % 20 == 0 {
                ev_note_on(&mut t, 1, ch, 60, 100);
                ev_note_off(&mut t, 24, ch, 60, 0);
            }
        }
        ev_meta(&mut t, 0, 0x2F, &[]);
        bodies.push(t);
    }
    smf(1, 480, bodies)
}

// ── SMF 写入原语 ────────────────────────────────────────────────────────────

/// MThd + N × MTrk 组装。
fn smf(format: u16, division: u16, bodies: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::with_capacity(bodies.iter().map(|b| b.len() + 8).sum::<usize>() + 14);
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&format.to_be_bytes());
    out.extend_from_slice(&(bodies.len() as u16).to_be_bytes());
    out.extend_from_slice(&division.to_be_bytes());
    for body in &bodies {
        out.extend_from_slice(b"MTrk");
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
    }
    out
}

/// 变长量（VLQ）写入。
fn push_vlq(out: &mut Vec<u8>, mut value: u32) {
    let mut buf = [0u8; 5];
    let mut n = 0;
    loop {
        buf[n] = (value & 0x7F) as u8;
        value >>= 7;
        n += 1;
        if value == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        let mut byte = buf[i];
        if i != 0 {
            byte |= 0x80;
        }
        out.push(byte);
    }
}

fn ev_note_on(t: &mut Vec<u8>, delta: u32, ch: u8, key: u8, vel: u8) {
    push_vlq(t, delta);
    t.extend_from_slice(&[0x90 | (ch & 0x0F), key, vel]);
}

fn ev_note_off(t: &mut Vec<u8>, delta: u32, ch: u8, key: u8, vel: u8) {
    push_vlq(t, delta);
    t.extend_from_slice(&[0x80 | (ch & 0x0F), key, vel]);
}

fn ev_cc(t: &mut Vec<u8>, delta: u32, ch: u8, controller: u8, value: u8) {
    push_vlq(t, delta);
    t.extend_from_slice(&[0xB0 | (ch & 0x0F), controller, value]);
}

fn ev_pitch_bend(t: &mut Vec<u8>, delta: u32, ch: u8, raw14: u16) {
    push_vlq(t, delta);
    let raw = raw14 & 0x3FFF;
    t.extend_from_slice(&[0xE0 | (ch & 0x0F), (raw & 0x7F) as u8, (raw >> 7) as u8]);
}

fn ev_channel_at(t: &mut Vec<u8>, delta: u32, ch: u8, value: u8) {
    push_vlq(t, delta);
    t.extend_from_slice(&[0xD0 | (ch & 0x0F), value]);
}

fn ev_poly_at(t: &mut Vec<u8>, delta: u32, ch: u8, key: u8, value: u8) {
    push_vlq(t, delta);
    t.extend_from_slice(&[0xA0 | (ch & 0x0F), key, value]);
}

fn ev_meta(t: &mut Vec<u8>, delta: u32, meta_type: u8, data: &[u8]) {
    push_vlq(t, delta);
    t.extend_from_slice(&[0xFF, meta_type]);
    push_vlq(t, data.len() as u32);
    t.extend_from_slice(data);
}

fn ev_sysex(t: &mut Vec<u8>, delta: u32, data: &[u8]) {
    push_vlq(t, delta);
    t.push(0xF0);
    push_vlq(t, data.len() as u32);
    t.extend_from_slice(data);
}

fn ev_escape(t: &mut Vec<u8>, delta: u32, data: &[u8]) {
    push_vlq(t, delta);
    t.push(0xF7);
    push_vlq(t, data.len() as u32);
    t.extend_from_slice(data);
}

#[cfg(test)]
mod tests {
    use super::*;
    use midly::Smf;
    use midly::loader::extract_all_events_per_track_collect_from_bytes;

    /// 每轨摘要：(音符, 控制, SysEx, 文本类, port)。
    type Digest = (usize, usize, usize, usize, Option<u8>);

    fn digest(bytes: &[u8]) -> Vec<Digest> {
        extract_all_events_per_track_collect_from_bytes(bytes, |_idx, ev| {
            (
                ev.notes.len(),
                ev.control_events.len(),
                ev.sys_ex.len(),
                ev.lyrics.len() + ev.markers.len() + ev.text_events.len(),
                ev.midi_port,
            )
        })
        .expect("生成素材必须可被快路径解析")
    }

    /// 生成素材必须同时通过参考链（Smf::parse）与快路径，且逐轨计数符合规格。
    #[test]
    fn generated_materials_are_valid_and_match_spec() {
        // 单轨小文件
        let bytes = single_track_small();
        let smf = Smf::parse(&bytes).expect("single-small 必须可被 Smf::parse");
        assert_eq!(smf.tracks.len(), 1);
        assert_eq!(digest(&bytes), vec![(120, 0, 0, 0, None)]);

        // 单巨轨
        let bytes = single_giant_track(GIANT_NOTES);
        let smf = Smf::parse(&bytes).expect("single-giant 必须可被 Smf::parse");
        assert_eq!(smf.tracks.len(), 1);
        assert_eq!(digest(&bytes), vec![(GIANT_NOTES as usize, 0, 0, 0, None)]);

        // 控制密集
        let bytes = control_dense(CONTROL_TRACKS, CONTROL_NOTES_PER_TRACK);
        let smf = Smf::parse(&bytes).expect("control-dense 必须可被 Smf::parse");
        assert_eq!(smf.tracks.len(), CONTROL_TRACKS);
        let per_track = CONTROL_NOTES_PER_TRACK as usize;
        for d in digest(&bytes) {
            assert_eq!(d.0, per_track, "控制密集每轨音符数");
            // 每个音符 4 CC + 1 PB + 1 ChannelAT + 1 PolyAT
            assert_eq!(d.1, per_track * 7, "控制密集每轨控制事件数（7:1）");
        }

        // SysEx 密集
        let bytes = sysex_dense(SYSEX_TRACKS, SYSEX_PER_TRACK, SYSEX_PAYLOAD);
        let smf = Smf::parse(&bytes).expect("sysex-dense 必须可被 Smf::parse");
        assert_eq!(smf.tracks.len(), SYSEX_TRACKS);
        for d in digest(&bytes) {
            let notes = SYSEX_PER_TRACK.div_ceil(20);
            // 每条 Sysex + 每 10 条一个 Escape
            let expected_sysex = SYSEX_PER_TRACK + SYSEX_PER_TRACK.div_ceil(10);
            assert_eq!(d.0, notes, "SysEx 密集每轨音符数");
            assert_eq!(
                d.2, expected_sysex,
                "SysEx 密集每轨 SysEx 条数（含 Escape）"
            );
        }
    }

    /// 生成器确定性：同参数两次生成必须字节一致（素材可复现）。
    #[test]
    fn generators_are_deterministic() {
        assert_eq!(single_track_small(), single_track_small());
        assert_eq!(
            control_dense(CONTROL_TRACKS, CONTROL_NOTES_PER_TRACK),
            control_dense(CONTROL_TRACKS, CONTROL_NOTES_PER_TRACK)
        );
        assert_eq!(
            sysex_dense(SYSEX_TRACKS, SYSEX_PER_TRACK, SYSEX_PAYLOAD),
            sysex_dense(SYSEX_TRACKS, SYSEX_PER_TRACK, SYSEX_PAYLOAD)
        );
    }
}
