// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ScaleNinja
// DriveSync (dsync) — https://github.com/scaleninja/drivesync

//! SQLite-backed index of the remote tree (`.gd/cache.db`), kept current via the Drive Changes API,
//! plus the stat-keyed cache of local file hashes.
//!
//! The connection sits behind a mutex so worker threads can record each completed upload or folder
//! creation immediately, which keeps a cancelled push resumable even if the Changes feed lags.
//! Subtree queries compare exact path prefixes (never `LIKE`), so they are case-sensitive.
use crate::drive::Drive;
use crate::sync::{join_rel, Entry};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

const TOKEN_KEY: &str = "start_page_token";
const UPDATED_KEY: &str = "updated_at";
const DEPTH_KEY: &str = "depth";
/// `path = ?1 OR path starts with ?1 + "/"` — exact, case-sensitive subtree match.
const SUBTREE: &str = "(path = ?1 OR substr(path, 1, length(?1) + 1) = ?1 || '/')";

pub struct Cache {
    conn: Mutex<Connection>,
}

impl Cache {
    /// Open (or create) the cache. WAL mode plus a busy timeout lets several CLI instances share it.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(15))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS files (
                 path TEXT PRIMARY KEY, id TEXT NOT NULL, mtime_ms INTEGER NOT NULL,
                 md5 TEXT, is_dir INTEGER NOT NULL, native_doc INTEGER NOT NULL, size INTEGER);
             CREATE INDEX IF NOT EXISTS files_id ON files(id);
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS local_hashes (
                 path TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime_ms INTEGER NOT NULL, md5 TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS pending_walks (
                 id TEXT PRIMARY KEY, path TEXT NOT NULL, depth INTEGER NOT NULL);",
        )?;
        // Databases created before the size column existed.
        let has_size = conn
            .prepare("SELECT 1 FROM pragma_table_info('files') WHERE name = 'size'")?
            .exists([])?;
        if !has_size {
            conn.execute("ALTER TABLE files ADD COLUMN size INTEGER", [])?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        meta(&self.conn(), key)
    }

    #[cfg(test)]
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        set_meta(&self.conn(), key, value)
    }

    pub fn count(&self) -> Result<usize> {
        Ok(self
            .conn()
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))?
            as usize)
    }

    pub fn updated_at(&self) -> Result<Option<String>> {
        self.meta(UPDATED_KEY)
    }

    /// Record one remote entry; safe to call from worker threads.
    pub fn upsert(&self, path: &str, e: &Entry) -> Result<()> {
        upsert(&self.conn(), path, e)
    }

    #[cfg(test)]
    pub fn remove(&self, path: &str) -> Result<()> {
        remove(&self.conn(), path)
    }

    #[cfg(test)]
    pub fn rename(&self, old: &str, new: &str) -> Result<()> {
        rename(&self.conn(), old, new)
    }

    #[cfg(test)]
    pub fn path_of_id(&self, id: &str) -> Result<Option<String>> {
        path_of_id(&self.conn(), id)
    }

    #[cfg(test)]
    pub fn id_at(&self, path: &str) -> Result<Option<String>> {
        id_at(&self.conn(), path)
    }

    /// Cached MD5 of a local file, valid only if its size and mtime are unchanged.
    pub fn local_hash(&self, path: &str, size: u64, mtime_ms: i64) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT md5 FROM local_hashes WHERE path = ?1 AND size = ?2 AND mtime_ms = ?3",
                params![path, size, mtime_ms],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_local_hash(&self, path: &str, size: u64, mtime_ms: i64, md5: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO local_hashes(path, size, mtime_ms, md5) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET size = excluded.size, mtime_ms = excluded.mtime_ms, md5 = excluded.md5",
            params![path, size, mtime_ms, md5],
        )?;
        Ok(())
    }

    /// Folders whose contents still need listing (recorded durably so a crash cannot lose them).
    pub fn pending_walks(&self) -> Result<Vec<(String, String, i32)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, path, depth FROM pending_walks ORDER BY path")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    #[cfg(test)]
    pub fn add_pending(&self, id: &str, path: &str, depth: i32) -> Result<()> {
        add_pending(&self.conn(), id, path, depth)
    }

    /// Entries at or below `prefix` ("" = whole tree), keyed by relative path.
    pub fn load(&self, prefix: &str) -> Result<BTreeMap<String, Entry>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("SELECT path, id, mtime_ms, md5, is_dir, native_doc, size FROM files WHERE ?1 = '' OR {SUBTREE}"))?;
        let rows = stmt.query_map(params![prefix], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Entry {
                    id: Some(r.get(1)?),
                    mtime_ms: r.get(2)?,
                    md5: r.get(3)?,
                    is_dir: r.get(4)?,
                    native_doc: r.get(5)?,
                    size: r.get::<_, Option<i64>>(6)?.map(|n| n as u64),
                    unreadable: false,
                },
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn replace_all(
        &self,
        entries: &BTreeMap<String, Entry>,
        token: &str,
        depth: i32,
    ) -> Result<()> {
        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM files", [])?;
        tx.execute("DELETE FROM pending_walks", [])?;
        for (p, e) in entries {
            upsert(&tx, p, e)?;
        }
        touch(&tx, token, depth)?;
        Ok(tx.commit()?)
    }
}

// Connection-level operations, shared by the public methods and by `apply_changes`, which holds the
// lock for its whole transaction.

fn meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?)
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute("INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, value])?;
    Ok(())
}

fn upsert(conn: &Connection, path: &str, e: &Entry) -> Result<()> {
    conn.execute(
        "INSERT INTO files(path, id, mtime_ms, md5, is_dir, native_doc, size) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(path) DO UPDATE SET id = excluded.id, mtime_ms = excluded.mtime_ms, md5 = excluded.md5,
             is_dir = excluded.is_dir, native_doc = excluded.native_doc, size = excluded.size",
        params![path, e.id.as_deref().unwrap_or(""), e.mtime_ms, e.md5, e.is_dir, e.native_doc, e.size.map(|n| n as i64)],
    )?;
    Ok(())
}

fn remove(conn: &Connection, path: &str) -> Result<()> {
    conn.execute(&format!("DELETE FROM files WHERE {SUBTREE}"), params![path])?;
    Ok(())
}

fn rename(conn: &Connection, old: &str, new: &str) -> Result<()> {
    remove(conn, new)?;
    conn.execute(
        &format!("UPDATE files SET path = ?2 || substr(path, length(?1) + 1) WHERE {SUBTREE}"),
        params![old, new],
    )?;
    Ok(())
}

/// Drop rows under `prefix` that lie deeper than `depth` levels from the root.
fn prune_deeper(conn: &Connection, prefix: &str, depth: i32) -> Result<()> {
    conn.execute(
        &format!("DELETE FROM files WHERE {SUBTREE} AND (length(path) - length(replace(path, '/', '')) + 1) > ?2"),
        params![prefix, depth],
    )?;
    Ok(())
}

fn path_of_id(conn: &Connection, id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT path FROM files WHERE id = ?1 LIMIT 1", [id], |r| {
            r.get(0)
        })
        .optional()?)
}

fn id_at(conn: &Connection, path: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT id FROM files WHERE path = ?1", [path], |r| r.get(0))
        .optional()?)
}

fn add_pending(conn: &Connection, id: &str, path: &str, depth: i32) -> Result<()> {
    conn.execute(
        "INSERT INTO pending_walks(id, path, depth) VALUES (?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET path = excluded.path, depth = excluded.depth",
        params![id, path, depth],
    )?;
    Ok(())
}

fn touch(conn: &Connection, token: &str, depth: i32) -> Result<()> {
    set_meta(conn, TOKEN_KEY, token)?;
    set_meta(conn, DEPTH_KEY, &depth.to_string())?;
    set_meta(
        conn,
        UPDATED_KEY,
        &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

/// Bring the cache up to date: incrementally via the Changes API when possible, otherwise a full listing.
pub fn refresh(drive: &Drive, cache: &Cache, root_id: &str, depth: i32, full: bool) -> Result<()> {
    let same_depth = cache.meta(DEPTH_KEY)?.as_deref() == Some(&depth.to_string());
    if let (false, true, Some(token)) = (full, same_depth, cache.meta(TOKEN_KEY)?) {
        let incremental = drain_pending(drive, cache)
            .and_then(|()| apply_changes(drive, cache, root_id, depth, &token));
        match incremental {
            Ok(()) => return Ok(()),
            Err(e) => crate::progress::eprintln(&format!("warning: incremental cache refresh failed ({e:#}); listing the remote tree in full")),
        }
    }
    // Take the token before listing so nothing that happens during the listing is missed.
    let token = drive.start_page_token()?;
    let all = drive.list_tree(root_id, depth)?;
    cache.replace_all(&all, &token, depth)
}

/// List every folder recorded in `pending_walks` (from this run or an interrupted earlier one) and
/// store its contents. Each folder is removed from the table in the same transaction as its rows.
fn drain_pending(drive: &Drive, cache: &Cache) -> Result<()> {
    for (id, path, remaining) in cache.pending_walks()? {
        let mut sub = BTreeMap::new();
        drive.walk(&id, &path, remaining, &mut sub)?;
        let conn = cache.conn();
        let tx = conn.unchecked_transaction()?;
        for (p, e) in &sub {
            upsert(&tx, p, e)?;
        }
        tx.execute("DELETE FROM pending_walks WHERE id = ?1", [&id])?;
        tx.commit()?;
    }
    Ok(())
}

fn apply_changes(
    drive: &Drive,
    cache: &Cache,
    root_id: &str,
    depth: i32,
    token: &str,
) -> Result<()> {
    let (changes, new_token) = drive.changes(token)?;
    {
        let conn = cache.conn();
        let tx = conn.unchecked_transaction()?;
        for ch in changes {
            let old_path = path_of_id(&tx, &ch.file_id)?;
            let drop_old = |conn: &Connection| -> Result<()> {
                old_path.as_deref().map_or(Ok(()), |p| remove(conn, p))
            };
            let Some(f) = ch.file.filter(|f| !ch.removed && !f.trashed) else {
                drop_old(&tx)?;
                continue;
            };
            // Locate the parent inside our tree; anything else has moved out of (or was never in) scope.
            let parent_path = match f.parents.first() {
                Some(p) if p == root_id => Some(String::new()),
                Some(p) => path_of_id(&tx, p)?,
                None => None,
            };
            let Some(parent_path) = parent_path else {
                drop_old(&tx)?;
                continue;
            };
            if !crate::sync::valid_name(&f.file.name) {
                crate::progress::eprintln(&format!(
                    "! skip     remote name {:?} cannot be a local path",
                    f.file.name
                ));
                drop_old(&tx)?;
                continue;
            }
            let new_path = join_rel(&parent_path, &f.file.name);
            let level = new_path.matches('/').count() as i32 + 1;
            if depth >= 0 && level > depth {
                drop_old(&tx)?;
                continue;
            }
            // Drive allows several items with one name in a folder; the index keeps the first one it
            // saw, so a change to a duplicate never silently swaps the identity behind a path.
            if old_path.is_none()
                && id_at(&tx, &new_path)?.is_some_and(|existing| existing != f.file.id)
            {
                continue;
            }
            let moved = old_path.as_deref().is_some_and(|old| old != new_path);
            if let Some(old) = old_path.as_deref().filter(|_| moved) {
                rename(&tx, old, &new_path)?;
            }
            let entry = f.file.to_entry();
            upsert(&tx, &new_path, &entry)?;
            if entry.is_dir {
                let remaining = if depth < 0 { -1 } else { depth - level };
                if old_path.is_none() {
                    add_pending(&tx, &f.file.id, &new_path, remaining)?;
                } else if moved && depth >= 0 {
                    // A folder moved to another level: children beyond the limit go, children that
                    // were beyond it before must be fetched.
                    prune_deeper(&tx, &new_path, depth)?;
                    add_pending(&tx, &f.file.id, &new_path, remaining)?;
                }
            }
        }
        tx.commit()?;
    }
    // Network calls happen with no transaction open, so other instances are not blocked. The token
    // is only advanced once everything is stored; a failure or crash here leaves the pending walks
    // on disk and the old token in place, so the next run finishes the job.
    drain_pending(drive, cache)?;
    let conn = cache.conn();
    touch(&conn, &new_token, depth)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: &str, is_dir: bool) -> Entry {
        Entry {
            id: Some(id.into()),
            mtime_ms: 1,
            size: (!is_dir).then_some(5),
            md5: None,
            is_dir,
            native_doc: false,
            unreadable: false,
        }
    }

    #[test]
    fn size_roundtrip_and_migration() {
        let c = Cache::open(Path::new(":memory:")).unwrap();
        c.upsert("f", &e("1", false)).unwrap();
        c.upsert("d", &e("2", true)).unwrap();
        let all = c.load("").unwrap();
        assert_eq!(all["f"].size, Some(5));
        assert_eq!(all["d"].size, None);
        // A pre-size schema is upgraded in place.
        let dir = std::env::temp_dir().join(format!("dsync_migrate_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE files (path TEXT PRIMARY KEY, id TEXT NOT NULL, mtime_ms INTEGER NOT NULL, md5 TEXT, is_dir INTEGER NOT NULL, native_doc INTEGER NOT NULL);
                 INSERT INTO files VALUES ('x', 'i', 1, NULL, 0, 0);",
            )
            .unwrap();
        }
        let c = Cache::open(&path).unwrap();
        assert_eq!(c.load("").unwrap()["x"].size, None);
        c.upsert("x", &e("i", false)).unwrap();
        assert_eq!(c.load("").unwrap()["x"].size, Some(5));
        drop(c);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn crud_prefix_rename() {
        let c = Cache::open(Path::new(":memory:")).unwrap();
        for (p, id, d) in [
            ("a", "1", true),
            ("a/x.txt", "2", false),
            ("a/b", "3", true),
            ("a/b/y.txt", "4", false),
            ("ab.txt", "5", false),
            ("a_c.txt", "6", false),
        ] {
            c.upsert(p, &e(id, d)).unwrap();
        }
        assert_eq!(c.count().unwrap(), 6);
        assert_eq!(
            c.load("a").unwrap().keys().cloned().collect::<Vec<_>>(),
            vec!["a", "a/b", "a/b/y.txt", "a/x.txt"]
        );
        assert_eq!(c.load("a_c.txt").unwrap().len(), 1);
        assert_eq!(c.load("").unwrap().len(), 6);
        assert_eq!(c.path_of_id("4").unwrap().as_deref(), Some("a/b/y.txt"));
        c.rename("a/b", "z").unwrap();
        assert_eq!(c.path_of_id("4").unwrap().as_deref(), Some("z/y.txt"));
        c.remove("a").unwrap();
        assert_eq!(
            c.load("").unwrap().keys().cloned().collect::<Vec<_>>(),
            vec!["a_c.txt", "ab.txt", "z", "z/y.txt"]
        );
        c.set_meta("k", "v").unwrap();
        c.set_meta("k", "w").unwrap();
        assert_eq!(c.meta("k").unwrap().as_deref(), Some("w"));
        assert_eq!(c.meta("missing").unwrap(), None);
    }

    #[test]
    fn subtree_queries_are_case_sensitive() {
        let c = Cache::open(Path::new(":memory:")).unwrap();
        for (p, id, d) in [
            ("a", "1", true),
            ("a/public.txt", "2", false),
            ("A", "3", true),
            ("A/secret.txt", "4", false),
            ("a%", "5", true),
            ("a%/x", "6", false),
        ] {
            c.upsert(p, &e(id, d)).unwrap();
        }
        assert_eq!(
            c.load("a").unwrap().keys().cloned().collect::<Vec<_>>(),
            vec!["a", "a/public.txt"]
        );
        assert_eq!(
            c.load("A").unwrap().keys().cloned().collect::<Vec<_>>(),
            vec!["A", "A/secret.txt"]
        );
        c.remove("a").unwrap();
        assert_eq!(
            c.load("").unwrap().keys().cloned().collect::<Vec<_>>(),
            vec!["A", "A/secret.txt", "a%", "a%/x"]
        );
        c.rename("A", "b").unwrap();
        assert_eq!(c.path_of_id("4").unwrap().as_deref(), Some("b/secret.txt"));
        assert_eq!(c.load("a%").unwrap().len(), 2);
    }

    #[test]
    fn special_characters_in_paths() {
        let c = Cache::open(Path::new(":memory:")).unwrap();
        let names = [
            "My Documents",
            "My Documents/it's 100%_done.md",
            "My Documents/sub folder",
            "My Documents/sub folder/deep file.txt",
            "My_Documents",
            "My Documentsx/other.txt",
            "back\\slash/f.txt",
            "ünïcödé 日本語/café.txt",
        ];
        for (i, n) in names.iter().enumerate() {
            c.upsert(n, &e(&i.to_string(), !n.contains('.'))).unwrap();
        }
        assert_eq!(
            c.load("My Documents")
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec![
                "My Documents",
                "My Documents/it's 100%_done.md",
                "My Documents/sub folder",
                "My Documents/sub folder/deep file.txt"
            ]
        );
        assert_eq!(c.load("My_Documents").unwrap().len(), 1);
        assert_eq!(c.load("back\\slash").unwrap().len(), 1);
        assert_eq!(c.load("ünïcödé 日本語").unwrap().len(), 1);
        c.rename("My Documents/sub folder", "Renamed Folder (v2)")
            .unwrap();
        assert_eq!(
            c.path_of_id("3").unwrap().as_deref(),
            Some("Renamed Folder (v2)/deep file.txt")
        );
        c.remove("My Documents").unwrap();
        assert_eq!(c.load("").unwrap().len(), 6);
    }

    #[test]
    fn pending_walks_persist_and_prune_respects_depth() {
        let c = Cache::open(Path::new(":memory:")).unwrap();
        c.add_pending("id1", "a/b", 2).unwrap();
        c.add_pending("id1", "a/c", 1).unwrap(); // same folder re-queued: latest wins
        c.add_pending("id2", "z", -1).unwrap();
        assert_eq!(
            c.pending_walks().unwrap(),
            vec![
                ("id1".to_string(), "a/c".to_string(), 1),
                ("id2".to_string(), "z".to_string(), -1)
            ]
        );
        for (p, id, d) in [
            ("m", "1", true),
            ("m/n", "2", true),
            ("m/n/deep.txt", "3", false),
            ("m/top.txt", "4", false),
            ("other/x/y", "5", false),
        ] {
            c.upsert(p, &e(id, d)).unwrap();
        }
        prune_deeper(&c.conn(), "m", 2).unwrap();
        assert_eq!(
            c.load("").unwrap().keys().cloned().collect::<Vec<_>>(),
            vec!["m", "m/n", "m/top.txt", "other/x/y"]
        );
        assert_eq!(c.id_at("m/n").unwrap().as_deref(), Some("2"));
        assert_eq!(c.id_at("nope").unwrap(), None);
    }

    #[test]
    fn concurrent_upserts_from_workers() {
        let dir = std::env::temp_dir().join(format!("dsync_cache_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = Cache::open(&dir.join("cache.db")).unwrap();
        std::thread::scope(|s| {
            for t in 0..8 {
                let c = &c;
                s.spawn(move || {
                    for i in 0..50 {
                        c.upsert(&format!("t{t}/f{i}"), &e(&format!("{t}-{i}"), false))
                            .unwrap();
                    }
                });
            }
        });
        assert_eq!(c.count().unwrap(), 400);
        drop(c);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
