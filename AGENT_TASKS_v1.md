# AGENT_TASKS.md — diskexplorer (Rust, Windows TUI)

## Context
Rust TUI disk explorer. Stack: jwalk+rayon, blake3, rusqlite (bundled), ratatui/crossterm, `trash`, `open`.
Shipped (v2–v2.3): Browse / Duplicates / Growth views, recursive sizes, size bars, strip toggle; Duplicates = instant size grouping + manual blake3 via `h`, hashes persist across scans; Growth = sqlite snapshots, new/grown/deleted vs previous scan; opens instantly from last snapshot, background diff-only scan, 150ms live repaint, `r` rescans without blanking; `←` up, `→` enter/open; names hard-truncated so sizes align; Cleanup sessions scoped to current folder, trash with `y` confirm.

## Standing rules (never violate)
1. Brainstorm before building. Verify before building.
2. Commits stay LOCAL. No `git push`, no remote ops, no force anything.
3. Trash is NEVER permanent delete. If a file cannot be recycled, REFUSE and report. No fallback to permanent delete, ever.
4. Test only on throwaway files you create in a temp dir. Never touch real user files.
5. Don't invent keybindings that collide: list existing keys first, pick unused ones, document them in the in-app help.

## Working protocol (every task)
1. Read the relevant code first. Write findings (3–10 lines) into `DECISIONS.md` under the task ID.
2. Pre-register the hypothesis + pass/fail threshold in `DECISIONS.md` BEFORE running the measurement/test.
3. Implement. Run `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test`.
4. Record evidence (numbers, command output, test names) and the locked decision in `DECISIONS.md`.
5. One local commit per task: `T<n>: <summary>`.
6. If verification fails, STOP. Record it. Do not start dependent tasks.

`DECISIONS.md` entry format: `## T<n> — title` / Hypothesis / Method / Result / Decision (LOCKED | PROPOSED | REJECTED) / Commit hash.

---

## T0 — Recon (no code changes)
- Map: scanner, tree representation, snapshot schema (sqlite), hash cache schema, cleanup/trash path, key bindings, render loop.
- Report in `DECISIONS.md`: node struct and measured bytes/node, peak RSS on the largest scan available, release profile settings, `cargo tree -d` duplicates, whether paths are stored per-node as `PathBuf` or via arena/interned components, whether sorts/aggregates recompute every 150ms tick or only on dirty.
- Change nothing here. Findings feed T9.

## T1 — Scanner benchmark + trash safety test
### T1a — Nt walker vs jwalk
- Build a bench harness (separate bin or `--bench-scan <path>`), not wired into the TUI yet.
- Implement a Windows walker on `NtQueryDirectoryFileEx` with `FileIdExtdDirectoryInformation`, parallelized with rayon, capturing per entry: name, logical size (EndOfFile), allocated size (AllocationSize), 128-bit file ID, attributes, reparse tag, mtime.
- Compare against current jwalk scan on the same large directory (largest available, ideally a whole drive root or user profile). Run each 3x; report first run (coldest) and median of remaining; also wall time, file count, dir count, total logical bytes.
- Pre-register: H1 = Nt walker is ≥1.5x faster on median warm runs AND file count and total logical bytes are identical (reparse points excluded from both).
- Decision: if H1 holds → Nt walker becomes the Windows default, jwalk kept as fallback behind a flag/cfg. If not → keep jwalk, record numbers, and still capture file-ID/allocated size via the cheapest alternative needed by T2.

### T1b — Recycle Bin permanence test
- Pre-register: H2 = the `trash` crate on Windows permanently deletes (instead of recycling) in at least one of: (a) file larger than the volume's Recycle Bin max capacity, (b) file on a removable or network volume without a bin.
- Method: create throwaway files; (a) set up a file bigger than the bin quota for the test volume; (b) if a removable drive is present, repeat there — if none, mark (b) UNVERIFIED and say so. After each delete, verify whether the file is actually in the Recycle Bin (enumerate the bin) or gone.
- Implement `can_recycle(path) -> Result<(), RefuseReason>`: drive type (GetDriveTypeW), per-volume bin enabled / NukeOnDelete / MaxCapacity (derive from system APIs or the BitBucket registry key; verify the actual location empirically), file size vs capacity. Wire it into every trash call site. On refusal: skip the item, show the reason in the UI, never delete.
- Add tests for the pure-logic parts (size vs capacity, drive type mapping).

## T2 — Scan correctness (depends on T1a decision)
Implement and verify with a generated fixture dir (hardlinks, junction, symlink, empty file, nested dirs, a file with no read permission):
- Hardlinks: dedupe by (volume serial, file ID). Size totals count a hardlinked file once; Duplicates excludes hardlink siblings (they reclaim nothing). Show a `hardlinked` marker.
- Size toggle: logical vs size-on-disk (allocated). Persist the choice. Show current mode in the footer.
- Reparse points / junctions / symlinks: never followed. Listed as entries with zero contributed size and a flag. No loops possible.
- Cloud placeholders (attributes RECALL_ON_DATA_ACCESS / RECALL_ON_OPEN / OFFLINE): never read or hash them (must not trigger hydration); exclude from Duplicates hashing and from reclaimable totals; show a `cloud` marker.
- Access denied / errors: count them; footer shows `N unreadable, ~X not counted` so totals are never silently wrong.
- Tests: assert each fixture case. Record evidence.

## T3 — Growth: schema lock + arbitrary timeframe
### T3a — Schema (migrate, don't lose history)
- Back up the existing sqlite db file before migrating. Migration must preserve existing snapshot history.
- New model: per-directory size time series (dir_id, ts, bytes, file_count) + file-level changelog (file_id/path_id, ts, event = appeared | disappeared | resized, old_size, new_size). Integer path IDs, no repeated text paths. WAL mode, one transaction per scan, prepared statements.
- Retention: keep every scan for 30 days, then one per week. Constants in one config struct. Pruning runs after a successful scan only.
- Pre-register: H3 = for randomized synthetic histories (property test, ≥200 random histories), the changelog-derived diff between any two timestamps equals the brute-force diff of full snapshots exactly.
### T3b — Timeframe locking UI
- Growth view gets a locked "from" anchor: presets (24h, 7d, 30d) plus custom date; lock persists across rescans until cleared. Default unlocked behavior = vs previous scan (unchanged).
- Footer shows the active timeframe. New/grown/deleted computed from the changelog.

## T4 — Types view (view 4)
- Group files by extension (lowercase; no extension = `(none)`). Columns: ext, total size, count, % of scope. Sort toggle: size / count / name.
- Category rollups (Video, Images, Audio, Archives, Installers, Disk images, Documents, Code, Other) via a table in one file, easy to edit.
- Drill into a type: folder distribution (group by immediate parent dir): bytes, count, % of that type.
- Stray detection — PROPOSED, NOT APPROVED (record as PROPOSED in DECISIONS.md): per extension, greedily take parent folders by bytes (descending) until they cover 90% of that type's bytes = "home folders". Strays = files of that type outside home folders with size ≥ 1 MiB, ranked by size. 90% and 1 MiB are constants in the config struct. Types with a single folder have no strays.
- Pre-register H4 on fixture data: strays equal exactly the planted outliers, with no false positives from home folders.
- In the report, print stray output for the largest real scan available so Tyler can judge the rule. Do not finalize the thresholds.
- Respect T2 semantics (hardlinks once, cloud placeholders flagged, size mode toggle).

## T5 — Duplicates pipeline
- Keep hashing manual via `h`. Under `h`, stage it: size groups → partial hash (first 64 KiB + last 64 KiB) → full blake3 only for partial-hash collisions. Use blake3 `mmap` + `rayon` features (`update_mmap_rayon`) for large files.
- Hash cache key: (file ID, size, mtime) — not path. Moved/renamed file = cache hit; modified file = cache miss. Migrate the existing cache without losing entries where possible (else invalidate once and record why).
- Exclude zero-byte files, hardlink siblings, and cloud placeholders.
- Show per group whether copies are within one folder tree vs spread, and mark a suggested keeper (default: oldest mtime, tie-break shortest path) — suggestion only, never auto-select for deletion.
- Verify: fixture with true dups, same-size-different-content files, same-head-different-tail files, hardlinks, a moved file. Assert correct grouping and cache behavior.

## T6 — Browse additions
- Flat top-N largest files toggle (scope = current folder subtree).
- `/` live filter over the in-memory index (substring, case-insensitive); Esc clears.
- Regenerable tag (read-only marker + filter): `target/` only if sibling `Cargo.toml`; `node_modules/`; `.venv`/`venv` only if it contains `pyvenv.cfg`; `__pycache__`; `.gradle`. Do NOT tag ambiguous names like `build/` or `dist/`. Tagged items go through the existing trash/cleanup path (with T1b guard), no new delete route.
- Staleness lens (if time): use mtime only. Do not use last-access (unreliable on NTFS).

## T7 — USN journal incremental scan (needs admin; depends on T1–T3)
- Store journal ID + next USN per volume in each snapshot. On open: read journal records since stored USN, stat only changed entries, apply to the in-memory tree + changelog.
- Fallback to full walk when: not elevated, journal ID changed, USN out of range/wrapped, or any error. Never fail the app because of this path.
- Pre-register H5: after random create/modify/delete/rename operations in a test dir on an NTFS volume, incremental result equals a fresh full scan exactly (counts, sizes, tree). Record evidence. If not equal, STOP and leave it disabled behind a flag.

## T8 — BLOCKED: Cleanup session redesign
- DO NOT BUILD. Waiting on Tyler's notes from the Windows build.
- Record in DECISIONS.md as PROPOSED only: persistent staging list across folders, running "will free X" total (hardlink/placeholder aware), single batch confirm, restore via Recycle Bin.
- Keep the T1b guard on the existing cleanup path in the meantime.

## T9 — Hygiene (measure first; change only with evidence)
- Based on T0 numbers: arena with `u32` indices + interned names if per-node cost is high; dirty-flag recompute for sorts/aggregates instead of every tick; release profile `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip = true` — report binary size and scan time before/after; resolve duplicate dependency versions only if trivial.
- Each change must show before/after numbers in DECISIONS.md. No change without a measured gain.

## Definition of done (per task)
Hypotheses pre-registered and resolved with evidence; tests pass; clippy clean; DECISIONS.md updated; one local commit; nothing pushed; nothing permanently deleted by the app under any path.

## Final report (when stopping, for any reason)
Task status table (done / blocked / failed), each hypothesis result, open PROPOSED decisions needing Tyler's approval, and anything UNVERIFIED.