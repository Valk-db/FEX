//! Profile scan - runs scan_nt_full directly with profiling enabled
//!
//! Run with: DISKEXPLORER_PROFILE=1 cargo run --release --example profile_scan -- <path>

use std::path::PathBuf;
use diskexplorer::nt_walker::scan_nt_full;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = if args.len() > 1 {
        PathBuf::from(&args[1])
    } else {
        std::env::current_dir().unwrap()
    };

    println!("Profiling scan on: {}", path.display());
    let scan_data = scan_nt_full(&path).expect("scan_nt_full failed");
    
    println!("\nScan results:");
    println!("  Files: {}", scan_data.file_count);
    println!("  Dirs: {}", scan_data.dir_count);
    println!("  Logical bytes: {}", scan_data.total_logical_bytes);
    println!("  Allocated bytes: {}", scan_data.total_allocated_bytes);
    println!("  Hardlink siblings: {}", scan_data.hardlink_siblings);
    println!("  Reparse skipped: {}", scan_data.reparse_skipped);
    println!("  Cloud skipped: {}", scan_data.cloud_skipped);
    println!("  Unreadable dirs: {}", scan_data.unreadable_dirs);
    println!("  Unreadable files: {}", scan_data.unreadable_files);
}
