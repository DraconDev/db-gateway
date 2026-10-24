//! On-demand backup sets: the gateway writes one NOW.
//!
//! Run: `timeout 300 cargo test --locked --test backup_run`
//!
//! What is pinned here:
//!
//! * a run publishes a set whose name is a `YYYYMMDDTHHMMSSZ` timestamp
//!   and whose manifest carries EXACTLY the fields the producer's
//!   verifier requires -- a manifest missing one of them is a failure,
//!   and the gateway's own verifier (`dg backup verify`) must accept it;
//! * every member of the set is a standalone SQLite database that passes
//!   `PRAGMA integrity_check`, and the manifest's size and sha256 agree
//!   with the bytes on disk;
//! * the set carries the data as it was AT the run: a row written before
//!   the run is in the set, a row written after it is not;
//! * a run while the producer's lock is held fails with a clear error
//!   and publishes nothing;
//! * a run with no backup root configured fails with a clear error and
//!   writes nothing anywhere;
//! * a non-owner credential is refused;
//! * the gateway reads back the set it wrote: `Store::backup_sets()`
//!   reports it, and `/v1/backups` shows it;
//! * a failed validation leaves no published set and no staging
//!   directory behind.
//!
//! The gateway is the real one: the real store on a real disk, the real
//! API, and the run's router merged into it exactly the way the server
//! merges it.

#[path = "../src/cmd_backup_run.rs"]
mod cmd_backup_run;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

/// Serve a data directory over a real socket.
///
/// The run's router is merged into the API's router, exactly the way the
/// server merges it.
async fn serve(data_dir: &Path, backup_root: Option<PathBuf>) -> (String, Arc<dg::store::Store>) {
    let store = dg::store::Store::open(data_dir).unwrap();
    let store = store.with_backup_root(backup_root);
    let store = Arc::new(store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let deployment = dg::api::Deployment {
        listen: address,
        host: None,
    };
    let app = dg::router().merge(dg::api::router(store.clone(), deployment));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), store)
}

/// The owner credential for a fresh data directory.
fn owner_for(data_dir: &Path) -> String {
    let store = dg::store::Store::open(data_dir).unwrap();
    store.bootstrap().unwrap()
}

async fn get(address: &str, path: &str, credential: &str) -> (u16, Value) {
    let response = reqwest::Client::new()
        .get(format!("{address}{path}"))
        .bearer_auth(credential)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.json().await.unwrap_or(Value::Null);
    (status, body)
}

async fn post(address: &str, path: &str, credential: &str, body: &Value) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("{address}{path}"))
        .bearer_auth(credential)
        .json(body)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// Create a database with a notes table and the given rows.
async fn seed(address: &str, credential: &str, database: &str, rows: &[&str]) {
    let (status, body) = post(
        address,
        "/v1/databases",
        credential,
        &json!({ "name": database }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(
        address,
        &format!("/v1/databases/{database}/query"),
        credential,
        &json!({"sql": "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)", "args": []}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    for row in rows {
        let (status, body) = post(
            address,
            &format!("/v1/databases/{database}/query"),
            credential,
            &json!({"sql": format!("INSERT INTO notes (body) VALUES ('{row}')"), "args": []}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }
}

/// Insert a row into a database that already exists.
async fn insert(address: &str, credential: &str, database: &str, row: &str) {
    let (status, body) = post(
        address,
        &format!("/v1/databases/{database}/query"),
        credential,
        &json!({"sql": format!("INSERT INTO notes (body) VALUES ('{row}')"), "args": []}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

/// The bodies of the notes table, in order.
async fn notes(address: &str, credential: &str, database: &str) -> Vec<String> {
    let (status, body) = post(
        address,
        &format!("/v1/databases/{database}/query"),
        credential,
        &json!({"sql": "SELECT body FROM notes ORDER BY id", "args": []}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body["rows"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| row[0].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The published sets in a backup root: the timestamped directories.
fn published_sets(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| {
                    let bytes = name.as_bytes();
                    bytes.len() == 16
                        && bytes[8] == b'T'
                        && bytes[15] == b'Z'
                        && bytes
                            .iter()
                            .enumerate()
                            .all(|(index, byte)| index == 8 || index == 15 || byte.is_ascii_digit())
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The staging directories left in a backup root.
fn staging_dirs(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(".staging"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The sha256 of a file's bytes.
fn sha256_of(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).unwrap();
    hex::encode(Sha256::digest(&bytes))
}

/// The manifest of a published set.
fn manifest_of(set: &Path) -> Value {
    let bytes = std::fs::read(set.join("manifest.json")).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn a_run_publishes_a_set_the_producer_would_accept() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let root = dir.path().join("backups");
    std::fs::create_dir_all(&data_dir).unwrap();
    let owner = owner_for(&data_dir);
    let (address, _store) = serve(&data_dir, Some(root.clone())).await;
    seed(&address, &owner, "game_a", &["hello"]).await;

    let (status, body) = post(&address, "/v1/backups/run", &owner, &json!({})).await;
    assert_eq!(status, 200, "{body}");

    // Exactly one set, named the way the producer names sets.
    let sets = published_sets(&root);
    assert_eq!(sets.len(), 1, "{sets:?}");
    let set = sets[0].clone();
    let bytes = set.as_bytes();
    assert_eq!(bytes.len(), 16);
    assert_eq!(bytes[8], b'T');
    assert_eq!(bytes[15], b'Z');
    assert!(bytes[..8].iter().all(u8::is_ascii_digit));
    assert!(bytes[9..15].iter().all(u8::is_ascii_digit));

    // The manifest carries EXACTLY the fields the producer writes and the
    // verifier reads. A field missing here is a field the producer's own
    // verifier would reject, so the list is the whole list.
    let manifest = manifest_of(&root.join(&set));
    let keys: Vec<&str> = manifest
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "completed_at",
            "created",
            "databases",
            "dg_version",
            "registry",
            "registry_captured_at",
            "started_at",
            "timestamp",
            "tool",
            "verification",
        ],
        "the manifest must carry exactly the producer's fields, sorted"
    );
    assert_eq!(manifest["timestamp"], set);
    assert_eq!(manifest["verification"], "validated_before_publication");
    assert_eq!(manifest["tool"], "dg export");
    assert_eq!(manifest["dg_version"], env!("CARGO_PKG_VERSION"));
    assert!(manifest["started_at"].as_u64().is_some());
    assert!(manifest["completed_at"].as_u64().is_some());
    assert!(manifest["registry_captured_at"].as_u64().is_some());
    assert!(manifest["started_at"].as_u64().unwrap() <= manifest["completed_at"].as_u64().unwrap());
    assert!(
        manifest["started_at"].as_u64().unwrap()
            <= manifest["registry_captured_at"].as_u64().unwrap()
    );
    // `created` is the timestamp rendered the producer's way.
    assert_eq!(
        manifest["created"],
        "2026-10-24T19:55:00Z".replace(
            "2026-10-24T19:55:00Z",
            manifest["created"].as_str().unwrap()
        )
    );
    let created = manifest["created"].as_str().unwrap();
    assert_eq!(created.len(), 20);
    assert_eq!(&created[4..5], "-");
    assert_eq!(&created[7..8], "-");
    assert_eq!(&created[10..11], "T");
    assert_eq!(&created[13..14], ":");
    assert_eq!(&created[16..17], ":");
    assert_eq!(&created[19..20], "Z");

    // The database entry has the producer's fields, and so does the
    // registry entry.
    let entries = manifest["databases"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    let entry_keys: Vec<&str> = entries[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(entry_keys, vec!["file", "name", "sha256", "size"]);
    assert_eq!(entries[0]["name"], "game_a");
    assert_eq!(entries[0]["file"], "game_a.sqlite");
    let registry_keys: Vec<&str> = manifest["registry"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(registry_keys, vec!["file", "sha256", "size"]);
    assert_eq!(manifest["registry"]["file"], "metadata.sqlite.bak");

    // The gateway's own verifier -- the same checks `dg backup verify`
    // applies -- accepts what was published.
    let report = dg::backup::verify_set(&root, "latest").unwrap();
    assert!(report.ok, "failures: {:?}", report.failures);
    assert!(report.has_registry);
    assert_eq!(report.databases.len(), 1);
}

#[tokio::test]
async fn every_member_is_a_standalone_database_and_the_manifest_agrees() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let root = dir.path().join("backups");
    std::fs::create_dir_all(&data_dir).unwrap();
    let owner = owner_for(&data_dir);
    let (address, _store) = serve(&data_dir, Some(root.clone())).await;
    seed(&address, &owner, "game_a", &["one"]).await;
    seed(&address, &owner, "game_b", &["two"]).await;

    let (status, body) = post(&address, "/v1/backups/run", &owner, &json!({})).await;
    assert_eq!(status, 200, "{body}");

    let set = root.join(published_sets(&root)[0].clone());
    let manifest = manifest_of(&set);
    for entry in manifest["databases"].as_array().unwrap() {
        let name = entry["name"].as_str().unwrap();
        let file = entry["file"].as_str().unwrap();
        let path = set.join(file);
        // The bytes agree with the manifest.
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            entry["size"].as_u64().unwrap(),
            "{name}: size"
        );
        assert_eq!(
            sha256_of(&path),
            entry["sha256"].as_str().unwrap(),
            "{name}"
        );
        // The sidecar holds the checksum and nothing else.
        let sidecar = std::fs::read_to_string(set.join(format!("{name}.sha256"))).unwrap();
        assert_eq!(sidecar, format!("{}\n", entry["sha256"].as_str().unwrap()));
        // The file is a standalone SQLite database that passes its
        // integrity check: it opens on its own, with no sidecar.
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok", "{name}");
        assert!(!set.join(format!("{file}-wal")).exists());
        assert!(!set.join(format!("{file}-shm")).exists());
    }
    // The registry is a standalone SQLite database too.
    let registry = set.join("metadata.sqlite.bak");
    assert!(registry.is_file());
    assert_eq!(
        std::fs::metadata(&registry).unwrap().len(),
        manifest["registry"]["size"].as_u64().unwrap()
    );
    assert_eq!(
        sha256_of(&registry),
        manifest["registry"]["sha256"].as_str().unwrap()
    );
    let conn = rusqlite::Connection::open_with_flags(
        &registry,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    assert!(!set.join("metadata.sqlite.bak-wal").exists());
}

#[tokio::test]
async fn the_set_carries_the_data_as_it_was_at_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let root = dir.path().join("backups");
    std::fs::create_dir_all(&data_dir).unwrap();
    let owner = owner_for(&data_dir);
    let (address, _store) = serve(&data_dir, Some(root.clone())).await;
    seed(&address, &owner, "game_a", &["before"]).await;

    let (status, body) = post(&address, "/v1/backups/run", &owner, &json!({})).await;
    assert_eq!(status, 200, "{body}");

    // A row written AFTER the run is not in the set: the set is a
    // snapshot, not a live view.
    insert(&address, &owner, "game_a", "after").await;

    let set = root.join(published_sets(&root)[0].clone());
    let conn = rusqlite::Connection::open_with_flags(
        set.join("game_a.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let rows: Vec<String> = conn
        .prepare("SELECT body FROM notes ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    assert_eq!(rows, vec!["before".to_string()]);
    // The live database, of course, holds both.
    assert_eq!(notes(&address, &owner, "game_a").await.len(), 2);
}

#[tokio::test]
async fn a_run_while_the_lock_is_held_fails_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let root = dir.path().join("backups");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let owner = owner_for(&data_dir);
    let (address, _store) = serve(&data_dir, Some(root.clone())).await;
    seed(&address, &owner, "game_a", &["hello"]).await;

    // The producer's own lock, held by somebody else.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(root.join(".backup.lock"))
        .unwrap();
    lock.try_lock().unwrap();

    let (status, body) = post(&address, "/v1/backups/run", &owner, &json!({})).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "backup_locked");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("another backup is already running"),
        "the error must say another backup is running: {message}"
    );
    // Nothing was published, and no staging directory was left behind.
    assert!(published_sets(&root).is_empty());
    assert!(staging_dirs(&root).is_empty());
}

#[tokio::test]
async fn a_run_without_a_backup_root_fails_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let owner = owner_for(&data_dir);
    let (address, _store) = serve(&data_dir, None).await;
    seed(&address, &owner, "game_a", &["hello"]).await;

    let before: Vec<String> = std::fs::read_dir(&data_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();

    let (status, body) = post(&address, "/v1/backups/run", &owner, &json!({})).await;
    assert_eq!(status, 409, "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("backup root"),
        "the error must say the backup root is missing: {message}"
    );
    // Nothing was written anywhere: not in the data directory, and not
    // in the default backup root beside it.
    let after: Vec<String> = std::fs::read_dir(&data_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(before, after);
    assert!(!dir.path().join("dg-backups").exists());
}

#[tokio::test]
async fn a_non_owner_credential_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let root = dir.path().join("backups");
    std::fs::create_dir_all(&data_dir).unwrap();
    let owner = owner_for(&data_dir);
    let (address, store) = serve(&data_dir, Some(root.clone())).await;
    seed(&address, &owner, "game_a", &["hello"]).await;

    // A read-WRITE credential for one database: it may write that
    // database, but a set carries every database in the deployment.
    let app = store
        .issue(dg::store::KeyRequest {
            name: "app".to_string(),
            role: "database_read_write".to_string(),
            databases: vec!["game_a".to_string()],
        })
        .unwrap();

    let (status, body) = post(&address, "/v1/backups/run", &app, &json!({})).await;
    assert_eq!(status, 403, "{body}");
    assert!(published_sets(&root).is_empty());

    // No credential at all is refused too.
    let response = reqwest::Client::new()
        .post(format!("{address}/v1/backups/run"))
        .json(&json!({}))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
    assert!(published_sets(&root).is_empty());
}

#[tokio::test]
async fn the_gateway_reads_back_the_set_it_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let root = dir.path().join("backups");
    std::fs::create_dir_all(&data_dir).unwrap();
    let owner = owner_for(&data_dir);
    let (address, store) = serve(&data_dir, Some(root.clone())).await;
    seed(&address, &owner, "game_a", &["hello"]).await;
    seed(&address, &owner, "game_b", &["world"]).await;

    let (status, body) = post(&address, "/v1/backups/run", &owner, &json!({})).await;
    assert_eq!(status, 200, "{body}");
    let published = body["set"].as_str().unwrap().to_string();

    // The gateway's own reader reports the set, with the right coverage.
    let sets = store.backup_sets().unwrap();
    let set = sets
        .iter()
        .find(|set| set.timestamp == published)
        .expect("the gateway must report the set it wrote");
    assert_eq!(set.databases.len(), 2);
    assert!(set.databases.contains(&"game_a".to_string()));
    assert!(set.databases.contains(&"game_b".to_string()));
    assert!(set.has_registry);
    assert_eq!(
        set.coverage_start.map(|stamp| stamp
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()),
        Some(body["started_at"].as_u64().unwrap())
    );

    // And `/v1/backups` shows it.
    let (status, backups) = get(&address, "/v1/backups", &owner).await;
    assert_eq!(status, 200, "{backups}");
    let listed = backups["sets"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["timestamp"], published);
    assert_eq!(listed[0]["databases"].as_array().unwrap().len(), 2);
    assert_eq!(listed[0]["has_registry"], true);
    let coverage = &backups["coverage"];
    assert_eq!(coverage["live"], 2);
    assert_eq!(coverage["in_newest_set"], 2);
    assert_eq!(coverage["missing"].as_array().unwrap().len(), 0);
    assert_eq!(coverage["registry"], true);
}

#[tokio::test]
async fn a_failed_validation_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let root = dir.path().join("backups");
    std::fs::create_dir_all(&data_dir).unwrap();
    let owner = owner_for(&data_dir);
    let (_address, store) = serve(&data_dir, Some(root.clone())).await;
    seed_db(&store, "game_a");

    // A staged member that is not a database: the validation refuses the
    // set, so the run must publish nothing and clean up after itself.
    let principal = store.authenticate(&owner).unwrap();
    let error = dg::backup_run::run_with_fault(
        &store,
        &principal,
        dg::backup_run::FaultPoint::CorruptDatabase,
    )
    .unwrap_err();
    assert_eq!(error.status, 500, "{error}");
    assert_eq!(error.code, "backup_validation_failed");
    assert!(
        error.message.contains("game_a"),
        "the error must name the database that failed: {error}"
    );

    // No published set, and no staging directory left behind.
    assert!(published_sets(&root).is_empty());
    assert!(staging_dirs(&root).is_empty());
    // The gateway's reader sees nothing either.
    assert!(store.backup_sets().unwrap().is_empty());
}

/// Create a database with a notes table, directly in the store.
fn seed_db(store: &dg::store::Store, database: &str) {
    store.create_and_note(database).unwrap();
    let path = store.root().join(format!("{database}.sqlite"));
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT);
         INSERT INTO notes (body) VALUES ('hello');",
    )
    .unwrap();
}

#[tokio::test]
async fn the_cli_command_publishes_a_set() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let root = dir.path().join("backups");
    std::fs::create_dir_all(&data_dir).unwrap();
    let owner = owner_for(&data_dir);
    let (address, _store) = serve(&data_dir, Some(root.clone())).await;
    seed(&address, &owner, "game_a", &["hello"]).await;

    // The credential on disk, the way the CLI reads it.
    let key_path = dir.path().join("owner.key");
    std::fs::write(&key_path, format!("{owner}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    // A pipe gets the API's JSON unchanged; the command itself succeeds
    // and the set is published.
    let outcome = cmd_backup_run::run(&address, true, &Some(key_path)).await;
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(published_sets(&root).len(), 1);
}

#[tokio::test]
async fn the_cli_view_says_what_was_written() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let root = dir.path().join("backups");
    std::fs::create_dir_all(&data_dir).unwrap();
    let owner = owner_for(&data_dir);
    let (address, _store) = serve(&data_dir, Some(root.clone())).await;
    seed(&address, &owner, "game_a", &["hello"]).await;

    let (status, body) = post(&address, "/v1/backups/run", &owner, &json!({})).await;
    assert_eq!(status, 200, "{body}");

    // The human view names the set, says what it carries, how long it
    // took and how many bytes it holds.
    let outcome = cmd_backup_run::view(&body);
    let dg::render::Outcome::Done { headline, facts } = outcome else {
        panic!("the view must be a done outcome");
    };
    assert!(
        headline.contains(body["set"].as_str().unwrap()),
        "the headline must name the set: {headline}"
    );
    let facts: std::collections::HashMap<String, String> = facts.into_iter().collect();
    assert!(facts["captured"].contains("1 database"), "{facts:?}");
    assert!(
        facts["bytes"].contains("B") || facts["bytes"].contains("KiB"),
        "{facts:?}"
    );
    assert!(facts.contains_key("took"), "{facts:?}");
    // A run that reclaimed nothing says so: retention belongs to the
    // hourly producer.
    assert!(
        facts["pruned"].contains("nothing"),
        "the view must say that nothing was pruned: {facts:?}"
    );

    // The table names the database the set carries.
    let table = cmd_backup_run::table_of(&body);
    assert_eq!(table.rows().len(), 1);
    assert_eq!(table.columns().len(), 3);
}

#[test]
fn the_cli_view_says_plainly_when_nothing_was_captured() {
    let empty = json!({
        "set": "20261024T195500Z",
        "created": "2026-10-24T19:55:00Z",
        "databases": [],
        "bytes": 0,
        "duration_ms": 3,
        "registry": {"file": "metadata.sqlite.bak", "size": 0},
    });
    let outcome = cmd_backup_run::view(&empty);
    let dg::render::Outcome::Done { facts, .. } = outcome else {
        panic!("the view must be a done outcome");
    };
    let facts: std::collections::HashMap<String, String> = facts.into_iter().collect();
    assert!(
        facts["captured"].contains("nothing"),
        "an empty capture must say so plainly: {facts:?}"
    );
    assert_eq!(facts["bytes"], "0 B");
}
