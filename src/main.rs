mod auth;
mod cache;
mod config;
mod drive;
mod sync;

use anyhow::{bail, Context, Result};
use cache::Cache;
use clap::{Parser, Subcommand};
use config::{load_json, save_json, Config, Credentials, Workspace, GD_DIR};
use drive::Drive;
use std::path::Path;
use sync::Change;

#[derive(Parser)]
#[command(name = "dsync", version, about = "DriveSync: push, pull and diff a local directory against Google Drive", long_about = None, after_help = "Project: https://github.com/scaleninja/drivesync")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
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
        /// Traversal depth (-1 = unlimited)
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
    Push {
        /// Relative path to push (default: current directory)
        #[arg(default_value = ".")]
        path: String,
        /// Overwrite even when the remote copy is newer
        #[arg(long)]
        force: bool,
        /// Apply changes without asking for confirmation
        #[arg(long, short = 'y')]
        no_prompt: bool,
        /// Number of parallel transfer streams
        #[arg(long, short = 'j', default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=64))]
        threads: u16,
        /// Ignore the cache and re-list the whole remote tree
        #[arg(long)]
        refresh: bool,
    },
    /// Download remote changes from Google Drive
    Pull {
        /// Relative path to pull (default: current directory)
        #[arg(default_value = ".")]
        path: String,
        /// Overwrite even when the local copy is newer
        #[arg(long)]
        force: bool,
        /// Apply changes without asking for confirmation
        #[arg(long, short = 'y')]
        no_prompt: bool,
        /// Number of parallel transfer streams
        #[arg(long, short = 'j', default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=64))]
        threads: u16,
        /// Ignore the cache and re-list the whole remote tree
        #[arg(long)]
        refresh: bool,
    },
    /// Show the workspace configuration and cache state
    Status,
    /// List files whose local and remote modification times differ
    Diff {
        /// Relative path to compare (default: current directory)
        #[arg(default_value = ".")]
        path: String,
        /// Ignore the cache and re-list the whole remote tree
        #[arg(long)]
        refresh: bool,
    },
    /// Print the CLI version
    Version,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
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
        ),
        Cmd::Push {
            path,
            force,
            no_prompt,
            threads,
            refresh,
        } => push(&path, force, no_prompt, threads.into(), refresh),
        Cmd::Pull {
            path,
            force,
            no_prompt,
            threads,
            refresh,
        } => pull(&path, force, no_prompt, threads.into(), refresh),
        Cmd::Status => status(),
        Cmd::Diff { path, refresh } => diff(&path, refresh),
        Cmd::Version => {
            println!(
                "dsync {} (https://github.com/scaleninja/drivesync)",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
    }
}

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
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
    let (client_id, client_secret) = match credentials {
        Some(file) => {
            let v: serde_json::Value = load_json(Path::new(&file))?;
            let app = v.get("installed").or_else(|| v.get("web")).context("client_secret.json has no 'installed' or 'web' section")?;
            let get = |k: &str| app.get(k).and_then(|x| x.as_str()).map(String::from).with_context(|| format!("client_secret.json missing {k}"));
            (get("client_id")?, get("client_secret")?)
        }
        None => (
            client_id.context("missing --client-id (or GOOGLE_CLIENT_ID, or --credentials client_secret.json)")?,
            client_secret.context("missing --client-secret (or GOOGLE_CLIENT_SECRET, or --credentials client_secret.json)")?,
        ),
    };
    let root = sync::absolutize(dir)?;
    std::fs::create_dir_all(root.join(GD_DIR))?;
    let remote_folder = remote_folder.trim_matches('/').to_string();
    let mut config = Config {
        client_id,
        client_secret,
        remote_folder,
        remote_folder_id: "root".into(),
        depth,
    };

    let http = http();
    let creds = auth::Auth::login(&http, &config)?;
    let creds_path = root.join(GD_DIR).join("credentials.json");
    save_json(&creds_path, &creds)?;

    let drive = Drive::new(
        http.clone(),
        auth::Auth::new(http, config.clone(), creds, creds_path),
    );
    config.remote_folder_id = drive
        .resolve_folder("root", &config.remote_folder, true)?
        .context("resolving remote folder")?;
    save_json(&root.join(GD_DIR).join("config.json"), &config)?;
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

fn open(ws: &Workspace) -> Result<Drive> {
    let creds_path = ws.gd("credentials.json");
    let creds: Credentials = load_json(&creds_path).context("no credentials; run `dsync init`")?;
    let http = http();
    Ok(Drive::new(
        http.clone(),
        auth::Auth::new(http, ws.config.clone(), creds, creds_path),
    ))
}

/// Resolve a user-supplied relative path to (absolute local path, workspace-relative path).
fn target(ws: &Workspace, path: &str) -> Result<(std::path::PathBuf, String)> {
    let abs = sync::absolutize(path)?;
    let rel = sync::rel_path(&ws.root, &abs)?;
    Ok((abs, rel))
}

/// Take an exclusive workspace lock so two instances cannot push/pull the same tree at once.
fn lock(ws: &Workspace) -> Result<fd_lock::RwLock<std::fs::File>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(ws.gd("lock"))?;
    let mut lock = fd_lock::RwLock::new(file);
    if lock.try_write().is_err() {
        eprintln!("waiting for another dsync instance to finish...");
        drop(lock.write()?);
    }
    Ok(lock)
}

type Snapshot = std::collections::BTreeMap<String, sync::Entry>;

/// Snapshot both sides of the subtree at `rel`: the local walk and the (refreshed) cache of the remote tree.
fn snapshot(
    ws: &Workspace,
    drive: &Drive,
    cache: &Cache,
    abs: &Path,
    rel: &str,
    refresh: bool,
) -> Result<(Snapshot, Snapshot)> {
    let ignore = sync::load_ignore(&ws.root);
    let local = sync::local_walk(&ws.root, abs, ws.config.depth, &ignore, true)?;
    cache::refresh(
        drive,
        cache,
        &ws.config.remote_folder_id,
        ws.config.depth,
        refresh,
    )?;
    Ok((local, cache.load(rel)?))
}

fn push(path: &str, force: bool, no_prompt: bool, threads: usize, refresh: bool) -> Result<()> {
    let ws = Workspace::find()?;
    let drive = open(&ws)?;
    let mut lock = lock(&ws)?;
    let _guard = lock.write()?;
    let cache = Cache::open(&ws.gd("cache.db"))?;
    let (abs, rel) = target(&ws, path)?;
    if !abs.exists() {
        bail!("{} does not exist", abs.display());
    }
    let (local, mut remote) = snapshot(&ws, &drive, &cache, &abs, &rel, refresh)?;
    let (actions, skips) = sync::plan_push(&local, &remote, force);
    if !sync::confirm(&actions, &skips, no_prompt)? {
        return Ok(());
    }
    // Folder that will hold the pushed subtree (created on demand); the cache learns about it on the next refresh.
    let base_rel = if abs.is_file() {
        rel.rsplit_once('/')
            .map(|(p, _)| p)
            .unwrap_or("")
            .to_string()
    } else {
        rel.clone()
    };
    let base_id = match remote.get(&base_rel).and_then(|e| e.id.clone()) {
        Some(id) => id,
        None => drive
            .resolve_folder(&ws.config.remote_folder_id, &base_rel, true)?
            .context("creating remote folder")?,
    };
    let total = actions.len();
    let failures = sync::exec_push(
        &drive,
        &ws.root,
        &base_rel,
        &base_id,
        actions,
        &mut remote,
        threads,
    )?;
    cache.upsert_all(&remote)?;
    finish("push", total, failures)
}

fn pull(path: &str, force: bool, no_prompt: bool, threads: usize, refresh: bool) -> Result<()> {
    let ws = Workspace::find()?;
    let drive = open(&ws)?;
    let mut lock = lock(&ws)?;
    let _guard = lock.write()?;
    let cache = Cache::open(&ws.gd("cache.db"))?;
    let (abs, rel) = target(&ws, path)?;
    let (local, remote) = snapshot(&ws, &drive, &cache, &abs, &rel, refresh)?;
    if remote.is_empty() && !rel.is_empty() {
        bail!(
            "remote path '{rel}' does not exist under My Drive/{}",
            ws.config.remote_folder
        );
    }
    let (actions, skips) = sync::plan_pull(&local, &remote, force);
    if !sync::confirm(&actions, &skips, no_prompt)? {
        return Ok(());
    }
    let total = actions.len();
    let failures = sync::exec_pull(&drive, &ws.root, actions, threads)?;
    finish("pull", total, failures)
}

fn finish(verb: &str, total: usize, failures: usize) -> Result<()> {
    if failures > 0 {
        bail!("{verb} finished with {failures} of {total} change(s) failed");
    }
    println!("{verb} complete: {total} change(s)");
    Ok(())
}

fn diff(path: &str, refresh: bool) -> Result<()> {
    let ws = Workspace::find()?;
    let drive = open(&ws)?;
    let cache = Cache::open(&ws.gd("cache.db"))?;
    let (abs, rel) = target(&ws, path)?;
    let (local, remote) = snapshot(&ws, &drive, &cache, &abs, &rel, refresh)?;
    let changes = sync::diff(&local, &remote);
    if changes.is_empty() {
        println!("local and remote are in sync");
        return Ok(());
    }
    println!(
        "{:<2} {:<40} {:<24} {:<24}",
        "", "path", "local modified", "remote modified"
    );
    for (p, change) in &changes {
        let l = local
            .get(p)
            .map(|e| sync::fmt_ms(e.mtime_ms))
            .unwrap_or_else(|| "-".into());
        let r = remote
            .get(p)
            .map(|e| sync::fmt_ms(e.mtime_ms))
            .unwrap_or_else(|| "-".into());
        let mark = match change {
            Change::LocalOnly => "+",
            Change::RemoteOnly => "-",
            Change::LocalNewer => ">",
            Change::RemoteNewer => "<",
        };
        println!("{mark:<2} {p:<40} {l:<24} {r:<24}");
    }
    println!("\n+ local only   - remote only   > local newer   < remote newer");
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
            "Cache           : {} (empty; run push, pull or diff)",
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
