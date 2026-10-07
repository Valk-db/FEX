# diskexplorer

A terminal file explorer that shows how much storage every file and folder
holds, so cleaning and organizing storage is easy. Tyler's first Rust tool.

v2 adds duplicate detection, growth tracking across scans, and scoped
cleanup sessions. Deletes go to the OS trash, never permanent deletion.

## Stack

- `ratatui` 0.30 (TUI widgets/layout, the current Rust standard)
- `crossterm` 0.29 (terminal backend: raw mode, key events)
- `jwalk` 0.8 (parallel directory walking for fast size scans)
- `rusqlite` 0.40, bundled (snapshot db at `~/.local/share/diskexplorer/scans.db`)
- `blake3` 1.8 (duplicate hashing, background thread)
- `trash` 5.2 (safe deletion via OS trash)

## Run

```sh
cargo run                 # TUI at current directory
cargo run -- ~/workspace  # TUI at a path
cargo run -- --scan-only ~/workspace   # headless: print 20 largest dirs
cargo test                # unit tests (growth, dupes, db roundtrip)
```

## Keys

| Key | Action |
| --- | ------ |
| `1` `2` `3` | Browse / Duplicates / Growth views |
| `c` | start cleanup session scoped to current folder |
| `↑` `↓` / `j` `k` | move selection |
| `Enter` / `l` | open folder (browse) |
| `⌫` / `h` | go up (browse) |
| `t` | toggle top strip: breadcrumb path <-> set of subfolders |
| `r` | rescan from the root |
| `q` / `Esc` | quit (Esc ends session first) |

In a cleanup session:

| Key | Action |
| --- | ------ |
| `d` | trash current candidate (asks y/n first) |
| `s` | skip candidate |
| `Esc` | end session (auto-rescans to reconcile) |

## How it works

1. On open, the last persisted snapshot loads instantly, so the UI
   paints real data in milliseconds. A background thread then walks
   the tree as a diff: only new, changed, or deleted files (by
   size+mtime) stream to the UI, applied as deltas to folder sizes.
   The loop wakes every 150ms, so the scan visibly updates live with
   no keypress needed.
2. `SnapshotDb` persists each finished scan. The next scan diffs against
   the previous one: new, grown, and deleted files become the Growth view.
3. Duplicate detection groups files by size, then hashes candidates with
   blake3 on a background thread. Hashing is deliberately procrastinated:
   it becomes eligible 10s after the scan and only starts after 5s of
   keyboard idle, so it never competes with you. Hashes are stored in
   the db, so unchanged files are never re-hashed.
4. `c` builds a cleanup session: duplicates (all but one copy) plus
   new/grown files inside the current folder, biggest first. `d`
   moves to OS trash with confirmation.

## Windows notes

- The UI appears immediately; on a full `C:\` scan the sizes stream in
  over a minute or two rather than blocking up front.
- Windows Defender's real-time protection slows file enumeration a lot.
  Adding the exe to Defender's exclusions (Virus & threat protection,
  Manage settings, Exclusions) can speed up scans several-fold.

## Ideas for next steps

- Stray file-type detection (a `.psd` lost in Downloads)
- Same-date clusters (one forgotten event, decide its fate at once)
- Miller-column preview pane
- Filter/search box
