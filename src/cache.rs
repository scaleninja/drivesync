// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ScaleNinja
// DriveSync (dsync) — https://github.com/scaleninja/drivesync

//! SQLite-backed index of the remote tree (`.gd/cache.db`), kept current via the Drive Changes API.
use crate::drive::Drive;
use crate::sync::{join_rel, Entry};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeMap;
use std::path::Path;

const TOKEN_KEY: &str = "start_page_token";
const UPDATED_KEY: &str = "updated_at";
const DEPTH_KEY: &str = "depth";

pub struct Cache {
    conn: Connection,
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
                 md5 TEXT, is_dir INTEGER NOT NULL, native_doc INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS files_id ON files(id);
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;
        Ok(Self { conn })
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, value])?;
        Ok(())
    }

    pub fn count(&self) -> Result<usize> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))?
            as usize)
    }

    pub fn updated_at(&self) -> Result<Option<String>> {
        self.meta(UPDATED_KEY)
    }

    pub fn upsert(&self, path: &str, e: &Entry) -> Result<()> {
        self.conn.execute(
            "INSERT INTO files(path, id, mtime_ms, md5, is_dir, native_doc) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET id = excluded.id, mtime_ms = excluded.mtime_ms, md5 = excluded.md5,
                 is_dir = excluded.is_dir, native_doc = excluded.native_doc",
            params![path, e.id.as_deref().unwrap_or(""), e.mtime_ms, e.md5, e.is_dir, e.native_doc],
        )?;
        Ok(())
    }

    pub fn upsert_all(&self, entries: &BTreeMap<String, Entry>) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (p, e) in entries {
            self.upsert(p, e)?;
        }
        Ok(tx.commit()?)
    }

    /// Remove `path` and everything below it.
    pub fn remove(&self, path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM files WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'",
            params![path, format!("{}/%", like_escape(path))],
        )?;
        Ok(())
    }

    /// Move `old` (and its subtree) to `new`.
    pub fn rename(&self, old: &str, new: &str) -> Result<()> {
        self.remove(new)?;
        self.conn.execute(
            "UPDATE files SET path = ?2 || substr(path, length(?1) + 1) WHERE path = ?1 OR path LIKE ?3 ESCAPE '\\'",
            params![old, new, format!("{}/%", like_escape(old))],
        )?;
        Ok(())
    }

    pub fn path_of_id(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT path FROM files WHERE id = ?1 LIMIT 1", [id], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Entries at or below `prefix` ("" = whole tree), keyed by relative path.
    pub fn load(&self, prefix: &str) -> Result<BTreeMap<String, Entry>> {
        let mut stmt = self.conn.prepare("SELECT path, id, mtime_ms, md5, is_dir, native_doc FROM files WHERE ?1 = '' OR path = ?1 OR path LIKE ?2 ESCAPE '\\'")?;
        let rows = stmt.query_map(params![prefix, format!("{}/%", like_escape(prefix))], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Entry {
                    id: Some(r.get(1)?),
                    mtime_ms: r.get(2)?,
                    md5: r.get(3)?,
                    is_dir: r.get(4)?,
                    native_doc: r.get(5)?,
                },
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn replace_all(&self, entries: &BTreeMap<String, Entry>) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        self.conn.execute("DELETE FROM files", [])?;
        for (p, e) in entries {
            self.upsert(p, e)?;
        }
        Ok(tx.commit()?)
    }

    fn touch(&self, token: &str, depth: i32) -> Result<()> {
        self.set_meta(TOKEN_KEY, token)?;
        self.set_meta(DEPTH_KEY, &depth.to_string())?;
        self.set_meta(
            UPDATED_KEY,
            &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
    }
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Bring the cache up to date: incrementally via the Changes API when possible, otherwise a full listing.
pub fn refresh(drive: &Drive, cache: &Cache, root_id: &str, depth: i32, full: bool) -> Result<()> {
    let same_depth = cache.meta(DEPTH_KEY)?.as_deref() == Some(&depth.to_string());
    if let (false, true, Some(token)) = (full, same_depth, cache.meta(TOKEN_KEY)?) {
        match apply_changes(drive, cache, root_id, depth, &token) {
            Ok(()) => return Ok(()),
            Err(e) => crate::progress::eprintln(&format!("warning: incremental cache refresh failed ({e:#}); listing the remote tree in full")),
        }
    }
    // Take the token before walking so nothing that happens during the walk is missed.
    let token = drive.start_page_token()?;
    let mut all = BTreeMap::new();
    drive.walk(root_id, "", depth, &mut all)?;
    cache.replace_all(&all)?;
    cache.touch(&token, depth)
}

fn apply_changes(
    drive: &Drive,
    cache: &Cache,
    root_id: &str,
    depth: i32,
    token: &str,
) -> Result<()> {
    let (changes, new_token) = drive.changes(token)?;
    let tx = cache.conn.unchecked_transaction()?;
    for ch in changes {
        let old_path = cache.path_of_id(&ch.file_id)?;
        let drop_old = |cache: &Cache| -> Result<()> {
            old_path.as_deref().map_or(Ok(()), |p| cache.remove(p))
        };
        let Some(f) = ch.file.filter(|f| !ch.removed && !f.trashed) else {
            drop_old(cache)?;
            continue;
        };
        // Locate the parent inside our tree; anything else has moved out of (or was never in) scope.
        let parent_path = match f.parents.first() {
            Some(p) if p == root_id => Some(String::new()),
            Some(p) => cache.path_of_id(p)?,
            None => None,
        };
        let Some(parent_path) = parent_path else {
            drop_old(cache)?;
            continue;
        };
        let new_path = join_rel(&parent_path, &f.file.name);
        let level = new_path.matches('/').count() as i32 + 1;
        if depth >= 0 && level > depth {
            drop_old(cache)?;
            continue;
        }
        if let Some(old) = old_path.as_deref().filter(|old| *old != new_path) {
            cache.rename(old, &new_path)?;
        }
        let entry = f.file.to_entry();
        let new_folder = entry.is_dir && old_path.is_none();
        cache.upsert(&new_path, &entry)?;
        if new_folder {
            let mut sub = BTreeMap::new();
            drive.walk(
                &f.file.id,
                &new_path,
                if depth < 0 { -1 } else { depth - level },
                &mut sub,
            )?;
            for (p, e) in &sub {
                cache.upsert(p, e)?;
            }
        }
    }
    cache.touch(&new_token, depth)?;
    Ok(tx.commit()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: &str, is_dir: bool) -> Entry {
        Entry {
            id: Some(id.into()),
            mtime_ms: 1,
            md5: None,
            is_dir,
            native_doc: false,
        }
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
}
