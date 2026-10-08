# AGENT_TASKS.md — diskexplorer P-PASS (evidence repair). Supersedes the G-pass list. T3–T9 (AGENT_TASKS_v1.md) stay paused until this completes and Tyler approves.

## Why this exists
Review of master @ 900ebc6. Guard path (G1–G3) is good. These G-pass claims are NOT supported by the code:
1. Benchmark (H13) did not time the shipped walker. `examples/bench_scan.rs` `run_nt_child` is a separate single-threaded copy (`read_dir_nt` + `dirs_to_process` stack) that only counts; production `nt_walker::scan_nt_full` / `spawn_scan` (parallel, builds records) was never timed. The "2.53x" and "7x lower memory" describe the harness copy. 2.53x also mixes jwalk COLD / Nt WARM (warm/warm = 2.65x); DECISIONS.md uses both. 174k files in ~8.6s (~20k files/s; cold ≈ warm) suggests syscall/CPU-bound directory opens; thread scaling was never checked.
2. Parity: logical bytes differ by exactly 8,192 and dir counts by 1 (75,048 vs 75,047); labeled "reparse noise" with no evidence.
3. H7 re-lock is vacuous: `fixture_test_nested_dirs_empty_file_hardlink_junction_unreadable` asserts only `is_ok`, `file_count > 0`, `total_logical_bytes > 0`. The expected 8,292 is a comment. `_junction_created` result is discarded (passes if mklink fails). Hardlink dedup, junction-not-followed, `unreadable_dirs` unasserted.
4. `fixture_test_footer_render` builds the footer string itself and asserts it contains words it just wrote (never renders the app).
5. `fixture_test_dupes_excludes_hardlink_cloud_reparse_zero` calls `dupes::size_groups` on (path,size) pairs; the hardlink/cloud/reparse flags in its tuples are never read. Only zero-byte is actually tested.
6. G2 write-up: table says canonical (A) never found in `list()`, note says one A case was true ("race"); the two "locations" are the same dir; the `$I`-metadata root cause was asserted without inspecting it.

## Standing rules (never violate)
1. Brainstorm before building. Verify before building.
2. Commits stay LOCAL until Tyler pushes. No push, no remote ops.
3. Trash is NEVER permanent delete. Cannot recycle → REFUSE and report.
4. Test only on throwaway files you create. Never touch real user files.
5. Every `unsafe` block has a `// SAFETY:` comment.

## Working protocol (every task)
1. Read code first; findings into DECISIONS.md.
2. Pre-register hypothesis + threshold BEFORE measuring. IDs unique; this pass uses H15–H18.
3. `cargo fmt`; `cargo clippy --all-targets -- -D warnings`; `cargo test`. No exemptions.
4. LOCKED only with reproducible evidence: exact command, test names, measured numbers. A test only counts as evidence if it can fail: show it failing (mutation check, below) before claiming it passes.
5. Benchmarks measure the SHIPPED code path with the SAME retained data on both sides. Never compare cold to warm. Report cold/cold and warm/warm separately.
6. Never measure by shelling out; direct Win32/NT calls only.
7. One local commit per task: `P<n>: <summary>`. Fill real commit hashes in DECISIONS.md (replace every "pending").
8. Failed verification → STOP, record, do not continue.
9. Never edit past DECISIONS.md entries; append `## CORRECTION`.

---

## P1 — Real fixture test (replaces the vacuous one)
- Expose the split counters on `ScanData` (`hardlink_siblings`, `reparse_skipped`, `cloud_skipped`, `unreadable_dirs`, `unreadable_files`) if not already; the test must read them from the real data, not infer.
- Fixture built by code with a known expected result: nested dirs (3 levels); empty file; files of distinct known sizes; hardlink pair (`fs::hard_link`); directory junction pointing at its own parent; one deny-ACL dir.
- The test must FAIL (not skip, not pass vacuously) if: mklink fails (assert exit status AND that the junction exists and `fs::read_dir` through it works), or icacls deny didn't take effect (assert `fs::read_dir(unreadable)` returns Err before scanning). ACL restore in a drop guard; delete the junction with `rmdir` (never recurse through it).
- Assert EXACT: file_count, dir_count, logical bytes (hardlink counted once), allocated-bytes relationship (≥ logical for non-empty files), `hardlink_siblings == 1`, `reparse_skipped == 1`, `unreadable_dirs == 1`, `cloud_skipped == 0`, `unreadable_files == 0`, and that NO scanned path lies under the junction.
- Run the same assertions through BOTH paths: `scan_nt_full` and the real streaming path (`spawn_scan` → `App::poll_scan`).
- Mutation evidence (H15): temporarily (a) make the walker descend reparse dirs, (b) disable hardlink dedup, (c) swallow dir-open errors silently. Show each makes the test FAIL; record the failure messages in DECISIONS.md; revert each mutation. Hypothesis H15: all three mutations are caught.

## P2 — Real footer test
- Feed scan events into a real `App`, render the UI into a ratatui `TestBackend` buffer, and assert the buffer text contains each split counter with the exact fixture numbers and the size-mode text. No hand-built strings. Delete `fixture_test_footer_render`'s tautology.
- Mutation: change one counter label/value in the footer code → test fails; record, revert.

## P3 — Real dupes exclusion test
- Drive `App::poll_scan` with synthetic records: (1) a genuine duplicate pair (same content, different file IDs, real files); (2) a hardlink pair (same non-zero volume_serial+file_id); (3) a cloud-flagged record at a NONEXISTENT path; (4) a reparse-flagged record; (5) a zero-byte file; (6) a same-size/different-content file.
- Assert `size_groups`/`dupe_groups` equal exactly the expected set (only the genuine pair + the same-size candidate in the size group; hardlink sibling, cloud, reparse, zero-byte absent), and that no hash attempt is made for the cloud record (nonexistent path would error — assert no error and no hash entry).
- If exclusions are not implemented where the test says they must be, fix the code (filter in the App's file list / grouping using the record flags), then re-run. Mutation check: remove one exclusion → test fails; record, revert.

## P4 — Benchmark the shipped walker (REQUIRED)
- New example `bench_prod` (keep the old harness but label it "harness copy" in DECISIONS.md). Each measured run is a separate child process, one walker per process, one run per process; parent collects results.
- Side A: production Nt walker (`scan_nt_full` or the exact function `spawn_scan` uses), default threads. Side B: jwalk-based scan that builds the SAME record structure into the SAME retained collections (extract the non-Windows `scan()` jwalk logic as a bench-only function). Both retain what the app retains.
- Record per run: wall ms, files, dirs, logical bytes, peak working set (`PeakWorkingSetSize`), thread count.
- Trees: `C:\Windows` and the user profile dir (`%USERPROFILE%`), whichever exist; report file counts.
- Thread scaling for side A via an env var (e.g. `DISKEXPLORER_NT_THREADS`): 1, 4, and default (logical cores), 3 runs each (first = cold, rest warm).
- Pre-register H16: production Nt, default threads, warm median ≥ 1.5x faster than jwalk-equivalent (warm/warm), with the SAME retained data. H17: Nt walker scales: default-thread warm median ≤ 0.6x of the 1-thread warm median. If H17 fails: profile per-directory open time vs enumeration vs record-processing (instrument with `QueryPerformanceCounter` totals) and report where time goes. Do not change the walker in this pass unless the profile shows an obvious bug; report instead.
- Decision rule: H16 pass → LOCKED. 1.0–1.5x → keep Nt, record honestly. <1.0x or parity fails → STOP and report.

## P5 — Parity, explained
- Outside timing, dump sorted (path, logical size) lists from both sides for each tree and diff them. Name EVERY differing path with its size/mtime. Classify each as (a) changed during run (re-run twice; if the same path differs every time it is not churn), (b) genuine handling difference, or (c) harness artifact.
- Pre-register H18: after classification, every difference is explained, and there are zero unexplained differences. The 8,192-byte delta and the 75,048 vs 75,047 directory count must be explained by name (likely the root dir counting — confirm).

## P6 — DECISIONS.md corrections
- CORRECTION entries: H13 (harness copy, mixed cold/warm ratio), H7 re-lock (vacuous tests), the footer/dupes tests, G2 note inconsistencies (reconcile the table vs the "race" note; state that both locations were the same directory and run the probe once more from a genuinely different directory, e.g. a folder on the user profile, if one differs).
- Do NOT assert the `$I`-metadata root cause unless you inspect an actual `$I` file and record the evidence; otherwise mark it UNVERIFIED.
- Replace all "(pending)" commit hashes with real ones.

## Stop point
After P6, STOP. Do not start T3+. Final report: P1–P6 status, H15–H18 results with numbers (warm/warm and cold/cold separately), mutation-check evidence, parity classification table, anything UNVERIFIED, and PROPOSED decisions awaiting Tyler (1%-of-volume bound, PathTooLong refusal, UNC refusal).