// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ScaleNinja
// DriveSync (dsync) — https://github.com/scaleninja/drivesync

mod auth;
mod cache;
mod config;
mod drive;
mod progress;
mod sync;

use anyhow::{bail, Context, Result};
use cache::Cache;
use clap::{Parser, Subcommand};
use config::{load_json, save_json, Config, Credentials, Workspace, GD_DIR};
use drive::Drive;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Parser)]
#[command(
    name = "dsync",
    version,
    about = "DriveSync: push, pull and diff a local directory against Google Drive",
    long_about = None,
    after_help = "Home: https://scaleninja.com/drivesync/  Source: https://github.com/scaleninja/drivesync"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct SyncOpts {
    /// Relative path to operate on (default: current directory)
    #[arg(default_value = ".")]
    path: String,
    /// Overwrite even when the destination copy is newer or content differs at equal mtime
    #[arg(long)]
    force: bool,
    /// Apply changes without asking for confirmation (refused if there are conflicts)
    #[arg(long, short = 'y')]
    no_prompt: bool,
    /// Number of parallel transfer streams
    #[arg(long, short = 'j', default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=64))]
    threads: u16,
    /// Ignore the cache and re-list the whole remote tree
    #[arg(long)]
    refresh: bool,
    /// Trust equal size and mtime instead of verifying MD5 (rsync-style quick check)
    #[arg(long, conflicts_with = "verify")]
    fast: bool,
    /// Re-read every file that needs hashing instead of trusting the local hash cache
    #[arg(long)]
    verify: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Initialize DIR as a sync root and authorize with Google Drive
    Init {
        /// Directory to initialize (default: current directory)
        #[arg(default_value = ".")]
        dir: String,
        /// Remote folder path under "My Drive" (created if missing; default: My Drive root)
        #[arg(long, default_value = "")]
        remote_folder: String,
        /// Traversal depth from the sync root (-1 = unlimited)
        #[arg(long, default_value_t = -1, allow_hyphen_values = true)]
        depth: i32,
        /// OAuth client id (or env GOOGLE_CLIENT_ID)
        #[arg(long, env = "GOOGLE_CLIENT_ID")]
        client_id: Option<String>,
        /// OAuth client secret (or env GOOGLE_CLIENT_SECRET)
        #[arg(long, env = "GOOGLE_CLIENT_SECRET")]
        client_secret: Option<String>,
        /// Path to a client_secret.json downloaded from Google Cloud Console
        #[arg(long, conflicts_with_all = ["client_id", "client_secret"])]
        credentials: Option<String>,
    },
    /// Upload local changes to Google Drive
    Push(SyncOpts),
    /// Download remote changes from Google Drive
    Pull(SyncOpts),
    /// Show the workspace configuration and cache state
    Status,
    /// List files that differ between local and remote (exit status 1 if any do)
    Diff {
        /// Relative path to compare (default: current directory)
        #[arg(default_value = ".")]
        path: String,
        /// Ignore the cache and re-list the whole remote tree
        #[arg(long)]
        refresh: bool,
        /// Trust equal size and mtime instead of verifying MD5 (rsync-style quick check)
        #[arg(long, conflicts_with = "verify")]
        fast: bool,
        /// Re-read every file that needs hashing instead of trusting the local hash cache
        #[arg(long)]
        verify: bool,
        /// Number of threads used for hashing
        #[arg(long, short = 'j', default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=64))]
        threads: u16,
    },
    /// Refresh the local index of remote files (incrementally, or fully with --refresh)
    UpdateCache {
        /// Ignore the cache and re-list the whole remote tree
        #[arg(long)]
        refresh: bool,
    },
    /// Print the CLI version
    Version,
}

fn main() {
    // Every command returns before the process exits, so locks and the cache close cleanly.
    let code = match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

/// Runs a command and returns the process exit code (`diff` uses 1 to mean "differences found").
fn run() -> Result<i32> {
    match Cli::parse().cmd {
        Cmd::Init {
            dir,
            remote_folder,
            depth,
            client_id,
            client_secret,
            credentials,
        } => init(
            &dir,
            &remote_folder,
            depth,
            client_id,
            client_secret,
            credentials,
        )
        .map(|()| 0),
        Cmd::Push(o) => push(&o).map(|()| 0),
        Cmd::Pull(o) => pull(&o).map(|()| 0),
        Cmd::Status => status().map(|()| 0),
        Cmd::UpdateCache { refresh } => update_cache(refresh).map(|()| 0),
        Cmd::Diff {
            path,
            refresh,
            fast,
            verify,
            threads,
        } => diff(&path, refresh, fast, verify, threads.into()),
        Cmd::Version => {
            println!(
                "dsync {} (https://scaleninja.com/drivesync/)",
                env!("CARGO_PKG_VERSION")
            );
            Ok(0)
        }
    }
}

fn http() -> reqwest::blocking::Client {
    // No overall timeout: a multi-gigabyte upload or download may legitimately take hours.
    // Dead connections are still detected via the connect timeout and TCP keepalive.
    reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .timeout(None)
        .build()
        .expect("http client")
}

fn init(
    dir: &str,
    remote_folder: &str,
    depth: i32,
    client_id: Option<String>,
    client_secret: Option<String>,
    credentials: Option<String>,
) -> Result<()> {
    // A client bundled at build time is used only as a whole, never mixed with a partial override.
    let bundled = match (
        option_env!("DSYNC_CLIENT_ID"),
        option_env!("DSYNC_CLIENT_SECRET"),
    ) {
        (Some(id), Some(secret)) => Some((id.to_string(), secret.to_string())),
        _ => None,
    };
    let (client_id, client_secret) = match (credentials, client_id, client_secret) {
        (Some(file), _, _) => {
            let v: serde_json::Value = load_json(Path::new(&file))?;
            let app = v.get("installed").or_else(|| v.get("web")).context("client_secret.json has no 'installed' or 'web' section")?;
            let get = |k: &str| app.get(k).and_then(|x| x.as_str()).map(String::from).with_context(|| format!("client_secret.json missing {k}"));
            (get("client_id")?, get("client_secret")?)
        }
        (None, Some(id), Some(secret)) => (id, secret),
        (None, None, None) => bundled.context("missing --client-id/--client-secret (or GOOGLE_CLIENT_ID/GOOGLE_CLIENT_SECRET, or --credentials client_secret.json)")?,
        _ => bail!("--client-id and --client-secret must be given together"),
    };
    if depth != -1 && depth < 1 {
        bail!("--depth must be -1 (unlimited) or a positive number of levels");
    }
    let root = sync::absolutize(dir)?;
    let mut config = Config {
        client_id,
        client_secret,
        remote_folder: remote_folder.trim_matches('/').to_string(),
        remote_folder_id: String::new(),
        depth,
    };

    // Nothing on disk changes until Google has accepted the authorization.
    let http = http();
    let creds = auth::Auth::login(&http, &config)?;
    std::fs::create_dir_all(root.join(GD_DIR))?;
    let ws = Workspace {
        root: root.clone(),
        config: config.clone(),
    };
    let mut lock = lock(&ws)?;
    let _guard = lock.write()?;
    for stale in ["cache.db", "cache.db-wal", "cache.db-shm"] {
        let _ = std::fs::remove_file(ws.gd(stale)); // a re-init may point at another folder
    }
    let creds_path = ws.gd("credentials.json");
    let drive = Drive::new(
        http.clone(),
        auth::Auth::new(http, config.clone(), creds.clone(), creds_path.clone()),
    );
    let root_id = drive.file_id("root")?; // real id, so Changes-feed parent ids can be matched
    config.remote_folder_id = drive
        .resolve_folder(&root_id, &config.remote_folder, true)?
        .context("resolving remote folder")?;
    save_json(&creds_path, &creds)?;
    save_json(&ws.gd("config.json"), &config)?;
    if !root.join(config::IGNORE_FILE).exists() {
        std::fs::write(
            root.join(config::IGNORE_FILE),
            "# gitignore-style patterns for files dsync should not sync\n.DS_Store\n",
        )?;
    }
    println!(
        "Initialized {} <-> My Drive/{} (id {})",
        root.display(),
        config.remote_folder,
        config.remote_folder_id
    );
    Ok(())
}

fn open(ws: &mut Workspace) -> Result<Drive> {
    let creds_path = ws.gd("credentials.json");
    let creds: Credentials = load_json(&creds_path).context("no credentials; run `dsync init`")?;
    let http = http();
    let drive = Drive::new(
        http.clone(),
        auth::Auth::new(http, ws.config.clone(), creds, creds_path),
    );
    if ws.config.remote_folder_id == "root" {
        // Older workspaces stored the alias; the Changes feed reports real parent ids.
        ws.config.remote_folder_id = drive.file_id("root")?;
        save_json(&ws.gd("config.json"), &ws.config)?;
    }
    Ok(drive)
}

/// Resolve a user-supplied relative path to (absolute local path, workspace-relative path).
fn target(ws: &Workspace, path: &str) -> Result<(std::path::PathBuf, String)> {
    let abs = sync::absolutize(path)?;
    let rel = sync::rel_path(&ws.root, &abs)?;
    if sync::is_reserved(&rel) {
        bail!("{rel} is reserved for dsync's own state");
    }
    Ok((abs, rel))
}

/// The workspace lock: push, pull and init hold it exclusively; diff and update-cache hold it
/// shared, so readers never overlap a writer but may overlap each other.
fn lock(ws: &Workspace) -> Result<fd_lock::RwLock<std::fs::File>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(ws.gd("lock"))?;
    let mut lock = fd_lock::RwLock::new(file);
    if lock.try_write().is_err() {
        eprintln!("waiting for another dsync instance to finish...");
    }
    Ok(lock)
}

/// Whether the workspace filesystem folds case (macOS and Windows defaults). Probed by looking
/// up the always-present `config.json` under another spelling; nothing is written.
fn case_insensitive_fs(ws: &Workspace) -> bool {
    ws.gd("config.json").is_file() && ws.gd("CONFIG.JSON").is_file()
}

type Snapshot = BTreeMap<String, sync::Entry>;

struct Snap {
    local: Snapshot,
    remote: Snapshot,
    collisions: BTreeSet<String>,
}

/// Snapshot both sides of the subtree at `rel`: the local walk and the refreshed remote index,
/// both filtered by the same ignore and reserved-path rules, with local hashes where needed.
fn snapshot(
    ws: &Workspace,
    drive: &Drive,
    cache: &Cache,
    abs: &Path,
    rel: &str,
    o: &SyncOpts,
) -> Result<Snap> {
    let spinner = progress::Spinner::start("Scanning local files...");
    let ignore = sync::load_ignore(&ws.root);
    if !rel.is_empty() && sync::is_excluded(&ws.root, &ignore, abs) {
        spinner.finish();
        bail!("{rel} is excluded by {}", config::IGNORE_FILE);
    }
    let mut local = sync::local_walk(&ws.root, abs, ws.config.depth, &ignore)?;
    cache.prune_local_hashes(rel, &local)?;
    spinner.set(
        if o.refresh {
            "Listing the remote tree..."
        } else {
            "Refreshing remote index..."
        }
        .to_string(),
    );
    cache::refresh(
        drive,
        cache,
        &ws.config.remote_folder_id,
        ws.config.depth,
        o.refresh,
    )?;
    let mut remote = cache.load(rel)?;
    sync::filter_remote(&ws.root, &ignore, &mut remote);
    spinner.finish();
    sync::fill_hashes(
        &ws.root,
        cache,
        &mut local,
        &remote,
        o.fast,
        o.verify,
        o.threads.into(),
    )?;
    let collisions = if case_insensitive_fs(ws) {
        sync::case_collisions(&local, &remote)
    } else {
        BTreeSet::new()
    };
    Ok(Snap {
        local,
        remote,
        collisions,
    })
}

fn push(o: &SyncOpts) -> Result<()> {
    let mut ws = Workspace::find()?;
    let drive = open(&mut ws)?;
    let mut lock = lock(&ws)?;
    let _guard = lock.write()?;
    let cache = Cache::open(&ws.gd("cache.db"))?;
    let (abs, rel) = target(&ws, &o.path)?;
    if !abs.exists() {
        bail!("{} does not exist", abs.display());
    }
    let snap = snapshot(&ws, &drive, &cache, &abs, &rel, o)?;
    let plan = sync::plan_push(&snap.local, &snap.remote, o.force, &snap.collisions);
    if !sync::confirm(&plan, &snap.local, &snap.remote, o.no_prompt)? {
        return finish("push", 0, plan.errors.len());
    }
    // Folder that will hold the pushed subtree; created on demand and recorded in the cache.
    let base_rel = if abs.is_file() {
        sync::parent_of(&rel).to_string()
    } else {
        rel.clone()
    };
    let known = snap
        .remote
        .get(&base_rel)
        .or(cache.load(&base_rel)?.get(&base_rel))
        .and_then(|e| e.id.clone());
    let base_id = match known {
        Some(id) => id,
        None if base_rel.is_empty() => ws.config.remote_folder_id.clone(),
        None => {
            let id = drive
                .resolve_folder(&ws.config.remote_folder_id, &base_rel, true)?
                .context("creating remote folder")?;
            cache.upsert(
                &base_rel,
                &sync::Entry {
                    is_dir: true,
                    id: Some(id.clone()),
                    ..Default::default()
                },
            )?;
            id
        }
    };
    let total = plan.actions.len();
    let failures = sync::exec_push(
        &drive,
        &cache,
        &ws.root,
        &base_rel,
        &base_id,
        plan.actions,
        o.threads.into(),
    )?;
    finish("push", total, failures + plan.errors.len())
}

fn pull(o: &SyncOpts) -> Result<()> {
    let mut ws = Workspace::find()?;
    let drive = open(&mut ws)?;
    let mut lock = lock(&ws)?;
    let _guard = lock.write()?;
    let cache = Cache::open(&ws.gd("cache.db"))?;
    let (abs, rel) = target(&ws, &o.path)?;
    let snap = snapshot(&ws, &drive, &cache, &abs, &rel, o)?;
    if snap.remote.is_empty() && !rel.is_empty() {
        bail!(
            "remote path '{rel}' does not exist under My Drive/{}",
            ws.config.remote_folder
        );
    }
    let plan = sync::plan_pull(&snap.local, &snap.remote, o.force, &snap.collisions);
    if !sync::confirm(&plan, &snap.local, &snap.remote, o.no_prompt)? {
        return finish("pull", 0, plan.errors.len());
    }
    let total = plan.actions.len();
    let failures = sync::exec_pull(&drive, &cache, &ws.root, plan.actions, o.threads.into())?;
    finish("pull", total, failures + plan.errors.len())
}

fn finish(verb: &str, total: usize, failures: usize) -> Result<()> {
    if failures > 0 {
        bail!(
            "{verb} finished with {failures} failure(s) out of {} item(s)",
            total + failures
        );
    }
    if total > 0 {
        println!("{verb} complete: {total} change(s)");
    }
    Ok(())
}

fn diff(path: &str, refresh: bool, fast: bool, verify: bool, threads: usize) -> Result<i32> {
    let mut ws = Workspace::find()?;
    let drive = open(&mut ws)?;
    let lock = lock(&ws)?;
    let _guard = lock.read()?;
    let cache = Cache::open(&ws.gd("cache.db"))?;
    let (abs, rel) = target(&ws, path)?;
    let opts = SyncOpts {
        path: path.into(),
        force: false,
        no_prompt: false,
        threads: threads as u16,
        refresh,
        fast,
        verify,
    };
    let snap = snapshot(&ws, &drive, &cache, &abs, &rel, &opts)?;
    let mut changes = sync::diff(&snap.local, &snap.remote);
    for p in &snap.collisions {
        if !changes.iter().any(|(c, _)| c == p) {
            changes.push((p.clone(), sync::Change::TypeMismatch));
        }
    }
    changes.sort();
    if changes.is_empty() {
        println!("local and remote are in sync");
        return Ok(0);
    }
    let mut counts = BTreeMap::new();
    for (p, change) in &changes {
        let label = if snap.collisions.contains(p) {
            "case collision"
        } else {
            change.label()
        };
        if snap.collisions.contains(p) {
            println!(
                "{}",
                sync::line("C", p, "name differs only by case from another entry")
            );
        } else {
            println!(
                "{}",
                sync::diff_line(p, *change, snap.local.get(p), snap.remote.get(p))
            );
        }
        *counts.entry(label).or_insert(0usize) += 1;
    }
    let summary: Vec<String> = counts
        .iter()
        .map(|(label, n)| format!("{n} {label}"))
        .collect();
    println!("{} file(s) differ: {}", changes.len(), summary.join(", "));
    Ok(1)
}

fn update_cache(refresh: bool) -> Result<()> {
    let mut ws = Workspace::find()?;
    let drive = open(&mut ws)?;
    let lock = lock(&ws)?;
    let _guard = lock.read()?;
    let cache = Cache::open(&ws.gd("cache.db"))?;
    let before = cache.count()?;
    let spinner = progress::Spinner::start(if refresh {
        "Listing the remote tree..."
    } else {
        "Refreshing remote index..."
    });
    cache::refresh(
        &drive,
        &cache,
        &ws.config.remote_folder_id,
        ws.config.depth,
        refresh,
    )?;
    spinner.finish();
    println!(
        "cache updated: {} entries (was {before}), {}",
        cache.count()?,
        cache.updated_at()?.unwrap_or_default()
    );
    Ok(())
}

fn status() -> Result<()> {
    let ws = Workspace::find()?;
    let c = &ws.config;
    let remote = if c.remote_folder.is_empty() {
        "My Drive".to_string()
    } else {
        format!("My Drive/{}", c.remote_folder)
    };
    println!("Local directory : {}", ws.root.display());
    println!("Remote folder   : {remote} (id {})", c.remote_folder_id);
    println!(
        "Depth           : {}",
        if c.depth < 0 {
            "unlimited".to_string()
        } else {
            c.depth.to_string()
        }
    );
    let cache_path = ws.gd("cache.db");
    match Cache::open(&cache_path).and_then(|c| Ok((c.count()?, c.updated_at()?))) {
        Ok((n, Some(updated))) => println!(
            "Cache           : {} ({n} entries, updated {updated}, incremental)",
            cache_path.display()
        ),
        Ok((_, None)) => println!(
            "Cache           : {} (empty; run update-cache, push, pull or diff)",
            cache_path.display()
        ),
        Err(e) => println!(
            "Cache           : {} (unreadable: {e:#})",
            cache_path.display()
        ),
    }
    let ignore = ws.ignore_file();
    match std::fs::read_to_string(&ignore) {
        Ok(text) => {
            let n = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .count();
            println!("Ignore file     : {} ({n} pattern(s))", ignore.display());
        }
        Err(_) => println!("Ignore file     : {} (absent)", ignore.display()),
    }
    println!(
        "Filesystem      : case-{}",
        if case_insensitive_fs(&ws) {
            "insensitive (name collisions are reported as conflicts)"
        } else {
            "sensitive"
        }
    );
    match load_json::<Credentials>(&ws.gd("credentials.json")) {
        Ok(creds) => {
            let left = creds.expires_at - auth::now();
            println!(
                "Access token    : {}",
                if left > 0 {
                    format!("valid for {} min (auto-refreshes)", left / 60)
                } else {
                    "expired (will refresh on next use)".into()
                }
            );
        }
        Err(_) => println!("Access token    : none (run `dsync init`)"),
    }
    Ok(())
}
