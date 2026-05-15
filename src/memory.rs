//! Memory usage reporting helpers used for debugging/resource tracing.
use std::time::Duration;

#[cfg(feature = "memory-report")]
mod imp {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread::{self, JoinHandle};

    use sysinfo::{ProcessExt, System, SystemExt, get_current_pid};

    pub struct MemoryReporter {
        stop_flag: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl MemoryReporter {
        /// Start printing memory usage with the provided label every `interval`.
        pub fn start(label: impl Into<String>, interval: Duration) -> MemoryReporter {
            let label = label.into();
            let stop_flag = Arc::new(AtomicBool::new(false));
            let stop_flag_clone = stop_flag.clone();
            let handle = thread::spawn(move || {
                let pid = match get_current_pid() {
                    Ok(pid) => pid,
                    Err(_) => {
                        eprintln!(
                            "[midly memory] {}: failed to get PID, stopping reporter",
                            label
                        );
                        return;
                    }
                };
                let mut sys = System::new_all();

                while !stop_flag_clone.load(Ordering::Relaxed) {
                    sys.refresh_all();
                    if let Some(process) = sys.process(pid) {
                        let rss_mb = process.memory() / 1024 / 1024;
                        let virt_mb = process.virtual_memory() / 1024 / 1024;
                        eprintln!(
                            "[midly memory] {}: rss={} MB, virtual={} MB",
                            label, rss_mb, virt_mb
                        );
                    } else {
                        eprintln!("[midly memory] {}: process info unavailable", label);
                    }
                    thread::sleep(interval);
                }
            });

            MemoryReporter {
                stop_flag,
                handle: Some(handle),
            }
        }

        /// Stop the reporter (will also happen automatically when dropped).
        pub fn stop(&mut self) {
            self.stop_flag.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    impl Drop for MemoryReporter {
        fn drop(&mut self) {
            self.stop();
        }
    }

    pub fn print_memory_usage(label: &str) {
        let pid = match get_current_pid() {
            Ok(pid) => pid,
            Err(_) => {
                eprintln!("[midly memory] {}: failed to get PID", label);
                return;
            }
        };
        let mut sys = System::new_all();
        sys.refresh_all();
        if let Some(process) = sys.process(pid) {
            let rss_mb = process.memory() / 1024 / 1024;
            let virt_mb = process.virtual_memory() / 1024 / 1024;
            eprintln!(
                "[midly memory] {}: rss={} MB, virtual={} MB",
                label, rss_mb, virt_mb
            );
        } else {
            eprintln!("[midly memory] {}: process info unavailable", label);
        }
    }
}

#[cfg(not(feature = "memory-report"))]
mod imp {
    use super::*;

    /// Stub reporter when `memory-report` is disabled.
    pub struct MemoryReporter;

    impl MemoryReporter {
        /// No-op start when memory reporting is disabled.
        pub fn start<T>(_label: T, _interval: Duration) -> Self {
            MemoryReporter
        }

        /// No-op stop when memory reporting is disabled.
        pub fn stop(&mut self) {}
    }

    /// No-op memory usage printing when the feature is disabled.
    pub fn print_memory_usage(_: &str) {}
}

pub use imp::{MemoryReporter, print_memory_usage};
