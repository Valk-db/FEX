//! Recycle Bin guard: prevents permanent deletion by checking if a file can be recycled
//!
//! On Windows, the `trash` crate may permanently delete files instead of recycling them
//! in certain conditions (disabled bin, file too large, removable/network drive).
//! This module implements a pre-check before calling `trash::delete()`.
//!
//! Key findings from F3 probe:
//! - Registry is in HKCU (not HKLM), subkey is bare GUID (not Volume\GUID)
//! - MaxCapacity is in MB (e.g., 8089 = ~8GB, 192820 = ~188GB)
//! - NukeOnDelete=0 means enabled
//! - trash::delete() returns Ok(()) but PERMANENTLY DELETES on this system
//! - Post-delete verification via SHQueryRecycleBinW is required

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use crate::nt_walker::simplify_path;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeNameForVolumeMountPointW};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
};
use windows::core::PCWSTR;

/// Drive type constants
const DRIVE_UNKNOWN: u32 = 0;
const DRIVE_NO_ROOT_DIR: u32 = 1;
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;
const DRIVE_RAMDISK: u32 = 6;

/// Reason why recycling was refused
#[derive(Debug, Clone, PartialEq)]
pub enum RefuseReason {
    /// Drive type doesn't support Recycle Bin (removable, network, CD-ROM, RAM disk)
    UnsupportedDriveType {
        drive_type: u32,
        drive_type_name: String,
    },
    /// Recycle Bin is disabled for this volume (NukeOnDelete = 1)
    BinDisabled,
    /// File size exceeds volume's Recycle Bin max capacity
    ExceedsCapacity { file_size: u64, max_capacity: u64 },
    /// Could not determine Recycle Bin settings (conservative: refuse)
    UnknownConfiguration,
    /// Volume GUID not found for this path
    VolumeGuidNotFound,
}

impl std::fmt::Display for RefuseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefuseReason::UnsupportedDriveType {
                drive_type_name, ..
            } => {
                write!(
                    f,
                    "Drive type {} does not support Recycle Bin",
                    drive_type_name
                )
            }
            RefuseReason::BinDisabled => {
                write!(f, "Recycle Bin is disabled for this volume")
            }
            RefuseReason::ExceedsCapacity {
                file_size,
                max_capacity,
            } => {
                write!(
                    f,
                    "File size ({}) exceeds Recycle Bin capacity ({})",
                    human_size(*file_size),
                    human_size(*max_capacity)
                )
            }
            RefuseReason::UnknownConfiguration => {
                write!(
                    f,
                    "Could not verify Recycle Bin configuration (conservative refusal)"
                )
            }
            RefuseReason::VolumeGuidNotFound => {
                write!(f, "Could not find volume GUID for this path")
            }
        }
    }
}

fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}", n, UNITS[u])
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

/// Get the drive type for a path
/// Uses simplify_path to handle verbatim paths (\\?\ prefix) correctly
fn get_drive_type(path: &Path) -> u32 {
    let simple = simplify_path(path);
    let root = simple.components().next().unwrap().as_os_str();
    let mut root_str = root.to_string_lossy().into_owned();
    if !root_str.ends_with('\\') {
        root_str.push('\\');
    }
    let wide: Vec<u16> = OsStr::new(&root_str).encode_wide().chain(Some(0)).collect();
    unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) }
}

/// Get drive type name for display
fn drive_type_name(drive_type: u32) -> String {
    match drive_type {
        DRIVE_UNKNOWN => "Unknown".to_string(),
        DRIVE_NO_ROOT_DIR => "No Root Directory".to_string(),
        DRIVE_REMOVABLE => "Removable".to_string(),
        DRIVE_FIXED => "Fixed".to_string(),
        DRIVE_REMOTE => "Network".to_string(),
        DRIVE_CDROM => "CD-ROM".to_string(),
        DRIVE_RAMDISK => "RAM Disk".to_string(),
        _ => format!("Unknown ({})", drive_type),
    }
}

/// Get the volume GUID for a path (e.g., `\\?\Volume{...}\`)
fn get_volume_guid(path: &Path) -> Option<String> {
    // GetVolumeNameForVolumeMountPointW requires a volume mount point (root like C:\)
    // Extract the root component from the path
    let root = path.components().next()?.as_os_str();
    let mut root_str = root.to_string_lossy().into_owned();
    if !root_str.ends_with('\\') {
        root_str.push('\\');
    }

    let wide: Vec<u16> = std::ffi::OsStr::new(&root_str)
        .encode_wide()
        .chain(Some(0))
        .collect();

    let mut buffer = vec![0u16; 260];
    let result = unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(wide.as_ptr()), &mut buffer) };

    if result.is_ok() {
        let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        let guid_str = OsString::from_wide(&buffer[..len])
            .to_string_lossy()
            .into_owned();
        Some(guid_str)
    } else {
        None
    }
}

/// Check if Recycle Bin is enabled and get max capacity for a volume
/// Checks HKCU first (per F3 findings), then HKLM. Subkey is bare GUID.
fn get_recycle_bin_config(volume_guid: &str) -> Option<(bool, u64)> {
    // Remove the leading \\?\ and trailing \
    let guid = volume_guid
        .trim_start_matches("\\\\?\\")
        .trim_end_matches('\\');
    let subkey = format!(
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{}",
        guid
    );

    let subkey_wide: Vec<u16> = OsStr::new(&subkey).encode_wide().chain(Some(0)).collect();

    // Try HKCU first, then HKLM
    let hives = [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE];
    let mut bin_enabled = false;
    let mut max_capacity_mb = 0u32;
    let mut found = false;

    for &hive in &hives {
        let mut hkey = HKEY::default();
        let result = unsafe {
            RegOpenKeyExW(
                hive,
                PCWSTR(subkey_wide.as_ptr()),
                Some(0),
                KEY_READ,
                &mut hkey,
            )
        };

        if result != ERROR_SUCCESS {
            continue;
        }
        found = true;

        // Read NukeOnDelete (1 = bin disabled, 0 = enabled)
        let mut nuke_on_delete = 0u32;
        let mut cb_data = std::mem::size_of::<u32>() as u32;
        let nuke_name: Vec<u16> = OsStr::new("NukeOnDelete")
            .encode_wide()
            .chain(Some(0))
            .collect();
        let result = unsafe {
            RegQueryValueExW(
                hkey,
                PCWSTR(nuke_name.as_ptr()),
                None,
                None,
                Some(&mut nuke_on_delete as *mut _ as *mut u8),
                Some(&mut cb_data),
            )
        };

        if result == ERROR_SUCCESS {
            bin_enabled = nuke_on_delete == 0;
        }

        // Read MaxCapacity (in MB on Windows 10+)
        let mut cb_data = std::mem::size_of::<u32>() as u32;
        let max_name: Vec<u16> = OsStr::new("MaxCapacity")
            .encode_wide()
            .chain(Some(0))
            .collect();
        let result = unsafe {
            RegQueryValueExW(
                hkey,
                PCWSTR(max_name.as_ptr()),
                None,
                None,
                Some(&mut max_capacity_mb as *mut _ as *mut u8),
                Some(&mut cb_data),
            )
        };

        unsafe {
            let _ = RegCloseKey(hkey);
        };

        if result == ERROR_SUCCESS {
            break; // Found capacity
        }
    }

    if !found {
        return None;
    }

    // MaxCapacity is in MB on Windows 10+
    Some((bin_enabled, max_capacity_mb as u64 * 1024 * 1024))
}

/// Check if a file can be recycled
pub fn can_recycle(path: &Path) -> Result<(), RefuseReason> {
    // 1. Check drive type
    let drive_type = get_drive_type(path);
    match drive_type {
        DRIVE_REMOVABLE | DRIVE_REMOTE | DRIVE_CDROM | DRIVE_RAMDISK | DRIVE_NO_ROOT_DIR => {
            return Err(RefuseReason::UnsupportedDriveType {
                drive_type,
                drive_type_name: drive_type_name(drive_type),
            });
        }
        DRIVE_FIXED => {} // OK
        DRIVE_UNKNOWN => {
            // Conservative: refuse if unknown
            return Err(RefuseReason::UnknownConfiguration);
        }
        _ => {} // Other types: allow but warn
    }

    // 2. Get volume GUID
    let volume_guid = match get_volume_guid(path) {
        Some(g) => g,
        None => return Err(RefuseReason::VolumeGuidNotFound),
    };

    // 3. Check Recycle Bin config
    let (bin_enabled, max_capacity) = match get_recycle_bin_config(&volume_guid) {
        Some(cfg) => cfg,
        None => return Err(RefuseReason::UnknownConfiguration),
    };

    if !bin_enabled {
        return Err(RefuseReason::BinDisabled);
    }

    // 4. Check file size vs capacity
    if max_capacity > 0 {
        let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if file_size > max_capacity {
            return Err(RefuseReason::ExceedsCapacity {
                file_size,
                max_capacity,
            });
        }
    }

    Ok(())
}

/// Try to delete a file via Recycle Bin, with guard
/// Returns:
/// - Ok(()) if file was recycled and verified
/// - Err(RefuseReason) if guard refused
/// - Err(TrashFailed) if trash::delete failed or post-delete verification failed
pub fn safe_trash(path: &Path) -> Result<(), RecycleError> {
    // Pre-check
    can_recycle(path).map_err(RecycleError::Refused)?;

    // Get drive root for post-delete verification
    let simple = simplify_path(path);
    let root = simple.components().next().unwrap().as_os_str();
    let mut root_str = root.to_string_lossy().into_owned();
    if !root_str.ends_with('\\') {
        root_str.push('\\');
    }

    // Get recycle bin count before
    let before_count = query_recycle_bin_count(&root_str);

    // Try to trash
    trash::delete(path).map_err(|e| RecycleError::TrashFailed(e.to_string()))?;

    // Post-delete verification: check recycle bin count increased
    let after_count = query_recycle_bin_count(&root_str);
    if after_count <= before_count {
        // Recycle bin count didn't increase - file was permanently deleted!
        // This is a critical failure
        return Err(RecycleError::NotVerifiedInRecycleBin);
    }

    Ok(())
}

/// Recycle error types - distinct from refusal reasons
#[derive(Debug, Clone, PartialEq)]
pub enum RecycleError {
    Refused(RefuseReason),
    TrashFailed(String),
    NotVerifiedInRecycleBin,
}

impl std::fmt::Display for RecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecycleError::Refused(reason) => write!(f, "Refused: {reason}"),
            RecycleError::TrashFailed(e) => write!(f, "Trash operation failed: {e}"),
            RecycleError::NotVerifiedInRecycleBin => {
                write!(f, "NOT VERIFIED IN RECYCLE BIN - file was permanently deleted!")
            }
        }
    }
}

impl std::error::Error for RecycleError {}

/// Query recycle bin item count for a drive root
fn query_recycle_bin_count(root_path: &str) -> u64 {
    use std::process::Command;

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
                $result = [RecycleBin]::SHQueryRecycleBinW('{root_path}', [ref]$info)
                if ($result -eq 0) {{ $info.i64NumItems }} else {{ 0 }}
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_drive_type_mapping() {
        assert_eq!(drive_type_name(DRIVE_FIXED), "Fixed");
        assert_eq!(drive_type_name(DRIVE_REMOVABLE), "Removable");
        assert_eq!(drive_type_name(DRIVE_REMOTE), "Network");
        assert_eq!(drive_type_name(DRIVE_CDROM), "CD-ROM");
        assert_eq!(drive_type_name(DRIVE_RAMDISK), "RAM Disk");
        assert_eq!(drive_type_name(999), "Unknown (999)");
    }

    #[test]
    fn test_human_size() {
        assert_eq!(human_size(500), "500 B");
        assert_eq!(human_size(1500), "1.5 KB");
        assert_eq!(human_size(1500 * 1024), "1.5 MB");
        assert_eq!(human_size(1500 * 1024 * 1024), "1.5 GB");
    }

    #[test]
    fn test_can_recycle_temp_dir() {
        let temp = std::env::temp_dir();
        let test_file = temp.join("recycle_guard_test.txt");
        fs::write(&test_file, "test").unwrap();

        let result = can_recycle(&test_file);
        println!("Temp dir recycle check: {:?}", result);

        let _ = fs::remove_file(&test_file);
    }

    #[test]
    fn test_get_volume_guid() {
        // Use C:\ as it's guaranteed to work
        let root = Path::new("C:\\");
        let guid = get_volume_guid(root);
        println!("Volume GUID for C:\\: {:?}", guid);
        assert!(guid.is_some(), "Failed to get volume GUID for C:\\");
        assert!(guid.unwrap().starts_with("\\\\?\\Volume{"));
    }
}
