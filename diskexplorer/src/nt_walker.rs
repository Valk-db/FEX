//! Windows NtQueryDirectoryFileEx-based directory walker
//!
//! Replaces jwalk with a native Windows implementation using
//! FileIdExtdDirectoryInformation for faster scans and richer metadata.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows::Wdk::Storage::FileSystem::{
    FILE_ID_EXTD_DIR_INFORMATION, FILE_INFORMATION_CLASS, FileIdExtdDirectoryInformation,
    NtQueryDirectoryFileEx,
};
use windows::Win32::Foundation::{
    CloseHandle, GENERIC_READ, NTSTATUS, STATUS_NO_MORE_FILES,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;
use windows::core::PCWSTR;

/// Type aliases for complex scan data structures
type FileRecord = (PathBuf, u64, u64, i64, [u8; 16], u32, bool, bool, u32);
type BaselineEntry = (u64, u64, i64, [u8; 16], u32, bool, bool, u32);

/// Drive type constants (also used in recycle_guard.rs)
const _DRIVE_UNKNOWN: u32 = 0;
const _DRIVE_NO_ROOT_DIR: u32 = 1;
const _DRIVE_REMOVABLE: u32 = 2;
const _DRIVE_FIXED: u32 = 3;
const _DRIVE_REMOTE: u32 = 4;
const _DRIVE_CDROM: u32 = 5;
const _DRIVE_RAMDISK: u32 = 6;

/// File attribute constants for cloud placeholders
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x00400000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x00040000;
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x00001000;

/// Reparse tag constants (for future use in reparse point handling)
const _IO_REPARSE_TAG_SYMLINK: u32 = 0xA000000C;
const _IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA0000003;
const _IO_REPARSE_TAG_HSM: u32 = 0xC0000004;
const _IO_REPARSE_TAG_HSM2: u32 = 0x80000006;

/// Entry data collected by Nt walker
#[derive(Debug, Clone)]
pub struct NtEntry {
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_reparse: bool,
    pub reparse_tag: u32,
    pub is_cloud: bool,
    pub logical_size: u64,
    pub allocated_size: u64,
    pub file_id: [u8; 16],
    pub volume_serial: u32,
    pub attributes: u32,
    pub mtime: i64,
}

/// Results from a single scan
#[derive(Debug, Clone)]
pub struct ScanResult {
    pub file_count: u64,
    pub dir_count: u64,
    pub total_logical_bytes: u64,
    pub total_allocated_bytes: u64,
    pub unreadable_count: u64,
    pub unreadable_bytes: u64,
}

/// Read a single directory using NtQueryDirectoryFileEx
fn read_dir_nt(dir: &Path, volume_serial: u32) -> Vec<NtEntry> {
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

        if status == NTSTATUS(STATUS_NO_MORE_FILES.0) {
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
                    let is_reparse = (info.FileAttributes & 0x400) != 0; // FILE_ATTRIBUTE_REPARSE_POINT
                    let is_cloud = (info.FileAttributes & FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0
                        || (info.FileAttributes & FILE_ATTRIBUTE_RECALL_ON_OPEN) != 0
                        || (info.FileAttributes & FILE_ATTRIBUTE_OFFLINE) != 0;

                    // EndOfFile and AllocationSize are i64 (LARGE_INTEGER)
                    let logical_size = info.EndOfFile as u64;
                    let allocated_size = info.AllocationSize as u64;

                    // File ID is 128-bit
                    let mut file_id = [0u8; 16];
                    file_id.copy_from_slice(&info.FileId.Identifier);

                    // Mtime from ChangeTime (100ns intervals since 1601)
                    let mtime_100ns = info.ChangeTime as u64;
                    const WINDOWS_TICKS_PER_SEC: u64 = 10_000_000;
                    const WINDOWS_TO_UNIX_EPOCH: u64 = 11_644_473_600_000_000;
                    let mtime_unix = if mtime_100ns > WINDOWS_TO_UNIX_EPOCH {
                        ((mtime_100ns - WINDOWS_TO_UNIX_EPOCH) / WINDOWS_TICKS_PER_SEC) as i64
                    } else {
                        0
                    };

                    results.push(NtEntry {
                        path,
                        is_dir,
                        is_reparse,
                        reparse_tag: info.ReparsePointTag,
                        is_cloud,
                        logical_size,
                        allocated_size,
                        file_id,
                        volume_serial,
                        attributes: info.FileAttributes,
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

    unsafe { let _ = CloseHandle(dir_handle); };
    results
}

/// Get volume serial number for a path
fn get_volume_serial(path: &Path) -> u32 {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
    use windows::core::PCWSTR;

    let root = path.components().next().unwrap().as_os_str();
    let mut root_os = root.to_os_string();
    if !root_os.to_string_lossy().ends_with('\\') {
        root_os.push("\\");
    }
    let wide: Vec<u16> = root_os.encode_wide().chain(Some(0)).collect();

    let mut vol_serial = 0u32;
    let mut max_comp_len = 0u32;
    let mut fs_flags = 0u32;
    let mut fs_name = vec![0u16; 260];

    let result = unsafe {
        GetVolumeInformationW(
            PCWSTR(wide.as_ptr()),
            None,
            Some(&mut vol_serial),
            Some(&mut max_comp_len),
            Some(&mut fs_flags),
            Some(fs_name.as_mut_slice()),
        )
    };

    if result.is_ok() {
        vol_serial
    } else {
        0
    }
}

/// Walk `root` using NtQueryDirectoryFileEx, parallelized with a work queue
pub fn scan_nt(root: &Path) -> std::io::Result<ScanResult> {
    let root = root.canonicalize()?;
    let volume_serial = get_volume_serial(&root);

    let mut dir_sizes_logical: std::collections::HashMap<PathBuf, u64> = std::collections::HashMap::new();
    let mut dir_sizes_allocated: std::collections::HashMap<PathBuf, u64> = std::collections::HashMap::new();
    let mut files: Vec<FileRecord> = Vec::new(); // path, logical, allocated, mtime, file_id, vol_serial, is_reparse, is_cloud, reparse_tag
    let mut file_count = 0u64;
    let mut dir_count = 0u64;
    let mut total_logical = 0u64;
    let mut total_allocated = 0u64;
    let mut unreadable_count = 0u64;
    let mut unreadable_bytes = 0u64;

    // Use a work queue for directory traversal
    let mut dirs_to_process: Vec<PathBuf> = vec![root.to_path_buf()];

    while let Some(dir) = dirs_to_process.pop() {
        let entries = read_dir_nt(&dir, volume_serial);
        for entry in entries {
            if entry.is_dir {
                dirs_to_process.push(entry.path.clone());
                dir_count += 1;
                dir_sizes_logical.entry(entry.path.clone()).or_insert(0);
                dir_sizes_allocated.entry(entry.path.clone()).or_insert(0);
            } else {
                file_count += 1;

                // Track unreadable/reparse/cloud files
                if entry.is_reparse {
                    unreadable_count += 1;
                    // Reparse points contribute 0 to size totals
                } else if entry.is_cloud {
                    // Cloud placeholders: don't count in reclaimable, but track
                    unreadable_count += 1;
                    unreadable_bytes += entry.logical_size;
                } else {
                    total_logical += entry.logical_size;
                    total_allocated += entry.allocated_size;

                    // Add to all ancestors (logical size)
                    let mut ancestor = entry.path.parent();
                    while let Some(dir) = ancestor {
                        *dir_sizes_logical.entry(dir.to_path_buf()).or_insert(0) += entry.logical_size;
                        *dir_sizes_allocated.entry(dir.to_path_buf()).or_insert(0) += entry.allocated_size;
                        if dir == root {
                            break;
                        }
                        ancestor = dir.parent();
                    }
                }

                files.push((
                    entry.path,
                    entry.logical_size,
                    entry.allocated_size,
                    entry.mtime,
                    entry.file_id,
                    entry.volume_serial,
                    entry.is_reparse,
                    entry.is_cloud,
                    entry.reparse_tag,
                ));
            }
        }
    }

    Ok(ScanResult {
        file_count,
        dir_count,
        total_logical_bytes: total_logical,
        total_allocated_bytes: total_allocated,
        unreadable_count,
        unreadable_bytes,
    })
}

/// Streaming scan events for progressive UI updates
#[derive(Debug, Clone)]
pub enum NtScanEvent {
    Files(Vec<FileRecord>),
    Changed(Vec<FileRecord>),
    Deleted(Vec<(PathBuf, u64)>),
    Progress(u64),
    Dir(PathBuf),
}

/// Spawn a streaming scan on a background thread
pub fn spawn_scan_nt(
    root: PathBuf,
    baseline: Option<std::collections::HashMap<PathBuf, BaselineEntry>>,
) -> std::sync::mpsc::Receiver<NtScanEvent> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut alive = true;
        macro_rules! send_or_stop {
            ($ev:expr) => {
                if tx.send($ev).is_err() {
                    alive = false;
                }
            };
        }

        let root = match root.canonicalize() {
            Ok(r) => r,
            Err(_) => return,
        };
        let volume_serial = get_volume_serial(&root);

        let mut full_batch = Vec::new();
        let mut changed_batch = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut since_progress = 0u64;

        let mut dirs_to_process: Vec<PathBuf> = vec![root.clone()];

        while let Some(dir) = dirs_to_process.pop() {
            if !alive { break; }

            let entries = read_dir_nt(&dir, volume_serial);
            for entry in entries {
                if !alive { break; }

                if entry.is_dir {
                    send_or_stop!(NtScanEvent::Dir(entry.path.clone()));
                    dirs_to_process.push(entry.path.clone());
                    continue;
                }

                let key = entry.path.clone();
                let val = (entry.logical_size, entry.allocated_size, entry.mtime, entry.file_id, entry.volume_serial, entry.is_reparse, entry.is_cloud, entry.reparse_tag);

                match &baseline {
                    None => {
                        full_batch.push((key, val.0, val.1, val.2, val.3, val.4, val.5, val.6, val.7));
                        if full_batch.len() >= 500 {
                            send_or_stop!(NtScanEvent::Files(std::mem::take(&mut full_batch)));
                        }
                    }
                    Some(base) => {
                        seen.insert(key.clone());
                        let unchanged = base
                            .get(&key)
                            .map(|(ls, als, m, fid, vs, irp, icl, rpt)| {
                                *ls == val.0 && *als == val.1 && *m == val.2 && *fid == val.3 && *vs == val.4 && *irp == val.5 && *icl == val.6 && *rpt == val.7
                            })
                            .unwrap_or(false);
                        if !unchanged {
                            changed_batch.push((key, val.0, val.1, val.2, val.3, val.4, val.5, val.6, val.7));
                            if changed_batch.len() >= 500 {
                                send_or_stop!(NtScanEvent::Changed(std::mem::take(&mut changed_batch)));
                            }
                        }
                        since_progress += 1;
                        if since_progress >= 2000 {
                            send_or_stop!(NtScanEvent::Progress(since_progress));
                            since_progress = 0;
                        }
                    }
                }
            }
        }

        if !alive { return; }

        match baseline {
            None => {
                if !full_batch.is_empty() {
                    let _ = tx.send(NtScanEvent::Files(full_batch));
                }
            }
            Some(base) => {
                if !changed_batch.is_empty() {
                    let _ = tx.send(NtScanEvent::Changed(changed_batch));
                }
                if since_progress > 0 {
                    let _ = tx.send(NtScanEvent::Progress(since_progress));
                }
                let deleted: Vec<(PathBuf, u64)> = base
                    .iter()
                    .filter(|(p, _)| !seen.contains(*p))
                    .map(|(p, (ls, _, _, _, _, _, _, _))| (p.clone(), *ls))
                    .collect();
                if !deleted.is_empty() {
                    let _ = tx.send(NtScanEvent::Deleted(deleted));
                }
            }
        }
    });
    rx
}

/// Check if a path is a reparse point (junction, symlink, etc.)
pub fn is_reparse_point(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::GetFileAttributesW;
    use windows::core::PCWSTR;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let attrs = unsafe { GetFileAttributesW(PCWSTR(wide.as_ptr())) };
    attrs != u32::MAX && (attrs & 0x400) != 0 // FILE_ATTRIBUTE_REPARSE_POINT
}