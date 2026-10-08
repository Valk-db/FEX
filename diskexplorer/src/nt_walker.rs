//! Windows NtQueryDirectoryFileEx-based directory walker
//!
//! Replaces jwalk with a native Windows implementation using
//! FileIdExtdDirectoryInformation for faster scans and richer metadata.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{BaselineEntry, FileRecord, ScanData};
use rayon::ThreadPoolBuilder;
use windows::Wdk::Storage::FileSystem::{
    FILE_ID_EXTD_DIR_INFORMATION, FILE_INFORMATION_CLASS, FileIdExtdDirectoryInformation,
    NtQueryDirectoryFileEx,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, NTSTATUS, STATUS_NO_MORE_FILES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;
use windows::core::PCWSTR;

/// Simplify Windows verbatim paths (\\?\X:\...) to normal form.
/// Used for UI display, trash, GetDriveTypeW, and registry lookups.
pub fn simplify_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        // \\?\C:\... -> C:\...
        // \\?\UNC\server\share\... -> \\server\share\...
        if let Some(unc_stripped) = stripped.strip_prefix("UNC\\") {
            let mut p = PathBuf::from("\\\\");
            p.push(unc_stripped);
            // If simplified path would exceed 259 chars, keep verbatim
            if p.to_string_lossy().len() > 259 {
                return path.to_path_buf();
            }
            return p;
        } else {
            let p = PathBuf::from(stripped);
            if p.to_string_lossy().len() > 259 {
                return path.to_path_buf();
            }
            return p;
        }
    }
    path.to_path_buf()
}

/// Thread count for parallel Nt walker (logical cores)
/// 0 = use rayon default (logical cores)
const THREAD_COUNT: usize = 0;

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
/// Returns (entries, unreadable_dir_errors) where errors is a list of (path, NTSTATUS) for failed opens
fn read_dir_nt(dir: &Path, volume_serial: u32) -> (Vec<NtEntry>, Vec<(PathBuf, NTSTATUS)>) {
    let mut results = Vec::new();
    let mut unreadable_dirs = Vec::new();

    let dir_wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let dir_handle = unsafe {
        CreateFileW(
            PCWSTR(dir_wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    };

    let dir_handle = match dir_handle {
        Ok(h) => h,
        Err(_) => {
            unreadable_dirs.push((dir.to_path_buf(), NTSTATUS(0xC000000Du32 as i32))); // STATUS_INVALID_PARAMETER as placeholder
            return (results, unreadable_dirs);
        }
    };

    // Use aligned buffer for FILE_ID_EXTD_DIR_INFORMATION
    let mut buffer = vec![0u64; 8192]; // 64KB aligned to 8 bytes
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
                buffer.len() as u32 * 8,
                FILE_INFORMATION_CLASS(FileIdExtdDirectoryInformation.0),
                0,
                None,
            )
        };

        if status == NTSTATUS(STATUS_NO_MORE_FILES.0) {
            break;
        }
        if status != NTSTATUS(0) {
            if status != NTSTATUS(0x80000005u32 as i32) && status != NTSTATUS(0x80000006u32 as i32)
            {
                // Not STATUS_BUFFER_OVERFLOW or STATUS_BUFFER_TOO_SMALL - real error
                unreadable_dirs.push((dir.to_path_buf(), status));
            }
            break;
        }

        // Parse the buffer - FILE_ID_EXTD_DIR_INFORMATION structures
        let mut offset = 0;
        while offset < iosb.Information {
            // SAFETY: buffer is properly aligned for FILE_ID_EXTD_DIR_INFORMATION
            // and we only read within the valid range returned by iosb.Information
            let info = unsafe {
                &*(buffer.as_ptr().add(offset / 8) as *const FILE_ID_EXTD_DIR_INFORMATION)
            };

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

                    // Check reparse point BEFORE checking is_dir
                    // Reparse directories are emitted as entries (with is_reparse=true) and NOT descended
                    let is_reparse = (info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0) != 0;
                    let is_dir = (info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0) != 0;
                    let is_cloud = (info.FileAttributes & FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
                        != 0
                        || (info.FileAttributes & FILE_ATTRIBUTE_RECALL_ON_OPEN) != 0
                        || (info.FileAttributes & FILE_ATTRIBUTE_OFFLINE) != 0;

                    // EndOfFile and AllocationSize are i64 (LARGE_INTEGER)
                    let logical_size = info.EndOfFile as u64;
                    let allocated_size = info.AllocationSize as u64;

                    // File ID is 128-bit
                    let mut file_id = [0u8; 16];
                    file_id.copy_from_slice(&info.FileId.Identifier);

                    // Mtime from LastWriteTime (not ChangeTime)
                    let mtime_100ns = info.LastWriteTime as u64;
                    const WINDOWS_TICKS_PER_SEC: u64 = 10_000_000;
                    const WINDOWS_TO_UNIX_EPOCH: u64 = 11_644_473_600_000_000;
                    let mtime_unix = if mtime_100ns > WINDOWS_TO_UNIX_EPOCH {
                        ((mtime_100ns - WINDOWS_TO_UNIX_EPOCH) / WINDOWS_TICKS_PER_SEC) as i64
                    } else {
                        0
                    };

                    // For reparse directories, we still emit them but mark as dir + reparse
                    // They will be displayed with 0 size and never descended into
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

    unsafe {
        let _ = CloseHandle(dir_handle);
    };
    (results, unreadable_dirs)
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

    if result.is_ok() { vol_serial } else { 0 }
}

/// Walk `root` using NtQueryDirectoryFileEx, parallelized with rayon work-stealing
/// Returns ScanResult with summary counts (for streaming scan use cases)
pub fn scan_nt(root: &Path) -> std::io::Result<ScanResult> {
    let (result, _dir_sizes_logical, _dir_sizes_allocated, _files) = scan_nt_internal(root)?;
    Ok(result)
}

/// Full scan returning ScanData with all fields populated (for sync scan in App::new)
pub fn scan_nt_full(root: &Path) -> std::io::Result<ScanData> {
    let (result, dir_sizes_logical, dir_sizes_allocated, files) = scan_nt_internal(root)?;
    Ok(ScanData {
        dir_sizes_logical,
        dir_sizes_allocated,
        files,
        file_count: result.file_count,
        dir_count: result.dir_count,
        total_logical_bytes: result.total_logical_bytes,
        total_allocated_bytes: result.total_allocated_bytes,
        unreadable_count: result.unreadable_count,
        unreadable_bytes: result.unreadable_bytes,
    })
}

/// Internal scan results type
type InternalScanResult = (
    ScanResult,
    std::collections::HashMap<PathBuf, u64>,
    std::collections::HashMap<PathBuf, u64>,
    Vec<FileRecord>,
);

/// Internal implementation shared by scan_nt and scan_nt_full
fn scan_nt_internal(root: &Path) -> std::io::Result<InternalScanResult> {
    let root = root.canonicalize()?;
    let volume_serial = get_volume_serial(&root);

    // Collect all directories first (single-threaded breadth-first to build the work list)
    let mut all_dirs = Vec::new();
    let mut dirs_to_scan = vec![root.to_path_buf()];
    let mut unreadable_dirs_total = Vec::new();

    while let Some(dir) = dirs_to_scan.pop() {
        let (entries, unreadable_dirs) = read_dir_nt(&dir, volume_serial);
        unreadable_dirs_total.extend(unreadable_dirs);

        for entry in entries {
            if entry.is_dir && !entry.is_reparse {
                // Only descend into non-reparse directories
                dirs_to_scan.push(entry.path.clone());
            }
        }
        // Always add the directory to all_dirs for counting
        all_dirs.push(dir);
    }

    // Now process all directories in parallel
    let pool = if THREAD_COUNT != 0 {
        ThreadPoolBuilder::new()
            .num_threads(THREAD_COUNT)
            .build()
            .unwrap()
    } else {
        ThreadPoolBuilder::new().build().unwrap()
    };

    // Shared accumulators
    let dir_sizes_logical = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
        PathBuf,
        u64,
    >::new()));
    let dir_sizes_allocated = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
        PathBuf,
        u64,
    >::new()));
    let files = Arc::new(std::sync::Mutex::new(Vec::<FileRecord>::new()));
    let file_count = Arc::new(AtomicU64::new(0));
    let total_logical = Arc::new(AtomicU64::new(0));
    let total_allocated = Arc::new(AtomicU64::new(0));
    let unreadable_count = Arc::new(AtomicU64::new(0));
    let unreadable_bytes = Arc::new(AtomicU64::new(0));

    pool.install(|| {
        use rayon::prelude::*;
        all_dirs.par_iter().for_each(|dir| {
            let (entries, _) = read_dir_nt(dir, volume_serial);
            for entry in entries {
                if entry.is_dir {
                    // Directories are already in all_dirs, just initialize their size entries
                    dir_sizes_logical
                        .lock()
                        .unwrap()
                        .entry(entry.path.clone())
                        .or_insert(0);
                    dir_sizes_allocated
                        .lock()
                        .unwrap()
                        .entry(entry.path.clone())
                        .or_insert(0);
                } else {
                    file_count.fetch_add(1, Ordering::Relaxed);

                    if entry.is_reparse {
                        unreadable_count.fetch_add(1, Ordering::Relaxed);
                        // Still add to files with zero size
                        files.lock().unwrap().push((
                            entry.path,
                            0, // logical_size = 0 for reparse
                            0, // allocated_size = 0 for reparse
                            entry.mtime,
                            entry.file_id,
                            entry.volume_serial,
                            entry.is_reparse,
                            entry.is_cloud,
                            entry.reparse_tag,
                        ));
                    } else if entry.is_cloud {
                        unreadable_count.fetch_add(1, Ordering::Relaxed);
                        unreadable_bytes.fetch_add(entry.logical_size, Ordering::Relaxed);
                        // Cloud placeholders - zero contributed size
                        files.lock().unwrap().push((
                            entry.path,
                            0,
                            0,
                            entry.mtime,
                            entry.file_id,
                            entry.volume_serial,
                            entry.is_reparse,
                            entry.is_cloud,
                            entry.reparse_tag,
                        ));
                    } else {
                        total_logical.fetch_add(entry.logical_size, Ordering::Relaxed);
                        total_allocated.fetch_add(entry.allocated_size, Ordering::Relaxed);

                        // Add to all ancestors
                        let mut ancestor = entry.path.parent();
                        while let Some(dir) = ancestor {
                            dir_sizes_logical
                                .lock()
                                .unwrap()
                                .entry(dir.to_path_buf())
                                .or_insert(0);
                            dir_sizes_allocated
                                .lock()
                                .unwrap()
                                .entry(dir.to_path_buf())
                                .or_insert(0);
                            *dir_sizes_logical.lock().unwrap().get_mut(dir).unwrap() +=
                                entry.logical_size;
                            *dir_sizes_allocated.lock().unwrap().get_mut(dir).unwrap() +=
                                entry.allocated_size;
                            if dir == root {
                                break;
                            }
                            ancestor = dir.parent();
                        }

                        files.lock().unwrap().push((
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
        });
    });

    // Extract results from mutexes
    let dir_sizes_logical_map = Arc::try_unwrap(dir_sizes_logical)
        .unwrap()
        .into_inner()
        .unwrap();
    let dir_sizes_allocated_map = Arc::try_unwrap(dir_sizes_allocated)
        .unwrap()
        .into_inner()
        .unwrap();
    let files_vec = Arc::try_unwrap(files).unwrap().into_inner().unwrap();

    let result = ScanResult {
        file_count: file_count.load(Ordering::Relaxed),
        dir_count: all_dirs.len() as u64,
        total_logical_bytes: total_logical.load(Ordering::Relaxed),
        total_allocated_bytes: total_allocated.load(Ordering::Relaxed),
        unreadable_count: unreadable_count.load(Ordering::Relaxed)
            + unreadable_dirs_total.len() as u64,
        unreadable_bytes: unreadable_bytes.load(Ordering::Relaxed),
    };

    Ok((
        result,
        dir_sizes_logical_map,
        dir_sizes_allocated_map,
        files_vec,
    ))
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

/// Spawn a streaming scan on a background thread using parallel Nt walker
pub fn spawn_scan_nt(
    root: PathBuf,
    baseline: Option<std::collections::HashMap<PathBuf, BaselineEntry>>,
) -> std::sync::mpsc::Receiver<NtScanEvent> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let root = match root.canonicalize() {
            Ok(r) => r,
            Err(_) => return,
        };
        let volume_serial = get_volume_serial(&root);

        // First pass: collect all directories (breadth-first, single-threaded)
        let mut all_dirs = Vec::new();
        let mut dirs_to_scan = vec![root.clone()];
        let mut unreadable_dirs = Vec::new();

        while let Some(dir) = dirs_to_scan.pop() {
            let (entries, unreadable) = read_dir_nt(&dir, volume_serial);
            unreadable_dirs.extend(unreadable);
            for entry in entries {
                if entry.is_dir && !entry.is_reparse {
                    dirs_to_scan.push(entry.path.clone());
                }
            }
            all_dirs.push(dir);
        }

        // Log unreadable dirs
        for (dir_path, status) in unreadable_dirs {
            eprintln!("Unreadable dir: {} status={:?}", dir_path.display(), status);
        }

        // Second pass: process all directories in parallel, sending events via channel
        let pool = if THREAD_COUNT != 0 {
            ThreadPoolBuilder::new()
                .num_threads(THREAD_COUNT)
                .build()
                .unwrap()
        } else {
            ThreadPoolBuilder::new().build().unwrap()
        };

        let (event_tx, event_rx) = std::sync::mpsc::channel::<NtScanEvent>();

        // For diff scan, we need shared seen set - use Arc<Mutex<HashSet>>
        let seen_arc = if baseline.is_some() {
            Some(Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )))
        } else {
            None
        };

        pool.install(|| {
            use rayon::prelude::*;
            all_dirs.par_iter().for_each(|dir| {
                let (entries, _) = read_dir_nt(dir, volume_serial);
                for entry in entries {
                    if entry.is_dir {
                        let _ = event_tx.send(NtScanEvent::Dir(entry.path.clone()));
                    } else {
                        let key = entry.path.clone();
                        let val = (
                            entry.logical_size,
                            entry.allocated_size,
                            entry.mtime,
                            entry.file_id,
                            entry.volume_serial,
                            entry.is_reparse,
                            entry.is_cloud,
                            entry.reparse_tag,
                        );

                        match &baseline {
                            None => {
                                let _ = event_tx.send(NtScanEvent::Files(vec![(
                                    key, val.0, val.1, val.2, val.3, val.4, val.5, val.6, val.7,
                                )]));
                            }
                            Some(base) => {
                                // Thread-safe check for seen
                                if let Some(ref seen_mutex) = seen_arc {
                                    let mut seen = seen_mutex.lock().unwrap();
                                    if !seen.insert(key.clone()) {
                                        continue; // already processed this path
                                    }
                                }
                                let unchanged = base
                                    .get(&key)
                                    .map(|entry| {
                                        entry.logical_size == val.0
                                            && entry.allocated_size == val.1
                                            && entry.mtime == val.2
                                            && entry.file_id == val.3
                                            && entry.volume_serial == val.4
                                            && entry.is_reparse == val.5
                                            && entry.is_cloud == val.6
                                            && entry.reparse_tag == val.7
                                    })
                                    .unwrap_or(false);
                                if !unchanged {
                                    let _ = event_tx.send(NtScanEvent::Changed(vec![(
                                        key, val.0, val.1, val.2, val.3, val.4, val.5, val.6, val.7,
                                    )]));
                                }
                            }
                        }
                    }
                }
            });
        });

        // Drop the sender so the channel closes
        drop(event_tx);

        // Collect and batch events from the channel
        let mut collected: Vec<NtScanEvent> = Vec::new();
        for ev in event_rx {
            collected.push(ev);
        }

        // Batch the collected events
        match &baseline {
            None => {
                // Full scan: batch Files events
                let mut batch = Vec::new();
                for ev in collected {
                    if let NtScanEvent::Files(files) = ev {
                        batch.extend(files);
                        if batch.len() >= 500
                            && tx
                                .send(NtScanEvent::Files(std::mem::take(&mut batch)))
                                .is_err()
                        {
                            return;
                        }
                    } else if let NtScanEvent::Dir(path) = ev
                        && tx.send(NtScanEvent::Dir(path)).is_err()
                    {
                        return;
                    }
                }
                if !batch.is_empty() {
                    let _ = tx.send(NtScanEvent::Files(batch));
                }
            }
            Some(base) => {
                // Diff scan: batch Changed events
                let mut batch = Vec::new();
                let mut seen_paths = std::collections::HashSet::new();
                for ev in collected {
                    if let NtScanEvent::Changed(files) = ev {
                        for file in files {
                            if seen_paths.insert(file.0.clone()) {
                                batch.push(file);
                                if batch.len() >= 500
                                    && tx
                                        .send(NtScanEvent::Changed(std::mem::take(&mut batch)))
                                        .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    } else if let NtScanEvent::Dir(path) = ev
                        && tx.send(NtScanEvent::Dir(path)).is_err()
                    {
                        return;
                    }
                }
                if !batch.is_empty() {
                    let _ = tx.send(NtScanEvent::Changed(batch));
                }

                // Send Progress (total files processed)
                let total_processed = seen_paths.len() as u64;
                if total_processed > 0 && tx.send(NtScanEvent::Progress(total_processed)).is_err() {
                    return;
                }

                // Send Deleted
                let deleted: Vec<(PathBuf, u64)> = base
                    .iter()
                    .filter(|(p, _)| !seen_paths.contains(*p))
                    .map(|(p, entry)| (p.clone(), entry.logical_size))
                    .collect();
                if !deleted.is_empty() && tx.send(NtScanEvent::Deleted(deleted)).is_err() {
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
