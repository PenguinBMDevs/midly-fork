//! Windows 进程计量：CPU 时间（kernel+user）、峰值工作集、当前工作集。
//! 复用 benchmark crate 的 K32 模式，补 GetProcessTimes（指导书 P2 要求 CPU 时间口径）。

#[cfg(windows)]
mod imp {
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

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FileTime {
        lo: u32,
        hi: u32,
    }

    unsafe extern "system" {
        fn K32GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn GetProcessTimes(
            process: *mut core::ffi::c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }

    fn filetime_100ns(ft: FileTime) -> u64 {
        ((ft.hi as u64) << 32) | ft.lo as u64
    }

    /// 进程累计 CPU 时间（kernel+user），单位秒。
    pub fn cpu_time_secs() -> f64 {
        unsafe {
            let mut creation = FileTime::default();
            let mut exit = FileTime::default();
            let mut kernel = FileTime::default();
            let mut user = FileTime::default();
            let ok = GetProcessTimes(
                GetCurrentProcess(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            );
            if ok == 0 {
                return 0.0;
            }
            (filetime_100ns(kernel) + filetime_100ns(user)) as f64 / 10_000_000.0
        }
    }

    /// (峰值工作集字节, 当前工作集字节)。
    pub fn working_set() -> (u64, u64) {
        unsafe {
            let mut pmc: ProcessMemoryCounters = core::mem::zeroed();
            pmc.cb = core::mem::size_of::<ProcessMemoryCounters>() as u32;
            if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) != 0 {
                (
                    pmc.peak_working_set_size as u64,
                    pmc.working_set_size as u64,
                )
            } else {
                (0, 0)
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn cpu_time_secs() -> f64 {
        0.0
    }
    pub fn working_set() -> (u64, u64) {
        (0, 0)
    }
}

pub use imp::{cpu_time_secs, working_set};
