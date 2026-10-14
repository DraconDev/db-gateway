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
    // kernel and bind that instead.
    let ephemeral = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("ephemeral probe")
        .local_addr()
        .expect("ephemeral addr");
    drop(ephemeral); // release the port; the test only cares about the address
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("ephemeral probe 2")
        .local_addr()
        .expect("ephemeral addr 2")
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
