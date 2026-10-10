//! Parity diff tool: compare Nt walker vs jwalk file listings
//!
//! Run with: cargo run --example parity_diff -- <path>
//! Dumps sorted (path, logical size) lists from both walkers and diffs them.

use std::path::PathBuf;
use std::process::Command;

use diskexplorer::nt_walker::scan_nt_full;
use jwalk::WalkDir;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = if args.len() > 1 {
        PathBuf::from(&args[1])
    } else {
        std::env::current_dir().unwrap()
    };

    println!("=== Parity Diff on {} ===\n", path.display());

    // Run Nt walker
    println!("Running Nt walker...");
    let nt_data = scan_nt_full(&path).expect("scan_nt_full failed");
    println!(
        "NT: {} files, {} dirs, {} logical bytes",
        nt_data.file_count, nt_data.dir_count, nt_data.total_logical_bytes
    );

    // Run jwalk
    println!("Running jwalk...");
    let mut jwalk_files: Vec<(PathBuf, u64)> = Vec::new();
    let root = path.canonicalize().expect("canonicalize failed");
    for entry in WalkDir::new(&root).skip_hidden(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().is_file()
            && let Ok(meta) = entry.metadata()
        {
            let size = meta.len();
            jwalk_files.push((entry.path(), size));
        }
    }
    let jwalk_count = jwalk_files.len();
    let jwalk_logical: u64 = jwalk_files.iter().map(|(_, s)| *s).sum();
    println!(
        "jwalk: {} files, {} logical bytes",
        jwalk_count, jwalk_logical
    );

    // Sort both lists by path
    let mut nt_list: Vec<(PathBuf, u64)> = nt_data
        .files
        .iter()
        .filter(|f| !f.is_reparse && !f.is_cloud)
        .map(|f| (f.path.clone(), f.logical_size))
        .collect();
    nt_list.sort_by(|a, b| a.0.cmp(&b.0));
    jwalk_files.sort_by(|a, b| a.0.cmp(&b.0));

    // Write to temp files for diff
    let nt_file = std::env::temp_dir().join("nt_parity.txt");
    let jwalk_file = std::env::temp_dir().join("jwalk_parity.txt");

    let nt_content = nt_list
        .iter()
        .map(|(p, s)| format!("{}\t{}", p.display(), s))
        .collect::<Vec<_>>()
        .join("\n");
    let jwalk_content = jwalk_files
        .iter()
        .map(|(p, s)| format!("{}\t{}", p.display(), s))
        .collect::<Vec<_>>()
        .join("\n");

    std::fs::write(&nt_file, nt_content).unwrap();
    std::fs::write(&jwalk_file, jwalk_content).unwrap();

    println!("\n--- Diffing ---");
    let output = Command::new("cmd")
        .args([
            "/C",
            "diff",
            "-u",
            nt_file.to_str().unwrap(),
            jwalk_file.to_str().unwrap(),
        ])
        .output()
        .expect("diff failed");

    let diff = String::from_utf8_lossy(&output.stdout);
    if diff.is_empty() {
        println!("NO DIFFERENCES - perfect parity!");
    } else {
        println!("{}", diff);
    }

    // Also show files only in NT
    println!("\n=== Files only in NT walker ===");
    let jwalk_set: std::collections::HashSet<_> = jwalk_files.iter().map(|(p, _)| p).collect();
    let mut nt_only = 0;
    for (p, s) in &nt_list {
        if !jwalk_set.contains(p) {
            println!("  NT-only: {} ({} bytes)", p.display(), s);
            nt_only += 1;
        }
    }
    if nt_only == 0 {
        println!("  (none)");
    }

    // Files only in jwalk
    println!("\n=== Files only in jwalk ===");
    let nt_set: std::collections::HashSet<_> = nt_list.iter().map(|(p, _)| p).collect();
    let mut jwalk_only = 0;
    for (p, s) in &jwalk_files {
        if !nt_set.contains(p) {
            println!("  jwalk-only: {} ({} bytes)", p.display(), s);
            jwalk_only += 1;
        }
    }
    if jwalk_only == 0 {
        println!("  (none)");
    }

    // Size mismatches for same paths
    println!("\n=== Size mismatches for same paths ===");
    let nt_map: std::collections::HashMap<_, _> = nt_list.iter().map(|(p, s)| (p, *s)).collect();
    let mut mismatches = 0;
    for (p, s) in &jwalk_files {
        if let Some(nt_s) = nt_map.get(p)
            && *nt_s != *s
        {
            println!("  MISMATCH: {} NT={} jwalk={}", p.display(), nt_s, s);
            mismatches += 1;
        }
    }
    if mismatches == 0 {
        println!("  (none)");
    }

    // Summary
    println!("\n=== SUMMARY ===");
    println!("NT files: {}", nt_list.len());
    println!("jwalk files: {}", jwalk_files.len());
    println!("NT-only: {}", nt_only);
    println!("jwalk-only: {}", jwalk_only);
    println!("Size mismatches: {}", mismatches);
    println!(
        "Logical bytes diff: {} (NT - jwalk = {})",
        (nt_data.total_logical_bytes as i128 - jwalk_logical as i128),
        nt_data.total_logical_bytes as i128 - jwalk_logical as i128
    );

    // Re-run check for churn
    if nt_only > 0 || jwalk_only > 0 || mismatches > 0 {
        println!("\n=== Re-run verification (checking for churn) ===");
        println!("Re-running both walkers to see if differences persist...");

        // Second run
        let nt_data2 = scan_nt_full(&path).expect("scan_nt_full failed");
        let mut jwalk_files2: Vec<(PathBuf, u64)> = Vec::new();
        for entry in WalkDir::new(&root).skip_hidden(false) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.file_type().is_file()
                && let Ok(meta) = entry.metadata()
            {
                jwalk_files2.push((entry.path(), meta.len()));
            }
        }
        jwalk_files2.sort_by(|a, b| a.0.cmp(&b.0));

        let mut nt_list2: Vec<(PathBuf, u64)> = nt_data2
            .files
            .iter()
            .filter(|f| !f.is_reparse && !f.is_cloud)
            .map(|f| (f.path.clone(), f.logical_size))
            .collect();
        nt_list2.sort_by(|a, b| a.0.cmp(&b.0));

        // Check if differences persist
        let nt_set2: std::collections::HashSet<_> = nt_list2.iter().map(|(p, _)| p).collect();
        let jwalk_set2: std::collections::HashSet<_> =
            jwalk_files2.iter().map(|(p, _)| p).collect();

        let nt_only_1: Vec<_> = nt_list
            .iter()
            .filter(|(p, _)| !jwalk_set.contains(p))
            .collect();
        let nt_only_2: Vec<_> = nt_list2
            .iter()
            .filter(|(p, _)| !jwalk_set2.contains(p))
            .collect();

        println!("NT-only in run 1: {}", nt_only_1.len());
        println!("NT-only in run 2: {}", nt_only_2.len());
        let persistent_nt_only = nt_only_1
            .iter()
            .filter(|(p, _)| nt_only_2.iter().any(|(p2, _)| p == p2))
            .count();
        println!("Persistent NT-only (both runs): {}", persistent_nt_only);

        let jwalk_only_1: Vec<_> = jwalk_files
            .iter()
            .filter(|(p, _)| !nt_set.contains(p))
            .collect();
        let jwalk_only_2: Vec<_> = jwalk_files2
            .iter()
            .filter(|(p, _)| !nt_set2.contains(p))
            .collect();
        println!("jwalk-only in run 1: {}", jwalk_only_1.len());
        println!("jwalk-only in run 2: {}", jwalk_only_2.len());
        let persistent_jwalk_only = jwalk_only_1
            .iter()
            .filter(|(p, _)| jwalk_only_2.iter().any(|(p2, _)| p == p2))
            .count();
        println!(
            "Persistent jwalk-only (both runs): {}",
            persistent_jwalk_only
        );
    }
}
