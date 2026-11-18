//! 音符排序原型（P4 后续：164M 实测「排序」占逐轨 handler 工作 62%）。
//!
//! 候选实现（都按 `start_tick` 升序）：
//! - `stable`：现状 `sort_by_key`（稳定 driftsort，14 字节 packed 记录原地交换）；
//! - `unstable`：`sort_unstable_by_key`（pdqsort；同 tick 顺序不保证）；
//! - `radix`：稳定 LSD 基数排序（3 趟 × 11 位，覆盖 u32），额外 scratch = n×14 字节；
//! - `is_sorted` 快速路径：已有序则直接跳过排序（代价一次线性扫描）。
//!
//! 原型只做测量与正确性对拍，不接入生产路径。

use std::time::Instant;

use midly::loader::{
    PackedNote, TrackAllEvents, extract_all_events_per_track_streaming_from_bytes,
};

const RADIX_BITS: u32 = 11;
const RADIX: usize = 1 << RADIX_BITS;
const RADIX_MASK: u32 = (RADIX as u32) - 1;
/// 短轨直接用标准库排序：scratch 与 radix 常数不值得。
const RADIX_MIN: usize = 4096;

/// 现状：稳定比较排序。
pub fn sort_stable(notes: &mut [PackedNote]) {
    notes.sort_by_key(|n| n.start_tick);
}

/// 不稳定比较排序（pdqsort）。
pub fn sort_unstable(notes: &mut [PackedNote]) {
    notes.sort_unstable_by_key(|n| n.start_tick);
}

/// 是否已按 `start_tick` 升序（O(n) 扫描）。
pub fn is_sorted(notes: &[PackedNote]) -> bool {
    notes.windows(2).all(|w| w[0].start_tick <= w[1].start_tick)
}

/// 一次扫描同时得到「是否已按 start_tick 升序」与最大 start_tick。
pub fn scan_sorted_and_max(notes: &[PackedNote]) -> (bool, u32) {
    let mut sorted = true;
    let mut max = 0u32;
    let mut prev = 0u32;
    for (i, n) in notes.iter().enumerate() {
        let t = n.start_tick;
        if i > 0 && t < prev {
            sorted = false;
        }
        prev = t;
        max = max.max(t);
    }
    (sorted, max)
}

/// 稳定 LSD 基数排序（趟数按 max_tick 自适应）；返回 scratch 字节数。
///
/// 只处理实际用到的位：164M 素材 max_tick=3,085,441（22 位）→ 2 趟 ×11 位。
pub fn radix_sort(notes: &mut Vec<PackedNote>, max_tick: u32) -> usize {
    let n = notes.len();
    let bits = 32 - max_tick.leading_zeros() as usize;
    let passes = bits.div_ceil(RADIX_BITS as usize);
    if n < RADIX_MIN || passes == 0 {
        notes.sort_by_key(|x| x.start_tick);
        return 0;
    }
    let zero = PackedNote {
        start_tick: 0,
        end_tick: 0,
        key: 0,
        velocity: 0,
        release_velocity: 0,
        channel: 0,
        track: 0,
    };
    let mut scratch = vec![zero; n];
    let mut count = [0usize; RADIX];
    let mut data = std::mem::take(notes);
    for p in 0..passes {
        let shift = (p as u32) * RADIX_BITS;
        count.fill(0);
        for note in &data {
            count[((note.start_tick >> shift) & RADIX_MASK) as usize] += 1;
        }
        let mut sum = 0usize;
        for c in count.iter_mut() {
            let t = *c;
            *c = sum;
            sum += t;
        }
        for note in &data {
            let d = ((note.start_tick >> shift) & RADIX_MASK) as usize;
            scratch[count[d]] = *note;
            count[d] += 1;
        }
        std::mem::swap(&mut data, &mut scratch);
    }
    *notes = data;
    n * std::mem::size_of::<PackedNote>()
}

/// 生产候选：一次扫描 → 已排序则跳过；≥阈值走基数排序，否则稳定比较排序。
pub fn sort_candidate(notes: &mut Vec<PackedNote>) -> usize {
    let (sorted, max_tick) = scan_sorted_and_max(notes);
    if sorted || notes.len() < RADIX_MIN {
        if !sorted {
            notes.sort_by_key(|x| x.start_tick);
        }
        return 0;
    }
    radix_sort(notes, max_tick)
}

/// 生成 `n` 个 start_tick 伪随机分布的音符（LCG，确定性），用于规模曲线。
pub fn synthetic_notes(n: usize) -> Vec<PackedNote> {
    let mut out = Vec::with_capacity(n);
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let span = n.max(1) as u64;
    for i in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let start = (state >> 33) % span;
        let dur = 1 + ((state >> 20) % 120);
        out.push(PackedNote {
            start_tick: start as u32,
            end_tick: (start + dur) as u32,
            key: (i % 128) as u8,
            velocity: 90,
            release_velocity: 0,
            channel: (i % 16) as u8,
            track: 0,
        });
    }
    out
}

/// 取「音符数最多」的轨（流式，只保留当前最大轨，内存 ~单轨）。
pub fn largest_track(bytes: &[u8]) -> Result<Vec<PackedNote>, String> {
    let mut best: Vec<PackedNote> = Vec::new();
    let mut best_idx = 0usize;
    extract_all_events_per_track_streaming_from_bytes(bytes, |idx: usize, ev: TrackAllEvents| {
        if ev.notes.len() > best.len() {
            best = ev.notes;
            best_idx = idx;
        }
    })
    .map_err(|e| format!("解析失败: {e:?}"))?;
    eprintln!("[sort-proto] 选中 track {best_idx}，notes={}", best.len());
    Ok(best)
}

/// 单变体计时统计。
pub struct Timing {
    pub label: &'static str,
    pub min_ms: f64,
    pub med_ms: f64,
    pub scratch_bytes: usize,
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = v.len() / 2;
    if v.len() % 2 == 1 {
        v[mid]
    } else {
        (v[mid - 1] + v[mid]) / 2.0
    }
}

/// 对 `pristine` 的副本跑 `runs` 次（预热 1 次丢弃），返回 min/median。
pub fn bench_variant(
    label: &'static str,
    pristine: &[PackedNote],
    runs: usize,
    mut run: impl FnMut(Vec<PackedNote>) -> usize,
) -> Timing {
    let mut walls = Vec::with_capacity(runs);
    let mut scratch = 0usize;
    // 预热
    let _ = run(pristine.to_vec());
    for _ in 0..runs {
        let data = pristine.to_vec();
        let t0 = Instant::now();
        scratch = run(data);
        walls.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    Timing {
        label,
        min_ms: walls.iter().copied().fold(f64::INFINITY, f64::min),
        med_ms: median(&mut walls),
        scratch_bytes: scratch,
    }
}

/// 正确性对拍：候选输出必须与稳定排序**逐元素一致**（radix 也应稳定）。
pub fn verify_stable(
    pristine: &[PackedNote],
    mut run: impl FnMut(Vec<PackedNote>) -> Vec<PackedNote>,
) -> bool {
    let mut reference = pristine.to_vec();
    sort_stable(&mut reference);
    run(pristine.to_vec()) == reference
}

/// 不稳定排序只需验证「有序」，同 tick 顺序允许变化。
pub fn verify_sorted(
    pristine: &[PackedNote],
    mut run: impl FnMut(Vec<PackedNote>) -> Vec<PackedNote>,
) -> bool {
    let out = run(pristine.to_vec());
    out.len() == pristine.len() && is_sorted(&out)
}
