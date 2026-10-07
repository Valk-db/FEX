//! Duplicate detection.
//!
//! Strategy, cheapest first:
//! 1. Group files by size. A lone size can't be a duplicate, skip it.
//! 2. For size-groups with 2+ files, hash each file with blake3.
//! 3. Files sharing a hash are true duplicates.
//!
//! Hashing runs on a background thread and streams results over a channel,
//! so the UI stays responsive while gigabytes get hashed.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

/// Hash one file with blake3, streaming so huge files don't eat RAM.
pub fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// A set of files believed identical. `hash` is None while the group is
/// still only size-matched (hashing in progress).
#[derive(Debug, Clone)]
pub struct DupeGroup {
    pub hash: Option<String>,
    pub size: u64,
    pub files: Vec<PathBuf>,
}

/// Group (path, size) records by size, keeping only groups of 2+.
/// Zero-byte files are ignored: they're "duplicates" of nothing meaningful.
pub fn size_groups(files: &[(PathBuf, u64)]) -> Vec<DupeGroup> {
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    for (path, size) in files {
        if *size == 0 {
            continue;
        }
        by_size.entry(*size).or_default().push(path.clone());
    }
    let mut groups: Vec<DupeGroup> = by_size
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(size, files)| DupeGroup {
            hash: None,
            size,
            files,
        })
        .collect();
    // Biggest waste first.
    groups.sort_by_key(|a| std::cmp::Reverse(a.size * a.files.len() as u64));
    groups
}

/// Refine size-groups by hash. Files with a known hash are split into
/// true-duplicate subgroups; files still waiting on the background hasher
/// stay in a pending size-group. `known` maps path -> hash and should
/// already merge db-known hashes with this run's fresh results.
pub fn refine_by_hash(groups: &[DupeGroup], known: &HashMap<PathBuf, String>) -> Vec<DupeGroup> {
    let mut out = Vec::new();
    for g in groups {
        let mut by_hash: HashMap<String, Vec<PathBuf>> = HashMap::new();
        let mut pending: Vec<PathBuf> = Vec::new();
        for f in &g.files {
            match known.get(f) {
                Some(h) => by_hash.entry(h.clone()).or_default().push(f.clone()),
                None => pending.push(f.clone()),
            }
        }
        for (hash, files) in by_hash {
            if files.len() > 1 {
                out.push(DupeGroup {
                    hash: Some(hash),
                    size: g.size,
                    files,
                });
            }
            // hashed but unique: proven not-a-duplicate, drop it
        }
        if pending.len() > 1 {
            out.push(DupeGroup {
                hash: None,
                size: g.size,
                files: pending,
            });
        }
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.size * a.files.len() as u64));
    out
}

/// Spawn the background hasher. Sends (path, hex_hash) per file, then drops
/// the sender so the receiver sees EOF when done.
pub fn spawn_hasher(paths: Vec<PathBuf>) -> Receiver<(PathBuf, String)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for path in paths {
            match hash_file(&path) {
                Ok(h) => {
                    if tx.send((path, h)).is_err() {
                        break; // receiver gone, stop work
                    }
                }
                Err(_) => continue, // vanished or unreadable: skip
            }
        }
    });
    rx
}

/// Flatten groups into the file list the hasher should chew through,
/// skipping files whose hash we already know.
pub fn hash_candidates(groups: &[DupeGroup], known: &HashMap<PathBuf, String>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for g in groups {
        for f in &g.files {
            if !known.contains_key(f) {
                out.push(f.clone());
            }
        }
    }
    out
}
