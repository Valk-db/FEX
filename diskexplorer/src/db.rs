//! Snapshot database: every scan is persisted to sqlite so later scans
//! can be diffed (growth tracking) and hashes survive between runs
//! (duplicate tracking without re-hashing everything).
//!
//! Location: `$HOME/.local/share/diskexplorer/scans.db` (XDG-style).

use rusqlite::{Connection, Result as SqlResult, params};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct FileRec {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub hash: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ScanMeta {
    pub id: i64,
    pub root: String,
    pub started_at: i64,
}

pub struct SnapshotDb {
    conn: Connection,
}

fn db_path() -> PathBuf {
    // HOME on unix, USERPROFILE on Windows, cwd as last resort.
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    let mut p = PathBuf::from(home);
    p.push(".local/share/diskexplorer/scans.db");
    p
}

impl SnapshotDb {
    pub fn open() -> SqlResult<Self> {
        let path = db_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS scans (
                 id INTEGER PRIMARY KEY,
                 root TEXT NOT NULL,
                 started_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS files (
                 scan_id INTEGER NOT NULL,
                 path TEXT NOT NULL,
                 size INTEGER NOT NULL,
                 mtime INTEGER NOT NULL,
                 hash TEXT,
                 PRIMARY KEY (scan_id, path)
             );
             CREATE TABLE IF NOT EXISTS dirs (
                 scan_id INTEGER NOT NULL,
                 path TEXT NOT NULL,
                 size INTEGER NOT NULL,
                 PRIMARY KEY (scan_id, path)
             );
             CREATE INDEX IF NOT EXISTS idx_scans_root ON scans(root, started_at);",
        )?;
        Ok(SnapshotDb { conn })
    }

    pub fn now_unix() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Persist a scan. `files` is (path, size, mtime), `dirs` is (path, size).
    /// Returns the new scan id.
    pub fn save_scan(
        &mut self,
        root: &str,
        files: &[(String, u64, i64)],
        dirs: &[(String, u64)],
    ) -> SqlResult<i64> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO scans (root, started_at) VALUES (?1, ?2)",
            params![root, Self::now_unix()],
        )?;
        let scan_id = tx.last_insert_rowid();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO files (scan_id, path, size, mtime) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (path, size, mtime) in files {
                stmt.execute(params![scan_id, path, *size as i64, mtime])?;
            }
        }
        {
            let mut stmt =
                tx.prepare("INSERT INTO dirs (scan_id, path, size) VALUES (?1, ?2, ?3)")?;
            for (path, size) in dirs {
                stmt.execute(params![scan_id, path, *size as i64])?;
            }
        }
        tx.commit()?;
        Ok(scan_id)
    }

    /// The most recent scan for this root, whatever it is. Used to paint
    /// the UI instantly from the snapshot instead of opening blank.
    pub fn latest_scan(&self, root: &str) -> SqlResult<Option<ScanMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, root, started_at FROM scans WHERE root = ?1 ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![root], |r| {
            Ok(ScanMeta {
                id: r.get(0)?,
                root: r.get(1)?,
                started_at: r.get(2)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn dirs_of(&self, scan_id: i64) -> SqlResult<Vec<(String, u64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, size FROM dirs WHERE scan_id = ?1")?;
        let rows = stmt.query_map(params![scan_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })?;
        rows.collect()
    }

    /// The most recent scan for this root before `before_id` (i.e. the one
    /// to diff against). None on first ever scan.
    pub fn previous_scan(&self, root: &str, before_id: i64) -> SqlResult<Option<ScanMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, root, started_at FROM scans
             WHERE root = ?1 AND id < ?2 ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![root, before_id], |r| {
            Ok(ScanMeta {
                id: r.get(0)?,
                root: r.get(1)?,
                started_at: r.get(2)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn files_of(&self, scan_id: i64) -> SqlResult<Vec<FileRec>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, size, mtime, hash FROM files WHERE scan_id = ?1")?;
        let rows = stmt.query_map(params![scan_id], |r| {
            Ok(FileRec {
                path: r.get(0)?,
                size: r.get::<_, i64>(1)? as u64,
                mtime: r.get(2)?,
                hash: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// Record computed hashes so future runs skip re-hashing unchanged files.
    /// One transaction for the whole batch: a single fsync instead of one
    /// per file. Only writes when size+mtime still match (untouched file).
    pub fn update_hashes(
        &mut self,
        scan_id: i64,
        items: &[(String, u64, i64, String)],
    ) -> SqlResult<()> {
        if items.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE files SET hash = ?1
                 WHERE scan_id = ?2 AND path = ?3 AND size = ?4 AND mtime = ?5",
            )?;
            for (path, size, mtime, hash) in items {
                stmt.execute(params![hash, scan_id, path, *size as i64, mtime])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Known hashes from older scans, keyed by (path, size, mtime): lets a
    /// fresh scan reuse hashes for files that haven't changed.
    pub fn known_hashes(
        &self,
        root: &str,
    ) -> SqlResult<std::collections::HashMap<(String, u64, i64), String>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.path, f.size, f.mtime, f.hash FROM files f
             JOIN scans s ON s.id = f.scan_id
             WHERE s.root = ?1 AND f.hash IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![root], |r| {
            let path: String = r.get(0)?;
            let size = r.get::<_, i64>(1)? as u64;
            let mtime: i64 = r.get(2)?;
            let hash: String = r.get(3)?;
            Ok(((path, size, mtime), hash))
        })?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (k, v) = row?;
            // latest scan wins: rows come in scan-id order only if we sort;
            // simpler to let later inserts overwrite by re-querying per scan.
            map.insert(k, v);
        }
        Ok(map)
    }
}
