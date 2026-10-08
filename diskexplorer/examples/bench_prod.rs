//! Production benchmark: Nt walker vs jwalk with identical retained data
//!
//! Run with: cargo run --example bench_prod -- <path>
//! Each measured run is a separate child process for accurate peak working set measurement.
//! Side A: production Nt walker (scan_nt_full). Side B: jwalk building same records.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use jwalk::WalkDir;
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::GetCurrentProcess;

use diskexplorer::nt_walker::scan_nt_full;
use diskexplorer::{FileRecord, ScanData};

// Results from a single scan
#[derive(Debug, Clone)]
struct ScanResult {
    walker: String,
    run: u32,
    wall_ms: u64,
    file_count: u64,
    dir_count: u64,
    total_logical_bytes: u64,
    total_allocated_bytes: u64,
    peak_working_set_mb: f64,
    thread_count: usize,
}

// Parse scan result from stdout
fn parse_scan_result(stdout: &str, walker: &str) -> ScanResult {
    let mut result = ScanResult {
        walker: walker.to_string(),
        run: 0,
        wall_ms: 0,
        file_count: 0,
        dir_count: 0,
        total_logical_bytes: 0,
        total_allocated_bytes: 0,
        peak_working_set_mb: 0.0,
        thread_count: 0,
    };

    for line in stdout.lines() {
        if line.starts_with("RESULT,") {
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() >= 8 {
                result.wall_ms = parts[1].parse().unwrap_or(0);
                result.file_count = parts[2].parse().unwrap_or(0);
                result.dir_count = parts[3].parse().unwrap_or(0);
                result.total_logical_bytes = parts[4].parse().unwrap_or(0);
                result.total_allocated_bytes = parts[5].parse().unwrap_or(0);
                result.peak_working_set_mb = parts[6].parse().unwrap_or(0.0);
                result.thread_count = parts[7].parse().unwrap_or(0);
            }
            break;
        }
    }
    result
}

/// Get peak working set size in MB
fn get_peak_working_set_mb() -> f64 {
    let mut pmc = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };

    let result = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut pmc as *mut _ as *mut _,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    };

    if result.is_ok() {
        pmc.PeakWorkingSetSize as f64 / (1024.0 * 1024.0)
    } else {
        0.0
    }
}

/// Get thread count from env or default
fn get_thread_count() -> usize {
    std::env::var("DISKEXPLORER_NT_THREADS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0) // 0 = rayon default (logical cores)
}

/// Set env var for thread count (unsafe)
fn set_thread_count(threads: usize) {
    if threads > 0 {
        unsafe { std::env::set_var("DISKEXPLORER_NT_THREADS", threads.to_string()) };
    } else {
        unsafe { std::env::remove_var("DISKEXPLORER_NT_THREADS") };
    }
}

/// Run production Nt walker scan in child process
fn scan_nt_prod(root: &Path) -> ScanResult {
    let root_str = root.to_string_lossy().to_string();
    let exe = std::env::current_exe().unwrap();
    let thread_count = get_thread_count();
    let output = Command::new(&exe)
        .args(["--nt-prod", &root_str, &thread_count.to_string()])
        .output()
        .expect("Failed to run NT walker child process");

    if !output.status.success() {
        eprintln!("NT walker child failed: {}", String::from_utf8_lossy(&output.stderr));
        std::process::exit(1);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_scan_result(&stdout, "nt")
}

/// Run jwalk scan building identical retained data in child process
fn scan_jwalk_prod(root: &Path) -> ScanResult {
    let root_str = root.to_string_lossy().to_string();
    let exe = std::env::current_exe().unwrap();
    let output = Command::new(&exe)
        .args(["--jwalk-prod", &root_str])
        .output()
        .expect("Failed to run jwalk child process");

    if !output.status.success() {
        eprintln!("jwalk child failed: {}", String::from_utf8_lossy(&output.stderr));
        std::process::exit(1);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_scan_result(&stdout, "jwalk")
}

/// Child process entry point for production Nt walker
fn run_nt_prod_child(root: &Path, thread_count: usize) -> ScanResult {
    let start = Instant::now();

    // Use the production scan_nt_full function
    let scan_data = scan_nt_full(root).expect("scan_nt_full failed");

    let peak_mb = get_peak_working_set_mb();
    let actual_threads = if thread_count == 0 {
        num_cpus::get()
    } else {
        thread_count
    };

    ScanResult {
        walker: "nt".to_string(),
        run: 0,
        wall_ms: start.elapsed().as_millis() as u64,
        file_count: scan_data.file_count,
        dir_count: scan_data.dir_count,
        total_logical_bytes: scan_data.total_logical_bytes,
        total_allocated_bytes: scan_data.total_allocated_bytes,
        peak_working_set_mb: peak_mb,
        thread_count: actual_threads,
    }
}

/// Child process entry point for jwalk scan building same retained data
fn run_jwalk_prod_child(root: &Path) -> ScanResult {
    let start = Instant::now();
    let root = root.canonicalize().expect("canonicalize failed");

    let mut dir_sizes_logical = std::collections::HashMap::<PathBuf, u64>::new();
    let mut dir_sizes_allocated = std::collections::HashMap::<PathBuf, u64>::new();
    let mut files: Vec<FileRecord> = Vec::new();
    let mut file_count = 0u64;
    let mut dir_count = 0u64;
    let mut total_logical = 0u64;
    let mut total_allocated = 0u64;
    let mut unreadable_count = 0u64;
    let mut unreadable_bytes = 0u64;

    // To match NT walker's retained data structure, we need:
    // - file_id, volume_serial (set to 0 for jwalk)
    // - is_reparse, is_cloud (set to false for jwalk)
    // - reparse_tag (set to 0)

    for entry in WalkDir::new(&root).skip_hidden(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();

        if entry.file_type().is_dir() {
            dir_count += 1;
            dir_sizes_logical.entry(path.clone()).or_insert(0);
            dir_sizes_allocated.entry(path.clone()).or_insert(0);
        } else if entry.file_type().is_file() {
            file_count += 1;
            let (size, mtime) = entry
                .metadata()
                .map(|m| {
                    let mtime = m.modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    (m.len(), mtime)
                })
                .unwrap_or((0, 0));

            // Same record structure as NT walker
            files.push((
                path.clone(),
                size,          // logical_size
                size,          // allocated_size (jwalk doesn't distinguish)
                mtime,
                [0u8; 16],     // file_id
                0,             // volume_serial
                false,         // is_reparse
                false,         // is_cloud
                0,             // reparse_tag
            ));

            total_logical += size;
            total_allocated += size;

            // Add to all ancestors (same as NT walker)
            let mut ancestor = path.parent();
            while let Some(dir) = ancestor {
                *dir_sizes_logical.entry(dir.to_path_buf()).or_insert(0) += size;
                *dir_sizes_allocated.entry(dir.to_path_buf()).or_insert(0) += size;
                if dir == root {
                    break;
                }
                ancestor = dir.parent();
            }
        }
    }

    let peak_mb = get_peak_working_set_mb();

    ScanResult {
        walker: "jwalk".to_string(),
        run: 0,
        wall_ms: start.elapsed().as_millis() as u64,
        file_count,
        dir_count,
        total_logical_bytes: total_logical,
        total_allocated_bytes: total_allocated,
        peak_working_set_mb: peak_mb,
        thread_count: 1, // jwalk is single-threaded
    }
}

fn print_result(result: &ScanResult) {
    // Print in format: RESULT,wall_ms,file_count,dir_count,total_logical_bytes,total_allocated_bytes,peak_working_set_mb,thread_count
    println!(
        "RESULT,{},{},{},{},{},{:.1},{}",
        result.wall_ms,
        result.file_count,
        result.dir_count,
        result.total_logical_bytes,
        result.total_allocated_bytes,
        result.peak_working_set_mb,
        result.thread_count
    );
}

fn run_benchmark(root: &Path, runs: u32, thread_counts: &[usize]) -> Vec<ScanResult> {
    let mut all_results = Vec::new();

    println!("\n=== Benchmarking on {} ===", root.display());
    println!("Running {} iterations each...\n", runs);

    // Warm up
    println!("Warming up...");
    let _ = scan_nt_prod(root);
    let _ = scan_jwalk_prod(root);

    for &threads in thread_counts {
        // Set thread count for NT walker
        set_thread_count(threads);

        let thread_label = if threads == 0 { "default (rayon)".to_string() } else { threads.to_string() };
        println!("--- Thread count: {} ---", thread_label);

        for run in 1..=runs {
            println!("  Run {}...", run);

            // NT walker
            let nt_result = scan_nt_prod(root);
            println!(
                "    NT:    {}ms, {} files, {} dirs, {:.1} MB peak, {} threads",
                nt_result.wall_ms,
                nt_result.file_count,
                nt_result.dir_count,
                nt_result.peak_working_set_mb,
                nt_result.thread_count
            );

            // jwalk
            let jwalk_result = scan_jwalk_prod(root);
            println!(
                "    jwalk: {}ms, {} files, {} dirs, {:.1} MB peak, {} threads",
                jwalk_result.wall_ms,
                jwalk_result.file_count,
                jwalk_result.dir_count,
                jwalk_result.peak_working_set_mb,
                jwalk_result.thread_count
            );

            let mut nt_r = nt_result;
            nt_r.run = run;
            let mut jwalk_r = jwalk_result;
            jwalk_r.run = run;

            all_results.push(nt_r);
            all_results.push(jwalk_r);
        }
    }

    all_results
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Child process entry points
    if args.len() >= 2 && args[1] == "--nt-prod" {
        let root = PathBuf::from(&args[2]);
        let thread_count = args[3].parse().unwrap_or(0);
        let result = run_nt_prod_child(&root, thread_count);
        print_result(&result);
        return;
    }
    if args.len() >= 2 && args[1] == "--jwalk-prod" {
        let root = PathBuf::from(&args[2]);
        let result = run_jwalk_prod_child(&root);
        print_result(&result);
        return;
    }

    // Parent process
    let path = if args.len() > 1 {
        PathBuf::from(&args[1])
    } else {
        std::env::current_dir().unwrap()
    };

    // Test on C:\Windows and %USERPROFILE%
    let test_dirs: Vec<PathBuf> = vec![
        PathBuf::from(r"C:\Windows"),
        dirs::home_dir().unwrap(),
    ];

    let runs = 3;
    // Thread counts: 1, 4, and default (rayon)
    let thread_counts = vec![1, 4, 0];

    for dir in &test_dirs {
        if dir.exists() {
            let results = run_benchmark(dir, runs, &thread_counts);

            // Analyze results
            println!("\n=== SUMMARY FOR {} ===", dir.display());
            for walker in ["jwalk", "nt"] {
                for &threads in &thread_counts {
                    let walker_results: Vec<&ScanResult> =
                        results.iter().filter(|r| r.walker == walker && r.thread_count == (if threads == 0 { num_cpus::get() } else { threads })).collect();
                    if walker_results.is_empty() {
                        continue;
                    }

                    let cold = walker_results[0];
                    let warm: Vec<u64> = walker_results.iter().skip(1).map(|r| r.wall_ms).collect();
                    let median_warm = if warm.is_empty() {
                        cold.wall_ms
                    } else {
                        let mut sorted = warm.clone();
                        sorted.sort();
                        sorted[sorted.len() / 2]
                    };

                    let thread_label = if threads == 0 { "default".to_string() } else { threads.to_string() };
                    println!("{} ({})", walker.to_uppercase(), thread_label);
                    println!("  Cold (run 1):    {}ms", cold.wall_ms);
                    println!("  Warm median:     {}ms", median_warm);
                    println!("  Files:           {}", cold.file_count);
                    println!("  Dirs:            {}", cold.dir_count);
                    println!("  Logical bytes:   {}", cold.total_logical_bytes);
                    println!("  Allocated bytes: {}", cold.total_allocated_bytes);
                    println!("  Peak working set: {:.1} MB", cold.peak_working_set_mb);
                    println!("  Threads:         {}", cold.thread_count);
                }
            }

            // H16: production Nt, default threads, warm median >= 1.5x faster than jwalk-equivalent (warm/warm)
            let nt_default_results: Vec<&ScanResult> = results.iter().filter(|r| r.walker == "nt" && r.thread_count == num_cpus::get()).collect();
            let jwalk_results: Vec<&ScanResult> = results.iter().filter(|r| r.walker == "jwalk").collect();

            if !nt_default_results.is_empty() && !jwalk_results.is_empty() {
                let nt_cold = nt_default_results[0];
                let jwalk_cold = jwalk_results[0];

                let nt_warm: Vec<u64> = nt_default_results.iter().skip(1).map(|r| r.wall_ms).collect();
                let jwalk_warm: Vec<u64> = jwalk_results.iter().skip(1).map(|r| r.wall_ms).collect();

                let nt_median = if nt_warm.is_empty() { nt_cold.wall_ms } else {
                    let mut s = nt_warm.clone(); s.sort(); s[s.len()/2]
                };
                let jwalk_median = if jwalk_warm.is_empty() { jwalk_cold.wall_ms } else {
                    let mut s = jwalk_warm.clone(); s.sort(); s[s.len()/2]
                };

                let speedup = jwalk_median as f64 / nt_median as f64;

                println!("\n  H16 (NT default vs jwalk warm/warm):");
                println!("    NT warm median:    {}ms", nt_median);
                println!("    jwalk warm median: {}ms", jwalk_median);
                println!("    Speedup:           {:.2}x", speedup);
                println!("    PASS (>=1.5x):     {}", if speedup >= 1.5 { "YES" } else { "NO" });

                // Parity check
                let files_match = nt_cold.file_count == jwalk_cold.file_count;
                let logical_match = nt_cold.total_logical_bytes == jwalk_cold.total_logical_bytes;
                println!("    File count match:  {}", files_match);
                println!("    Logical bytes match: {}", logical_match);
                if !logical_match {
                    let diff = (nt_cold.total_logical_bytes as i128 - jwalk_cold.total_logical_bytes as i128).abs();
                    println!("    Diff: {} bytes", diff);
                }

                // H17: Nt walker scales: default-thread warm median <= 0.6x of 1-thread warm median
                let nt_1thread_results: Vec<&ScanResult> = results.iter().filter(|r| r.walker == "nt" && r.thread_count == 1).collect();
                let nt_default_results: Vec<&ScanResult> = results.iter().filter(|r| r.walker == "nt" && r.thread_count == num_cpus::get()).collect();

                if !nt_1thread_results.is_empty() && !nt_default_results.is_empty() {
                    let nt_1thread_warm: Vec<u64> = nt_1thread_results.iter().skip(1).map(|r| r.wall_ms).collect();
                    let nt_default_warm: Vec<u64> = nt_default_results.iter().skip(1).map(|r| r.wall_ms).collect();

                    let nt_1thread_median = if nt_1thread_warm.is_empty() { nt_1thread_results[0].wall_ms } else {
                        let mut s = nt_1thread_warm.clone(); s.sort(); s[s.len()/2]
                    };
                    let nt_default_median = if nt_default_warm.is_empty() { nt_default_results[0].wall_ms } else {
                        let mut s = nt_default_warm.clone(); s.sort(); s[s.len()/2]
                    };

                    let scale_ratio = nt_default_median as f64 / nt_1thread_median as f64;

                    println!("\n  H17 (NT scaling default/1-thread):");
                    println!("    NT 1-thread warm median:    {}ms", nt_1thread_median);
                    println!("    NT default warm median:     {}ms", nt_default_median);
                    println!("    Ratio (default/1-thread):   {:.2}x", scale_ratio);
                    println!("    PASS (<=0.6x):              {}", if scale_ratio <= 0.6 { "YES" } else { "NO" });

                    if scale_ratio > 0.6 {
                        println!("    WARNING: Poor scaling. Profile per-directory open time vs enumeration vs record-processing.");
                    }
                }
            }
        }
    }
}