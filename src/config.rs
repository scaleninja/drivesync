// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ScaleNinja
// DriveSync (dsync) — https://github.com/scaleninja/drivesync

//! On-disk state: `.gd/config.json` and `.gd/credentials.json` (the cache lives in `.gd/cache.db`).
use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const GD_DIR: &str = ".gd";
pub const IGNORE_FILE: &str = ".driveignore";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Config {
    pub client_id: String,
    pub client_secret: String,
    /// Path of the remote folder under "My Drive" ("" = My Drive root).
    pub remote_folder: String,
    pub remote_folder_id: String,
    /// Traversal depth measured from the sync root on both sides; -1 = unlimited.
    pub depth: i32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds at which `access_token` expires.
    pub expires_at: i64,
}

/// A located sync root plus its config.
pub struct Workspace {
    pub root: PathBuf,
    pub config: Config,
}

impl Workspace {
    /// Walk up from the current directory until a `.gd/` directory is found.
    pub fn find() -> Result<Self> {
        let mut dir = std::env::current_dir()?;
        loop {
            // Only a real directory counts: a symlinked `.gd` could redirect credentials, the
            // cache and the lock outside the workspace.
            if let Ok(m) = std::fs::symlink_metadata(dir.join(GD_DIR)) {
                if m.file_type().is_symlink() {
                    bail!(
                        "{} is a symlink; dsync state must be a real directory",
                        dir.join(GD_DIR).display()
                    );
                }
                if m.is_dir() {
                    let config = load_json(&dir.join(GD_DIR).join("config.json"))?;
                    // On-disk spelling, without Windows' verbatim prefix, so that paths resolved
                    // the same way later strip cleanly against the root.
                    let root = dunce::canonicalize(&dir).unwrap_or(dir);
                    return Ok(Self { root, config });
                }
            }
            if !dir.pop() {
                bail!("not inside a dsync workspace (no .gd directory found); run `dsync init`");
            }
        }
    }
    pub fn gd(&self, name: &str) -> PathBuf {
        self.root.join(GD_DIR).join(name)
    }
    pub fn ignore_file(&self) -> PathBuf {
        self.root.join(IGNORE_FILE)
    }
}

pub fn load_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Write `value` atomically and privately: a fresh 0600 temp file in the same directory is fsynced
/// and renamed over `path`. A crash never leaves a truncated file, no reader ever sees a file with
/// broader permissions, and a symlink at `path` is replaced rather than followed.
pub fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("path has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("state");
    let tmp = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600); // holds the client secret / tokens
    }
    let result = (|| -> Result<()> {
        let mut file = opts.open(&tmp)?;
        std::io::Write::write_all(&mut file, serde_json::to_string_pretty(value)?.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_json_is_private_atomic_and_replaces_symlinks() {
        let dir = std::env::temp_dir().join(format!("dsync_cfg_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        let creds = Credentials {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 1,
        };
        save_json(&path, &creds).unwrap();
        let back: Credentials = load_json(&path).unwrap();
        assert_eq!(back.refresh_token, "r");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no temp file left behind"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            // A symlink at the target is replaced by a regular file; its target is untouched.
            let outside = dir.join("outside.txt");
            std::fs::write(&outside, "keep").unwrap();
            let link = dir.join("linked.json");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            save_json(&link, &creds).unwrap();
            assert!(!std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
