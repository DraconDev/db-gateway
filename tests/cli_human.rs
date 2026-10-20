//! Contract tests for the human output of every command.
//!
//! The rule is one rule: a terminal gets a human view, a pipe or
//! `--json` gets JSON. These tests hold every command to it, because a
//! command that prints raw JSON on a terminal is a command that looks
//! unfinished.
//!
//! Run: `timeout 300 cargo test --locked --test cli_human`

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use std::process::Command;

/// Strip ANSI escapes so a text assertion sees the words.
fn plain(screen: &str) -> String {
    let mut out = String::with_capacity(screen.len());
    let mut chars = screen.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
        }
        for c in chars.by_ref() {
            if ('\u{40}'..='\u{7e}').contains(&c) {
                break;
            }
        }
    }
    out
}

/// Run a command under a pty so the CLI takes its terminal path.
fn pty(args: &[&str]) -> (bool, String) {
    let out = Command::new("script")
        .args(["-qec", &args.join(" "), "/dev/null"])
        .output()
        .unwrap();
    (
        out.status.success(),
        plain(&String::from_utf8_lossy(&out.stdout)),
    )
}

/// Run a command with a pipe, so the CLI takes its machine path.
///
/// The binary is added by the helper, so the arguments are the
/// command's own.
fn piped(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn every_mutating_command_shows_a_human_view_on_a_terminal() {
    // Each of these commands used to print raw JSON on a terminal. The
    // list is the contract: a command that is not on it is a command
    // whose human output was forgotten.
    let fleet = Fleet::boot();
    let server = &fleet.cli.server;
    let key = fleet.cli.key_file.to_str().unwrap();
    let dg = env!("CARGO_BIN_EXE_dg");

    let cases: Vec<(&str, Vec<&str>, &str)> = vec![
        (
            "create",
            vec![
                dg,
                "--server",
                server,
                "--key-file",
                key,
                "create",
                "human_db",
            ],
            "created human_db",
        ),
        (
            "key create",
            vec![
                dg,
                "--server",
                server,
                "--key-file",
                key,
                "key",
                "create",
                "human_bot",
                "--role",
                "database_read_only",
                "--database",
                "human_db",
            ],
            "issued human_bot",
        ),
        (
            "key revoke",
            vec![
                dg,
                "--server",
                server,
                "--key-file",
                key,
                "key",
                "revoke",
                "human_bot",
            ],
            "revoked human_bot",
        ),
        (
            "delete",
            vec![
                dg,
                "--server",
                server,
                "--key-file",
                key,
                "delete",
                "human_db",
                "--confirm",
                "human_db",
            ],
            "tombstoned human_db",
        ),
    ];

    for (label, args, expected) in cases {
        let (ok, screen) = pty(&args);
        assert!(ok, "{label} failed: {screen}");
        assert!(
            screen.contains(expected),
            "{label} did not show a human view: {screen}"
        );
        // A human view is not JSON: the point is that it reads.
        assert!(
            !screen.trim_start().starts_with('{'),
            "{label} printed raw JSON on a terminal: {screen}"
        );
    }
}

#[test]
fn every_mutating_command_still_emits_json_when_piped() {
    // The human view must not cost the machine surface.
    let fleet = Fleet::boot();
    let server = &fleet.cli.server;
    let key = fleet.cli.key_file.to_str().unwrap();

    let cases: Vec<Vec<&str>> = vec![
        vec!["--server", server, "--key-file", key, "create", "piped_db"],
        vec![
            "--server",
            server,
            "--key-file",
            key,
            "key",
            "create",
            "piped_bot",
            "--role",
            "database_read_only",
            "--database",
            "piped_db",
        ],
        vec![
            "--server",
            server,
            "--key-file",
            key,
            "key",
            "revoke",
            "piped_bot",
        ],
        vec![
            "--server",
            server,
            "--key-file",
            key,
            "delete",
            "piped_db",
            "--confirm",
            "piped_db",
        ],
    ];

    for args in cases {
        let (ok, stdout) = piped(&args);
        assert!(ok, "piped `{args:?}` failed: {stdout}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&stdout).is_ok(),
            "piped `{args:?}` is not JSON: {stdout}"
        );
    }
}

#[test]
fn a_command_is_not_run_twice() {
    // The human view must not be drawn FROM a request that then runs
    // again: `dg create` would create the database, fail to create it
    // a second time, and report the failure of a command that
    // succeeded.
    let fleet = Fleet::boot();
    let server = &fleet.cli.server;
    let key = fleet.cli.key_file.to_str().unwrap();
    let dg = env!("CARGO_BIN_EXE_dg");

    let (ok, screen) = pty(&[
        dg,
        "--server",
        server,
        "--key-file",
        key,
        "create",
        "once_db",
    ]);
    assert!(ok, "create failed: {screen}");
    assert!(screen.contains("created once_db"), "{screen}");
    // The second run finds the database already there, which is what
    // proves the first run created it exactly once.
    let (ok, screen) = pty(&[
        dg,
        "--server",
        server,
        "--key-file",
        key,
        "create",
        "once_db",
    ]);
    assert!(!ok, "the second create succeeded: {screen}");
    assert!(
        screen.contains("database_exists"),
        "the second create did not report the conflict: {screen}"
    );
}

#[test]
fn dg_init_shows_the_key_to_a_person_and_json_to_a_pipe() {
    // The owner key is shown once and never again, so it has to be on
    // the screen for a person AND on stdout for a pipe: the documented
    // workflow extracts it with a JSON parser.
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let dg = env!("CARGO_BIN_EXE_dg");

    // The terminal view.
    let (ok, screen) = pty(&[dg, "init", "--data-dir", data_dir.to_str().unwrap()]);
    assert!(ok, "init failed: {screen}");
    assert!(
        screen.contains("owner key"),
        "the terminal view does not show the key: {screen}"
    );
    // The key itself is 64 hex characters, so it can be picked out of
    // the view.
    assert!(
        screen.contains(&"a".repeat(64))
            || screen
                .split_whitespace()
                .any(|word| { word.len() == 64 && word.chars().all(|c| c.is_ascii_hexdigit()) }),
        "the terminal view does not carry the key: {screen}"
    );

    // The piped view: the documented extraction still works.
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let (ok, stdout) = piped(&["init", "--data-dir", data_dir.to_str().unwrap()]);
    assert!(ok, "init failed: {stdout}");
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("not JSON: {stdout}"));
    let owner = parsed["owner_key"].as_str().unwrap_or("");
    assert_eq!(
        owner.len(),
        64,
        "the owner key is not 64 characters: {stdout}"
    );
    assert!(
        owner.chars().all(|c| c.is_ascii_hexdigit()),
        "the owner key is not hex: {stdout}"
    );
}

#[test]
fn dg_export_shows_what_it_wrote_and_its_hash() {
    // An export is the command where the operator most wants the
    // facts: how big the file is, and the hash to check it against.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "export_db"]);
    fleet
        .cli
        .q("seed", "export_db", "CREATE TABLE t (id INTEGER)");
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.sqlite");
    let server = &fleet.cli.server;
    let key = fleet.cli.key_file.to_str().unwrap();
    let dg = env!("CARGO_BIN_EXE_dg");

    let (ok, screen) = pty(&[
        dg,
        "--server",
        server,
        "--key-file",
        key,
        "export",
        "export_db",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert!(ok, "export failed: {screen}");
    assert!(screen.contains("exported export_db"), "{screen}");
    assert!(screen.contains("size"), "no size: {screen}");
    assert!(screen.contains("sha256"), "no hash: {screen}");

    // And the piped view is still JSON.
    let out = dir.path().join("out2.sqlite");
    let (ok, stdout) = piped(&[
        "--server",
        server,
        "--key-file",
        key,
        "export",
        "export_db",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert!(ok, "export failed: {stdout}");
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("not JSON: {stdout}"));
    assert_eq!(parsed["name"], "export_db");
    assert!(parsed["size"].as_u64().unwrap() > 0);
    assert!(parsed["sha256"].as_str().unwrap().len() == 64);
}

#[test]
fn a_failing_command_shows_a_one_line_error_on_a_terminal() {
    // The rule covers failures as well as successes: an error envelope
    // is JSON, and a terminal shows the error in words.
    let fleet = Fleet::boot();
    let server = &fleet.cli.server;
    let key = fleet.cli.key_file.to_str().unwrap();
    let dg = env!("CARGO_BIN_EXE_dg");

    let (ok, screen) = pty(&[
        dg,
        "--server",
        server,
        "--key-file",
        key,
        "export",
        "no_such_db",
        "--out",
        "/tmp/never-written.sqlite",
    ]);
    assert!(!ok, "the export should have failed: {screen}");
    assert!(
        screen.contains("database_not_found"),
        "the error is not shown in words: {screen}"
    );
    assert!(
        !screen.contains("\"error\""),
        "the error envelope is drawn raw on a terminal: {screen}"
    );

    // And a pipe still gets the envelope.
    let (ok, stdout) = piped(&[
        "--server",
        server,
        "--key-file",
        key,
        "export",
        "no_such_db",
        "--out",
        "/tmp/never-written.sqlite",
    ]);
    assert!(!ok, "the export should have failed: {stdout}");
    let envelope: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("not JSON: {stdout}"));
    assert_eq!(envelope["error"]["code"], "database_not_found", "{stdout}");
}

#[test]
fn the_backup_digest_is_the_one_deliberate_exception() {
    // The digest is a REPORT, not a command with a machine form: the
    // scheduled unit runs it and its output lands in the journal,
    // where a human reads it. So the human note is the default even
    // when stdout is a pipe, and `--json` asks for the structured
    // form. Every other command follows the rule.
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let dg = env!("CARGO_BIN_EXE_dg");

    // The human note, even through a pipe.
    let (ok, stdout) = piped(&["backup", "digest", "--data-dir", data_dir.to_str().unwrap()]);
    // The digest exits nonzero when a database is unrepresented, which
    // is the point of it.
    let _ = ok;
    assert!(
        stdout.contains("backup digest"),
        "the human note is not shown: {stdout}"
    );

    // The structured form, when asked for.
    let (ok, stdout) = piped(&[
        "backup",
        "digest",
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--json",
    ]);
    let _ = ok;
    let note: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("not JSON: {stdout}"));
    assert_eq!(note["kind"], "backup_digest", "{stdout}");
}
