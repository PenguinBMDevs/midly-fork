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
use std::sync::atomic::{AtomicUsize, Ordering};

use midly::Smf;
use midly::loader::extract_all_events_per_track_streaming_from_bytes;

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

/// 遍历 test-asset 全部 .mid/.rmi：
/// - 参考链可解析 → 完整对拍；
/// - 参考链拒绝（畸形文件）→ 快路径不得 panic，且两次运行结果逐轨一致（确定性）。
#[test]
fn test_oracle_all_assets() {
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

/// 畸形文件的快路径摘要（每轨计数 + 边界 tick + port），用于确定性比较。
fn fast_digest(bytes: &[u8]) -> Vec<TrackDigest> {
    let mut slots: Vec<Option<TrackDigest>> = Vec::new();
    extract_all_events_per_track_streaming_from_bytes(bytes, |idx, ev| {
        if slots.len() <= idx {
            slots.resize_with(idx + 1, || None);
        }
        let max_end = ev.notes.iter().map(|n| n.end_tick).max().unwrap_or(0);
        let min_start = ev.notes.iter().map(|n| n.start_tick).min().unwrap_or(0);
        slots[idx] = Some((
            ev.notes.len(),
            ev.control_events.len(),
            ev.sys_ex.len(),
            ev.text_events.len() + ev.lyrics.len() + ev.markers.len(),
            min_start,
            max_end,
            ev.midi_port,
        ));
    })
    .expect("畸形输入快路径不得返回错误（宽松解析）");
    slots
        .into_iter()
        .map(|s| s.unwrap_or((0, 0, 0, 0, 0, 0, None)))
        .collect()
}
