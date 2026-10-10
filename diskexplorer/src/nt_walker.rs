//! Windows NtQueryDirectoryFileEx-based directory walker
//!
//! Replaces jwalk with a native Windows implementation using
//! FileIdExtdDirectoryInformation for faster scans and richer metadata.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// DirId arena for O(1) ancestor propagation without PathBuf hashing
#[derive(Default, Debug)]
struct DirArena {
    // path -> dir_id (assigned in pass 1)
    path_to_id: std::collections::HashMap<PathBuf, u32>,
    // dir_id -> parent_dir_id (None for root)
    parents: Vec<Option<u32>>,
    // dir_id -> logical size sum of own files
    own_logical: Vec<u64>,
    // dir_id -> allocated size sum of own files
    own_allocated: Vec<u64>,
    // dir_id -> path (for final conversion to HashMap)
    paths: Vec<PathBuf>,
}

impl DirArena {
    fn new() -> Self {
        Self::default()
    }

    /// Assign or get DirId for a path. Returns (dir_id, is_new).
    fn get_or_assign(&mut self, path: PathBuf) -> (u32, bool) {
        if let Some(&id) = self.path_to_id.get(&path) {
            return (id, false);
        }
        let id = self.paths.len() as u32;
        self.path_to_id.insert(path.clone(), id);
        self.paths.push(path);
        self.parents.push(None);
        self.own_logical.push(0);
        self.own_allocated.push(0);
        (id, true)
    }

    /// Set parent for a dir_id (called in pass 1)
    fn set_parent(&mut self, child_id: u32, parent_id: u32) {
        if (child_id as usize) < self.parents.len() {
            self.parents[child_id as usize] = Some(parent_id);
        }
    }

    /// Add size to a directory's own totals (called by workers in pass 2)
    #[inline]
    fn add_own_size(&mut self, dir_id: u32, logical: u64, allocated: u64) {
        let idx = dir_id as usize;
        if idx < self.own_logical.len() {
            self.own_logical[idx] = self.own_logical[idx].saturating_add(logical);
            self.own_allocated[idx] = self.own_allocated[idx].saturating_add(allocated);
        }
    }

    /// Propagate all own sizes to ancestors (single-threaded, after pass 2 merge)
    fn propagate_to_ancestors(&mut self) {
        // Process in reverse order (children before parents) so propagation works
        // We already know the topological order from pass 1 BFS (all_dirs vector)
        // But here we just iterate all dirs and propagate
        for id in (0..self.paths.len()).rev() {
            if let Some(parent) = self.parents[id] {
                let p = parent as usize;
                if p < self.own_logical.len() {
                    self.own_logical[p] = self.own_logical[p].saturating_add(self.own_logical[id]);
                    self.own_allocated[p] = self.own_allocated[p].saturating_add(self.own_allocated[id]);
                }
            }
        }
    }

    /// Convert to HashMap<PathBuf, u64> for ScanData
    fn to_hashmaps(&self) -> (std::collections::HashMap<PathBuf, u64>, std::collections::HashMap<PathBuf, u64>) {
        let mut logical = std::collections::HashMap::with_capacity(self.paths.len());
        let mut allocated = std::collections::HashMap::with_capacity(self.paths.len());
        for (i, path) in self.paths.iter().enumerate() {
            logical.insert(path.clone(), self.own_logical[i]);
            allocated.insert(path.clone(), self.own_allocated[i]);
        }
        (logical, allocated)
    }

    fn len(&self) -> usize {
        self.paths.len()
    }
}

/// Thread-local accumulator for single-pass walker
#[derive(Default)]
struct ThreadLocalAccum {
    files: Vec<FileRecord>,
    // Use DirId indices instead of PathBuf keys for zero-hashing dir size accumulation
    dir_sizes_logical: Vec<u64>,
    dir_sizes_allocated: Vec<u64>,
    file_count: u64,
    total_logical: u64,
    total_allocated: u64,
    hardlink_siblings: u64,
    reparse_skipped: u64,
    cloud_skipped: u64,
    unreadable_bytes: u64,
    dir_count: u64,
}

use crate::{BaselineEntry, FileRecord, ScanData};
use crate::profile::ScanProfile;
use rayon::prelude::*;
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
/// Can be overridden via DISKEXPLORER_NT_THREADS env var
fn get_thread_count() -> usize {
    std::env::var("DISKEXPLORER_NT_THREADS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

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
    // Split counters for testability and UI
    pub hardlink_siblings: u64,
    pub reparse_skipped: u64,
    pub cloud_skipped: u64,
    pub unreadable_dirs: u64,
    pub unreadable_files: u64,
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
        hardlink_siblings: result.hardlink_siblings,
        reparse_skipped: result.reparse_skipped,
        cloud_skipped: result.cloud_skipped,
        unreadable_dirs: result.unreadable_dirs,
        unreadable_files: result.unreadable_files,
    })
}

/// Internal scan results type
type InternalScanResult = (
    ScanResult,
    std::collections::HashMap<PathBuf, u64>,
    std::collections::HashMap<PathBuf, u64>,
    Vec<FileRecord>,
);

/// Single-pass parallel walker using rayon work-stealing
/// Each directory is enumerated EXACTLY ONCE.
fn scan_nt_internal(root: &Path) -> std::io::Result<InternalScanResult> {
    let profile = if ScanProfile::enabled() { Some(ScanProfile::new()) } else { None };
    let total_start = profile.as_ref().map(|_| std::time::Instant::now());

    let root = root.canonicalize()?;
    let volume_serial = get_volume_serial(&root);

    let thread_count = get_thread_count();
    let _pool = if thread_count != 0 {
        ThreadPoolBuilder::new()
            .num_threads(thread_count)
            .build()
            .unwrap()
    } else {
        ThreadPoolBuilder::new().build().unwrap()
    };

    // Pass 1: collect all directories (single-threaded BFS) and build DirArena
    // We assign DirIds and track parent relationships for O(1) ancestor propagation
    let mut arena = DirArena::new();
    let mut dirs_to_scan = vec![root.to_path_buf()];
    let mut unreadable_dirs_total = Vec::new();
    let mut all_dirs_ordered = Vec::new(); // For final topological order (children before parents)

    let pass1_start = profile.as_ref().map(|_| std::time::Instant::now());

    // Assign root DirId
    let (root_id, _) = arena.get_or_assign(root.to_path_buf());

    while let Some(dir) = dirs_to_scan.pop() {
        let (entries, unreadable_dirs) = read_dir_nt(&dir, volume_serial);
        unreadable_dirs_total.extend(unreadable_dirs);

        if let Some(p) = &profile {
            p.pass1_dir_count.fetch_add(1, Ordering::Relaxed);
            p.pass1_file_count.fetch_add(entries.len() as u64, Ordering::Relaxed);
        }

        // Get parent DirId for this directory
        let parent_id = arena.path_to_id.get(&dir).copied().unwrap_or(root_id);

        for entry in entries {
            if entry.is_dir && !entry.is_reparse {
                dirs_to_scan.push(entry.path.clone());
                // Assign DirId for subdirectory and link to parent
                let (child_id, _) = arena.get_or_assign(entry.path.clone());
                arena.set_parent(child_id, parent_id);
            }
        }
        all_dirs_ordered.push(dir);
    }

    if let (Some(p), Some(start)) = (&profile, pass1_start) {
        p.pass1_enumeration_ns.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    // Pre-size thread-local vectors to match arena size (known after pass 1)
    let dir_count = arena.len();

    // Pass 2: process all directories in parallel with thread-local accumulators
    // Workers use DirId indices instead of PathBuf hashing for dir size accumulation
    let hardlink_map = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
        (u32, [u8; 16]),
        PathBuf,
    >::new()));

    // Use rayon's parallel iterator with thread-local accumulators
    // Process directories in parallel using map, then merge sequentially
    let enum_start = profile.as_ref().map(|_| std::time::Instant::now());

    let locals: Vec<ThreadLocalAccum> = all_dirs_ordered.into_par_iter()
        .map(|dir| {
            let mut local = ThreadLocalAccum::default();
            // Pre-size local dir size vectors to avoid reallocation
            local.dir_sizes_logical.resize(dir_count, 0);
            local.dir_sizes_allocated.resize(dir_count, 0);

            let read_start = profile.as_ref().map(|_| std::time::Instant::now());
            let (entries, _) = read_dir_nt(&dir, volume_serial);
            if let (Some(p), Some(start)) = (&profile, read_start) {
                p.pass2_enumeration_ns.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
            }
            if let Some(p) = &profile {
                p.pass2_dir_count.fetch_add(1, Ordering::Relaxed);
                p.pass2_file_count.fetch_add(entries.len() as u64, Ordering::Relaxed);
            }

            local.dir_count += 1;

            // Get DirId for this directory
            let dir_id = *arena.path_to_id.get(&dir).unwrap();

            for entry in entries {
                if let Some(p) = &profile {
                    p.path_clone_count.fetch_add(1, Ordering::Relaxed);
                    p.pathbuf_alloc_count.fetch_add(1, Ordering::Relaxed);
                }

                if entry.is_dir {
                    if entry.is_reparse {
                        // Reparse directory - count it, add to files with zero size
                        local.reparse_skipped += 1;
                        local.files.push(FileRecord {
                            path: entry.path,
                            logical_size: 0,
                            allocated_size: 0,
                            mtime: entry.mtime,
                            file_id: entry.file_id,
                            volume_serial: entry.volume_serial,
                            is_reparse: entry.is_reparse,
                            is_cloud: entry.is_cloud,
                            reparse_tag: entry.reparse_tag,
                        });
                    } else {
                        // Regular directory - ensure entry in local vectors (already pre-sized)
                        // The DirId was assigned in pass 1
                    }
                } else {
                    if entry.is_reparse {
                        local.reparse_skipped += 1;
                        local.files.push(FileRecord {
                            path: entry.path,
                            logical_size: 0,
                            allocated_size: 0,
                            mtime: entry.mtime,
                            file_id: entry.file_id,
                            volume_serial: entry.volume_serial,
                            is_reparse: entry.is_reparse,
                            is_cloud: entry.is_cloud,
                            reparse_tag: entry.reparse_tag,
                        });
                    } else if entry.is_cloud {
                        local.cloud_skipped += 1;
                        local.unreadable_bytes += entry.logical_size;
                        local.files.push(FileRecord {
                            path: entry.path,
                            logical_size: 0,
                            allocated_size: 0,
                            mtime: entry.mtime,
                            file_id: entry.file_id,
                            volume_serial: entry.volume_serial,
                            is_reparse: entry.is_reparse,
                            is_cloud: entry.is_cloud,
                            reparse_tag: entry.reparse_tag,
                        });
                    } else {
                        // Regular file - hardlink deduplication
                        let per_file_start = profile.as_ref().map(|_| std::time::Instant::now());
                        let zero_id = entry.file_id == [0u8; 16] || entry.volume_serial == 0;
                        let hardlink_key = (entry.volume_serial, entry.file_id);

                        let hm_start = profile.as_ref().map(|_| std::time::Instant::now());
                        let is_first_hardlink = if zero_id {
                            true
                        } else {
                            hardlink_map
                                .lock()
                                .unwrap()
                                .insert(hardlink_key, entry.path.clone())
                                .is_none()
                        };
                        if let (Some(p), Some(start)) = (&profile, hm_start) {
                            p.mutex_wait_hardlink_map_ns.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        }

                        if is_first_hardlink {
                            local.file_count += 1;
                            local.total_logical += entry.logical_size;
                            local.total_allocated += entry.allocated_size;

                            // Add to own directory's totals using DirId (O(1) no hashing)
                            local.dir_sizes_logical[dir_id as usize] =
                                local.dir_sizes_logical[dir_id as usize].saturating_add(entry.logical_size);
                            local.dir_sizes_allocated[dir_id as usize] =
                                local.dir_sizes_allocated[dir_id as usize].saturating_add(entry.allocated_size);

                            if let Some(p) = &profile {
                                p.ancestor_walk_count.fetch_add(1, Ordering::Relaxed);
                            }
                        } else {
                            local.hardlink_siblings += 1;
                        }

                        let files_start = profile.as_ref().map(|_| std::time::Instant::now());
                        local.files.push(FileRecord {
                            path: entry.path,
                            logical_size: entry.logical_size,
                            allocated_size: entry.allocated_size,
                            mtime: entry.mtime,
                            file_id: entry.file_id,
                            volume_serial: entry.volume_serial,
                            is_reparse: entry.is_reparse,
                            is_cloud: entry.is_cloud,
                            reparse_tag: entry.reparse_tag,
                        });
                        if let (Some(p), Some(start)) = (&profile, files_start) {
                            p.mutex_wait_files_ns.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        }

                        if let (Some(p), Some(start)) = (&profile, per_file_start) {
                            p.per_file_processing_ns.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
                            p.file_processed_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }

            local
        })
        .collect();

    // Merge results sequentially
    let mut merged = ThreadLocalAccum::default();
    merged.dir_sizes_logical.resize(dir_count, 0);
    merged.dir_sizes_allocated.resize(dir_count, 0);

    for local in locals {
        merged.file_count += local.file_count;
        merged.dir_count += local.dir_count;
        merged.total_logical += local.total_logical;
        merged.total_allocated += local.total_allocated;
        merged.hardlink_siblings += local.hardlink_siblings;
        merged.reparse_skipped += local.reparse_skipped;
        merged.cloud_skipped += local.cloud_skipped;
        merged.unreadable_bytes += local.unreadable_bytes;
        merged.files.extend(local.files);

        // Merge per-dir sizes (Vec addition by index - no hashing!)
        for (i, v) in local.dir_sizes_logical.into_iter().enumerate() {
            if v != 0 {
                merged.dir_sizes_logical[i] = merged.dir_sizes_logical[i].saturating_add(v);
            }
        }
        for (i, v) in local.dir_sizes_allocated.into_iter().enumerate() {
            if v != 0 {
                merged.dir_sizes_allocated[i] = merged.dir_sizes_allocated[i].saturating_add(v);
            }
        }
    }

    if let (Some(p), Some(start)) = (&profile, enum_start) {
        p.pass2_enumeration_ns.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    // Copy merged per-dir own sizes into arena
    for (i, &v) in merged.dir_sizes_logical.iter().enumerate() {
        if v != 0 && i < arena.own_logical.len() {
            arena.own_logical[i] = arena.own_logical[i].saturating_add(v);
        }
    }
    for (i, &v) in merged.dir_sizes_allocated.iter().enumerate() {
        if v != 0 && i < arena.own_allocated.len() {
            arena.own_allocated[i] = arena.own_allocated[i].saturating_add(v);
        }
    }

    // Propagate own sizes to ancestors using integer indices (O(1) no hashing!)
    arena.propagate_to_ancestors();

    // Convert arena to HashMaps for ScanData
    let (all_dir_sizes_logical, all_dir_sizes_allocated) = arena.to_hashmaps();

    let total_file_count = merged.file_count;
    let total_dir_count = merged.dir_count;
    let total_logical = merged.total_logical;
    let total_allocated = merged.total_allocated;
    let total_hardlink_siblings = merged.hardlink_siblings;
    let total_reparse_skipped = merged.reparse_skipped;
    let total_cloud_skipped = merged.cloud_skipped;
    let total_unreadable_bytes = merged.unreadable_bytes;
    let all_files = merged.files;

    let result = ScanResult {
        file_count: total_file_count,
        dir_count: total_dir_count,
        total_logical_bytes: total_logical,
        total_allocated_bytes: total_allocated,
        unreadable_count: total_hardlink_siblings + total_reparse_skipped + total_cloud_skipped
            + unreadable_dirs_total.len() as u64,
        unreadable_bytes: total_unreadable_bytes,
        hardlink_siblings: total_hardlink_siblings,
        reparse_skipped: total_reparse_skipped,
        cloud_skipped: total_cloud_skipped,
        unreadable_dirs: unreadable_dirs_total.len() as u64,
        unreadable_files: 0, // Not tracked separately in single-pass (handled via unreadable_count)
    };

    // Print profile if enabled
    if let Some(p) = profile.as_ref() {
        if let Some(start) = total_start {
            p.total_wall_ns.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        p.print_summary();
    }

    Ok((
        result,
        all_dir_sizes_logical,
        all_dir_sizes_allocated,
        all_files,
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

        // Create event channel early so we can send events during first pass
        let (event_tx, event_rx) = std::sync::mpsc::channel::<NtScanEvent>();

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
        for (dir_path, status) in &unreadable_dirs {
            eprintln!("Unreadable dir: {} status={:?}", dir_path.display(), status);
        }

        // Second pass: process all directories in parallel, sending events via channel
        let thread_count = get_thread_count();
        let pool = if thread_count != 0 {
            ThreadPoolBuilder::new()
                .num_threads(thread_count)
                .build()
                .unwrap()
        } else {
            ThreadPoolBuilder::new().build().unwrap()
        };

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
            // Shared hardlink map for streaming scan
            let hardlink_map = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
                (u32, [u8; 16]),
                PathBuf,
            >::new()));
            // Track which directories we've already sent Dir events for (to avoid duplicates)
            let sent_dirs = Arc::new(std::sync::Mutex::new(std::collections::HashSet::<PathBuf>::new()));
            all_dirs.par_iter().for_each(|dir| {
                let (entries, _) = read_dir_nt(dir, volume_serial);
                for entry in entries {
                    if entry.is_dir {
                        if entry.is_reparse {
                            // Reparse directory - send as a zero-size file entry so UI can track it
                            let _ = event_tx.send(NtScanEvent::Files(vec![FileRecord {
                                path: entry.path.clone(),
                                logical_size: 0,
                                allocated_size: 0,
                                mtime: entry.mtime,
                                file_id: entry.file_id,
                                volume_serial: entry.volume_serial,
                                is_reparse: true,
                                is_cloud: false,
                                reparse_tag: entry.reparse_tag,
                            }]));
                        } else {
                            // Only send Dir event if we haven't sent it before
                            let mut sent = sent_dirs.lock().unwrap();
                            if sent.insert(entry.path.clone()) {
                                let _ = event_tx.send(NtScanEvent::Dir(entry.path.clone()));
                            }
                        }
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

                        // Hardlink deduplication for streaming scan
                        let zero_id = entry.file_id == [0u8; 16] || entry.volume_serial == 0;
                        let hardlink_key = (entry.volume_serial, entry.file_id);
                        let is_first_hardlink = if zero_id {
                            true // never dedup when identity is unknown
                        } else {
                            hardlink_map
                                .lock()
                                .unwrap()
                                .insert(hardlink_key, entry.path.clone())
                                .is_none()
                        };

                        if !is_first_hardlink {
                            // Hardlink sibling - skip sending (will be handled by UI layer)
                            // We still send it but with zero sizes to indicate it's a sibling
                            // The UI layer will filter based on flags
                        }

                        match &baseline {
                            None => {
                                let _ = event_tx.send(NtScanEvent::Files(vec![FileRecord {
                                    path: key,
                                    logical_size: val.0,
                                    allocated_size: val.1,
                                    mtime: val.2,
                                    file_id: val.3,
                                    volume_serial: val.4,
                                    is_reparse: val.5,
                                    is_cloud: val.6,
                                    reparse_tag: val.7,
                                }]));
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
                                    let _ = event_tx.send(NtScanEvent::Changed(vec![FileRecord {
                                        path: key,
                                        logical_size: val.0,
                                        allocated_size: val.1,
                                        mtime: val.2,
                                        file_id: val.3,
                                        volume_serial: val.4,
                                        is_reparse: val.5,
                                        is_cloud: val.6,
                                        reparse_tag: val.7,
                                    }]));
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
                            if seen_paths.insert(file.path.clone()) {
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
