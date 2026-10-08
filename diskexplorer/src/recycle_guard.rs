//! Recycle Bin guard: prevents permanent deletion by checking if a file can be recycled
//!
//! On Windows, the `trash` crate may permanently delete files instead of recycling them
//! in certain conditions (disabled bin, file too large, removable/network drive).
//! This module implements a pre-check before calling `trash::delete()`.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeNameForVolumeMountPointW};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
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
fn get_drive_type(path: &Path) -> u32 {
    let root = path.components().next().unwrap().as_os_str();
    let wide: Vec<u16> = root.encode_wide().chain(Some(0)).collect();
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
fn get_recycle_bin_config(volume_guid: &str) -> Option<(bool, u64)> {
    // Registry path: HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\<GUID>
    // Remove the leading \\?\ and trailing \
    let guid = volume_guid
        .trim_start_matches("\\\\?\\")
        .trim_end_matches('\\');
    let subkey = format!(
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{}",
        guid
    );

    let subkey_wide: Vec<u16> = OsStr::new(&subkey).encode_wide().chain(Some(0)).collect();

    let mut hkey = HKEY::default();
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey_wide.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hkey,
        )
    };

    if result != ERROR_SUCCESS {
        return None;
    }

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

    let bin_enabled = if result == ERROR_SUCCESS {
        nuke_on_delete == 0
    } else {
        // Default: assume enabled if key not found
        true
    };

    // Read MaxCapacity (in MB on Windows 10+)
    let mut max_capacity_mb = 0u32;
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

    if result != ERROR_SUCCESS {
        return Some((bin_enabled, 0)); // Unknown capacity
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
pub fn safe_trash(path: &Path) -> Result<(), RefuseReason> {
    can_recycle(path)?;
    trash::delete(path).map_err(|_| RefuseReason::UnknownConfiguration)
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
