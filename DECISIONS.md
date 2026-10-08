# DECISIONS.md — diskexplorer (Rust, Windows TUI)

## T0 — Recon

### Findings

**Node struct and measured bytes/node:**
- `Entry` struct (lib.rs:60-65): `path: PathBuf`, `name: String`, `size: u64`, `is_dir: bool` = ~80-100 bytes minimum per entry (PathBuf + String overhead)
- `ScanData` (lib.rs:84-90): `dir_sizes: HashMap<PathBuf, u64>`, `files: Vec<(PathBuf, u64, i64)>`, `file_count`, `dir_count`
- Paths stored as `PathBuf` per node (not arena/interned)
- `file_index: HashMap<PathBuf, usize>` for O(1) path→index lookups (lib.rs:127)

**Peak RSS on largest scan available:** Tested on ~/Downloads (4,344 files, 493 dirs):
- Release binary: 2.97 MB
- Scan time: ~2-3 seconds

**Release profile settings:** Not configured — default Cargo.toml has no `[profile.release]` section (defaults only)

**`cargo tree -d` duplicates:**
```
hashbrown v0.16.1 / v0.17.1
syn v2.0.119 / v3.0.6
```

**Sorts/aggregates recompute:** `refresh()` (lib.rs:727-764) rebuilds entries on every directory change; `poll_scan()` calls `refresh()` on every batch (lib.rs:547-548). 150ms tick in main.rs:20 triggers `poll_scan()` + `poll_hashes()` + `draw()`. No dirty-flag optimization currently.

**Key bindings in use:** 1/2/3 (views), c (cleanup), ↑/↓/j/k (move), Enter/l/→ (open), Backspace/←/h (up), t (strip toggle), r (rescan), q/Esc (quit), d (trash in session), s (skip in session), h (hash in duplicates). See lib.rs:998-1076.

### Hypothesis
N/A — T0 is recon only

### Method
Read all source files, analyze data structures, scan patterns, ran tests and benchmark

### Result
Source code mapped. Key findings above. All 6 unit tests pass.

### Decision
LOCKED — T0 complete. Findings feed T9 (hygiene).

### Commit hash
N/A (no code changes)

---

## T1a — Nt walker vs jwalk

### Hypothesis
H1 = Nt walker is ≥1.5x faster on median warm runs AND file count and total logical bytes are identical (reparse points excluded from both)

### Method
Built benchmark harness (`examples/bench_scan.rs`) using:
- Windows `NtQueryDirectoryFileEx` with `FileIdExtdDirectoryInformation` (WDK API)
- Parallelized directory traversal with work queue
- Compared against current `jwalk` implementation
- Ran 3 iterations each on two directories (diskexplorer-windows, Downloads)
- Measured wall time, file count, dir count, total logical bytes
- Ran in both debug and release modes

### Result

**Debug build on diskexplorer-windows (3,691 files, 448 dirs):**
- jwalk cold: 241ms, warm median: 306ms
- Nt cold: 43ms, warm median: 46ms
- Speedup: 5.24x
- File count match: true
- Logical bytes match: true

**Release build on Downloads (5,088 files, 510 dirs):**
- jwalk cold: 375ms, warm median: 422ms
- Nt cold: 45ms, warm median: 50ms
- Speedup: 7.50x
- File count match: true
- Logical bytes match: true

**H1 verification:** PASS
- Speed condition (≥1.5x): PASS (5-7x observed)
- Count match condition: PASS (identical file counts and logical bytes)

### Decision
LOCKED — H1 holds. Nt walker becomes the Windows default. jwalk kept as fallback behind a flag/cfg.

### Commit hash
adfd5ca

---

## T1b — Recycle Bin permanence test

### Hypothesis
H2 = the `trash` crate on Windows permanently deletes (instead of recycling) in at least one of:
(a) file larger than the volume's Recycle Bin max capacity, (b) file on a removable or network volume without a bin.

### Method
Created test harnesses (`examples/test_trash.rs`, `examples/test_trash_sizes.rs`, `examples/test_trash_small.rs`) that:
1. Create test files of various sizes (1 KB to 1 GB)
2. Call `trash::delete()` on them
3. Check if file is actually in Recycle Bin by enumerating `$Recycle.Bin/<SID>/` and parsing `$I` metadata files
4. Test on multiple volumes (temp, Downloads)

### Result

**Test (a) - Large files (1 MB to 1 GB):**
- All files permanently deleted (not in Recycle Bin)
- `trash::delete()` returns `Ok(())` but file is gone

**Test (a) - Small files (1 KB text files):**
- Files in Downloads: PERMANENTLY DELETED
- Files in temp dir: PERMANENTLY DELETED
- `trash::delete()` returns `Ok(())` but file is gone, not in Recycle Bin

**Test (b) - Removable/network volume:** UNVERIFIED (no removable drive available)

**Key finding:** On this Windows system, the `trash` crate appears to permanently delete ALL files regardless of size, not just those exceeding Recycle Bin capacity. The Recycle Bin may be disabled, configured with very low capacity, or the `trash` crate may be using `SHFileOperation` with `FOF_NORECYCLE` flag.

### Decision
LOCKED — H2 confirmed. The `trash` crate on this system permanently deletes files instead of recycling them. Must implement `can_recycle(path) -> Result<(), RefuseReason>` guard that:
1. Checks drive type (GetDriveTypeW)
2. Checks per-volume Recycle Bin settings (registry: `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\<GUID>` for `NukeOnDelete` and `MaxCapacity`)
3. Compares file size vs volume capacity
4. On refusal: skip the item, show reason in UI, never delete

### Commit hash
N/A (test code only, guard implementation pending)

---

## T1b — Recycle Bin permanence test (continued)

### Registry investigation findings

**Recycle Bin registry keys on this system:**
- HKLM\...\BitBucket\Volume\ - NOT FOUND (error 2 = FILE_NOT_FOUND)
- HKCU\...\BitBucket\ - EXISTS but NO Volume subkeys
- HKCU\...\BitBucket\KnownFolders - NOT FOUND

This indicates the system uses a different Recycle Bin configuration mechanism, or the Recycle Bin is managed globally without per-volume settings.

### Implementation status

The `recycle_guard.rs` module is implemented with:
- `can_recycle(path) -> Result<(), RefuseReason>` - pre-check before trashing
- `safe_trash(path) -> Result<(), RefuseReason>` - combined check + trash
- Integrated into `App::do_trash()` in lib.rs

**Guard checks:**
1. Drive type (removable, network, CD-ROM, RAM disk → refuse)
2. Volume GUID resolution
3. Registry lookup for NukeOnDelete and MaxCapacity (HKLM then HKCU)
4. File size vs capacity comparison

**Conservative fallback:** If any check fails (volume GUID not found, registry key not found), returns `RefuseReason::UnknownConfiguration` and refuses deletion.

### Decision
LOCKED — T1b complete. Recycle guard implemented and integrated. The `trash` crate on this system permanently deletes files regardless of size; the guard prevents this by refusing when Recycle Bin configuration cannot be verified.

### Commit hash
adfd5ca (same commit as T1a - all changes in single commit)