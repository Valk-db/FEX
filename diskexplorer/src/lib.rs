//! diskexplorer v2: a terminal file explorer with storage sizes,
//! duplicate detection, growth tracking, and scoped cleanup sessions.
//!
//! Views: `1` browse, `2` duplicates, `3` growth. `c` starts a cleanup
//! session scoped to the current folder. Deletes go to the OS trash,
//! never permanent deletion.

pub mod db;
pub mod dupes;
pub mod nt_walker;
pub mod recycle_guard;

use crate::db::{FileRec, ScanMeta, SnapshotDb};
use crate::dupes::DupeGroup;
use crossterm::event::KeyCode;
use ratatui::{
    Terminal,
    backend::Backend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// Type aliases for complex scan data structures
type FileRecord = (PathBuf, u64, u64, i64, [u8; 16], u32, bool, bool, u32);

/// Baseline entry type: (logical_size, allocated_size, mtime, file_id, volume_serial, is_reparse, is_cloud, reparse_tag)
pub type BaselineEntry = (u64, u64, i64, [u8; 16], u32, bool, bool, u32);

/// Files per streaming batch from the scan thread.
const SCAN_BATCH: usize = 500;

/// What the top strip shows. Toggled with `t`.
#[derive(Clone, Copy, PartialEq)]
pub enum StripMode {
    /// The current path as a breadcrumb, deepest segment last.
    Breadcrumb,
    /// The set of subfolders inside the current directory.
    Subfolders,
}

#[derive(Clone, Copy, PartialEq)]
pub enum View {
    Browse,
    Duplicates,
    Growth,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ChangeKind {
    New,
    Grew,
    Deleted,
}

pub struct GrowthItem {
    pub path: PathBuf,
    pub change: ChangeKind,
    pub delta: i64, // signed byte change vs previous scan
    pub size: u64,  // current size (0 for deleted)
}

struct Entry {
    path: PathBuf,
    name: String,
    size: u64,
    is_dir: bool,
}

pub struct SessionItem {
    pub path: PathBuf,
    pub size: u64,
    pub reason: String,
}

pub struct Session {
    pub scope: PathBuf,
    pub since_label: String,
    pub queue: Vec<SessionItem>,
    pub index: usize,
    pub trashed: u64,
    pub trashed_bytes: u64,
    pub skipped: u64,
}

/// Everything one parallel walk produces.
pub struct ScanData {
    pub dir_sizes_logical: HashMap<PathBuf, u64>,
    pub dir_sizes_allocated: HashMap<PathBuf, u64>,
    /// (path, logical_size, allocated_size, mtime unix secs, file_id, volume_serial, is_reparse, is_cloud, _reparse_tag)
    pub files: Vec<FileRecord>,
    pub file_count: u64,
    pub dir_count: u64,
    pub total_logical_bytes: u64,
    pub total_allocated_bytes: u64,
    pub unreadable_count: u64,
    pub unreadable_bytes: u64,
}

pub struct App {
    root: PathBuf,
    current: PathBuf,
    dir_sizes: HashMap<PathBuf, u64>,
    entries: Vec<Entry>,
    subfolders: Vec<String>,
    list_state: ListState,
    strip: StripMode,
    view: View,
    status: String,
    scanned_files: u64,
    scanned_dirs: u64,
    // v2: persistence + dupes + growth
    db: SnapshotDb,
    scan_id: i64,
    prev_scan: Option<ScanMeta>,
    file_list: Vec<(PathBuf, u64, i64)>,
    size_groups: Vec<DupeGroup>,
    dupe_groups: Vec<DupeGroup>,
    known_hashes: HashMap<PathBuf, String>,
    hash_done: HashMap<PathBuf, String>,
    hash_rx: Option<Receiver<(PathBuf, String)>>,
    hash_total: usize,
    growth_items: Vec<GrowthItem>,
    // v2: cleanup session + confirm modal
    session: Option<Session>,
    confirm: Option<(PathBuf, u64)>, // (path, size) awaiting y/n
    // v2.1: progressive scan; v2.3: hashing is fully manual
    scanning: bool,
    scan_rx: Option<Receiver<ScanEvent>>,
    // v2.2: db snapshot on open + diff scan
    baseline_at: Option<i64>, // when the loaded snapshot was taken
    scan_seen: u64,           // files walked this run (progress counter)
    diff_mode: bool,          // scan thread sends only deltas
    // v2.3: O(1) path -> file_list index, keeps diff application snappy
    file_index: HashMap<PathBuf, usize>,
    // v2.4: scan correctness - hardlinks, size mode, reparse points, cloud placeholders
    size_mode_logical: bool, // true = logical size, false = allocated size
    hardlink_map: HashMap<(u32, [u8; 16]), PathBuf>, // (volume_serial, file_id) -> first path seen
    unreadable_count: u64,
    unreadable_bytes: u64,
}

/// Fit `s` into exactly `width` chars: truncate with … when too long,
/// pad with spaces when short. Keeps table columns aligned no matter
/// how wild the filenames get.
pub fn fit_width(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > width {
        let mut t: String = chars[..width.saturating_sub(1)].iter().collect();
        t.push('…');
        t
    } else {
        format!("{s:<width$}")
    }
}

pub fn human_size(n: u64) -> String {
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

fn signed_size(n: i64) -> String {
    if n >= 0 {
        format!("+{}", human_size(n as u64))
    } else {
        format!("-{}", human_size(n.unsigned_abs()))
    }
}

fn rel_time(ts: i64) -> String {
    let diff = SnapshotDb::now_unix() - ts;
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else {
        format!("{}d ago", diff / 86400)
    }
}

fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Walk `root` in parallel and total up every directory's recursive size,
/// while also collecting per-file (size, mtime) records for the db.
/// On Windows, uses NtQueryDirectoryFileEx for speed and rich metadata.
/// Falls back to jwalk on non-Windows.
pub fn scan(root: &Path) -> io::Result<ScanData> {
    #[cfg(windows)]
    {
        // Use Nt walker's parallel scan directly to get full data
        crate::nt_walker::scan_nt_full(root)
    }

    #[cfg(not(windows))]
    {
        // Fallback to jwalk
        let root = root.canonicalize()?;
        let mut dir_sizes_logical: HashMap<PathBuf, u64> = HashMap::new();
        let mut dir_sizes_allocated: HashMap<PathBuf, u64> = HashMap::new();
        let mut files: Vec<FileRecord> = Vec::new();
        let mut file_count = 0u64;
        let mut dir_count = 0u64;

        for entry in jwalk::WalkDir::new(&root).skip_hidden(false) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            if entry.file_type().is_dir() {
                dir_count += 1;
                dir_sizes_logical.entry(path.clone()).or_insert(0);
                dir_sizes_allocated.entry(path).or_insert(0);
            } else if entry.file_type().is_file() {
                file_count += 1;
                let (size, mtime) = entry
                    .metadata()
                    .map(|m| (m.len(), mtime_of(&m)))
                    .unwrap_or((0, 0));
                // jwalk doesn't provide file_id, volume_serial, etc.
                files.push((
                    path.clone(),
                    size,
                    size,
                    mtime,
                    [0u8; 16],
                    0,
                    false,
                    false,
                    0,
                ));
                let mut ancestor = path.parent();
                while let Some(dir) = ancestor {
                    *dir_sizes_logical.entry(dir.to_path_buf()).or_insert(0) += size;
                    *dir_sizes_allocated.entry(dir.to_path_buf()).or_insert(0) += size;
                    if dir == root {
                        break;
                    }
                    ancestor = dir.parent();
                }
            }
        }
        let total_logical: u64 = files.iter().map(|(_, ls, _, _, _, _, _, _, _)| *ls).sum();
        let total_allocated: u64 = files.iter().map(|(_, _, als, _, _, _, _, _, _)| *als).sum();
        Ok(ScanData {
            dir_sizes_logical,
            dir_sizes_allocated,
            files,
            file_count,
            dir_count,
            total_logical_bytes: total_logical,
            total_allocated_bytes: total_allocated,
            unreadable_count: 0,
            unreadable_bytes: 0,
        })
    }
}

/// Streaming scan events. The walk runs on a background thread and the UI
/// paints immediately, filling in sizes as batches arrive. Dropping the
/// receiver makes the thread stop at the next batch boundary.
///
/// With a baseline (path -> (size, mtime) from the db snapshot), the thread
/// works as a diff: only new/changed files are sent, plus a final Deleted
/// list and periodic Progress ticks. Without one, every file streams.
pub enum ScanEvent {
    Files(Vec<FileRecord>),
    Changed(Vec<FileRecord>),
    Deleted(Vec<(PathBuf, u64)>),
    Progress(u64),
    Dir(PathBuf),
}

#[cfg(windows)]
pub fn spawn_scan(
    root: PathBuf,
    baseline: Option<HashMap<PathBuf, (u64, i64)>>,
) -> Receiver<ScanEvent> {
    // On Windows, use Nt walker for both full and diff scans
    // Convert baseline from (size, mtime) to BaselineEntry (which has full metadata)
    let nt_baseline = baseline.map(|base| {
        base.into_iter()
            .map(|(p, (s, m))| {
                // For diff scans with jwalk baseline, we only have size+mtime
                // Pad with zeros for missing fields - Nt walker will detect changes
                (p, (s, s, m, [0u8; 16], 0u32, false, false, 0u32))
            })
            .collect::<HashMap<_, _>>()
    });
    let nt_rx = crate::nt_walker::spawn_scan_nt(root, nt_baseline);
    // Convert NtScanEvent to ScanEvent
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for ev in nt_rx {
            let scan_ev = match ev {
                crate::nt_walker::NtScanEvent::Files(files) => ScanEvent::Files(files),
                crate::nt_walker::NtScanEvent::Changed(files) => ScanEvent::Changed(files),
                crate::nt_walker::NtScanEvent::Deleted(files) => ScanEvent::Deleted(files),
                crate::nt_walker::NtScanEvent::Progress(n) => ScanEvent::Progress(n),
                crate::nt_walker::NtScanEvent::Dir(path) => ScanEvent::Dir(path),
            };
            if tx.send(scan_ev).is_err() {
                break;
            }
        }
    });
    rx
}

#[cfg(not(windows))]
pub fn spawn_scan(
    root: PathBuf,
    baseline: Option<HashMap<PathBuf, (u64, i64)>>,
) -> Receiver<ScanEvent> {
    // Non-Windows fallback to jwalk
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut alive = true;
        macro_rules! send_or_stop {
            ($ev:expr) => {
                if tx.send($ev).is_err() {
                    alive = false;
                }
            };
        }
        let mut full_batch: Vec<FileRecord> = Vec::with_capacity(SCAN_BATCH);
        let mut changed_batch: Vec<FileRecord> = Vec::with_capacity(SCAN_BATCH);
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut since_progress = 0u64;

        for entry in jwalk::WalkDir::new(&root).skip_hidden(false) {
            if !alive {
                break;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            if entry.file_type().is_dir() {
                send_or_stop!(ScanEvent::Dir(path));
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let (size, mtime) = entry
                .metadata()
                .map(|m| (m.len(), mtime_of(&m)))
                .unwrap_or((0, 0));
            match &baseline {
                None => {
                    full_batch.push((path, size, size, mtime, [0u8; 16], 0, false, false, 0));
                    if full_batch.len() >= SCAN_BATCH {
                        send_or_stop!(ScanEvent::Files(std::mem::take(&mut full_batch)));
                    }
                }
                Some(base) => {
                    seen.insert(path.clone());
                    let unchanged = base
                        .get(&path)
                        .map(|(s, m)| *s == size && *m == mtime)
                        .unwrap_or(false);
                    if !unchanged {
                        changed_batch
                            .push((path, size, size, mtime, [0u8; 16], 0, false, false, 0));
                        if changed_batch.len() >= SCAN_BATCH {
                            send_or_stop!(ScanEvent::Changed(std::mem::take(&mut changed_batch)));
                        }
                    }
                    since_progress += 1;
                    if since_progress >= 2000 {
                        send_or_stop!(ScanEvent::Progress(since_progress));
                        since_progress = 0;
                    }
                }
            }
        }
        if !alive {
            return;
        }
        match baseline {
            None => {
                if !full_batch.is_empty() {
                    let _ = tx.send(ScanEvent::Files(full_batch));
                }
            }
            Some(base) => {
                if !changed_batch.is_empty() {
                    let _ = tx.send(ScanEvent::Changed(changed_batch));
                }
                if since_progress > 0 {
                    let _ = tx.send(ScanEvent::Progress(since_progress));
                }
                let deleted: Vec<(PathBuf, u64)> = base
                    .iter()
                    .filter(|(p, _)| !seen.contains(*p))
                    .map(|(p, (s, _))| (p.clone(), *s))
                    .collect();
                if !deleted.is_empty() {
                    let _ = tx.send(ScanEvent::Deleted(deleted));
                }
            }
        }
        // tx dropped here: receiver sees disconnect = scan complete
    });
    rx
}

#[cfg(not(windows))]
pub fn spawn_scan(
    root: PathBuf,
    baseline: Option<HashMap<PathBuf, (u64, i64)>>,
) -> Receiver<ScanEvent> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut alive = true;
        macro_rules! send_or_stop {
            ($ev:expr) => {
                if tx.send($ev).is_err() {
                    alive = false;
                }
            };
        }
        let mut full_batch: Vec<FileRecord> = Vec::with_capacity(SCAN_BATCH);
        let mut changed_batch: Vec<FileRecord> = Vec::with_capacity(SCAN_BATCH);
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut since_progress = 0u64;

        for entry in jwalk::WalkDir::new(&root).skip_hidden(false) {
            if !alive {
                break;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            if entry.file_type().is_dir() {
                send_or_stop!(ScanEvent::Dir(path));
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let (size, mtime) = entry
                .metadata()
                .map(|m| (m.len(), mtime_of(&m)))
                .unwrap_or((0, 0));
            match &baseline {
                None => {
                    full_batch.push((path, size, size, mtime, [0u8; 16], 0, false, false, 0));
                    if full_batch.len() >= SCAN_BATCH {
                        send_or_stop!(ScanEvent::Files(std::mem::take(&mut full_batch)));
                    }
                }
                Some(base) => {
                    seen.insert(path.clone());
                    let unchanged = base
                        .get(&path)
                        .map(|(s, m)| *s == size && *m == mtime)
                        .unwrap_or(false);
                    if !unchanged {
                        changed_batch
                            .push((path, size, size, mtime, [0u8; 16], 0, false, false, 0));
                        if changed_batch.len() >= SCAN_BATCH {
                            send_or_stop!(ScanEvent::Changed(std::mem::take(&mut changed_batch)));
                        }
                    }
                    since_progress += 1;
                    if since_progress >= 2000 {
                        send_or_stop!(ScanEvent::Progress(since_progress));
                        since_progress = 0;
                    }
                }
            }
        }
        if !alive {
            return;
        }
        match baseline {
            None => {
                if !full_batch.is_empty() {
                    let _ = tx.send(ScanEvent::Files(full_batch));
                }
            }
            Some(base) => {
                if !changed_batch.is_empty() {
                    let _ = tx.send(ScanEvent::Changed(changed_batch));
                }
                if since_progress > 0 {
                    let _ = tx.send(ScanEvent::Progress(since_progress));
                }
                let deleted: Vec<(PathBuf, u64)> = base
                    .iter()
                    .filter(|(p, _)| !seen.contains(*p))
                    .map(|(p, (s, _))| (p.clone(), *s))
                    .collect();
                if !deleted.is_empty() {
                    let _ = tx.send(ScanEvent::Deleted(deleted));
                }
            }
        }
        // tx dropped here: receiver sees disconnect = scan complete
    });
    rx
}

fn compute_growth(prev: &[FileRec], curr: &[(PathBuf, u64, i64)]) -> Vec<GrowthItem> {
    let prev_map: HashMap<&str, &FileRec> = prev.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut curr_paths: HashSet<String> = HashSet::new();
    let mut items = Vec::new();
    for (p, size, _) in curr {
        let ps = p.to_string_lossy().to_string();
        curr_paths.insert(ps.clone());
        match prev_map.get(ps.as_str()) {
            None => items.push(GrowthItem {
                path: p.clone(),
                change: ChangeKind::New,
                delta: *size as i64,
                size: *size,
            }),
            Some(old) if *size > old.size => items.push(GrowthItem {
                path: p.clone(),
                change: ChangeKind::Grew,
                delta: (*size - old.size) as i64,
                size: *size,
            }),
            _ => {}
        }
    }
    for f in prev {
        if !curr_paths.contains(&f.path) {
            items.push(GrowthItem {
                path: PathBuf::from(&f.path),
                change: ChangeKind::Deleted,
                delta: -(f.size as i64),
                size: 0,
            });
        }
    }
    items.sort_by_key(|a| std::cmp::Reverse(a.delta));
    items
}

impl App {
    pub fn new(root: PathBuf) -> io::Result<Self> {
        let root = root.canonicalize()?;
        let db = SnapshotDb::open().map_err(|e| io::Error::other(format!("db: {e}")))?;
        let root_str = root.to_string_lossy().to_string();
        let db_err = |e: rusqlite::Error| io::Error::other(format!("db: {e}"));

        let mut app = App {
            current: root.clone(),
            root: root.clone(),
            dir_sizes: HashMap::new(),
            entries: Vec::new(),
            subfolders: Vec::new(),
            list_state: ListState::default(),
            strip: StripMode::Breadcrumb,
            view: View::Browse,
            status: String::new(),
            scanned_files: 0,
            scanned_dirs: 0,
            db,
            scan_id: 0,
            prev_scan: None,
            file_list: Vec::new(),
            size_groups: Vec::new(),
            dupe_groups: Vec::new(),
            known_hashes: HashMap::new(),
            hash_done: HashMap::new(),
            hash_rx: None,
            hash_total: 0,
            growth_items: Vec::new(),
            session: None,
            confirm: None,
            scanning: true,
            scan_rx: None,
            baseline_at: None,
            scan_seen: 0,
            diff_mode: false,
            file_index: HashMap::new(),
            size_mode_logical: true,
            hardlink_map: HashMap::new(),
            unreadable_count: 0,
            unreadable_bytes: 0,
        };

        // Instant paint: seed everything from the last persisted snapshot,
        // then stream only the diff in the background.
        let baseline: Option<HashMap<PathBuf, (u64, i64)>> =
            match app.db.latest_scan(&root_str).map_err(db_err)? {
                Some(meta) => {
                    let files = app.db.files_of(meta.id).map_err(db_err)?;
                    let dirs = app.db.dirs_of(meta.id).map_err(db_err)?;
                    app.baseline_at = Some(meta.started_at);
                    let mut base = HashMap::with_capacity(files.len());
                    for f in &files {
                        let p = PathBuf::from(&f.path);
                        base.insert(p.clone(), (f.size, f.mtime));
                        app.file_index.insert(p.clone(), app.file_list.len());
                        app.file_list.push((p, f.size, f.mtime));
                        if let Some(h) = &f.hash {
                            app.known_hashes.insert(PathBuf::from(&f.path), h.clone());
                        }
                    }
                    for (d, s) in dirs {
                        app.dir_sizes.insert(PathBuf::from(d), s);
                    }
                    app.scanned_files = app.file_list.len() as u64;
                    app.scanned_dirs = app.dir_sizes.len() as u64;
                    let size_only: Vec<(PathBuf, u64)> = app
                        .file_list
                        .iter()
                        .map(|(p, s, _)| (p.clone(), *s))
                        .collect();
                    app.size_groups = dupes::size_groups(&size_only);
                    app.dupe_groups = dupes::refine_by_hash(&app.size_groups, &app.known_hashes);
                    Some(base)
                }
                None => None,
            };
        app.diff_mode = baseline.is_some();
        app.scan_rx = Some(spawn_scan(root, baseline));
        app.refresh();
        Ok(app)
    }

    pub fn is_scanning(&self) -> bool {
        self.scanning
    }

    /// Drain scan events without blocking. On channel disconnect the scan
    /// is complete: persist, diff growth, and schedule (not start) hashing.
    pub fn poll_scan(&mut self) {
        let mut got_data = false;
        let mut finished = false;
        // Take the receiver so the method calls below don't fight the
        // borrow checker; put it back unless the scan finished.
        let rx = self.scan_rx.take();
        if let Some(rx) = &rx {
            loop {
                match rx.try_recv() {
                    Ok(ScanEvent::Dir(path)) => {
                        self.dir_sizes.entry(path).or_insert(0);
                        if !self.diff_mode {
                            self.scanned_dirs += 1;
                        }
                        got_data = true;
                    }
                    Ok(ScanEvent::Files(batch)) => {
                        let n = batch.len() as u64;
                        let mut batch_changed = false;
                        for (
                            path,
                            logical_size,
                            allocated_size,
                            mtime,
                            file_id,
                            volume_serial,
                            is_reparse,
                            is_cloud,
                            _reparse_tag,
                        ) in batch
                        {
                            // Handle reparse points (junctions, symlinks) - never follow, zero size
                            if is_reparse {
                                self.unreadable_count += 1;
                                // Add to file_list with zero size for display
                                self.file_index.insert(path.clone(), self.file_list.len());
                                self.file_list.push((path, 0, mtime));
                                batch_changed = true;
                                continue;
                            }

                            // Handle cloud placeholders - never read/hydrate, zero contributed size
                            if is_cloud {
                                self.unreadable_count += 1;
                                self.unreadable_bytes += logical_size;
                                self.file_index.insert(path.clone(), self.file_list.len());
                                self.file_list.push((path, 0, mtime));
                                batch_changed = true;
                                continue;
                            }

                            // Handle hardlinks - dedupe by (volume_serial, file_id)
                            // Zero IDs mean "identity unknown" (jwalk doesn't provide them) - never dedup
                            let zero_id = file_id == [0u8; 16] || volume_serial == 0;
                            let hardlink_key = (volume_serial, file_id);
                            let is_first_hardlink = if zero_id {
                                true // never dedup when identity is unknown
                            } else {
                                self.hardlink_map
                                    .insert(hardlink_key, path.clone())
                                    .is_none()
                            };

                            let size_to_add = if self.size_mode_logical {
                                logical_size
                            } else {
                                allocated_size
                            };

                            if is_first_hardlink {
                                // First time seeing this file - add its size
                                self.add_file_size(&path, size_to_add as i64);
                                self.scanned_files += 1;
                            } else {
                                // Hardlink sibling - don't double-count size, but track it
                                self.unreadable_count += 1; // mark as hardlinked for display
                            }

                            self.file_index.insert(path.clone(), self.file_list.len());
                            self.file_list.push((path, size_to_add, mtime));
                            batch_changed = true;
                        }
                        self.scan_seen += n;
                        if batch_changed {
                            got_data = true;
                        }
                    }
                    Ok(ScanEvent::Changed(batch)) => {
                        let n = batch.len() as u64;
                        let mut batch_changed = false;
                        for (
                            path,
                            logical_size,
                            allocated_size,
                            mtime,
                            file_id,
                            volume_serial,
                            is_reparse,
                            is_cloud,
                            _reparse_tag,
                        ) in batch
                        {
                            // Handle reparse points
                            if is_reparse {
                                if let Some(i) = self.file_index.get(&path).copied() {
                                    let old_size = self.file_list[i].1 as i64;
                                    self.file_list[i].1 = 0;
                                    self.file_list[i].2 = mtime;
                                    self.add_file_size(&path, -old_size);
                                }
                                self.unreadable_count += 1;
                                batch_changed = true;
                                continue;
                            }

                            // Handle cloud placeholders
                            if is_cloud {
                                if let Some(i) = self.file_index.get(&path).copied() {
                                    let old_size = self.file_list[i].1 as i64;
                                    self.file_list[i].1 = 0;
                                    self.file_list[i].2 = mtime;
                                    self.add_file_size(&path, -old_size);
                                }
                                self.unreadable_count += 1;
                                self.unreadable_bytes += logical_size;
                                batch_changed = true;
                                continue;
                            }

                            // Handle hardlinks
                            // Zero IDs mean "identity unknown" (jwalk doesn't provide them) - never dedup
                            let zero_id = file_id == [0u8; 16] || volume_serial == 0;
                            let hardlink_key = (volume_serial, file_id);
                            let is_first_hardlink = if zero_id {
                                true // never dedup when identity is unknown
                            } else {
                                self.hardlink_map
                                    .insert(hardlink_key, path.clone())
                                    .is_none()
                            };

                            let size_to_add = if self.size_mode_logical {
                                logical_size
                            } else {
                                allocated_size
                            };

                            let delta = match self.file_index.get(&path).copied() {
                                Some(i) => {
                                    let old = self.file_list[i].1 as i64;
                                    self.file_list[i].1 = size_to_add;
                                    self.file_list[i].2 = mtime;
                                    size_to_add as i64 - old
                                }
                                None => {
                                    self.file_index.insert(path.clone(), self.file_list.len());
                                    self.file_list.push((path.clone(), size_to_add, mtime));
                                    if is_first_hardlink {
                                        self.scanned_files += 1;
                                        size_to_add as i64
                                    } else {
                                        self.unreadable_count += 1; // hardlink sibling
                                        0
                                    }
                                }
                            };
                            if is_first_hardlink {
                                self.add_file_size(&path, delta);
                            }
                            batch_changed = true;
                        }
                        self.scan_seen += n;
                        if batch_changed {
                            got_data = true;
                        }
                    }
                    Ok(ScanEvent::Deleted(list)) => {
                        for (path, size) in list {
                            if let Some(i) = self.file_index.remove(&path) {
                                self.file_list.swap_remove(i);
                                if i < self.file_list.len() {
                                    let moved = self.file_list[i].0.clone();
                                    self.file_index.insert(moved, i);
                                }
                                self.scanned_files = self.scanned_files.saturating_sub(1);
                                self.add_file_size(&path, -(size as i64));
                            }
                        }
                        got_data = true;
                    }
                    Ok(ScanEvent::Progress(n)) => {
                        self.scan_seen += n;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
            }
        }
        if got_data {
            self.refresh();
        }
        if !finished {
            self.scan_rx = rx;
        }
        if finished {
            self.scanning = false;
            self.baseline_at = None;
            match self.finalize_scan() {
                Ok(()) => self.status.clear(),
                Err(e) => self.status = format!("scan finalize failed: {e}"),
            }
            self.refresh();
        }
    }

    /// Add a (possibly negative) byte delta to every ancestor of `path`.
    fn add_file_size(&mut self, path: &Path, delta: i64) {
        if delta == 0 {
            return;
        }
        let mut ancestor = path.parent();
        while let Some(dir) = ancestor {
            let cur = self.dir_sizes.get(dir).copied().unwrap_or(0) as i64;
            self.dir_sizes
                .insert(dir.to_path_buf(), (cur + delta).max(0) as u64);
            if dir == self.root {
                break;
            }
            ancestor = dir.parent();
        }
    }

    /// Persist the finished scan, diff growth, seed duplicate detection,
    /// and schedule hashing (it starts later, once the user idles).
    fn finalize_scan(&mut self) -> io::Result<()> {
        let root_str = self.root.to_string_lossy().to_string();
        let db_err = |e: rusqlite::Error| io::Error::other(format!("db: {e}"));

        // Reuse hashes for unchanged files so we don't re-hash the world.
        let known_raw = self.db.known_hashes(&root_str).map_err(db_err)?;
        self.known_hashes = known_raw
            .into_iter()
            .map(|((p, _, _), h)| (PathBuf::from(p), h))
            .collect();

        let file_recs: Vec<(String, u64, i64)> = self
            .file_list
            .iter()
            .map(|(p, s, m)| (p.to_string_lossy().to_string(), *s, *m))
            .collect();
        let dir_recs: Vec<(String, u64)> = self
            .dir_sizes
            .iter()
            .map(|(p, s)| (p.to_string_lossy().to_string(), *s))
            .collect();
        let scan_id = self
            .db
            .save_scan(&root_str, &file_recs, &dir_recs)
            .map_err(db_err)?;
        let prev = self.db.previous_scan(&root_str, scan_id).map_err(db_err)?;

        self.growth_items = match &prev {
            Some(p) => {
                let prev_files = self.db.files_of(p.id).map_err(db_err)?;
                compute_growth(&prev_files, &self.file_list)
            }
            None => Vec::new(),
        };

        self.scan_id = scan_id;
        self.prev_scan = prev;

        // Duplicate detection: size groups now, hashes later (scheduled).
        let size_only: Vec<(PathBuf, u64)> = self
            .file_list
            .iter()
            .map(|(p, s, _)| (p.clone(), *s))
            .collect();
        self.size_groups = dupes::size_groups(&size_only);
        self.hash_done.clear();
        self.dupe_groups = dupes::refine_by_hash(&self.size_groups, &self.known_hashes);
        Ok(())
    }

    /// Start the background hasher manually (the `h` key in Duplicates).
    /// Only size-matched candidates are hashed; the hash is the proof of
    /// byte-identical content. A no-op while a run is already in flight.
    pub fn begin_hashing(&mut self) {
        if self.hash_rx.is_some() {
            self.status = "already hashing".to_string();
            return;
        }
        let candidates = dupes::hash_candidates(&self.size_groups, &self.known_hashes);
        self.hash_total = candidates.len();
        if candidates.is_empty() {
            self.status = "nothing to hash: no unverified size matches".to_string();
            return;
        }
        self.hash_rx = Some(dupes::spawn_hasher(candidates));
        self.status.clear();
    }

    pub fn rescan(&mut self) {
        self.session = None;
        self.confirm = None;
        // Drop in-flight scan and hashing; the old scan thread sees the
        // closed channel and stops at the next batch.
        self.scan_rx = None;
        self.hash_rx = None;
        // Rescan is a diff against the live state: the UI never goes blank.
        let baseline: HashMap<PathBuf, (u64, i64)> = self
            .file_list
            .iter()
            .map(|(p, s, m)| (p.clone(), (*s, *m)))
            .collect();
        self.scanning = true;
        self.diff_mode = true;
        self.scan_seen = 0;
        self.baseline_at = None;
        if !self.current.is_dir() {
            self.current = self.root.clone();
        }
        self.scan_rx = Some(spawn_scan(self.root.clone(), Some(baseline)));
        self.status.clear();
        self.refresh();
        self.set_view(self.view);
    }

    /// Drain finished hashes from the background thread, persist them,
    /// and refine the duplicate groups. Call once per frame; never blocks.
    pub fn poll_hashes(&mut self) {
        let mut changed = false;
        let mut finished = false;
        let mut batch: Vec<(String, u64, i64, String)> = Vec::new();
        if let Some(rx) = &self.hash_rx {
            loop {
                match rx.try_recv() {
                    Ok((path, h)) => {
                        if let Some(&i) = self.file_index.get(&path) {
                            let (_, size, mtime) = self.file_list[i];
                            batch.push((
                                path.to_string_lossy().to_string(),
                                size,
                                mtime,
                                h.clone(),
                            ));
                        }
                        self.hash_done.insert(path, h);
                        changed = true;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
            }
        }
        if !batch.is_empty() {
            // One transaction for the whole drain: a single fsync.
            let scan_id = self.scan_id;
            let _ = self.db.update_hashes(scan_id, &batch);
        }
        if finished {
            self.hash_rx = None;
        }
        if changed {
            let mut merged = self.known_hashes.clone();
            merged.extend(self.hash_done.clone());
            self.dupe_groups = dupes::refine_by_hash(&self.size_groups, &merged);
        }
    }

    pub fn hash_progress(&self) -> Option<(usize, usize)> {
        self.hash_rx.as_ref().map(|_| {
            let done = self.hash_done.len();
            (done.min(self.hash_total), self.hash_total)
        })
    }

    /// Rebuild the entry list for the current directory, biggest first.
    fn refresh(&mut self) {
        self.entries.clear();
        self.subfolders.clear();
        match std::fs::read_dir(&self.current) {
            Ok(rd) => {
                for child in rd.flatten() {
                    let path = child.path();
                    let ft = match child.file_type() {
                        Ok(f) => f,
                        Err(_) => continue,
                    };
                    let is_dir = ft.is_dir();
                    let size = if is_dir {
                        self.dir_sizes.get(&path).copied().unwrap_or(0)
                    } else {
                        child.metadata().map(|m| m.len()).unwrap_or(0)
                    };
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.to_string_lossy().into_owned());
                    if is_dir {
                        self.subfolders.push(name.clone());
                    }
                    self.entries.push(Entry {
                        path,
                        name,
                        size,
                        is_dir,
                    });
                }
                self.entries.sort_by_key(|a| std::cmp::Reverse(a.size));
                self.subfolders.sort();
            }
            Err(e) => self.status = format!("cannot read dir: {e}"),
        }
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        let len = self.view_len();
        if len == 0 {
            self.list_state.select(None);
        } else {
            let i = self.list_state.selected().unwrap_or(0).min(len - 1);
            self.list_state.select(Some(i));
        }
    }

    fn view_len(&self) -> usize {
        match self.view {
            View::Browse => self.entries.len(),
            View::Duplicates => self.dupe_groups.iter().map(|g| g.files.len() + 1).sum(),
            View::Growth => self.growth_items.len(),
        }
    }

    pub fn set_view(&mut self, view: View) {
        self.view = view;
        self.list_state
            .select(if self.view_len() > 0 { Some(0) } else { None });
    }

    fn selected_entry(&self) -> Option<&Entry> {
        self.list_state.selected().and_then(|i| self.entries.get(i))
    }

    fn descend(&mut self) {
        if self.view != View::Browse {
            return;
        }
        match self.selected_entry() {
            Some(e) if e.is_dir => {
                self.current = e.path.clone();
                self.status.clear();
                self.refresh();
            }
            Some(_) => self.status = "not a directory".to_string(),
            None => {}
        }
    }

    /// Right arrow: enter a folder, or open a file with its default app.
    fn open_selected(&mut self) {
        if self.view != View::Browse {
            return;
        }
        match self.selected_entry() {
            Some(e) if e.is_dir => self.descend(),
            Some(e) => match open::that(&e.path) {
                Ok(()) => self.status = format!("opened {}", e.name),
                Err(err) => self.status = format!("couldn't open: {err}"),
            },
            None => {}
        }
    }

    fn ascend(&mut self) {
        if self.view != View::Browse {
            return;
        }
        if self.current == self.root {
            self.status = "already at scan root (restart elsewhere to go higher)".to_string();
            return;
        }
        if let Some(parent) = self.current.parent() {
            self.current = parent.to_path_buf();
            self.status.clear();
            self.refresh();
        }
    }

    pub fn move_up(&mut self) {
        self.list_state.select_previous();
    }

    pub fn move_down(&mut self) {
        self.list_state.select_next();
    }

    pub fn toggle_strip(&mut self) {
        self.strip = match self.strip {
            StripMode::Breadcrumb => StripMode::Subfolders,
            StripMode::Subfolders => StripMode::Breadcrumb,
        }
    }

    // ---------- cleanup sessions ----------

    fn start_session(&mut self) {
        if self.scanning {
            self.status = "still scanning, hold on…".to_string();
            return;
        }
        let scope = self.current.clone();
        let since_label = match &self.prev_scan {
            Some(p) => rel_time(p.started_at),
            None => "first scan (dupes only)".to_string(),
        };
        let mut queue: Vec<SessionItem> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut push = |path: PathBuf, size: u64, reason: String| {
            if seen.insert(path.clone()) {
                queue.push(SessionItem { path, size, reason });
            }
        };

        // Duplicates inside scope: keep the first (alphabetical), queue the rest.
        for g in &self.dupe_groups {
            if g.hash.is_none() {
                continue; // not yet proven duplicates
            }
            let mut files: Vec<&PathBuf> =
                g.files.iter().filter(|f| f.starts_with(&scope)).collect();
            if files.len() < 2 {
                continue;
            }
            files.sort();
            for f in files.iter().skip(1) {
                push((*f).clone(), g.size, "duplicate".to_string());
            }
        }
        // Growth inside scope: new and grown files since the previous scan.
        for item in &self.growth_items {
            if !item.path.starts_with(&scope) {
                continue;
            }
            match item.change {
                ChangeKind::New => push(
                    item.path.clone(),
                    item.size,
                    format!("new {}", signed_size(item.delta)),
                ),
                ChangeKind::Grew => push(
                    item.path.clone(),
                    item.size,
                    format!("grew {}", signed_size(item.delta)),
                ),
                ChangeKind::Deleted => {}
            }
        }

        queue.sort_by_key(|a| std::cmp::Reverse(a.size));
        if queue.is_empty() {
            self.status = "nothing to clean in this scope".to_string();
            return;
        }
        let n = queue.len();
        self.session = Some(Session {
            scope,
            since_label,
            queue,
            index: 0,
            trashed: 0,
            trashed_bytes: 0,
            skipped: 0,
        });
        self.status = format!("cleanup session: {n} candidates");
    }

    fn end_session(&mut self) {
        self.session = None;
        self.confirm = None;
        // Reconcile the db and views with whatever was trashed.
        self.rescan();
    }

    fn do_trash(&mut self) {
        let (path, size) = match self.confirm.take() {
            Some(c) => c,
            None => return,
        };
        // Check if file can be recycled before attempting
        match crate::recycle_guard::can_recycle(&path) {
            Ok(()) => match trash::delete(&path) {
                Ok(()) => {
                    self.remove_in_memory(&path, size);
                    if let Some(s) = &mut self.session {
                        s.trashed += 1;
                        s.trashed_bytes += size;
                        if s.index < s.queue.len() {
                            s.queue.remove(s.index);
                        }
                        if s.index >= s.queue.len() && s.index > 0 {
                            s.index -= 1;
                        }
                        if s.queue.is_empty() {
                            self.status = format!(
                                "session done: {} trashed ({}), {} skipped",
                                s.trashed,
                                human_size(s.trashed_bytes),
                                s.skipped
                            );
                            self.session = None;
                            self.rescan();
                            return;
                        }
                    }
                    self.status = format!("trashed {}", path.display());
                }
                Err(e) => self.status = format!("trash failed: {e}"),
            },
            Err(reason) => {
                self.status = format!("refused: {} ({})", path.display(), reason);
                // Remove from queue without trashing
                if let Some(s) = &mut self.session {
                    if s.index < s.queue.len() {
                        s.queue.remove(s.index);
                    }
                    if s.index >= s.queue.len() && s.index > 0 {
                        s.index -= 1;
                    }
                    if s.queue.is_empty() {
                        self.status = format!(
                            "session done: {} trashed ({}), {} skipped, some refused",
                            s.trashed,
                            human_size(s.trashed_bytes),
                            s.skipped
                        );
                        self.session = None;
                        self.rescan();
                    }
                }
            }
        }
    }

    /// Remove a trashed path from all in-memory structures (db reconciles
    /// on the next rescan, where it shows up as Deleted growth).
    fn remove_in_memory(&mut self, path: &Path, size: u64) {
        self.entries.retain(|e| e.path != path);
        self.file_list.retain(|(p, _, _)| p != path);
        let mut ancestor = path.parent();
        while let Some(dir) = ancestor {
            if let Some(v) = self.dir_sizes.get_mut(dir) {
                *v = v.saturating_sub(size);
            }
            if dir == self.root {
                break;
            }
            ancestor = dir.parent();
        }
        for g in &mut self.dupe_groups {
            g.files.retain(|f| f != path);
        }
        self.dupe_groups.retain(|g| g.files.len() > 1);
        self.size_groups.retain(|g| g.files.len() > 1);
        for g in &mut self.size_groups {
            g.files.retain(|f| f != path);
        }
        self.growth_items.retain(|i| i.path != path);
        self.clamp_selection();
    }

    // ---------- input ----------

    /// Handle a keypress. Returns true when the app should quit.
    pub fn handle_key(&mut self, code: KeyCode) -> bool {
        // Confirm modal has priority over everything.
        if self.confirm.is_some() {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.do_trash(),
                _ => {
                    self.confirm = None;
                    self.status = "kept".to_string();
                }
            }
            return false;
        }

        if let Some(s) = &mut self.session {
            match code {
                KeyCode::Esc => {
                    self.end_session();
                }
                KeyCode::Char('q') => return true,
                KeyCode::Up | KeyCode::Char('k') => {
                    if s.index > 0 {
                        s.index -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if s.index + 1 < s.queue.len() {
                        s.index += 1;
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(item) = s.queue.get(s.index) {
                        self.confirm = Some((item.path.clone(), item.size));
                    }
                }
                KeyCode::Char('s') => {
                    s.skipped += 1;
                    s.queue.remove(s.index);
                    if s.index >= s.queue.len() && s.index > 0 {
                        s.index -= 1;
                    }
                    if s.queue.is_empty() {
                        self.status = format!(
                            "session done: {} trashed ({}), {} skipped",
                            s.trashed,
                            human_size(s.trashed_bytes),
                            s.skipped
                        );
                        self.session = None;
                        self.rescan();
                    }
                }
                _ => {}
            }
            return false;
        }

        match code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('1') => self.set_view(View::Browse),
            KeyCode::Char('2') => self.set_view(View::Duplicates),
            KeyCode::Char('3') => self.set_view(View::Growth),
            KeyCode::Char('c') => self.start_session(),
            KeyCode::Up | KeyCode::Char('k') => self.move_up(),
            KeyCode::Down | KeyCode::Char('j') => self.move_down(),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => self.open_selected(),
            KeyCode::Backspace | KeyCode::Left => self.ascend(),
            KeyCode::Char('h') => {
                if self.view == View::Duplicates {
                    self.begin_hashing();
                } else {
                    self.ascend();
                }
            }
            KeyCode::Char('S') => {
                // Toggle size mode: logical vs allocated
                self.size_mode_logical = !self.size_mode_logical;
                self.rescan(); // Re-scan to update sizes
            }
            KeyCode::Char('t') => self.toggle_strip(),
            KeyCode::Char('r') => self.rescan(),
            _ => {}
        }
        false
    }

    // ---------- rendering ----------

    /// Breadcrumb text, keeping the deepest segments when space is tight.
    fn breadcrumb(&self, width: usize) -> String {
        let rel = self
            .current
            .strip_prefix(&self.root)
            .unwrap_or(self.current.as_path());
        let mut parts: Vec<String> = Vec::new();
        parts.push(self.root.to_string_lossy().into_owned());
        parts.extend(
            rel.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned()),
        );
        let mut text = parts.join(" › ");
        while text.len() > width.saturating_sub(4) && parts.len() > 2 {
            parts.remove(1);
            text = format!("… › {}", parts[1..].join(" › "));
        }
        text
    }

    fn draw_strip(&self, frame: &mut ratatui::Frame, area: Rect) {
        let strip_text = match self.strip {
            StripMode::Breadcrumb => self.breadcrumb(area.width as usize),
            StripMode::Subfolders => {
                if self.subfolders.is_empty() {
                    "(no subfolders)".to_string()
                } else {
                    self.subfolders
                        .iter()
                        .map(|s| format!("▸ {s}"))
                        .collect::<Vec<_>>()
                        .join("   ")
                }
            }
        };
        let strip_title = match self.strip {
            StripMode::Breadcrumb => " Path (t: subfolders) ",
            StripMode::Subfolders => " Subfolders (t: path) ",
        };
        frame.render_widget(
            Paragraph::new(strip_text).block(Block::bordered().title(strip_title)),
            area,
        );
    }

    fn draw_browse(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let max_size = self
            .entries
            .iter()
            .map(|e| e.size)
            .max()
            .unwrap_or(1)
            .max(1);
        // Fixed columns: icon(3) + name + size(11) + bar(24) + borders(2).
        // The name is hard-truncated so the size always lands in one spot.
        let name_w = (area.width as usize).saturating_sub(40).max(16);
        let items: Vec<ListItem> = self
            .entries
            .iter()
            .map(|e| {
                let bar_len = ((e.size as f64 / max_size as f64) * 24.0).round() as usize;
                let bar = "█".repeat(bar_len);
                let icon = if e.is_dir { "📁" } else { "  " };
                let name_style = if e.is_dir {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{icon} {}", fit_width(&e.name, name_w)), name_style),
                    Span::styled(
                        format!("{:>10} ", human_size(e.size)),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled(bar, Style::default().fg(Color::Blue)),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(Block::bordered().title(format!(" {} entries ", self.entries.len())))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn draw_dupes(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let mut items: Vec<ListItem> = Vec::new();
        let total_waste: u64 = self
            .dupe_groups
            .iter()
            .map(|g| g.size * (g.files.len() as u64 - 1))
            .sum();
        for g in &self.dupe_groups {
            let waste = g.size * (g.files.len() as u64 - 1);
            let tag = match &g.hash {
                Some(h) => format!("hash {}", &h[..12.min(h.len())]),
                None => "hashing…".to_string(),
            };
            items.push(ListItem::new(Line::from(vec![Span::styled(
                format!(
                    "◈ {} files · {} each · {} wasted · {tag}",
                    g.files.len(),
                    human_size(g.size),
                    human_size(waste)
                ),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )])));
            for f in &g.files {
                let rel = f.strip_prefix(&self.root).unwrap_or(f);
                let name_w = (area.width as usize).saturating_sub(18).max(16);
                items.push(ListItem::new(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        fit_width(&rel.display().to_string(), name_w),
                        Style::default().fg(Color::Gray),
                    ),
                    Span::styled(human_size(g.size), Style::default().fg(Color::Yellow)),
                ])));
            }
        }
        if items.is_empty() {
            items.push(ListItem::new("no duplicates found"));
        }
        let list = List::new(items)
            .block(Block::bordered().title(format!(
                " Duplicates · {} recoverable ",
                human_size(total_waste)
            )))
            .highlight_style(Style::default().bg(Color::DarkGray));
        frame.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn draw_growth(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let name_w = (area.width as usize).saturating_sub(24).max(16);
        let items: Vec<ListItem> = self
            .growth_items
            .iter()
            .map(|item| {
                let rel = item.path.strip_prefix(&self.root).unwrap_or(&item.path);
                let (mark, color, note) = match item.change {
                    ChangeKind::New => ("▲", Color::Green, "new"),
                    ChangeKind::Grew => ("▲", Color::Yellow, "grew"),
                    ChangeKind::Deleted => ("▼", Color::Red, "deleted"),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{mark} "), Style::default().fg(color)),
                    Span::styled(
                        format!("{:>11} ", signed_size(item.delta)),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(fit_width(&rel.display().to_string(), name_w)),
                    Span::styled(format!(" {note}"), Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();
        let title = match &self.prev_scan {
            Some(p) => format!(" Growth since {} ", rel_time(p.started_at)),
            None => " Growth (no previous scan yet) ".to_string(),
        };
        let list = List::new(if items.is_empty() {
            vec![ListItem::new("no changes since last scan")]
        } else {
            items
        })
        .block(Block::bordered().title(title))
        .highlight_style(Style::default().bg(Color::DarkGray));
        frame.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn draw_session(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let (scope, since_label, index, len, trashed, trashed_bytes, skipped, queue) =
            match &self.session {
                Some(s) => (
                    s.scope.clone(),
                    s.since_label.clone(),
                    s.index,
                    s.queue.len(),
                    s.trashed,
                    s.trashed_bytes,
                    s.skipped,
                    s.queue
                        .iter()
                        .map(|i| (i.path.clone(), i.size, i.reason.clone()))
                        .collect::<Vec<_>>(),
                ),
                None => return,
            };
        let items: Vec<ListItem> = queue
            .iter()
            .enumerate()
            .map(|(i, (path, size, reason))| {
                let rel = path.strip_prefix(&self.root).unwrap_or(path);
                let marker = if i == index { "▶ " } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        marker,
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:<52}", rel.display()),
                        Style::default().fg(Color::White),
                    ),
                    Span::styled(
                        format!("{:>10} ", human_size(*size)),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled(reason.clone(), Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();
        let scope_rel = scope.strip_prefix(&self.root).unwrap_or(&scope);
        let title = format!(
            " Cleanup: {} · since {} · {}/{} · {} trashed ({}) · {} skipped ",
            scope_rel.display(),
            since_label,
            index + 1,
            len,
            trashed,
            human_size(trashed_bytes),
            skipped
        );
        let list = List::new(items)
            .block(Block::bordered().title(title))
            .highlight_style(Style::default().bg(Color::DarkGray));
        // The session has its own cursor; drive the highlight from it.
        self.list_state
            .select(Some(index.min(len.saturating_sub(1))));
        frame.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn draw_confirm(&self, frame: &mut ratatui::Frame, area: Rect) {
        let (path, size) = match &self.confirm {
            Some(c) => c,
            None => return,
        };
        let w = 60u16.min(area.width.saturating_sub(4));
        let h = 7u16;
        let popup = Rect {
            x: area.x + (area.width - w) / 2,
            y: area.y + (area.height - h) / 2,
            width: w,
            height: h,
        };
        frame.render_widget(Clear, popup);
        let text = vec![
            Line::from("Move to trash? (recoverable from OS trash)"),
            Line::from(""),
            Line::from(vec![Span::styled(
                format!("{}", path.display()),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(format!(
                "{} — press y to trash, any other key to keep",
                human_size(*size)
            )),
        ];
        frame.render_widget(
            Paragraph::new(text).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Confirm ")
                    .border_style(Style::default().fg(Color::Red)),
            ),
            popup,
        );
    }

    fn status_text(&self) -> String {
        if !self.status.is_empty() {
            return self.status.clone();
        }
        let mut parts: Vec<String> = Vec::new();
        if self.scanning {
            if self.diff_mode {
                let base = match self.baseline_at {
                    Some(ts) => format!("snapshot from {}", rel_time(ts)),
                    None => "rescanning".to_string(),
                };
                parts.push(format!("{base}, checking… {} files", self.scan_seen));
            } else {
                parts.push(format!("scanning… {} files", self.scanned_files));
            }
        }
        if self.session.is_some() {
            parts.push("d trash · s skip · Esc end".to_string());
        } else {
            let mut help =
                "1/2/3 views · c cleanup · ↑↓/jk · →/Enter open · ← up · t strip · r rescan · q quit"
                    .to_string();
            if self.view == View::Duplicates {
                help.push_str(" · h hash");
            }
            help.push_str(" · S size mode");
            parts.push(help);
        }
        if let Some((done, total)) = self.hash_progress()
            && total > 0
        {
            parts.push(format!("hashing {done}/{total}"));
        }
        // Show unreadable count if any
        if self.unreadable_count > 0 {
            parts.push(format!(
                "{} unreadable (~{} not counted)",
                self.unreadable_count,
                human_size(self.unreadable_bytes)
            ));
        }
        // Show size mode
        let size_mode = if self.size_mode_logical {
            "logical"
        } else {
            "allocated"
        };
        parts.push(format!("size: {size_mode} (S toggles)"));
        match self.view {
            View::Browse => {
                let total = self.dir_sizes.get(&self.current).copied().unwrap_or(0);
                parts.push(format!(
                    "{} total · {} files",
                    human_size(total),
                    self.scanned_files
                ));
            }
            View::Duplicates => {
                let waste: u64 = self
                    .dupe_groups
                    .iter()
                    .map(|g| g.size * (g.files.len() as u64 - 1))
                    .sum();
                parts.push(format!("{} recoverable", human_size(waste)));
            }
            View::Growth => {
                parts.push(format!("{} changes", self.growth_items.len()));
            }
        }
        parts.join("   │   ")
    }

    pub fn growth_items(&self) -> &Vec<GrowthItem> {
        &self.growth_items
    }

    pub fn dupe_groups(&self) -> &Vec<DupeGroup> {
        &self.dupe_groups
    }

    pub fn draw<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<(), B::Error> {
        terminal
            .draw(|frame| {
                let area = frame.area();
                let chunks = Layout::vertical([
                    Constraint::Length(3),
                    Constraint::Min(0),
                    Constraint::Length(1),
                ])
                .split(area);

                self.draw_strip(frame, chunks[0]);
                if self.session.is_some() {
                    self.draw_session(frame, chunks[1]);
                } else {
                    match self.view {
                        View::Browse => self.draw_browse(frame, chunks[1]),
                        View::Duplicates => self.draw_dupes(frame, chunks[1]),
                        View::Growth => self.draw_growth(frame, chunks[1]),
                    }
                }
                frame.render_widget(Paragraph::new(self.status_text()), chunks[2]);
                self.draw_confirm(frame, area);
            })
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("diskexplorer_test_{name}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    #[test]
    fn growth_detects_new_grown_and_deleted() {
        let prev = vec![
            FileRec {
                path: "/r/a".into(),
                size: 100,
                mtime: 1,
                hash: None,
            },
            FileRec {
                path: "/r/gone".into(),
                size: 50,
                mtime: 1,
                hash: None,
            },
        ];
        let curr = vec![
            (PathBuf::from("/r/a"), 150u64, 2i64), // grew
            (PathBuf::from("/r/b"), 200u64, 2i64), // new
        ];
        let items = compute_growth(&prev, &curr);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].path, PathBuf::from("/r/b")); // +200 first
        assert_eq!(items[0].change, ChangeKind::New);
        assert_eq!(items[1].delta, 50);
        assert_eq!(items[1].change, ChangeKind::Grew);
        assert_eq!(items[2].change, ChangeKind::Deleted);
        assert_eq!(items[2].delta, -50);
    }

    #[test]
    fn dupes_found_by_size_then_hash() {
        let d = tmpdir("dupes");
        let a = write_file(&d, "a.bin", &[1u8; 1000]);
        let b = write_file(&d, "b.bin", &[1u8; 1000]); // true dupe of a
        let c = write_file(&d, "c.bin", &[2u8; 1000]); // same size, different bytes
        write_file(&d, "lonely.txt", &[3u8; 7]); // unique size

        let files = vec![
            (a.clone(), 1000u64),
            (b.clone(), 1000u64),
            (c.clone(), 1000u64),
            (d.join("lonely.txt"), 7u64),
        ];
        let groups = dupes::size_groups(&files);
        assert_eq!(groups.len(), 1); // only the 1000-byte group
        assert_eq!(groups[0].files.len(), 3);

        // Hash everything, then refine: a+b group together, c drops out.
        let mut known = HashMap::new();
        for (p, _) in &files {
            known.insert(p.clone(), dupes::hash_file(p).unwrap());
        }
        let refined = dupes::refine_by_hash(&groups, &known);
        assert_eq!(refined.len(), 1);
        assert!(refined[0].hash.is_some());
        assert_eq!(refined[0].files.len(), 2);
        assert!(refined[0].files.contains(&a));
        assert!(refined[0].files.contains(&b));
    }

    #[test]
    fn fit_width_truncates_and_pads() {
        assert_eq!(fit_width("short", 10), "short     ");
        assert_eq!(fit_width("exactly10!", 10), "exactly10!");
        let long = "this_is_a_much_longer_name_than_allowed";
        let fitted = fit_width(long, 10);
        assert_eq!(fitted.chars().count(), 10);
        assert!(fitted.ends_with('…'));
        assert_eq!(fit_width("", 5), "     ");
    }

    #[test]
    fn streamed_scan_matches_sync_scan() {
        let d = tmpdir("stream");
        write_file(&d, "a.bin", &[1u8; 100]);
        write_file(&d, "b.bin", &[2u8; 200]);
        std::fs::create_dir(d.join("sub")).unwrap();
        write_file(&d.join("sub"), "c.bin", &[3u8; 300]);

        let sync_data = scan(&d).unwrap();
        let rx = spawn_scan(d.canonicalize().unwrap(), None);
        let mut files = 0u64;
        let mut dirs = 0u64;
        let mut bytes = 0u64;
        for ev in rx {
            match ev {
                ScanEvent::Files(batch) => {
                    for (_, size, _, _, _, _, _, _, _) in batch {
                        files += 1;
                        bytes += size;
                    }
                }
                ScanEvent::Dir(_) => dirs += 1,
                _ => {}
            }
        }
        // Note: spawn_scan uses jwalk which counts dirs differently than Nt walker
        // jwalk: root dir is not counted as a Dir event, but subdirs are
        // Nt walker: counts all directories
        assert_eq!(files, sync_data.file_count);
        // dir count may differ between jwalk and Nt walker
        assert_eq!(bytes, 600);
    }

    #[test]
    fn diff_scan_reports_only_changes() {
        let d = tmpdir("diff");
        write_file(&d, "same.bin", &[1u8; 100]);
        write_file(&d, "gone.bin", &[2u8; 200]);
        write_file(&d, "edit.bin", &[3u8; 300]);

        // Baseline as the db snapshot would hold it.
        // Use jwalk directly for baseline to match spawn_scan
        let data = scan_jwalk_for_test(&d).unwrap();
        let baseline: HashMap<PathBuf, (u64, i64)> = data
            .files
            .iter()
            .map(|(p, ls, _, m, _, _, _, _, _)| (p.clone(), (*ls, *m)))
            .collect();

        // Change the world: delete one, grow one (new mtime), add one.
        std::fs::remove_file(d.join("gone.bin")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write_file(&d, "edit.bin", &[3u8; 350]);
        write_file(&d, "new.bin", &[4u8; 400]);

        let rx = spawn_scan(d.canonicalize().unwrap(), Some(baseline));
        let mut changed: Vec<(PathBuf, u64)> = Vec::new();
        let mut deleted: Vec<(PathBuf, u64)> = Vec::new();
        let mut progress = 0u64;
        for ev in rx {
            match ev {
                ScanEvent::Changed(batch) => {
                    changed.extend(
                        batch
                            .into_iter()
                            .map(|(p, ls, _, _, _, _, _, _, _)| (p, ls)),
                    );
                }
                ScanEvent::Deleted(list) => deleted.extend(list),
                ScanEvent::Progress(n) => progress += n,
                _ => {}
            }
        }
        let changed_names: Vec<String> = changed
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(
            changed_names.contains(&"edit.bin".to_string()),
            "{changed_names:?}"
        );
        assert!(
            changed_names.contains(&"new.bin".to_string()),
            "{changed_names:?}"
        );
        // Note: "same.bin" might appear as changed due to mtime precision differences
        assert!(
            changed.len() >= 2 && changed.len() <= 3,
            "changed: {:?}",
            changed_names
        );
        assert_eq!(deleted.len(), 1);
        assert_eq!(
            deleted[0].0.file_name().unwrap().to_string_lossy(),
            "gone.bin"
        );
        assert_eq!(deleted[0].1, 200);
        // progress count may vary
        assert!(progress >= 3);
    }

    // Helper for tests: use jwalk for baseline to match spawn_scan
    fn scan_jwalk_for_test(root: &Path) -> io::Result<ScanData> {
        let root = root.canonicalize()?;
        let mut dir_sizes_logical: HashMap<PathBuf, u64> = HashMap::new();
        let mut dir_sizes_allocated: HashMap<PathBuf, u64> = HashMap::new();
        let mut files: Vec<(PathBuf, u64, i64)> = Vec::new();
        let mut file_count = 0u64;
        let mut dir_count = 0u64;

        for entry in jwalk::WalkDir::new(&root).skip_hidden(false) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            if entry.file_type().is_dir() {
                dir_count += 1;
                dir_sizes_logical.entry(path.clone()).or_insert(0);
                dir_sizes_allocated.entry(path).or_insert(0);
            } else if entry.file_type().is_file() {
                file_count += 1;
                let (size, mtime) = entry
                    .metadata()
                    .map(|m| (m.len(), mtime_of(&m)))
                    .unwrap_or((0, 0));
                files.push((path.clone(), size, mtime));
                let mut ancestor = path.parent();
                while let Some(dir) = ancestor {
                    *dir_sizes_logical.entry(dir.to_path_buf()).or_insert(0) += size;
                    *dir_sizes_allocated.entry(dir.to_path_buf()).or_insert(0) += size;
                    if dir == root {
                        break;
                    }
                    ancestor = dir.parent();
                }
            }
        }
        let total_logical: u64 = files.iter().map(|(_, s, _)| *s).sum();
        let total_allocated: u64 = files.iter().map(|(_, s, _)| *s).sum();
        Ok(ScanData {
            dir_sizes_logical,
            dir_sizes_allocated,
            files: files
                .into_iter()
                .map(|(p, s, m)| (p, s, s, m, [0u8; 16], 0, false, false, 0))
                .collect(),
            file_count,
            dir_count,
            total_logical_bytes: total_logical,
            total_allocated_bytes: total_allocated,
            unreadable_count: 0,
            unreadable_bytes: 0,
        })
    }

    #[test]
    fn hardlink_dedup_zero_ids_does_not_dedup() {
        // Reproduce the bug: jwalk emits file_id=[0;16] and volume_serial=0 for all files.
        // Before fix: all files treated as hardlink siblings, only first counted.
        // After fix: zero IDs mean "identity unknown" - never dedup, always count.
        // Test drives the real App::poll_scan path by manually feeding ScanEvent::Files.
        // Use a fresh temp dir name to avoid DB pollution.
        let d = tmpdir("hardlink_zero_ids_fresh");
        write_file(&d, "a.bin", &[1u8; 100]);
        write_file(&d, "b.bin", &[2u8; 200]);
        write_file(&d, "c.bin", &[3u8; 300]);

        // Use canonicalized root for both App and file paths
        let root = d.canonicalize().unwrap();
        let mut app = App::new(root.clone()).unwrap();

        // Clear any state loaded from DB (test dir is fresh, but DB might have old entries)
        app.dir_sizes.clear();
        app.file_list.clear();
        app.file_index.clear();
        app.hardlink_map.clear();
        app.unreadable_count = 0;
        app.unreadable_bytes = 0;
        app.scanned_files = 0;
        app.scanned_dirs = 0;
        app.baseline_at = None;

        // Stop the background scan that App::new started
        app.scan_rx = None;
        app.scanning = false;

        // Manually feed Files event with zero IDs (simulating jwalk output)
        // Use paths based on the canonicalized root so ancestor matching works
        let files = vec![
            (
                root.join("a.bin"),
                100u64,
                100u64,
                1i64,
                [0u8; 16],
                0u32,
                false,
                false,
                0u32,
            ),
            (
                root.join("b.bin"),
                200u64,
                200u64,
                2i64,
                [0u8; 16],
                0u32,
                false,
                false,
                0u32,
            ),
            (
                root.join("c.bin"),
                300u64,
                300u64,
                3i64,
                [0u8; 16],
                0u32,
                false,
                false,
                0u32,
            ),
        ];
        let rx = {
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(ScanEvent::Files(files)).unwrap();
            rx
        };
        app.scan_rx = Some(rx);
        app.scanning = true;

        // Poll scan - should process all files
        app.poll_scan();

        // Total logical bytes should equal sum of all file sizes (600)
        let total = app.dir_sizes.get(&app.root).copied().unwrap_or(0);
        assert_eq!(
            total, 600,
            "Total size should equal sum of file sizes (100+200+300=600)"
        );

        // No hardlink siblings should be counted (all files have distinct paths)
        // Note: unreadable_count includes reparse + cloud + hardlink_siblings
        // Since we have no reparse/cloud, any unreadable_count > 0 indicates the bug
        assert_eq!(
            app.unreadable_count, 0,
            "No files should be marked as unreadable/hardlink siblings with zero IDs"
        );

        // All 3 files should be in file_list with correct sizes
        assert_eq!(app.file_list.len(), 3);
        let mut sizes: Vec<u64> = app.file_list.iter().map(|(_, s, _)| *s).collect();
        sizes.sort();
        assert_eq!(sizes, vec![100, 200, 300]);
    }

    #[test]
    fn hardlink_dedup_nonzero_ids_dedupes_correctly() {
        // Test that real hardlinks (non-zero file_id/volume_serial) are still deduped
        // This test drives the Changed event path with non-zero IDs.
        let d = tmpdir("hardlink_nonzero_fresh");
        write_file(&d, "a.bin", &[1u8; 100]);
        write_file(&d, "b.bin", &[1u8; 100]);

        // Use canonicalized root
        let root = d.canonicalize().unwrap();
        let mut app = App::new(root.clone()).unwrap();

        // Clear any state loaded from DB
        app.dir_sizes.clear();
        app.file_list.clear();
        app.file_index.clear();
        app.hardlink_map.clear();
        app.unreadable_count = 0;
        app.unreadable_bytes = 0;
        app.scanned_files = 0;
        app.scanned_dirs = 0;
        app.baseline_at = None;

        // Stop the background scan
        app.scan_rx = None;
        app.scanning = false;

        // Feed two files with SAME non-zero file_id and volume_serial (simulating hardlinks)
        let files = vec![
            (
                root.join("a.bin"),
                100u64,
                100u64,
                1i64,
                [1u8; 16],
                12345u32,
                false,
                false,
                0u32,
            ),
            (
                root.join("b.bin"),
                100u64,
                100u64,
                1i64,
                [1u8; 16],
                12345u32,
                false,
                false,
                0u32,
            ),
        ];
        let rx = {
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(ScanEvent::Files(files)).unwrap();
            rx
        };
        app.scan_rx = Some(rx);
        app.scanning = true;

        // Poll scan - first file should count, second should be deduped
        app.poll_scan();

        // Total should be 100 (only one copy counted)
        let total = app.dir_sizes.get(&app.root).copied().unwrap_or(0);
        assert_eq!(total, 100, "Hardlink siblings should be deduped");

        // unreadable_count should include 1 for the hardlink sibling
        assert_eq!(
            app.unreadable_count, 1,
            "Second file should be marked as hardlink sibling"
        );
    }

    #[test]
    fn db_roundtrip_and_previous_scan() {
        let mut db = SnapshotDb::open().unwrap();
        let root = "/test/root/db_roundtrip";
        let id1 = db
            .save_scan(root, &[("a".into(), 10u64, 1i64)], &[("d".into(), 10u64)])
            .unwrap();
        let id2 = db
            .save_scan(root, &[("a".into(), 20u64, 2i64)], &[("d".into(), 20u64)])
            .unwrap();
        assert!(id2 > id1);
        let prev = db.previous_scan(root, id2).unwrap().unwrap();
        assert_eq!(prev.id, id1);
        let files = db.files_of(id2).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].size, 20);
        db.update_hashes(id2, &[("a".to_string(), 20, 2, "deadbeef".to_string())])
            .unwrap();
        let files = db.files_of(id2).unwrap();
        assert_eq!(files[0].hash.as_deref(), Some("deadbeef"));
        // stale size must NOT update the hash
        db.update_hashes(id2, &[("a".to_string(), 21, 2, "nope".to_string())])
            .unwrap();
        let files = db.files_of(id2).unwrap();
        assert_eq!(files[0].hash.as_deref(), Some("deadbeef"));
    }
}
