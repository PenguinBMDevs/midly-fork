use std::{env, path::Path, time::Instant};

const DEFAULT_MIDI_PATH: &str = "test-asset/Funky Stars Black Redone.mid";

/// Lightweight process memory info using Windows API directly.
/// Returns (working_set_bytes, pagefile_usage_bytes).
/// - working_set = physical memory (RSS)
/// - pagefile_usage = committed private virtual memory
#[cfg(windows)]
fn get_memory_info() -> (u64, u64) {
    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    extern "system" {
        fn K32GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            ppsmemcounters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
    }

    unsafe {
        let mut pmc: ProcessMemoryCounters = core::mem::zeroed();
        pmc.cb = core::mem::size_of::<ProcessMemoryCounters>() as u32;
        let handle = GetCurrentProcess();
        if K32GetProcessMemoryInfo(handle, &mut pmc, pmc.cb) != 0 {
            (pmc.working_set_size as u64, pmc.pagefile_usage as u64)
        } else {
            (0, 0)
        }
    }
}

#[cfg(not(windows))]
fn get_memory_info() -> (u64, u64) {
    (0, 0)
}

fn print_memory(label: &str) {
    let (rss, virt) = get_memory_info();
    let rss_kb = rss / 1024;
    let virt_kb = virt / 1024;
    let total_mb = (rss + virt) as f64 / (1024.0 * 1024.0);
    eprintln!(
        "  [mem] {}: rss={} KB, virtual={} KB, total={:.2} MB",
        label, rss_kb, virt_kb, total_mb
    );
}

fn main() {
    // Collect all args after the program name and join with spaces. This lets users
    // pass an unquoted path that contains spaces (e.g. `benchmark path to midi file.mid`).
    let override_args: Vec<String> = env::args().skip(1).collect();
    let midi_path_owned: Option<String> = if override_args.is_empty() {
        None
    } else {
        Some(override_args.join(" "))
    };
    let midi_path = midi_path_owned.as_deref().unwrap_or(DEFAULT_MIDI_PATH);
    let path = Path::new(midi_path);

    if !path.exists() {
        eprintln!("ERROR: MIDI file not found: {}", path.display());
        eprintln!("Usage: benchmark [path_to_midi_file]");
        std::process::exit(1);
    }

    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    eprintln!("=== MIDI Benchmark ===");
    eprintln!(
        "File: {} ({:.1} MB)",
        path.display(),
        file_size as f64 / (1024.0 * 1024.0)
    );
    eprintln!();

    print_memory("startup");

    // --- Sequential scan: bounded memory, fast ---
    eprintln!("--- scan_midi_file (sequential, bounded memory) ---");
    let start = Instant::now();
    match midly::scan_midi_file(path) {
        Ok(result) => {
            let elapsed = start.elapsed();
            let ms = elapsed.as_secs_f64() * 1000.0;
            eprintln!("  Tracks:        {}", result.track_count);
            eprintln!("  Notes:         {}", result.note_count);
            eprintln!("  Tempo changes: {}", result.tempo_changes.len());
            eprintln!("  Max tick:      {}", result.max_tick);
            eprintln!("  Division:      {}", result.division);
            eprintln!("  Time:          {:.2} ms", ms);

            print_memory("after scan");

            let (rss, virt) = get_memory_info();
            let total_mb = (rss + virt) as f64 / (1024.0 * 1024.0);

            eprintln!();
            eprintln!("=== Result ===");
            if ms < 3000.0 {
                eprintln!("  Speed:  PASS ({:.2} ms < 3000 ms)", ms);
            } else {
                eprintln!("  Speed:  FAIL ({:.2} ms >= 3000 ms)", ms);
            }
            if total_mb < 30.0 {
                eprintln!("  Memory: PASS ({:.2} MB < 30 MB)", total_mb);
            } else {
                eprintln!("  Memory: FAIL ({:.2} MB >= 30 MB)", total_mb);
            }
        }
        Err(e) => {
            eprintln!("  FAILED: {}", e);
        }
    }

    print_memory("end");
}
