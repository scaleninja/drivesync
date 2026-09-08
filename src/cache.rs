// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ScaleNinja
// DriveSync (dsync) — https://github.com/scaleninja/drivesync

//! SQLite-backed index of the remote tree (`.gd/cache.db`), kept current via the Drive Changes API,
//! plus the stat-keyed cache of local file hashes.
//!
//! The connection sits behind a mutex so worker threads can record each completed upload or folder
//! creation immediately, which keeps a cancelled push resumable even if the Changes feed lags.
//! Subtree queries compare exact path prefixes (never `LIKE`), so they are case-sensitive.
use crate::drive::{Change, Drive};
use crate::sync::{join_rel, Entry};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

const TOKEN_KEY: &str = "start_page_token";
const UPDATED_KEY: &str = "updated_at";
const DEPTH_KEY: &str = "depth";
/// Marker in `pending_walks.depth`: list the folder's direct children and add whatever the index
/// lacks (a duplicate that was suppressed while another entry owned its path), touching nothing
/// else. Any other value means a full walk of the folder to the configured depth.
const SHALLOW_WALK: i32 = -2;
/// `path = ?1 OR path starts with ?1 + "/"` — exact, case-sensitive subtree match.
const SUBTREE: &str = "(path = ?1 OR substr(path, 1, length(?1) + 1) = ?1 || '/')";

pub struct Cache {
    conn: Mutex<Connection>,
}

impl Cache {
    /// Open (or create) the cache. WAL mode plus a busy timeout lets several CLI instances share it.
    /// The file is created privately (0600): it holds the names of everything on the remote side.
    pub fn open(path: &Path) -> Result<Self> {
        if path.to_str() != Some(":memory:") && !path.exists() {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            match opts.open(path) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(60))?;
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
                 id TEXT PRIMARY KEY, path TEXT NOT NULL, depth INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS upload_sessions (
                 path TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime_ms INTEGER NOT NULL, uri TEXT NOT NULL);",
        )
        .with_context(|| format!("initializing {}", path.display()))?;
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

    /// Open the cache, rebuilding it from scratch if the file is damaged. The cache is only an
    /// index of the remote side and of local hashes, so losing it costs one full listing. Only
    /// call this while holding the exclusive workspace lock.
    pub fn open_or_rebuild(path: &Path) -> Result<Self> {
        match Self::open(path) {
            Ok(c) => Ok(c),
            Err(e) if is_corruption(&e) => {
                crate::progress::eprintln(&format!(
                    "warning: {} is damaged ({e:#}); rebuilding it",
                    path.display()
                ));
                for suffix in ["", "-wal", "-shm"] {
                    let mut p = path.as_os_str().to_owned();
                    p.push(suffix);
                    let _ = std::fs::remove_file(p);
                }
                Self::open(path)
            }
            Err(e) => Err(e),
        }
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

    /// Drop `path` and everything below it from the index (after a trash on Drive).
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

    /// Drive id of the indexed entry at `path`, if any.
    pub fn id_at(&self, path: &str) -> Result<Option<String>> {
        id_at(&self.conn(), path)
    }

    /// The indexed entry at exactly `path`, if any.
    pub fn entry(&self, path: &str) -> Result<Option<Entry>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, mtime_ms, md5, is_dir, native_doc, size FROM files WHERE path = ?1",
                [path],
                |r| {
                    Ok(Entry {
                        id: Some(r.get(0)?),
                        mtime_ms: r.get(1)?,
                        md5: r.get(2)?,
                        is_dir: r.get(3)?,
                        native_doc: r.get(4)?,
                        size: r.get::<_, Option<i64>>(5)?.map(|n| n as u64),
                        unreadable: false,
                    })
                },
            )
            .optional()?)
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

    /// Serialized resumable-upload context for a local file with matching stat. The original
    /// `uri` column is retained for compatibility; callers discard legacy URI-only records.
    pub fn upload_session(&self, path: &str, size: u64, mtime_ms: i64) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT uri FROM upload_sessions WHERE path = ?1 AND size = ?2 AND mtime_ms = ?3",
                params![path, size, mtime_ms],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_upload_session(
        &self,
        path: &str,
        size: u64,
        mtime_ms: i64,
        uri: &str,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO upload_sessions(path, size, mtime_ms, uri) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET size = excluded.size, mtime_ms = excluded.mtime_ms, uri = excluded.uri",
            params![path, size, mtime_ms, uri],
        )?;
        Ok(())
    }

    pub fn clear_upload_session(&self, path: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM upload_sessions WHERE path = ?1", [path])?;
        Ok(())
    }

    /// Forget cached hashes under `prefix` for files that no longer exist locally.
    pub fn prune_local_hashes(&self, prefix: &str, keep: &BTreeMap<String, Entry>) -> Result<()> {
        let conn = self.conn();
        let stale: Vec<String> = {
            let mut stmt = conn.prepare(&format!(
                "SELECT path FROM local_hashes WHERE ?1 = '' OR {SUBTREE}"
            ))?;
            let rows = stmt.query_map(params![prefix], |r| r.get::<_, String>(0))?;
            rows.filter_map(|r| r.ok())
                .filter(|p| !keep.contains_key(p))
                .collect()
        };
        let tx = conn.unchecked_transaction()?;
        for p in &stale {
            tx.execute("DELETE FROM local_hashes WHERE path = ?1", [p])?;
        }
        Ok(tx.commit()?)
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
    conn.execute(
        &format!("DELETE FROM pending_walks WHERE {SUBTREE}"),
        params![path],
    )?;
    Ok(())
}

fn rename(conn: &Connection, old: &str, new: &str) -> Result<()> {
    remove(conn, new)?;
    conn.execute(
        &format!("UPDATE files SET path = ?2 || substr(path, length(?1) + 1) WHERE {SUBTREE}"),
        params![old, new],
    )?;
    conn.execute(
        &format!(
            "UPDATE pending_walks SET path = ?2 || substr(path, length(?1) + 1) WHERE {SUBTREE}"
        ),
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
    drive.check_folder(root_id)?;
    let same_depth = cache.meta(DEPTH_KEY)?.as_deref() == Some(&depth.to_string());
    if let (false, true, Some(token)) = (full, same_depth, cache.meta(TOKEN_KEY)?) {
        let incremental = apply_changes(drive, cache, root_id, depth, &token);
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
fn drain_pending(drive: &Drive, cache: &Cache, root_id: &str, depth: i32) -> Result<()> {
    // Each task is resolved against the current index when it runs (tasks may predate a rename,
    // or come from an older version), and the queue is re-read after every task because a full
    // walk also completes any nested tasks.
    while let Some((id, _, kind)) = cache.pending_walks()?.into_iter().next() {
        let path = if id == root_id {
            Some(String::new())
        } else {
            path_of_id(&cache.conn(), &id)?
        };
        let Some(path) = path else {
            cache
                .conn()
                .execute("DELETE FROM pending_walks WHERE id = ?1", [&id])?;
            continue;
        };
        let level = if path.is_empty() {
            0
        } else {
            path.matches('/').count() as i32 + 1
        };
        let remaining = if depth < 0 {
            -1
        } else {
            (depth - level).max(0)
        };
        if kind == SHALLOW_WALK {
            reconcile_children(drive, cache, &id, &path, depth, remaining)?;
            continue;
        }
        let mut sub = BTreeMap::new();
        drive.walk(&id, &path, remaining, &mut sub)?;
        let conn = cache.conn();
        let tx = conn.unchecked_transaction()?;
        // Replace, rather than append, so removed entries and former duplicate owners disappear.
        tx.execute(
            "DELETE FROM files WHERE ?1 = '' OR (path != ?1 AND substr(path, 1, length(?1) + 1) = ?1 || '/')",
            [&path],
        )?;
        tx.execute(
            &format!("DELETE FROM pending_walks WHERE ?1 = '' OR {SUBTREE}"),
            [&path],
        )?;
        for (p, e) in &sub {
            upsert(&tx, p, e)?;
        }
        tx.execute("DELETE FROM pending_walks WHERE id = ?1", [&id])?;
        tx.commit()?;
    }
    Ok(())
}

/// One listing of a folder's direct children: entries the index lacks are added (first of any
/// duplicate name wins, as everywhere), and a newly found folder is queued for a full walk.
/// Nothing is removed; removals arrive through the Changes feed.
fn reconcile_children(
    drive: &Drive,
    cache: &Cache,
    id: &str,
    path: &str,
    depth: i32,
    remaining: i32,
) -> Result<()> {
    let children = if remaining == 0 {
        Vec::new() // the folder sits at the depth limit; its children are out of scope
    } else {
        drive.list_children(id)?
    };
    let conn = cache.conn();
    let tx = conn.unchecked_transaction()?;
    for f in children {
        if !crate::sync::valid_name(&f.name) {
            continue;
        }
        let rel = join_rel(path, &f.name);
        if id_at(&tx, &rel)?.is_some() {
            continue;
        }
        let entry = f.to_entry();
        upsert(&tx, &rel, &entry)?;
        if entry.is_dir {
            let level = rel.matches('/').count() as i32 + 1;
            add_pending(&tx, &f.id, &rel, if depth < 0 { -1 } else { depth - level })?;
        }
    }
    tx.execute("DELETE FROM pending_walks WHERE id = ?1", [id])?;
    tx.commit()?;
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
            apply_change(&tx, ch, root_id, depth)?;
        }
        tx.commit()?;
    }
    // Network calls happen with no transaction open, so other instances are not blocked. The token
    // is only advanced once everything is stored; a failure or crash here leaves the pending walks
    // on disk and the old token in place, so the next run finishes the job.
    drain_pending(drive, cache, root_id, depth)?;
    let conn = cache.conn();
    touch(&conn, &new_token, depth)
}

/// Apply one record from the Changes feed to the index (no network).
///
/// Rules: a removed, trashed or out-of-scope file leaves the index together with its subtree; a
/// file is located through whichever of its parents is inside the tree; the first entry seen at a
/// path keeps it, so neither a new duplicate nor a rename onto an occupied path can swap the
/// identity behind a path; a newly visible folder is queued for listing.
fn apply_change(tx: &Connection, ch: Change, root_id: &str, depth: i32) -> Result<()> {
    let Some(file_id) = ch.file_id.as_deref() else {
        return Ok(()); // a change to a shared drive, not to a file
    };
    let old_path = path_of_id(tx, file_id)?;
    let drop_old = |conn: &Connection| -> Result<()> {
        if let Some(p) = old_path.as_deref() {
            remove(conn, p)?;
            queue_parent(conn, p, root_id)?;
        }
        Ok(())
    };
    let Some(f) = ch.file.filter(|f| !ch.removed && !f.trashed) else {
        return drop_old(tx);
    };
    // Locate a parent inside our tree; a file with none has moved out of (or was never in) scope.
    let mut parent_path = None;
    for p in &f.parents {
        parent_path = if p == root_id {
            Some(String::new())
        } else {
            path_of_id(tx, p)?
        };
        if parent_path.is_some() {
            break;
        }
    }
    let Some(parent_path) = parent_path else {
        return drop_old(tx);
    };
    if !crate::sync::valid_name(&f.file.name) {
        crate::progress::eprintln(&format!(
            "! skip     remote name {:?} cannot be a local path",
            f.file.name
        ));
        return drop_old(tx);
    }
    let new_path = join_rel(&parent_path, &f.file.name);
    let level = new_path.matches('/').count() as i32 + 1;
    if depth >= 0 && level > depth {
        return drop_old(tx);
    }
    let moved = old_path.as_deref().is_some_and(|old| old != new_path);
    if (old_path.is_none() || moved)
        && id_at(tx, &new_path)?.is_some_and(|existing| existing != f.file.id)
    {
        // Another entry already owns this path; the newcomer is not indexed (and if it was
        // indexed elsewhere, it leaves, since it no longer lives there).
        return drop_old(tx);
    }
    if let Some(old) = old_path.as_deref().filter(|_| moved) {
        rename(tx, old, &new_path)?;
        queue_parent(tx, old, root_id)?;
    }
    let entry = f.file.to_entry();
    upsert(tx, &new_path, &entry)?;
    if entry.is_dir {
        let remaining = if depth < 0 { -1 } else { depth - level };
        if old_path.is_none() {
            add_pending(tx, &f.file.id, &new_path, remaining)?;
        } else if moved && depth >= 0 {
            // A folder moved to another level: children beyond the limit go, children that
            // were beyond it before must be fetched.
            prune_deeper(tx, &new_path, depth)?;
            add_pending(tx, &f.file.id, &new_path, remaining)?;
        }
    }
    Ok(())
}

/// A suppressed duplicate may still occupy a vacated path: queue a shallow listing of the parent.
/// An existing full-walk task for that folder is never downgraded.
fn queue_parent(conn: &Connection, path: &str, root_id: &str) -> Result<()> {
    let parent = crate::sync::parent_of(path);
    let id = if parent.is_empty() {
        Some(root_id.to_string())
    } else {
        id_at(conn, parent)?
    };
    if let Some(id) = id {
        conn.execute(
            "INSERT INTO pending_walks(id, path, depth) VALUES (?1, ?2, ?3) ON CONFLICT(id) DO NOTHING",
            params![id, parent, SHALLOW_WALK],
        )?;
    }
    Ok(())
}

/// Whether an open/init error means the database file itself is unusable.
fn is_corruption(e: &anyhow::Error) -> bool {
    use rusqlite::ErrorCode::*;
    e.chain().any(|c| {
        c.downcast_ref::<rusqlite::Error>().is_some_and(|e| {
            matches!(
                e,
                rusqlite::Error::SqliteFailure(f, _)
                    if matches!(f.code, NotADatabase | DatabaseCorrupt)
            )
        })
    })
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
    fn shallow_reconcile_never_downgrades_a_full_walk() {
        let c = Cache::open(Path::new(":memory:")).unwrap();
        c.upsert("d", &e("D", true)).unwrap();
        c.upsert("d/f.txt", &e("F", false)).unwrap();
        c.add_pending("D", "d", -1).unwrap(); // a full walk is already queued for d
        queue_parent(&c.conn(), "d/f.txt", "ROOT").unwrap();
        assert_eq!(
            c.pending_walks().unwrap(),
            vec![("D".into(), "d".into(), -1)]
        );
        // With nothing queued, a vacated path queues a shallow listing of its parent.
        c.upsert("top.txt", &e("T", false)).unwrap();
        queue_parent(&c.conn(), "top.txt", "ROOT").unwrap();
        assert!(c
            .pending_walks()
            .unwrap()
            .contains(&("ROOT".into(), "".into(), SHALLOW_WALK)));
        // A later full walk for the same folder replaces the shallow one.
        c.add_pending("ROOT", "", -1).unwrap();
        assert!(c
            .pending_walks()
            .unwrap()
            .contains(&("ROOT".into(), "".into(), -1)));
    }

    #[test]
    fn pending_descendants_follow_rename_and_are_cancelled_on_removal() {
        let c = Cache::open(Path::new(":memory:")).unwrap();
        c.upsert("a", &e("A", true)).unwrap();
        c.upsert("a/b", &e("B", true)).unwrap();
        c.add_pending("B", "a/b", -1).unwrap();
        c.rename("a", "z").unwrap();
        assert_eq!(
            c.pending_walks().unwrap(),
            vec![("B".into(), "z/b".into(), -1)]
        );
        c.remove("z").unwrap();
        assert!(c.pending_walks().unwrap().is_empty());
    }

    #[test]
    fn local_hash_pruning_is_scoped() {
        let c = Cache::open(Path::new(":memory:")).unwrap();
        for p in ["a/keep.txt", "a/gone.txt", "b/other.txt", "A/case.txt"] {
            c.set_local_hash(p, 1, 1, "h").unwrap();
        }
        let keep: BTreeMap<String, Entry> = [("a/keep.txt".to_string(), Entry::default())]
            .into_iter()
            .collect();
        c.prune_local_hashes("a", &keep).unwrap();
        assert_eq!(
            c.local_hash("a/keep.txt", 1, 1).unwrap().as_deref(),
            Some("h")
        );
        assert_eq!(c.local_hash("a/gone.txt", 1, 1).unwrap(), None);
        assert_eq!(
            c.local_hash("b/other.txt", 1, 1).unwrap().as_deref(),
            Some("h"),
            "outside the prefix is untouched"
        );
        assert_eq!(
            c.local_hash("A/case.txt", 1, 1).unwrap().as_deref(),
            Some("h"),
            "prefix is case-sensitive"
        );
    }

    fn change(id: &str, name: &str, parents: &[&str], folder: bool, trashed: bool) -> Change {
        use crate::drive::{ChangedFile, File, FOLDER_MIME};
        Change {
            file_id: Some(id.into()),
            removed: false,
            file: Some(ChangedFile {
                file: File {
                    id: id.into(),
                    name: name.into(),
                    mime_type: if folder {
                        FOLDER_MIME.into()
                    } else {
                        "text/plain".into()
                    },
                    modified_time: Some("2026-09-08T00:00:00.000Z".into()),
                    md5_checksum: (!folder).then(|| "m".into()),
                    size: (!folder).then(|| "1".into()),
                },
                parents: parents.iter().map(|p| p.to_string()).collect(),
                trashed,
            }),
        }
    }

    #[test]
    fn changes_keep_first_owner_and_locate_by_any_parent_in_tree() {
        let c = Cache::open(Path::new(":memory:")).unwrap();
        for (p, id, d) in [
            ("a", "A", true),
            ("a/x.txt", "X", false),
            ("b", "B", true),
            ("b/y.txt", "Y", false),
        ] {
            c.upsert(p, &e(id, d)).unwrap();
        }
        let conn = c.conn();
        // A record without a file id (a shared-drive change) is ignored.
        let none = Change {
            file_id: None,
            removed: false,
            file: None,
        };
        apply_change(&conn, none, "ROOT", -1).unwrap();
        // Legacy multi-parent file: the parent inside the tree is the one that counts.
        apply_change(
            &conn,
            change("Z", "z.txt", &["OUTSIDE", "B"], false, false),
            "ROOT",
            -1,
        )
        .unwrap();
        assert_eq!(path_of_id(&conn, "Z").unwrap().as_deref(), Some("b/z.txt"));
        // Y moved and renamed onto a/x.txt, which X already owns: X keeps the path, Y leaves.
        apply_change(
            &conn,
            change("Y", "x.txt", &["A"], false, false),
            "ROOT",
            -1,
        )
        .unwrap();
        assert_eq!(id_at(&conn, "a/x.txt").unwrap().as_deref(), Some("X"));
        assert_eq!(path_of_id(&conn, "Y").unwrap(), None);
        // A brand-new duplicate of an indexed name is not indexed either.
        apply_change(
            &conn,
            change("X2", "x.txt", &["A"], false, false),
            "ROOT",
            -1,
        )
        .unwrap();
        assert_eq!(id_at(&conn, "a/x.txt").unwrap().as_deref(), Some("X"));
        // Content change in place keeps the identity and updates the metadata.
        apply_change(
            &conn,
            change("X", "x.txt", &["A"], false, false),
            "ROOT",
            -1,
        )
        .unwrap();
        assert_eq!(id_at(&conn, "a/x.txt").unwrap().as_deref(), Some("X"));
        // A file moved out of the tree, and a trashed folder, leave with their subtrees.
        apply_change(
            &conn,
            change("Z", "z.txt", &["OUTSIDE"], false, false),
            "ROOT",
            -1,
        )
        .unwrap();
        assert_eq!(path_of_id(&conn, "Z").unwrap(), None);
        apply_change(&conn, change("A", "a", &["ROOT"], true, true), "ROOT", -1).unwrap();
        assert_eq!(path_of_id(&conn, "X").unwrap(), None);
        assert_eq!(path_of_id(&conn, "A").unwrap(), None);
        // A newly visible folder is queued for listing with the remaining depth; one beyond the
        // depth limit is ignored.
        apply_change(&conn, change("N", "n", &["ROOT"], true, false), "ROOT", 2).unwrap();
        apply_change(
            &conn,
            change("DEEP", "deep", &["N"], true, false),
            "ROOT",
            2,
        )
        .unwrap();
        apply_change(
            &conn,
            change("TOODEEP", "f.txt", &["DEEP"], false, false),
            "ROOT",
            2,
        )
        .unwrap();
        assert_eq!(id_at(&conn, "n/deep").unwrap().as_deref(), Some("DEEP"));
        assert_eq!(id_at(&conn, "n/deep/f.txt").unwrap(), None);
        drop(conn);
        assert_eq!(
            c.pending_walks().unwrap(),
            vec![
                ("ROOT".into(), "".into(), SHALLOW_WALK),
                ("B".into(), "b".into(), SHALLOW_WALK),
                ("N".into(), "n".into(), 1),
                ("DEEP".into(), "n/deep".into(), 0)
            ]
        );
    }

    #[test]
    fn damaged_cache_is_rebuilt_and_sessions_and_entries_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dsync_rebuild_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cache.db");
        std::fs::write(&path, b"this is not a database").unwrap();
        assert!(Cache::open(&path).is_err());
        let c = Cache::open_or_rebuild(&path).unwrap();
        assert_eq!(c.count().unwrap(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        c.upsert("d", &e("D", true)).unwrap();
        c.upsert("d/f", &e("F", false)).unwrap();
        assert!(c.entry("d").unwrap().unwrap().is_dir);
        assert_eq!(c.entry("d/f").unwrap().unwrap().id.as_deref(), Some("F"));
        assert!(c.entry("nope").unwrap().is_none());
        // Sessions are valid only for the exact stat they were recorded under.
        c.set_upload_session("big.bin", 10, 20, "https://session")
            .unwrap();
        assert_eq!(
            c.upload_session("big.bin", 10, 20).unwrap().as_deref(),
            Some("https://session")
        );
        assert_eq!(c.upload_session("big.bin", 11, 20).unwrap(), None);
        c.clear_upload_session("big.bin").unwrap();
        assert_eq!(c.upload_session("big.bin", 10, 20).unwrap(), None);
        drop(c);
        std::fs::remove_dir_all(&dir).unwrap();
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
