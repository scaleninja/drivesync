//! HTTP regression fixtures for interrupted transfers and incremental refresh. All credentials
//! and payloads are synthetic; each test binds its own loopback port and workspace.
use super::*;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

struct Request {
    method: String,
    path: String,
    query: String,
    headers: String,
    body: Vec<u8>,
}
impl Request {
    fn query(&self, key: &str) -> Option<String> {
        reqwest::Url::parse(&format!("http://fixture/?{}", self.query))
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    }
}
struct Reply {
    status: u16,
    headers: String,
    body: String,
}
impl Reply {
    fn json(value: Value) -> Self {
        Self {
            status: 200,
            headers: String::new(),
            body: value.to_string(),
        }
    }
    fn status(status: u16) -> Self {
        Self {
            status,
            headers: String::new(),
            body: String::new(),
        }
    }
    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push_str(&format!("{name}: {value}\r\n"));
        self
    }
}

struct Server {
    base: String,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn start(handler: impl Fn(&Request, &str) -> Reply + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log, done, origin) = (requests.clone(), stop.clone(), base.clone());
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let (mut socket, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("fixture accept: {e}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut data = Vec::new();
                let mut buf = [0; 8192];
                let end = loop {
                    let n = socket.read(&mut buf).unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&buf[..n]);
                    if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&data[..end]).to_string();
                let size: usize = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|n| n.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                while data.len() < end + size {
                    let n = socket.read(&mut buf).unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&buf[..n]);
                }
                let mut line = headers.lines().next().unwrap().split_whitespace();
                let method = line.next().unwrap().to_string();
                let target = line.next().unwrap();
                let (path, query) = target.split_once('?').unwrap_or((target, ""));
                let req = Request {
                    method,
                    path: path.into(),
                    query: query.into(),
                    headers,
                    body: data[end..].to_vec(),
                };
                let reply = handler(&req, &origin);
                log.lock().unwrap().push(req);
                let _ = write!(socket, "HTTP/1.1 {} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}", reply.status, reply.body.len(), reply.headers, reply.body);
            }
        });
        Self {
            base,
            requests,
            stop,
            thread: Some(thread),
        }
    }
    fn drive(&self) -> Drive {
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        Drive::new(
            http.clone(),
            auth::Auth::new(
                http,
                config(),
                config::Credentials {
                    access_token: "synthetic".into(),
                    refresh_token: "synthetic".into(),
                    expires_at: auth::now() + 3600,
                },
                PathBuf::from("unused-synthetic-credentials"),
            ),
        )
        .with_test_server(&self.base)
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}
struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "dsync_regression_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn config() -> Config {
    Config {
        client_id: "synthetic".into(),
        client_secret: "synthetic".into(),
        remote_folder: "".into(),
        remote_folder_id: "ROOT".into(),
        depth: -1,
    }
}
fn file(id: &str, name: &str, parent: &str, folder: bool) -> Value {
    json!({"id":id,"name":name,"parents":[parent],"mimeType":if folder {drive::FOLDER_MIME} else {"application/octet-stream"},"modifiedTime":"2026-09-08T00:00:00Z","size":"6291456","md5Checksum":"old"})
}
fn entry(value: Value) -> sync::Entry {
    serde_json::from_value::<drive::File>(value)
        .unwrap()
        .to_entry()
}

fn upload_case(stale_identity: bool, stale_hash: bool, wrong_response: bool) {
    let dir = Dir::new();
    let local = dir.0.join("f.bin");
    std::fs::write(&local, vec![b'x'; 6 * 1024 * 1024]).unwrap();
    let hash = sync::file_md5(&local).unwrap();
    let meta = std::fs::metadata(&local).unwrap();
    let mtime = meta
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let remote = entry(file("NEW", "f.bin", "ROOT", false));
    let ex = sync::Existing {
        id: "NEW".into(),
        mtime_ms: remote.mtime_ms,
        md5: remote.md5.clone(),
    };
    let mut completed = file(
        if wrong_response { "WRONG" } else { "NEW" },
        "f.bin",
        "ROOT",
        false,
    );
    completed["md5Checksum"] = json!(hash);
    let server = Server::start(move |r, base| match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/files/NEW") => Reply::json(file("NEW", "f.bin", "ROOT", false)),
        ("PATCH", "/upload/NEW") => {
            Reply::json(json!({})).header("Location", &format!("{base}/fresh"))
        }
        ("PUT", "/saved") if r.body.is_empty() => {
            Reply::status(308).header("Range", "bytes=0-262143")
        }
        ("PUT", "/saved" | "/fresh") => Reply::json(completed.clone()),
        _ => Reply::status(404),
    });
    let cache = Cache::open(&dir.0.join("cache.db")).unwrap();
    let session = drive::UploadSession {
        uri: Some(format!("{}/saved", server.base)),
        file_id: if stale_identity { "OLD" } else { "NEW" }.into(),
        parent_id: "ROOT".into(),
        name: "f.bin".into(),
        existing: Some(if stale_identity {
            sync::Existing {
                id: "OLD".into(),
                ..ex.clone()
            }
        } else {
            ex.clone()
        }),
        md5: if stale_hash {
            "stale".into()
        } else {
            hash.clone()
        },
    };
    cache
        .set_upload_session(
            "f.bin",
            meta.len(),
            mtime,
            &serde_json::to_string(&session).unwrap(),
        )
        .unwrap();
    cache.upsert("f.bin", &remote).unwrap();
    // A stale stat-keyed hash must not cause the old session to be reused either.
    cache
        .set_local_hash("f.bin", meta.len(), mtime, "stale")
        .unwrap();
    let failures = sync::exec_push(
        &server.drive(),
        &cache,
        &dir.0,
        "ROOT",
        "",
        "ROOT",
        vec![sync::Action::Upload {
            path: "f.bin".into(),
            existing: Some(ex),
            mtime_ms: mtime,
        }],
        1,
    )
    .unwrap();
    assert_eq!(failures, usize::from(wrong_response));
    assert_eq!(
        cache.entry("f.bin").unwrap().unwrap().id.as_deref(),
        Some("NEW")
    );
    assert!(cache
        .upload_session("f.bin", meta.len(), mtime)
        .unwrap()
        .is_none());
    let requests = server.requests.lock().unwrap();
    if stale_identity || stale_hash {
        assert!(!requests.iter().any(|r| r.path == "/saved"));
        assert!(requests
            .iter()
            .any(|r| r.path == "/fresh" && r.body.len() == 6 * 1024 * 1024));
    } else {
        assert!(!requests.iter().any(|r| r.method == "PATCH"));
        assert!(requests.iter().any(|r| r.path == "/saved"
            && r.body.len() == 6 * 1024 * 1024 - 262144
            && r.headers.contains("bytes 262144-")));
    }
}
#[test]
fn changed_remote_identity_discards_saved_session() {
    upload_case(true, false, false);
}
#[test]
fn same_stat_content_change_discards_saved_session() {
    upload_case(false, true, false);
}
#[test]
fn matching_session_resumes_from_recorded_offset() {
    upload_case(false, false, false);
}
#[test]
fn unexpected_completion_identity_is_not_cached() {
    upload_case(false, false, true);
}

#[test]
fn scoped_create_validates_all_existing_ancestors_first() {
    let server = Server::start(|r, _| match r.path.as_str() {
        "/files/ROOT" => Reply::json(file("ROOT", "root", "", true)),
        "/files/A" => Reply::json(file("A", "a", "OUTSIDE", true)),
        _ => Reply::status(404),
    });
    let cache = Cache::open(Path::new(":memory:")).unwrap();
    cache
        .upsert("a", &entry(file("A", "a", "ROOT", true)))
        .unwrap();
    let ws = Workspace {
        root: PathBuf::from("unused"),
        config: config(),
    };
    let err = ensure_remote_folder(&server.drive(), &cache, &ws, "a/new/deeper").unwrap_err();
    assert!(err.to_string().contains("moved"));
    assert!(server
        .requests
        .lock()
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
    assert!(cache.entry("a/new").unwrap().is_none());
}

#[test]
fn queued_scan_uses_current_path_and_depth_after_restart() {
    let server = Server::start(|r, _| match r.path.as_str() {
        "/files/ROOT" => Reply::json(file("ROOT", "root", "", true)),
        "/changes" => Reply::json(json!({"changes":[],"newStartPageToken":"next"})),
        "/files" => {
            assert!(r.query("q").unwrap().contains("'B' in parents"));
            Reply::json(json!({"files":[file("F","f.txt","B",false)]}))
        }
        _ => Reply::status(404),
    });
    let dir = Dir::new();
    let path = dir.0.join("cache.db");
    {
        let c = Cache::open(&path).unwrap();
        c.upsert("z", &entry(file("A", "z", "ROOT", true))).unwrap();
        c.upsert("z/b", &entry(file("B", "b", "A", true))).unwrap();
        c.set_meta("start_page_token", "before").unwrap();
        c.set_meta("depth", "3").unwrap();
        // Old-version task has both an obsolete path and an obsolete depth.
        c.add_pending("B", "a/b", 0).unwrap();
    }
    let c = Cache::open(&path).unwrap();
    cache::refresh(&server.drive(), &c, "ROOT", 3, false).unwrap();
    assert!(c.entry("z/b/f.txt").unwrap().is_some());
    assert!(c.load("a").unwrap().is_empty());
    assert!(c.pending_walks().unwrap().is_empty());
    assert_eq!(c.meta("start_page_token").unwrap().as_deref(), Some("next"));
}

#[test]
fn duplicate_removal_rediscovers_survivor_and_its_children() {
    let server = Server::start(|r, _| match r.path.as_str() {
        "/files/ROOT" => Reply::json(file("ROOT", "root", "", true)),
        "/changes" => Reply::json(
            json!({"changes":[{"fileId":"OLD","removed":true}],"newStartPageToken":"next"}),
        ),
        "/files" if r.query("q").unwrap().contains("'ROOT' in parents") => {
            Reply::json(json!({"files":[file("OTHER","d","ROOT",true)]}))
        }
        "/files" => Reply::json(json!({"files":[file("F","f.txt","OTHER",false)]})),
        _ => Reply::status(404),
    });
    let c = Cache::open(Path::new(":memory:")).unwrap();
    c.upsert("d", &entry(file("OLD", "d", "ROOT", true)))
        .unwrap();
    c.upsert("d/obsolete", &entry(file("X", "obsolete", "OLD", false)))
        .unwrap();
    c.set_meta("start_page_token", "before").unwrap();
    c.set_meta("depth", "-1").unwrap();
    cache::refresh(&server.drive(), &c, "ROOT", -1, false).unwrap();
    assert_eq!(c.entry("d").unwrap().unwrap().id.as_deref(), Some("OTHER"));
    assert!(c.entry("d/obsolete").unwrap().is_none());
    assert!(c.entry("d/f.txt").unwrap().is_some());
    assert!(c.pending_walks().unwrap().is_empty());
}

#[test]
fn interrupted_create_keeps_generated_id_when_session_expires_or_was_not_saved() {
    for expired in [false, true] {
        let dir = Dir::new();
        let local = dir.0.join("f.bin");
        std::fs::write(&local, vec![b'x'; 6 * 1024 * 1024]).unwrap();
        let hash = sync::file_md5(&local).unwrap();
        let meta = std::fs::metadata(&local).unwrap();
        let mtime = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let mut completed = file("RESERVED", "f.bin", "ROOT", false);
        completed["md5Checksum"] = json!(hash);
        let server = Server::start(move |r, base| match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/files") => Reply::json(json!({"files":[]})),
            ("PUT", "/expired") => Reply::status(404),
            ("POST", "/upload") => {
                let metadata: Value = serde_json::from_slice(&r.body).unwrap();
                assert_eq!(metadata["id"], "RESERVED");
                Reply::json(json!({})).header("Location", &format!("{base}/fresh"))
            }
            ("PUT", "/fresh") => Reply::json(completed.clone()),
            _ => Reply::status(404),
        });
        let cache = Cache::open(&dir.0.join("cache.db")).unwrap();
        let saved = drive::UploadSession {
            uri: expired.then(|| format!("{}/expired", server.base)),
            file_id: "RESERVED".into(),
            parent_id: "ROOT".into(),
            name: "f.bin".into(),
            existing: None,
            md5: hash.clone(),
        };
        cache
            .set_upload_session(
                "f.bin",
                meta.len(),
                mtime,
                &serde_json::to_string(&saved).unwrap(),
            )
            .unwrap();
        let failures = sync::exec_push(
            &server.drive(),
            &cache,
            &dir.0,
            "ROOT",
            "",
            "ROOT",
            vec![sync::Action::Upload {
                path: "f.bin".into(),
                existing: None,
                mtime_ms: mtime,
            }],
            1,
        )
        .unwrap();
        assert_eq!(failures, 0);
        assert_eq!(
            cache.entry("f.bin").unwrap().unwrap().id.as_deref(),
            Some("RESERVED")
        );
        assert!(!server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.path.contains("generateIds")));
    }
}

#[test]
fn batched_ancestor_rename_reconciles_child_at_its_current_path() {
    let server = Server::start(|r, _| match r.path.as_str() {
        "/files/ROOT" => Reply::json(file("ROOT", "root", "", true)),
        "/changes" => Reply::json(json!({"changes":[
            {"fileId":"B","file":file("B","b","A",true)},
            {"fileId":"A","file":file("A","z","ROOT",true)}
        ],"newStartPageToken":"next"})),
        "/files" => {
            let q = r.query("q").unwrap();
            let child = if q.contains("'ROOT' in parents") {
                file("A", "z", "ROOT", true)
            } else if q.contains("'A' in parents") {
                file("B", "b", "A", true)
            } else {
                file("F", "f.txt", "B", false)
            };
            Reply::json(json!({"files":[child]}))
        }
        _ => Reply::status(404),
    });
    let c = Cache::open(Path::new(":memory:")).unwrap();
    c.upsert("a", &entry(file("A", "a", "ROOT", true))).unwrap();
    c.set_meta("start_page_token", "before").unwrap();
    c.set_meta("depth", "-1").unwrap();
    cache::refresh(&server.drive(), &c, "ROOT", -1, false).unwrap();
    assert_eq!(
        c.load("")
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["z", "z/b", "z/b/f.txt"]
    );
    assert!(c.pending_walks().unwrap().is_empty());
    assert_eq!(c.meta("start_page_token").unwrap().as_deref(), Some("next"));
}
