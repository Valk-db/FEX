# AGENT_TASKS.md — diskexplorer FIX PASS 2 (G-pass). Supersedes the F-pass list. T3–T9 stay in AGENT_TASKS_v1.md and remain paused.

## Why this exists
Review of github.com/Valk-db/FEX master @ db530fe. F1 and the Nt-walker wiring are real. These are NOT verified or are wrong:
1. H9 ("trash permanently deletes everything") is almost certainly a measurement artifact. Bin item count is measured by spawning PowerShell with inline C# (`recycle_guard.rs` `query_recycle_bin_count`, `examples/trash_probe.rs` `get_recycle_bin_count`). The struct `SHQUERYRBINFO` is declared top-level but the script does `New-Object RecycleBin+SHQUERYRBINFO` (nested-type syntax) → likely throws → output empty → code returns 0 on ANY failure. So before=after=0 always; "delta 0" proves nothing. Registry evidence (bin enabled, NukeOnDelete=0, 8089 MB cap) contradicts the conclusion; `os_limited::list()` results are absent from the F3 table.
2. Consequence: `safe_trash` returns `NotVerifiedInRecycleBin` after every delete → UI says "permanently deleted!" and disables cleanup after the first trash.
3. H10 pre-registered "actually recycled"; H9 says nothing recycles; H10 marked LOCKED with no evidence. Promised unit tests (capacity compare, PathTooLong, injectable config, single-call-site) were not added.
4. H7 marked LOCKED without the required fixture tests (junction loop, hardlink pair, unreadable dir, simplify_path, jwalk parity). Test count is 12 (only +2 since before F1).
5. H8 not measured. No F5 entry in DECISIONS.md (F4 text was committed inside F5). Dupes exclusions untested.
6. Hygiene: `cargo clippy -- -D warnings` fails (dead `mtime_of`/`SCAN_BATCH`, unused mut, cast, type complexity, needless return); two `#[cfg(not(windows))] pub fn spawn_scan` defs (lib.rs ~334 and ~434); `BaselineEntry` is an 8-tuple; stale comment at lib.rs ~1946.

## Standing rules (never violate)
1. Brainstorm before building. Verify before building.
2. Commits stay LOCAL. No push, no remote ops.
3. Trash is NEVER permanent delete. Cannot recycle → REFUSE and report.
4. Test only on throwaway files you create. Never touch real user files.
5. Every `unsafe` block has a `// SAFETY:` comment.

## Working protocol (every task)
1. Read code first; findings into DECISIONS.md.
2. Pre-register hypothesis + threshold BEFORE measuring. IDs unique, never reused; this pass uses H11–H14.
3. `cargo fmt`; `cargo clippy --all-targets -- -D warnings` MUST pass (no "pre-existing warnings" exemption); `cargo test`.
4. LOCKED only with reproducible evidence: exact command, test names, measured numbers. If your evidence contradicts your conclusion or a pre-registered outcome, say so and mark the decision OPEN.
5. Never measure by shelling out to PowerShell/cmd. Use direct Win32/NT calls via the `windows` crate. A measurement that cannot fail loudly (returns 0 on error) is a bug: return `Result`.
6. One local commit per task: `G<n>: <summary>`.
7. Failed verification → STOP, record, do not start dependent tasks.
8. Never edit past DECISIONS.md entries; append `## CORRECTION`.

---

## G1 — Direct bin query (replaces PowerShell)
- Add `windows` feature `Win32_UI_Shell`; implement `fn recycle_bin_item_count(root: &str) -> Result<u64, BinQueryError>` via `SHQueryRecycleBinW` (struct size set correctly). Never return 0 on failure.
- Remove ALL PowerShell spawning from `src/` and `examples/` for counting (registry dump in the probe may use `windows` registry APIs or `reg query` via std::process only if direct API is impractical — prefer the API).
- Test: `recycle_bin_item_count("C:\\")` returns Ok (system drive from env).

## G2 — Validate the counter, then redo the trash probe
- Control FIRST (validates the measurement): delete a throwaway file via a known-good recycle path (`powershell -Command "Add-Type -AssemblyName Microsoft.VisualBasic; [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile(path,'OnlyErrorDialogs','SendToRecycleBin')"` — PowerShell allowed for the DELETE only). Expect count delta +1 and `trash::os_limited::list()` shows it.
- Print environment facts into DECISIONS.md: `whoami`, elevated yes/no, session id (`ProcessIdToSessionId`), Windows build, `trash` crate version.
- If the control does NOT show +1: measurement or environment is invalid → STOP and report; do not run further cases.
- Then `examples/trash_probe.rs` cases: path A (`canonicalize()` verbatim) / path B (`simplify_path`) × temp dir / profile-temp dir × sizes 1 KB, 100 KB, 1 MB, 100 MB. Per case record: count delta, `os_limited::list()` delta matched by original path, file-exists-after. Put ALL three columns in the DECISIONS.md table.
- Pre-register H11: for path B, `trash::delete` recycles (delta +1 AND found in `list()`). Record A separately; no prior on A.
- If B fails while the control passes: find the root cause with evidence (check `trash` crate source/flags, COM apartment init on the calling thread, thread used by rayon/TUI) before concluding anything. Do not guess.
- Append `## CORRECTION — H9` with the result.

## G3 — Guard semantics + tests
- Verification result enum: `Verified | Unverifiable(BinQueryError) | NotIncreased`. Only `NotIncreased` disables further deletions this session. `Unverifiable` shows a warning and continues. Status text for `NotIncreased` must say "recycle bin count did not increase" (do not assert permanent deletion as certain).
- With direct FFI the count check is per file (cheap). No process spawns anywhere in the delete path.
- Make config access injectable (trait or closure) so tests can fake it. Unit tests: missing key = default enabled; NukeOnDelete=1 → BinDisabled; policy NoRecycleFiles=1 → BinDisabled; MaxCapacity boundary (file == cap passes, cap+1 refused; MB vs bytes); unknown cap → 1% rule (PROPOSED); `PathTooLong` at 259 vs 260; UNC refused; drive-type mapping; `simplify_path` cases (drive path, UNC, already simple, >259 stays verbatim).
- Test that scans `src/**/*.rs` and fails if `trash::delete(` appears outside `recycle_guard.rs`.
- End-to-end test marked `#[ignore]` (run with `cargo test -- --ignored`): normal temp file → guard Ok → `safe_trash` Ok → found in `list()`.
- Hypothesis H12: on this machine, a normal temp file passes the guard, `safe_trash` returns Ok, and the file is in the bin (G2/H11 must hold first; if H11 failed, H12 cannot be claimed). Record evidence.

## G4 — The missing F2/F5 tests
- Fixture tests (temp dir): nested dirs; empty file; hardlink pair (`fs::hard_link`); directory junction pointing at its own parent (`mklink /J` via std::process::Command, then clean up); unreadable dir (`icacls` deny for current user; restore ACL in a drop guard); symlink only if privilege allows (log skip reason).
- Assertions: scan terminates on the junction loop; no double counting; each split counter (`hardlink_siblings`, `reparse_skipped`, `cloud_skipped`, `unreadable_dirs`, `unreadable_files`) equals the fixture exactly; file count + logical bytes equal a jwalk scan of the fixture excluding reparse entries (keep jwalk available under `cfg(test)` or as a dev path for parity).
- Classifier unit tests on synthetic attribute values: reparse, recall-on-open, recall-on-data-access, offline.
- Footer: render with `examples/preview.rs` on the fixture and assert the split counters and sizes appear.
- Dupes tests via `poll_scan` with synthetic records: hardlink siblings, cloud-flag record, reparse record, zero-byte → never grouped and never hashed/opened (use a nonexistent path for the cloud record; hashing must not be attempted).
- Size-mode toggle: set → reopen the db → value persists; totals differ between logical/allocated on a fixture file with different sizes.
- Append `## CORRECTION — H7`/F5 and a proper `## F5` entry with test names.

## G5 — H8 benchmark (REQUIRED)
- Pre-register H13: on the largest available tree, parallel Nt walker median of 3 warm runs ≥1.5x jwalk, with file-count and logical-byte parity (reparse excluded on both). Report actual file count; if <500k say so. Report first (coldest) run for each too.
- Measure peak working set via `GetProcessMemoryInfo` (`PeakWorkingSetSize`) after the scan and bytes/file. Record raw numbers.
- Decision rule: ≥1.5x and parity → LOCKED. Parity but 1.0–1.5x → keep Nt (it provides file IDs/allocated size), record honestly. <1.0x or parity fails → STOP and report.

## G6 — Hygiene
- `cargo clippy --all-targets -- -D warnings` passes. Remove or cfg-gate dead code (`mtime_of`, `SCAN_BATCH`, unused vars).
- Exactly one `spawn_scan` per cfg (remove the duplicate `#[cfg(not(windows))]` definition). If the Linux target is installed run `cargo check --target x86_64-unknown-linux-gnu`; otherwise note it was not run.
- Replace the 8-tuple `BaselineEntry` with a named struct. Fix the stale comment near lib.rs:1946.
- Hypothesis H14: no behavior change — all tests (existing + G3/G4) pass before and after; record counts.

## G7 — DECISIONS.md accuracy
- CORRECTION entries for: H7 (locked without required tests), H9 (measurement artifact, per G2), H10 (contradicted its pre-registration, unverified), F5 (missing entry).
- New entries G1–G6 with evidence. Hypotheses: H11, H12, H13, H14 each with LOCKED / OPEN / REJECTED and the evidence line.

## Stop point
After G7, STOP. Do not start T3+. Final report: status table G1–G7, H11–H14 results with numbers, anything UNVERIFIED, PROPOSED decisions awaiting Tyler (1%-of-volume bound, PathTooLong refusal), and any new discrepancy found.