//! P2 测量核心：墙钟 / CPU 时间 / MB/s / Mevents/s / 回调串行占比 / 峰值内存。
//!
//! 口径（指导书 P2 / P2.5-G）：
//! - 每次 bench 调用 = 独立进程（峰值内存为进程生命周期峰值，矩阵逐格新进程）；
//! - 预热 >= 2 轮丢弃，正式轮报告 median/min/max；
//! - minimal 回调 ≈ 纯解析+派发；realistic 回调模拟下游（排序+转换+文本复制+全局归并），
//!   二者差值 = 下游串行工作量（Amdahl 串行段实证，P3 假设输入）；
//! - 回调耗时按轨记录（每轨一次 Instant，非每事件），sum/wall = 串行占比上界。

use std::time::Instant;

use midly::loader::{TrackAllEvents, extract_all_events_per_track_streaming_from_bytes};

use crate::sys;

/// 测量配置。
pub struct BenchConfig {
    pub threads: usize,
    pub warmup: usize,
    pub runs: usize,
    pub realistic: bool,
    pub label: String,
}

/// 单轨回调记录。
pub struct TrackStat {
    pub track_idx: usize,
    pub notes: usize,
    pub callback_ms: f64,
}

/// 测量报告。
pub struct BenchReport {
    pub mode: &'static str,
    pub threads_actual: usize,
    pub wall_median_ms: f64,
    pub wall_min_ms: f64,
    pub wall_max_ms: f64,
    pub cpu_median_ms: f64,
    pub mb_per_s: f64,
    pub mevents_per_s: f64,
    pub callback_sum_median_ms: f64,
    pub serial_fraction: f64,
    pub peak_ws_mb: f64,
    pub total_notes: u64,
    pub total_events: u64,
    pub tracks: usize,
    pub track_stats: Vec<TrackStat>,
}

/// 下游模拟 sink：minimal 只计数；realistic 复刻 midi-model 回调工作。
#[derive(Default)]
struct Sink {
    notes: u64,
    events: u64,
    tracks: usize,
    // realistic 产物（防优化删除 + 模拟峰值内存）
    per_track_notes: Vec<Vec<midly::loader::PackedNote>>,
    global_controls: Vec<midly::loader::PackedControlEvent>,
    global_tempo: Vec<(u32, f32)>,
    text_arena: Vec<u8>,
    track_stats: Vec<TrackStat>,
}

impl Sink {
    fn handle(&mut self, idx: usize, mut ev: TrackAllEvents, realistic: bool) {
        let t0 = Instant::now();
        let n = ev.notes.len();
        let e = n
            + ev.control_events.len()
            + ev.tempo_changes.len()
            + ev.time_signatures.len()
            + ev.key_signatures.len()
            + ev.lyrics.len()
            + ev.markers.len()
            + ev.sys_ex.len();
        #[cfg(feature = "text-events")]
        let e = e + ev.text_events.len();
        if realistic {
            // 模拟下游：稳定排序（start_tick）→ 转换进每轨连续 Vec → 文本复制 → 全局归并。
            // 当前 API 回调在全局 Mutex 内执行，这些工作全部串行（基线现状）。
            ev.notes.sort_by_key(|n| n.start_tick);
            self.per_track_notes.push(ev.notes.clone());
            self.global_controls
                .extend(ev.control_events.iter().copied());
            self.global_tempo.extend(ev.tempo_changes.iter().copied());
            for t in ev.lyrics.iter().chain(ev.markers.iter()) {
                self.text_arena.extend_from_slice(t.text);
            }
            #[cfg(feature = "text-events")]
            for t in &ev.text_events {
                self.text_arena.extend_from_slice(t.text);
            }
            for s in &ev.sys_ex {
                self.text_arena.extend_from_slice(s.data);
            }
        }
        self.notes += n as u64;
        self.events += e as u64;
        self.tracks = self.tracks.max(idx + 1);
        self.track_stats.push(TrackStat {
            track_idx: idx,
            notes: n,
            callback_ms: t0.elapsed().as_secs_f64() * 1000.0,
        });
    }
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if v.is_empty() {
        return 0.0;
    }
    let mid = v.len() / 2;
    if v.len() % 2 == 1 {
        v[mid]
    } else {
        (v[mid - 1] + v[mid]) / 2.0
    }
}

/// 执行一次完整测量（预热 + 正式轮）。
pub fn run_bench(bytes: &[u8], file_size: u64, cfg: &BenchConfig) -> Result<BenchReport, String> {
    let mut walls: Vec<f64> = Vec::new();
    let mut cpus: Vec<f64> = Vec::new();
    let mut cb_sums: Vec<f64> = Vec::new();
    let mut last = Sink::default();

    // 一个加载任务一个池（指导书 P2.5-A），整个 bench 复用，不逐轮建池。
    #[cfg(feature = "parallel")]
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cfg.threads)
        .build()
        .map_err(|e| format!("rayon 池构建失败: {e}"))?;
    #[cfg(feature = "parallel")]
    let threads_actual = pool.current_num_threads();
    #[cfg(not(feature = "parallel"))]
    let threads_actual = 0usize;

    let total_rounds = cfg.warmup + cfg.runs;
    for round in 0..total_rounds {
        let mut sink = Sink::default();
        let cpu0 = sys::cpu_time_secs();
        let t0 = Instant::now();
        // serial 构建需要 mut（FnMut 捕获 sink）；parallel 构建 install 按值消费，豁免 unused_mut。
        #[cfg_attr(feature = "parallel", allow(unused_mut))]
        let mut invoke = || {
            extract_all_events_per_track_streaming_from_bytes(bytes, |idx, ev| {
                sink.handle(idx, ev, cfg.realistic);
            })
        };
        #[cfg(feature = "parallel")]
        let result = pool.install(invoke);
        #[cfg(not(feature = "parallel"))]
        let result = invoke();
        let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let cpu_ms = (sys::cpu_time_secs() - cpu0) * 1000.0;
        result.map_err(|e| format!("解析失败: {e:?}"))?;
        if round >= cfg.warmup {
            walls.push(wall_ms);
            cpus.push(cpu_ms);
            cb_sums.push(sink.track_stats.iter().map(|t| t.callback_ms).sum());
            last = sink;
        }
    }

    let wall_med = median(&mut walls.clone());
    let cpu_med = median(&mut cpus.clone());
    let cb_med = median(&mut cb_sums.clone());
    let (peak, _) = sys::working_set();
    let file_mb = file_size as f64 / (1024.0 * 1024.0);
    let mode = if cfg!(feature = "parallel") {
        "parallel"
    } else {
        "serial"
    };

    Ok(BenchReport {
        mode,
        threads_actual,
        wall_median_ms: wall_med,
        wall_min_ms: walls.iter().cloned().fold(f64::INFINITY, f64::min),
        wall_max_ms: walls.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        cpu_median_ms: cpu_med,
        mb_per_s: file_mb / (wall_med / 1000.0),
        mevents_per_s: last.events as f64 / 1e6 / (wall_med / 1000.0),
        callback_sum_median_ms: cb_med,
        serial_fraction: if wall_med > 0.0 {
            cb_med / wall_med
        } else {
            0.0
        },
        peak_ws_mb: peak as f64 / (1024.0 * 1024.0),
        total_notes: last.notes,
        total_events: last.events,
        tracks: last.tracks,
        track_stats: last.track_stats,
    })
}

/// TSV 表头（原始数据归档用）。
pub fn tsv_header() -> &'static str {
    "label\tfile\tmode\tcallback\tthreads\twarmup\truns\twall_median_ms\twall_min_ms\twall_max_ms\tcpu_median_ms\tmb_per_s\tmevents_per_s\tcallback_sum_median_ms\tserial_fraction\tpeak_ws_mb\tnotes\tevents\ttracks\tfile_mb"
}

/// 单行 TSV。
pub fn tsv_row(
    label: &str,
    file: &str,
    cfg: &BenchConfig,
    r: &BenchReport,
    file_mb: f64,
) -> String {
    format!(
        "{label}\t{file}\t{}\t{}\t{}\t{}\t{}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.2}\t{:.3}\t{:.3}\t{:.4}\t{:.2}\t{}\t{}\t{}\t{:.3}",
        r.mode,
        if cfg.realistic {
            "realistic"
        } else {
            "minimal"
        },
        r.threads_actual,
        cfg.warmup,
        cfg.runs,
        r.wall_median_ms,
        r.wall_min_ms,
        r.wall_max_ms,
        r.cpu_median_ms,
        r.mb_per_s,
        r.mevents_per_s,
        r.callback_sum_median_ms,
        r.serial_fraction,
        r.peak_ws_mb,
        r.total_notes,
        r.total_events,
        r.tracks,
        file_mb,
    )
}
