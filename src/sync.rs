// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ScaleNinja
// DriveSync (dsync) — https://github.com/scaleninja/drivesync

//! Local walking, local/remote comparison, plan listing, and the push/pull engines.
//!
//! Comparison: equal MD5 means identical (Drive supplies remote MD5s; local ones are computed and
//! cached by stat); differing sizes mean different; only when neither settles it do mtimes decide.
//!
//! Safety rule: the plan is made from a snapshot, so the executor re-validates every destination
//! against the live filesystem (or Drive) immediately before writing, refuses to write through
//! symlinks or into reserved paths, and never deletes anything.
use crate::cache::Cache;
use crate::config::{GD_DIR, IGNORE_FILE};
use crate::drive::Drive;
use crate::progress::{self, Spinner};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use ignore::gitignore::Gitignore;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use walkdir::WalkDir;

/// Clock skew tolerated before two mtimes are considered different.
const MTIME_TOLERANCE_MS: i64 = 1000;
pub const PART_SUFFIX: &str = ".dsync-part";

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Entry {
    pub mtime_ms: i64,
    pub size: Option<u64>,
    pub md5: Option<String>,
    pub is_dir: bool,
    pub id: Option<String>,
    pub native_doc: bool,
    /// Local file that could not be read when hashing; never transferred, reported as an error.
    pub unreadable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    LocalOnly,
    RemoteOnly,
    LocalNewer,
    RemoteNewer,
    /// Content differs but the modification times are equal.
    Modified,
    /// A folder on one side and a file on the other.
    TypeMismatch,
    /// The local file could not be read.
    Unreadable,
}

impl Change {
    pub fn label(self) -> &'static str {
        match self {
            Change::LocalOnly => "local only",
            Change::RemoteOnly => "remote only",
            Change::LocalNewer => "local newer",
            Change::RemoteNewer => "remote newer",
            Change::Modified => "content differs, same mtime",
            Change::TypeMismatch => "folder on one side, file on the other",
            Change::Unreadable => "local file could not be read",
        }
    }
}

pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp_millis())
}

pub fn fmt_ms(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(|| "-".into())
}

fn mtime_ms_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Drive allows names that cannot be mapped onto a local path ("/", ".", "..", empty, or longer
/// than the 255 bytes Linux and macOS filesystems accept for one component).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 255
        && !name.contains('/')
        && !name.contains('\0')
}

/// Name of the private temp file a download is written to, next to its destination. Kept short so
/// it fits the 255-byte component limit whatever the real name's length, and unique per name.
pub fn part_name(name: &str) -> String {
    let short: String = name.chars().take(40).collect();
    let digest = md5::compute(name.as_bytes());
    format!(
        ".{short}.{:08x}{PART_SUFFIX}",
        u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
    )
}

/// Whether `name` is exactly of the form `part_name` produces (`.<short>.<8 hex>.dsync-part`).
/// Only such files are ever deleted; anything merely ending in the suffix is left alone.
pub fn is_part_name(name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix('.')
        .and_then(|n| n.strip_suffix(PART_SUFFIX))
    else {
        return false;
    };
    stem.rsplit_once('.')
        .is_some_and(|(_, hex)| hex.len() == 8 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Delete temp files left under `base` by an interrupted earlier download. Returns how many went.
pub fn remove_stale_parts(root: &Path, base: &Path) -> usize {
    let mut removed = 0;
    let walker = WalkDir::new(base)
        .into_iter()
        .filter_entry(|e| !e.file_type().is_symlink() && !is_under_gd(root, e.path()));
    for entry in walker.flatten() {
        let is_part = entry.file_name().to_str().is_some_and(is_part_name);
        if is_part && entry.file_type().is_file() && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn is_under_gd(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .map(|rel| rel.components().any(|c| c.as_os_str() == GD_DIR))
        .unwrap_or(false)
}

pub fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

pub fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

/// Printable form of a path: control characters are escaped so untrusted names cannot drive the terminal.
pub fn display(path: &str) -> String {
    path.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// Paths dsync owns or must never write: anything under `.gd/` and download temp files.
pub fn is_reserved(rel: &str) -> bool {
    rel.split('/')
        .any(|c| c == GD_DIR || c.ends_with(PART_SUFFIX))
}

/// Normalize `path` (relative to cwd) to an absolute path without touching the filesystem.
pub fn absolutize(path: &str) -> Result<PathBuf> {
    let mut out = std::env::current_dir()?;
    let p = Path::new(path);
    if p.is_absolute() {
        out = PathBuf::from("/");
    }
    for c in p.components() {
        use std::path::Component::*;
        match c {
            RootDir | CurDir | Prefix(_) => {}
            ParentDir => {
                out.pop();
            }
            Normal(n) => out.push(n),
        }
    }
    Ok(out)
}

/// Turn an absolute local path into a slash-separated path relative to `root`.
pub fn rel_path(root: &Path, abs: &Path) -> Result<String> {
    let rel = abs.strip_prefix(root).with_context(|| {
        format!(
            "{} is outside the workspace {}",
            abs.display(),
            root.display()
        )
    })?;
    let parts = rel
        .components()
        .map(|c| {
            c.as_os_str()
                .to_str()
                .map(String::from)
                .with_context(|| format!("{} is not valid UTF-8", abs.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(parts.join("/"))
}

pub fn load_ignore(root: &Path) -> Gitignore {
    let (gi, err) = Gitignore::new(root.join(IGNORE_FILE));
    if let Some(e) = err {
        progress::eprintln(&format!("warning: problem in {IGNORE_FILE}: {e}"));
    }
    gi
}

fn is_ignored(root: &Path, ignore: &Gitignore, path: &Path, is_dir: bool) -> bool {
    if let Ok(rel) = path.strip_prefix(root) {
        if rel.components().any(|c| c.as_os_str() == GD_DIR) {
            return true;
        }
        if rel
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(PART_SUFFIX))
        {
            return true; // leftover from an interrupted download
        }
    }
    ignore.matched_path_or_any_parents(path, is_dir).is_ignore()
}

/// Whether an explicitly selected path is excluded by `.driveignore` (or reserved).
pub fn is_excluded(root: &Path, ignore: &Gitignore, abs: &Path) -> bool {
    let is_dir = std::fs::symlink_metadata(abs)
        .map(|m| m.is_dir())
        .unwrap_or(false);
    is_ignored(root, ignore, abs, is_dir)
}

/// Drop remote entries the local side would never scan, so nothing can be pulled over an ignored
/// or reserved local path, and nothing ignored is ever compared.
pub fn filter_remote(root: &Path, ignore: &Gitignore, remote: &mut BTreeMap<String, Entry>) {
    remote.retain(|rel, e| {
        !is_reserved(rel)
            && !ignore
                .matched_path_or_any_parents(root.join(rel), e.is_dir)
                .is_ignore()
    });
}

/// Walk the local subtree at `base` (an absolute path under `root`), keyed by path relative to
/// `root`. `depth` counts levels from the root on both sides. Only stat information is collected;
/// hashes are filled in later, and only where needed. Symlinks and non-regular files are skipped.
pub fn local_walk(
    root: &Path,
    base: &Path,
    depth: i32,
    ignore: &Gitignore,
) -> Result<BTreeMap<String, Entry>> {
    let mut out = BTreeMap::new();
    let base_meta = match std::fs::symlink_metadata(base) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", base.display())),
    };
    if base_meta.file_type().is_symlink() {
        bail!(
            "{} is a symlink; dsync does not follow symlinks",
            base.display()
        );
    }
    let base_rel = rel_path(root, base)?;
    if !base_rel.is_empty() {
        guard_parents(root, &base_rel)?; // never read through a symlinked ancestor either
    }
    let base_level = if base_rel.is_empty() {
        0
    } else {
        base_rel.matches('/').count() as i32 + 1
    };
    if depth >= 0 && base_level > depth {
        bail!("{base_rel} is deeper than the configured depth {depth}");
    }
    // The selected directory is itself part of the snapshot (the root never is).
    let mut walker = WalkDir::new(base).min_depth(usize::from(base_rel.is_empty()));
    if depth >= 0 && base_meta.is_dir() {
        walker = walker.max_depth((depth - base_level) as usize);
    }
    for entry in walker.into_iter().filter_entry(|e| {
        !e.file_type().is_symlink() && !is_ignored(root, ignore, e.path(), e.file_type().is_dir())
    }) {
        // One unreadable directory must not abort the whole run: it is recorded as an unreadable
        // entry (reported as `E`, never transferred, exit status non-zero) and the walk goes on.
        // A file that vanished between readdir and stat is simply no longer there.
        let entry = match entry {
            Ok(e) => e,
            Err(e)
                if e.io_error()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue
            }
            Err(e) => {
                let path = e.path().unwrap_or(base).to_path_buf();
                if path == base {
                    return Err(e).with_context(|| format!("reading {}", base.display()));
                }
                if let Ok(rel) = rel_path(root, &path) {
                    progress::eprintln(&format!("error: could not read {}: {e}", display(&rel)));
                    out.insert(
                        rel,
                        Entry {
                            is_dir: true,
                            unreadable: true,
                            ..Default::default()
                        },
                    );
                }
                continue;
            }
        };
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e)
                if e.io_error()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", entry.path().display())),
        };
        let rel = match rel_path(root, entry.path()) {
            Ok(rel) => rel,
            Err(e) => {
                progress::eprintln(&format!("! skip     {e:#}"));
                continue;
            }
        };
        let ft = meta.file_type();
        if !ft.is_file() && !ft.is_dir() {
            progress::eprintln(&format!(
                "! skip     {}  (not a regular file)",
                display(&rel)
            ));
            continue;
        }
        if is_reserved(&rel) {
            continue;
        }
        out.insert(
            rel,
            Entry {
                mtime_ms: mtime_ms_of(&meta),
                size: ft.is_file().then_some(meta.len()),
                is_dir: ft.is_dir(),
                ..Default::default()
            },
        );
    }
    Ok(out)
}

/// MD5 of a file, streamed in 1 MiB chunks.
pub fn file_md5(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut ctx = md5::Context::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(format!("{:x}", ctx.compute()));
        }
        ctx.consume(&buf[..n]);
    }
}

/// MD5 of a file that must still have the `size` and `mtime_ms` seen earlier once it has been
/// read; a file being written while it is hashed would otherwise be cached under a stat that
/// describes different bytes, and the plan would be made from a stale snapshot.
pub fn hash_stable(path: &Path, size: u64, mtime_ms: i64) -> Result<String> {
    let md5 = file_md5(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    if meta.len() != size || mtime_ms_of(&meta) != mtime_ms {
        bail!("file changed while it was being read; re-run when it is stable");
    }
    Ok(md5)
}

/// Some(true) = identical, Some(false) = different, None = cannot tell without hashing.
fn same_content(a: &Entry, b: &Entry) -> Option<bool> {
    if let (Some(x), Some(y)) = (&a.md5, &b.md5) {
        return Some(x == y);
    }
    match (a.size, b.size) {
        (Some(x), Some(y)) if x != y => Some(false),
        _ => None,
    }
}

fn within_tolerance(a: &Entry, b: &Entry) -> bool {
    (a.mtime_ms - b.mtime_ms).abs() <= MTIME_TOLERANCE_MS
}

/// Does deciding this pair require a local hash? Never when sizes differ (already known to be
/// different) or the remote has no MD5. By default every remaining pair is verified by MD5; the
/// hash cache makes that a one-time read per file. With `fast`, equal size and mtime is trusted
/// (rsync's quick check) and only equal size with differing mtimes is hashed.
fn needs_hash(l: &Entry, r: &Entry, fast: bool) -> bool {
    !l.is_dir
        && !r.is_dir
        && l.md5.is_none()
        && r.md5.is_some()
        && l.size == r.size
        && (!fast || !within_tolerance(l, r))
}

/// Compute (or fetch from the stat-keyed cache, unless `verify`) the MD5 of every local file whose
/// comparison depends on it. Hashing runs on `threads` workers; results are stored in the cache.
/// A file that cannot be read is marked unreadable and is never transferred.
pub fn fill_hashes(
    root: &Path,
    cache: &Cache,
    local: &mut BTreeMap<String, Entry>,
    remote: &BTreeMap<String, Entry>,
    fast: bool,
    verify: bool,
    threads: usize,
) -> Result<()> {
    let todo: Vec<(String, u64, i64)> = local
        .iter()
        .filter(|(p, l)| remote.get(*p).is_some_and(|r| needs_hash(l, r, fast)))
        .map(|(p, l)| (p.clone(), l.size.unwrap_or(0), l.mtime_ms))
        .collect();
    if todo.is_empty() {
        return Ok(());
    }
    let total = todo.len();
    let done = AtomicUsize::new(0);
    let spinner = Spinner::start(&format!(
        "Hashing 0/{total} local files ({threads} threads)"
    ));
    let results = parallel(todo, threads, |(path, size, mtime_ms)| {
        let cached = if verify {
            None
        } else {
            cache.local_hash(&path, size, mtime_ms).ok().flatten()
        };
        let result = match cached {
            Some(md5) => Ok(md5),
            None => hash_stable(&root.join(&path), size, mtime_ms).inspect(|md5| {
                if let Err(e) = cache.set_local_hash(&path, size, mtime_ms, md5) {
                    progress::eprintln(&format!(
                        "warning: hash cache update failed for {}: {e:#}",
                        display(&path)
                    ));
                }
            }),
        };
        spinner.set(format!(
            "Hashing {}/{total} local files ({threads} threads)",
            done.fetch_add(1, Ordering::SeqCst) + 1
        ));
        (path, result)
    });
    spinner.finish();
    for (path, r) in results {
        let entry = local.get_mut(&path).expect("hashed path exists");
        match r {
            Ok(md5) => entry.md5 = Some(md5),
            Err(e) => {
                progress::eprintln(&format!("error: could not read {}: {e:#}", display(&path)));
                entry.unreadable = true;
            }
        }
    }
    Ok(())
}

/// The form under which a case- and normalization-insensitive filesystem (macOS, Windows) treats
/// two names as the same file.
fn fold(path: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    path.nfc().collect::<String>().to_lowercase()
}

/// Paths that collide with another path once case and Unicode normalization are ignored,
/// including everything below a colliding folder. On such a filesystem these map to one local
/// file and must not be transferred.
pub fn case_collisions(
    local: &BTreeMap<String, Entry>,
    remote: &BTreeMap<String, Entry>,
) -> BTreeSet<String> {
    let mut groups: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    for p in local.keys().chain(remote.keys()) {
        groups.entry(fold(p)).or_default().insert(p);
    }
    let roots: BTreeSet<&str> = groups
        .values()
        .filter(|g| g.len() > 1)
        .flatten()
        .copied()
        .collect();
    local
        .keys()
        .chain(remote.keys())
        .filter(|p| roots.contains(p.as_str()) || ancestors(p).any(|a| roots.contains(a)))
        .cloned()
        .collect()
}

fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.char_indices()
        .filter(|&(_, c)| c == '/')
        .map(move |(i, _)| &path[..i])
}

/// Compare two snapshots; returns only paths that differ.
pub fn diff(
    local: &BTreeMap<String, Entry>,
    remote: &BTreeMap<String, Entry>,
) -> Vec<(String, Change)> {
    let mut out = Vec::new();
    for (path, l) in local {
        match remote.get(path) {
            None => out.push((path.clone(), Change::LocalOnly)),
            Some(_) if l.unreadable => out.push((path.clone(), Change::Unreadable)),
            Some(r) if l.is_dir != r.is_dir => out.push((path.clone(), Change::TypeMismatch)),
            Some(_) if l.is_dir => {}
            Some(r) => match same_content(l, r) {
                Some(true) => {}
                _ if l.mtime_ms > r.mtime_ms + MTIME_TOLERANCE_MS => {
                    out.push((path.clone(), Change::LocalNewer))
                }
                _ if r.mtime_ms > l.mtime_ms + MTIME_TOLERANCE_MS => {
                    out.push((path.clone(), Change::RemoteNewer))
                }
                Some(false) => out.push((path.clone(), Change::Modified)),
                None => {}
            },
        }
    }
    for path in remote.keys().filter(|p| !local.contains_key(*p)) {
        out.push((path.clone(), Change::RemoteOnly));
    }
    out.sort();
    out
}

/// What the plan saw on Drive for a file that will be updated; re-checked before the upload.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Existing {
    pub id: String,
    pub mtime_ms: i64,
    pub md5: Option<String>,
}

/// One unit of work produced by planning and executed after confirmation.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Mkdir {
        path: String,
    },
    Upload {
        path: String,
        existing: Option<Existing>,
        mtime_ms: i64,
    },
    /// `expected_local` is the (size, mtime) the plan saw locally, or None if the file was absent;
    /// the destination must still match right before it is replaced.
    Download {
        path: String,
        id: String,
        mtime_ms: i64,
        md5: Option<String>,
        expected_local: Option<(u64, i64)>,
    },
}

impl Action {
    #[cfg(test)]
    pub fn path(&self) -> &str {
        match self {
            Action::Mkdir { path }
            | Action::Upload { path, .. }
            | Action::Download { path, .. } => path,
        }
    }
}

/// One deletion, planned only with `--delete` and executed after every transfer succeeded.
#[derive(Debug, Clone, PartialEq)]
pub enum Delete {
    /// Move a Drive entry to the trash (push). Re-verified against what the plan saw.
    Remote {
        path: String,
        id: String,
        is_dir: bool,
        mtime_ms: i64,
        md5: Option<String>,
    },
    /// Remove a local entry (pull). `expected` is the (size, mtime) the plan saw for a file.
    Local {
        path: String,
        is_dir: bool,
        expected: Option<(u64, i64)>,
    },
}

impl Delete {
    pub fn path(&self) -> &str {
        match self {
            Delete::Remote { path, .. } | Delete::Local { path, .. } => path,
        }
    }
    pub fn is_dir(&self) -> bool {
        match self {
            Delete::Remote { is_dir, .. } | Delete::Local { is_dir, .. } => *is_dir,
        }
    }
}

/// Which side `--delete` removes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Trash on Drive whatever is missing locally (push).
    Remote,
    /// Delete locally whatever is missing on Drive (pull).
    Local,
}

/// A path that was left alone, with the reason.
pub type Skip = (String, String);

/// The result of planning.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub actions: Vec<Action>,
    /// Structurally impossible (folder vs file, Google-native document). Not an error.
    pub skips: Vec<Skip>,
    /// Destination is newer than the source, content differs with equal mtimes, or names collide
    /// on a case-insensitive filesystem. Never transferred unless `--force` (which turns the first
    /// two kinds into actions; case collisions are never forced).
    pub conflicts: Vec<Skip>,
    /// Local files that could not be read. Never transferred; the command exits non-zero.
    pub errors: Vec<Skip>,
    /// Entries present only on the destination side, removed after the transfers with `--delete`.
    pub deletions: Vec<Delete>,
}

const CASE_COLLISION: &str =
    "name differs only by case from another entry; case-insensitive filesystem";

/// Decide whether a file needs transferring from `src` to `dst`. Returns Some(dst exists).
fn needs_transfer(
    src: &Entry,
    dst: Option<&Entry>,
    force: bool,
    path: &str,
    other: &str,
    plan: &mut Plan,
) -> Option<bool> {
    let d = match dst {
        None => return Some(false),
        Some(d) if d.is_dir => {
            plan.skips
                .push((path.into(), format!("{other} is a folder")));
            return None;
        }
        Some(d) => d,
    };
    let same = same_content(src, d);
    if same == Some(true) {
        return None;
    }
    if within_tolerance(src, d) && same != Some(false) {
        return None; // same size and mtime: assumed identical
    }
    let conflict = if d.mtime_ms > src.mtime_ms + MTIME_TOLERANCE_MS {
        Some(format!("{other} is newer"))
    } else if within_tolerance(src, d) {
        Some(Change::Modified.label().to_string())
    } else {
        None
    };
    match conflict {
        Some(reason) if !force => {
            plan.conflicts.push((path.into(), reason));
            None
        }
        _ => Some(true),
    }
}

/// Plan a push.
pub fn plan_push(
    local: &BTreeMap<String, Entry>,
    remote: &BTreeMap<String, Entry>,
    force: bool,
    collisions: &BTreeSet<String>,
) -> Plan {
    let mut plan = Plan::default();
    for (path, l) in local {
        if collisions.contains(path) {
            plan.conflicts.push((path.clone(), CASE_COLLISION.into()));
        } else if l.unreadable {
            plan.errors
                .push((path.clone(), "could not read local file".into()));
        } else if l.is_dir {
            match remote.get(path) {
                None => plan.actions.push(Action::Mkdir { path: path.clone() }),
                Some(r) if !r.is_dir => plan
                    .skips
                    .push((path.clone(), "remote is a file, local is a folder".into())),
                Some(_) => {}
            }
        } else if remote.get(path).is_some_and(|r| r.native_doc) {
            plan.skips.push((
                path.clone(),
                "remote is a Google-native document; not overwritten".into(),
            ));
        } else if let Some(exists) =
            needs_transfer(l, remote.get(path), force, path, "remote", &mut plan)
        {
            let existing = if exists {
                remote.get(path).and_then(|r| {
                    Some(Existing {
                        id: r.id.clone()?,
                        mtime_ms: r.mtime_ms,
                        md5: r.md5.clone(),
                    })
                })
            } else {
                None
            };
            plan.actions.push(Action::Upload {
                path: path.clone(),
                existing,
                mtime_ms: l.mtime_ms,
            });
        }
    }
    plan
}

/// Plan a pull.
pub fn plan_pull(
    local: &BTreeMap<String, Entry>,
    remote: &BTreeMap<String, Entry>,
    force: bool,
    collisions: &BTreeSet<String>,
) -> Plan {
    let mut plan = Plan::default();
    for (path, r) in remote {
        let l = local.get(path);
        if collisions.contains(path) {
            plan.conflicts.push((path.clone(), CASE_COLLISION.into()));
        } else if l.is_some_and(|l| l.unreadable) {
            plan.errors
                .push((path.clone(), "could not read local file".into()));
        } else if r.is_dir {
            match l {
                None => plan.actions.push(Action::Mkdir { path: path.clone() }),
                Some(l) if !l.is_dir => plan
                    .skips
                    .push((path.clone(), "local is a file, remote is a folder".into())),
                Some(_) => {}
            }
        } else if r.native_doc {
            plan.skips.push((
                path.clone(),
                "Google-native document; export not supported".into(),
            ));
        } else if needs_transfer(r, l, force, path, "local", &mut plan).is_some() {
            plan.actions.push(Action::Download {
                path: path.clone(),
                id: r.id.clone().unwrap_or_default(),
                mtime_ms: r.mtime_ms,
                md5: r.md5.clone(),
                expected_local: l.and_then(|l| Some((l.size?, l.mtime_ms))),
            });
        }
    }
    plan
}

/// Plan `--delete`: every entry that exists only on the destination side, sorted deepest first so
/// folders are removed after their contents. Never included, and reported as skips instead:
/// names that collide on a case-insensitive filesystem (the "missing" file is the same file under
/// another spelling); Google-native documents; anything below a path that is a folder on one side
/// and a file on the other; anything below a local folder that could not be read (its contents
/// are unknown, not absent); and, for the remote side, anything whose path or ancestor exists on
/// disk under `root` without being part of the local snapshot (a symlink, a special file, or a
/// folder `.driveignore` pruned), since such a file may well exist locally after all.
pub fn plan_deletions(
    root: &Path,
    local: &BTreeMap<String, Entry>,
    remote: &BTreeMap<String, Entry>,
    collisions: &BTreeSet<String>,
    on: Side,
) -> (Vec<Delete>, Vec<Skip>) {
    let (dst, src) = match on {
        Side::Remote => (remote, local),
        Side::Local => (local, remote),
    };
    let under = |path: &str, top: &str| path == top || path.starts_with(&format!("{top}/"));
    let unreadable: Vec<&str> = local
        .iter()
        .filter(|(_, e)| e.unreadable)
        .map(|(p, _)| p.as_str())
        .collect();
    let mismatched: Vec<&str> = local
        .iter()
        .filter(|(p, l)| remote.get(*p).is_some_and(|r| r.is_dir != l.is_dir))
        .map(|(p, _)| p.as_str())
        .collect();
    // Remote-only paths whose path or an ancestor exists on disk unknown to the walk.
    let unwalked_locally = |path: &str| -> bool {
        on == Side::Remote
            && ancestors(path)
                .chain(std::iter::once(path))
                .any(|a| !local.contains_key(a) && std::fs::symlink_metadata(root.join(a)).is_ok())
    };
    let mut deletions = Vec::new();
    let mut skips = Vec::new();
    for (path, e) in dst {
        if src.contains_key(path) || collisions.contains(path) {
            continue;
        }
        if unreadable.iter().any(|u| under(path, u)) {
            skips.push((
                path.clone(),
                "local folder could not be read; not deleted".into(),
            ));
            continue;
        }
        if mismatched.iter().any(|m| under(path, m)) {
            skips.push((
                path.clone(),
                "below a path that is a folder on one side and a file on the other; not deleted"
                    .into(),
            ));
            continue;
        }
        if unwalked_locally(path) {
            skips.push((
                path.clone(),
                "exists locally outside the sync (symlink, special file or ignored folder); not trashed"
                    .into(),
            ));
            continue;
        }
        match on {
            Side::Remote => {
                if e.native_doc {
                    skips.push((path.clone(), "Google-native document; not trashed".into()));
                    continue;
                }
                let Some(id) = e.id.clone() else { continue };
                deletions.push(Delete::Remote {
                    path: path.clone(),
                    id,
                    is_dir: e.is_dir,
                    mtime_ms: e.mtime_ms,
                    md5: e.md5.clone(),
                });
            }
            Side::Local => deletions.push(Delete::Local {
                path: path.clone(),
                is_dir: e.is_dir,
                expected: e.size.map(|size| (size, e.mtime_ms)),
            }),
        }
    }
    // Deepest first, so a folder is only ever removed after everything inside it.
    deletions.sort_by(|a, b| {
        b.path()
            .matches('/')
            .count()
            .cmp(&a.path().matches('/').count())
            .then_with(|| a.path().cmp(b.path()))
    });
    (deletions, skips)
}

/// The `--delete` guard, checked before anything is transferred. `source_has_content` is false
/// when the source side holds nothing but the selected folder itself: syncing an empty source with
/// `--delete` would clear the destination, which is the classic way to lose everything, so it is
/// refused outright.
pub fn check_deletions(plan: &Plan, source_has_content: bool) -> Result<()> {
    if !plan.deletions.is_empty() && !source_has_content {
        bail!(
            "refusing --delete: the source side is empty, so every one of the {} destination entry(ies) would be deleted; remove them by hand if that is really intended",
            plan.deletions.len()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Listing: one line per path in the style of odeke-em/drive.
//   + added   M modified   - remote only   ! skipped   C conflict   E error

pub fn fmt_size(size: Option<u64>) -> String {
    let Some(n) = size else { return "-".into() };
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out + " B"
}

/// `marker path  note`
pub fn line(marker: &str, path: &str, note: &str) -> String {
    let path = display(path);
    if note.is_empty() {
        format!("{marker} {path}")
    } else {
        format!("{marker} {path}  {note}")
    }
}

/// One line per planned change. `src` is the side being copied from, `dst` the side overwritten.
fn action_line(
    a: &Action,
    local: &BTreeMap<String, Entry>,
    remote: &BTreeMap<String, Entry>,
) -> String {
    let transfer =
        |path: &str, src: Option<&Entry>, dst: Option<&Entry>, src_name: &str, dst_name: &str| {
            match (src, dst) {
                (Some(s), None) => line("+", path, &fmt_size(s.size)),
                (Some(s), Some(d)) if s.mtime_ms > d.mtime_ms + MTIME_TOLERANCE_MS => line(
                    "M",
                    path,
                    &format!("{}, {src_name} newer", fmt_size(s.size)),
                ),
                (Some(s), Some(d)) if d.mtime_ms > s.mtime_ms + MTIME_TOLERANCE_MS => line(
                    "M",
                    path,
                    &format!("{}, forced: {dst_name} is newer", fmt_size(s.size)),
                ),
                (Some(s), Some(_)) => line(
                    "M",
                    path,
                    &format!("{}, {}", fmt_size(s.size), Change::Modified.label()),
                ),
                (None, _) => line("M", path, ""),
            }
        };
    match a {
        Action::Mkdir { path } => line("+", &format!("{path}/"), ""),
        Action::Upload { path, .. } => {
            transfer(path, local.get(path), remote.get(path), "local", "remote")
        }
        Action::Download { path, .. } => {
            transfer(path, remote.get(path), local.get(path), "remote", "local")
        }
    }
}

/// One line per planned deletion.
fn delete_line(
    d: &Delete,
    local: &BTreeMap<String, Entry>,
    remote: &BTreeMap<String, Entry>,
) -> String {
    let (path, size, what) = match d {
        Delete::Remote { path, .. } => (path, remote.get(path), "trash on Drive"),
        Delete::Local { path, .. } => (path, local.get(path), "delete locally"),
    };
    if d.is_dir() {
        line("D", &format!("{path}/"), what)
    } else {
        line(
            "D",
            path,
            &format!("{}, {what}", fmt_size(size.and_then(|e| e.size))),
        )
    }
}

/// Interpret a confirmation answer. `None` is end-of-input (no terminal, `< /dev/null`), which is
/// never consent.
pub fn parse_answer(input: Option<&str>, default_yes: bool) -> bool {
    match input {
        None => false,
        Some(s) => {
            let s = s.trim().to_ascii_lowercase();
            if s.is_empty() {
                default_yes
            } else {
                s == "y" || s == "yes"
            }
        }
    }
}

/// Print the plan and ask for confirmation. Conflicts are listed but never transferred; while any
/// exist the prompt defaults to "no", and `--no-prompt` refuses to proceed at all.
/// Returns false if there is nothing to do or the user declined.
pub fn confirm(
    plan: &Plan,
    local: &BTreeMap<String, Entry>,
    remote: &BTreeMap<String, Entry>,
    no_prompt: bool,
) -> Result<bool> {
    for a in &plan.actions {
        println!("{}", action_line(a, local, remote));
    }
    for (path, reason) in &plan.skips {
        println!("{}", line("!", path, &format!("skipped: {reason}")));
    }
    for (path, reason) in &plan.conflicts {
        println!("{}", line("C", path, &format!("conflict: {reason}")));
    }
    for (path, reason) in &plan.errors {
        println!("{}", line("E", path, &format!("error: {reason}")));
    }
    for d in &plan.deletions {
        println!("{}", delete_line(d, local, remote));
    }
    let size_of = |a: &Action| match a {
        Action::Upload { path, .. } => local.get(path).and_then(|e| e.size).unwrap_or(0),
        Action::Download { path, .. } => remote.get(path).and_then(|e| e.size).unwrap_or(0),
        Action::Mkdir { .. } => 0,
    };
    let is_add = |a: &Action| match a {
        Action::Mkdir { .. } => true,
        Action::Upload { existing, .. } => existing.is_none(),
        Action::Download { expected_local, .. } => expected_local.is_none(),
    };
    let (adds, mods): (Vec<_>, Vec<_>) = plan.actions.iter().partition(|a| is_add(a));
    if !adds.is_empty() {
        println!(
            "Addition count {} src: {}",
            adds.len(),
            fmt_size(Some(adds.iter().map(|a| size_of(a)).sum()))
        );
    }
    if !mods.is_empty() {
        println!(
            "Modification count {} src: {}",
            mods.len(),
            fmt_size(Some(mods.iter().map(|a| size_of(a)).sum()))
        );
    }
    if !plan.deletions.is_empty() {
        let bytes: u64 = plan
            .deletions
            .iter()
            .map(|d| {
                let side = match d {
                    Delete::Remote { .. } => remote,
                    Delete::Local { .. } => local,
                };
                side.get(d.path()).and_then(|e| e.size).unwrap_or(0)
            })
            .sum();
        println!(
            "Deletion count {} dst: {}",
            plan.deletions.len(),
            fmt_size(Some(bytes))
        );
        println!(
            "{}",
            match plan.deletions[0] {
                Delete::Remote { .. } =>
                    "Deletions move Drive entries to the trash (restorable from Drive for a while).",
                Delete::Local { .. } if LOCAL_TRASH => "Deletions move local files to the Trash.",
                Delete::Local { .. } =>
                    "Deletions remove local files permanently; there is no trash on this platform.",
            }
        );
    }
    if !plan.skips.is_empty() {
        println!("Skip count {}", plan.skips.len());
    }
    if !plan.errors.is_empty() {
        println!("Error count {}", plan.errors.len());
    }
    if !plan.conflicts.is_empty() {
        println!("Conflict count {}", plan.conflicts.len());
        println!("Conflicts are never overwritten: both sides differ and the destination is not older, or names collide.\nResolve them manually (see `dsync diff`), or re-run with --force to overwrite newer/modified files.");
        if no_prompt {
            bail!("{} conflict(s); refusing to proceed without a prompt (resolve manually or use --force)", plan.conflicts.len());
        }
    }
    if plan.actions.is_empty() && plan.deletions.is_empty() {
        println!(
            "{}",
            if plan.skips.is_empty() && plan.conflicts.is_empty() && plan.errors.is_empty() {
                "Everything is up to date."
            } else {
                "Nothing to transfer."
            }
        );
        return Ok(false);
    }
    if no_prompt {
        return Ok(true);
    }
    // Deletions never happen on a reflexive Enter: the default flips to "no" whenever any exist.
    let changes = if plan.conflicts.is_empty() {
        "the changes"
    } else {
        "the non-conflicting changes only"
    };
    let (question, default_yes) = match plan.deletions.len() {
        0 if plan.conflicts.is_empty() => (format!("Proceed with {changes}? [Y/n]: "), true),
        0 => (format!("Proceed with {changes}? [y/N]: "), false),
        n => (
            format!("Proceed with {changes}, including {n} deletion(s)? [y/N]: "),
            false,
        ),
    };
    print!("{question}");
    std::io::Write::flush(&mut std::io::stdout())?;
    let mut answer = String::new();
    let read = std::io::stdin().read_line(&mut answer)?;
    Ok(parse_answer(
        (read > 0).then_some(answer.as_str()),
        default_yes,
    ))
}

/// One line per differing path for `dsync diff`, with both modification times.
pub fn diff_line(
    path: &str,
    change: Change,
    local: Option<&Entry>,
    remote: Option<&Entry>,
) -> String {
    let stamp = |e: Option<&Entry>| e.map(|e| fmt_ms(e.mtime_ms)).unwrap_or_else(|| "-".into());
    let what = |e: Option<&Entry>| match e {
        Some(e) if e.is_dir => "folder".to_string(),
        Some(e) => fmt_size(e.size),
        None => "-".into(),
    };
    match change {
        Change::LocalOnly => line("+", path, &format!("local only, {}", what(local))),
        Change::RemoteOnly => line("-", path, &format!("remote only, {}", what(remote))),
        Change::TypeMismatch => line("!", path, Change::TypeMismatch.label()),
        Change::Unreadable => line("E", path, Change::Unreadable.label()),
        c => line(
            "M",
            path,
            &format!(
                "{}  local: {}  remote: {}",
                c.label(),
                stamp(local),
                stamp(remote)
            ),
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Filesystem guards: the executor never trusts the plan's view of the destination.

/// Every ancestor of `rel` inside the workspace must be a real directory or absent: no symlinks,
/// so a write can never land outside the workspace. Reserved paths are refused outright.
pub fn guard_parents(root: &Path, rel: &str) -> Result<()> {
    if is_reserved(rel) {
        bail!("refusing to write reserved path {}", display(rel));
    }
    let mut cur = root.to_path_buf();
    let comps: Vec<&str> = rel.split('/').collect();
    for c in &comps[..comps.len().saturating_sub(1)] {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => bail!(
                "{} is a symlink; refusing to write through it",
                cur.display()
            ),
            Ok(m) if !m.is_dir() => bail!("{} is not a directory", cur.display()),
            _ => {}
        }
    }
    Ok(())
}

/// The destination file itself, checked immediately before it is replaced: not a symlink, and
/// still exactly what the plan saw (`expected` = (size, mtime)), or still absent if the plan
/// expected nothing there.
pub fn guard_file(dest: &Path, expected: Option<(u64, i64)>) -> Result<()> {
    match std::fs::symlink_metadata(dest) {
        Ok(m) if m.file_type().is_symlink() => {
            bail!("destination is a symlink; refusing to replace it")
        }
        Ok(m) => {
            let Some((size, mtime_ms)) = expected else {
                bail!("destination appeared after the plan was made; re-run to re-plan")
            };
            if !m.is_file() {
                bail!("destination is not a regular file");
            }
            if m.len() != size || mtime_ms_of(&m) != mtime_ms {
                bail!("destination changed since the plan was made; re-run to re-plan");
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if expected.is_some() {
                bail!("destination disappeared since the plan was made; re-run to re-plan");
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Move a verified temp file into place: the destination is re-checked, an existing file's
/// permissions are kept (new files get 0644), and the remote mtime is applied.
pub fn finalize_download(
    tmp: &Path,
    dest: &Path,
    expected: Option<(u64, i64)>,
    mtime_ms: i64,
) -> Result<()> {
    let result = (|| -> Result<()> {
        guard_file(dest, expected)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dest)
                .map(|m| m.permissions().mode() & 0o777) // never carry setuid/setgid/sticky bits
                .unwrap_or(0o644);
            std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode))?;
        }
        std::fs::rename(tmp, dest)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result?;
    filetime::set_file_mtime(
        dest,
        filetime::FileTime::from_unix_time(
            mtime_ms.div_euclid(1000),
            (mtime_ms.rem_euclid(1000) * 1_000_000) as u32,
        ),
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Execution

/// Run `f` over `items` on up to `threads` worker threads; results keep input order.
fn parallel<T: Send, R: Send>(items: Vec<T>, threads: usize, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let queue = std::sync::Mutex::new(items.into_iter().enumerate());
    let results = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| loop {
                let next = queue.lock().unwrap_or_else(|e| e.into_inner()).next();
                let Some((i, item)) = next else {
                    break;
                };
                let r = f(item);
                results
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((i, r));
            });
        }
    });
    let mut results = results.into_inner().unwrap_or_else(|e| e.into_inner());
    results.sort_by_key(|(i, _)| *i);
    results.into_iter().map(|(_, r)| r).collect()
}

/// Persists resumable-upload sessions in the cache, keyed by the file's stat at upload time, so
/// a session is only ever resumed for the exact bytes it was opened for.
struct Sessions<'a> {
    cache: &'a Cache,
    size: u64,
    mtime_ms: i64,
}

impl crate::drive::SessionStore for Sessions<'_> {
    fn load(&self, path: &str) -> Result<Option<crate::drive::UploadSession>> {
        match self.cache.upload_session(path, self.size, self.mtime_ms)? {
            Some(json) => match serde_json::from_str(&json) {
                Ok(session) => Ok(Some(session)),
                Err(_) => {
                    // Legacy sessions contain only a URI and cannot establish remote ownership.
                    self.cache.clear_upload_session(path)?;
                    Ok(None)
                }
            },
            None => Ok(None),
        }
    }
    fn save(&self, path: &str, session: &crate::drive::UploadSession) -> Result<()> {
        self.cache.set_upload_session(
            path,
            self.size,
            self.mtime_ms,
            &serde_json::to_string(session)?,
        )
    }
    fn clear(&self, path: &str) -> Result<()> {
        self.cache.clear_upload_session(path)
    }
}

/// Execute a push plan. Folders are created level by level, each level in parallel; then file
/// uploads run in parallel. Every completed action is recorded in the cache immediately, so a
/// cancelled push resumes cleanly. `base_rel` is the pushed subtree's root ("" for the workspace
/// root), which the caller has already made sure exists on Drive as `base_id`; `root_id` is the
/// sync root's id. Returns the number of failures.
#[allow(clippy::too_many_arguments)]
pub fn exec_push(
    drive: &Drive,
    cache: &Cache,
    root: &Path,
    root_id: &str,
    base_rel: &str,
    base_id: &str,
    actions: Vec<Action>,
    threads: usize,
) -> Result<usize> {
    let mut folder_ids: BTreeMap<String, String> = cache
        .load(base_rel)?
        .into_iter()
        .filter(|(_, e)| e.is_dir)
        .filter_map(|(p, e)| Some((p, e.id?)))
        .collect();
    folder_ids.insert(base_rel.to_string(), base_id.to_string());
    let mut failures = 0;

    // Phase 0: every already-indexed folder the plan writes into, and each of its ancestors, must
    // still hang where the index says it does. A folder moved out of the sync tree (or trashed)
    // since the refresh would otherwise receive new content at its new location. The ids come
    // from the index, so its ancestors' ids are looked up there too.
    let id_of = |p: &str| -> Option<String> {
        folder_ids.get(p).cloned().or_else(|| {
            cache
                .entry(p)
                .ok()
                .flatten()
                .filter(|e| e.is_dir)
                .and_then(|e| e.id)
        })
    };
    let written_into = actions.iter().filter_map(|a| match a {
        Action::Mkdir { path } if path == base_rel => None,
        Action::Mkdir { path } | Action::Upload { path, .. } => Some(parent_of(path)),
        Action::Download { .. } => None,
    });
    let bad = stale_folders(drive, root_id, &id_of, written_into, threads);
    if !bad.is_empty() {
        // Nothing is written into a bad folder or anything below it.
        folder_ids.retain(|p, _| {
            !bad.iter()
                .any(|b| p == b || p.starts_with(&format!("{b}/")))
        });
    }

    // Phase 1: folders, grouped by depth. Every folder at one level has its parent from the level above.
    let mut levels: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut uploads = Vec::new();
    for a in actions {
        match a {
            // The subtree root itself was created by the caller before execution began.
            Action::Mkdir { path } if path == base_rel => {
                progress::println(&format!("+ mkdir    {}/", display(&path)))
            }
            Action::Mkdir { path } => levels
                .entry(path.matches('/').count())
                .or_default()
                .push(path),
            Action::Upload {
                path,
                existing,
                mtime_ms,
            } => uploads.push((path, existing, mtime_ms)),
            Action::Download { .. } => unreachable!(),
        }
    }
    let total_dirs: usize = levels.values().map(Vec::len).sum();
    if total_dirs > 0 {
        let done = AtomicUsize::new(0);
        let spinner = Spinner::start(&format!(
            "Creating folders 0/{total_dirs} ({threads} streams)"
        ));
        for paths in levels.into_values() {
            let mut batch = Vec::new();
            for path in paths {
                match folder_ids.get(parent_of(&path)) {
                    Some(parent_id) => batch.push((path, parent_id.clone())),
                    None => {
                        progress::eprintln(&format!(
                            "x failed   {}/: parent folder is not available on Drive",
                            display(&path)
                        ));
                        failures += 1;
                    }
                }
            }
            let results = parallel(batch, threads, |(path, parent_id)| {
                // Drive is asked first: a folder that appeared since the index was refreshed is
                // adopted, never duplicated.
                let result =
                    drive.find_or_create_folder(&parent_id, path.rsplit('/').next().unwrap());
                match &result {
                    Ok(f) => {
                        if let Err(e) = cache.upsert(&path, &f.to_entry()) {
                            progress::eprintln(&format!(
                                "warning: cache update failed for {}: {e:#}",
                                display(&path)
                            ));
                        }
                        progress::println(&format!("+ mkdir    {}/", display(&path)));
                    }
                    Err(e) => progress::eprintln(&format!("x failed   {}/: {e:#}", display(&path))),
                }
                spinner.set(format!(
                    "Creating folders {}/{total_dirs} ({threads} streams)",
                    done.fetch_add(1, Ordering::SeqCst) + 1
                ));
                (path, result)
            });
            for (path, r) in results {
                match r {
                    Ok(f) => {
                        folder_ids.insert(path, f.id);
                    }
                    Err(_) => failures += 1,
                }
            }
        }
        spinner.finish();
    }

    // Phase 2: files, in parallel. Skip anything whose parent folder failed to be created.
    let mut jobs = Vec::new();
    for (path, existing, mtime_ms) in uploads {
        match folder_ids.get(parent_of(&path)) {
            Some(parent_id) => jobs.push((path, parent_id.clone(), existing, mtime_ms)),
            None => {
                progress::eprintln(&format!(
                    "x failed   {}: parent folder is not available on Drive",
                    display(&path)
                ));
                failures += 1;
            }
        }
    }
    let total = jobs.len();
    let done = AtomicUsize::new(0);
    let spinner = Spinner::start(&format!("Uploading 0/{total} ({threads} streams)"));
    let results = parallel(
        jobs,
        threads,
        |(path, parent_id, existing, planned_mtime)| {
            let local_path = root.join(&path);
            let result = (|| -> Result<crate::drive::File> {
                // The source is re-read now; if it changed since the plan, its current mtime is what
                // gets recorded so the next comparison is honest.
                let meta = std::fs::symlink_metadata(&local_path)?;
                if meta.file_type().is_symlink() || !meta.is_file() {
                    bail!("source is no longer a regular file");
                }
                let (size, mtime_ms) = (meta.len(), mtime_ms_of(&meta));
                if mtime_ms != planned_mtime {
                    progress::eprintln(&format!(
                        "note: {} changed since the plan was made; uploading its current content",
                        display(&path)
                    ));
                }
                // Fresh bytes bind resumed sessions even when an editor preserves size and mtime.
                let md5 = hash_stable(&local_path, size, mtime_ms)?;
                let sessions = Sessions {
                    cache,
                    size,
                    mtime_ms,
                };
                let f = drive.upload(
                    &crate::drive::Upload {
                        parent_id: &parent_id,
                        name: path.rsplit('/').next().unwrap(),
                        existing: existing.as_ref(),
                        local: &local_path,
                        rel: &path,
                        modified_time: &fmt_ms(mtime_ms),
                        md5: &md5,
                    },
                    &sessions,
                )?;
                let after = std::fs::symlink_metadata(&local_path)?;
                if after.len() != size || mtime_ms_of(&after) != mtime_ms {
                    bail!("file changed during the upload; re-run to push its current content");
                }
                match &f.md5_checksum {
                    Some(remote) if *remote != md5 => {
                        bail!("checksum mismatch after upload (local {md5}, Drive {remote}); re-run to push it again")
                    }
                    Some(_) => {}
                    None => progress::eprintln(&format!(
                        "warning: {} was uploaded but Drive returned no checksum, so it could not be verified",
                        display(&path)
                    )),
                }
                if let Err(e) = cache.upsert(&path, &f.to_entry()) {
                    progress::eprintln(&format!(
                        "warning: cache update failed for {}: {e:#}",
                        display(&path)
                    ));
                }
                let _ = cache.set_local_hash(&path, size, mtime_ms, &md5);
                let _ = cache.clear_upload_session(&path);
                Ok(f)
            })();
            match &result {
                Ok(_) => progress::println(&format!(
                    "^ {:<8} {}",
                    if existing.is_some() {
                        "updated"
                    } else {
                        "uploaded"
                    },
                    display(&path)
                )),
                Err(e) => progress::eprintln(&format!("x failed   {}: {e:#}", display(&path))),
            }
            spinner.set(format!(
                "Uploading {}/{total} ({threads} streams)",
                done.fetch_add(1, Ordering::SeqCst) + 1
            ));
            result.is_err()
        },
    );
    spinner.finish();
    Ok(failures + results.into_iter().filter(|failed| *failed).count())
}

/// Ask Drive whether every indexed folder among `parents` (and each of their ancestors) still
/// hangs where the index says it does; `id_of` gives a path's indexed folder id. Returns the
/// paths that were moved, trashed, deleted or could not be checked: nothing below them may be
/// written to or trashed, since they are no longer part of the sync tree.
fn stale_folders<'a>(
    drive: &Drive,
    root_id: &str,
    id_of: &dyn Fn(&str) -> Option<String>,
    parents: impl Iterator<Item = &'a str>,
    threads: usize,
) -> BTreeSet<String> {
    let to_verify = folders_to_verify(parents, &|p| id_of(p).is_some());
    let checks: Vec<(String, String, String)> = to_verify
        .iter()
        .filter_map(|p| {
            let expected = if parent_of(p).is_empty() {
                root_id.to_string()
            } else {
                id_of(parent_of(p))?
            };
            Some((p.clone(), id_of(p)?, expected))
        })
        .collect();
    let mut bad: BTreeSet<String> = BTreeSet::new();
    for (path, ok) in parallel(checks, threads, |(path, id, expected)| {
        let ok = match drive.live_folder_parents(&id) {
            Ok(Some(parents)) => parents.contains(&expected),
            Ok(None) => false,
            Err(e) => {
                progress::eprintln(&format!(
                    "x failed   {}/: could not verify the folder on Drive: {e:#}",
                    display(&path)
                ));
                false
            }
        };
        if !ok {
            progress::eprintln(&format!(
                "x failed   {}/: folder was moved, trashed or deleted on Drive since the plan was made; re-run",
                display(&path)
            ));
        }
        (path, ok)
    }) {
        if !ok {
            bad.insert(path);
        }
    }
    bad
}

/// The already-indexed folders (and all their ancestors, up to but excluding the root) that a
/// set of destination parents refers to. `known` says whether a path is an indexed folder.
fn folders_to_verify<'a>(
    parents: impl Iterator<Item = &'a str>,
    known: &dyn Fn(&str) -> bool,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for parent in parents {
        let mut p = parent;
        while !p.is_empty() {
            if known(p) && !out.insert(p.to_string()) {
                break;
            }
            p = parent_of(p);
        }
    }
    out
}

/// Execute a pull plan: local folders first, then downloads in parallel. Every write goes through
/// the filesystem guards. Returns the number of failures.
pub fn exec_pull(
    drive: &Drive,
    cache: &Cache,
    root: &Path,
    actions: Vec<Action>,
    threads: usize,
) -> Result<usize> {
    let mut failures = 0;
    let mut downloads = Vec::new();
    for a in actions {
        match a {
            Action::Mkdir { path } => {
                let result = (|| -> Result<()> {
                    guard_parents(root, &path)?;
                    let dest = root.join(&path);
                    match std::fs::symlink_metadata(&dest) {
                        Ok(m) if m.file_type().is_symlink() => {
                            bail!("{} is a symlink; refusing to use it", dest.display())
                        }
                        Ok(m) if !m.is_dir() => {
                            bail!("{} exists and is not a directory", dest.display())
                        }
                        _ => {}
                    }
                    std::fs::create_dir_all(&dest)?;
                    Ok(())
                })();
                match result {
                    Ok(()) => progress::println(&format!("+ mkdir    {}/", display(&path))),
                    Err(e) => {
                        progress::eprintln(&format!("x failed   {}/: {e:#}", display(&path)));
                        failures += 1;
                    }
                }
            }
            Action::Download {
                path,
                id,
                mtime_ms,
                md5,
                expected_local,
            } => downloads.push((path, id, mtime_ms, md5, expected_local)),
            Action::Upload { .. } => unreachable!(),
        }
    }
    let total = downloads.len();
    let done = AtomicUsize::new(0);
    let spinner = Spinner::start(&format!("Downloading 0/{total} ({threads} streams)"));
    let results = parallel(
        downloads,
        threads,
        |(path, id, mtime_ms, md5, expected_local)| {
            let dest = root.join(&path);
            let result = (|| -> Result<()> {
                guard_parents(root, &path)?;
                guard_file(&dest, expected_local)?; // cheap early check before spending bandwidth
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let tmp = drive.download_to_temp(&id, &dest, md5.as_deref())?;
                finalize_download(&tmp, &dest, expected_local, mtime_ms)?;
                if let (Some(md5), Ok(meta)) = (&md5, std::fs::symlink_metadata(&dest)) {
                    // Keyed by the stat the filesystem actually kept (coarser timestamps on
                    // FAT, HFS+ or network shares round the mtime), so the next diff needs no read.
                    let _ = cache.set_local_hash(&path, meta.len(), mtime_ms_of(&meta), md5);
                }
                Ok(())
            })();
            match &result {
                Ok(()) => progress::println(&format!("v downloaded {}", display(&path))),
                Err(e) => progress::eprintln(&format!("x failed   {}: {e:#}", display(&path))),
            }
            spinner.set(format!(
                "Downloading {}/{total} ({threads} streams)",
                done.fetch_add(1, Ordering::SeqCst) + 1
            ));
            result.is_err()
        },
    );
    spinner.finish();
    Ok(failures + results.into_iter().filter(|failed| *failed).count())
}

/// Trash on Drive what a push plan marked for deletion: files first, in parallel, then folders
/// deepest first. Every ancestor folder must still hang inside the sync tree, every entry is
/// re-fetched and must still be what the plan saw (name, parent, content, type), a folder must be
/// empty, and nothing that meanwhile exists locally is touched.
pub fn exec_delete_remote(
    drive: &Drive,
    cache: &Cache,
    root: &Path,
    remote_folder_id: &str,
    deletions: Vec<Delete>,
    threads: usize,
) -> Result<Deleted> {
    let total = deletions.len();
    if total == 0 {
        return Ok(Deleted::default());
    }
    let parent_id = |path: &str| -> Result<Option<String>> {
        let parent = parent_of(path);
        if parent.is_empty() {
            Ok(Some(remote_folder_id.to_string()))
        } else {
            cache.id_at(parent)
        }
    };
    // A folder moved out of the sync tree since the plan takes its contents with it: nothing under
    // it is ours to trash any more. Every ancestor of every target is checked first, once.
    let id_of = |p: &str| -> Option<String> {
        cache
            .entry(p)
            .ok()
            .flatten()
            .filter(|e| e.is_dir)
            .and_then(|e| e.id)
    };
    let bad = stale_folders(
        drive,
        remote_folder_id,
        &id_of,
        deletions.iter().map(|d| parent_of(d.path())),
        threads,
    );
    let mut failures = 0;
    let skipped = AtomicUsize::new(0);
    let deletions: Vec<Delete> = deletions
        .into_iter()
        .filter(|d| {
            let under_bad = bad
                .iter()
                .any(|b| d.path() == b || d.path().starts_with(&format!("{b}/")));
            if under_bad {
                failures += 1;
            }
            !under_bad
        })
        .collect();
    // Files at any depth can go in one parallel batch; folders wait for their contents.
    let mut files = Vec::new();
    let mut levels: BTreeMap<std::cmp::Reverse<usize>, Vec<Delete>> = BTreeMap::new();
    for d in deletions {
        if d.is_dir() {
            levels
                .entry(std::cmp::Reverse(d.path().matches('/').count()))
                .or_default()
                .push(d);
        } else {
            files.push(d);
        }
    }
    let done = AtomicUsize::new(0);
    let spinner = Spinner::start(&format!("Trashing 0/{total} ({threads} streams)"));
    for batch in std::iter::once(files).chain(levels.into_values()) {
        let jobs: Vec<(Delete, String)> = batch
            .into_iter()
            .filter_map(|d| match parent_id(d.path()) {
                Ok(Some(parent)) => Some((d, parent)),
                Ok(None) => {
                    progress::eprintln(&format!(
                        "x failed   {}: parent folder is not in the index; re-run to re-plan",
                        display(d.path())
                    ));
                    failures += 1;
                    None
                }
                Err(e) => {
                    progress::eprintln(&format!("x failed   {}: {e:#}", display(d.path())));
                    failures += 1;
                    None
                }
            })
            .collect();
        let results = parallel(jobs, threads, |(d, parent)| {
            let Delete::Remote {
                path,
                id,
                is_dir,
                mtime_ms,
                md5,
            } = &d
            else {
                unreachable!()
            };
            let shown = if *is_dir {
                format!("{}/", display(path))
            } else {
                display(path)
            };
            let result = (|| -> Result<Outcome> {
                // Nothing that exists locally in any form is trashed, and no ancestor may be a
                // symlink (the path would then denote something outside the workspace).
                guard_parents(root, path)?;
                if std::fs::symlink_metadata(root.join(path)).is_ok() {
                    return Ok(Outcome::Skipped(
                        "exists locally now; not trashed, re-run to re-plan",
                    ));
                }
                let expect = crate::drive::TrashExpect {
                    name: path.rsplit('/').next().unwrap(),
                    parent_id: &parent,
                    is_dir: *is_dir,
                    mtime_ms: *mtime_ms,
                    md5: md5.as_deref(),
                };
                let outcome = match drive.trash(id, &expect)? {
                    crate::drive::Trash::Done => Outcome::Done,
                    crate::drive::Trash::Gone => Outcome::Gone,
                    crate::drive::Trash::NotEmpty => {
                        return Ok(Outcome::Skipped(
                            "folder is not empty on Drive (files hidden by .driveignore, Google-native documents, content beyond the depth limit, new files, or a listing that has not caught up yet); left in place",
                        ))
                    }
                    crate::drive::Trash::Forbidden => {
                        return Ok(Outcome::Skipped(
                            "Drive refused: only its owner can trash it",
                        ))
                    }
                };
                if let Err(e) = cache.remove_and_rescan_parent(path, remote_folder_id) {
                    progress::eprintln(&format!(
                        "warning: cache update failed for {}: {e:#}",
                        display(path)
                    ));
                }
                Ok(outcome)
            })();
            match &result {
                Ok(Outcome::Done) => progress::println(&format!("- trashed  {shown}")),
                Ok(Outcome::Gone) => {
                    skipped.fetch_add(1, Ordering::SeqCst);
                    progress::println(&format!("- gone     {shown}  already removed on Drive"));
                }
                Ok(Outcome::Skipped(why)) => {
                    skipped.fetch_add(1, Ordering::SeqCst);
                    progress::println(&format!("! skipped  {shown}  {why}"));
                }
                Err(e) => progress::eprintln(&format!("x failed   {shown}: {e:#}")),
            }
            spinner.set(format!(
                "Trashing {}/{total} ({threads} streams)",
                done.fetch_add(1, Ordering::SeqCst) + 1
            ));
            result.is_err()
        });
        failures += results.into_iter().filter(|failed| *failed).count();
    }
    spinner.finish();
    Ok(Deleted {
        failed: failures,
        skipped: skipped.into_inner(),
    })
}

/// What happened to one planned deletion.
enum Outcome {
    Done,
    /// Was already absent; nothing to do.
    Gone,
    Skipped(&'static str),
}

/// Tally of a deletion phase: `failed` counts toward the exit status, `skipped` (including
/// entries that were already gone) is subtracted from the number of changes reported.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Deleted {
    pub failed: usize,
    pub skipped: usize,
}

/// Whether `pull --delete` moves local entries to the system trash (macOS) or removes them
/// outright (everywhere else).
pub const LOCAL_TRASH: bool = cfg!(target_os = "macos");

/// Remove a local file or (empty) folder. On macOS it goes to the user's Trash through
/// `NSFileManager`, which needs no Finder automation permission; if the Trash is unavailable the
/// entry stays, there is no fallback to deletion. On Linux it is deleted permanently.
pub fn remove_local(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        let mut ctx = trash::TrashContext::new();
        ctx.set_delete_method(DeleteMethod::NsFileManager);
        ctx.delete(path)
            .map_err(|e| anyhow::anyhow!("could not move to the Trash: {e}"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        if std::fs::symlink_metadata(path)?.is_dir() {
            std::fs::remove_dir(path)?;
        } else {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
}

/// Remove locally what a pull plan marked for deletion: files first, then folders deepest first,
/// each handed to `remove` (the Trash on macOS, deletion elsewhere). A file must still have the
/// size and mtime the plan saw; nothing is followed through a symlink; a folder is only removed
/// when empty (ignored files such as `.DS_Store` keep it, reported as a skip).
pub fn exec_delete_local(
    root: &Path,
    deletions: Vec<Delete>,
    remove: &dyn Fn(&Path) -> Result<()>,
) -> Deleted {
    let mut tally = Deleted::default();
    let (dirs, files): (Vec<Delete>, Vec<Delete>) = deletions.into_iter().partition(Delete::is_dir);
    for d in files.into_iter().chain(dirs) {
        let Delete::Local {
            path,
            is_dir,
            expected,
        } = &d
        else {
            unreachable!()
        };
        let dest = root.join(path);
        let shown = if *is_dir {
            format!("{}/", display(path))
        } else {
            display(path)
        };
        let result = (|| -> Result<Outcome> {
            guard_parents(root, path)?;
            let meta = match std::fs::symlink_metadata(&dest) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Outcome::Gone),
                Err(e) => return Err(e.into()),
            };
            if meta.file_type().is_symlink() {
                bail!("is a symlink; refusing to remove it");
            }
            if *is_dir {
                if !meta.is_dir() {
                    bail!("is no longer a folder; re-run to re-plan");
                }
                if std::fs::read_dir(&dest)?.next().is_some() {
                    return Ok(Outcome::Skipped(
                        "folder is not empty (ignored or new files inside); left in place",
                    ));
                }
            } else {
                let Some(expected) = *expected else {
                    bail!("no size and mtime were recorded for it; re-run to re-plan")
                };
                guard_file(&dest, Some(expected))?;
            }
            remove(&dest)?;
            Ok(Outcome::Done)
        })();
        match &result {
            Ok(Outcome::Done) => progress::println(&format!(
                "- {:<8} {shown}",
                if LOCAL_TRASH { "trashed" } else { "deleted" }
            )),
            Ok(Outcome::Gone) => {
                tally.skipped += 1;
                progress::println(&format!("- gone     {shown}  already removed"));
            }
            Ok(Outcome::Skipped(why)) => {
                tally.skipped += 1;
                progress::println(&format!("! skipped  {shown}  {why}"));
            }
            Err(e) => {
                progress::eprintln(&format!("x failed   {shown}: {e:#}"));
                tally.failed += 1;
            }
        }
    }
    tally
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(mtime_ms: i64, md5: Option<&str>, is_dir: bool) -> Entry {
        Entry {
            mtime_ms,
            size: (!is_dir).then_some(10),
            md5: md5.map(String::from),
            is_dir,
            ..Default::default()
        }
    }
    fn sized(mtime_ms: i64, size: u64) -> Entry {
        Entry {
            mtime_ms,
            size: Some(size),
            ..Default::default()
        }
    }
    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsync_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn none() -> BTreeSet<String> {
        BTreeSet::new()
    }

    #[test]
    fn diff_classifies_changes() {
        let local: BTreeMap<_, _> = [
            ("a.txt".to_string(), e(5000, Some("x"), false)),
            ("b.txt".to_string(), e(1000, Some("y"), false)),
            ("same.txt".to_string(), e(9000, Some("z"), false)),
            ("tol.txt".to_string(), e(1500, Some("q"), false)),
            ("only_local.txt".to_string(), e(1, None, false)),
            ("dir".to_string(), e(0, None, true)),
            ("size_differs.txt".to_string(), sized(1000, 10)),
            ("size_same_time_same.txt".to_string(), sized(1000, 10)),
            ("modified.txt".to_string(), e(1000, Some("m1"), false)),
            ("typed".to_string(), e(0, None, true)),
            (
                "bad.txt".to_string(),
                Entry {
                    unreadable: true,
                    ..sized(1, 1)
                },
            ),
        ]
        .into_iter()
        .collect();
        let remote: BTreeMap<_, _> = [
            ("a.txt".to_string(), e(1000, Some("x2"), false)),
            ("b.txt".to_string(), e(5000, Some("y2"), false)),
            ("same.txt".to_string(), e(1, Some("z"), false)),
            ("tol.txt".to_string(), e(1000, Some("q2"), false)),
            ("only_remote.txt".to_string(), e(1, None, false)),
            ("dir".to_string(), e(123, None, true)),
            ("size_differs.txt".to_string(), sized(1000, 11)),
            ("size_same_time_same.txt".to_string(), sized(1500, 10)),
            ("modified.txt".to_string(), e(1000, Some("m2"), false)),
            ("typed".to_string(), e(0, Some("f"), false)),
            ("bad.txt".to_string(), sized(1, 1)),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            diff(&local, &remote),
            vec![
                ("a.txt".to_string(), Change::LocalNewer),
                ("b.txt".to_string(), Change::RemoteNewer),
                ("bad.txt".to_string(), Change::Unreadable),
                ("modified.txt".to_string(), Change::Modified),
                ("only_local.txt".to_string(), Change::LocalOnly),
                ("only_remote.txt".to_string(), Change::RemoteOnly),
                ("size_differs.txt".to_string(), Change::Modified),
                ("tol.txt".to_string(), Change::Modified),
                ("typed".to_string(), Change::TypeMismatch),
            ]
        );
    }

    #[test]
    fn hashing_policy() {
        let remote = Entry {
            md5: Some("r".into()),
            ..sized(1000, 10)
        };
        assert!(
            needs_hash(&sized(5000, 10), &remote, false),
            "same size, different mtime"
        );
        assert!(
            needs_hash(&sized(1500, 10), &remote, false),
            "same size, same mtime: still verified"
        );
        assert!(
            !needs_hash(&sized(5000, 11), &remote, false),
            "different size: known different"
        );
        assert!(
            needs_hash(&sized(5000, 10), &remote, true),
            "fast: same size, different mtime"
        );
        assert!(
            !needs_hash(&sized(1500, 10), &remote, true),
            "fast: same size, same mtime trusted"
        );
        assert!(
            !needs_hash(&sized(5000, 10), &sized(1000, 10), false),
            "remote has no md5 (native doc)"
        );
        assert!(
            !needs_hash(
                &Entry {
                    md5: Some("x".into()),
                    ..sized(5000, 10)
                },
                &remote,
                false
            ),
            "already hashed"
        );
        assert!(
            !needs_hash(
                &Entry {
                    is_dir: true,
                    ..Default::default()
                },
                &remote,
                false
            ),
            "folders"
        );
    }

    #[test]
    fn fill_hashes_uses_cache_unless_verifying_and_flags_unreadable() {
        let dir = tmpdir("hash");
        std::fs::write(dir.join("f.txt"), b"hello").unwrap();
        let cache = Cache::open(&dir.join("cache.db")).unwrap();
        let ignore = load_ignore(&dir);
        let mut local = local_walk(&dir, &dir, -1, &ignore).unwrap();
        local.retain(|p, _| p == "f.txt");
        let (size, mtime) = (local["f.txt"].size.unwrap(), local["f.txt"].mtime_ms);
        let remote: BTreeMap<_, _> = [(
            "f.txt".to_string(),
            Entry {
                md5: Some("zz".into()),
                mtime_ms: mtime + 60_000,
                size: Some(size),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect();
        fill_hashes(&dir, &cache, &mut local, &remote, false, false, 2).unwrap();
        assert_eq!(
            local["f.txt"].md5.as_deref(),
            Some("5d41402abc4b2a76b9719d911017c592")
        );
        assert_eq!(
            cache.local_hash("f.txt", size, mtime).unwrap().as_deref(),
            Some("5d41402abc4b2a76b9719d911017c592")
        );
        assert_eq!(
            cache.local_hash("f.txt", size + 1, mtime).unwrap(),
            None,
            "stat change invalidates"
        );
        // Poison the cache entry: default mode trusts it, --verify re-reads the file.
        cache
            .set_local_hash("f.txt", size, mtime, "cached")
            .unwrap();
        let mut again = local.clone();
        again.get_mut("f.txt").unwrap().md5 = None;
        fill_hashes(&dir, &cache, &mut again, &remote, false, false, 2).unwrap();
        assert_eq!(again["f.txt"].md5.as_deref(), Some("cached"));
        let mut verified = local.clone();
        verified.get_mut("f.txt").unwrap().md5 = None;
        fill_hashes(&dir, &cache, &mut verified, &remote, false, true, 2).unwrap();
        assert_eq!(
            verified["f.txt"].md5.as_deref(),
            Some("5d41402abc4b2a76b9719d911017c592")
        );
        // A file that vanished between scan and hash is flagged, never treated as identical.
        std::fs::remove_file(dir.join("f.txt")).unwrap();
        cache.set_local_hash("f.txt", size, mtime, "stale").unwrap();
        let mut gone = local.clone();
        gone.get_mut("f.txt").unwrap().md5 = None;
        fill_hashes(&dir, &cache, &mut gone, &remote, false, true, 2).unwrap();
        assert!(gone["f.txt"].unreadable);
        assert_eq!(
            diff(&gone, &remote),
            vec![("f.txt".to_string(), Change::Unreadable)]
        );
        let plan = plan_push(&gone, &remote, false, &none());
        assert!(plan.actions.is_empty());
        assert_eq!(plan.errors.len(), 1);
        assert_eq!(
            plan_pull(&gone, &remote, true, &none()).errors.len(),
            1,
            "even --force never overwrites an unreadable local file"
        );
        drop(cache);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn plans_push_and_pull() {
        let local: BTreeMap<_, _> = [
            ("d".to_string(), e(0, None, true)),
            ("d/new.txt".to_string(), e(5000, Some("a"), false)),
            ("newer.txt".to_string(), e(9000, Some("b"), false)),
            ("older.txt".to_string(), e(1000, Some("c"), false)),
            ("same.txt".to_string(), e(1000, Some("s"), false)),
            ("unhashed_same.txt".to_string(), sized(1000, 10)),
            ("size_differs.txt".to_string(), sized(1000, 10)),
        ]
        .into_iter()
        .collect();
        let mut remote: BTreeMap<_, _> = [
            (
                "newer.txt".to_string(),
                Entry {
                    id: Some("id1".into()),
                    ..e(1000, Some("b2"), false)
                },
            ),
            (
                "older.txt".to_string(),
                Entry {
                    id: Some("id2".into()),
                    ..e(9000, Some("c2"), false)
                },
            ),
            (
                "same.txt".to_string(),
                Entry {
                    id: Some("id3".into()),
                    ..e(9000, Some("s"), false)
                },
            ),
            (
                "rdir".to_string(),
                Entry {
                    id: Some("id4".into()),
                    ..e(0, None, true)
                },
            ),
            (
                "rdir/r.txt".to_string(),
                Entry {
                    id: Some("id5".into()),
                    ..e(1, Some("r"), false)
                },
            ),
            (
                "unhashed_same.txt".to_string(),
                Entry {
                    id: Some("id6".into()),
                    ..sized(1500, 10)
                },
            ),
            (
                "size_differs.txt".to_string(),
                Entry {
                    id: Some("id7".into()),
                    ..sized(1000, 20)
                },
            ),
        ]
        .into_iter()
        .collect();
        let plan = plan_push(&local, &remote, false, &none());
        assert_eq!(
            plan.actions,
            vec![
                Action::Mkdir { path: "d".into() },
                Action::Upload {
                    path: "d/new.txt".into(),
                    existing: None,
                    mtime_ms: 5000,
                },
                Action::Upload {
                    path: "newer.txt".into(),
                    existing: Some(Existing {
                        id: "id1".into(),
                        mtime_ms: 1000,
                        md5: Some("b2".into())
                    }),
                    mtime_ms: 9000,
                },
            ]
        );
        assert!(plan.skips.is_empty() && plan.errors.is_empty());
        assert_eq!(
            plan.conflicts,
            vec![
                ("older.txt".to_string(), "remote is newer".to_string()),
                (
                    "size_differs.txt".to_string(),
                    "content differs, same mtime".to_string()
                )
            ]
        );
        let forced = plan_push(&local, &remote, true, &none());
        assert_eq!(
            forced.actions.len(),
            5,
            "force turns conflicts into actions"
        );
        assert!(forced.conflicts.is_empty());

        let plan = plan_pull(&local, &remote, false, &none());
        assert_eq!(
            plan.actions,
            vec![
                Action::Download {
                    path: "older.txt".into(),
                    id: "id2".into(),
                    mtime_ms: 9000,
                    md5: Some("c2".into()),
                    expected_local: Some((10, 1000))
                },
                Action::Mkdir {
                    path: "rdir".into()
                },
                Action::Download {
                    path: "rdir/r.txt".into(),
                    id: "id5".into(),
                    mtime_ms: 1,
                    md5: Some("r".into()),
                    expected_local: None
                },
            ]
        );
        assert_eq!(
            plan.conflicts,
            vec![
                ("newer.txt".to_string(), "local is newer".to_string()),
                (
                    "size_differs.txt".to_string(),
                    "content differs, same mtime".to_string()
                )
            ]
        );
        remote.get_mut("rdir/r.txt").unwrap().native_doc = true;
        assert_eq!(
            plan_pull(&local, &remote, false, &none()).skips,
            vec![(
                "rdir/r.txt".to_string(),
                "Google-native document; export not supported".to_string()
            )]
        );
        // A local file that collides with a Google-native document is never uploaded over it.
        let l3: BTreeMap<_, _> = [("doc".to_string(), sized(9000, 10))].into_iter().collect();
        let r3: BTreeMap<_, _> = [(
            "doc".to_string(),
            Entry {
                native_doc: true,
                id: Some("n".into()),
                mtime_ms: 1,
                ..Default::default()
            },
        )]
        .into_iter()
        .collect();
        let p3 = plan_push(&l3, &r3, false, &none());
        assert!(p3.actions.is_empty() && p3.conflicts.is_empty() && p3.skips.len() == 1);
        // Folder/file type mismatches are skips, not conflicts.
        let l2: BTreeMap<_, _> = [("x".to_string(), e(0, None, true))].into_iter().collect();
        let r2: BTreeMap<_, _> = [("x".to_string(), e(0, Some("q"), false))]
            .into_iter()
            .collect();
        assert_eq!(plan_push(&l2, &r2, false, &none()).skips.len(), 1);
        assert_eq!(plan_pull(&l2, &r2, false, &none()).skips.len(), 1);
    }

    #[test]
    fn case_collisions_are_conflicts_even_with_force() {
        let local: BTreeMap<_, _> = [
            (
                "Report.txt".to_string(),
                Entry {
                    md5: Some("new".into()),
                    ..sized(9000, 10)
                },
            ),
            ("Docs".to_string(), e(0, None, true)),
            ("Docs/inner.txt".to_string(), sized(1, 1)),
            ("plain.txt".to_string(), sized(1, 1)),
        ]
        .into_iter()
        .collect();
        let remote: BTreeMap<_, _> = [
            (
                "report.txt".to_string(),
                Entry {
                    id: Some("r".into()),
                    md5: Some("old".into()),
                    ..sized(1000, 10)
                },
            ),
            (
                "docs".to_string(),
                Entry {
                    id: Some("d".into()),
                    ..e(0, None, true)
                },
            ),
            (
                "docs/other.txt".to_string(),
                Entry {
                    id: Some("o".into()),
                    ..sized(1, 1)
                },
            ),
        ]
        .into_iter()
        .collect();
        let collisions = case_collisions(&local, &remote);
        assert_eq!(
            collisions.iter().cloned().collect::<Vec<_>>(),
            vec![
                "Docs",
                "Docs/inner.txt",
                "Report.txt",
                "docs",
                "docs/other.txt",
                "report.txt"
            ]
        );
        // Without collision detection this pull would replace the newer local Report.txt.
        let naive = plan_pull(&local, &remote, false, &none());
        assert!(naive.actions.iter().any(|a| a.path() == "report.txt"));
        let safe = plan_pull(&local, &remote, true, &collisions);
        assert!(safe.actions.is_empty());
        assert_eq!(safe.conflicts.len(), 3);
        let push = plan_push(&local, &remote, true, &collisions);
        assert_eq!(
            push.actions,
            vec![Action::Upload {
                path: "plain.txt".into(),
                existing: None,
                mtime_ms: 1,
            }]
        );
        assert!(case_collisions(&local, &BTreeMap::new()).is_empty());
        // NFC and NFD spellings of the same name are one file on macOS and Windows.
        let nfc: BTreeMap<_, _> = [("caf\u{e9}.txt".to_string(), sized(1, 1))]
            .into_iter()
            .collect();
        let nfd: BTreeMap<_, _> = [("cafe\u{301}.txt".to_string(), sized(1, 1))]
            .into_iter()
            .collect();
        assert_eq!(case_collisions(&nfc, &nfd).len(), 2);
        assert!(case_collisions(&nfc, &nfc).is_empty());
    }

    #[test]
    fn remote_snapshot_is_filtered_like_the_local_one() {
        let dir = tmpdir("filter");
        std::fs::write(dir.join(IGNORE_FILE), "*.log\nprivate/\n").unwrap();
        let ignore = load_ignore(&dir);
        let mut remote: BTreeMap<String, Entry> = [
            ".gd/credentials.json",
            ".gd/config.json",
            "nested/.gd/lock",
            "keep.txt",
            "debug.log",
            "private",
            "private/secret.txt",
            ".hidden.txt.dsync-part",
        ]
        .into_iter()
        .map(|p| {
            (
                p.to_string(),
                Entry {
                    is_dir: p == "private",
                    ..sized(1, 1)
                },
            )
        })
        .collect();
        filter_remote(&dir, &ignore, &mut remote);
        assert_eq!(remote.keys().cloned().collect::<Vec<_>>(), vec!["keep.txt"]);
        // And the executor refuses reserved paths regardless of what a plan says.
        assert!(guard_parents(&dir, ".gd/credentials.json").is_err());
        assert!(guard_parents(&dir, "a/.gd/x").is_err());
        assert!(guard_parents(&dir, "a/.x.dsync-part").is_err());
        assert!(guard_parents(&dir, "a/b/c.txt").is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn guards_refuse_symlinks_and_stale_destinations() {
        let dir = tmpdir("guard");
        let outside = tmpdir("guard_outside");
        std::os::unix::fs::symlink(&outside, dir.join("linked")).unwrap();
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::write(dir.join("real/file.txt"), "one").unwrap();
        std::os::unix::fs::symlink(dir.join("real/file.txt"), dir.join("real/link.txt")).unwrap();
        // Ancestor symlink: writes would land outside the workspace.
        assert!(guard_parents(&dir, "linked/secret.txt")
            .unwrap_err()
            .to_string()
            .contains("symlink"));
        assert!(guard_parents(&dir, "linked/deeper/x.txt").is_err());
        assert!(guard_parents(&dir, "real/file.txt/child")
            .unwrap_err()
            .to_string()
            .contains("not a directory"));
        assert!(guard_parents(&dir, "real/new.txt").is_ok());
        assert!(
            guard_parents(&dir, "brand/new/dir/file.txt").is_ok(),
            "absent ancestors are fine"
        );
        // Destination symlink is never replaced.
        assert!(guard_file(&dir.join("real/link.txt"), None).is_err());
        assert!(guard_file(&dir.join("real/link.txt"), Some((3, 0))).is_err());
        // Destination must match the plan exactly.
        let m = std::fs::metadata(dir.join("real/file.txt")).unwrap();
        let expected = Some((m.len(), mtime_ms_of(&m)));
        assert!(guard_file(&dir.join("real/file.txt"), expected).is_ok());
        assert!(guard_file(&dir.join("real/file.txt"), None)
            .unwrap_err()
            .to_string()
            .contains("appeared"));
        assert!(guard_file(
            &dir.join("real/file.txt"),
            Some((m.len() + 1, mtime_ms_of(&m)))
        )
        .unwrap_err()
        .to_string()
        .contains("changed"));
        assert!(guard_file(&dir.join("real/missing.txt"), expected)
            .unwrap_err()
            .to_string()
            .contains("disappeared"));
        assert!(guard_file(&dir.join("real/missing.txt"), None).is_ok());
        assert!(
            guard_file(&dir.join("real"), None).is_err(),
            "a directory is not a valid file destination"
        );
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn finalize_preserves_permissions_and_detects_concurrent_edits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir("finalize");
        let dest = dir.join("doc.txt");
        std::fs::write(&dest, "private").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600)).unwrap();
        let m = std::fs::metadata(&dest).unwrap();
        let expected = Some((m.len(), mtime_ms_of(&m)));
        let tmp = dir.join(".doc.txt.dsync-part");
        std::fs::write(&tmp, "downloaded").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        finalize_download(&tmp, &dest, expected, 1_700_000_000_500).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "downloaded");
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o600,
            "existing mode kept"
        );
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o7000,
            0,
            "no special bits"
        );
        // A setuid destination never passes setuid on to downloaded content.
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o4755)).unwrap();
        let m2 = std::fs::metadata(&dest).unwrap();
        let expected2 = Some((m2.len(), mtime_ms_of(&m2)));
        std::fs::write(&tmp, "again").unwrap();
        finalize_download(&tmp, &dest, expected2, 1_700_000_000_500).unwrap();
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o7777,
            0o755
        );
        assert_eq!(
            mtime_ms_of(&std::fs::metadata(&dest).unwrap()),
            1_700_000_000_500
        );
        assert!(!tmp.exists());
        // The user edits the file after the plan: the download must not clobber it.
        std::fs::write(&tmp, "newer download").unwrap();
        std::fs::write(&dest, "user edit").unwrap();
        let err = finalize_download(&tmp, &dest, expected, 1).unwrap_err();
        assert!(err.to_string().contains("changed since the plan"), "{err}");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "user edit");
        assert!(!tmp.exists(), "temp file cleaned up");
        // A brand-new file gets a sane default mode rather than the private temp mode.
        let fresh = dir.join("fresh.txt");
        let tmp2 = dir.join(".fresh.txt.dsync-part");
        std::fs::write(&tmp2, "x").unwrap();
        std::fs::set_permissions(&tmp2, std::fs::Permissions::from_mode(0o600)).unwrap();
        finalize_download(&tmp2, &fresh, None, 1000).unwrap();
        assert_eq!(
            std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o644
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn listing_lines_and_answers() {
        assert_eq!(fmt_size(Some(0)), "0 B");
        assert_eq!(fmt_size(Some(999)), "999 B");
        assert_eq!(fmt_size(Some(1234567)), "1,234,567 B");
        assert_eq!(fmt_size(None), "-");
        let l = Entry {
            mtime_ms: 1_700_000_000_000,
            size: Some(1234567),
            ..Default::default()
        };
        let r = Entry {
            mtime_ms: 1_700_000_005_000,
            size: Some(99),
            ..Default::default()
        };
        assert_eq!(diff_line("My Documents/report.txt", Change::RemoteNewer, Some(&l), Some(&r)), "M My Documents/report.txt  remote newer  local: 2023-11-14T22:13:20.000Z  remote: 2023-11-14T22:13:25.000Z");
        assert_eq!(
            diff_line("x", Change::LocalOnly, Some(&l), None),
            "+ x  local only, 1,234,567 B"
        );
        assert_eq!(
            diff_line("y", Change::RemoteOnly, None, Some(&r)),
            "- y  remote only, 99 B"
        );
        assert_eq!(
            diff_line(
                "Work",
                Change::RemoteOnly,
                None,
                Some(&Entry {
                    is_dir: true,
                    ..Default::default()
                })
            ),
            "- Work  remote only, folder"
        );
        assert_eq!(
            diff_line("t", Change::TypeMismatch, Some(&l), Some(&r)),
            "! t  folder on one side, file on the other"
        );
        assert_eq!(
            line("+", "evil\x1b[31mname\n.txt", ""),
            "+ evil\\u{1b}[31mname\\n.txt",
            "control characters never reach the terminal"
        );
        assert!(parse_answer(Some("\n"), true));
        assert!(!parse_answer(Some("\n"), false));
        assert!(parse_answer(Some("Y\n"), false));
        assert!(parse_answer(Some(" yes "), false));
        assert!(!parse_answer(Some("n"), true));
        assert!(!parse_answer(None, true), "EOF is never consent");
    }

    #[test]
    fn compact_plan_lines() {
        let local: BTreeMap<_, _> = [
            ("a b.txt".to_string(), sized(5000, 1500)),
            ("n.txt".to_string(), sized(1, 7)),
        ]
        .into_iter()
        .collect();
        let remote: BTreeMap<_, _> = [("a b.txt".to_string(), sized(1000, 10))]
            .into_iter()
            .collect();
        let up = |path: &str, existing: bool, mtime_ms: i64| Action::Upload {
            path: path.into(),
            existing: existing.then(|| Existing {
                id: "i".into(),
                mtime_ms: 0,
                md5: None,
            }),
            mtime_ms,
        };
        let down = |path: &str, mtime_ms: i64| Action::Download {
            path: path.into(),
            id: "i".into(),
            mtime_ms,
            md5: None,
            expected_local: None,
        };
        assert_eq!(
            action_line(&Action::Mkdir { path: "d/e".into() }, &local, &remote),
            "+ d/e/"
        );
        assert_eq!(
            action_line(&up("n.txt", false, 1), &local, &remote),
            "+ n.txt  7 B"
        );
        assert_eq!(
            action_line(&up("a b.txt", true, 5000), &local, &remote),
            "M a b.txt  1,500 B, local newer"
        );
        assert_eq!(
            action_line(&down("a b.txt", 1000), &local, &remote),
            "M a b.txt  10 B, forced: local is newer"
        );
        let same_time: BTreeMap<_, _> = [("a b.txt".to_string(), sized(5000, 10))]
            .into_iter()
            .collect();
        assert_eq!(
            action_line(&down("a b.txt", 5000), &local, &same_time),
            "M a b.txt  10 B, content differs, same mtime"
        );
    }

    #[test]
    fn parallel_preserves_order_and_uses_workers() {
        let seen = std::sync::Mutex::new(std::collections::HashSet::new());
        let out = parallel((0..100).collect(), 8, |i| {
            seen.lock().unwrap().insert(std::thread::current().id());
            std::thread::sleep(std::time::Duration::from_millis(2));
            i * 2
        });
        assert_eq!(out, (0..100).map(|i| i * 2).collect::<Vec<_>>());
        assert!(seen.lock().unwrap().len() > 1);
    }

    #[test]
    fn local_walk_scope_depth_and_exclusions() {
        let dir = tmpdir("walk");
        for d in ["a/b/c", ".gd", "nested/.gd", "node_modules/x", "empty"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        for f in [
            "top.txt",
            "a/one.txt",
            "a/b/two.log",
            "a/b/c/three.txt",
            ".gd/config.json",
            "nested/.gd/credentials.json",
            "nested/ok.txt",
            "node_modules/x/y.js",
            ".part.dsync-part",
        ] {
            std::fs::write(dir.join(f), b"hi").unwrap();
        }
        std::fs::write(dir.join(IGNORE_FILE), "*.log\nnode_modules/\n").unwrap();
        let ignore = load_ignore(&dir);
        let all = local_walk(&dir, &dir, -1, &ignore).unwrap();
        assert_eq!(
            all.keys().cloned().collect::<Vec<_>>(),
            vec![
                ".driveignore",
                "a",
                "a/b",
                "a/b/c",
                "a/b/c/three.txt",
                "a/one.txt",
                "empty",
                "nested",
                "nested/ok.txt",
                "top.txt"
            ],
            "nested .gd, ignored, and temp files are excluded"
        );
        assert_eq!(all["top.txt"].size, Some(2));
        assert_eq!(all["a"].size, None);
        // Depth is measured from the root on both sides: depth 1 from the root is top level only...
        let shallow = local_walk(&dir, &dir, 1, &ignore).unwrap();
        assert_eq!(
            shallow.keys().cloned().collect::<Vec<_>>(),
            vec![".driveignore", "a", "empty", "nested", "top.txt"]
        );
        // ...and a subtree at level 1 with depth 1 contributes only itself, never its children.
        let sub = local_walk(&dir, &dir.join("a"), 1, &ignore).unwrap();
        assert_eq!(sub.keys().cloned().collect::<Vec<_>>(), vec!["a"]);
        let sub2 = local_walk(&dir, &dir.join("a"), 2, &ignore).unwrap();
        assert_eq!(
            sub2.keys().cloned().collect::<Vec<_>>(),
            vec!["a", "a/b", "a/one.txt"]
        );
        assert!(
            local_walk(&dir, &dir.join("a/b/c"), 2, &ignore).is_err(),
            "selected path deeper than depth"
        );
        // The selected directory is part of its own snapshot, so an empty folder can be pushed.
        let empty = local_walk(&dir, &dir.join("empty"), -1, &ignore).unwrap();
        assert_eq!(empty.keys().cloned().collect::<Vec<_>>(), vec!["empty"]);
        assert!(empty["empty"].is_dir);
        let single = local_walk(&dir, &dir.join("a/one.txt"), -1, &ignore).unwrap();
        assert_eq!(
            single.keys().cloned().collect::<Vec<_>>(),
            vec!["a/one.txt"]
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("a"), dir.join("alias")).unwrap();
            assert!(
                local_walk(&dir, &dir.join("alias"), -1, &ignore).is_err(),
                "a symlinked selection is refused"
            );
            assert!(
                local_walk(&dir, &dir.join("alias/one.txt"), -1, &ignore)
                    .unwrap_err()
                    .to_string()
                    .contains("symlink"),
                "so is a selection below a symlinked ancestor"
            );
            assert!(is_excluded(&dir, &ignore, &dir.join("node_modules")));
            assert!(is_excluded(&dir, &ignore, &dir.join("a/b/two.log")));
            assert!(!is_excluded(&dir, &ignore, &dir.join("a")));
            assert!(!local_walk(&dir, &dir, -1, &ignore)
                .unwrap()
                .contains_key("alias"));
            if std::process::Command::new("mkfifo")
                .arg(dir.join("pipe"))
                .status()
                .is_ok_and(|s| s.success())
            {
                let with_fifo = local_walk(&dir, &dir, -1, &ignore).unwrap();
                assert!(
                    !with_fifo.contains_key("pipe"),
                    "non-regular files are never planned"
                );
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names_with_spaces_and_special_chars() {
        let dir = tmpdir("names");
        let names = [
            "My Documents/Q3 report (final).txt",
            "My Documents/sub folder/it's 100%_done.md",
            "  leading and trailing  /x.txt",
            "ünïcödé 日本語/naïve café.txt",
            "back\\slash/tab\there.txt",
            "#hash and $dollar/a&b=c.txt",
            "my logs/should be ignored.log",
        ];
        for n in names {
            let p = dir.join(n);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, n.as_bytes()).unwrap();
        }
        std::fs::write(dir.join(IGNORE_FILE), "my logs/\n").unwrap();
        let ignore = load_ignore(&dir);
        let local = local_walk(&dir, &dir, -1, &ignore).unwrap();
        for n in &names[..6] {
            assert!(local.contains_key(*n), "missing {n}");
            assert_eq!(local[*n].size, Some(n.len() as u64));
            assert_eq!(
                file_md5(&dir.join(n)).unwrap(),
                format!("{:x}", md5::compute(n.as_bytes()))
            );
        }
        assert!(!local.contains_key("my logs/should be ignored.log"));
        assert!(local["My Documents/sub folder"].is_dir);
        let sub = local_walk(&dir, &dir.join("My Documents/sub folder"), -1, &ignore).unwrap();
        assert_eq!(
            sub.keys().cloned().collect::<Vec<_>>(),
            vec![
                "My Documents/sub folder",
                "My Documents/sub folder/it's 100%_done.md"
            ]
        );
        let plan = plan_push(&local, &BTreeMap::new(), false, &none());
        assert!(plan.actions.contains(&Action::Mkdir {
            path: "My Documents/sub folder".into()
        }));
        assert_eq!(
            parent_of("My Documents/sub folder/it's 100%_done.md"),
            "My Documents/sub folder"
        );
        assert_eq!(parent_of("top level.txt"), "");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_unmappable_remote_names() {
        assert!(valid_name("normal name.txt"));
        assert!(valid_name("  spaces  "));
        assert!(!valid_name("a/b"));
        assert!(!valid_name("."));
        assert!(!valid_name(".."));
        assert!(!valid_name(""));
        assert!(is_reserved(".gd"));
        assert!(is_reserved(".gd/config.json"));
        assert!(is_reserved("x/.gd/y"));
        assert!(is_reserved("x/.y.dsync-part"));
        assert!(
            is_reserved("x.dsync-part/child.txt"),
            "reserved suffix applies to every component"
        );
        assert!(!is_reserved(".gdx/file"));
        assert!(!is_reserved("normal/.hidden"));
    }

    #[test]
    fn paths_with_spaces_and_dotdot() {
        assert_eq!(
            rel_path(Path::new("/r oot"), Path::new("/r oot/a b/c d.txt")).unwrap(),
            "a b/c d.txt"
        );
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(absolutize("a b/../c d").unwrap(), cwd.join("c d"));
        assert_eq!(absolutize("/x y/./z").unwrap(), PathBuf::from("/x y/z"));
        assert_eq!(ancestors("a/b/c").collect::<Vec<_>>(), vec!["a", "a/b"]);
        assert_eq!(ancestors("top").count(), 0);
    }

    #[test]
    fn rfc3339_roundtrip() {
        let ms = parse_rfc3339_ms("2026-09-08T10:20:30.123Z").unwrap();
        assert_eq!(fmt_ms(ms), "2026-09-08T10:20:30.123Z");
    }

    #[test]
    fn folders_to_verify_covers_ancestors_once_and_never_the_root() {
        let known = |p: &str| ["a", "a/b", "a/b/c", "x"].contains(&p);
        let parents = ["a/b/c", "a/b", "x", "", "a/new"];
        let out = folders_to_verify(parents.into_iter(), &known);
        assert_eq!(
            out.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["a", "a/b", "a/b/c", "x"]
        );
        // An unindexed parent is skipped, but its indexed ancestors are still checked.
        assert!(!out.contains("") && !out.contains("a/new"));
        assert_eq!(
            folders_to_verify(["a/new/deeper"].into_iter(), &known),
            BTreeSet::from(["a".into()])
        );
    }

    #[test]
    fn part_names_are_short_unique_and_reserved() {
        let long = "x".repeat(300);
        let a = part_name(&long);
        let b = part_name(&format!("{long}y"));
        assert!(a.len() <= 255 && a.ends_with(PART_SUFFIX) && a.starts_with('.'));
        assert_ne!(a, b, "names that share a prefix get different temp files");
        assert_eq!(part_name("f.txt"), part_name("f.txt"));
        assert!(is_reserved(&part_name("f.txt")));
        assert!(is_reserved(&format!("sub/{}", part_name("f.txt"))));
        // 40 multi-byte characters stay within the limit.
        let wide = "日".repeat(200);
        assert!(part_name(&wide).len() <= 255);
    }

    #[test]
    fn stale_part_files_are_removed_but_nothing_else() {
        let dir = tmpdir("parts");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join(".gd")).unwrap();
        std::fs::write(dir.join(part_name("a.txt")), b"partial").unwrap();
        std::fs::write(dir.join("sub").join(part_name("b.txt")), b"partial").unwrap();
        std::fs::write(dir.join(".gd").join(part_name("c")), b"keep").unwrap();
        std::fs::write(dir.join("a.txt"), b"real").unwrap();
        std::fs::write(dir.join("mine.dsync-part"), b"user data").unwrap();
        assert_eq!(remove_stale_parts(&dir, &dir), 2);
        assert!(dir.join("a.txt").exists());
        assert!(
            dir.join("mine.dsync-part").exists(),
            "only dsync's own temp names are deleted"
        );
        assert!(is_part_name(&part_name("x")) && !is_part_name("mine.dsync-part"));
        assert!(!is_part_name(".x.zzzzzzzz.dsync-part"));
        assert!(dir.join(".gd").join(part_name("c")).exists());
        assert_eq!(remove_stale_parts(&dir, &dir), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hash_stable_rejects_a_file_that_changed_while_read() {
        let dir = tmpdir("stable");
        let f = dir.join("f.txt");
        std::fs::write(&f, b"hello").unwrap();
        let meta = std::fs::metadata(&f).unwrap();
        let (size, mtime) = (meta.len(), mtime_ms_of(&meta));
        assert_eq!(
            hash_stable(&f, size, mtime).unwrap(),
            "5d41402abc4b2a76b9719d911017c592"
        );
        // The snapshot said 5 bytes; the file is now longer, so the hash is not trusted.
        std::fs::write(&f, b"hello world").unwrap();
        assert!(hash_stable(&f, size, mtime).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_directory_is_reported_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir("unreadable");
        std::fs::create_dir_all(dir.join("locked")).unwrap();
        std::fs::write(dir.join("locked/secret.txt"), b"s").unwrap();
        std::fs::write(dir.join("ok.txt"), b"o").unwrap();
        std::fs::set_permissions(dir.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let readable = std::fs::read_dir(dir.join("locked")).is_ok(); // true when running as root
        let ignore = load_ignore(&dir);
        let walk = local_walk(&dir, &dir, -1, &ignore);
        std::fs::set_permissions(dir.join("locked"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let walk = walk.unwrap();
        assert!(walk.contains_key("ok.txt"));
        if !readable {
            assert!(walk["locked"].unreadable && walk["locked"].is_dir);
            assert!(!walk.contains_key("locked/secret.txt"));
            let remote: BTreeMap<_, _> = [("locked".to_string(), e(1, None, true))]
                .into_iter()
                .collect();
            let plan = plan_push(&walk, &remote, false, &none());
            assert_eq!(plan.errors.len(), 1);
            assert!(plan.actions.iter().all(|a| a.path() != "locked"));
            let plan = plan_pull(&walk, &remote, false, &none());
            assert_eq!(plan.errors.len(), 1);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn join_and_rel() {
        assert_eq!(join_rel("", "a"), "a");
        assert_eq!(join_rel("a/b", "c"), "a/b/c");
        assert_eq!(
            rel_path(Path::new("/r"), Path::new("/r/x/y")).unwrap(),
            "x/y"
        );
        assert_eq!(rel_path(Path::new("/r"), Path::new("/r")).unwrap(), "");
        assert!(rel_path(Path::new("/r"), Path::new("/other")).is_err());
    }

    fn remote_entry(id: &str, is_dir: bool, size: u64) -> Entry {
        Entry {
            mtime_ms: 1_000_000,
            size: (!is_dir).then_some(size),
            md5: (!is_dir).then(|| "m".to_string()),
            is_dir,
            id: Some(id.into()),
            ..Default::default()
        }
    }

    #[test]
    fn deletions_cover_only_the_destination_side_deepest_first() {
        let no_root = PathBuf::from("/nonexistent/dsync-plan-test");
        let mut local = BTreeMap::new();
        let mut remote = BTreeMap::new();
        local.insert("keep.txt".to_string(), sized(1_000_000, 3));
        remote.insert("keep.txt".to_string(), remote_entry("K", false, 3));
        local.insert("mine.txt".to_string(), sized(1, 10));
        local.insert("dir".to_string(), e(1, None, true));
        local.insert("dir/a.txt".to_string(), sized(1, 20));
        remote.insert("theirs.txt".to_string(), remote_entry("T", false, 30));
        remote.insert("rdir".to_string(), remote_entry("RD", true, 0));
        remote.insert("rdir/b.txt".to_string(), remote_entry("RB", false, 40));
        let mut doc = remote_entry("DOC", false, 0);
        doc.native_doc = true;
        remote.insert("doc".to_string(), doc);

        let (push, skips) = plan_deletions(&no_root, &local, &remote, &none(), Side::Remote);
        let paths: Vec<&str> = push.iter().map(Delete::path).collect();
        assert_eq!(paths, ["rdir/b.txt", "rdir", "theirs.txt"]);
        assert!(
            matches!(&push[0], Delete::Remote { id, is_dir: false, md5: Some(m), .. } if id == "RB" && m == "m")
        );
        assert!(matches!(&push[1], Delete::Remote { is_dir: true, .. }));
        assert_eq!(skips.len(), 1);
        assert!(skips[0].0 == "doc" && skips[0].1.contains("Google-native"));

        let (pull, skips) = plan_deletions(&no_root, &local, &remote, &none(), Side::Local);
        let paths: Vec<&str> = pull.iter().map(Delete::path).collect();
        assert_eq!(paths, ["dir/a.txt", "dir", "mine.txt"]);
        assert!(matches!(
            &pull[0],
            Delete::Local {
                expected: Some((20, 1)),
                is_dir: false,
                ..
            }
        ));
        assert!(matches!(
            &pull[1],
            Delete::Local {
                expected: None,
                is_dir: true,
                ..
            }
        ));
        assert!(skips.is_empty());

        // A name that only collides by case is the same file under another spelling: never deleted.
        let collisions = BTreeSet::from(["theirs.txt".to_string(), "mine.txt".to_string()]);
        let (push, _) = plan_deletions(&no_root, &local, &remote, &collisions, Side::Remote);
        assert!(!push.iter().any(|d| d.path() == "theirs.txt"));
        let (pull, _) = plan_deletions(&no_root, &local, &remote, &collisions, Side::Local);
        assert!(!pull.iter().any(|d| d.path() == "mine.txt"));

        // Below a local folder that could not be read, remote entries are unknown, not absent.
        local.insert(
            "locked".to_string(),
            Entry {
                is_dir: true,
                unreadable: true,
                ..Default::default()
            },
        );
        remote.insert("locked".to_string(), remote_entry("L", true, 0));
        remote.insert("locked/x.txt".to_string(), remote_entry("LX", false, 5));
        let (push, skips) = plan_deletions(&no_root, &local, &remote, &none(), Side::Remote);
        assert!(!push.iter().any(|d| d.path().starts_with("locked")));
        assert!(skips
            .iter()
            .any(|(p, why)| p == "locked/x.txt" && why.contains("could not be read")));

        // A file on one side and a folder on the other: the folder's contents are not "missing".
        local.insert("notes".to_string(), sized(1, 9));
        remote.insert("notes".to_string(), remote_entry("N", true, 0));
        remote.insert("notes/a.txt".to_string(), remote_entry("NA", false, 1));
        remote.insert("notes/b".to_string(), remote_entry("NB", true, 0));
        remote.insert("notes/b/c.txt".to_string(), remote_entry("NC", false, 1));
        let (push, skips) = plan_deletions(&no_root, &local, &remote, &none(), Side::Remote);
        assert!(!push.iter().any(|d| d.path().starts_with("notes")));
        assert_eq!(
            skips
                .iter()
                .filter(|(_, why)| why.contains("folder on one side"))
                .count(),
            3
        );
        local.insert("mixed".to_string(), e(1, None, true));
        local.insert("mixed/deep.txt".to_string(), sized(1, 2));
        remote.insert("mixed".to_string(), remote_entry("M", false, 3));
        let (pull, skips) = plan_deletions(&no_root, &local, &remote, &none(), Side::Local);
        assert!(!pull.iter().any(|d| d.path().starts_with("mixed")));
        assert!(skips.iter().any(|(p, _)| p == "mixed/deep.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn remote_deletions_skip_what_exists_locally_outside_the_walk() {
        // The walk drops symlinked and ignored folders silently; their remote counterparts must
        // not look "remote only".
        let root = std::env::temp_dir().join(format!("dsync_unwalked_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(root.join("build/keep.txt"), b"k").unwrap();
        std::os::unix::fs::symlink(root.join("build"), root.join("media")).unwrap();
        let local = BTreeMap::new(); // both pruned from the snapshot
        let mut remote = BTreeMap::new();
        remote.insert("build".to_string(), remote_entry("B", true, 0));
        remote.insert("build/keep.txt".to_string(), remote_entry("BK", false, 1));
        remote.insert("media".to_string(), remote_entry("M", true, 0));
        remote.insert("media/a.txt".to_string(), remote_entry("MA", false, 1));
        remote.insert("really-gone.txt".to_string(), remote_entry("G", false, 1));
        let (push, skips) = plan_deletions(&root, &local, &remote, &none(), Side::Remote);
        let paths: Vec<&str> = push.iter().map(Delete::path).collect();
        assert_eq!(paths, ["really-gone.txt"]);
        assert_eq!(skips.len(), 4);
        assert!(skips
            .iter()
            .all(|(_, why)| why.contains("outside the sync")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn delete_guard_refuses_an_empty_source() {
        let mut plan = Plan::default();
        assert!(
            check_deletions(&plan, false).is_ok(),
            "nothing planned, nothing to refuse"
        );
        plan.deletions.push(Delete::Local {
            path: "a".into(),
            is_dir: false,
            expected: Some((1, 1)),
        });
        let err = check_deletions(&plan, false).unwrap_err().to_string();
        assert!(err.contains("source side is empty"), "{err}");
        assert!(check_deletions(&plan, true).is_ok());
    }

    #[test]
    fn deletion_lines_name_the_side_and_size() {
        let mut local = BTreeMap::new();
        let mut remote = BTreeMap::new();
        remote.insert("a/b.txt".to_string(), remote_entry("B", false, 1234));
        local.insert("c.txt".to_string(), sized(1, 5));
        let remote_file = Delete::Remote {
            path: "a/b.txt".into(),
            id: "B".into(),
            is_dir: false,
            mtime_ms: 0,
            md5: None,
        };
        let remote_dir = Delete::Remote {
            path: "a".into(),
            id: "A".into(),
            is_dir: true,
            mtime_ms: 0,
            md5: None,
        };
        let local_file = Delete::Local {
            path: "c.txt".into(),
            is_dir: false,
            expected: Some((5, 1)),
        };
        assert_eq!(
            delete_line(&remote_file, &local, &remote),
            "D a/b.txt  1,234 B, trash on Drive"
        );
        assert_eq!(
            delete_line(&remote_dir, &local, &remote),
            "D a/  trash on Drive"
        );
        assert_eq!(
            delete_line(&local_file, &local, &remote),
            "D c.txt  5 B, delete locally"
        );
    }

    /// On macOS this touches the real Trash, so it only runs on request: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn remove_local_removes_a_file_and_an_empty_folder() {
        let dir = std::env::temp_dir().join(format!("dsync_trash_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("dsync-trash-probe.txt");
        std::fs::write(&file, b"probe").unwrap();
        remove_local(&file).unwrap();
        assert!(!file.exists());
        remove_local(&dir).unwrap();
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn local_deletions_verify_each_target_and_leave_folders_with_content() {
        let root = std::env::temp_dir().join(format!("dsync_delete_local_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        std::fs::write(root.join("gone.txt"), b"x").unwrap();
        std::fs::write(root.join("changed.txt"), b"yy").unwrap();
        std::fs::write(root.join("sub/inner.txt"), b"zzz").unwrap();
        std::fs::write(root.join("sub/.DS_Store"), b"").unwrap(); // ignored, so not in the plan
        std::os::unix::fs::symlink(root.join("gone.txt"), root.join("link.txt")).unwrap();
        let stat = |name: &str| {
            let m = std::fs::metadata(root.join(name)).unwrap();
            (m.len(), mtime_ms_of(&m))
        };
        let file = |path: &str, expected: Option<(u64, i64)>| Delete::Local {
            path: path.into(),
            is_dir: false,
            expected,
        };
        let dir = |path: &str| Delete::Local {
            path: path.into(),
            is_dir: true,
            expected: None,
        };
        let (size, mtime) = stat("changed.txt");
        let deletions = vec![
            dir("sub"),
            dir("empty"),
            dir("never-existed"),
            file("gone.txt", Some(stat("gone.txt"))),
            file("sub/inner.txt", Some(stat("sub/inner.txt"))),
            file("changed.txt", Some((size, mtime + 5000))), // stale plan
            file("link.txt", Some((1, 0))),                  // symlink, never followed
        ];
        // Plain removal stands in for the macOS Trash, which the test must not pollute.
        let remove = |p: &Path| -> Result<()> {
            if p.is_dir() {
                std::fs::remove_dir(p)?;
            } else {
                std::fs::remove_file(p)?;
            }
            Ok(())
        };
        let tally = exec_delete_local(&root, deletions, &remove);
        assert_eq!(tally.failed, 2, "the stale file and the symlink fail");
        assert_eq!(
            tally.skipped, 2,
            "the missing folder and the folder with content"
        );
        assert!(!root.join("gone.txt").exists());
        assert!(!root.join("sub/inner.txt").exists());
        assert!(!root.join("empty").exists());
        assert!(root.join("changed.txt").exists(), "stale stat: untouched");
        assert!(
            std::fs::symlink_metadata(root.join("link.txt")).is_ok(),
            "symlink: untouched"
        );
        assert!(
            root.join("sub").is_dir(),
            "folder with an ignored file is kept"
        );
        assert!(root.join("sub/.DS_Store").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
