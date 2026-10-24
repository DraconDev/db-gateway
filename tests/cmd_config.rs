//! Named contexts: the config file, its privacy, and the resolution order.
//!
//! Run: `timeout 300 cargo test --locked --test cmd_config`
//!
//! Proves that a named context round-trips (set, list, show, remove), that
//! the config file is created 0600 and is never widened, that the
//! resolution order is flag > context > environment > default context >
//! built-in default, and that a corrupt config file is an error rather
//! than a panic. The module under test is the CLI's own source file, so
//! the tests exercise the code the binary runs.

#[path = "../src/cmd_config.rs"]
mod cmd_config;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The config file inside a temp directory.
fn config_file(dir: &Path) -> PathBuf {
    dir.join("config.toml")
}

/// A credential file, private like the real one.
fn write_key(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, "credential").expect("the key file is written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("the key file is private");
    path
}

#[test]
fn set_list_and_show_round_trip() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let config = config_file(dir.path());
    let key = write_key(dir.path(), "micro2.key");
    // A context whose key file is not there: a broken context has to say
    // so, not look like a working one.
    let missing = dir.path().join("staging.key");

    cmd_config::set(
        &config,
        "micro2",
        "http://127.0.0.1:7432",
        &key,
        true,
        true,
    )
    .expect("the context is saved");
    cmd_config::set(
        &config,
        "my ctx",
        "https://staging.example",
        &missing,
        false,
        true,
    )
    .expect("the second context is saved");

    let listed = cmd_config::contexts(&config).expect("the config is read");
    assert_eq!(listed["default"], "micro2", "the marked context is the default");
    let contexts = listed["contexts"]
        .as_array()
        .expect("the contexts are a list");
    assert_eq!(contexts.len(), 2, "both contexts are listed: {listed}");
    assert_eq!(contexts[0]["name"], "micro2");
    assert_eq!(contexts[0]["server"], "http://127.0.0.1:7432");
    assert_eq!(contexts[0]["key_file"], key.to_string_lossy().as_ref());
    assert_eq!(contexts[0]["key_present"], true, "the key file is there");
    assert_eq!(contexts[1]["name"], "my ctx");
    assert_eq!(contexts[1]["server"], "https://staging.example");
    assert_eq!(
        contexts[1]["key_present"], false,
        "the missing key file is said out loud: {listed}"
    );

    let shown = cmd_config::context(&config, "my ctx").expect("the context is shown");
    assert_eq!(shown["name"], "my ctx");
    assert_eq!(shown["server"], "https://staging.example");
    assert_eq!(shown["key_present"], false);
    assert_eq!(shown["default"], false);
    assert_eq!(shown["config_path"], config.to_string_lossy().as_ref());

    // The printed forms run too: they are the commands an operator types.
    cmd_config::list(&config, true).expect("the list is printed");
    cmd_config::show(&config, "micro2", true).expect("the context is printed");

    // The file holds the pair and never the credential, and the name with
    // a space is quoted so a TOML parser can read it.
    let text = std::fs::read_to_string(&config).expect("the config file is read");
    assert!(text.contains("[contexts.micro2]"), "{text}");
    assert!(text.contains("[contexts.\"my ctx\"]"), "{text}");
    assert!(
        !text.contains("credential"),
        "the credential is not in the config: {text}"
    );
}

#[test]
fn remove_deletes_only_that_context() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let config = config_file(dir.path());
    let key = write_key(dir.path(), "micro2.key");
    let other = write_key(dir.path(), "staging.key");

    cmd_config::set(
        &config,
        "micro2",
        "http://127.0.0.1:7432",
        &key,
        true,
        true,
    )
    .expect("the context is saved");
    cmd_config::set(
        &config,
        "staging",
        "https://staging.example",
        &other,
        false,
        true,
    )
    .expect("the second context is saved");

    cmd_config::remove(&config, "staging", true).expect("the context is removed");
    let listed = cmd_config::contexts(&config).expect("the config is read");
    let contexts = listed["contexts"]
        .as_array()
        .expect("the contexts are a list");
    assert_eq!(contexts.len(), 1, "one context is left: {listed}");
    assert_eq!(contexts[0]["name"], "micro2", "the other one is untouched");
    assert_eq!(listed["default"], "micro2", "the default is untouched");

    let error = cmd_config::remove(&config, "staging", true)
        .expect_err("removing a context that is not there is an error");
    assert_eq!(error.code(), "no_such_context");
}

#[test]
fn removing_the_default_context_clears_the_default() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let config = config_file(dir.path());
    let key = write_key(dir.path(), "micro2.key");

    cmd_config::set(
        &config,
        "micro2",
        "http://127.0.0.1:7432",
        &key,
        true,
        true,
    )
    .expect("the context is saved");
    cmd_config::remove(&config, "micro2", true).expect("the context is removed");

    let listed = cmd_config::contexts(&config).expect("the config is read");
    assert_eq!(listed["default"], serde_json::Value::Null);
    assert!(
        listed["contexts"].as_array().expect("a list").is_empty(),
        "no context is left: {listed}"
    );
}

#[test]
fn the_config_file_is_private_and_never_widened() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let config = config_file(dir.path());
    let key = write_key(dir.path(), "micro2.key");

    cmd_config::set(
        &config,
        "micro2",
        "http://127.0.0.1:7432",
        &key,
        false,
        true,
    )
    .expect("the context is saved");
    let mode = std::fs::metadata(&config)
        .expect("the config file is there")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the config file is 0600: {mode:o}");
    assert_eq!(mode & 0o077, 0, "it is not readable by anyone else: {mode:o}");

    // A file that was too open is tightened by a rewrite, never widened.
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644))
        .expect("the mode is loosened");
    cmd_config::set(
        &config,
        "micro2",
        "http://127.0.0.1:7432",
        &key,
        false,
        true,
    )
    .expect("the context is saved again");
    let mode = std::fs::metadata(&config)
        .expect("the config file is there")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the rewrite tightens the mode: {mode:o}");
}

#[test]
fn writing_without_its_directory_is_refused() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let config = dir.path().join("missing/config.toml");
    let key = write_key(dir.path(), "micro2.key");

    let error = cmd_config::set(
        &config,
        "micro2",
        "http://127.0.0.1:7432",
        &key,
        false,
        true,
    )
    .expect_err("a missing directory is an error");
    assert_eq!(error.code(), "config_dir_missing");
    assert!(!config.exists(), "no config file was written");
    assert!(
        !dir.path().join("missing").exists(),
        "the directory tree was not created"
    );
}

#[test]
fn resolve_follows_the_documented_order() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let config = config_file(dir.path());
    let key = write_key(dir.path(), "ctx.key");
    cmd_config::set(
        &config,
        "micro2",
        "http://context:7432",
        &key,
        true,
        true,
    )
    .expect("the context is saved");

    // The flag beats the context.
    let resolved = cmd_config::resolve_with(
        &config,
        Some("micro2"),
        Some("http://flag:7432"),
        Some(Path::new("/flag.key")),
        None,
        None,
    )
    .expect("the context resolves");
    assert_eq!(resolved.server, "http://flag:7432");
    assert_eq!(resolved.key_file, Some(PathBuf::from("/flag.key")));

    // The context beats the environment.
    let resolved = cmd_config::resolve_with(
        &config,
        Some("micro2"),
        None,
        None,
        Some("http://env:7432"),
        Some(Path::new("/env.key")),
    )
    .expect("the context resolves");
    assert_eq!(resolved.server, "http://context:7432");
    assert_eq!(resolved.key_file, Some(key.clone()));

    // The environment beats the default context.
    let resolved = cmd_config::resolve_with(
        &config,
        None,
        None,
        None,
        Some("http://env:7432"),
        Some(Path::new("/env.key")),
    )
    .expect("the default context resolves");
    assert_eq!(resolved.server, "http://env:7432");
    assert_eq!(resolved.key_file, Some(PathBuf::from("/env.key")));

    // The default context beats the built-in defaults.
    let resolved =
        cmd_config::resolve_with(&config, None, None, None, None, None).expect("the default resolves");
    assert_eq!(resolved.server, "http://context:7432");
    assert_eq!(resolved.key_file, Some(key.clone()));

    // With no context and no default, the built-in defaults answer.
    let empty = dir.path().join("empty.toml");
    let resolved =
        cmd_config::resolve_with(&empty, None, None, None, None, None).expect("the defaults resolve");
    assert_eq!(resolved.server, "http://127.0.0.1:7432");
    assert_eq!(resolved.key_file, dg::client::default_key_file());

    // A context that is not there is an error, never a quiet fallback to
    // another gateway.
    let error = cmd_config::resolve_with(&config, Some("nope"), None, None, None, None)
        .expect_err("an unknown context is an error");
    assert_eq!(error.code(), "no_such_context");
}

#[test]
fn a_corrupt_config_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let config = config_file(dir.path());
    for garbage in [
        "this is not a config\n",
        "[contexts.oops]\nserver = 42\n",
        "default = \n",
        "[contexts.\"unterminated]\n",
        "default = \"a\"\ndefault = \"b\"\n",
        "[contexts.oops]\nserver = \"http://x\"\n",
    ] {
        std::fs::write(&config, garbage).expect("the corrupt file is written");
        let error = cmd_config::contexts(&config).expect_err("a corrupt config is an error");
        assert_eq!(error.code(), "config_invalid", "{garbage}");
        let error = cmd_config::resolve_with(&config, None, None, None, None, None)
            .expect_err("a corrupt config is an error");
        assert_eq!(error.code(), "config_invalid", "{garbage}");
    }
}

#[test]
fn the_default_location_is_beside_the_default_key_file() {
    let path = cmd_config::config_path().expect("the config path is known");
    assert!(
        path.ends_with(".config/dg/config.toml"),
        "{}",
        path.display()
    );

    // `dg config path` prints the path; without HOME there is none.
    match cmd_config::path() {
        Ok(()) => {}
        Err(error) => assert_eq!(error.code(), "no_home", "HOME is not set"),
    }

    // The default-path resolution reads the operator's own config file,
    // so only the shape of its answer is asserted here: the order itself
    // is proved above, where every input is given.
    if let Ok(resolved) = cmd_config::resolve(None, None, None) {
        assert!(!resolved.server.is_empty(), "the server is resolved");
    }
}
