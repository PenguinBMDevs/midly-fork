//! 逐轨比较器：参考期望值 vs 快路径 `TrackAllEvents`。
//! 顺序敏感（音符按 NoteOff 到达序、控制事件按文件序），全字段比较。
//!
//! 注意：Packed* 结构为 `#[repr(C, packed)]`，禁止直接对字段取引用
//! （assert_eq! 内部会取引用）——一律先按值拷贝到对齐局部变量再比较。

use midly::loader::TrackAllEvents;

use super::reference::{RefControl, RefNote, RefTrack};

/// 单轨全量断言。任何差异都带轨道索引与上下文 panic，便于定位。
pub fn assert_track_eq(expected: &RefTrack, actual: &TrackAllEvents, name: &str, idx: usize) {
    let ctx = format!("{name} track#{idx}");

    // ── 音符：数量 + 逐条全字段（到达顺序敏感）──
    assert_eq!(
        expected.notes.len(),
        actual.notes.len(),
        "{ctx}: 音符数量不一致"
    );
    for (i, (exp, act)) in expected.notes.iter().zip(actual.notes.iter()).enumerate() {
        let act = *act; // 拷贝到局部；字段一律按值取出，禁止对 packed 字段取引用
        let got = RefNote {
            start_tick: act.start_tick,
            end_tick: act.end_tick,
            key: act.key,
            velocity: act.velocity,
            release_velocity: act.release_velocity,
            channel: act.channel,
        };
        let got_track = act.track;
        assert_eq!(*exp, got, "{ctx}: 音符#{i} 字段不一致（顺序敏感）");
        assert_eq!(got_track, idx as u16, "{ctx}: 音符#{i} track 索引错误");
    }

    // ── 控制事件：文件序敏感 ──
    assert_eq!(
        expected.controls.len(),
        actual.control_events.len(),
        "{ctx}: 控制事件数量不一致"
    );
    for (i, (exp, act)) in expected
        .controls
        .iter()
        .zip(actual.control_events.iter())
        .enumerate()
    {
        let act = *act;
        let got = RefControl {
            tick: act.tick,
            kind: act.kind,
            channel: act.channel,
            param: act.param,
        };
        let got_track = act.track;
        assert_eq!(*exp, got, "{ctx}: 控制事件#{i} 不一致（文件序敏感）");
        assert_eq!(got_track, idx as u16, "{ctx}: 控制事件#{i} track 索引错误");
    }

    // ── Tempo：tick + bpm 位级一致（两侧同为 60e6/us 的 f32 除法）──
    assert_eq!(
        expected.tempo.len(),
        actual.tempo_changes.len(),
        "{ctx}: tempo 数量不一致"
    );
    for (i, ((et, eb), (at, ab))) in expected
        .tempo
        .iter()
        .zip(actual.tempo_changes.iter())
        .enumerate()
    {
        assert_eq!(et, at, "{ctx}: tempo#{i} tick 不一致");
        assert_eq!(eb.to_bits(), ab.to_bits(), "{ctx}: tempo#{i} bpm 不一致");
    }

    // ── 拍号 / 调号 ──
    assert_eq!(
        expected.time_sigs.len(),
        actual.time_signatures.len(),
        "{ctx}: 拍号数量不一致"
    );
    for (i, (exp, act)) in expected
        .time_sigs
        .iter()
        .zip(actual.time_signatures.iter())
        .enumerate()
    {
        let act = *act;
        assert_eq!(
            (exp.0, exp.1, exp.2),
            (act.tick, act.numerator, act.denominator),
            "{ctx}: 拍号#{i} 不一致"
        );
    }
    assert_eq!(
        expected.key_sigs.len(),
        actual.key_signatures.len(),
        "{ctx}: 调号数量不一致"
    );
    for (i, (exp, act)) in expected
        .key_sigs
        .iter()
        .zip(actual.key_signatures.iter())
        .enumerate()
    {
        let act = *act;
        assert_eq!(
            (exp.0, exp.1, exp.2),
            (act.tick, act.sharps, act.is_minor),
            "{ctx}: 调号#{i} 不一致"
        );
    }

    // ── 歌词 / 标记 / 文本类 meta：tick + 内容摘要 ──
    assert_eq!(
        expected.lyrics.len(),
        actual.lyrics.len(),
        "{ctx}: 歌词数量不一致"
    );
    for (i, ((et, ep), act)) in expected.lyrics.iter().zip(actual.lyrics.iter()).enumerate() {
        let a = *act;
        let (at, atext, atrack) = (a.tick, a.text, a.track);
        assert_eq!(*et, at, "{ctx}: 歌词#{i} tick 不一致");
        assert_eq!(ep.as_slice(), atext, "{ctx}: 歌词#{i} 内容不一致");
        assert_eq!(atrack, idx as u16, "{ctx}: 歌词#{i} track 索引错误");
    }
    assert_eq!(
        expected.markers.len(),
        actual.markers.len(),
        "{ctx}: 标记数量不一致"
    );
    for (i, ((et, ep), act)) in expected
        .markers
        .iter()
        .zip(actual.markers.iter())
        .enumerate()
    {
        let a = *act;
        let (at, atext, atrack) = (a.tick, a.text, a.track);
        assert_eq!(*et, at, "{ctx}: 标记#{i} tick 不一致");
        assert_eq!(ep.as_slice(), atext, "{ctx}: 标记#{i} 内容不一致");
        assert_eq!(atrack, idx as u16, "{ctx}: 标记#{i} track 索引错误");
    }
    assert_eq!(
        expected.texts.len(),
        actual.text_events.len(),
        "{ctx}: 文本 meta 数量不一致"
    );
    for (i, ((et, em, ep), act)) in expected
        .texts
        .iter()
        .zip(actual.text_events.iter())
        .enumerate()
    {
        let a = *act;
        let (at, am, atext, atrack) = (a.tick, a.meta_type, a.text, a.track);
        assert_eq!(*et, at, "{ctx}: 文本#{i} tick 不一致");
        assert_eq!(*em, am, "{ctx}: 文本#{i} meta_type 不一致");
        assert_eq!(ep.as_slice(), atext, "{ctx}: 文本#{i} 内容不一致");
        assert_eq!(atrack, idx as u16, "{ctx}: 文本#{i} track 索引错误");
    }

    // ── SysEx（含 F7 Escape 并入）：tick + 原始字节 ──
    assert_eq!(
        expected.sysex.len(),
        actual.sys_ex.len(),
        "{ctx}: SysEx 数量不一致"
    );
    for (i, ((et, ep), act)) in expected.sysex.iter().zip(actual.sys_ex.iter()).enumerate() {
        let a = *act;
        let (at, adata, atrack) = (a.tick, a.data, a.track);
        assert_eq!(*et, at, "{ctx}: SysEx#{i} tick 不一致");
        assert_eq!(ep.as_slice(), adata, "{ctx}: SysEx#{i} 内容不一致");
        assert_eq!(atrack, idx as u16, "{ctx}: SysEx#{i} track 索引错误");
    }

    // ── MIDI Port：首个出现值 ──
    assert_eq!(
        expected.midi_port, actual.midi_port,
        "{ctx}: midi_port 不一致"
    );

    // ── EOT 结构元数据（指导书 §2.5）：三个时间 + 畸形尾部诊断 ──
    assert_eq!(
        expected.timing, actual.timing,
        "{ctx}: TrackTiming 不一致（eot/content/physical/after_eot）"
    );
}
