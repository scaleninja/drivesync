// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ScaleNinja
// DriveSync (dsync) — https://github.com/scaleninja/drivesync

//! OAuth2 installed-app flow (loopback redirect) and access-token refresh.
use crate::config::{save_json, Config, Credentials};
use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use serde::Deserialize;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/drive";
/// Refresh when fewer than this many seconds of validity remain.
const REFRESH_MARGIN_SECS: i64 = 60;

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: i64,
    refresh_token: Option<String>,
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub struct Auth {
    http: Client,
    config: Config,
    creds: Credentials,
    creds_path: PathBuf,
}

impl Auth {
    pub fn new(http: Client, config: Config, creds: Credentials, creds_path: PathBuf) -> Self {
        Self {
            http,
            config,
            creds,
            creds_path,
        }
    }

    pub fn expires_in_secs(&self) -> i64 {
        self.creds.expires_at - now()
    }

    /// Interactive browser login. Returns credentials including a refresh token.
    pub fn login(http: &Client, config: &Config) -> Result<Credentials> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let redirect_uri = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        let mut nonce = [0u8; 16];
        getrandom::getrandom(&mut nonce)
            .map_err(|e| anyhow::anyhow!("generating OAuth state: {e}"))?;
        let state: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let url = format!(
            "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&state={state}",
            urlencoding::encode(&config.client_id),
            urlencoding::encode(&redirect_uri),
            urlencoding::encode(SCOPE)
        );
        println!(
            "Authorize this app by visiting:\n\n  {url}\n\nWaiting for the browser redirect..."
        );
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let _ = std::process::Command::new(opener)
            .arg(&url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();

        // Browsers may open speculative connections that never send a request; skip those.
        let (mut stream, query) = loop {
            let (mut stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
            let mut buf = [0u8; 8192];
            let n = match stream.read(&mut buf) {
                Ok(n) if n > 0 => n,
                _ => continue,
            };
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let query = request
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|target| target.split_once('?'))
                .map(|(_, q)| q.to_string());
            match query {
                Some(q) if q.contains("state=") => break (stream, q),
                _ => {
                    let _ =
                        stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                }
            }
        };
        let query = query.as_str();
        let param = |key: &str| {
            query
                .split('&')
                .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
        };
        if param("state") != Some(state.as_str()) {
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\n\r\n<h2>Invalid OAuth state.</h2>");
            bail!("OAuth redirect carried an unexpected state value; aborting");
        }
        let code = match (param("code"), param("error")) {
            (Some(code), _) => urlencoding::decode(code)?.into_owned(),
            (None, Some(err)) => {
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<h2>Authorization failed.</h2>");
                bail!("authorization denied: {err}")
            }
            _ => bail!("no authorization code in redirect"),
        };
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<h2>dsync: authorized. You can close this tab.</h2>");

        let tok: TokenResponse = http
            .post(TOKEN_URL)
            .form(&[
                ("code", code.as_str()),
                ("client_id", &config.client_id),
                ("client_secret", &config.client_secret),
                ("redirect_uri", &redirect_uri),
                ("grant_type", "authorization_code"),
            ])
            .send()?
            .error_for_status()
            .context("exchanging authorization code")?
            .json()?;
        Ok(Credentials {
            access_token: tok.access_token,
            refresh_token: tok
                .refresh_token
                .context("Google did not return a refresh token; revoke app access and retry")?,
            expires_at: now() + tok.expires_in,
        })
    }

    /// Returns a valid access token, refreshing it first if it is about to expire.
    pub fn token(&mut self) -> Result<String> {
        if self.expires_in_secs() < REFRESH_MARGIN_SECS {
            self.refresh()?;
        }
        Ok(self.creds.access_token.clone())
    }

    /// Refresh only if `used` is still the current token (another thread may have refreshed already).
    pub fn refresh_if_stale(&mut self, used: &str) -> Result<()> {
        if self.creds.access_token == used {
            self.refresh()
        } else {
            Ok(())
        }
    }

    /// Exchange the refresh token for a new access token and persist it.
    pub fn refresh(&mut self) -> Result<()> {
        let resp = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("refresh_token", self.creds.refresh_token.as_str()),
                ("client_id", &self.config.client_id),
                ("client_secret", &self.config.client_secret),
                ("grant_type", "refresh_token"),
            ])
            .send()?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            bail!(
                "token refresh failed ({status}): {body}\nRun `dsync init` again to re-authorize."
            );
        }
        let tok: TokenResponse = resp.json()?;
        self.creds.access_token = tok.access_token;
        self.creds.expires_at = now() + tok.expires_in;
        if let Some(rt) = tok.refresh_token {
            self.creds.refresh_token = rt;
        }
        save_json(&self.creds_path, &self.creds)
    }
}
