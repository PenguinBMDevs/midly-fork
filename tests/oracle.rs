//! P1 正确性 Oracle：快路径（字节流式提取）vs `Smf::parse` 参考链逐轨对拍。
//!
//! 指导书 `docs/research/lumino-midly-performance-guide.md` §P1：
//! 不只比较总事件数；比较类型、tick、通道、参数、顺序和内容摘要。
//! 参考累加器在 `reference.rs` 中**独立实现**（不复用 NoteAccumulator），
//! 避免共享配对逻辑掩盖共同 bug。
//!
//! 运行方式（parallel / serial 双构建对拍）：
//! - `cargo test --test oracle --features memmap,strict`
//! - `cargo test --test oracle --no-default-features --features std,alloc,memmap,strict`

#![cfg(all(feature = "alloc", feature = "std"))]

#[path = "oracle/compare.rs"]
mod compare;
#[path = "oracle/reference.rs"]
mod reference;
#[path = "oracle/synthetic.rs"]
mod synthetic;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use midly::Smf;
use midly::loader::{
    TrackAllEvents, extract_all_events_per_track_collect_from_bytes,
    extract_all_events_per_track_parallel_from_bytes,
    extract_all_events_per_track_streaming_from_bytes,
};

/// 对一份 MIDI 字节执行完整对拍：
/// 1. `Smf::parse` 成功 → 独立参考累加器逐轨产出期望值；
/// 2. 快路径回调内逐轨比较（顺序敏感，含同 tick 文件序）；
/// 3. 每轨回调恰好一次，轨道索引完整。
pub fn run_oracle(bytes: &[u8], name: &str) {
    let smf = Smf::parse(bytes).expect("参考链 Smf::parse 必须成功（合成/有效 fixture）");
    let expected: Vec<reference::RefTrack> = smf
        .tracks
        .iter()
        .enumerate()
        .map(|(idx, track)| reference::accumulate_track(track, idx))
        .collect();

    let seen = AtomicUsize::new(0);
    let result = extract_all_events_per_track_streaming_from_bytes(bytes, |idx, events| {
        assert!(
            idx < expected.len(),
            "{name}: 快路径轨索引 {idx} 超出参考轨数 {}",
            expected.len()
        );
        compare::assert_track_eq(&expected[idx], &events, name, idx);
        seen.fetch_add(1, Ordering::Relaxed);
    });
    result.expect("Smf::parse 成功时快路径不得返回错误");
    assert_eq!(
        seen.load(Ordering::Relaxed),
        expected.len(),
        "{name}: 每轨回调必须恰好一次（期望 {} 轨）",
        expected.len()
    );
}

/// test-asset 下全部 .mid/.rmi，按路径排序（固定遍历顺序）。
fn asset_paths() -> Vec<PathBuf> {
    let mut entries: Vec<_> = fs::read_dir("test-asset")
        .expect("test-asset 目录必须存在（P0 已恢复 fixture）")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .map(|ext| ext.eq_ignore_ascii_case("mid") || ext.eq_ignore_ascii_case("rmi"))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();
    assert!(entries.len() >= 10, "fixture 数量不足，P0 资产恢复可能失效");
    entries
}

/// 遍历 test-asset 全部 .mid/.rmi：
/// - 参考链可解析 → 完整对拍；
/// - 参考链拒绝（畸形文件）→ 快路径不得 panic，且两次运行结果逐轨一致（确定性）。
#[test]
fn test_oracle_all_assets() {
    let entries = asset_paths();

    let mut full = 0;
    let mut malformed = 0;
    for path in &entries {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let bytes = fs::read(path).expect("fixture 必须可读");
        match Smf::parse(&bytes) {
            Ok(_) => {
                run_oracle(&bytes, &name);
                full += 1;
                eprintln!("[oracle] {name}: 完整对拍通过");
            }
            Err(err) => {
                // 畸形输入：参考链不可用，锁定快路径的确定性与无 panic。
                let a = fast_digest(&bytes);
                let b = fast_digest(&bytes);
                assert_eq!(a, b, "{name}: 畸形输入两次解析结果必须一致");
                malformed += 1;
                eprintln!("[oracle] {name}: 参考链拒绝（{err:?}），快路径确定性通过");
            }
        }
    }
    eprintln!("[oracle] 汇总：完整对拍 {full} 个，畸形确定性 {malformed} 个");
    assert!(full >= 8, "至少 8 个 fixture 应走完整对拍");
}

/// 畸形文件单轨摘要：(音符数, 控制数, SysEx数, 文本数, 最小start, 最大end, port)。
type TrackDigest = (usize, usize, usize, usize, u32, u32, Option<u8>);

/// 快路径单轨摘要（计数 + 边界 tick + port）。
fn digest_of(ev: &TrackAllEvents) -> TrackDigest {
    let max_end = ev.notes.iter().map(|n| n.end_tick).max().unwrap_or(0);
    let min_start = ev.notes.iter().map(|n| n.start_tick).min().unwrap_or(0);
    (
        ev.notes.len(),
        ev.control_events.len(),
        ev.sys_ex.len(),
        ev.text_events.len() + ev.lyrics.len() + ev.markers.len(),
        min_start,
        max_end,
        ev.midi_port,
    )
}

/// 参考期望值的同口径摘要（用于 collect API 的按轨序对拍）。
fn expected_digest(rt: &reference::RefTrack) -> TrackDigest {
    (
        rt.notes.len(),
        rt.controls.len(),
        rt.sysex.len(),
        rt.texts.len() + rt.lyrics.len() + rt.markers.len(),
        rt.notes.iter().map(|n| n.start_tick).min().unwrap_or(0),
        rt.notes.iter().map(|n| n.end_tick).max().unwrap_or(0),
        rt.midi_port,
    )
}

/// 畸形文件的快路径摘要（每轨计数 + 边界 tick + port），用于确定性比较。
fn fast_digest(bytes: &[u8]) -> Vec<TrackDigest> {
    let mut slots: Vec<Option<TrackDigest>> = Vec::new();
    extract_all_events_per_track_streaming_from_bytes(bytes, |idx, ev| {
        if slots.len() <= idx {
            slots.resize_with(idx + 1, || None);
        }
        slots[idx] = Some(digest_of(&ev));
    })
    .expect("畸形输入快路径不得返回错误（宽松解析）");
    slots
        .into_iter()
        .map(|s| s.unwrap_or((0, 0, 0, 0, 0, 0, None)))
        .collect()
}

/// P3 无锁并行 API 对拍（指导书 §P3 验收门禁）：
/// - `_parallel_`：`Fn + Sync` 回调由工作线程并发调用，逐轨全字段对拍，
///   并断言**每轨恰好一次**（不是"至少一次"，也不是"总次数相等"）；
/// - `_collect_`：并行 map 后按轨序返回，`results[i]` 必须来自第 i 轨；
/// - 畸形输入：旧 `FnMut` 回调 API 与 collect API 的逐轨摘要必须完全一致。
#[test]
fn test_oracle_parallel_apis() {
    let mut checked = 0;
    for path in &asset_paths() {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let bytes = fs::read(path).expect("fixture 必须可读");
        match Smf::parse(&bytes) {
            Ok(smf) => {
                let expected: Vec<reference::RefTrack> = smf
                    .tracks
                    .iter()
                    .enumerate()
                    .map(|(idx, track)| reference::accumulate_track(track, idx))
                    .collect();

                // 1) 无锁并行回调：并发执行完整全字段比较。
                let seen: Vec<AtomicU8> = (0..expected.len()).map(|_| AtomicU8::new(0)).collect();
                extract_all_events_per_track_parallel_from_bytes(&bytes, |idx, events| {
                    assert!(
                        idx < expected.len(),
                        "{name}: 并行回调轨索引 {idx} 超出参考轨数 {}",
                        expected.len()
                    );
                    compare::assert_track_eq(&expected[idx], &events, &name, idx);
                    seen[idx].fetch_add(1, Ordering::Relaxed);
                })
                .expect("Smf::parse 成功时并行回调 API 不得返回错误");
                for (i, s) in seen.iter().enumerate() {
                    assert_eq!(
                        s.load(Ordering::Relaxed),
                        1,
                        "{name}: 轨#{i} 并行回调必须恰好一次"
                    );
                }

                // 2) collect：返回值必须按轨序、逐轨摘要与参考一致。
                let got = extract_all_events_per_track_collect_from_bytes(&bytes, |idx, ev| {
                    (idx, digest_of(&ev))
                })
                .expect("Smf::parse 成功时 collect API 不得返回错误");
                assert_eq!(
                    got.len(),
                    expected.len(),
                    "{name}: collect 返回轨数必须等于参考轨数"
                );
                for (i, (idx, digest)) in got.iter().enumerate() {
                    assert_eq!(*idx, i, "{name}: collect 结果必须按轨序返回（位置 {i}）");
                    assert_eq!(
                        *digest,
                        expected_digest(&expected[i]),
                        "{name}: 轨#{i} collect 摘要与参考不一致"
                    );
                }
                checked += 1;
            }
            Err(_) => {
                // 畸形输入：两条路径的宽松解析结果必须逐轨一致。
                let legacy = fast_digest(&bytes);
                let par: Vec<TrackDigest> =
                    extract_all_events_per_track_collect_from_bytes(&bytes, |_, ev| digest_of(&ev))
                        .expect("畸形输入 collect 不得返回错误（宽松解析）");
                assert_eq!(
                    legacy, par,
                    "{name}: 旧回调 API 与 collect API 的畸形输入摘要必须一致"
                );
            }
        }
    }
    assert!(checked >= 8, "至少 8 个 fixture 应走并行 API 完整对拍");
    eprintln!("[oracle] 并行 API 汇总：完整对拍 {checked} 个");
}
