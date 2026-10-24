use std::process::Command;

fn dg(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn help_and_version() {
    let help = dg(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("serve"));
    let version = dg(&["--version"]);
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("dg "));
}

#[test]
fn invalid_configuration_fails_without_binding() {
    assert!(
        !dg(&["serve", "--listen", "not-an-address"])
            .status
            .success()
    );
    // The public-bind check is a policy, not a port-number, test: a real
    // local `dg serve` typically sits on 7432 (the documented default
    // and the systemd unit's hardcoded value), so binding that port here
    // would steal it for the duration of the test and make the assertion
    // unable to distinguish "policy correctly refused" from "the bind
    // failed for some other reason". Probe an ephemeral port from the
    // kernel and bind that instead -- the socket is dropped (releasing
    // the port) and only its number is kept.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("ephemeral probe")
        .local_addr()
        .expect("ephemeral addr")
        .port();
    let public = dg(&["serve", "--listen", &format!("0.0.0.0:{port}")]);
    assert!(!public.status.success());
    let log: serde_json::Value = serde_json::from_slice(&public.stderr).unwrap();
    assert_eq!(log["fields"]["code"], "invalid_listen_address");
}

#[test]
fn help_documents_positional_arguments_and_flags() {
    let help = dg(&["query", "--help"]);
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    // A blank `<NAME>`/`<SQL>`/`--args`/`--idempotency-key` was the whole
    // user-facing documentation of the command's inputs.
    for documented in [
        "Database to run the statement against",
        "One SQL statement to execute",
        "JSON array of bind parameters",
        "Replay key",
    ] {
        assert!(
            text.contains(documented),
            "dg query --help lost {documented:?}: {text}"
        );
    }
    let export = dg(&["export", "--help"]);
    let export_text = String::from_utf8_lossy(&export.stdout);
    assert!(
        export_text.contains("Local .sqlite path to write"),
        "dg export --help lost its --out documentation: {export_text}"
    );
}

#[test]
fn import_rejects_a_directory_without_a_size_error() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("owner.key");
    std::fs::write(&key, "a".repeat(64)).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let source = dir.path().join("a-directory");
    std::fs::create_dir_all(&source).unwrap();
    let output = dg(&[
        "--key-file",
        key.to_str().unwrap(),
        "import",
        "newdb",
        "--from",
        source.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    // A directory is a bad path, not a size problem: the 413 promised a
    // bound, and the sibling `dg batch` already answered invalid_request.
    assert!(
        stderr.contains("invalid_request"),
        "directory import must be invalid_request: {stderr}"
    );
    assert!(
        !stderr.contains("result_limit_exceeded"),
        "directory import must not claim a size limit: {stderr}"
    );
}

#[test]
fn the_tui_help_describes_the_tabbed_view_it_actually_opens() {
    // The defect this protects: the view became tabbed, but `dg tui --help`
    // went on describing the tiled screen ("one screen that holds ... at
    // once"). A help text that describes a layout the program no longer has
    // is worse than silence -- an operator reads it, expects four things side
    // by side, and concludes the view is broken when they see tabs.
    //
    // So the help has to name the tabs and the keys that reach them. The
    // assertion is on the behaviour an operator is told to expect, not on any
    // particular sentence, so rewording the prose still passes.
    let help = dg(&["tui", "--help"]);
    assert!(help.status.success(), "dg tui --help failed");
    let text = String::from_utf8_lossy(&help.stdout);

    for tab in ["databases", "service", "backups", "keys", "queries"] {
        assert!(
            text.contains(tab),
            "dg tui --help does not name the {tab:?} tab:\n{text}"
        );
    }
    // The keys that reach those tabs. Assert the navigation keys by name: a
    // looser `contains("tab")` is satisfied by the word "table" in the prose
    // above, so it would pass on a help text that had lost every instruction
    // for moving between tabs (verified by mutation).
    assert!(
        text.contains("1-5"),
        "dg tui --help does not name the number keys that reach the tabs:\n{text}"
    );
    assert!(
        text.contains("shift-tab"),
        "dg tui --help does not say shift-tab goes back a tab:\n{text}"
    );
    // And not the tiled description it used to give.
    assert!(
        !text.contains("at once"),
        "dg tui --help still describes the old tiled screen:\n{text}"
    );
}
