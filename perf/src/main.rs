//! midly-perf：P2 性能基线测量台 CLI。
//!
//! 用法：
//!   midly-perf characterize <file.mid>
//!   midly-perf bench <file.mid> [--threads N] [--warmup W] [--runs R]
//!                  [--callback minimal|realistic] [--label L] [--out results.tsv]
//!
//! 版本切换（registry 依赖 + patch）：
//!   默认 patch 指向 fork（0.6.5+修复）；基线对照：
//!   cargo run --release --config "patch.crates-io.lumino-midly.path='F:/midly-baselines/lumino-midly-0.6.3'" -- bench ...
//!   serial 构建：cargo run --release --no-default-features -- bench ...

mod bench;
mod sys;

use std::fs;
use std::io::Write;
use std::process::ExitCode;

use midly::loader::extract_all_events_per_track_streaming_from_bytes;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("用法: midly-perf <characterize|bench> <file.mid> [选项]");
        return ExitCode::FAILURE;
    }
    let mode = args[0].as_str();
    let path = &args[1];
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("读取失败 {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let file_mb = bytes.len() as f64 / (1024.0 * 1024.0);

    match mode {
        "characterize" => characterize(path, &bytes, file_mb),
        "bench" => run_bench_cli(path, &bytes, file_mb, &args[2..]),
        _ => {
            eprintln!("未知模式: {mode}");
            ExitCode::FAILURE
        }
    }
}

/// 素材表征：逐轨事件计数 + 形态分类（P2 矩阵素材选型输入）。
fn characterize(path: &str, bytes: &[u8], file_mb: f64) -> ExitCode {
    #[derive(Default, Clone)]
    struct TrackInfo {
        notes: usize,
        controls: usize,
        tempo: usize,
        ts: usize,
        ks: usize,
        lyrics: usize,
        markers: usize,
        texts: usize,
        sysex: usize,
        sysex_bytes: usize,
        max_end: u32,
        port: Option<u8>,
    }
    let mut tracks: Vec<TrackInfo> = Vec::new();
    let result = extract_all_events_per_track_streaming_from_bytes(bytes, |idx, ev| {
        if tracks.len() <= idx {
            tracks.resize_with(idx + 1, TrackInfo::default);
        }
        let t = &mut tracks[idx];
        t.notes = ev.notes.len();
        t.controls = ev.control_events.len();
        t.tempo = ev.tempo_changes.len();
        t.ts = ev.time_signatures.len();
        t.ks = ev.key_signatures.len();
        t.lyrics = ev.lyrics.len();
        t.markers = ev.markers.len();
        #[cfg(feature = "text-events")]
        {
            t.texts = ev.text_events.len();
        }
        t.sysex = ev.sys_ex.len();
        t.sysex_bytes = ev.sys_ex.iter().map(|s| s.data.len()).sum();
        t.max_end = ev.notes.iter().map(|n| n.end_tick).max().unwrap_or(0);
        t.port = ev.midi_port;
    });
    if let Err(e) = result {
        eprintln!("{path}: 解析失败 {e:?}");
        return ExitCode::FAILURE;
    }

    let total_notes: usize = tracks.iter().map(|t| t.notes).sum();
    let max_notes = tracks.iter().map(|t| t.notes).max().unwrap_or(0);
    let shape = if tracks.len() == 1 {
        "单轨"
    } else if total_notes > 0 && max_notes as f64 / total_notes as f64 > 0.8 {
        "巨轨主导"
    } else {
        "多轨均衡"
    };

    println!(
        "file\t{path}\tsize_mb\t{file_mb:.3}\ttracks\t{}\tnotes\t{total_notes}\tshape\t{shape}",
        tracks.len()
    );
    println!(
        "track\tnotes\tcontrols\ttempo\tts\tks\tlyrics\tmarkers\ttexts\tsysex\tsysex_bytes\tmax_end\tport"
    );
    for (i, t) in tracks.iter().enumerate() {
        println!(
            "{i}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            t.notes,
            t.controls,
            t.tempo,
            t.ts,
            t.ks,
            t.lyrics,
            t.markers,
            t.texts,
            t.sysex,
            t.sysex_bytes,
            t.max_end,
            t.port.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
        );
    }
    ExitCode::SUCCESS
}

/// bench 子命令。
fn run_bench_cli(path: &str, bytes: &[u8], file_mb: f64, opts: &[String]) -> ExitCode {
    let mut cfg = bench::BenchConfig {
        threads: 0,
        warmup: 2,
        runs: 5,
        realistic: false,
        label: "fork-0.6.5".to_string(),
    };
    let mut out: Option<String> = None;
    let mut i = 0;
    while i < opts.len() {
        let key = opts[i].as_str();
        let val = opts.get(i + 1).map(|s| s.as_str()).unwrap_or("");
        match key {
            "--threads" => cfg.threads = val.parse().unwrap_or(0),
            "--warmup" => cfg.warmup = val.parse().unwrap_or(2),
            "--runs" => cfg.runs = val.parse().unwrap_or(5),
            "--callback" => cfg.realistic = val == "realistic",
            "--label" => cfg.label = val.to_string(),
            "--out" => out = Some(val.to_string()),
            _ => {
                eprintln!("未知选项: {key}");
                return ExitCode::FAILURE;
            }
        }
        i += 2;
    }

    match bench::run_bench(bytes, bytes.len() as u64, &cfg) {
        Ok(r) => {
            eprintln!(
                "[bench] {} mode={} threads={} callback={} | wall median={:.2}ms min={:.2} max={:.2} | cpu={:.2}ms | {:.1} MB/s {:.3} Mevents/s | 回调串行占比={:.1}% | peak={:.1}MB | notes={} events={} tracks={}",
                path,
                r.mode,
                r.threads_actual,
                if cfg.realistic {
                    "realistic"
                } else {
                    "minimal"
                },
                r.wall_median_ms,
                r.wall_min_ms,
                r.wall_max_ms,
                r.cpu_median_ms,
                r.mb_per_s,
                r.mevents_per_s,
                r.serial_fraction * 100.0,
                r.peak_ws_mb,
                r.total_notes,
                r.total_events,
                r.tracks,
            );
            // 每轨回调耗时分布（指导书 P2：每轨耗时分布；按轨记录，非每事件计时）。
            if !r.track_stats.is_empty() {
                let mut holds: Vec<f64> = r.track_stats.iter().map(|t| t.callback_ms).collect();
                holds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let p50 = holds[holds.len() / 2];
                let p90 = holds[(holds.len() * 9 / 10).min(holds.len() - 1)];
                let max = holds[holds.len() - 1];
                let slowest = r
                    .track_stats
                    .iter()
                    .max_by(|a, b| {
                        a.callback_ms
                            .partial_cmp(&b.callback_ms)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .expect("track_stats 非空");
                eprintln!(
                    "[tracks] 回调耗时 p50={p50:.3}ms p90={p90:.3}ms max={max:.3}ms | 最慢轨#{} notes={} {:.3}ms",
                    slowest.track_idx, slowest.notes, slowest.callback_ms,
                );
            }
            if let Some(out_path) = out {
                let row = bench::tsv_row(&cfg.label, path, &cfg, &r, file_mb);
                let need_header = !std::path::Path::new(&out_path).exists();
                let mut f = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&out_path)
                    .expect("结果文件必须可写");
                if need_header {
                    writeln!(f, "{}", bench::tsv_header()).expect("TSV 表头写入失败");
                }
                writeln!(f, "{row}").expect("TSV 行写入失败");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{path}: {e}");
            ExitCode::FAILURE
        }
    }
}
