//! Branch forks: a copy that can diverge from what it came from.
//!
//! Run: `timeout 300 cargo test --locked --test branch`
//!
//! What is pinned here:
//!
//! * a fork of the LIVE parent carries the parent's tables and rows;
//! * a fork of a MOMENT carries the rows of that moment and not the
//!   rows the parent holds now -- the mutation is what makes that test
//!   mean anything;
//! * a moment before the oldest published set is refused, and the
//!   refusal names the window that IS available;
//! * a fork onto a name that is taken is refused;
//! * an app credential cannot fork;
//! * the branch registry survives a restart;
//! * the branch list never carries the credential.
//!
//! The gateway is the real one: the real store on a real disk, the real
//! API, and the branch router merged into it exactly the way the
//! server merges it.

#[path = "../src/cmd_branch.rs"]
mod cmd_branch;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

/// Serve a data directory over a real socket.
///
/// The branch router is merged here, so this is the same router the
/// server runs.
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

async fn get_raw(address: &str, path: &str, credential: &str) -> (u16, String) {
    let response = reqwest::Client::new()
        .get(format!("{address}{path}"))
        .bearer_auth(credential)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (status, text)
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

async fn delete(address: &str, path: &str, credential: &str) -> (u16, Value) {
    let response = reqwest::Client::new()
        .delete(format!("{address}{path}"))
        .bearer_auth(credential)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// Run one statement against a database.
async fn query(address: &str, credential: &str, database: &str, sql: &str) -> (u16, Value) {
    post(
        address,
        &format!("/v1/databases/{database}/query"),
        credential,
        &json!({"sql": sql, "args": []}),
    )
    .await
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
    let (status, body) = query(
        address,
        credential,
        database,
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    for row in rows {
        let (status, body) = query(
            address,
            credential,
            database,
            &format!("INSERT INTO notes (body) VALUES ('{row}')"),
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }
}

/// The bodies of the notes table, in order.
async fn notes(address: &str, credential: &str, database: &str) -> Vec<String> {
    let (status, body) = query(
        address,
        credential,
        database,
        "SELECT body FROM notes ORDER BY id",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body["rows"]
        .as_array()
        .expect("rows is an array")
        .iter()
        .map(|row| row[0].as_str().unwrap_or_default().to_string())
        .collect()
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Publish a versioned backup set in the producer's layout.
///
/// The set holds the database with the given rows, so a fork at this
/// moment carries exactly these rows.
fn craft_set(root: &Path, stamp: &str, database: &str, rows: &[&str]) {
    let set = root.join(stamp);
    std::fs::create_dir_all(&set).unwrap();
    let member = set.join(format!("{database}.sqlite"));
    {
        let conn = rusqlite::Connection::open(&member).unwrap();
        conn.execute_batch("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)")
            .unwrap();
        for row in rows {
            conn.execute("INSERT INTO notes (body) VALUES (?1)", [row])
                .unwrap();
        }
    }
    let member_bytes = std::fs::read(&member).unwrap();
    let member_sha = sha256(&member_bytes);
    std::fs::write(
        set.join(format!("{database}.sha256")),
        format!("{member_sha}\n"),
    )
    .unwrap();
    // The set carries a registry copy too, and the set is verified
    // whole: a registry that is not a database fails the set.
    let registry = set.join("metadata.sqlite.bak");
    {
        let conn = rusqlite::Connection::open(&registry).unwrap();
        conn.execute_batch("CREATE TABLE registry_note (note TEXT)")
            .unwrap();
    }
    let registry_bytes = std::fs::read(&registry).unwrap();
    let registry_sha = sha256(&registry_bytes);
    let manifest = json!({
        "started_at": 1,
        "completed_at": 2,
        "registry_captured_at": 2,
        "databases": [{
            "name": database,
            "file": format!("{database}.sqlite"),
            "size": member_bytes.len(),
            "sha256": member_sha,
        }],
        "registry": {
            "file": "metadata.sqlite.bak",
            "size": registry_bytes.len(),
            "sha256": registry_sha,
        },
    });
    std::fs::write(set.join("manifest.json"), manifest.to_string()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fork_of_the_live_parent_carries_its_tables_and_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let owner = owner_for(&data_dir);
    let (address, _store) = serve(&data_dir, None).await;
    seed(&address, &owner, "parent", &["one", "two"]).await;

    let (status, outcome) = post(
        &address,
        "/v1/branch",
        &owner,
        &json!({"parent": "parent", "name": "fork", "at": null}),
    )
    .await;
    assert_eq!(status, 200, "{outcome}");
    assert_eq!(outcome["name"], json!("fork"));
    assert_eq!(outcome["parent"], json!("parent"));
    assert_eq!(outcome["source"], json!({"kind": "live"}));

    // The fork is a database like any other: it has the parent's rows.
    assert_eq!(notes(&address, &owner, "fork").await, vec!["one", "two"]);
    // And the parent's tables.
    let (status, body) = query(
        &address,
        &owner,
        "fork",
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='notes'",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["rows"], json!([[1]]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fork_of_a_moment_carries_the_rows_of_that_moment() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let backup_root = dir.path().join("sets");
    std::fs::create_dir_all(&backup_root).unwrap();
    let owner = owner_for(&data_dir);
    let (address, _store) = serve(&data_dir, Some(backup_root.clone())).await;

    seed(&address, &owner, "parent", &["old"]).await;
    // The set is published with the rows the parent holds NOW.
    craft_set(&backup_root, "20261016T000000Z", "parent", &["old"]);
    // The parent moves on. A fork that used the live parent would carry
    // this row, and this test would fail.
    let (status, body) = query(
        &address,
        &owner,
        "parent",
        "INSERT INTO notes (body) VALUES ('new')",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(notes(&address, &owner, "parent").await, vec!["old", "new"]);

    let (status, outcome) = post(
        &address,
        "/v1/branch",
        &owner,
        &json!({
            "parent": "parent",
            "name": "fork_past",
            "at": "2026-10-16T01:00:00Z",
        }),
    )
    .await;
    assert_eq!(status, 200, "{outcome}");
    assert_eq!(
        outcome["source"],
        json!({"kind": "snapshot", "set": "20261016T000000Z"})
    );
    // The fork holds the moment, not the live parent.
    assert_eq!(notes(&address, &owner, "fork_past").await, vec!["old"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_moment_before_the_oldest_set_is_refused_and_the_window_is_named() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let backup_root = dir.path().join("sets");
    std::fs::create_dir_all(&backup_root).unwrap();
    let owner = owner_for(&data_dir);
    let (address, _store) = serve(&data_dir, Some(backup_root.clone())).await;
    seed(&address, &owner, "parent", &["one"]).await;
    craft_set(&backup_root, "20261016T000000Z", "parent", &["one"]);
    craft_set(&backup_root, "20261018T000000Z", "parent", &["one"]);

    let (status, body) = post(
        &address,
        "/v1/branch",
        &owner,
        &json!({
            "parent": "parent",
            "name": "too_early",
            "at": "2026-10-15T00:00:00Z",
        }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    // The refusal names the window that IS available.
    assert!(
        message.contains("20261016T000000Z") && message.contains("20261018T000000Z"),
        "the refusal does not name the available window: {message}"
    );
    // Nothing was published.
    let (status, _) = get(&address, "/v1/branches/too_early", &owner).await;
    assert_eq!(status, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fork_onto_a_name_that_is_taken_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let owner = owner_for(&data_dir);
    let (address, _store) = serve(&data_dir, None).await;
    seed(&address, &owner, "parent", &["one"]).await;
    seed(&address, &owner, "taken", &["other"]).await;

    let (status, body) = post(
        &address,
        "/v1/branch",
        &owner,
        &json!({"parent": "parent", "name": "taken", "at": null}),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    // The database that was there is still there, and still its own.
    assert_eq!(notes(&address, &owner, "taken").await, vec!["other"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_credential_cannot_fork() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let owner = owner_for(&data_dir);
    let (address, _store) = serve(&data_dir, None).await;
    seed(&address, &owner, "parent", &["one"]).await;
    let (status, key) = post(
        &address,
        "/v1/keys",
        &owner,
        &json!({
            "name": "rk",
            "role": "database_read_write",
            "databases": ["parent"],
        }),
    )
    .await;
    assert_eq!(status, 200, "{key}");
    let app_key = key["key"].as_str().unwrap().to_string();

    let (status, body) = post(
        &address,
        "/v1/branch",
        &app_key,
        &json!({"parent": "parent", "name": "fork", "at": null}),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    // And it cannot read the branch list either.
    let (status, _) = get(&address, "/v1/branches", &app_key).await;
    assert_eq!(status, 403);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_branch_registry_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let owner = owner_for(&data_dir);
    let (address, _store) = serve(&data_dir, None).await;
    seed(&address, &owner, "parent", &["one"]).await;
    let (status, outcome) = post(
        &address,
        "/v1/branch",
        &owner,
        &json!({"parent": "parent", "name": "fork", "at": null}),
    )
    .await;
    assert_eq!(status, 200, "{outcome}");

    // The record is on disk, not only in the running process.
    let registry = dg::store::BranchRegistry::load_from(&data_dir);
    let records = registry.all();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].name, "fork");
    assert_eq!(records[0].parent, "parent");
    assert_eq!(records[0].set, None);

    // A gateway that starts on the same directory knows the branch.
    let (address, _store) = serve(&data_dir, None).await;
    let (status, body) = get(&address, "/v1/branches", &owner).await;
    assert_eq!(status, 200, "{body}");
    let branches = body["branches"].as_array().expect("branches is an array");
    assert_eq!(branches.len(), 1, "{body}");
    assert_eq!(branches[0]["name"], json!("fork"));
    assert_eq!(branches[0]["parent"], json!("parent"));
    assert_eq!(branches[0]["source"], json!({"kind": "live"}));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_branch_list_returns_the_records_and_never_the_credential() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let owner = owner_for(&data_dir);
    let (address, _store) = serve(&data_dir, None).await;
    seed(&address, &owner, "parent", &["one"]).await;
    let (status, _) = post(
        &address,
        "/v1/branch",
        &owner,
        &json!({"parent": "parent", "name": "fork", "at": null}),
    )
    .await;
    assert_eq!(status, 200);

    let (status, text) = get_raw(&address, "/v1/branches", &owner).await;
    assert_eq!(status, 200);
    assert!(
        !text.contains(&owner),
        "the branch list carries the credential: {text}"
    );
    assert!(text.contains("\"fork\""), "{text}");
    assert!(text.contains("\"parent\""), "{text}");

    // One branch, in detail.
    let (status, body) = get(&address, "/v1/branches/fork", &owner).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["name"], json!("fork"));
    assert_eq!(body["parent"], json!("parent"));
    assert_eq!(body["source"], json!({"kind": "live"}));
    assert!(body["forked_at"].as_u64().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_branch_deletes_the_database_and_forgets_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let owner = owner_for(&data_dir);
    let (address, _store) = serve(&data_dir, None).await;
    seed(&address, &owner, "parent", &["one"]).await;
    let (status, _) = post(
        &address,
        "/v1/branch",
        &owner,
        &json!({"parent": "parent", "name": "fork", "at": null}),
    )
    .await;
    assert_eq!(status, 200);

    let (status, body) = delete(&address, "/v1/branches/fork", &owner).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["deleted"], json!(true));
    // The database is gone: it can neither be read nor recreated.
    let (status, _) = get(&address, "/v1/branches/fork", &owner).await;
    assert_eq!(status, 404);
    let (status, _) = query(&address, &owner, "fork", "SELECT 1").await;
    assert_eq!(status, 404);
    // The record is gone too.
    let (status, body) = get(&address, "/v1/branches", &owner).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["branches"].as_array().map(Vec::len), Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cli_reads_the_same_branches_the_api_serves() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let owner = owner_for(&data_dir);
    let (address, _store) = serve(&data_dir, None).await;
    seed(&address, &owner, "parent", &["one"]).await;
    let key_file = dir.path().join("owner.key");
    std::fs::write(&key_file, &owner).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let key_path = Some(key_file);

    // The CLI forks through the same endpoint an operator would use.
    cmd_branch::create(
        &address,
        true,
        &key_path,
        "parent".to_string(),
        "fork".to_string(),
        None,
    )
    .await
    .expect("the CLI forks");
    cmd_branch::list(&address, true, &key_path)
        .await
        .expect("the CLI lists the branches");
    cmd_branch::show(&address, true, &key_path, "fork".to_string())
        .await
        .expect("the CLI shows one branch");

    // A destructive act still needs the name repeated.
    let refused = cmd_branch::delete(&address, true, &key_path, "fork".to_string(), None).await;
    assert!(refused.is_err(), "the CLI deleted without a confirmation");
    cmd_branch::delete(
        &address,
        true,
        &key_path,
        "fork".to_string(),
        Some("fork".to_string()),
    )
    .await
    .expect("the CLI deletes a confirmed branch");

    let (status, body) = get(&address, "/v1/branches", &owner).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["branches"].as_array().map(Vec::len), Some(0));
}
