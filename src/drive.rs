//! Minimal Google Drive v3 client: list, folder resolution, upload, download.
use crate::auth::Auth;
use crate::sync::Entry;
use anyhow::{bail, Result};
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::StatusCode;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const API: &str = "https://www.googleapis.com/drive/v3/files";
const CHANGES: &str = "https://www.googleapis.com/drive/v3/changes";
const UPLOAD: &str = "https://www.googleapis.com/upload/drive/v3/files";
const FIELDS: &str = "id,name,mimeType,modifiedTime,md5Checksum";
pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct File {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub modified_time: Option<String>,
    pub md5_checksum: Option<String>,
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

    /// Send a request with a bearer token; on 401, refresh the token once and retry.
    fn send(&self, build: &dyn Fn(&Client) -> RequestBuilder) -> Result<Response> {
        for attempt in 0..2 {
            let token = self.auth.lock().unwrap().token()?;
            let resp = build(&self.http).bearer_auth(&token).send()?;
            if resp.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                self.auth.lock().unwrap().refresh_if_stale(&token)?;
                continue;
            }
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().unwrap_or_default();
                bail!("Drive API error {status}: {body}");
            }
            return Ok(resp);
        }
        unreachable!()
    }

    fn query(&self, q: String) -> Result<Vec<File>> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let q = q.clone();
            let pt = page_token.clone();
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
                    c.get(API).query(&params)
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
            .send(&move |c| c.post(API).query(&[("fields", FIELDS)]).json(&body))?
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

    /// Multipart upload. Creates a new file, or replaces the content of `existing_id`.
    pub fn upload(
        &self,
        parent_id: &str,
        name: &str,
        existing_id: Option<&str>,
        data: Vec<u8>,
        modified_time: &str,
    ) -> Result<File> {
        let mut meta = serde_json::json!({ "name": name, "modifiedTime": modified_time });
        if existing_id.is_none() {
            meta["parents"] = serde_json::json!([parent_id]);
        }
        let boundary = "drive_rs_boundary_7f3a9c";
        let mut body = Vec::with_capacity(data.len() + 512);
        body.extend_from_slice(format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
        body.extend_from_slice(&data);
        body.extend_from_slice(format!("\r\n--{boundary}--").as_bytes());
        let url = match existing_id {
            Some(id) => format!("{UPLOAD}/{id}"),
            None => UPLOAD.to_string(),
        };
        let is_update = existing_id.is_some();
        Ok(self
            .send(&move |c| {
                let rb = if is_update {
                    c.patch(&url)
                } else {
                    c.post(&url)
                };
                rb.query(&[("uploadType", "multipart"), ("fields", FIELDS)])
                    .header(
                        "Content-Type",
                        format!("multipart/related; boundary={boundary}"),
                    )
                    .body(body.clone())
            })?
            .json()?)
    }

    pub fn download(&self, id: &str) -> Result<Vec<u8>> {
        let url = format!("{API}/{id}");
        Ok(self
            .send(&move |c| c.get(&url).query(&[("alt", "media")]))?
            .bytes()?
            .to_vec())
    }

    /// Token marking "now" in the Changes feed.
    pub fn start_page_token(&self) -> Result<String> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct T {
            start_page_token: String,
        }
        let t: T = self
            .send(&|c| c.get(format!("{CHANGES}/startPageToken")))?
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
                    c.get(CHANGES).query(&[
                        ("pageToken", pt.as_str()),
                        ("pageSize", "1000"),
                        ("spaces", "drive"),
                        ("includeRemoved", "true"),
                        ("restrictToMyDrive", "true"),
                        ("fields", &format!("nextPageToken,newStartPageToken,changes(fileId,removed,file({FIELDS},parents,trashed))")),
                    ])
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

/// Escape a value for use inside single quotes in a Drive query.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}
