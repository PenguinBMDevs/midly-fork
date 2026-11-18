//! midly-perf：P2 性能基线测量台 CLI。
//!
//! 用法：
//!   midly-perf gen <out_dir>
//!   midly-perf characterize <file.mid>
//!   midly-perf to-format0 <src.mid> <dst.mid> [--track N]
//!   midly-perf bench <file.mid> [--threads N] [--warmup W] [--runs R]
//!                  [--callback minimal|realistic|realistic-par] [--label L] [--out results.tsv]
//!
//! 版本切换（registry 依赖 + patch）：
//!   默认 patch 指向 fork（0.6.5+修复）；基线对照：
//!   cargo run --release --config "patch.crates-io.lumino-midly.path='F:/midly-baselines/lumino-midly-0.6.3'" -- bench ...
//!   serial 构建：cargo run --release --no-default-features -- bench ...

mod bench;
mod materials;
mod sort_proto;
mod sys;

use std::fs;
use std::io::Write;
use std::process::ExitCode;

use midly::loader::extract_all_events_per_track_streaming_from_bytes;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("用法: midly-perf <gen|characterize|bench> <path> [选项]");
        return ExitCode::FAILURE;
    }
    let mode = args[0].as_str();
    // sort-proto 的合成模式不需要文件（规模曲线用）。
    if mode == "sort-proto" && args.get(1).map(String::as_str) == Some("--synthetic") {
        return run_sort_proto(&[], &args[1..]);
    }
    let path = &args[1];
    // gen 只需输出目录，不读文件。
    if mode == "gen" {
        return gen_cli(path);
    }
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
        "to-format0" => to_format0(&bytes, &args[2..]),
        "sort-proto" => run_sort_proto(&bytes, &args[2..]),
        "bench" => run_bench_cli(path, &bytes, file_mb, &args[2..]),
        _ => {
            eprintln!("未知模式: {mode}");
            ExitCode::FAILURE
        }
    }
}

/// 拆分 SMF 的 MTrk 原始字节（不解析事件，零分配拷贝）。
///
/// 返回 `(division, 各轨原始字节)`；只支持裸 SMF（RIFF/.rmi 先解包后使用）。
fn split_smf_tracks(data: &[u8]) -> Result<(u16, Vec<&[u8]>), String> {
    if data.len() < 14 || &data[0..4] != b"MThd" {
        return Err("不是裸 SMF（MThd 缺失；RIFF/.rmi 请先解包）".into());
    }
    let header_len = u32::from_be_bytes(data[4..8].try_into().unwrap_or([0; 4])) as usize;
    let division = u16::from_be_bytes(data[12..14].try_into().unwrap_or([0; 2]));
    let mut offset = 8 + header_len;
    let mut tracks = Vec::new();
    while offset + 8 <= data.len() {
        let id = &data[offset..offset + 4];
        let len =
            u32::from_be_bytes(data[offset + 4..offset + 8].try_into().unwrap_or([0; 4])) as usize;
        let start = offset + 8;
        let end = start.saturating_add(len).min(data.len());
        if id == b"MTrk" {
            tracks.push(&data[start..end]);
        }
        if end <= offset {
            break;
        }
        offset = end;
    }
    Ok((division, tracks))
}

/// 组装单轨 format 0 SMF（MThd + 一条原样 MTrk）。
fn build_format0(division: u16, track: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(track.len() + 22);
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // format 0
    out.extend_from_slice(&1u16.to_be_bytes()); // 1 track
    out.extend_from_slice(&division.to_be_bytes());
    out.extend_from_slice(b"MTrk");
    out.extend_from_slice(&(track.len() as u32).to_be_bytes());
    out.extend_from_slice(track);
    out
}

/// to-format0：把源文件的某条轨**原始字节**抽成单轨 format 0。
///
/// 语义保真：事件、绝对 tick、轨内 meta（TrackName/MidiPort）与 running status
/// 全部原样保留（不做事件重编码），因此不会出现「合并多轨导致的音符重新配对」。
/// 默认抽取事件数最多的轨。
fn to_format0(bytes: &[u8], opts: &[String]) -> ExitCode {
    let mut dst: Option<String> = None;
    let mut track_sel: Option<usize> = None;
    let mut i = 0;
    while i < opts.len() {
        match opts[i].as_str() {
            "--track" => {
                track_sel = opts.get(i + 1).and_then(|s| s.parse().ok());
                i += 2;
            }
            other if other.starts_with("--") => {
                eprintln!("未知选项: {other}");
                return ExitCode::FAILURE;
            }
            other => {
                dst = Some(other.to_string());
                i += 1;
            }
        }
    }
    let Some(dst) = dst else {
        eprintln!("用法: midly-perf to-format0 <src.mid> <dst.mid> [--track N]");
        return ExitCode::FAILURE;
    };
    let (division, tracks) = match split_smf_tracks(bytes) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if tracks.is_empty() {
        eprintln!("源文件没有 MTrk");
        return ExitCode::FAILURE;
    }
    let idx = track_sel.unwrap_or_else(|| {
        tracks
            .iter()
            .enumerate()
            .max_by_key(|(_, t)| t.len())
            .map(|(i, _)| i)
            .unwrap_or(0)
    });
    let Some(track) = tracks.get(idx) else {
        eprintln!("轨索引 {idx} 越界（共 {} 轨）", tracks.len());
        return ExitCode::FAILURE;
    };
    let out = build_format0(division, track);
    if let Err(e) = fs::write(&dst, &out) {
        eprintln!("写入失败 {dst}: {e}");
        return ExitCode::FAILURE;
    }
    println!(
        "src_tracks\t{}\tselected\t{}\ttrack_bytes\t{}\tout_bytes\t{}\tdivision\t{}",
        tracks.len(),
        idx,
        track.len(),
        out.len(),
        division
    );
    ExitCode::SUCCESS
}

/// gen 子命令：合成 4 类缺失素材（指导书 §P2 素材矩阵）。
fn gen_cli(out_dir: &str) -> ExitCode {
    match materials::generate_all(std::path::Path::new(out_dir)) {
        Ok(items) => {
            let mut total = 0usize;
            for (name, size) in &items {
                println!("{name}\t{size}\t{:.3} MB", *size as f64 / (1024.0 * 1024.0));
                total += size;
            }
            println!("total\t{total}\t{:.3} MB", total as f64 / (1024.0 * 1024.0));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("生成失败 {out_dir}: {e}");
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
        callback: bench::CallbackKind::Minimal,
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
            "--callback" => {
                cfg.callback = match val {
                    "minimal" => bench::CallbackKind::Minimal,
                    "retain-all" => bench::CallbackKind::Retain,
                    "realistic" => bench::CallbackKind::Realistic,
                    "realistic-par" => bench::CallbackKind::ParallelApi,
                    other => {
                        eprintln!(
                            "未知 callback: {other}（可选 minimal|retain-all|realistic|realistic-par）"
                        );
                        return ExitCode::FAILURE;
                    }
                }
            }
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
                "[bench] {} mode={} threads={} callback={} | wall median={:.2}ms min={:.2} max={:.2} | cpu={:.2}ms | {:.1} MB/s {:.3} Mevents/s | 串行段占比={:.1}% | peak={:.1}MB | notes={} events={} tracks={}",
                path,
                r.mode,
                r.threads_actual,
                cfg.callback.label(),
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
            // P3：并行回调总和是「并行工作量」，最终归并才是真正的串行段。
            if cfg.callback == bench::CallbackKind::ParallelApi {
                eprintln!(
                    "[p3] 并行回调总和={:.3}ms（/墙钟={:.2}×）| 最终归并={:.3}ms",
                    r.callback_sum_median_ms,
                    r.callback_sum_median_ms / r.wall_median_ms.max(f64::EPSILON),
                    r.merge_median_ms,
                );
            }
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

/// sort-proto：巨轨音符排序候选实现对比（稳定/不稳定/基数 + 已排序快速路径）。
fn run_sort_proto(bytes: &[u8], opts: &[String]) -> ExitCode {
    let mut runs = 5usize;
    let mut synthetic: Option<usize> = None;
    let mut i = 0;
    while i < opts.len() {
        match opts[i].as_str() {
            "--runs" => {
                runs = opts.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(5);
                i += 2;
            }
            "--synthetic" => {
                synthetic = opts.get(i + 1).and_then(|s| s.parse().ok());
                i += 2;
            }
            other => {
                eprintln!("未知选项: {other}");
                return ExitCode::FAILURE;
            }
        }
    }
    let pristine = match synthetic {
        Some(n) => sort_proto::synthetic_notes(n),
        None => match sort_proto::largest_track(bytes) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        },
    };
    if pristine.is_empty() {
        eprintln!("没有音符");
        return ExitCode::FAILURE;
    }
    let note_bytes = pristine.len() * core::mem::size_of::<midly::loader::PackedNote>();
    let (already_sorted, max_tick) = sort_proto::scan_sorted_and_max(&pristine);

    // 正确性对拍：radix/candidate 必须与稳定排序逐元素一致；unstable 仅要求有序。
    let stable_ok = sort_proto::verify_stable(&pristine, |mut v| {
        sort_proto::sort_stable(&mut v);
        v
    });
    let radix_ok = sort_proto::verify_stable(&pristine, |mut v| {
        sort_proto::radix_sort(&mut v, max_tick);
        v
    });
    let candidate_ok = sort_proto::verify_stable(&pristine, |mut v| {
        sort_proto::sort_candidate(&mut v);
        v
    });
    let unstable_ok = sort_proto::verify_sorted(&pristine, |mut v| {
        sort_proto::sort_unstable(&mut v);
        v
    });
    println!(
        "correctness\tstable={stable_ok}\tradix={radix_ok}\tcandidate={candidate_ok}\tunstable={unstable_ok}"
    );
    if !(stable_ok && radix_ok && candidate_ok && unstable_ok) {
        return ExitCode::FAILURE;
    }

    let t_stable = sort_proto::bench_variant("stable", &pristine, runs, |mut v| {
        sort_proto::sort_stable(&mut v);
        0
    });
    let t_unstable = sort_proto::bench_variant("unstable", &pristine, runs, |mut v| {
        sort_proto::sort_unstable(&mut v);
        0
    });
    let t_radix = sort_proto::bench_variant("radix", &pristine, runs, |mut v| {
        sort_proto::radix_sort(&mut v, max_tick)
    });
    let t_candidate = sort_proto::bench_variant("candidate", &pristine, runs, |mut v| {
        sort_proto::sort_candidate(&mut v)
    });
    let t_check = sort_proto::bench_variant("is_sorted_check", &pristine, runs, |v| {
        std::hint::black_box(sort_proto::is_sorted(&v));
        0
    });

    println!(
        "summary\tnotes={}\tnote_bytes={note_bytes}\tmax_tick={max_tick}\talready_sorted={already_sorted}",
        pristine.len()
    );
    for t in [&t_stable, &t_unstable, &t_radix, &t_candidate, &t_check] {
        println!(
            "variant\t{}\tmin={:.2}ms\tmed={:.2}ms\tscratch={:.1}MB",
            t.label,
            t.min_ms,
            t.med_ms,
            t.scratch_bytes as f64 / (1024.0 * 1024.0)
        );
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use midly::Smf;

    /// 抽轨必须是逐字节保真：MTrk 原样搬移，仅重写 MThd（format 0 / 1 轨）。
    #[test]
    fn format0_extraction_is_byte_faithful() {
        let src = crate::materials::control_dense(2, 8);
        let (division, tracks) = split_smf_tracks(&src).expect("split src");
        assert_eq!(tracks.len(), 2);

        let out = build_format0(division, tracks[1]);
        let (out_division, out_tracks) = split_smf_tracks(&out).expect("split out");
        assert_eq!(out_division, division);
        assert_eq!(out_tracks.len(), 1);
        assert_eq!(out_tracks[0], tracks[1], "抽出的 MTrk 必须逐字节一致");

        let src_smf = Smf::parse(&src).expect("参考链解析源文件");
        let out_smf = Smf::parse(&out).expect("参考链解析输出文件");
        assert!(matches!(out_smf.header.format, midly::Format::SingleTrack));
        assert_eq!(out_smf.tracks.len(), 1);
        assert_eq!(
            out_smf.tracks[0].len(),
            src_smf.tracks[1].len(),
            "输出轨事件数必须与源第 2 轨一致"
        );
    }
}
