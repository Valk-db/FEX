# AGENT_TASKS.md — diskexplorer S-PASS (scanner performance). Supersedes the P-pass list. T3–T9 (AGENT_TASKS_v1.md) stay paused until Tyler approves after this pass.

## Why this exists
P-pass accepted (master @ c380a04). Benchmark of the SHIPPED walker (bench_prod, C:\Windows, 174k files, warm medians): Nt 1 thread 45.6s, 4 threads 23.5s, 8 threads 20.3s; the old count-only harness did the same tree single-threaded in 8.6s. Scaling 1→8 threads is only 2.25x. Code at c380a04 (`nt_walker.rs` `spawn_scan_nt` ~L496–530, `scan_nt_internal` ~L323–440):
- Pass 1 is single-threaded: `read_dir_nt` (enumerates files too) on every directory just to collect subdirs. Pass 2 re-enumerates every directory in parallel → each directory is read twice, first read serial.
- Per-file global `Mutex`es (files Vec, two dir-size HashMaps), per-file `PathBuf` clones, per-file ancestor walk with PathBuf-keyed HashMap lookups.
- `FileRecord` is a 9-tuple type alias.
- Reparse FILES (sockets, app-exec aliases) are not emitted at all (53 on the user profile); F2 spec said emit with flag and 0 contributed bytes.
- Unexplained: parity gap of 9.8 MB between two timed runs (the P5 diff tool found only a 4 KB churn mismatch).
Goal: make the scanner fast on non-admin NTFS without changing observable behavior.

## Standing rules (never violate)
1. Brainstorm before building. Verify before building.
2. Commits stay LOCAL until Tyler pushes. No push, no remote ops.
3. Trash is NEVER permanent delete. (Not touched in this pass; do not modify recycle_guard.rs.)
4. Test only on throwaway files you create. Never touch real user files.
5. Every `unsafe` block has a `// SAFETY:` comment.

## Working protocol (every task)
1. Read code first; findings into DECISIONS.md.
2. Pre-register hypothesis + numeric threshold BEFORE measuring. IDs unique; this pass uses H19–H24.
3. `cargo fmt`; `cargo clippy --all-targets -- -D warnings`; `cargo test`. No exemptions.
4. LOCKED only with reproducible evidence: exact command, test names, measured numbers. A test only counts if it can fail: run a mutation and record the ACTUAL failure output.
5. Benchmarks: shipped code path, same retained data both sides, separate process per run, cold/cold and warm/warm reported separately, never mixed. A/B against the baseline commit in the SAME session (machine state drifts): `git worktree add ../FEX_baseline c380a04`, build `bench_prod` there, alternate A/B runs.
6. Direct Win32/NT calls only; no shelling out for measurement.
7. One local commit per task: `S<n>: <summary>`. Real commit hashes in DECISIONS.md.
8. Failed verification → STOP, record, do not continue.
9. Never edit past DECISIONS.md entries; append `## CORRECTION`.
10. Observable behavior is frozen unless a task says otherwise: ScanEvent stream semantics (Dir/Files/Changed/Deleted/Progress), live repaint cadence (150ms), counters, ScanData fields, db snapshot contents, diff-scan results.

---

## S0 — Attribute the time (profile before changing anything)
- Instrument the CURRENT production path with `QueryPerformanceCounter` totals (cfg(feature = "profile") or an env var; zero cost when off): pass-1 enumeration, pass-2 enumeration, per-file processing, mutex wait time, path clone/alloc count, channel send/batch, and — end to end — the consumer side: time inside `App::poll_scan` draining a full-tree scan headlessly (ancestor updates to `dir_sizes`, `file_list` pushes, hardlink map).
- Run on C:\Windows at 1 thread and default threads; record a table of seconds per component.
- Pre-register H19: ≥60% of the current 1-thread wall time (45.6s) is spent OUTSIDE raw directory enumeration (per-file processing, locks, path allocation, aggregation, consumer). If false, say so and re-plan S1 around what the profile shows.
- Record also: is the UI/consumer path a bottleneck? (consumer seconds vs producer seconds, overlapped or serialized).
- Commit instrumentation behind the flag only if it costs nothing when off; otherwise keep it out of main.

## S1 — Single-pass parallel walker
- Each directory is enumerated EXACTLY ONCE (assert with a debug counter in tests). Work-stealing: either `rayon::scope` recursive spawn per subdirectory or crossbeam-deque `Injector` + workers with an atomic pending-count termination. Pick one, justify with measurements; if H21 fails, try the other.
- No per-file locks. Workers accumulate into thread-local/per-task buffers (records batch, per-directory own-size sums) and send batches over the existing channel; batch size a named constant (start 4096 records) so the 150ms repaint stays smooth on small and huge trees.
- Directory sizes: per-directory own-file totals computed by the worker once per directory; propagation to ancestors happens once per DIRECTORY (not per file). Keep dir-size maps correct for live partial views (partial totals must never exceed final totals; monotonic).
- Keep `scan_nt_full` and `spawn_scan` signatures and ScanData/ScanEvent shapes. Keep diff/baseline scans on the same walker. THREAD_COUNT / `DISKEXPLORER_NT_THREADS` behavior preserved.
- Replace the 9-tuple `FileRecord` alias with a named struct (same fields, same order of meaning); update all uses; no behavior change.
- Hypothesis H20 (correctness): P1 fixture test (exact counters, both paths) passes unchanged; plus the new stress test below passes.
- Stress test (new): generate a temp tree with ≥2,000 dirs (random depth ≤ 8, 0–12 files each, a few empty dirs, deterministic seed printed on failure); compare file set, dir set, per-dir recursive sizes against a simple reference recursive `std::fs` walk. Run at 1, 2, 8 threads, 20 iterations each. No lost dirs, no duplicates, sizes equal.
- Mutation checks, record real failure output: (a) skip enumeration of one subdir under a race (e.g., drop a queued dir when pending count hits a threshold), (b) enumerate a dir twice, (c) descend reparse dirs. Each must fail a test.

## S2 — Aggregation / path cost (CONDITIONAL on S0 and S1 results)
- Do this only if, after S1, the profile still shows path hashing/cloning or consumer-side ancestor walks as the largest remaining component. Otherwise record "not needed" with the profile.
- If needed: a dir arena for scan-time aggregation (`DirId(u32)`, `parent: Vec<u32>`, interned name storage) so ancestor propagation uses parent indices (no PathBuf hashing); convert to the app's existing `HashMap<PathBuf,u64>` only where the UI actually needs paths, lazily or once per batch. Do not rewrite views in this pass. Hypothesis H21b is whatever the profile predicts; pre-register it with a number before building.

## S3 — Emit reparse files (decision from F2 spec, now implemented)
- Reparse FILES (non-directory reparse points: sockets, AppExecLink aliases, file symlinks) are emitted as entries with `is_reparse=true`, 0 contributed bytes, counted in `reparse_skipped`, never hashed or grouped by Duplicates, never double-counted. Reparse DIRECTORIES stay listed-but-not-descended.
- Update the fixture/dupes tests accordingly (extend, don't weaken). Integration evidence: on `%USERPROFILE%`, the jwalk-only entry count in the parity diff drops from 53 to 0 (or each remaining one is named and explained).

## S4 — Benchmark the shipped code, A/B (REQUIRED)
- Extend `bench_prod`: (a) scan-only (`scan_nt_full`), (b) END-TO-END headless: `spawn_scan` + the real `App::poll_scan` drain loop until finished (this is what the user waits for).
- A/B: baseline worktree at c380a04 vs new, alternating, same session, C:\Windows and %USERPROFILE%; threads 1, 4, default; 3 runs each (first = cold, rest warm). Also run the old count-only harness once in the same session as the physical floor.
- Pre-registered targets (record honestly if missed; do not move the goalposts after measuring):
  - H21: new, 1 thread, warm median, scan-only ≤ 2.0x the same-session count-only harness time.
  - H22: new, default threads, warm median, END-TO-END ≤ 0.5x of baseline end-to-end at default threads (baseline c380a04 scan-only was 20.3s).
  - H23: scaling: new default-thread warm median ≤ 0.35x of new 1-thread warm median.
  - H24: peak working set not worse than baseline by more than 10% (baseline 136 MB on C:\Windows).
- Decision rule: all pass → LOCKED. Any miss → record numbers, profile the miss (S0 instrumentation) and report; do not start unrelated work.

## S5 — Parity, again
- Dump sorted (path, logical size, is_reparse) lists from baseline and new for both trees outside timing; diff; name every differing path and classify (churn: rerun twice, same path differing every time is not churn; handling difference; artifact). Expected differences: only S3's newly emitted reparse files.
- Explain the earlier unexplained 9.8 MB gap: run baseline-vs-baseline back-to-back twice and report the run-to-run delta on C:\Windows so churn is bounded with data.
- Pre-register H25: after classification, zero unexplained differences.

## S6 — Evidence cleanup
- Re-run each P1/P2/P3 mutation once and paste the ACTUAL failure output (not a one-line summary) into DECISIONS.md.
- Remove the two tautological asserts left in the footer test (`"Mutation: ... CAUGHT"` lines).
- CORRECTION entries as needed; real commit hashes for S0–S6.

## Stop point
After S6, STOP. Do not start T3+. Final report: S0 time-attribution table, S1 design chosen and why, H19–H25 results with numbers (cold/cold and warm/warm separately, scan-only and end-to-end, A/B same session), mutation failure output, parity classification, anything UNVERIFIED, and open PROPOSED decisions for Tyler (1%-of-volume bound, PathTooLong refusal, UNC refusal).