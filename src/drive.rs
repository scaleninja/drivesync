// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ScaleNinja
// DriveSync (dsync) — https://github.com/scaleninja/drivesync

//! Minimal Google Drive v3 client: list, folder resolution, upload, download, changes feed.
use crate::auth::Auth;
use crate::sync::Entry;
use anyhow::{bail, Context, Result};
use reqwest::blocking::{Body, Client, RequestBuilder, Response};
use reqwest::StatusCode;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const API: &str = "https://www.googleapis.com/drive/v3/files";
const CHANGES: &str = "https://www.googleapis.com/drive/v3/changes";
const UPLOAD: &str = "https://www.googleapis.com/upload/drive/v3/files";
const FIELDS: &str = "id,name,mimeType,modifiedTime,md5Checksum,size";
pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
/// Google rejects multipart uploads above 5 MB; larger files use a resumable session.
const MULTIPART_LIMIT: u64 = 5 * 1024 * 1024;
const MAX_ATTEMPTS: u32 = 6;

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct File {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub modified_time: Option<String>,
    pub md5_checksum: Option<String>,
    /// Bytes, as a decimal string (absent for folders and Google-native docs).
    pub size: Option<String>,
}

impl File {
    pub fn is_folder(&self) -> bool {
        self.mime_type == FOLDER_MIME
    }
    /// Google-native docs/sheets/etc. have no binary content to download.
    pub fn is_native_doc(&self) -> bool {
        !self.is_folder() && self.mime_type.starts_with("application/vnd.google-apps.")
    }
    pub fn to_entry(&self) -> Entry {
        Entry {
            mtime_ms: self
                .modified_time
                .as_deref()
                .and_then(crate::sync::parse_rfc3339_ms)
                .unwrap_or(0),
            size: self.size.as_deref().and_then(|s| s.parse().ok()),
            md5: self.md5_checksum.clone(),
            is_dir: self.is_folder(),
            id: Some(self.id.clone()),
            native_doc: self.is_native_doc(),
        }
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ChangedFile {
    #[serde(flatten)]
    pub file: File,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub trashed: bool,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub file_id: String,
    #[serde(default)]
    pub removed: bool,
    pub file: Option<ChangedFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangesResponse {
    next_page_token: Option<String>,
    new_start_page_token: Option<String>,
    changes: Vec<Change>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListResponse {
    next_page_token: Option<String>,
    files: Vec<File>,
}

type Build<'a> = &'a dyn Fn(&Client) -> Result<RequestBuilder>;

/// Cheap to clone and safe to share between worker threads.
#[derive(Clone)]
pub struct Drive {
    http: Client,
    auth: Arc<Mutex<Auth>>,
}

impl Drive {
    pub fn new(http: Client, auth: Auth) -> Self {
        Self {
            http,
            auth: Arc::new(Mutex::new(auth)),
        }
    }

    /// Send with a bearer token. Refreshes once on 401; retries with exponential backoff on
    /// network errors, 429, 5xx and Drive's 403 rate-limit responses.
    fn send(&self, build: Build) -> Result<Response> {
        self.request(build, true)
    }

    fn request(&self, build: Build, retry: bool) -> Result<Response> {
        let mut refreshed = false;
        let attempts = if retry { MAX_ATTEMPTS } else { 1 };
        for attempt in 0..attempts {
            let token = self.auth.lock().unwrap().token()?;
            let resp = match build(&self.http)?.bearer_auth(&token).send() {
                Ok(r) => r,
                Err(e) if attempt + 1 < attempts => {
                    backoff(attempt, &e.to_string());
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                self.auth.lock().unwrap().refresh_if_stale(&token)?;
                continue;
            }
            if status.is_success() {
                return Ok(resp);
            }
            let body = resp.text().unwrap_or_default();
            let rate_limited = status == StatusCode::FORBIDDEN && body.contains("ateLimitExceeded");
            if (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() || rate_limited)
                && attempt + 1 < attempts
            {
                backoff(attempt, &format!("{status}"));
                continue;
            }
            bail!("Drive API error {status}: {body}");
        }
        bail!("request failed after {attempts} attempt(s)")
    }

    fn query(&self, q: String) -> Result<Vec<File>> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let (q, pt) = (q.clone(), page_token.clone());
            let resp: ListResponse = self
                .send(&move |c| {
                    let mut params = vec![
                        ("q", q.clone()),
                        ("fields", format!("nextPageToken,files({FIELDS})")),
                        ("pageSize", "1000".into()),
                        ("spaces", "drive".into()),
                    ];
                    if let Some(pt) = &pt {
                        params.push(("pageToken", pt.clone()));
                    }
                    Ok(c.get(API).query(&params))
                })?
                .json()?;
            out.extend(resp.files);
            match resp.next_page_token {
                Some(t) => page_token = Some(t),
                None => return Ok(out),
            }
        }
    }

    pub fn list_children(&self, parent_id: &str) -> Result<Vec<File>> {
        self.query(format!(
            "'{}' in parents and trashed = false",
            escape(parent_id)
        ))
    }

    pub fn find_child(&self, parent_id: &str, name: &str, folder: bool) -> Result<Option<File>> {
        let kind = if folder { "=" } else { "!=" };
        let files = self.query(format!(
            "'{}' in parents and name = '{}' and mimeType {kind} '{FOLDER_MIME}' and trashed = false",
            escape(parent_id),
            escape(name)
        ))?;
        Ok(files.into_iter().next())
    }

    pub fn create_folder(&self, parent_id: &str, name: &str) -> Result<File> {
        let body =
            serde_json::json!({ "name": name, "mimeType": FOLDER_MIME, "parents": [parent_id] });
        Ok(self
            .send(&move |c| Ok(c.post(API).query(&[("fields", FIELDS)]).json(&body)))?
            .json()?)
    }

    /// Resolve `path` (slash-separated, relative to `base_id`) to a folder id, creating folders if asked.
    pub fn resolve_folder(
        &self,
        base_id: &str,
        path: &str,
        create: bool,
    ) -> Result<Option<String>> {
        let mut id = base_id.to_string();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            id = match self.find_child(&id, part, true)? {
                Some(f) => f.id,
                None if create => self.create_folder(&id, part)?.id,
                None => return Ok(None),
            };
        }
        Ok(Some(id))
    }

    /// Upload `local` as `name` under `parent_id`, or replace the content of `existing_id`.
    /// Small files go in one multipart request; larger ones stream through a resumable session.
    pub fn upload(
        &self,
        parent_id: &str,
        name: &str,
        existing_id: Option<&str>,
        local: &Path,
        modified_time: &str,
    ) -> Result<File> {
        let size = std::fs::metadata(local)?.len();
        let mut meta = serde_json::json!({ "name": name, "modifiedTime": modified_time });
        if existing_id.is_none() {
            meta["parents"] = serde_json::json!([parent_id]);
        }
        let url = existing_id.map_or(UPLOAD.to_string(), |id| format!("{UPLOAD}/{id}"));
        let is_update = existing_id.is_some();
        let start = move |c: &Client| {
            if is_update {
                c.patch(&url)
            } else {
                c.post(&url)
            }
        };

        if size <= MULTIPART_LIMIT {
            let data = std::fs::read(local)?;
            let boundary = "dsync_boundary_7f3a9c";
            let mut body = Vec::with_capacity(data.len() + 512);
            body.extend_from_slice(format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
            body.extend_from_slice(&data);
            body.extend_from_slice(format!("\r\n--{boundary}--").as_bytes());
            return Ok(self
                .send(&move |c| {
                    Ok(start(c)
                        .query(&[("uploadType", "multipart"), ("fields", FIELDS)])
                        .header(
                            "Content-Type",
                            format!("multipart/related; boundary={boundary}"),
                        )
                        .body(body.clone()))
                })?
                .json()?);
        }

        // Resumable: open a session, then PUT the whole file in one streamed request.
        // A failed PUT restarts with a fresh session rather than resuming mid-stream.
        let mut last_err = None;
        for attempt in 0..3 {
            let init = self.send(&|c| {
                Ok(start(c)
                    .query(&[("uploadType", "resumable"), ("fields", FIELDS)])
                    .header("X-Upload-Content-Type", "application/octet-stream")
                    .header("X-Upload-Content-Length", size.to_string())
                    .json(&meta))
            })?;
            let session = init
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .context("resumable upload: no session URI")?
                .to_string();
            let put = self.request(
                &|c| {
                    let file = std::fs::File::open(local)?;
                    Ok(c.put(&session)
                        .header(reqwest::header::CONTENT_LENGTH, size)
                        .body(Body::sized(file, size)))
                },
                false,
            );
            match put {
                Ok(resp) => return Ok(resp.json()?),
                Err(e) => {
                    backoff(attempt, &e.to_string());
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap()).context("resumable upload failed")
    }

    /// Stream a file's content to `dest` via a temp file that is renamed into place only after the
    /// bytes have been verified against `expected_md5` (when Drive provides one).
    pub fn download_to(&self, id: &str, dest: &Path, expected_md5: Option<&str>) -> Result<()> {
        let url = format!("{API}/{id}");
        let mut resp = self.send(&move |c| Ok(c.get(&url).query(&[("alt", "media")])))?;
        let tmp = dest.with_file_name(format!(
            ".{}{}",
            dest.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("download"),
            crate::sync::PART_SUFFIX
        ));
        let result = (|| -> Result<()> {
            let mut writer = HashWriter {
                inner: std::fs::File::create(&tmp)?,
                ctx: md5::Context::new(),
            };
            resp.copy_to(&mut writer)?;
            let actual = format!("{:x}", writer.ctx.compute());
            if let Some(expected) = expected_md5 {
                if actual != expected {
                    bail!("checksum mismatch after download (expected {expected}, got {actual})");
                }
            }
            Ok(std::fs::rename(&tmp, dest)?)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Token marking "now" in the Changes feed.
    pub fn start_page_token(&self) -> Result<String> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct T {
            start_page_token: String,
        }
        let t: T = self
            .send(&|c| Ok(c.get(format!("{CHANGES}/startPageToken"))))?
            .json()?;
        Ok(t.start_page_token)
    }

    /// All changes in My Drive since `token`, plus the token to use next time.
    pub fn changes(&self, token: &str) -> Result<(Vec<Change>, String)> {
        let mut out = Vec::new();
        let mut page = token.to_string();
        loop {
            let pt = page.clone();
            let resp: ChangesResponse = self
                .send(&move |c| {
                    Ok(c.get(CHANGES).query(&[
                        ("pageToken", pt.as_str()),
                        ("pageSize", "1000"),
                        ("spaces", "drive"),
                        ("includeRemoved", "true"),
                        ("restrictToMyDrive", "true"),
                        ("fields", &format!("nextPageToken,newStartPageToken,changes(fileId,removed,file({FIELDS},parents,trashed))")),
                    ]))
                })?
                .json()?;
            out.extend(resp.changes);
            match (resp.next_page_token, resp.new_start_page_token) {
                (Some(next), _) => page = next,
                (None, Some(new)) => return Ok((out, new)),
                (None, None) => bail!("changes response without newStartPageToken"),
            }
        }
    }

    /// Resolve an alias such as `root` to the real file id.
    pub fn file_id(&self, alias: &str) -> Result<String> {
        let url = format!("{API}/{alias}");
        let f: File = self
            .send(&move |c| Ok(c.get(&url).query(&[("fields", FIELDS)])))?
            .json()?;
        Ok(f.id)
    }

    /// List the whole subtree under `root_id` with one paginated query over My Drive (rclone's
    /// "fast list"): every non-trashed file comes back with its parent id, and the tree is assembled
    /// locally. One request per 1000 files instead of one per folder.
    pub fn list_tree(&self, root_id: &str, depth: i32) -> Result<BTreeMap<String, Entry>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Row {
            #[serde(flatten)]
            file: File,
            #[serde(default)]
            parents: Vec<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page {
            next_page_token: Option<String>,
            files: Vec<Row>,
        }
        let mut by_parent: BTreeMap<String, Vec<File>> = BTreeMap::new();
        let mut page_token: Option<String> = None;
        loop {
            let pt = page_token.clone();
            let page: Page = self
                .send(&move |c| {
                    let mut params = vec![
                        ("q", "trashed = false".to_string()),
                        ("fields", format!("nextPageToken,files({FIELDS},parents)")),
                        ("pageSize", "1000".into()),
                        ("spaces", "drive".into()),
                    ];
                    if let Some(pt) = &pt {
                        params.push(("pageToken", pt.clone()));
                    }
                    Ok(c.get(API).query(&params))
                })?
                .json()?;
            for row in page.files {
                for parent in row.parents {
                    by_parent.entry(parent).or_default().push(row.file.clone());
                }
            }
            match page.next_page_token {
                Some(t) => page_token = Some(t),
                None => break,
            }
        }
        Ok(assemble_tree(&by_parent, root_id, depth))
    }

    /// Recursively list `folder_id` into `out`, keyed by relative path under `prefix`.
    pub fn walk(
        &self,
        folder_id: &str,
        prefix: &str,
        depth: i32,
        out: &mut BTreeMap<String, Entry>,
    ) -> Result<()> {
        if depth == 0 {
            return Ok(());
        }
        for f in self.list_children(folder_id)? {
            if !crate::sync::valid_name(&f.name) {
                crate::progress::eprintln(&format!(
                    "! skip     remote name {:?} cannot be a local path",
                    f.name
                ));
                continue;
            }
            let rel = crate::sync::join_rel(prefix, &f.name);
            if out.contains_key(&rel) {
                continue; // Drive allows duplicate names; keep the first.
            }
            let entry = f.to_entry();
            let is_dir = entry.is_dir;
            out.insert(rel.clone(), entry);
            if is_dir {
                self.walk(&f.id, &rel, depth - 1, out)?;
            }
        }
        Ok(())
    }
}

/// Writes through to `inner` while computing an MD5 of everything written.
struct HashWriter<W: std::io::Write> {
    inner: W,
    ctx: md5::Context,
}

impl<W: std::io::Write> std::io::Write for HashWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.ctx.consume(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Build the relative-path map from a parent-id -> children index, breadth-first from `root_id`,
/// honouring `depth`, skipping unmappable names and keeping the first of any duplicate names.
fn assemble_tree(
    by_parent: &BTreeMap<String, Vec<File>>,
    root_id: &str,
    depth: i32,
) -> BTreeMap<String, Entry> {
    let mut out = BTreeMap::new();
    let mut frontier = vec![(root_id.to_string(), String::new(), depth)];
    while let Some((id, prefix, remaining)) = frontier.pop() {
        if remaining == 0 {
            continue;
        }
        for f in by_parent.get(&id).into_iter().flatten() {
            if !crate::sync::valid_name(&f.name) {
                crate::progress::eprintln(&format!(
                    "! skip     remote name {:?} cannot be a local path",
                    f.name
                ));
                continue;
            }
            let rel = crate::sync::join_rel(&prefix, &f.name);
            if out.contains_key(&rel) {
                continue;
            }
            let entry = f.to_entry();
            if entry.is_dir {
                frontier.push((f.id.clone(), rel.clone(), remaining - 1));
            }
            out.insert(rel, entry);
        }
    }
    out
}

/// Sleep 0.5 s, 1 s, 2 s, ... plus jitter before retrying `attempt + 1`.
fn backoff(attempt: u32, why: &str) {
    let mut jitter = [0u8; 2];
    let _ = getrandom::getrandom(&mut jitter);
    let ms = 500 * 2u64.pow(attempt) + u64::from(u16::from_le_bytes(jitter)) % 500;
    crate::progress::eprintln(&format!("  retrying in {:.1}s ({why})", ms as f64 / 1000.0));
    std::thread::sleep(Duration::from_millis(ms));
}

/// Escape a value for use inside single quotes in a Drive query.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str, name: &str, folder: bool) -> File {
        File {
            id: id.into(),
            name: name.into(),
            mime_type: if folder {
                FOLDER_MIME.into()
            } else {
                "text/plain".into()
            },
            modified_time: Some("2026-09-08T00:00:00.000Z".into()),
            md5_checksum: (!folder).then(|| "abc".into()),
            size: (!folder).then(|| "42".into()),
        }
    }

    #[test]
    fn hash_writer_hashes_what_it_writes() {
        use std::io::Write;
        let mut w = HashWriter {
            inner: Vec::new(),
            ctx: md5::Context::new(),
        };
        w.write_all(b"hel").unwrap();
        w.write_all(b"lo").unwrap();
        assert_eq!(w.inner, b"hello");
        assert_eq!(
            format!("{:x}", w.ctx.compute()),
            "5d41402abc4b2a76b9719d911017c592"
        );
    }

    #[test]
    fn query_escaping() {
        assert_eq!(escape("it's"), "it\\'s");
        assert_eq!(escape("back\\slash"), "back\\\\slash");
        assert_eq!(escape("plain name with spaces"), "plain name with spaces");
    }

    #[test]
    fn tree_assembly_handles_spaces_depth_duplicates_and_bad_names() {
        let mut by_parent: BTreeMap<String, Vec<File>> = BTreeMap::new();
        by_parent.insert(
            "ROOT".into(),
            vec![
                file("d1", "My Documents", true),
                file("f1", "top level.txt", false),
                file("f1dup", "top level.txt", false),
                file("bad", "with/slash.txt", false),
                file("dot", ".", true),
            ],
        );
        by_parent.insert(
            "d1".into(),
            vec![
                file("d2", "sub folder", true),
                file("f2", "it's 100%_done.md", false),
            ],
        );
        by_parent.insert("d2".into(), vec![file("f3", "deep file.txt", false)]);
        by_parent.insert("dot".into(), vec![file("f4", "unreachable.txt", false)]);

        let all = assemble_tree(&by_parent, "ROOT", -1);
        assert_eq!(
            all.keys().cloned().collect::<Vec<_>>(),
            vec![
                "My Documents",
                "My Documents/it's 100%_done.md",
                "My Documents/sub folder",
                "My Documents/sub folder/deep file.txt",
                "top level.txt"
            ]
        );
        assert_eq!(all["top level.txt"].id.as_deref(), Some("f1")); // first duplicate wins
        assert!(all["My Documents/sub folder"].is_dir);

        let shallow = assemble_tree(&by_parent, "ROOT", 2);
        assert!(shallow.contains_key("My Documents/sub folder"));
        assert!(!shallow.contains_key("My Documents/sub folder/deep file.txt"));
    }
}
