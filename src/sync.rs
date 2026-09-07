// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ScaleNinja
// DriveSync (dsync) — https://github.com/scaleninja/drivesync

//! Local walking, remote/local comparison, and the push/pull engines.
use crate::cache::Cache;
use crate::config::{GD_DIR, IGNORE_FILE};
use crate::drive::Drive;
use crate::progress::{self, Spinner};
use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use ignore::gitignore::Gitignore;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use walkdir::WalkDir;

/// Clock skew tolerated before two mtimes are considered different.
const MTIME_TOLERANCE_MS: i64 = 1000;

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub mtime_ms: i64,
    pub md5: Option<String>,
    pub is_dir: bool,
    pub id: Option<String>,
    pub native_doc: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    LocalOnly,
    RemoteOnly,
    LocalNewer,
    RemoteNewer,
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

/// Drive allows names that cannot be mapped onto a local path ("/", ".", "..", empty).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

pub fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
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
    let (gi, _) = Gitignore::new(root.join(IGNORE_FILE));
    gi
}

pub const PART_SUFFIX: &str = ".dsync-part";

fn is_ignored(root: &Path, ignore: &Gitignore, path: &Path, is_dir: bool) -> bool {
    if path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(PART_SUFFIX))
    {
        return true; // leftover from an interrupted download
    }
    path.strip_prefix(root)
        .ok()
        .and_then(|r| r.components().next())
        .is_some_and(|c| c.as_os_str() == GD_DIR)
        || ignore.matched_path_or_any_parents(path, is_dir).is_ignore()
}

/// Walk the local subtree at `base` (an absolute path under `root`), keyed by path relative to `root`.
pub fn local_walk(
    root: &Path,
    base: &Path,
    depth: i32,
    ignore: &Gitignore,
    with_md5: bool,
) -> Result<BTreeMap<String, Entry>> {
    let mut out = BTreeMap::new();
    if !base.exists() {
        return Ok(out);
    }
    let mut walker = WalkDir::new(base).min_depth(usize::from(base.is_dir()));
    if depth >= 0 && base.is_dir() {
        walker = walker.max_depth(depth as usize);
    }
    for entry in walker.into_iter().filter_entry(|e| {
        !e.file_type().is_symlink() && !is_ignored(root, ignore, e.path(), e.file_type().is_dir())
    }) {
        let entry = entry?;
        let meta = entry.metadata()?;
        let mtime_ms = meta
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let md5 = if with_md5 && meta.is_file() {
            Some(file_md5(entry.path())?)
        } else {
            None
        };
        let rel = match rel_path(root, entry.path()) {
            Ok(rel) => rel,
            Err(e) => {
                progress::eprintln(&format!("! skip     {e:#}"));
                continue;
            }
        };
        out.insert(
            rel,
            Entry {
                mtime_ms,
                md5,
                is_dir: meta.is_dir(),
                id: None,
                native_doc: false,
            },
        );
    }
    Ok(out)
}

/// MD5 of a file, streamed in 64 KiB chunks so large files are not read into memory.
pub fn file_md5(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut ctx = md5::Context::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(format!("{:x}", ctx.compute()));
        }
        ctx.consume(&buf[..n]);
    }
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
            Some(r) if l.is_dir || r.is_dir => {}
            Some(r) if l.md5.is_some() && l.md5 == r.md5 => {}
            Some(r) if l.mtime_ms > r.mtime_ms + MTIME_TOLERANCE_MS => {
                out.push((path.clone(), Change::LocalNewer))
            }
            Some(r) if r.mtime_ms > l.mtime_ms + MTIME_TOLERANCE_MS => {
                out.push((path.clone(), Change::RemoteNewer))
            }
            Some(_) => {}
        }
    }
    for path in remote.keys().filter(|p| !local.contains_key(*p)) {
        out.push((path.clone(), Change::RemoteOnly));
    }
    out.sort();
    out
}

/// One unit of work produced by planning and executed after confirmation.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Mkdir {
        path: String,
    },
    Upload {
        path: String,
        existing_id: Option<String>,
        mtime_ms: i64,
    },
    Download {
        path: String,
        id: String,
        mtime_ms: i64,
    },
}

impl Action {
    pub fn describe(&self) -> String {
        match self {
            Action::Mkdir { path } => format!("+ mkdir    {path}/"),
            Action::Upload {
                path,
                existing_id: None,
                ..
            } => format!("^ upload   {path}"),
            Action::Upload { path, .. } => format!("^ update   {path}"),
            Action::Download { path, .. } => format!("v download {path}"),
        }
    }
}

/// Decide whether a file needs transferring from `src` to `dst`. Returns Some(true) when `dst` exists.
fn needs_transfer(
    src: &Entry,
    dst: Option<&Entry>,
    force: bool,
    path: &str,
    other: &str,
    skips: &mut Vec<String>,
) -> Option<bool> {
    match dst {
        None => Some(false),
        Some(d) if d.is_dir => {
            skips.push(format!("{path} ({other} is a folder)"));
            None
        }
        Some(d) if src.md5.is_some() && src.md5 == d.md5 => None,
        Some(d) if !force && d.mtime_ms > src.mtime_ms + MTIME_TOLERANCE_MS => {
            skips.push(format!("{path} ({other} is newer; use --force)"));
            None
        }
        Some(d) if !force && (src.mtime_ms - d.mtime_ms).abs() <= MTIME_TOLERANCE_MS => None,
        Some(_) => Some(true),
    }
}

/// Plan a push. Returns the actions and human-readable skip reasons.
pub fn plan_push(
    local: &BTreeMap<String, Entry>,
    remote: &BTreeMap<String, Entry>,
    force: bool,
) -> (Vec<Action>, Vec<String>) {
    let (mut actions, mut skips) = (Vec::new(), Vec::new());
    for (path, l) in local {
        if l.is_dir {
            match remote.get(path) {
                None => actions.push(Action::Mkdir { path: path.clone() }),
                Some(r) if !r.is_dir => {
                    skips.push(format!("{path} (remote is a file, local is a folder)"))
                }
                Some(_) => {}
            }
        } else if let Some(exists) =
            needs_transfer(l, remote.get(path), force, path, "remote", &mut skips)
        {
            let existing_id = if exists {
                remote.get(path).and_then(|r| r.id.clone())
            } else {
                None
            };
            actions.push(Action::Upload {
                path: path.clone(),
                existing_id,
                mtime_ms: l.mtime_ms,
            });
        }
    }
    (actions, skips)
}

/// Plan a pull.
pub fn plan_pull(
    local: &BTreeMap<String, Entry>,
    remote: &BTreeMap<String, Entry>,
    force: bool,
) -> (Vec<Action>, Vec<String>) {
    let (mut actions, mut skips) = (Vec::new(), Vec::new());
    for (path, r) in remote {
        if r.is_dir {
            match local.get(path) {
                None => actions.push(Action::Mkdir { path: path.clone() }),
                Some(l) if !l.is_dir => {
                    skips.push(format!("{path} (local is a file, remote is a folder)"))
                }
                Some(_) => {}
            }
        } else if r.native_doc {
            skips.push(format!(
                "{path} (Google-native document; export not supported)"
            ));
        } else if needs_transfer(r, local.get(path), force, path, "local", &mut skips).is_some() {
            actions.push(Action::Download {
                path: path.clone(),
                id: r.id.clone().unwrap_or_default(),
                mtime_ms: r.mtime_ms,
            });
        }
    }
    (actions, skips)
}

/// Print the plan and ask for confirmation. Returns false if there is nothing to do or the user declined.
pub fn confirm(actions: &[Action], skips: &[String], no_prompt: bool) -> Result<bool> {
    for s in skips {
        println!("! skip     {s}");
    }
    if actions.is_empty() {
        println!("Everything is up to date.");
        return Ok(false);
    }
    for a in actions {
        println!("{}", a.describe());
    }
    let (dirs, files) = actions
        .iter()
        .partition::<Vec<_>, _>(|a| matches!(a, Action::Mkdir { .. }));
    println!("\n{} folder(s), {} file(s)", dirs.len(), files.len());
    if no_prompt {
        return Ok(true);
    }
    print!("Proceed with the changes? [Y/n]: ");
    std::io::Write::flush(&mut std::io::stdout())?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer.is_empty() || answer == "y" || answer == "yes")
}

/// Run `f` over `items` on up to `threads` worker threads; results keep input order.
fn parallel<T: Send, R: Send>(items: Vec<T>, threads: usize, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let queue = std::sync::Mutex::new(items.into_iter().enumerate());
    let results = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| loop {
                let Some((i, item)) = queue.lock().unwrap().next() else {
                    break;
                };
                let r = f(item);
                results.lock().unwrap().push((i, r));
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|(i, _)| *i);
    results.into_iter().map(|(_, r)| r).collect()
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

/// Execute a push plan. Folders are created level by level, each level in parallel; then file
/// uploads run in parallel. Every completed action is recorded in the cache immediately, so a
/// cancelled push resumes cleanly. Returns the number of failures.
pub fn exec_push(
    drive: &Drive,
    cache: &Cache,
    root: &Path,
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

    // Phase 1: folders, grouped by depth. Every folder at one level has its parent from the level above.
    let mut levels: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut uploads = Vec::new();
    for a in actions {
        match a {
            Action::Mkdir { path } => levels
                .entry(path.matches('/').count())
                .or_default()
                .push(path),
            Action::Upload {
                path,
                existing_id,
                mtime_ms,
            } => uploads.push((path, existing_id, mtime_ms)),
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
                            "x failed   {path}/: parent folder was not created"
                        ));
                        failures += 1;
                    }
                }
            }
            let results = parallel(batch, threads, |(path, parent_id)| {
                let result = drive.create_folder(&parent_id, path.rsplit('/').next().unwrap());
                match &result {
                    Ok(f) => {
                        if let Err(e) = cache.upsert(&path, &f.to_entry()) {
                            progress::eprintln(&format!(
                                "warning: cache update failed for {path}: {e:#}"
                            ));
                        }
                        progress::println(&format!("+ mkdir    {path}/"));
                    }
                    Err(e) => progress::eprintln(&format!("x failed   {path}/: {e:#}")),
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
    for (path, existing_id, mtime_ms) in uploads {
        match folder_ids.get(parent_of(&path)) {
            Some(parent_id) => jobs.push((path, parent_id.clone(), existing_id, mtime_ms)),
            None => {
                progress::eprintln(&format!("x failed   {path}: parent folder was not created"));
                failures += 1;
            }
        }
    }
    let total = jobs.len();
    let done = AtomicUsize::new(0);
    let spinner = Spinner::start(&format!("Uploading 0/{total} ({threads} streams)"));
    let results = parallel(jobs, threads, |(path, parent_id, existing_id, mtime_ms)| {
        let result = drive.upload(
            &parent_id,
            path.rsplit('/').next().unwrap(),
            existing_id.as_deref(),
            &root.join(&path),
            &fmt_ms(mtime_ms),
        );
        match &result {
            Ok(f) => {
                if let Err(e) = cache.upsert(&path, &f.to_entry()) {
                    progress::eprintln(&format!("warning: cache update failed for {path}: {e:#}"));
                }
                progress::println(&format!(
                    "^ {:<8} {path}",
                    if existing_id.is_some() {
                        "updated"
                    } else {
                        "uploaded"
                    }
                ));
            }
            Err(e) => progress::eprintln(&format!("x failed   {path}: {e:#}")),
        }
        spinner.set(format!(
            "Uploading {}/{total} ({threads} streams)",
            done.fetch_add(1, Ordering::SeqCst) + 1
        ));
        result.is_err()
    });
    spinner.finish();
    Ok(failures + results.into_iter().filter(|failed| *failed).count())
}

/// Execute a pull plan: local folders first, then downloads in parallel. Returns the number of failures.
pub fn exec_pull(
    drive: &Drive,
    root: &Path,
    actions: Vec<Action>,
    threads: usize,
) -> Result<usize> {
    let mut downloads = Vec::new();
    for a in actions {
        match a {
            Action::Mkdir { path } => {
                std::fs::create_dir_all(root.join(&path))?;
                progress::println(&format!("+ mkdir    {path}/"));
            }
            Action::Download { path, id, mtime_ms } => downloads.push((path, id, mtime_ms)),
            Action::Upload { .. } => unreachable!(),
        }
    }
    let total = downloads.len();
    let done = AtomicUsize::new(0);
    let spinner = Spinner::start(&format!("Downloading 0/{total} ({threads} streams)"));
    let results = parallel(downloads, threads, |(path, id, mtime_ms)| {
        let dest = root.join(&path);
        let result = (|| -> Result<()> {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            drive.download_to(&id, &dest)?;
            filetime::set_file_mtime(
                &dest,
                filetime::FileTime::from_unix_time(
                    mtime_ms.div_euclid(1000),
                    (mtime_ms.rem_euclid(1000) * 1_000_000) as u32,
                ),
            )?;
            Ok(())
        })();
        match &result {
            Ok(()) => progress::println(&format!("v downloaded {path}")),
            Err(e) => progress::eprintln(&format!("x failed   {path}: {e:#}")),
        }
        spinner.set(format!(
            "Downloading {}/{total} ({threads} streams)",
            done.fetch_add(1, Ordering::SeqCst) + 1
        ));
        result.is_err()
    });
    spinner.finish();
    Ok(results.into_iter().filter(|failed| *failed).count())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(mtime_ms: i64, md5: Option<&str>, is_dir: bool) -> Entry {
        Entry {
            mtime_ms,
            md5: md5.map(String::from),
            is_dir,
            id: None,
            native_doc: false,
        }
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
        ]
        .into_iter()
        .collect();
        let remote: BTreeMap<_, _> = [
            ("a.txt".to_string(), e(1000, Some("x2"), false)),
            ("b.txt".to_string(), e(5000, Some("y2"), false)),
            ("same.txt".to_string(), e(1, Some("z"), false)), // md5 equal wins over mtime
            ("tol.txt".to_string(), e(1000, Some("q2"), false)), // within tolerance
            ("only_remote.txt".to_string(), e(1, None, false)),
            ("dir".to_string(), e(123, None, true)),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            diff(&local, &remote),
            vec![
                ("a.txt".to_string(), Change::LocalNewer),
                ("b.txt".to_string(), Change::RemoteNewer),
                ("only_local.txt".to_string(), Change::LocalOnly),
                ("only_remote.txt".to_string(), Change::RemoteOnly),
            ]
        );
    }

    #[test]
    fn plans_push_and_pull() {
        let local: BTreeMap<_, _> = [
            ("d".to_string(), e(0, None, true)),
            ("d/new.txt".to_string(), e(5000, Some("a"), false)),
            ("newer.txt".to_string(), e(9000, Some("b"), false)),
            ("older.txt".to_string(), e(1000, Some("c"), false)),
            ("same.txt".to_string(), e(1000, Some("s"), false)),
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
        ]
        .into_iter()
        .collect();
        let (actions, skips) = plan_push(&local, &remote, false);
        assert_eq!(
            actions,
            vec![
                Action::Mkdir { path: "d".into() },
                Action::Upload {
                    path: "d/new.txt".into(),
                    existing_id: None,
                    mtime_ms: 5000
                },
                Action::Upload {
                    path: "newer.txt".into(),
                    existing_id: Some("id1".into()),
                    mtime_ms: 9000
                },
            ]
        );
        assert_eq!(skips, vec!["older.txt (remote is newer; use --force)"]);
        let (forced, _) = plan_push(&local, &remote, true);
        assert_eq!(forced.len(), 4);

        let (actions, skips) = plan_pull(&local, &remote, false);
        assert_eq!(
            actions,
            vec![
                Action::Download {
                    path: "older.txt".into(),
                    id: "id2".into(),
                    mtime_ms: 9000
                },
                Action::Mkdir {
                    path: "rdir".into()
                },
                Action::Download {
                    path: "rdir/r.txt".into(),
                    id: "id5".into(),
                    mtime_ms: 1
                },
            ]
        );
        assert_eq!(skips, vec!["newer.txt (local is newer; use --force)"]);
        remote.get_mut("rdir/r.txt").unwrap().native_doc = true;
        assert_eq!(plan_pull(&local, &remote, false).1.len(), 2);
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
    fn names_with_spaces_and_special_chars() {
        let dir = std::env::temp_dir().join(format!("dsync_names_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
        let local = local_walk(&dir, &dir, -1, &ignore, true).unwrap();
        for n in &names[..6] {
            assert!(local.contains_key(*n), "missing {n}");
            assert_eq!(
                local[*n].md5.as_deref(),
                Some(format!("{:x}", md5::compute(n.as_bytes())).as_str())
            );
        }
        assert!(!local.contains_key("my logs/should be ignored.log"));
        assert!(local["My Documents/sub folder"].is_dir);
        // A subtree walk from a spaced directory keeps the full relative path.
        let sub = local_walk(
            &dir,
            &dir.join("My Documents/sub folder"),
            -1,
            &ignore,
            false,
        )
        .unwrap();
        assert_eq!(
            sub.keys().cloned().collect::<Vec<_>>(),
            vec!["My Documents/sub folder/it's 100%_done.md"]
        );
        // Planning and parent resolution keep the spaced components intact.
        let remote = BTreeMap::new();
        let (actions, _) = plan_push(&local, &remote, false);
        let mkdirs: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, Action::Mkdir { .. }))
            .map(|a| a.describe())
            .collect();
        assert!(mkdirs.contains(&"+ mkdir    My Documents/sub folder/".to_string()));
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
    }

    #[test]
    fn rfc3339_roundtrip() {
        let ms = parse_rfc3339_ms("2026-09-08T10:20:30.123Z").unwrap();
        assert_eq!(fmt_ms(ms), "2026-09-08T10:20:30.123Z");
    }

    #[test]
    fn local_walk_respects_ignore_depth_and_gd() {
        let dir = std::env::temp_dir().join(format!("drive_rs_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["a/b/c", ".gd", "node_modules/x"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        for f in [
            "top.txt",
            "a/one.txt",
            "a/b/two.log",
            "a/b/c/three.txt",
            ".gd/config.json",
            "node_modules/x/y.js",
        ] {
            std::fs::write(dir.join(f), b"hi").unwrap();
        }
        std::fs::write(dir.join(IGNORE_FILE), "*.log\nnode_modules/\n").unwrap();
        let ignore = load_ignore(&dir);
        let all = local_walk(&dir, &dir, -1, &ignore, true).unwrap();
        assert_eq!(
            all.keys().cloned().collect::<Vec<_>>(),
            vec![
                ".driveignore",
                "a",
                "a/b",
                "a/b/c",
                "a/b/c/three.txt",
                "a/one.txt",
                "top.txt"
            ]
        );
        assert_eq!(
            all["top.txt"].md5.as_deref(),
            Some("49f68a5c8493ec2c0bf489821c21fc3b")
        );
        let shallow = local_walk(&dir, &dir, 1, &ignore, false).unwrap();
        assert_eq!(
            shallow.keys().cloned().collect::<Vec<_>>(),
            vec![".driveignore", "a", "top.txt"]
        );
        let single = local_walk(&dir, &dir.join("a/one.txt"), -1, &ignore, false).unwrap();
        assert_eq!(
            single.keys().cloned().collect::<Vec<_>>(),
            vec!["a/one.txt"]
        );
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
}
