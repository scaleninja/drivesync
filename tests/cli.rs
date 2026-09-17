//! End-to-end checks of the binary's two output modes. Nothing here touches the network: the
//! commands exercised either need no workspace, or stop before contacting Drive.
use std::path::{Path, PathBuf};
use std::process::Command;

struct Run {
    status: i32,
    stdout: String,
    stderr: String,
}

fn dsync(cwd: &Path, args: &[&str]) -> Run {
    let out = Command::new(env!("CARGO_BIN_EXE_dsync"))
        .args(args)
        .current_dir(cwd)
        .env("NO_COLOR", "1")
        .output()
        .expect("run dsync");
    Run {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// A fresh directory; with `workspace`, one that looks initialized but has no credentials.
fn dir(tag: &str, workspace: bool) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dsync_cli_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    if workspace {
        std::fs::create_dir_all(d.join(".gd")).unwrap();
        std::fs::write(
            d.join(".gd/config.json"),
            r#"{"client_id":"x","client_secret":"y","remote_folder":"backups/lab","remote_folder_id":"1AbC","depth":-1}"#,
        )
        .unwrap();
        std::fs::write(d.join(".driveignore"), ".DS_Store\n").unwrap();
    }
    d
}

/// Every stdout line must be a JSON object with an `event` field; returns them parsed.
fn events(run: &Run) -> Vec<serde_json::Value> {
    assert!(
        run.stderr.is_empty(),
        "stderr must stay silent in JSON mode: {:?}",
        run.stderr
    );
    run.stdout
        .lines()
        .map(|l| {
            let v: serde_json::Value =
                serde_json::from_str(l).unwrap_or_else(|e| panic!("{l:?}: {e}"));
            assert!(v.get("event").is_some(), "no event field in {l}");
            v
        })
        .collect()
}

#[test]
fn version_in_both_modes_and_flag_positions() {
    let d = dir("version", false);
    let text = dsync(&d, &["version"]);
    assert_eq!(text.status, 0);
    assert!(text
        .stdout
        .starts_with(&format!("dsync {} (", env!("CARGO_PKG_VERSION"))));
    for args in [&["--json", "version"][..], &["version", "--json"][..]] {
        let run = dsync(&d, args);
        assert_eq!(run.status, 0);
        let ev = events(&run);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0]["event"], "version");
        assert_eq!(ev[0]["version"], env!("CARGO_PKG_VERSION"));
    }
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn errors_carry_stable_codes_in_both_modes() {
    let d = dir("codes", false);
    let text = dsync(&d, &["diff"]);
    assert_eq!(text.status, 2);
    assert!(text.stdout.is_empty());
    assert!(
        text.stderr
            .starts_with("error: not inside a dsync workspace"),
        "{}",
        text.stderr
    );
    assert!(
        text.stderr.trim_end().ends_with("[not_workspace]"),
        "{}",
        text.stderr
    );

    let json = dsync(&d, &["--json", "diff"]);
    assert_eq!(json.status, 2);
    let ev = events(&json);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["event"], "error");
    assert_eq!(ev[0]["code"], "not_workspace");
    assert!(ev[0]["message"].as_str().unwrap().contains("dsync init"));

    // A bad argument is a usage error in both modes, and never a stack of clap prose in JSON.
    let usage = dsync(&d, &["--json", "push", "--bogus"]);
    assert_eq!(usage.status, 2);
    let ev = events(&usage);
    assert_eq!(ev[0]["code"], "usage");
    assert!(ev[0]["message"].as_str().unwrap().contains("--bogus"));
    let usage = dsync(&d, &["push", "--bogus"]);
    assert_eq!(usage.status, 2);
    assert!(usage.stderr.contains("--bogus"));
    // --help is never an error.
    assert_eq!(dsync(&d, &["--json", "push", "--help"]).status, 0);
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn status_reports_the_workspace_and_push_needs_credentials() {
    let d = dir("status", true);
    let text = dsync(&d, &["status"]);
    assert_eq!(text.status, 0, "{}", text.stderr);
    assert!(text
        .stdout
        .contains("Remote folder   : My Drive/backups/lab (id 1AbC)"));
    assert!(text.stdout.contains("Depth           : unlimited"));
    assert!(text.stdout.contains("Ignore file     :"));
    assert!(text
        .stdout
        .contains("Access token    : none (run `dsync init`)"));

    let json = dsync(&d, &["status", "--json"]);
    assert_eq!(json.status, 0);
    let ev = events(&json);
    assert_eq!(ev.len(), 1);
    let s = &ev[0];
    assert_eq!(s["event"], "status");
    assert_eq!(s["remote_folder"], "backups/lab");
    assert_eq!(s["remote_folder_id"], "1AbC");
    assert_eq!(s["remote_display"], "My Drive/backups/lab");
    assert!(s["depth"].is_null());
    assert_eq!(s["ignore_file"]["patterns"], 1);
    assert_eq!(s["cache"]["entries"], 0);
    assert!(s["token"].is_null());
    assert!(s["case_insensitive"].is_boolean());

    // Anything that would talk to Drive stops at the missing credentials, with the code.
    for cmd in ["push", "pull", "diff", "update-cache"] {
        let run = dsync(&d, &["--json", cmd]);
        assert_eq!(run.status, 2, "{cmd}");
        let ev = events(&run);
        assert_eq!(ev.last().unwrap()["code"], "no_credentials", "{cmd}");
    }
    let check = dsync(&d, &["--json", "status", "--check"]);
    assert_eq!(check.status, 2);
    let ev = events(&check);
    assert_eq!(ev[0]["event"], "status", "the local facts come first");
    assert_eq!(ev[1]["code"], "no_credentials");
    let text = dsync(&d, &["push"]);
    assert_eq!(text.status, 2);
    assert!(
        text.stderr.trim_end().ends_with("[no_credentials]"),
        "{}",
        text.stderr
    );
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn path_errors_are_reported_before_any_network_use() {
    let d = dir("paths", true);
    // Outside the workspace, and dsync's own state directory.
    let outside = dsync(&d, &["--json", "push", ".."]);
    assert_eq!(outside.status, 2);
    assert_eq!(events(&outside)[0]["code"], "path_invalid");
    let reserved = dsync(&d, &["--json", "push", ".gd"]);
    assert_eq!(reserved.status, 2);
    assert_eq!(events(&reserved)[0]["code"], "path_invalid");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn help_documents_the_new_flags() {
    let d = dir("help", false);
    let push = dsync(&d, &["push", "--help"]).stdout;
    for flag in [
        "--dry-run",
        "--skip-conflicts",
        "--delete",
        "--json",
        "--no-prompt",
    ] {
        assert!(push.contains(flag), "push --help lacks {flag}");
    }
    assert!(dsync(&d, &["init", "--help"])
        .stdout
        .contains("--no-browser"));
    assert!(dsync(&d, &["status", "--help"]).stdout.contains("--check"));
    let root = dsync(&d, &["--help"]).stdout;
    assert!(root.contains("Home:   https://scaleninja.com/drivesync/"));
    assert!(root.contains("Source: https://github.com/scaleninja/drivesync"));
    std::fs::remove_dir_all(&d).unwrap();
}
