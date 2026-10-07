//! Benchmark harness: Nt walker vs jwalk
//!
//! Run with: cargo run --example bench_scan -- <path>
//! Compares jwalk (current) vs NtQueryDirectoryFileEx walker.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use jwalk::WalkDir;
use windows::Wdk::Storage::FileSystem::{
    FILE_ID_EXTD_DIR_INFORMATION, FILE_INFORMATION_CLASS, FileIdExtdDirectoryInformation,
    NtQueryDirectoryFileEx,
};
use windows::Win32::Foundation::{
    CloseHandle, GENERIC_READ, HANDLE, NTSTATUS, STATUS_NO_MORE_FILES,
};
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
}

// Entry data collected by Nt walker
#[derive(Debug, Clone)]
struct NtEntry {
    path: PathBuf,
    is_dir: bool,
    logical_size: u64,
    allocated_size: u64,
    file_id: [u8; 16],
    attributes: u32,
    reparse_tag: u32,
    mtime: i64,
}

// Run jwalk scan
fn scan_jwalk(root: &Path) -> ScanResult {
    let start = Instant::now();
    let mut file_count = 0u64;
    let mut dir_count = 0u64;
    let mut total_logical = 0u64;

    for entry in WalkDir::new(root).skip_hidden(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if entry.file_type().is_dir() {
            dir_count += 1;
        } else if entry.file_type().is_file() {
            file_count += 1;
            if let Ok(meta) = entry.metadata() {
                total_logical += meta.len();
            }
        }
    }

    ScanResult {
        walker: "jwalk".to_string(),
        run: 0,
        wall_ms: start.elapsed().as_millis() as u64,
        file_count,
        dir_count,
        total_logical_bytes: total_logical,
    }
}

// Run Nt walker scan
fn scan_nt(root: &Path) -> ScanResult {
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

    ScanResult {
        walker: "nt".to_string(),
        run: 0,
        wall_ms: start.elapsed().as_millis() as u64,
        file_count,
        dir_count,
        total_logical_bytes: total_logical,
    }
}

// Read a single directory using NtQueryDirectoryFileEx
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

        if status == NTSTATUS(STATUS_NO_MORE_FILES.0 as i32) {
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

                    // EndOfFile and AllocationSize are i64 (LARGE_INTEGER)
                    let logical_size = info.EndOfFile as u64;
                    let allocated_size = info.AllocationSize as u64;

                    // File ID is 128-bit
                    let mut file_id = [0u8; 16];
                    file_id.copy_from_slice(&info.FileId.Identifier);

                    // Mtime from ChangeTime (100ns intervals since 1601)
                    let mtime_100ns = info.ChangeTime as u64;
                    // Convert to Unix epoch (seconds)
                    const WINDOWS_TICKS_PER_SEC: u64 = 10_000_000;
                    const WINDOWS_TO_UNIX_EPOCH: u64 = 11_644_473_600_000_000; // 100ns intervals from 1601 to 1970
                    let mtime_unix = if mtime_100ns > WINDOWS_TO_UNIX_EPOCH {
                        ((mtime_100ns - WINDOWS_TO_UNIX_EPOCH) / WINDOWS_TICKS_PER_SEC) as i64
                    } else {
                        0
                    };

                    results.push(NtEntry {
                        path,
                        is_dir,
                        logical_size,
                        allocated_size,
                        file_id,
                        attributes: info.FileAttributes,
                        reparse_tag: info.ReparsePointTag,
                        mtime: mtime_unix,
                    });
                }
            }

            if info.NextEntryOffset == 0 {
                break;
            }
            offset += info.NextEntryOffset as usize;
        }
    }

    unsafe { CloseHandle(dir_handle) };
    results
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
