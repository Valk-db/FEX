//! Benchmark harness: Nt walker vs jwalk
//!
//! Run with: cargo run --example bench_scan -- <path>
//! Compares jwalk (current) vs NtQueryDirectoryFileEx walker.
//! Each walker runs in a separate process for accurate peak working set measurement.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use jwalk::WalkDir;
use windows::Wdk::Storage::FileSystem::{
    FILE_ID_EXTD_DIR_INFORMATION, FILE_INFORMATION_CLASS, FileIdExtdDirectoryInformation,
    NtQueryDirectoryFileEx,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, NTSTATUS, STATUS_NO_MORE_FILES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;
use windows::core::PCWSTR;

// Results from a single scan
#[derive(Debug, Clone)]
struct ScanResult {
    walker: String,
    run: u32,
    wall_ms: u64,
    file_count: u64,
    dir_count: u64,
    total_logical_bytes: u64,
    peak_working_set_mb: f64,
    bytes_per_file: f64,
}

// Entry data collected by Nt walker (used in child process)
#[derive(Debug, Clone)]
struct NtEntry {
    path: PathBuf,
    is_dir: bool,
    logical_size: u64,
}

// Run jwalk scan in a separate process for accurate memory measurement
fn scan_jwalk(root: &Path) -> ScanResult {
    let root_str = root.to_string_lossy().to_string();
    let exe = std::env::current_exe().unwrap();
    let output = Command::new(&exe)
        .args(["--jwalk", &root_str])
        .output()
        .expect("Failed to run jwalk child process");

    if !output.status.success() {
        eprintln!("jwalk child failed: {}", String::from_utf8_lossy(&output.stderr));
        std::process::exit(1);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_scan_result(&stdout, "jwalk")
}

// Run Nt walker scan in a separate process for accurate memory measurement
fn scan_nt(root: &Path) -> ScanResult {
    let root_str = root.to_string_lossy().to_string();
    let exe = std::env::current_exe().unwrap();
    let output = Command::new(&exe)
        .args(["--nt", &root_str])
        .output()
        .expect("Failed to run NT walker child process");

    if !output.status.success() {
        eprintln!("NT walker child failed: {}", String::from_utf8_lossy(&output.stderr));
        std::process::exit(1);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_scan_result(&stdout, "nt")
}

fn parse_scan_result(stdout: &str, walker: &str) -> ScanResult {
    let mut result = ScanResult {
        walker: walker.to_string(),
        run: 0,
        wall_ms: 0,
        file_count: 0,
        dir_count: 0,
        total_logical_bytes: 0,
        peak_working_set_mb: 0.0,
        bytes_per_file: 0.0,
    };

    for line in stdout.lines() {
        if line.starts_with("RESULT,") {
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() >= 6 {
                result.wall_ms = parts[1].parse().unwrap_or(0);
                result.file_count = parts[2].parse().unwrap_or(0);
                result.dir_count = parts[3].parse().unwrap_or(0);
                result.total_logical_bytes = parts[4].parse().unwrap_or(0);
                result.peak_working_set_mb = parts[5].parse().unwrap_or(0.0);
                if parts.len() > 6 {
                    result.bytes_per_file = parts[6].parse().unwrap_or(0.0);
                }
            }
            break; // Found it, stop parsing
        }
    }
    result
}

// Read a single directory using NtQueryDirectoryFileEx (used by NT walker child process)
fn read_dir_nt(dir: &Path) -> Vec<NtEntry> {
    let mut results = Vec::new();

    let dir_wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let dir_handle = unsafe {
        CreateFileW(
            PCWSTR(dir_wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    };

    let Ok(dir_handle) = dir_handle else {
        return results;
    };

    // Buffer for directory entries
    let mut buffer = vec![0u8; 64 * 1024];
    let mut iosb = IO_STATUS_BLOCK::default();

    loop {
        let status = unsafe {
            NtQueryDirectoryFileEx(
                dir_handle,
                None,
                None,
                None,
                &mut iosb,
                buffer.as_mut_ptr() as *mut _,
                buffer.len() as u32,
                FILE_INFORMATION_CLASS(FileIdExtdDirectoryInformation.0),
                0,
                None,
            )
        };

        if status == STATUS_NO_MORE_FILES {
            break;
        }
        if status != NTSTATUS(0) {
            break;
        }

        // Parse the buffer - FILE_ID_EXTD_DIR_INFORMATION structures
        let mut offset = 0;
        while offset < buffer.len() {
            let info =
                unsafe { &*(buffer.as_ptr().add(offset) as *const FILE_ID_EXTD_DIR_INFORMATION) };

            // Skip . and ..
            let name_len = info.FileNameLength as usize / 2;
            if name_len > 0 {
                let name_slice =
                    unsafe { std::slice::from_raw_parts(info.FileName.as_ptr(), name_len) };
                let name = OsString::from_wide(name_slice);
                let name_str = name.to_string_lossy();

                if name_str != "." && name_str != ".." {
                    let mut path = dir.to_path_buf();
                    path.push(&name);

                    let is_dir = (info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0) != 0;

                    // EndOfFile is i64 (LARGE_INTEGER)
                    let logical_size = info.EndOfFile as u64;

                    results.push(NtEntry {
                        path,
                        is_dir,
                        logical_size,
                    });
                }
            }

            if info.NextEntryOffset == 0 {
                break;
            }
            offset += info.NextEntryOffset as usize;
        }
    }

    let _ = unsafe { CloseHandle(dir_handle) };
    results
}

// Child process entry point for jwalk scan
fn run_jwalk_child(root: &Path) -> ScanResult {
    let start = Instant::now();
    let mut file_count = 0u64;
    let mut dir_count = 0u64;
    let mut total_logical = 0u64;

    for entry in WalkDir::new(root).skip_hidden(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().is_dir() {
            dir_count += 1;
        } else if entry.file_type().is_file() {
            file_count += 1;
            if let Ok(meta) = entry.metadata() {
                total_logical += meta.len();
            }
        }
    }

    let peak_mb = get_peak_working_set_mb();
    let bytes_per_file = if file_count > 0 { total_logical as f64 / file_count as f64 } else { 0.0 };

    ScanResult {
        walker: "jwalk".to_string(),
        run: 0,
        wall_ms: start.elapsed().as_millis() as u64,
        file_count,
        dir_count,
        total_logical_bytes: total_logical,
        peak_working_set_mb: peak_mb,
        bytes_per_file,
    }
}

// Child process entry point for NT walker scan
fn run_nt_child(root: &Path) -> ScanResult {
    let start = Instant::now();
    let mut file_count = 0u64;
    let mut dir_count = 0u64;
    let mut total_logical = 0u64;

    // Use a work queue for parallel directory traversal
    let mut dirs_to_process: Vec<PathBuf> = vec![root.to_path_buf()];

    while let Some(dir) = dirs_to_process.pop() {
        let entries = read_dir_nt(&dir);
        for entry in entries {
            if entry.is_dir {
                dirs_to_process.push(entry.path.clone());
                dir_count += 1;
            } else {
                file_count += 1;
                total_logical += entry.logical_size;
            }
        }
    }

    let peak_mb = get_peak_working_set_mb();
    let bytes_per_file = if file_count > 0 { total_logical as f64 / file_count as f64 } else { 0.0 };

    ScanResult {
        walker: "nt".to_string(),
        run: 0,
        wall_ms: start.elapsed().as_millis() as u64,
        file_count,
        dir_count,
        total_logical_bytes: total_logical,
        peak_working_set_mb: peak_mb,
        bytes_per_file,
    }
}

/// Get peak working set size in MB
fn get_peak_working_set_mb() -> f64 {
    let mut pmc = windows::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<windows::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };

    let result = unsafe {
        windows::Win32::System::ProcessStatus::GetProcessMemoryInfo(
            windows::Win32::System::Threading::GetCurrentProcess(),
            &mut pmc as *mut _ as *mut _,
            std::mem::size_of::<windows::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    };

    if result.is_ok() {
        pmc.PeakWorkingSetSize as f64 / (1024.0 * 1024.0)
    } else {
        0.0
    }
}

fn run_benchmark(root: &Path, runs: u32) -> Vec<ScanResult> {
    let mut all_results = Vec::new();

    println!("\n=== Benchmarking on {} ===", root.display());
    println!("Running {} iterations each...\n", runs);

    // Warm up
    println!("Warming up...");
    let _ = scan_jwalk(root);
    let _ = scan_nt(root);

    for run in 1..=runs {
        println!("--- Run {} ---", run);

        // jwalk
        let jwalk_result = scan_jwalk(root);
        println!(
            "  jwalk: {}ms, {} files, {} dirs, {} bytes",
            jwalk_result.wall_ms,
            jwalk_result.file_count,
            jwalk_result.dir_count,
            jwalk_result.total_logical_bytes
        );

        // Nt walker
        let nt_result = scan_nt(root);
        println!(
            "  nt:    {}ms, {} files, {} dirs, {} bytes",
            nt_result.wall_ms,
            nt_result.file_count,
            nt_result.dir_count,
            nt_result.total_logical_bytes
        );

        let mut jwalk_r = jwalk_result;
        jwalk_r.run = run;
        let mut nt_r = nt_result;
        nt_r.run = run;

        all_results.push(jwalk_r);
        all_results.push(nt_r);
    }

    all_results
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Child process entry points
    if args.len() >= 2 && args[1] == "--jwalk" {
        let root = PathBuf::from(&args[2]);
        let result = run_jwalk_child(&root);
        print_result(&result);
        return;
    }
    if args.len() >= 2 && args[1] == "--nt" {
        let root = PathBuf::from(&args[2]);
        let result = run_nt_child(&root);
        print_result(&result);
        return;
    }

    // Parent process
    let path = if args.len() > 1 {
        PathBuf::from(&args[1])
    } else {
        std::env::current_dir().unwrap()
    };

    let runs = 3;
    let results = run_benchmark(&path, runs);

    // Analyze results
    println!("\n=== SUMMARY ===");
    for walker in ["jwalk", "nt"] {
        let walker_results: Vec<&ScanResult> =
            results.iter().filter(|r| r.walker == walker).collect();
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

        println!("{}", walker.to_uppercase());
        println!("  Cold (run 1):    {}ms", cold.wall_ms);
        println!("  Warm median:     {}ms", median_warm);
        println!("  Files:           {}", cold.file_count);
        println!("  Dirs:            {}", cold.dir_count);
        println!("  Logical bytes:   {}", cold.total_logical_bytes);
        println!("  Peak working set: {:.1} MB", cold.peak_working_set_mb);
        println!("  Bytes/file (avg file size): {:.1}", cold.bytes_per_file);

        if walker == "jwalk" {
            // Check if NT walker matches
            let nt_results: Vec<&ScanResult> =
                results.iter().filter(|r| r.walker == "nt").collect();
            if !nt_results.is_empty() {
                let nt_cold = nt_results[0];
                let nt_warm: Vec<u64> = nt_results.iter().skip(1).map(|r| r.wall_ms).collect();
                let nt_median = if nt_warm.is_empty() {
                    nt_cold.wall_ms
                } else {
                    let mut sorted = nt_warm.clone();
                    sorted.sort();
                    sorted[sorted.len() / 2]
                };

                let speedup = cold.wall_ms as f64 / nt_median as f64;
                println!("\n  Speedup (jwalk cold / nt warm median): {:.2}x", speedup);

                let files_match = cold.file_count == nt_cold.file_count;
                let bytes_match = cold.total_logical_bytes == nt_cold.total_logical_bytes;

                println!("  File count match: {}", files_match);
                println!("  Logical bytes match: {}", bytes_match);

                // H1 verification
                let h1_speed = speedup >= 1.5;
                let h1_counts = files_match && bytes_match;
                println!(
                    "\n  H1 (Nt ≥1.5x faster AND counts match): {}",
                    if h1_speed && h1_counts {
                        "PASS"
                    } else {
                        "FAIL"
                    }
                );
                println!(
                    "    Speed condition (≥1.5x): {}",
                    if h1_speed { "PASS" } else { "FAIL" }
                );
                println!(
                    "    Count match condition:    {}",
                    if h1_counts { "PASS" } else { "FAIL" }
                );
            }
        }
    }
}

fn print_result(result: &ScanResult) {
    // Print in format: RESULT,wall_ms,file_count,dir_count,total_logical_bytes,peak_working_set_mb,bytes_per_file
    println!(
        "RESULT,{},{},{},{},{:.1},{:.1}",
        result.wall_ms,
        result.file_count,
        result.dir_count,
        result.total_logical_bytes,
        result.peak_working_set_mb,
        result.bytes_per_file
    );
}
