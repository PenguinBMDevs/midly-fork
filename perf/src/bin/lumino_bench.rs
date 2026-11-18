//! Tier2 测量台：lumino 主链路 `MidiDocument::from_notes_bytes` 端到端。
//!
//! 目的：在**不修改 lumino-rs 任何文件**的前提下，用真实下游 crate 测量
//! 「fork 接入前 / 接入后」的端到端墙钟、峰值内存与产物摘要（正确性门）。
//!
//! 构建（要求同机存在 `../../lumino-rs` 检出；仅本 bench 需要，不影响 midly-perf）：
//! ```text
//! cargo build --release --features lumino --bin lumino-bench
//! ```
//! 版本切换与 midly-perf 相同（`[patch.crates-io]` 已指向 fork）：
//! ```text
//! cargo build --release --features lumino --bin lumino-bench \
//!   --config "patch.crates-io.lumino-midly.path='F:/midly-baselines/lumino-midly-0.6.5'"
//! ```
//!
//! 用法：
//! ```text
//! lumino-bench digest <file.mid> [out.txt]
//! lumino-bench phases <file.mid>          # 进度回调 → 分阶段墙钟
//! lumino-bench run <file.mid> [--warmup 2] [--runs 5] [--out results.tsv] [--label L]
//! ```
//!
//! **摘要口径（重要）**：主链路用并行回调逐轨产出，跨轨**同 tick** 事件的相对
//! 顺序取决于轨到达顺序（既有行为，0.6.5 亦然）。因此摘要对全局事件列表按
//! `(tick, track, …)` 规范排序后再哈希 —— 语义等价判定不受调度顺序影响；
//! 逐轨音符本就确定（轨内文件序），按轨序哈希。摘要只覆盖公开字段。
//!
//! 口径：每次 `run` = 独立进程；预热丢弃；median/min/max；峰值 = 进程峰值工作集。
//! 计时轮次内不做摘要（避免临时分配污染峰值内存）。

#[path = "../sys.rs"]
mod sys;

use std::fs;
use std::hint::black_box;
use std::io::Write;
use std::process::ExitCode;
use std::time::Instant;

use lumino_midi_model::{MidiDocument, NoteEvent};
use midly::loader::PackedControlEvent;

/// FNV-1a 64 位（跨构建稳定，不依赖 std 的 DefaultHasher 随机种子）。
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    #[inline]
    fn byte(&mut self, b: u8) {
        self.0 ^= b as u64;
        self.0 = self.0.wrapping_mul(0x100_0000_01b3);
    }

    fn u16(&mut self, v: u16) {
        for b in v.to_le_bytes() {
            self.byte(b);
        }
    }

    fn u32(&mut self, v: u32) {
        for b in v.to_le_bytes() {
            self.byte(b);
        }
    }

    fn u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.byte(b);
        }
    }

    fn i8(&mut self, v: i8) {
        self.byte(v as u8);
    }

    fn bytes(&mut self, data: &[u8]) {
        self.u64(data.len() as u64);
        for &b in data {
            self.byte(b);
        }
    }

    fn opt_str(&mut self, value: Option<&str>) {
        match value {
            None => self.byte(0),
            Some(s) => {
                self.byte(1);
                self.bytes(s.as_bytes());
            }
        }
    }
}

fn hash_notes(doc: &MidiDocument) -> (u64, u64) {
    let mut h = Fnv::new();
    let mut total = 0u64;
    h.u64(doc.notes.len() as u64);
    for list in &doc.notes {
        h.u64(list.len() as u64);
        for n in list.iter() {
            hash_note(&mut h, n);
        }
        total += list.len() as u64;
    }
    (h.0, total)
}

fn hash_note(h: &mut Fnv, n: &NoteEvent) {
    h.u32(n.start_tick);
    h.u32(n.end_tick);
    h.byte(n.key);
    h.byte(n.velocity);
    h.byte(n.release_velocity);
    h.byte(n.channel);
}

/// 控制事件按 `(tick, track, kind, channel, param)` 规范排序后哈希。
fn hash_controls(doc: &MidiDocument) -> u64 {
    let mut items: Vec<(u32, u16, u8, u8, u16)> = Vec::with_capacity(doc.control_events.len());
    for c in doc.control_events.iter() {
        let c: PackedControlEvent = *c; // packed：先按值拷贝
        items.push((c.tick, c.track, c.kind, c.channel, c.param));
    }
    items.sort_unstable();
    let mut h = Fnv::new();
    h.u64(items.len() as u64);
    for (tick, track, kind, channel, param) in items {
        h.u32(tick);
        h.u16(track);
        h.byte(kind);
        h.byte(channel);
        h.u16(param);
    }
    h.0
}

fn hash_tempo(doc: &MidiDocument) -> u64 {
    let mut items: Vec<(u32, u32)> = doc
        .tempo_changes
        .iter()
        .map(|&(tick, bpm)| (tick, bpm.to_bits()))
        .collect();
    items.sort_unstable();
    let mut h = Fnv::new();
    h.u64(items.len() as u64);
    for (tick, bits) in items {
        h.u32(tick);
        h.u32(bits);
    }
    h.0
}

fn hash_time_sigs(doc: &MidiDocument) -> u64 {
    let mut items = doc.time_signatures.clone();
    items.sort_unstable();
    let mut h = Fnv::new();
    h.u64(items.len() as u64);
    for (tick, num, den) in items {
        h.u32(tick);
        h.byte(num);
        h.byte(den);
    }
    h.0
}

fn hash_key_sigs(doc: &MidiDocument) -> u64 {
    let mut items = doc.key_signatures.clone();
    items.sort_unstable();
    let mut h = Fnv::new();
    h.u64(items.len() as u64);
    for (tick, sharps, minor) in items {
        h.u32(tick);
        h.i8(sharps);
        h.byte(minor as u8);
    }
    h.0
}

fn hash_text_list(items: &[(u32, u16, Vec<u8>)]) -> u64 {
    let mut items: Vec<(u32, u16, &[u8])> = items
        .iter()
        .map(|(t, tr, d)| (*t, *tr, d.as_slice()))
        .collect();
    items.sort_unstable();
    let mut h = Fnv::new();
    h.u64(items.len() as u64);
    for (tick, track, data) in items {
        h.u32(tick);
        h.u16(track);
        h.bytes(data);
    }
    h.0
}

fn hash_text_meta(doc: &MidiDocument) -> u64 {
    let mut items: Vec<(u32, u16, u8, &[u8])> = doc
        .text_events
        .iter()
        .map(|(t, tr, ty, d)| (*t, *tr, *ty, d.as_slice()))
        .collect();
    items.sort_unstable();
    let mut h = Fnv::new();
    h.u64(items.len() as u64);
    for (tick, track, meta_type, data) in items {
        h.u32(tick);
        h.u16(track);
        h.byte(meta_type);
        h.bytes(data);
    }
    h.0
}

fn hash_track_meta(doc: &MidiDocument) -> u64 {
    let mut h = Fnv::new();
    h.u32(doc.track_ports.len() as u32);
    for &p in &doc.track_ports {
        h.byte(p);
    }
    h.u32(doc.track_names.len() as u32);
    for name in &doc.track_names {
        h.opt_str(name.as_deref());
    }
    h.0
}

/// 分字段摘要（任一项不一致都能直接定位到字段）。
struct FieldDigest {
    header: u64,
    notes: u64,
    controls: u64,
    tempo: u64,
    time_sigs: u64,
    key_sigs: u64,
    lyrics: u64,
    markers: u64,
    text_meta: u64,
    sysex: u64,
    track_meta: u64,
    combined: u64,
    total_notes: u64,
    controls_len: usize,
    tracks: usize,
    total_ticks: u32,
}

fn digest_document(doc: &MidiDocument) -> FieldDigest {
    let mut header_h = Fnv::new();
    header_h.u16(doc.track_count);
    header_h.u16(doc.division);
    header_h.u32(doc.total_ticks);

    let (notes_h, total_notes) = hash_notes(doc);
    let controls_h = hash_controls(doc);
    let tempo_h = hash_tempo(doc);
    let ts_h = hash_time_sigs(doc);
    let ks_h = hash_key_sigs(doc);
    let lyrics_h = hash_text_list(&doc.lyrics);
    let markers_h = hash_text_list(&doc.markers);
    let text_h = hash_text_meta(doc);
    let sysex_h = hash_text_list(&doc.sys_ex);
    let track_h = hash_track_meta(doc);

    let mut combined = Fnv::new();
    for field in [
        header_h.0, notes_h, controls_h, tempo_h, ts_h, ks_h, lyrics_h, markers_h, text_h, sysex_h,
        track_h,
    ] {
        combined.u64(field);
    }

    FieldDigest {
        header: header_h.0,
        notes: notes_h,
        controls: controls_h,
        tempo: tempo_h,
        time_sigs: ts_h,
        key_sigs: ks_h,
        lyrics: lyrics_h,
        markers: markers_h,
        text_meta: text_h,
        sysex: sysex_h,
        track_meta: track_h,
        combined: combined.0,
        total_notes,
        controls_len: doc.control_events.len(),
        tracks: doc.track_count as usize,
        total_ticks: doc.total_ticks,
    }
}

impl FieldDigest {
    fn to_line(&self) -> String {
        format!(
            "combined={:016x}\theader={:016x}\tnotes={:016x}\tcontrols={:016x}\ttempo={:016x}\ttime_sigs={:016x}\tkey_sigs={:016x}\tlyrics={:016x}\tmarkers={:016x}\ttext_meta={:016x}\tsysex={:016x}\ttrack_meta={:016x}\tnotes_n={}\tcontrols_n={}\ttracks={}\ttotal_ticks={}",
            self.combined,
            self.header,
            self.notes,
            self.controls,
            self.tempo,
            self.time_sigs,
            self.key_sigs,
            self.lyrics,
            self.markers,
            self.text_meta,
            self.sysex,
            self.track_meta,
            self.total_notes,
            self.controls_len,
            self.tracks,
            self.total_ticks,
        )
    }
}

fn parse_document(bytes: &[u8]) -> Result<MidiDocument, String> {
    let (doc, _division, _total_notes) =
        MidiDocument::from_notes_bytes(bytes, None).map_err(|e| format!("加载失败: {e}"))?;
    Ok(doc)
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2.0
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!(
            "用法: lumino-bench <digest|run> <file.mid> [out] [--warmup N] [--runs N] [--label L] [--out tsv]"
        );
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

    if mode == "digest" {
        return match parse_document(&bytes) {
            Ok(doc) => {
                let line = digest_document(&doc).to_line();
                println!("{line}");
                if let Some(out) = args.get(2) {
                    if let Err(e) = fs::write(out, format!("{line}\n")) {
                        eprintln!("写入摘要失败 {out}: {e}");
                        return ExitCode::FAILURE;
                    }
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{path}: {e}");
                ExitCode::FAILURE
            }
        };
    }

    if mode == "phases" {
        // 分阶段时间线：把主链路的进度回调（0.05/0.10/0.15/…/0.90）映射成墙钟。
        // 不改变任何被测代码；进度点由 document_build 内部决定。
        let t0 = Instant::now();
        let marks: std::sync::Mutex<Vec<(f64, f64)>> = std::sync::Mutex::new(Vec::new());
        let result = MidiDocument::from_notes_bytes(
            &bytes,
            Some(&|p| {
                if let Ok(mut m) = marks.lock() {
                    m.push((p, t0.elapsed().as_secs_f64() * 1000.0));
                }
            }),
        );
        return match result {
            Ok((_doc, _division, notes)) => {
                let list = marks.lock().map(|m| m.clone()).unwrap_or_default();
                let mut prev = 0.0f64;
                for (p, t) in list {
                    println!("phase\t{p:.2}\t{t:.1}ms\t+{:.1}ms", t - prev);
                    prev = t;
                }
                println!("phase\tend\t{:.1}ms", t0.elapsed().as_secs_f64() * 1000.0);
                println!("notes\t{notes}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{path}: 加载失败: {e}");
                ExitCode::FAILURE
            }
        };
    }

    if mode != "run" {
        eprintln!("未知模式: {mode}");
        return ExitCode::FAILURE;
    }

    let mut warmup = 2usize;
    let mut runs = 5usize;
    let mut tsv_out: Option<String> = None;
    let mut label = "lumino".to_string();
    let mut i = 2;
    while i < args.len() {
        let key = args[i].as_str();
        let val = args.get(i + 1).map(|s| s.as_str()).unwrap_or("");
        match key {
            "--warmup" => warmup = val.parse().unwrap_or(2),
            "--runs" => runs = val.parse().unwrap_or(5),
            "--out" => tsv_out = Some(val.to_string()),
            "--label" => label = val.to_string(),
            _ => {
                eprintln!("未知选项: {key}");
                return ExitCode::FAILURE;
            }
        }
        i += 2;
    }

    let mut walls: Vec<f64> = Vec::new();
    let mut cpus: Vec<f64> = Vec::new();
    let mut stats: Option<(u64, usize, usize, u32)> = None;

    for round in 0..(warmup + runs) {
        let cpu0 = sys::cpu_time_secs();
        let t0 = Instant::now();
        let doc = match parse_document(&bytes) {
            Ok(doc) => doc,
            Err(e) => {
                eprintln!("{path}: {e}");
                return ExitCode::FAILURE;
            }
        };
        let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let cpu_ms = (sys::cpu_time_secs() - cpu0) * 1000.0;
        black_box(&doc);
        if round >= warmup {
            walls.push(wall_ms);
            cpus.push(cpu_ms);
            // 计时轮内只做计数（零临时分配），摘要走独立 `digest` 模式。
            let total_notes: u64 = doc.notes.iter().map(|l| l.len() as u64).sum();
            stats = Some((
                total_notes,
                doc.control_events.len(),
                doc.track_count as usize,
                doc.total_ticks,
            ));
        }
    }

    let (total_notes, controls, tracks, total_ticks) = match stats {
        Some(s) => s,
        None => {
            eprintln!("未产生任何正式轮次（runs 必须 > 0）");
            return ExitCode::FAILURE;
        }
    };
    let wall_med = median(&mut walls.clone());
    let wall_min = walls.iter().cloned().fold(f64::INFINITY, f64::min);
    let wall_max = walls.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let cpu_med = median(&mut cpus.clone());
    let (peak_ws, _) = sys::working_set();

    eprintln!(
        "[lumino] {} | wall median={:.2}ms min={:.2} max={:.2} | cpu={:.2}ms | peak={:.1}MB | notes={} controls={} tracks={} ticks={}",
        path,
        wall_med,
        wall_min,
        wall_max,
        cpu_med,
        peak_ws as f64 / (1024.0 * 1024.0),
        total_notes,
        controls,
        tracks,
        total_ticks,
    );

    if let Some(out) = tsv_out {
        let need_header = !std::path::Path::new(&out).exists();
        let mut f = match fs::OpenOptions::new().create(true).append(true).open(&out) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("结果文件不可写 {out}: {e}");
                return ExitCode::FAILURE;
            }
        };
        if need_header {
            writeln!(
                f,
                "label\tfile\twarmup\truns\twall_median_ms\twall_min_ms\twall_max_ms\tcpu_median_ms\tpeak_ws_mb\tnotes\tcontrols\ttracks\ttotal_ticks\tfile_mb"
            )
            .ok();
        }
        writeln!(
            f,
            "{label}\t{path}\t{warmup}\t{runs}\t{wall_med:.3}\t{wall_min:.3}\t{wall_max:.3}\t{cpu_med:.3}\t{:.2}\t{total_notes}\t{controls}\t{tracks}\t{total_ticks}\t{file_mb:.3}",
            peak_ws as f64 / (1024.0 * 1024.0),
        )
        .ok();
    }

    ExitCode::SUCCESS
}
