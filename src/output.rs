// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ScaleNinja
// DriveSync (dsync) — https://github.com/scaleninja/drivesync

//! Where output goes. By default dsync prints text for people. With `--json` it prints one
//! JSON object per line on stdout instead (an *event*, tagged by its `event` field) and nothing on
//! stderr, so a GUI or script can follow a run without parsing prose. Errors carry a stable
//! [`ErrorCode`] in both modes.
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};

static JSON_MODE: AtomicBool = AtomicBool::new(false);

pub fn set_json_mode(on: bool) {
    JSON_MODE.store(on, Ordering::SeqCst);
}

pub fn json_mode() -> bool {
    JSON_MODE.load(Ordering::SeqCst)
}

/// Print one event line. Only meaningful in JSON mode; callers that have no human
/// equivalent (byte progress, phases) check [`json_mode`] first.
pub fn emit(event: Value) {
    crate::progress::println(&event.to_string());
}

/// Something worth telling on stdout: the event in JSON mode, `human` otherwise.
pub fn out(event: Value, human: impl FnOnce() -> String) {
    if json_mode() {
        emit(event);
    } else {
        crate::progress::println(&human());
    }
}

/// A diagnostic: the event in JSON mode, `human` on stderr otherwise.
pub fn err(event: Value, human: impl FnOnce() -> String) {
    if json_mode() {
        emit(event);
    } else {
        crate::progress::eprintln(&human());
    }
}

/// A non-fatal problem the run continues past.
pub fn warning(message: &str) {
    err(json!({ "event": "warning", "message": message }), || {
        format!("warning: {message}")
    });
}

/// A Drive name that cannot be a local path (contains `/`, or is `.`/`..`); the entry is skipped.
pub fn unmappable(name: &str) {
    err(
        json!({ "event": "warning", "code": "unmappable", "name": name, "message": "remote name cannot be a local path" }),
        || format!("! skip     remote name {name:?} cannot be a local path"),
    );
}

/// A step of the run has begun (scanning, listing, uploading...). JSON mode only: the spinner
/// carries the same text for people.
pub fn phase(message: &str) {
    if json_mode() {
        emit(json!({ "event": "phase", "message": message }));
    }
}

/// Stable identifiers for failures, so a caller can pick the right recovery without parsing the
/// message. Every fatal error still exits with status 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Bad arguments or an impossible combination of options.
    Usage,
    /// Not inside an initialized folder.
    NotWorkspace,
    /// The workspace has no stored credentials; `dsync init` is needed.
    NoCredentials,
    /// The refresh token was revoked or has expired; `dsync init` is needed.
    AuthExpired,
    /// Any other authorization failure (login denied, token endpoint rejected the request).
    AuthFailed,
    /// The remote folder was deleted, trashed, moved or replaced on Drive.
    RemoteFolderMissing,
    /// The given path is outside the workspace, reserved, excluded, a symlink, or missing.
    PathInvalid,
    /// `--no-prompt` met conflicts and refused to proceed (see `--skip-conflicts`).
    Conflicts,
    /// `--delete` was refused because the source side is empty.
    DeleteRefused,
    /// The run finished, but some items failed.
    TransferFailed,
    /// Google Drive rejected a request or could not be reached.
    Api,
    /// A local filesystem or database error.
    Io,
    /// Anything else.
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Usage => "usage",
            ErrorCode::NotWorkspace => "not_workspace",
            ErrorCode::NoCredentials => "no_credentials",
            ErrorCode::AuthExpired => "auth_expired",
            ErrorCode::AuthFailed => "auth_failed",
            ErrorCode::RemoteFolderMissing => "remote_folder_missing",
            ErrorCode::PathInvalid => "path_invalid",
            ErrorCode::Conflicts => "conflicts",
            ErrorCode::DeleteRefused => "delete_refused",
            ErrorCode::TransferFailed => "transfer_failed",
            ErrorCode::Api => "api",
            ErrorCode::Io => "io",
            ErrorCode::Internal => "internal",
        }
    }
}

/// An error with a code attached. Survives `anyhow` context wrapping: [`code_of`] walks the chain.
#[derive(Debug)]
pub struct Coded {
    pub code: ErrorCode,
    pub message: String,
    /// Extra machine-readable fields for the `error` event (counts, paths).
    pub fields: Value,
}

impl std::fmt::Display for Coded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Coded {}

pub fn coded(code: ErrorCode, message: impl Into<String>) -> anyhow::Error {
    Coded {
        code,
        message: message.into(),
        fields: Value::Null,
    }
    .into()
}

pub fn coded_with(code: ErrorCode, message: impl Into<String>, fields: Value) -> anyhow::Error {
    Coded {
        code,
        message: message.into(),
        fields,
    }
    .into()
}

/// Like `bail!`, with a code.
#[macro_export]
macro_rules! fail {
    ($code:expr, $($arg:tt)*) => {
        return Err($crate::output::coded($code, format!($($arg)*)))
    };
}

/// The code of an error: the innermost explicit one, else inferred from the cause.
pub fn code_of(e: &anyhow::Error) -> (ErrorCode, Value) {
    for cause in e.chain() {
        if let Some(c) = cause.downcast_ref::<Coded>() {
            return (c.code, c.fields.clone());
        }
    }
    for cause in e.chain() {
        if cause.downcast_ref::<crate::drive::ApiError>().is_some()
            || cause.downcast_ref::<reqwest::Error>().is_some()
        {
            return (ErrorCode::Api, Value::Null);
        }
        if cause.downcast_ref::<std::io::Error>().is_some()
            || cause.downcast_ref::<rusqlite::Error>().is_some()
        {
            return (ErrorCode::Io, Value::Null);
        }
    }
    (ErrorCode::Internal, Value::Null)
}

/// Report a fatal error the way the mode wants it: an `error` event, or `error: <message>
/// [<code>]` on stderr.
pub fn fatal(e: &anyhow::Error) {
    let (code, fields) = code_of(e);
    let message = format!("{e:#}");
    if json_mode() {
        let mut event = json!({ "event": "error", "code": code, "message": message });
        if let Value::Object(extra) = fields {
            event.as_object_mut().unwrap().extend(extra);
        }
        emit(event);
    } else {
        eprintln!("error: {message} [{}]", code.as_str());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_survive_context_and_fall_back_by_cause() {
        let e = coded(ErrorCode::NotWorkspace, "nope").context("outer");
        assert_eq!(code_of(&e).0, ErrorCode::NotWorkspace);
        assert_eq!(format!("{e:#}"), "outer: nope");
        let io: anyhow::Error = std::io::Error::other("disk").into();
        assert_eq!(code_of(&io.context("reading")).0, ErrorCode::Io);
        assert_eq!(code_of(&anyhow::anyhow!("plain")).0, ErrorCode::Internal);
        let with = coded_with(
            ErrorCode::TransferFailed,
            "3 failed",
            json!({ "failures": 3 }),
        );
        assert_eq!(code_of(&with).1["failures"], 3);
        assert_eq!(
            serde_json::to_value(ErrorCode::RemoteFolderMissing).unwrap(),
            "remote_folder_missing"
        );
    }
}
