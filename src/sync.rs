//! Local walking, remote/local comparison, and the push/pull engines.
use crate::config::{GD_DIR, IGNORE_FILE};
use crate::drive::Drive;
use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use ignore::gitignore::Gitignore;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
    Ok(rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/"))
}

pub fn load_ignore(root: &Path) -> Gitignore {
    let (gi, _) = Gitignore::new(root.join(IGNORE_FILE));
    gi
}

fn is_ignored(root: &Path, ignore: &Gitignore, path: &Path, is_dir: bool) -> bool {
    path.strip_prefix(root)
        .ok()
        .and_then(|r| r.components().next())
        .map_or(false, |c| c.as_os_str() == GD_DIR)
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
    for entry in walker
        .into_iter()
        .filter_entry(|e| !is_ignored(root, ignore, e.path(), e.file_type().is_dir()))
    {
        let entry = entry?;
        let meta = entry.metadata()?;
        let mtime_ms = meta
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let md5 = if with_md5 && meta.is_file() {
            Some(format!("{:x}", md5::compute(std::fs::read(entry.path())?)))
        } else {
            None
        };
        out.insert(
            rel_path(root, entry.path())?,
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
    pub fn path(&self) -> &str {
        match self {
            Action::Mkdir { path }
            | Action::Upload { path, .. }
            | Action::Download { path, .. } => path,
        }
    }
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

/// Execute a push plan: folders first (sequentially), then file uploads in parallel.
/// `remote` is updated with the resulting files so it can be cached. Returns the number of failures.
pub fn exec_push(
    drive: &Drive,
    root: &Path,
    base_rel: &str,
    base_id: &str,
    actions: Vec<Action>,
    remote: &mut BTreeMap<String, Entry>,
    threads: usize,
) -> Result<usize> {
    let mut folder_ids: BTreeMap<String, String> = remote
        .iter()
        .filter(|(_, e)| e.is_dir)
        .filter_map(|(p, e)| Some((p.clone(), e.id.clone()?)))
        .collect();
    folder_ids.insert(base_rel.to_string(), base_id.to_string());
    let mut uploads = Vec::new();
    for a in actions {
        let parent_id = folder_ids
            .get(parent_of(a.path()))
            .cloned()
            .with_context(|| format!("no remote parent folder for {}", a.path()))?;
        match a {
            Action::Mkdir { path } => {
                let f = drive.create_folder(&parent_id, path.rsplit('/').next().unwrap())?;
                println!("+ mkdir    {path}/");
                folder_ids.insert(path.clone(), f.id.clone());
                remote.insert(path, f.to_entry());
            }
            Action::Upload {
                path,
                existing_id,
                mtime_ms,
            } => uploads.push((path, parent_id, existing_id, mtime_ms)),
            Action::Download { .. } => unreachable!(),
        }
    }
    let results = parallel(
        uploads,
        threads,
        |(path, parent_id, existing_id, mtime_ms)| {
            let result = std::fs::read(root.join(&path))
                .map_err(anyhow::Error::from)
                .and_then(|data| {
                    drive.upload(
                        &parent_id,
                        path.rsplit('/').next().unwrap(),
                        existing_id.as_deref(),
                        data,
                        &fmt_ms(mtime_ms),
                    )
                });
            match &result {
                Ok(_) => println!(
                    "^ {:<8} {path}",
                    if existing_id.is_some() {
                        "updated"
                    } else {
                        "uploaded"
                    }
                ),
                Err(e) => eprintln!("x failed   {path}: {e:#}"),
            }
            (path, result)
        },
    );
    let mut failures = 0;
    for (path, r) in results {
        match r {
            Ok(f) => {
                remote.insert(path, f.to_entry());
            }
            Err(_) => failures += 1,
        }
    }
    Ok(failures)
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
                println!("+ mkdir    {path}/");
            }
            Action::Download { path, id, mtime_ms } => downloads.push((path, id, mtime_ms)),
            Action::Upload { .. } => unreachable!(),
        }
    }
    let results = parallel(downloads, threads, |(path, id, mtime_ms)| {
        let dest = root.join(&path);
        let result = drive.download(&id).and_then(|data| {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, data)?;
            filetime::set_file_mtime(
                &dest,
                filetime::FileTime::from_unix_time(
                    mtime_ms.div_euclid(1000),
                    (mtime_ms.rem_euclid(1000) * 1_000_000) as u32,
                ),
            )?;
            Ok(())
        });
        match &result {
            Ok(()) => println!("v downloaded {path}"),
            Err(e) => eprintln!("x failed   {path}: {e:#}"),
        }
        result.is_err()
    });
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
