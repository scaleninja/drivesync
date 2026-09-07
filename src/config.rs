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
    /// Traversal depth; -1 = unlimited.
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
            if dir.join(GD_DIR).is_dir() {
                let config = load_json(&dir.join(GD_DIR).join("config.json"))?;
                return Ok(Self { root: dir, config });
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

pub fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(value)?)
        .with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        // holds client secret / tokens
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_json_is_private() {
        let dir = std::env::temp_dir().join(format!("dsync_cfg_test_{}", std::process::id()));
        let path = dir.join("credentials.json");
        save_json(
            &path,
            &Credentials {
                access_token: "a".into(),
                refresh_token: "r".into(),
                expires_at: 1,
            },
        )
        .unwrap();
        let back: Credentials = load_json(&path).unwrap();
        assert_eq!(back.refresh_token, "r");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
