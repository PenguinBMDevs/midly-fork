//! EOT（End of Track）解析规格测试：逐项对应指导书 `§2.9` 十项必测用例。
//!
//! 语义基线（默认**兼容模式**，产品决策记录于 HANDOFF）：
//! - EOT 是轨道级结构元数据，不进入音频事件流，但保留 `TrackTiming`；
//! - 兼容模式：EOT 后事件照常解析（**不得静默丢数据**），以
//!   `has_events_after_eot` 标记；Strict 判定见 `TrackTiming::is_strict_clean()`，
//!   由调用方决定是否拒绝（解析器不替产品做策略）；
//! - 未闭合音符按既有语义在物理末尾收尾（EOT 为最后事件时即 EOT tick）；
//! - 缺失 EOT 时 `eot_tick == None`，`declared_end_tick()` 退化为物理末尾。
//!
//! parallel/serial 对拍：本文件在两种 feature 组合下各跑一次（HANDOFF §6），
//! 第 10 项用例在单次运行内对拍 collect / 并行回调两条路径。

use std::sync::Mutex;

use midly::loader::{
    TrackAllEvents, TrackTiming, extract_all_events_per_track_collect_from_bytes,
    extract_all_events_per_track_parallel_from_bytes,
};

// ── 最小 SMF 构造器（测试自用，不依赖 perf crate）────────────────────────────

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

#[derive(Default)]
struct TrackBuilder {
    body: Vec<u8>,
}

impl TrackBuilder {
    fn new() -> Self {
        Self::default()
    }

    fn raw(&mut self, delta: u32, bytes: &[u8]) -> &mut Self {
        push_vlq(&mut self.body, delta);
        self.body.extend_from_slice(bytes);
        self
    }

    fn note_on(&mut self, delta: u32, ch: u8, key: u8, vel: u8) -> &mut Self {
        self.raw(delta, &[0x90 | (ch & 0x0F), key, vel])
    }

    fn note_off(&mut self, delta: u32, ch: u8, key: u8, vel: u8) -> &mut Self {
        self.raw(delta, &[0x80 | (ch & 0x0F), key, vel])
    }

    fn cc(&mut self, delta: u32, ch: u8, controller: u8, value: u8) -> &mut Self {
        self.raw(delta, &[0xB0 | (ch & 0x0F), controller, value])
    }

    fn meta(&mut self, delta: u32, meta_type: u8, data: &[u8]) -> &mut Self {
        push_vlq(&mut self.body, delta);
        self.body.extend_from_slice(&[0xFF, meta_type]);
        push_vlq(&mut self.body, data.len() as u32);
        self.body.extend_from_slice(data);
        self
    }

    fn eot(&mut self, delta: u32) -> &mut Self {
        self.meta(delta, 0x2F, &[])
    }

    fn sysex(&mut self, delta: u32, data: &[u8]) -> &mut Self {
        push_vlq(&mut self.body, delta);
        self.body.push(0xF0);
        push_vlq(&mut self.body, data.len() as u32);
        self.body.extend_from_slice(data);
        self
    }
}

fn smf(tracks: Vec<TrackBuilder>) -> Vec<u8> {
    let format: u16 = if tracks.len() == 1 { 0 } else { 1 };
    let mut out = Vec::new();
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&format.to_be_bytes());
    out.extend_from_slice(&(tracks.len() as u16).to_be_bytes());
    out.extend_from_slice(&480u16.to_be_bytes());
    for t in &tracks {
        out.extend_from_slice(b"MTrk");
        out.extend_from_slice(&(t.body.len() as u32).to_be_bytes());
        out.extend_from_slice(&t.body);
    }
    out
}

// ── 快路径快照（拥有所有权，便于断言）──────────────────────────────────────

#[derive(Debug, Default, PartialEq)]
struct Snap {
    timing: TrackTiming,
    /// (start, end, key, velocity, release_velocity)
    notes: Vec<(u32, u32, u8, u8, u8)>,
    /// (tick, kind, param)
    controls: Vec<(u32, u8, u16)>,
    lyrics: Vec<(u32, Vec<u8>)>,
    texts: Vec<(u32, u8, Vec<u8>)>,
    sysex: Vec<(u32, Vec<u8>)>,
}

fn snapshot(ev: &TrackAllEvents) -> Snap {
    let mut snap = Snap {
        timing: ev.timing,
        ..Snap::default()
    };
    for n in &ev.notes {
        let n = *n; // packed：先按值拷贝，禁止对字段取引用
        snap.notes.push((
            n.start_tick,
            n.end_tick,
            n.key,
            n.velocity,
            n.release_velocity,
        ));
    }
    for c in &ev.control_events {
        let c = *c;
        snap.controls.push((c.tick, c.kind, c.param));
    }
    for t in &ev.lyrics {
        let t = *t;
        snap.lyrics.push((t.tick, t.text.to_vec()));
    }
    for t in &ev.text_events {
        let t = *t;
        snap.texts.push((t.tick, t.meta_type, t.text.to_vec()));
    }
    for s in &ev.sys_ex {
        let s = *s;
        snap.sysex.push((s.tick, s.data.to_vec()));
    }
    snap
}

fn run(bytes: &[u8]) -> Vec<Snap> {
    extract_all_events_per_track_collect_from_bytes(bytes, |_idx, ev| snapshot(&ev))
        .expect("EOT 规格用例必须可解析")
}

// ── §2.9 十项必测用例 ───────────────────────────────────────────────────────

/// 1. NoteOn → EOT：未闭合音符在 EOT 正确收尾（release 0）。
#[test]
fn test_eot_closes_unclosed_note() {
    let mut t = TrackBuilder::new();
    t.note_on(0, 0, 60, 100).eot(480);
    let snaps = run(&smf(vec![t]));
    assert_eq!(snaps[0].notes, vec![(0, 480, 60, 100, 0)]);
    assert_eq!(snaps[0].timing.eot_tick, Some(480));
    assert_eq!(snaps[0].timing.physical_end_tick, 480);
    assert!(!snaps[0].timing.has_events_after_eot);
    assert!(snaps[0].timing.is_strict_clean());
}

/// 2. NoteOn → NoteOff → 超晚 EOT：音符长度不被拉长，声明范围与内容范围分离。
#[test]
fn test_late_eot_does_not_stretch_note() {
    let mut t = TrackBuilder::new();
    t.note_on(0, 0, 64, 90)
        .note_off(240, 0, 64, 32)
        .eot(4800 - 240);
    let snaps = run(&smf(vec![t]));
    assert_eq!(snaps[0].notes, vec![(0, 240, 64, 90, 32)]);
    let tm = snaps[0].timing;
    assert_eq!(tm.eot_tick, Some(4800), "EOT tick 是声明范围");
    assert_eq!(tm.content_end_tick, 240, "内容范围以最后非 EOT 事件为准");
    assert_eq!(tm.physical_end_tick, 4800);
    assert_eq!(tm.declared_end_tick(), 4800);
}

/// 3. 多轨不同 EOT：文件声明结尾取最大值，轨道状态互不串扰（同 channel/key 也不串）。
#[test]
fn test_multi_track_eot_max_and_isolation() {
    let mut t0 = TrackBuilder::new();
    t0.note_on(0, 0, 60, 100).note_off(400, 0, 60, 0).eot(80);
    let mut t1 = TrackBuilder::new();
    t1.note_on(0, 0, 60, 80).note_off(900, 0, 60, 0).eot(60);
    let snaps = run(&smf(vec![t0, t1]));
    assert_eq!(snaps.len(), 2);
    assert_eq!(snaps[0].timing.eot_tick, Some(480));
    assert_eq!(snaps[1].timing.eot_tick, Some(960));
    let declared_max = snaps
        .iter()
        .map(|s| s.timing.declared_end_tick())
        .max()
        .unwrap_or(0);
    assert_eq!(declared_max, 960, "文件声明结尾 = max(各轨 EOT)");
    // 轨道间状态不串扰：两轨音符各自独立配对。
    assert_eq!(snaps[0].notes, vec![(0, 400, 60, 100, 0)]);
    assert_eq!(snaps[1].notes, vec![(0, 900, 60, 80, 0)]);
}

/// 4. 无 EOT 且有未闭合音符：`eot_tick == None`，fallback 为物理末尾。
#[test]
fn test_missing_eot_falls_back_to_physical_end() {
    let mut t = TrackBuilder::new();
    t.note_on(0, 0, 55, 70)
        .note_off(120, 0, 55, 0)
        .note_on(120, 0, 57, 60);
    let snaps = run(&smf(vec![t]));
    let tm = snaps[0].timing;
    assert_eq!(tm.eot_tick, None, "缺失 EOT 必须可诊断");
    assert_eq!(tm.physical_end_tick, 240);
    assert_eq!(tm.declared_end_tick(), 240, "fallback = 物理末尾");
    assert_eq!(snaps[0].notes[1], (240, 240, 57, 60, 0));
}

/// 5. EOT 后有 NoteOff：兼容模式保留并完成配对；Strict 判定为不干净。
#[test]
fn test_events_after_eot_preserved_and_strict_flagged() {
    let mut t = TrackBuilder::new();
    t.note_on(0, 0, 60, 100).eot(400).note_off(80, 0, 60, 55);
    let snaps = run(&smf(vec![t]));
    assert_eq!(
        snaps[0].notes,
        vec![(0, 480, 60, 100, 55)],
        "EOT 后的 NoteOff 必须完成配对（不得静默丢数据）"
    );
    let tm = snaps[0].timing;
    assert_eq!(tm.eot_tick, Some(400));
    assert_eq!(tm.content_end_tick, 480);
    assert_eq!(tm.physical_end_tick, 480);
    assert!(tm.has_events_after_eot, "兼容模式必须留下诊断标记");
    assert!(!tm.is_strict_clean(), "Strict 调用方据此拒绝");
}

/// 6. EOT 时 Hold（CC64）未释放：控制事件原样保留，供播放/导出收尾。
#[test]
fn test_hold_at_eot_is_preserved() {
    let mut t = TrackBuilder::new();
    t.cc(0, 0, 64, 127).note_on(10, 0, 60, 100).eot(480 - 10);
    let snaps = run(&smf(vec![t]));
    assert_eq!(
        snaps[0].controls,
        vec![(0, 0, ((64u16) << 8) | 127)],
        "CC64 必须保留在控制事件流"
    );
    assert_eq!(snaps[0].notes, vec![(10, 480, 60, 100, 0)]);
    assert_eq!(snaps[0].timing.eot_tick, Some(480));
}

/// 7. 空轨只有 EOT：不 panic、不生成音符，时间边界正确。
#[test]
fn test_empty_track_with_only_eot() {
    let mut t = TrackBuilder::new();
    t.eot(0);
    let snaps = run(&smf(vec![t]));
    assert_eq!(snaps.len(), 1);
    assert!(snaps[0].notes.is_empty());
    assert!(snaps[0].controls.is_empty());
    assert_eq!(snaps[0].timing.eot_tick, Some(0));
    assert_eq!(snaps[0].timing.physical_end_tick, 0);
    assert_eq!(snaps[0].timing.content_end_tick, 0);
    assert!(!snaps[0].timing.has_events_after_eot);
}

/// 8. 重复 EOT：按畸形输入处理，保留首个 EOT 并留下诊断。
#[test]
fn test_duplicate_eot_is_diagnosed() {
    let mut t = TrackBuilder::new();
    t.note_on(0, 0, 60, 100)
        .note_off(100, 0, 60, 0)
        .eot(0)
        .eot(100);
    let snaps = run(&smf(vec![t]));
    let tm = snaps[0].timing;
    assert_eq!(tm.eot_tick, Some(100), "保留首个 EOT");
    assert_eq!(tm.physical_end_tick, 200);
    assert!(tm.has_events_after_eot, "第二个 EOT 也算 EOT 后事件");
    assert_eq!(tm.content_end_tick, 100);
}

/// 9. EOT 后还有文本/SysEx：不得静默丢失（含 tick 正确）。
#[test]
fn test_text_and_sysex_after_eot_not_dropped() {
    let mut t = TrackBuilder::new();
    t.eot(100)
        .meta(50, 0x01, b"after-eot")
        .sysex(20, &[0x7D, 0x01, 0xF7]);
    let snaps = run(&smf(vec![t]));
    assert_eq!(snaps[0].texts, vec![(150, 0x01, b"after-eot".to_vec())]);
    assert_eq!(snaps[0].sysex, vec![(170, vec![0x7D, 0x01, 0xF7])]);
    assert_eq!(snaps[0].timing.content_end_tick, 170);
    assert!(snaps[0].timing.has_events_after_eot);
}

/// 10. parallel/serial 的 EOT 元数据与最终事件内容一致：
/// 同一次运行内对拍 collect 与并行回调两条路径，并校验时间边界不变式。
#[test]
fn test_eot_metadata_path_and_content_consistent() {
    let mut t0 = TrackBuilder::new();
    t0.note_on(0, 0, 60, 100)
        .note_off(200, 0, 60, 0)
        .eot(280)
        .note_on(20, 0, 62, 90);
    let mut t1 = TrackBuilder::new();
    t1.note_on(0, 1, 40, 80).eot(960);
    let bytes = smf(vec![t0, t1]);

    let collected = run(&bytes);
    assert_eq!(collected, run(&bytes), "同一路径两次解析必须一致");

    let slots: Vec<Mutex<Option<Snap>>> = (0..collected.len()).map(|_| Mutex::new(None)).collect();
    extract_all_events_per_track_parallel_from_bytes(&bytes, |idx, ev| {
        *slots[idx].lock().unwrap_or_else(|e| e.into_inner()) = Some(snapshot(&ev));
    })
    .expect("并行回调路径不得返回错误");
    let parallel: Vec<Snap> = slots
        .into_iter()
        .map(|m| {
            m.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .expect("每轨恰好一次")
        })
        .collect();
    assert_eq!(collected, parallel, "collect 与并行回调路径必须一致");

    for (i, snap) in collected.iter().enumerate() {
        let tm = snap.timing;
        assert!(
            tm.content_end_tick <= tm.physical_end_tick,
            "轨#{i}: 内容范围不得超过物理末尾"
        );
        if let Some(eot) = tm.eot_tick {
            assert!(eot <= tm.physical_end_tick, "轨#{i}: EOT 不得晚于物理末尾");
        }
        let max_event_tick = snap
            .notes
            .iter()
            .map(|n| n.0)
            .chain(snap.controls.iter().map(|c| c.0))
            .chain(snap.texts.iter().map(|t| t.0))
            .chain(snap.sysex.iter().map(|s| s.0))
            .max()
            .unwrap_or(0);
        assert!(
            max_event_tick <= tm.content_end_tick,
            "轨#{i}: 保留事件不得晚于 content_end_tick"
        );
    }
    // EOT 后的事件保留但被标记。
    assert!(collected[0].timing.has_events_after_eot);
    assert!(!collected[1].timing.has_events_after_eot);
}
