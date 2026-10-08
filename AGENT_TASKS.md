# AGENT_TASKS.md — diskexplorer FIX PASS (replaces previous list; T3–T9 resume after this)

## Why this exists
Independent review of github.com/Valk-db/FEX (master @ 81ef0a9) found T0–T2 were marked LOCKED but are not true in the running app. Findings (all from code, cite line numbers as of 81ef0a9):
1. Live scan is broken: `spawn_scan` (jwalk) emits `file_id=[0;16]`, `volume_serial=0` for every file (lib.rs:335, 347). `poll_scan` dedups hardlinks on `(volume_serial, file_id)` (lib.rs:560, 615), so every file after the first is treated as a hardlink sibling and its size is never added.
2. Nt walker is NOT wired in: `spawn_scan_nt` is never called; TUI uses jwalk (lib.rs:508, 798); `scan()` returns empty maps (lib.rs:210–222). DECISIONS.md claim "integrated as Windows default" is false.
3. Nt walker: single-threaded (not parallel); descends directory reparse points/junctions (`is_dir` checked before `is_reparse`, no `FILE_FLAG_OPEN_REPARSE_POINT`); open failures and mid-dir NTSTATUS errors are silent and uncounted; mtime uses `ChangeTime` not `LastWriteTime`; `Vec<u8>` cast to struct (alignment not guaranteed).
4. One `unreadable_count` is shared by reparse + cloud + hardlink siblings; footer text is wrong.
5. T1b: "trash permanently deletes everything" is unreproducible (test examples not committed) and likely caused by `\\?\`-prefixed paths (all paths are `canonicalize()`d: lib.rs:228, 430; main.rs:38). Guard (`recycle_guard.rs`) builds registry subkey as `Volume\Volume{GUID}` (likely wrong; real subkey probably bare `{GUID}`), reads HKLM only (DECISIONS.md says HKLM then HKCU), treats missing key as `UnknownConfiguration` (refuses on default config), calls `GetDriveTypeW` on `\\?\C:` with no trailing backslash, and `safe_trash` maps all trash errors to `UnknownConfiguration`. Net: cleanup refuses everything.
6. Process: hypothesis ID "H2" used twice; T0 "peak RSS" was actually binary size; `diskexplorer.exe` (6 MB) is committed.

## Standing rules (never violate)
1. Brainstorm before building. Verify before building.
2. Commits stay LOCAL. No push, no remote ops, no force anything.
3. Trash is NEVER permanent delete. Cannot recycle → REFUSE and report. No fallback to permanent delete, ever.
4. Test only on throwaway files you create in a temp dir. Never touch real user files.
5. Keybindings: list existing keys first, pick unused, document in in-app help.
6. Every `unsafe` block gets a `// SAFETY:` comment.

## Working protocol (every task)
1. Read the relevant code first; write 3–10 lines of findings into DECISIONS.md under the task ID.
2. Pre-register hypothesis + pass/fail threshold in DECISIONS.md BEFORE measuring/testing. Hypothesis IDs are unique and never reused (this pass uses H6–H10).
3. Implement. `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` (on Windows).
4. A decision may be marked LOCKED only if DECISIONS.md cites reproducible evidence: exact command, test names, key numbers. "Tests pass" alone is not evidence. Measured numbers must be measured (no estimates, no substituting binary size for RSS).
5. One local commit per task: `F<n>: <summary>`.
6. If verification fails: STOP, record, do not start dependent tasks.
7. Never edit past DECISIONS.md entries; append `## CORRECTION` entries instead.

---

## F0 — Housekeeping (first)
- `git show HEAD:AGENT_TASKS.md > AGENT_TASKS_v1.md` (old task list; T3–T9 in it resume after this pass, with amendments at the bottom of this file). Commit it.
- `git rm --cached diskexplorer.exe`; add `*.exe` and `target/` to the repo-root `.gitignore`. Keep the local file.
- Append `## CORRECTION` entries to DECISIONS.md marking T1a, T1b, T2 as REOPENED with the reasons from "Why this exists" (one short entry each).
- Commit: `F0: housekeeping`.

## F1 — Stop the bleeding: hardlink dedup on zero IDs
- Hypothesis H6: after the fix, scanning a fixture dir through the real `App`/`poll_scan` path yields dir totals equal to the sum of file sizes, and `hardlink_siblings` = 0. Before the fix, reproduce the bug with a failing test first (record the failing output).
- Fix: records with `file_id == [0;16]` (and/or `volume_serial == 0`) are "identity unknown": never dedup, always count.
- Tests must drive `App::poll_scan` (not only walker/ScanData): (a) N jwalk-style records with distinct sizes → totals == sum; (b) two records with identical non-zero (serial, id) → counted once, second flagged hardlink sibling; (c) both in Full and Changed event paths (lib.rs:538 and 586 blocks).
- Commit: `F1: ...`.

## F2 — Wire the Nt walker into the app, correctly
### Design requirements
- One walker for BOTH full scans and baseline/diff scans on Windows (so semantics never differ). jwalk stays for non-Windows and behind a `--jwalk` flag/fallback. Baseline map type must carry the fields needed to compare (size, allocated, mtime, file_id, serial) — replace the `(u64, i64)` baseline.
- Delete the stub `scan()` that returns empty maps, or make it return real data; grep callers and fix them.
- Walker:
  - Parallel across directories (rayon scope or crossbeam work-stealing; thread count a named constant, default = logical cores).
  - Open directories with `FILE_LIST_DIRECTORY | SYNCHRONIZE`, share `READ|WRITE|DELETE`, `FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT`.
  - Check reparse BEFORE is_dir: reparse directories are emitted as entries with `is_reparse=true`, contributing 0 bytes, never descended.
  - Aligned buffer (e.g. `Vec<u64>` or aligned alloc), parse using `iosb.Information`/NextEntryOffset safely.
  - Directory open failure or non-success NTSTATUS (other than NO_MORE_FILES) → count as `unreadable_dirs`, keep first 20 paths + status codes for display.
  - mtime = `LastWriteTime` (converted with checked arithmetic). Do not use ChangeTime.
  - Use `windows` crate constants for attributes (reparse, recall-on-open, recall-on-data-access, offline), not hand-typed hex.
- Path handling: single helper `simplify_path(&Path) -> PathBuf` (`\\?\X:\...` → `X:\...`; `\\?\UNC\srv\share\...` → `\\srv\share\...`; leave verbatim if simplified length would exceed 259). Everything shown in UI, passed to `trash`, `open`, `GetDriveTypeW`, registry/volume lookups goes through it. Unit-test the helper (drive path, UNC, long path, already-simple path).
- Counters split and shown separately in footer: `hardlink_siblings`, `reparse_skipped`, `cloud_skipped`, `unreadable_dirs`, `unreadable_files`. Footer text must say what each is. No shared counter.
### Verification
- Fixture test (temp dir): nested dirs, empty file, hardlink pair, directory junction pointing at its own parent (`mklink /J` via std::process::Command), symlink (skip with logged reason if no privilege), unreadable dir (deny ACL via `icacls` for current user; restore after), cloud-flag file cannot be faked → unit-test the attribute classifier on synthetic attribute values.
- Hypothesis H7 (correctness): scan terminates on the junction loop; no double counting; counts per category match the fixture exactly; file count and logical bytes equal jwalk on the fixture excluding reparse entries (report any discrepancy with cause).
- Hypothesis H8 (performance): on a large tree (≥500k files: use the largest real tree available, e.g. `C:\Windows` or `C:\Users`; if none reach 500k, say so and report the actual count), parallel Nt walker median of 3 warm runs ≥1.5x faster than jwalk, with parity as in H7. Also report the first (coldest) run for each. If H8 speed fails but parity holds, keep Nt anyway ONLY if median ≥1.0x (it provides file IDs/allocated size) and record the numbers honestly; if <1.0x, STOP and report.
- Also measure and record peak working set (`PeakWorkingSetSize` via `GetProcessMemoryInfo`) after the large scan and bytes/file. This replaces T0's wrong "RSS" entry (append CORRECTION).
- End-to-end: use `examples/preview.rs` / `preview_html.rs` to render the Browse view for the fixture dir and assert displayed sizes equal expected; assert the footer shows split counters.
- Commit: `F2: ...`.

## F3 — Redo T1b properly (reproducible probe)
- Commit the harness as `examples/trash_probe.rs` (not deleted afterwards).
- Cases, each using a throwaway file under (i) `std::env::temp_dir()` and (ii) a temp dir under the user profile: A) path as `canonicalize()` returns it (`\\?\` prefix); B) `simplify_path` result.
- Verify recycling two independent ways: `SHQueryRecycleBinW` item-count delta on the drive root, and `trash::os_limited::list()` before/after matching the file. Also confirm the file no longer exists.
- Dump (names and value data, no secrets) everything under `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket` and `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\BitBucket` recursively into DECISIONS.md, plus `NoRecycleFiles` under both `...\Policies\Explorer` keys. Determine empirically the real per-volume subkey naming and which hive holds `NukeOnDelete`/`MaxCapacity`.
- Oversize case: only if MaxCapacity is determinable. Create a sparse-style file with `File::set_len` slightly above capacity; do not write real data; ensure the temp volume has room. Otherwise mark UNVERIFIED.
- Removable/network: UNVERIFIED unless such a volume exists; do not fake it.
- Hypothesis H9: B (simplified) is recycled and A (verbatim) is not. Alternate outcome "neither recycles" → the bin is disabled/policy-blocked on this machine: record the registry evidence and report to Tyler; do not guess. Record the actual table of results.
- Commit: `F3: ...`.

## F4 — Fix the guard + single deletion choke point
- Every path goes through `simplify_path` before any check.
- Root for `GetDriveTypeW`/`GetVolumeNameForVolumeMountPointW` always `X:\` (trailing backslash). UNC paths → refuse as network.
- Registry per F3 findings: correct subkey format; check HKCU first then HKLM; MISSING KEY = default config (enabled), not a refusal; policy `NoRecycleFiles=1` → BinDisabled.
- MaxCapacity unknown → compare against a conservative bound: refuse files larger than 1% of volume size (PROPOSED constant, flag it in DECISIONS.md as PROPOSED).
- Paths still >259 chars after simplification → refuse with `PathTooLong` (PROPOSED).
- Distinct error types: `Refused(RefuseReason)` vs `TrashFailed(io/trash error)`. Never map a trash error to a refusal reason.
- Post-delete trip-wire: after `trash::delete` returns Ok, compare `SHQueryRecycleBinW` count before/after for that drive; if not increased, show a loud status "NOT VERIFIED IN RECYCLE BIN" and disable further deletions this session.
- Exactly one call site of `trash::delete` (inside `recycle_guard`). `do_trash` (lib.rs:1064) calls only the guard's entry point. Add a test that scans `src/**/*.rs` and fails if `trash::delete(` appears outside `recycle_guard.rs`.
- Tests: pure-logic units (capacity compare, drive-type map, `PathTooLong`, missing-key=default path via injectable config reader).
- Hypothesis H10: on this machine's default config, a normal temp file passes the guard AND is actually recycled (F3 probe style verification); oversize/removable/UNC/long-path cases are refused (units + F3 evidence where available).
- Commit: `F4: ...`.

## F5 — Re-verify the other T2 claims end to end
- Size mode toggle (`S`): totals change between logical/allocated; choice PERSISTS across restarts (implement persistence if missing, e.g. sqlite settings table); footer shows mode.
- Duplicates excludes hardlink siblings, cloud placeholders, reparse points, zero-byte files (read `dupes.rs`; add tests). Cloud placeholders are never opened/read for hashing.
- Evidence = test names + preview renders recorded in DECISIONS.md.
- Commit: `F5: ...`.

## Stop point
After F5, STOP. Do not start T3+. Produce the final report: status table F0–F5 (done/blocked/failed), results for H6–H10 with numbers, anything UNVERIFIED, PROPOSED decisions awaiting Tyler (1%-of-volume bound, PathTooLong refusal), and any other discrepancy found.

## Amendments for T3–T9 (when resumed from AGENT_TASKS_v1.md)
- All new code uses `simplify_path`, the single trash entry point, the split counters, and the unified FileRecord/baseline type.
- T3/T6 time semantics use LastWriteTime (now correct).
- T9 uses the F2 measured peak working set and bytes/file as its baseline.
- T8 stays BLOCKED on Tyler's Windows notes.