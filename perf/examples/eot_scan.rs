//! EOT 诊断扫描：统计一个 MIDI 文件里「EOT 后仍有事件」「悬挂音符」「缺 EOT」的轨数。
//!
//! 用法（在 `perf/` 下）：
//!   cargo run --release --example eot_scan -- <file.mid>

use midly::loader::{TrackAllEvents, extract_all_events_per_track_streaming_from_bytes};

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("用法: eot_scan <file.mid>");
            std::process::exit(2);
        }
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("读取失败 {path}: {e}");
            std::process::exit(2);
        }
    };

    let mut tracks = 0usize;
    let mut after_eot = 0usize;
    let mut unclosed = 0usize;
    let mut missing_eot = 0usize;
    let mut details = Vec::new();

    let result = extract_all_events_per_track_streaming_from_bytes(
        &bytes,
        |idx: usize, ev: TrackAllEvents| {
            tracks += 1;
            let t = ev.timing;
            if t.has_events_after_eot {
                after_eot += 1;
            }
            if t.has_unclosed_notes {
                unclosed += 1;
            }
            if t.eot_tick.is_none() {
                missing_eot += 1;
            }
            if (t.has_events_after_eot || t.has_unclosed_notes) && details.len() < 10 {
                details.push((
                    idx,
                    t.eot_tick,
                    t.content_end_tick,
                    t.physical_end_tick,
                    t.has_events_after_eot,
                    t.has_unclosed_notes,
                ));
            }
        },
    );
    if let Err(e) = result {
        eprintln!("解析失败: {e:?}");
        std::process::exit(1);
    }

    println!("file\t{path}");
    println!(
        "tracks\t{tracks}\tafter_eot\t{after_eot}\tunclosed\t{unclosed}\tmissing_eot\t{missing_eot}"
    );
    for (i, eot, content, phys, ae, un) in details {
        println!(
            "track\t{i}\teot={eot:?}\tcontent={content}\tphysical={phys}\tafter_eot={ae}\tunclosed={un}"
        );
    }
}
