// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ScaleNinja
// DriveSync (dsync) — https://github.com/scaleninja/drivesync

//! Local walking, local/remote comparison, diff-style rendering, and the push/pull engines.
//!
//! Comparison strategy (rsync/rclone style): equal MD5 means identical; differing sizes mean
//! different; equal size and equal mtime (within tolerance) is assumed identical without hashing;
//! only equal size with differing mtimes needs a local hash, which is cached keyed by stat.
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
pub const PART_SUFFIX: &str = ".dsync-part";

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Entry {
    pub mtime_ms: i64,
    pub size: Option<u64>,
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
    /// Content differs but the modification times are equal.
    Modified,
}

impl Change {
    pub fn label(self) -> &'static str {
        match self {
            Change::LocalOnly => "local only",
            Change::RemoteOnly => "remote only",
            Change::LocalNewer => "local newer",
            Change::RemoteNewer => "remote newer",
            Change::Modified => "content differs, same mtime",
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

pub fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
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
/// Only stat information is collected; hashes are filled in later, and only where needed.
pub fn local_walk(
    root: &Path,
    base: &Path,
    depth: i32,
    ignore: &Gitignore,
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
        let rel = match rel_path(root, entry.path()) {
            Ok(rel) => rel,
            Err(e) => {
                progress::eprintln(&format!("! skip     {e:#}"));
                continue;
            }
        };
        let mtime_ms = meta
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        out.insert(
            rel,
            Entry {
                mtime_ms,
                size: meta.is_file().then_some(meta.len()),
                md5: None,
                is_dir: meta.is_dir(),
                id: None,
                native_doc: false,
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

/// Compute (or fetch from the stat-keyed cache) the MD5 of every local file whose comparison
/// depends on it. Hashing runs on `threads` workers; results are stored back into the cache.
pub fn fill_hashes(
    root: &Path,
    cache: &Cache,
    local: &mut BTreeMap<String, Entry>,
    remote: &BTreeMap<String, Entry>,
    fast: bool,
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
        let result = match cache.local_hash(&path, size, mtime_ms) {
            Ok(Some(md5)) => Ok(md5),
            _ => file_md5(&root.join(&path)).inspect(|md5| {
                if let Err(e) = cache.set_local_hash(&path, size, mtime_ms, md5) {
                    progress::eprintln(&format!(
                        "warning: hash cache update failed for {path}: {e:#}"
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
        match r {
            Ok(md5) => local.get_mut(&path).expect("hashed path exists").md5 = Some(md5),
            Err(e) => progress::eprintln(&format!("warning: could not hash {path}: {e:#}")),
        }
    }
    Ok(())
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
        md5: Option<String>,
    },
}

/// A path that was deliberately left alone, with the reason.
pub type Skip = (String, String);

/// The result of planning: what to do, what was skipped, and what must not be overwritten.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub skips: Vec<Skip>,
    /// Destination is newer than the source, or content differs with equal mtimes. Never
    /// transferred unless `--force` was given (in which case they become actions instead).
    pub conflicts: Vec<Skip>,
}

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
) -> Plan {
    let mut plan = Plan::default();
    for (path, l) in local {
        if l.is_dir {
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
            let existing_id = if exists {
                remote.get(path).and_then(|r| r.id.clone())
            } else {
                None
            };
            plan.actions.push(Action::Upload {
                path: path.clone(),
                existing_id,
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
) -> Plan {
    let mut plan = Plan::default();
    for (path, r) in remote {
        if r.is_dir {
            match local.get(path) {
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
        } else if needs_transfer(r, local.get(path), force, path, "local", &mut plan).is_some() {
            plan.actions.push(Action::Download {
                path: path.clone(),
                id: r.id.clone().unwrap_or_default(),
                mtime_ms: r.mtime_ms,
                md5: r.md5.clone(),
            });
        }
    }
    plan
}

// ---------------------------------------------------------------------------------------------
// Listing: one line per path in the style of odeke-em/drive.
//   + added   M modified   - remote only   ! skipped   C conflict

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
    let size_of = |a: &Action| match a {
        Action::Upload { path, .. } => local.get(path).and_then(|e| e.size).unwrap_or(0),
        Action::Download { path, .. } => remote.get(path).and_then(|e| e.size).unwrap_or(0),
        Action::Mkdir { .. } => 0,
    };
    let is_add = |a: &Action| match a {
        Action::Mkdir { .. } => true,
        Action::Upload { existing_id, .. } => existing_id.is_none(),
        Action::Download { path, .. } => !local.contains_key(path),
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
    if !plan.skips.is_empty() {
        println!("Skip count {}", plan.skips.len());
    }
    if !plan.conflicts.is_empty() {
        println!("Conflict count {}", plan.conflicts.len());
        println!("Conflicts are never overwritten: both sides differ and the destination is not older.\nResolve them manually (see `dsync diff`), or re-run with --force to overwrite.");
        if no_prompt {
            anyhow::bail!("{} conflict(s); refusing to proceed without a prompt (resolve manually or use --force)", plan.conflicts.len());
        }
    }
    if plan.actions.is_empty() {
        println!(
            "{}",
            if plan.skips.is_empty() && plan.conflicts.is_empty() {
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
    let (question, default_yes) = if plan.conflicts.is_empty() {
        ("Proceed with the changes? [Y/n]: ", true)
    } else {
        (
            "Proceed with the non-conflicting changes only? [y/N]: ",
            false,
        )
    };
    print!("{question}");
    std::io::Write::flush(&mut std::io::stdout())?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(if answer.is_empty() {
        default_yes
    } else {
        answer == "y" || answer == "yes"
    })
}

/// One line per differing path for `dsync diff`, with both modification times.
pub fn diff_line(
    path: &str,
    change: Change,
    local: Option<&Entry>,
    remote: Option<&Entry>,
) -> String {
    let stamp = |e: Option<&Entry>| e.map(|e| fmt_ms(e.mtime_ms)).unwrap_or_else(|| "-".into());
    match change {
        Change::LocalOnly => line(
            "+",
            path,
            &format!("local only, {}", fmt_size(local.and_then(|e| e.size))),
        ),
        Change::RemoteOnly => line(
            "-",
            path,
            &format!("remote only, {}", fmt_size(remote.and_then(|e| e.size))),
        ),
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
// Execution

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
                if let (Some(md5), Ok(meta)) =
                    (&f.md5_checksum, std::fs::metadata(root.join(&path)))
                {
                    let _ = cache.set_local_hash(&path, meta.len(), mtime_ms, md5);
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
    cache: &Cache,
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
            Action::Download {
                path,
                id,
                mtime_ms,
                md5,
            } => downloads.push((path, id, mtime_ms, md5)),
            Action::Upload { .. } => unreachable!(),
        }
    }
    let total = downloads.len();
    let done = AtomicUsize::new(0);
    let spinner = Spinner::start(&format!("Downloading 0/{total} ({threads} streams)"));
    let results = parallel(downloads, threads, |(path, id, mtime_ms, md5)| {
        let dest = root.join(&path);
        let result = (|| -> Result<()> {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            drive.download_to(&id, &dest, md5.as_deref())?;
            filetime::set_file_mtime(
                &dest,
                filetime::FileTime::from_unix_time(
                    mtime_ms.div_euclid(1000),
                    (mtime_ms.rem_euclid(1000) * 1_000_000) as u32,
                ),
            )?;
            if let (Some(md5), Ok(meta)) = (&md5, std::fs::metadata(&dest)) {
                let _ = cache.set_local_hash(&path, meta.len(), mtime_ms, md5); // next diff needs no read
            }
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
            size: (!is_dir).then_some(10),
            md5: md5.map(String::from),
            is_dir,
            id: None,
            native_doc: false,
        }
    }
    fn sized(mtime_ms: i64, size: u64) -> Entry {
        Entry {
            mtime_ms,
            size: Some(size),
            ..Default::default()
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
            ("size_differs.txt".to_string(), sized(1000, 10)),
            ("size_same_time_same.txt".to_string(), sized(1000, 10)),
            ("modified.txt".to_string(), e(1000, Some("m1"), false)),
        ]
        .into_iter()
        .collect();
        let remote: BTreeMap<_, _> = [
            ("a.txt".to_string(), e(1000, Some("x2"), false)),
            ("b.txt".to_string(), e(5000, Some("y2"), false)),
            ("same.txt".to_string(), e(1, Some("z"), false)), // md5 equal wins over mtime
            ("tol.txt".to_string(), e(1000, Some("q2"), false)), // within tolerance and md5 differs -> Modified
            ("only_remote.txt".to_string(), e(1, None, false)),
            ("dir".to_string(), e(123, None, true)),
            ("size_differs.txt".to_string(), sized(1000, 11)), // same mtime, different size -> Modified
            ("size_same_time_same.txt".to_string(), sized(1500, 10)), // assumed identical, no hash
            ("modified.txt".to_string(), e(1000, Some("m2"), false)),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            diff(&local, &remote),
            vec![
                ("a.txt".to_string(), Change::LocalNewer),
                ("b.txt".to_string(), Change::RemoteNewer),
                ("modified.txt".to_string(), Change::Modified),
                ("only_local.txt".to_string(), Change::LocalOnly),
                ("only_remote.txt".to_string(), Change::RemoteOnly),
                ("size_differs.txt".to_string(), Change::Modified),
                ("tol.txt".to_string(), Change::Modified),
            ]
        );
    }

    #[test]
    fn hashing_policy() {
        let remote = Entry {
            md5: Some("r".into()),
            ..sized(1000, 10)
        };
        // Default: verify by MD5 whenever sizes match, regardless of mtime.
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
        // --fast: rsync quick check, equal size + mtime is trusted.
        assert!(
            needs_hash(&sized(5000, 10), &remote, true),
            "same size, different mtime"
        );
        assert!(
            !needs_hash(&sized(1500, 10), &remote, true),
            "same size, same mtime: trusted"
        );
        assert!(
            !needs_hash(&sized(5000, 11), &remote, true),
            "different size"
        );
        // Never when there is nothing to compare against or it is already known.
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
    fn fill_hashes_uses_and_populates_cache() {
        let dir = std::env::temp_dir().join(format!("dsync_hash_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), b"hello").unwrap();
        let cache = Cache::open(&dir.join("cache.db")).unwrap();
        let ignore = load_ignore(&dir);
        let mut local = local_walk(&dir, &dir, -1, &ignore).unwrap();
        local.remove("cache.db");
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
        fill_hashes(&dir, &cache, &mut local, &remote, false, 2).unwrap();
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
        // Poison the cache entry to prove it is used instead of re-reading the file.
        cache
            .set_local_hash("f.txt", size, mtime, "cached")
            .unwrap();
        let mut again = local_walk(&dir, &dir, -1, &ignore).unwrap();
        fill_hashes(&dir, &cache, &mut again, &remote, false, 2).unwrap();
        assert_eq!(again["f.txt"].md5.as_deref(), Some("cached"));
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
        let plan = plan_push(&local, &remote, false);
        assert_eq!(
            plan.actions,
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
        assert!(plan.skips.is_empty());
        assert_eq!(
            plan.conflicts,
            vec![
                ("older.txt".to_string(), "remote is newer".to_string()),
                (
                    "size_differs.txt".to_string(),
                    "content differs, same mtime".to_string()
                ),
            ]
        );
        let forced = plan_push(&local, &remote, true);
        assert_eq!(
            forced.actions.len(),
            5,
            "force turns conflicts into actions"
        );
        assert!(forced.conflicts.is_empty());

        let plan = plan_pull(&local, &remote, false);
        assert_eq!(
            plan.actions,
            vec![
                Action::Download {
                    path: "older.txt".into(),
                    id: "id2".into(),
                    mtime_ms: 9000,
                    md5: Some("c2".into())
                },
                Action::Mkdir {
                    path: "rdir".into()
                },
                Action::Download {
                    path: "rdir/r.txt".into(),
                    id: "id5".into(),
                    mtime_ms: 1,
                    md5: Some("r".into())
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
            plan_pull(&local, &remote, false).skips,
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
        let p3 = plan_push(&l3, &r3, false);
        assert!(p3.actions.is_empty() && p3.conflicts.is_empty() && p3.skips.len() == 1);
        // Folder/file type mismatches are skips, not conflicts.
        let l2: BTreeMap<_, _> = [("x".to_string(), e(0, None, true))].into_iter().collect();
        let r2: BTreeMap<_, _> = [("x".to_string(), e(0, Some("q"), false))]
            .into_iter()
            .collect();
        assert_eq!(plan_push(&l2, &r2, false).skips.len(), 1);
        assert_eq!(plan_pull(&l2, &r2, false).skips.len(), 1);
    }

    #[test]
    fn listing_lines() {
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
            existing_id: existing.then(|| "i".to_string()),
            mtime_ms,
        };
        let down = |path: &str, mtime_ms: i64| Action::Download {
            path: path.into(),
            id: "i".into(),
            mtime_ms,
            md5: None,
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
                "top.txt"
            ]
        );
        assert_eq!(all["top.txt"].size, Some(2));
        assert_eq!(all["a"].size, None);
        let shallow = local_walk(&dir, &dir, 1, &ignore).unwrap();
        assert_eq!(
            shallow.keys().cloned().collect::<Vec<_>>(),
            vec![".driveignore", "a", "top.txt"]
        );
        let single = local_walk(&dir, &dir.join("a/one.txt"), -1, &ignore).unwrap();
        assert_eq!(
            single.keys().cloned().collect::<Vec<_>>(),
            vec!["a/one.txt"]
        );
        std::fs::remove_dir_all(&dir).unwrap();
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
            vec!["My Documents/sub folder/it's 100%_done.md"]
        );
        let plan = plan_push(&local, &BTreeMap::new(), false);
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
