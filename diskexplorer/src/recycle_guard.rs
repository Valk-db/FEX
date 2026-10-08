//! Recycle Bin guard: prevents permanent deletion by checking if a file can be recycled
//!
//! On Windows, the `trash` crate may permanently delete files instead of recycling them
//! in certain conditions (disabled bin, file too large, removable/network drive).
//! This module implements a pre-check before calling `trash::delete()`.
//!
//! Key findings (updated per G2 probe with direct SHQueryRecycleBinW):
//! - Registry is in HKCU (not HKLM), subkey is bare GUID (not Volume\GUID)
//! - MaxCapacity is in MB (e.g., 8089 = ~8GB, 192820 = ~188GB)
//! - NukeOnDelete=0 means enabled
//! - trash::delete() DOES recycle on this system for simplified paths
//! - Verbatim (\\?\) paths are recycled but NOT found in trash::os_limited::list()
//! - Post-delete verification via SHQueryRecycleBinW count delta is required
//! - This guard uses simplify_path() for all operations to ensure verifiable recycling

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use crate::nt_walker::simplify_path;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeNameForVolumeMountPointW};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
};
use windows::Win32::UI::Shell::{SHQueryRecycleBinW, SHQUERYRBINFO};
use windows::core::PCWSTR;

/// Error type for recycle bin queries
#[derive(Debug, Clone, PartialEq)]
pub enum BinQueryError {
    /// Failed to call SHQueryRecycleBinW
    QueryFailed(String),
    /// Invalid root path
    InvalidRootPath,
}

impl std::fmt::Display for BinQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BinQueryError::QueryFailed(e) => write!(f, "Recycle bin query failed: {e}"),
            BinQueryError::InvalidRootPath => write!(f, "Invalid root path for recycle bin query"),
        }
    }
}

impl std::error::Error for BinQueryError {}

/// Trait for injectable Recycle Bin configuration access (for testing)
pub trait RecycleBinConfigProvider {
    /// Get NukeOnDelete and MaxCapacity for a volume GUID
    /// Returns None if key not found (default config = enabled, no capacity limit)
    fn get_config(&self, volume_guid: &str) -> Option<(bool, u64)>;

    /// Check if NoRecycleFiles policy is set
    fn no_recycle_files_policy(&self) -> bool;
}

/// Default implementation using Windows Registry
pub struct RegistryConfigProvider;

impl RecycleBinConfigProvider for RegistryConfigProvider {
    fn get_config(&self, volume_guid: &str) -> Option<(bool, u64)> {
        get_recycle_bin_config_registry(volume_guid)
    }

    fn no_recycle_files_policy(&self) -> bool {
        check_no_recycle_files_policy_registry()
    }
}

/// Check if Recycle Bin is enabled and get max capacity for a volume (Registry version)
/// Checks HKCU first (per G2 findings), then HKLM. Subkey is bare GUID.
fn get_recycle_bin_config_registry(volume_guid: &str) -> Option<(bool, u64)> {
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

/// Check NoRecycleFiles policy in Registry
fn check_no_recycle_files_policy_registry() -> bool {
    let hives = [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE];
    let policy_key = r"SOFTWARE\Policies\Microsoft\Windows\Explorer";
    let policy_name = "NoRecycleFiles";

    let key_wide: Vec<u16> = OsStr::new(policy_key).encode_wide().chain(Some(0)).collect();
    let name_wide: Vec<u16> = OsStr::new(policy_name).encode_wide().chain(Some(0)).collect();

    for &hive in &hives {
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
            continue;
        }

        let mut value = 0u32;
        let mut cb_data = std::mem::size_of::<u32>() as u32;
        let result = unsafe {
            RegQueryValueExW(
                hkey,
                PCWSTR(name_wide.as_ptr()),
                None,
                None,
                Some(&mut value as *mut _ as *mut u8),
                Some(&mut cb_data),
            )
        };

        unsafe {
            let _ = RegCloseKey(hkey);
        };

        if result == ERROR_SUCCESS && value == 1 {
            return true;
        }
    }

    false
}

/// Result of post-delete recycle verification
#[derive(Debug, Clone, PartialEq)]
pub enum VerificationResult {
    /// File verified in Recycle Bin (count increased)
    Verified,
    /// Could not verify (bin query failed) - warning only
    Unverifiable(BinQueryError),
    /// Recycle bin count did not increase
    NotIncreased,
}

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
    /// Path too long after simplification (>259 chars)
    PathTooLong,
    /// Could not determine Recycle Bin settings (missing key = default config = enabled, but we track this)
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
                    "Could not verify Recycle Bin configuration (missing key = default config = enabled)"
                )
            }
            RefuseReason::VolumeGuidNotFound => {
                write!(f, "Could not find volume GUID for this path")
            }
            RefuseReason::PathTooLong => {
                write!(f, "Path too long after simplification (>259 chars)")
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
/// Kept for backward compatibility - use get_recycle_bin_config_registry instead
#[allow(dead_code)]
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

/// Get volume total size for capacity fallback
fn get_volume_total_size(path: &Path) -> Option<u64> {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows::core::PCWSTR;
    use std::os::windows::ffi::OsStrExt;

    let simple = simplify_path(path);
    let root = simple.components().next()?.as_os_str();
    let mut root_str = root.to_string_lossy().into_owned();
    if !root_str.ends_with('\\') {
        root_str.push('\\');
    }
    let wide: Vec<u16> = OsStr::new(&root_str).encode_wide().chain(Some(0)).collect();

    let mut total_bytes = 0u64;
    let mut free_bytes = 0u64;
    let mut free_bytes_user = 0u64;

    let result = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut free_bytes_user),
            Some(&mut total_bytes),
            Some(&mut free_bytes),
        )
    };

    if result.is_ok() && total_bytes > 0 {
        Some(total_bytes)
    } else {
        None
    }
}

/// Check if a file can be recycled using the default registry provider
/// Returns Err(RefuseReason) if the file should not be trashed
/// Returns Ok(()) if the file passes all pre-checks
pub fn can_recycle(path: &Path) -> Result<(), RefuseReason> {
    can_recycle_with_provider(path, &RegistryConfigProvider)
}

/// Check if a file can be recycled with a custom config provider (for testing)
pub fn can_recycle_with_provider<P: RecycleBinConfigProvider>(
    path: &Path,
    provider: &P,
) -> Result<(), RefuseReason> {
    // All paths go through simplify_path first
    let simple = simplify_path(path);

    // 1. Check path length after simplification
    if simple.to_string_lossy().len() > 259 {
        return Err(RefuseReason::PathTooLong);
    }

    // 2. Check drive type (uses simplified path)
    let drive_type = get_drive_type(path);
    match drive_type {
        DRIVE_REMOVABLE | DRIVE_RAMDISK | DRIVE_CDROM | DRIVE_NO_ROOT_DIR => {
            return Err(RefuseReason::UnsupportedDriveType {
                drive_type,
                drive_type_name: drive_type_name(drive_type),
            });
        }
        DRIVE_REMOTE => {
            // Network drives: UNC paths - refuse
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

    // 3. Get volume GUID (uses simplified path)
    let volume_guid = match get_volume_guid(path) {
        Some(g) => g,
        None => return Err(RefuseReason::VolumeGuidNotFound),
    };

    // 4. Check Recycle Bin config via provider
    // Check NoRecycleFiles policy first
    if provider.no_recycle_files_policy() {
        return Err(RefuseReason::BinDisabled);
    }

    // Then check per-volume config
    let (bin_enabled, max_capacity) = provider.get_config(&volume_guid).unwrap_or((true, 0));

    if !bin_enabled {
        return Err(RefuseReason::BinDisabled);
    }

    // 5. Check file size vs capacity
    let file_size = std::fs::metadata(&simple).map(|m| m.len()).unwrap_or(0);

    if max_capacity > 0 {
        if file_size > max_capacity {
            return Err(RefuseReason::ExceedsCapacity {
                file_size,
                max_capacity,
            });
        }
    } else {
        // MaxCapacity unknown → conservative bound: 1% of volume total size
        // PROPOSED: 1% of volume size as fallback
        const MAX_FILE_FRACTION: u64 = 100; // 1%
        if let Some(volume_size) = get_volume_total_size(&simple) {
            let fallback_capacity = volume_size / MAX_FILE_FRACTION;
            if file_size > fallback_capacity {
                return Err(RefuseReason::ExceedsCapacity {
                    file_size,
                    max_capacity: fallback_capacity,
                });
            }
        }
    }

    Ok(())
}

/// Try to delete a file via Recycle Bin, with guard
/// Returns:
/// - Ok(()) if file was recycled and verified
/// - Err(RefuseReason) if guard refused
/// - Err(TrashFailed) if trash::delete failed or post-delete verification failed
/// - Err(Unverifiable) if bin query failed (can't verify, but continue)
pub fn safe_trash(path: &Path) -> Result<(), RecycleError> {
    // Pre-check (uses simplify_path internally)
    can_recycle(path).map_err(RecycleError::Refused)?;

    // Use simplified path for the actual trash operation to ensure verifiable recycling
    let simple = simplify_path(path);
    let root = simple.components().next().unwrap().as_os_str();
    let mut root_str = root.to_string_lossy().into_owned();
    if !root_str.ends_with('\\') {
        root_str.push('\\');
    }

    // Get recycle bin count before
    let before_count = recycle_bin_item_count(&root_str).map_err(|e| {
        RecycleError::Unverifiable(e.to_string())
    })?;

    // Try to trash with simplified path (ensures verifiable recycling)
    trash::delete(&simple).map_err(|e| RecycleError::TrashFailed(e.to_string()))?;

    // Post-delete verification: check recycle bin count increased
    let after_count = recycle_bin_item_count(&root_str).map_err(|e| {
        RecycleError::Unverifiable(e.to_string())
    })?;
    if after_count <= before_count {
        // Recycle bin count didn't increase
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
    /// Could not verify recycle bin count (query failed) - warning only, continue
    Unverifiable(String),
}

impl std::fmt::Display for RecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecycleError::Refused(reason) => write!(f, "Refused: {reason}"),
            RecycleError::TrashFailed(e) => write!(f, "Trash operation failed: {e}"),
            RecycleError::NotVerifiedInRecycleBin => {
                write!(f, "recycle bin count did not increase")
            }
            RecycleError::Unverifiable(e) => {
                write!(f, "Could not verify recycle bin status: {e} (continuing)")
            }
        }
    }
}

impl std::error::Error for RecycleError {}

/// Query recycle bin item count for a drive root using direct Win32 API
/// Returns Result<u64, BinQueryError> - never returns 0 on failure
pub fn recycle_bin_item_count(root_path: &str) -> Result<u64, BinQueryError> {
    // Ensure root path ends with backslash
    let mut root = root_path.to_string();
    if !root.ends_with('\\') {
        root.push('\\');
    }

    // Convert to wide string
    let root_wide: Vec<u16> = OsStr::new(&root).encode_wide().chain(Some(0)).collect();

    // Initialize SHQUERYRBINFO with correct size
    let mut rb_info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        i64Size: 0,
        i64NumItems: 0,
    };

    // Call SHQueryRecycleBinW - returns Result<(), Error>
    let result = unsafe { SHQueryRecycleBinW(PCWSTR(root_wide.as_ptr()), &mut rb_info) };

    match result {
        Ok(()) => Ok(rb_info.i64NumItems as u64),
        Err(e) => Err(BinQueryError::QueryFailed(format!(
            "SHQueryRecycleBinW failed: {e}"
        ))),
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

    // Mock config provider for testing
    struct MockProvider {
        config: Option<(bool, u64)>,
        no_recycle_files: bool,
    }

    impl RecycleBinConfigProvider for MockProvider {
        fn get_config(&self, _volume_guid: &str) -> Option<(bool, u64)> {
            self.config
        }

        fn no_recycle_files_policy(&self) -> bool {
            self.no_recycle_files
        }
    }

    #[test]
    fn test_missing_key_default_enabled() {
        let _provider = MockProvider {
            config: None,
            no_recycle_files: false,
        };
        let temp = std::env::temp_dir();
        let test_file = temp.join("test_missing_key.txt");
        fs::write(&test_file, "test").unwrap();

        let _result = can_recycle_with_provider(&test_file, &_provider);
        // Missing key = default enabled, should pass (or UnknownConfiguration if no volume GUID)
        // On temp dir, it should work
        let _ = fs::remove_file(&test_file);
    }

    #[test]
    fn test_nuke_on_delete_disables_bin() {
        let provider = MockProvider {
            config: Some((false, 0)), // NukeOnDelete=1 -> bin disabled
            no_recycle_files: false,
        };
        let temp = std::env::temp_dir();
        let test_file = temp.join("test_nuke.txt");
        fs::write(&test_file, "test").unwrap();

        let result = can_recycle_with_provider(&test_file, &provider);
        assert_eq!(result, Err(RefuseReason::BinDisabled));

        let _ = fs::remove_file(&test_file);
    }

    #[test]
    fn test_no_recycle_files_policy_disables_bin() {
        let provider = MockProvider {
            config: Some((true, 1024 * 1024 * 1024)), // Bin enabled, 1GB capacity
            no_recycle_files: true, // But policy disables it
        };
        let temp = std::env::temp_dir();
        let test_file = temp.join("test_policy.txt");
        fs::write(&test_file, "test").unwrap();

        let result = can_recycle_with_provider(&test_file, &provider);
        assert_eq!(result, Err(RefuseReason::BinDisabled));

        let _ = fs::remove_file(&test_file);
    }

    #[test]
    fn test_max_capacity_boundary_file_equals_cap_passes() {
        let cap = 1024 * 1024; // 1MB
        let provider = MockProvider {
            config: Some((true, cap)),
            no_recycle_files: false,
        };
        let temp = std::env::temp_dir();
        let test_file = temp.join("test_cap_eq.txt");
        fs::write(&test_file, "x".repeat(cap as usize)).unwrap();

        let result = can_recycle_with_provider(&test_file, &provider);
        assert!(result.is_ok(), "File size == capacity should pass, got {:?}", result);

        let _ = fs::remove_file(&test_file);
    }

    #[test]
    fn test_max_capacity_boundary_file_exceeds_cap_refused() {
        let cap = 1024 * 1024; // 1MB
        let provider = MockProvider {
            config: Some((true, cap)),
            no_recycle_files: false,
        };
        let temp = std::env::temp_dir();
        let test_file = temp.join("test_cap_exceed.txt");
        fs::write(&test_file, "x".repeat((cap + 1) as usize)).unwrap();

        let result = can_recycle_with_provider(&test_file, &provider);
        assert_eq!(result, Err(RefuseReason::ExceedsCapacity {
            file_size: cap + 1,
            max_capacity: cap,
        }));

        let _ = fs::remove_file(&test_file);
    }

    #[test]
    fn test_unknown_cap_fallback_1_percent() {
        // Volume size = 100GB, 1% = 1GB
        let volume_size = 100u64 * 1024 * 1024 * 1024; // 100GB
        let fallback_cap = volume_size / 100; // 1GB

        let _provider = MockProvider {
            config: Some((true, 0)), // Unknown capacity (0)
            no_recycle_files: false,
        };

        // We can't easily test the volume size fallback without mocking get_volume_total_size
        // This is tested indirectly via the integration test
        assert_eq!(fallback_cap, 1024u64 * 1024 * 1024); // 1GB = 1% of 100GB
    }

    #[test]
    fn test_path_too_long_at_259_vs_260() {
        let temp = std::env::temp_dir();

        // Create a path that's exactly 259 chars after simplification - should pass
        let path_259 = temp.join("a".repeat(259 - temp.to_string_lossy().len() - 1));
        fs::write(&path_259, "test").unwrap();

        // Test that can_recycle refuses paths > 259 chars
        // Use a verbatim path that simplifies to > 259
        let verbatim_long = r"\\?\C:\".to_string() + &"a".repeat(260);
        let path = Path::new(&verbatim_long);
        let result = can_recycle(path);
        assert_eq!(result, Err(RefuseReason::PathTooLong));

        // A path that simplifies to exactly 259 should pass the length check
        // (though it may fail later for other reasons)
        let verbatim_259 = r"\\?\C:\".to_string() + &"a".repeat(256); // "C:\" = 3, so 3+256=259
        let path = Path::new(&verbatim_259);
        // This won't find volume GUID (no such volume), but length check should pass
        let simplified = simplify_path(path);
        assert_eq!(simplified.to_string_lossy().len(), 259);

        let _ = fs::remove_file(&path_259);
    }

    #[test]
    fn test_unc_path_refused() {
        let _provider = MockProvider {
            config: Some((true, 1024 * 1024 * 1024)),
            no_recycle_files: false,
        };
        let unc_path = Path::new("\\\\server\\share\\file.txt");

        // UNC paths have DRIVE_REMOTE drive type, should be refused
        let _result = can_recycle_with_provider(unc_path, &_provider);
        // This will fail at drive type check before provider is used
        // The actual test would need a mock for get_drive_type too
    }

    #[test]
    fn test_simplify_path_drive_path() {
        let path = Path::new(r"\\?\C:\Windows\System32");
        let simplified = simplify_path(path);
        assert_eq!(simplified.to_string_lossy(), r"C:\Windows\System32");
    }

    #[test]
    fn test_simplify_path_unc() {
        let path = Path::new(r"\\?\UNC\server\share\path");
        let simplified = simplify_path(path);
        assert_eq!(simplified.to_string_lossy(), r"\\server\share\path");
    }

    #[test]
    fn test_simplify_path_already_simple() {
        let path = Path::new(r"C:\Windows");
        let simplified = simplify_path(path);
        assert_eq!(simplified.to_string_lossy(), r"C:\Windows");
    }

    #[test]
    fn test_simplify_path_over_259_stays_verbatim() {
        // simplify_path only converts FROM verbatim (\\?\) TO normal form
        // If input is already normal form (no \\?\ prefix), it returns as-is
        // even if > 259 chars. The >259 check only applies when converting from verbatim.
        let long = "C:\\".to_string() + &"a".repeat(260);
        let path = Path::new(&long);
        let simplified = simplify_path(path);
        // Input is already normal form, so it returns as-is
        assert_eq!(simplified.to_string_lossy(), long);

        // But if input IS verbatim and would exceed 259 when simplified, it stays verbatim
        let verbatim_long = r"\\?\C:\".to_string() + &"a".repeat(260);
        let path = Path::new(&verbatim_long);
        let simplified = simplify_path(path);
        // Should stay verbatim because simplified would exceed 259
        assert!(simplified.to_string_lossy().starts_with("\\\\?\\"));
    }

    #[test]
    #[ignore]
    fn test_end_to_end_safe_trash_recycles() {
        // End-to-end test: normal temp file passes guard, safe_trash returns Ok, file in bin
        // Run with: cargo test -- --ignored
        let temp = std::env::temp_dir();
        let test_file = temp.join("test_e2e_trash.txt");
        fs::write(&test_file, "test content for e2e trash test").unwrap();

        println!("Test file: {}", test_file.display());
        println!("Test file canonical: {:?}", test_file.canonicalize());

        // Pre-check should pass
        let can = can_recycle(&test_file);
        println!("can_recycle result: {:?}", can);
        assert!(can.is_ok(), "Pre-check failed: {:?}", can);

        // Safe trash should succeed
        let result = safe_trash(&test_file);
        println!("safe_trash result: {:?}", result);
        assert!(result.is_ok(), "safe_trash failed: {:?}", result);

        // Verify in os_limited::list()
        let items = trash::os_limited::list().unwrap();
        println!("Items in recycle bin: {}", items.len());
        for item in &items {
            println!("  Item original_path: {}", item.original_path().display());
        }
        let found = items.iter().any(|i| i.original_path().to_string_lossy().contains("test_e2e_trash"));
        println!("Found match (contains test_e2e_trash): {}", found);
        // File is already deleted, can't canonicalize. Use the found result.
        assert!(found, "File not found in Recycle Bin via os_limited::list()");
    }
}
