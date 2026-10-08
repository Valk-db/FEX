//! Trash probe - reproducible test for Recycle Bin behavior
//!
//! Tests whether trash::delete() actually recycles files on this system,
//! comparing canonicalized (verbatim) paths vs simplified paths.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use diskexplorer::nt_walker::simplify_path;

fn main() {
    println!("=== Trash Probe ===\n");

    // Test locations
    let test_dirs = vec![
        ("temp_dir", std::env::temp_dir()),
        ("user_profile_temp", {
            let mut p = dirs::home_dir().unwrap();
            p.push("AppData");
            p.push("Local");
            p.push("Temp");
            p
        }),
    ];

    for (label, dir) in test_dirs {
        println!("\n--- Testing: {} ({}) ---", label, dir.display());
        run_probe(&dir);
    }

    // Dump registry
    println!("\n=== Registry Dump ===");
    dump_registry();

    println!("\n=== Probe Complete ===");
}

fn run_probe(dir: &Path) {
    // Test file sizes
    let test_cases = vec![
        ("small_1kb.txt", 1024),
        ("small_100kb.txt", 100 * 1024),
        ("medium_1mb.bin", 1024 * 1024),
        ("large_100mb.bin", 100 * 1024 * 1024),
    ];

    for (name, size) in test_cases {
        println!("\n  Testing: {} ({})", name, size);

        // Create a sparse file (don't write real data for large sizes)
        let file_path = dir.join(name);
        let file = fs::File::create(&file_path).unwrap();
        file.set_len(size).unwrap();
        drop(file);

        // Verify file exists
        assert!(file_path.exists(), "File should exist after creation");

        // Test A: canonicalized path (verbatim, \\?\ prefix)
        let canonical = file_path.canonicalize().unwrap();
        println!("    Canonical path: {}", canonical.display());

        // Get recycle bin count before
        let before_count = get_recycle_bin_count(&canonical);
        println!("    Recycle bin count before: {}", before_count);

        // Try to trash with canonical path
        let result_a = trash::delete(&canonical);
        println!("    trash::delete(canonical): {:?}", result_a);

        // Check if file still exists
        let exists_after_a = canonical.exists();
        println!("    File exists after: {}", exists_after_a);

        // Get recycle bin count after
        let after_count = get_recycle_bin_count(&canonical);
        println!("    Recycle bin count after: {}", after_count);
        println!("    Delta: {}", after_count.saturating_sub(before_count));

        // Test B: simplified path (if file still exists, re-create it)
        let file_path_b = dir.join(format!("{}_b", name));
        let file = fs::File::create(&file_path_b).unwrap();
        file.set_len(size).unwrap();
        drop(file);

        let simplified = simplify_path(&file_path_b);
        println!("    Simplified path: {}", simplified.display());

        let before_count_b = get_recycle_bin_count(&simplified);
        println!("    Recycle bin count before: {}", before_count_b);

        let result_b = trash::delete(&simplified);
        println!("    trash::delete(simplified): {:?}", result_b);

        let exists_after_b = simplified.exists();
        println!("    File exists after: {}", exists_after_b);

        let after_count_b = get_recycle_bin_count(&simplified);
        println!("    Recycle bin count after: {}", after_count_b);
        println!("    Delta: {}", after_count_b.saturating_sub(before_count_b));

        // Cleanup
        let _ = fs::remove_file(&file_path);
        let _ = fs::remove_file(&file_path_b);
    }
}

fn get_recycle_bin_count(path: &Path) -> u64 {
    // Get drive root for SHQueryRecycleBinW
    let root = path.components().next().unwrap().as_os_str();
    let mut root_str = root.to_string_lossy().into_owned();
    if !root_str.ends_with('\\') {
        root_str.push('\\');
    }

    // Use SHQueryRecycleBinW via PowerShell
    let output = Command::new("powershell")
        .args([
            "-Command",
            &format!(
                r#"
                Add-Type -TypeDefinition @"
                using System;
                using System.Runtime.InteropServices;
                public class RecycleBin {{
                    [DllImport("shell32.dll", CharSet=CharSet.Unicode)]
                    public static extern int SHQueryRecycleBinW(string pszRootPath, ref SHQUERYRBINFO pSHQueryRBInfo);
                }}
                [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)]
                public struct SHQUERYRBINFO {{
                    public int cbSize;
                    public long i64Size;
                    public long i64NumItems;
                }}
"@
                $info = New-Object RecycleBin+SHQUERYRBINFO
                $info.cbSize = [System.Runtime.InteropServices.Marshal]::SizeOf($info)
                $result = [RecycleBin]::SHQueryRecycleBinW('{root_str}', [ref]$info)
                if ($result -eq 0) {{ $info.i64NumItems }} else {{ -1 }}
                "#
            ),
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.trim().parse().unwrap_or(0)
        }
        _ => 0,
    }
}

fn dump_registry() {
    use std::process::Command;

    let hives = vec![
        ("HKCU", r"HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket"),
        ("HKLM", r"HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\BitBucket"),
        (
            "HKCU_Policies",
            r"HKCU:\Software\Policies\Microsoft\Windows\Explorer",
        ),
        (
            "HKLM_Policies",
            r"HKLM:\SOFTWARE\Policies\Microsoft\Windows\Explorer",
        ),
    ];

    for (label, key) in hives {
        println!("\n  {label}: {key}");
        let output = Command::new("powershell")
            .args([
                "-Command",
                &format!(
                    r#"
                    if (Test-Path '{key}') {{
                        Get-ChildItem '{key}' -Recurse -ErrorAction SilentlyContinue |
                        ForEach-Object {{
                            $props = @{{}}
                            foreach ($name in $_.Property) {{
                                $props[$name] = $_.GetValue($name)
                            }}
                            [pscustomobject]@{{
                                Path = $_.PSPath
                                Name = $_.PSChildName
                                Props = $props
                            }}
                        }} | Format-List
                    }} else {{
                        Write-Host "  Key not found"
                    }}
                    "#
                ),
            ])
            .output();

        match output {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                for line in stdout.lines() {
                    let line = line.trim();
                    if !line.is_empty() && !line.starts_with("PSPath") {
                        println!("    {line}");
                    }
                }
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                println!("    Error: {stderr}");
            }
            Err(e) => {
                println!("    Failed to run: {e}");
            }
        }
    }

    // Also check NoRecycleFiles
    for (label, key) in [
        ("HKCU_NoRecycleFiles", r"HKCU:\Software\Policies\Microsoft\Windows\Explorer"),
        ("HKLM_NoRecycleFiles", r"HKLM:\SOFTWARE\Policies\Microsoft\Windows\Explorer"),
    ] {
        let output = Command::new("powershell")
            .args([
                "-Command",
                &format!(
                    r#"
                    if (Test-Path '{key}') {{
                        Get-ItemProperty '{key}' -Name 'NoRecycleFiles' -ErrorAction SilentlyContinue | Format-List
                    }} else {{
                        Write-Host "  Key not found"
                    }}
                    "#
                ),
            ])
            .output();

        println!("\n  {label}:");
        match output {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                for line in stdout.lines() {
                    let line = line.trim();
                    if !line.is_empty() {
                        println!("    {line}");
                    }
                }
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                println!("    Error: {stderr}");
            }
            Err(e) => {
                println!("    Failed to run: {e}");
            }
        }
    }
}