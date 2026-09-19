//! Connection config: env vars + default key file (CLI only).
//!
//! Run: `timeout 300 cargo test --locked --test cli_config`
//!
//! Proves bare `dg` commands (no --server/--key-file flags) resolve
//! credentials as flag > DG_* env > ~/.config/dg/key: env-only works,
//! default-key-file-only works, explicit flags beat a poisoned env,
//! and nothing-anywhere still fails locally with missing_credential.
//! Zero direct HTTP (asserts on argv/stdout/stderr/exit codes only).

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn bare(
    server_env: Option<&str>,
    key_env: Option<&str>,
    home: &std::path::Path,
    args: &[&str],
) -> (bool, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dg"));
    cmd.args(args);
    cmd.env_remove("DG_SERVER");
    cmd.env_remove("DG_KEY_FILE");
    if let Some(s) = server_env {
        cmd.env("DG_SERVER", s);
    }
    if let Some(k) = key_env {
        cmd.env("DG_KEY_FILE", k);
    }
    cmd.env("HOME", home);
    let out = cmd.output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn bare_commands_resolve_flag_over_env_over_default() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "cfg_db"]);
    let dir = tempfile::tempdir().unwrap();
    let key_src = dir.path().join("owner.key");
    std::fs::copy(&fleet.cli.key_file, &key_src).unwrap();

    // Home without any default key file.
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    // 1. Env-only: bare `list` works.
    let (ok, stdout, stderr) = bare(
        Some(&fleet.cli.server),
        Some(key_src.to_str().unwrap()),
        &home,
        &["list"],
    );
    assert!(ok, "env-only list failed: {stdout} {stderr}");
    assert!(stdout.contains("cfg_db"), "env-only list wrong: {stdout}");

    // 2. Default key file only: HOME with .config/dg/key, no DG_KEY_FILE.
    let cfg_dir = home.join(".config/dg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let default_key = cfg_dir.join("key");
    std::fs::copy(&fleet.cli.key_file, &default_key).unwrap();
    std::fs::set_permissions(&default_key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let (ok, stdout, stderr) = bare(Some(&fleet.cli.server), None, &home, &["list"]);
    assert!(ok, "default-key list failed: {stdout} {stderr}");
    assert!(
        stdout.contains("cfg_db"),
        "default-key list wrong: {stdout}"
    );

    // 3. Explicit flags beat a poisoned env.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dg"));
    let out = cmd
        .args([
            "--server",
            &fleet.cli.server,
            "--key-file",
            key_src.to_str().unwrap(),
            "list",
        ])
        .env("DG_SERVER", "http://127.0.0.1:1")
        .env("DG_KEY_FILE", "/nonexistent")
        .env("HOME", &home)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "flag-over-env list failed: {stdout}");
    assert!(
        stdout.contains("cfg_db"),
        "flag-over-env list wrong: {stdout}"
    );

    // 4. Nothing anywhere: local missing_credential failure.
    let empty_home = dir.path().join("empty_home");
    std::fs::create_dir_all(&empty_home).unwrap();
    let (ok, stdout, stderr) = bare(None, None, &empty_home, &["list"]);
    assert!(!ok, "credential-less list unexpectedly succeeded: {stdout}");
    assert!(
        stdout.contains("missing_credential") || stderr.contains("missing_credential"),
        "no missing_credential: {stdout} {stderr}"
    );
}
