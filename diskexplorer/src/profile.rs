//! Profiling instrumentation for the scanner
//!
//! Enable with DISKEXPLORER_PROFILE=1 env var. Zero overhead when disabled.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Default)]
pub struct ScanProfile {
    // Pass 1: single-threaded directory collection
    pub pass1_enumeration_ns: AtomicU64,
    pub pass1_dir_count: AtomicU64,
    pub pass1_file_count: AtomicU64,

    // Pass 2: parallel re-enumeration
    pub pass2_enumeration_ns: AtomicU64,
    pub pass2_dir_count: AtomicU64,
    pub pass2_file_count: AtomicU64,

    // Per-file processing
    pub per_file_processing_ns: AtomicU64,
    pub file_processed_count: AtomicU64,

    // Mutex contention
    pub mutex_wait_files_ns: AtomicU64,
    pub mutex_wait_dir_sizes_ns: AtomicU64,
    pub mutex_wait_hardlink_map_ns: AtomicU64,

    // Path operations
    pub path_clone_count: AtomicU64,
    pub pathbuf_alloc_count: AtomicU64,
    pub ancestor_walk_count: AtomicU64,

    // Channel operations
    pub channel_send_ns: AtomicU64,
    pub channel_batch_count: AtomicU64,

    // Consumer (App::poll_scan)
    pub consumer_drain_ns: AtomicU64,
    pub consumer_batch_count: AtomicU64,

    // Total wall time
    pub total_wall_ns: AtomicU64,
}

impl ScanProfile {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enabled() -> bool {
        std::env::var("DISKEXPLORER_PROFILE").is_ok()
    }

    pub fn print_summary(&self) {
        if !Self::enabled() { return; }
        println!("\n=== SCAN PROFILE ===");
        println!("Pass 1 (serial enumeration): {:.3}s ({} dirs, {} files)",
            self.pass1_enumeration_ns.load(Ordering::Relaxed) as f64 / 1e9,
            self.pass1_dir_count.load(Ordering::Relaxed),
            self.pass1_file_count.load(Ordering::Relaxed));
        println!("Pass 2 (parallel enumeration): {:.3}s ({} dirs, {} files)",
            self.pass2_enumeration_ns.load(Ordering::Relaxed) as f64 / 1e9,
            self.pass2_dir_count.load(Ordering::Relaxed),
            self.pass2_file_count.load(Ordering::Relaxed));
        println!("Per-file processing: {:.3}s ({} files)",
            self.per_file_processing_ns.load(Ordering::Relaxed) as f64 / 1e9,
            self.file_processed_count.load(Ordering::Relaxed));
        println!("Mutex wait - files: {:.3}s, dir_sizes: {:.3}s, hardlink_map: {:.3}s",
            self.mutex_wait_files_ns.load(Ordering::Relaxed) as f64 / 1e9,
            self.mutex_wait_dir_sizes_ns.load(Ordering::Relaxed) as f64 / 1e9,
            self.mutex_wait_hardlink_map_ns.load(Ordering::Relaxed) as f64 / 1e9);
        println!("Path ops - clones: {}, allocs: {}, ancestor walks: {}",
            self.path_clone_count.load(Ordering::Relaxed),
            self.pathbuf_alloc_count.load(Ordering::Relaxed),
            self.ancestor_walk_count.load(Ordering::Relaxed));
        println!("Channel - send time: {:.3}s, batches: {}",
            self.channel_send_ns.load(Ordering::Relaxed) as f64 / 1e9,
            self.channel_batch_count.load(Ordering::Relaxed));
        println!("Consumer drain: {:.3}s, batches: {}",
            self.consumer_drain_ns.load(Ordering::Relaxed) as f64 / 1e9,
            self.consumer_batch_count.load(Ordering::Relaxed));
        println!("Total wall: {:.3}s",
            self.total_wall_ns.load(Ordering::Relaxed) as f64 / 1e9);
    }
}

// Global profile instance
static SCAN_PROFILE: std::sync::OnceLock<ScanProfile> = std::sync::OnceLock::new();

pub fn profile() -> &'static ScanProfile {
    SCAN_PROFILE.get_or_init(ScanProfile::new)
}

// Timing helpers
pub struct Timer {
    start: Instant,
    counter: &'static AtomicU64,
}

impl Timer {
    pub fn new(counter: &'static AtomicU64) -> Self {
        Self { start: Instant::now(), counter }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed().as_nanos() as u64;
        self.counter.fetch_add(elapsed, Ordering::Relaxed);
    }
}

// Macros for zero-cost when profiling disabled
#[macro_export]
macro_rules! profile_time {
    ($counter:expr, $code:block) => {
        if $crate::profile::ScanProfile::enabled() {
            let _t = $crate::profile::Timer::new($counter);
            $code
        } else {
            $code
        }
    };
}

#[macro_export]
macro_rules! profile_count {
    ($counter:expr, $inc:expr) => {
        if $crate::profile::ScanProfile::enabled() {
            $counter.fetch_add($inc, std::sync::atomic::Ordering::Relaxed);
        }
    };
}