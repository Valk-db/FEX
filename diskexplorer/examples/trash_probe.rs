//! Trash probe - reproducible test for Recycle Bin behavior
//!
//! Tests whether trash::delete() actually recycles files on this system,
//! comparing canonicalized (verbatim) paths vs simplified paths.

use std::ffi::OsString;
use std::fs;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use diskexplorer::nt_walker::simplify_path;
use diskexplorer::recycle_guard::recycle_bin_item_count;
#[allow(clippy::single_component_path_imports)]
use trash;
use windows::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegEnumKeyExW,
    RegOpenKeyExW, RegQueryValueExW,
};
use windows::core::{PCWSTR, PWSTR};

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

        // Get recycle bin list before (for verification)
        let _list_before = trash::os_limited::list().unwrap();

        // Test A: canonicalized path (verbatim, \\?\ prefix)
        let canonical = file_path.canonicalize().unwrap();
        println!("    Canonical path: {}", canonical.display());

        // Get recycle bin count before
        let before_count = recycle_bin_item_count(&canonical.to_string_lossy()).unwrap_or(0);
        println!("    Recycle bin count before: {}", before_count);

        // Try to trash with canonical path
        let result_a = trash::delete(&canonical);
        println!("    trash::delete(canonical): {:?}", result_a);

        // Check if file still exists
        let exists_after_a = canonical.exists();
        println!("    File exists after: {}", exists_after_a);

        // Get recycle bin count after
        let after_count = recycle_bin_item_count(&canonical.to_string_lossy()).unwrap_or(0);
        println!("    Recycle bin count after: {}", after_count);
        println!("    Delta: {}", after_count.saturating_sub(before_count));

        // Check if file is in os_limited::list()
        let list_after_a = trash::os_limited::list().unwrap();
        let found_in_list_a = list_after_a.iter().any(|i| i.original_path() == canonical);
        println!("    Found in os_limited::list(): {}", found_in_list_a);

        // Test B: simplified path (if file still exists, re-create it)
        let file_path_b = dir.join(format!("{}_b", name));
        let file = fs::File::create(&file_path_b).unwrap();
        file.set_len(size).unwrap();
        drop(file);

        let simplified = simplify_path(&file_path_b);
        println!("    Simplified path: {}", simplified.display());

        let before_count_b = recycle_bin_item_count(&simplified.to_string_lossy()).unwrap_or(0);
        println!("    Recycle bin count before: {}", before_count_b);

        let result_b = trash::delete(&simplified);
        println!("    trash::delete(simplified): {:?}", result_b);

        let exists_after_b = simplified.exists();
        println!("    File exists after: {}", exists_after_b);

        let after_count_b = recycle_bin_item_count(&simplified.to_string_lossy()).unwrap_or(0);
        println!("    Recycle bin count after: {}", after_count_b);
        println!(
            "    Delta: {}",
            after_count_b.saturating_sub(before_count_b)
        );

        // Check if file is in os_limited::list()
        let list_after_b = trash::os_limited::list().unwrap();
        let found_in_list_b = list_after_b.iter().any(|i| i.original_path() == simplified);
        println!("    Found in os_limited::list(): {}", found_in_list_b);

        // Cleanup
        let _ = fs::remove_file(&file_path);
        let _ = fs::remove_file(&file_path_b);
    }
}

fn dump_registry() {
    let hives = vec![
        (
            "HKCU",
            HKEY_CURRENT_USER,
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\BitBucket",
        ),
        (
            "HKLM",
            HKEY_LOCAL_MACHINE,
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\BitBucket",
        ),
        (
            "HKCU_Policies",
            HKEY_CURRENT_USER,
            r"SOFTWARE\Policies\Microsoft\Windows\Explorer",
        ),
        (
            "HKLM_Policies",
            HKEY_LOCAL_MACHINE,
            r"SOFTWARE\Policies\Microsoft\Windows\Explorer",
        ),
    ];

    for (label, hive, key_path) in hives {
        println!("\n  {label}: {hive:?} {key_path}");

        let key_wide: Vec<u16> = std::ffi::OsStr::new(key_path)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut hkey = HKEY::default();
        let result = unsafe {
            RegOpenKeyExW(
                hive,
                PCWSTR(key_wide.as_ptr()),
                Some(0),
                KEY_READ,
                &mut hkey,
            )
        };

        if result != ERROR_SUCCESS {
            println!("    Key not found (error {})", result.0);
            continue;
        }

        // Enumerate subkeys
        let mut index = 0u32;
        loop {
            let mut name_buf = [0u16; 256];
            let mut name_len = name_buf.len() as u32;
            let result = unsafe {
                RegEnumKeyExW(
                    hkey,
                    index,
                    Some(PWSTR(name_buf.as_mut_ptr())),
                    &mut name_len,
                    None,
                    None,
                    None,
                    None,
                )
            };

            if result == WIN32_ERROR(259u32) {
                // ERROR_NO_MORE_ITEMS
                break;
            }
            if result != ERROR_SUCCESS {
                break;
            }

            let subkey_name = OsString::from_wide(&name_buf[..name_len as usize])
                .to_string_lossy()
                .into_owned();

            // Open subkey and read values
            let subkey_path = format!("{}\\{}", key_path, subkey_name);
            let subkey_wide: Vec<u16> = std::ffi::OsStr::new(&subkey_path)
                .encode_wide()
                .chain(Some(0))
                .collect();
            let mut sub_hkey = HKEY::default();
            let result = unsafe {
                RegOpenKeyExW(
                    hive,
                    PCWSTR(subkey_wide.as_ptr()),
                    Some(0),
                    KEY_READ,
                    &mut sub_hkey,
                )
            };

            if result == ERROR_SUCCESS {
                // Read NukeOnDelete and MaxCapacity
                for value_name in ["NukeOnDelete", "MaxCapacity", "NoRecycleFiles"] {
                    let value_wide: Vec<u16> = std::ffi::OsStr::new(value_name)
                        .encode_wide()
                        .chain(Some(0))
                        .collect();
                    let mut value = 0u32;
                    let mut value_size = std::mem::size_of::<u32>() as u32;
                    let result = unsafe {
                        RegQueryValueExW(
                            sub_hkey,
                            PCWSTR(value_wide.as_ptr()),
                            None,
                            None,
                            Some(&mut value as *mut _ as *mut u8),
                            Some(&mut value_size),
                        )
                    };
                    if result == ERROR_SUCCESS {
                        println!("    {}: {} = {}", subkey_name, value_name, value);
                    }
                }
                unsafe {
                    let _ = RegCloseKey(sub_hkey);
                }
            }

            index += 1;
        }

        unsafe {
            let _ = RegCloseKey(hkey);
        }
    }
}
