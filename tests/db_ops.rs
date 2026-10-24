//! The three questions about a database: the plan SQLite would follow,
//! whether the file is sound, and how much space the file is wasting.
//!
//! The server is the real one: the same API router the gateway serves,
//! with the database operations router merged in, on a loopback socket,
//! over the real store on disk. Only the process boundary is missing --
//! the endpoints under test live in their own module, so the test
//! builds the same router the server builds rather than waiting for a
//! binary to be rebuilt around them.

#[path = "../src/cmd_db_ops.rs"]
mod cmd_db_ops;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// A gateway listening on a loopback port.
struct Server {
    address: String,
    store: Arc<dg::store::Store>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Start a gateway over `dir` and return it with the owner credential.
async fn start(dir: &Path) -> (Server, String) {
    let store = Arc::new(dg::store::Store::open(dir).expect("the store opens"));
    let owner = store.bootstrap().expect("the store bootstraps");
    let deployment = dg::api::Deployment {
        listen: "127.0.0.1:0".parse().expect("a loopback address"),
        host: None,
    };
    // The merge the server itself does: the API router, which already
    // carries the database operations router.
    let app = dg::api::router(store.clone(), deployment);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let address = listener.local_addr().expect("a bound address").to_string();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("the server serves");
    });
    (
        Server {
            address,
            store,
            handle,
        },
        owner,
    )
}

async fn get(address: &str, path: &str, credential: &str) -> (u16, Value) {
    let response = reqwest::Client::new()
        .get(format!("http://{address}{path}"))
        .bearer_auth(credential)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .expect("the request completes");
    let status = response.status().as_u16();
    let body = response.json().await.unwrap_or(Value::Null);
    (status, body)
}

async fn post(address: &str, path: &str, credential: &str, body: Option<Value>) -> (u16, Value) {
    let mut request = reqwest::Client::new()
        .post(format!("http://{address}{path}"))
        .bearer_auth(credential)
        .timeout(Duration::from_secs(60));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("the request completes");
    let status = response.status().as_u16();
    let body = response.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// Run a statement through the ordinary query path.
async fn query(address: &str, credential: &str, database: &str, sql: &str) -> (u16, Value) {
    post(
        address,
        &format!("/v1/databases/{database}/query"),
        credential,
        Some(json!({"sql": sql, "args": []})),
    )
    .await
}

/// The details SQLite gave for each step of a plan.
fn details(plan: &Value) -> Vec<String> {
    plan["plan"]
        .as_array()
        .expect("the plan is a list")
        .iter()
        .map(|row| row["detail"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// A credential file the CLI will accept.
///
/// The file has to be private to its owner: a credential file anyone
/// else can read is refused, and rightly so.
fn key_file(dir: &Path, key: &str) -> PathBuf {
    let path = dir.join("key");
    std::fs::write(&path, key).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

/// Put a damaged copy of `source` where the database `target` keeps its
/// file.
///
/// The copy is scribbled over on purpose: the page size comes from the
/// file's own header, so the damage lands on a page whatever the
/// platform's default is. The first page is never touched -- it holds
/// the header and the schema, and a file whose header cannot be read is
/// a different failure from a file with a damaged page.
fn corrupt_copy(dir: &Path, source: &str, target: &str) {
    let copy = dir.join("copy.sqlite");
    std::fs::copy(dir.join(format!("{source}.sqlite")), &copy).unwrap();
    let mut bytes = std::fs::read(&copy).unwrap();
    let page_size = u16::from_be_bytes([bytes[16], bytes[17]]) as usize;
    assert!(page_size > 0);
    let pages = bytes.len() / page_size;
    assert!(pages >= 2, "the file has no page to scribble over");
    let offset = (pages - 1) * page_size;
    let end = (offset + page_size).min(bytes.len());
    for byte in bytes[offset..end].iter_mut() {
        *byte = 0x5a;
    }
    std::fs::write(&copy, &bytes).unwrap();

    std::fs::write(dir.join(format!("{target}.sqlite")), &bytes).unwrap();
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.join(format!("{target}.sqlite{suffix}")));
    }
}

#[tokio::test]
async fn explain_reports_the_plan_for_a_table_with_no_index() {
    let dir = tempfile::tempdir().unwrap();
    let (server, owner) = start(dir.path()).await;
    server.store.create_and_note("shop").unwrap();
    let (status, _) = query(
        &server.address,
        &owner,
        "shop",
        "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)",
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = query(
        &server.address,
        &owner,
        "shop",
        "INSERT INTO items (name) VALUES ('a'), ('b')",
    )
    .await;
    assert_eq!(status, 200);

    let (status, body) = post(
        &server.address,
        "/v1/databases/shop/explain",
        &owner,
        Some(json!({"sql": "SELECT * FROM items WHERE name = 'a'"})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let plan = body["plan"].as_array().expect("the plan is a list");
    assert!(!plan.is_empty(), "the plan is empty: {body}");
    // Nothing indexes `name`, so SQLite must say it scans the table.
    let details = details(&body);
    assert!(
        details.iter().any(|detail| detail.contains("SCAN items")),
        "the plan does not report the scan: {details:?}"
    );
    // The plan says what the step means, and says nothing about cost:
    // SQLite's plan has no costs to report.
    assert!(
        plan.iter()
            .any(|row| row["meaning"].as_str().unwrap_or_default().contains("scan")),
        "the plan does not say what the step means: {body}"
    );
    assert!(body.get("cost").is_none(), "a cost was invented: {body}");

    // The human view carries the same words.
    let table = cmd_db_ops::plan_table(&body);
    let drawn = table
        .rows()
        .iter()
        .flat_map(|row| row.iter())
        .map(|cell| cell.parts().0.to_string())
        .collect::<Vec<String>>()
        .join(" ");
    assert!(
        drawn.contains("SCAN items"),
        "the table hides the plan: {drawn}"
    );
}

#[tokio::test]
async fn explain_reports_sqlites_own_error_for_a_statement_that_does_not_compile() {
    let dir = tempfile::tempdir().unwrap();
    let (server, owner) = start(dir.path()).await;
    server.store.create_and_note("shop").unwrap();
    let (status, body) = post(
        &server.address,
        "/v1/databases/shop/explain",
        &owner,
        Some(json!({"sql": "SELECT * FROM no_such_table"})),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    // The message is SQLite's, not a generic refusal: "no such table" is
    // the diagnosis, and a fixed sentence would lose it.
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("no such table"),
        "SQLite's own error is lost: {body}"
    );
}

#[tokio::test]
async fn integrity_reports_a_sound_database_as_sound() {
    let dir = tempfile::tempdir().unwrap();
    let (server, owner) = start(dir.path()).await;
    server.store.create_and_note("shop").unwrap();
    let (status, _) = query(
        &server.address,
        &owner,
        "shop",
        "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)",
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = query(
        &server.address,
        &owner,
        "shop",
        "INSERT INTO items (name) VALUES ('a'), ('b')",
    )
    .await;
    assert_eq!(status, 200);

    let (status, body) = get(&server.address, "/v1/databases/shop/integrity", &owner).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ok"], json!(true), "a sound file is not sound: {body}");
    assert_eq!(body["integrity"], json!(["ok"]), "{body}");
    assert_eq!(body["foreign_keys"], json!([]), "{body}");
    assert_eq!(body["foreign_key_count"], json!(0), "{body}");
}

#[tokio::test]
async fn integrity_reports_a_corrupted_file_as_not_sound() {
    let dir = tempfile::tempdir().unwrap();
    let (server, owner) = start(dir.path()).await;
    server.store.create_and_note("sound").unwrap();
    server.store.create_and_note("broken").unwrap();
    let (status, _) = query(
        &server.address,
        &owner,
        "sound",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = query(
        &server.address,
        &owner,
        "sound",
        "INSERT INTO t (v) VALUES ('a'), ('b'), ('c')",
    )
    .await;
    assert_eq!(status, 200);

    // A copy of a sound database, scribbled over on purpose.
    corrupt_copy(dir.path(), "sound", "broken");

    let (status, body) = get(&server.address, "/v1/databases/broken/integrity", &owner).await;
    assert_eq!(status, 200, "{body}");
    // The verdict is what matters: how SQLite describes the damage
    // depends on its version, but a damaged file is not sound.
    assert_eq!(
        body["ok"],
        json!(false),
        "a corrupted file was reported as sound: {body}"
    );
    // The sound database next to it is still sound: the check reads the
    // file it was asked about.
    let (status, body) = get(&server.address, "/v1/databases/sound/integrity", &owner).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["ok"],
        json!(true),
        "the sound file is not sound: {body}"
    );
}

#[tokio::test]
async fn integrity_reports_the_row_that_breaks_a_foreign_key() {
    let dir = tempfile::tempdir().unwrap();
    let (server, owner) = start(dir.path()).await;
    server.store.create_and_note("fk").unwrap();
    let (status, _) = query(
        &server.address,
        &owner,
        "fk",
        "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = query(
        &server.address,
        &owner,
        "fk",
        "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id))",
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = query(
        &server.address,
        &owner,
        "fk",
        "INSERT INTO parent (id) VALUES (1)",
    )
    .await;
    assert_eq!(status, 200);

    // The gateway's own statements enforce foreign keys, so the row that
    // breaks the rule is written straight into the file.
    {
        let conn = rusqlite::Connection::open(dir.path().join("fk.sqlite")).unwrap();
        // The bundled SQLite enforces foreign keys by default, and the
        // row that breaks the rule is exactly the one it refuses.
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute("INSERT INTO child (pid) VALUES (42)", [])
            .unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
    }

    let (status, body) = get(&server.address, "/v1/databases/fk/integrity", &owner).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["ok"],
        json!(false),
        "the violation is not sound: {body}"
    );
    // The file itself is fine; it is the rows that are wrong.
    assert_eq!(body["integrity"], json!(["ok"]), "{body}");
    let violations = body["foreign_keys"]
        .as_array()
        .expect("a list of violations");
    assert_eq!(violations.len(), 1, "{body}");
    assert_eq!(violations[0]["table"], json!("child"), "{body}");
    assert_eq!(violations[0]["parent"], json!("parent"), "{body}");
    assert_eq!(violations[0]["rowid"], json!(1), "{body}");
    assert_eq!(body["foreign_key_count"], json!(1), "{body}");

    // The human view names the row that breaks the rule.
    let problems = cmd_db_ops::integrity_problems(&body);
    assert!(
        problems.iter().any(|problem| problem.contains("child")),
        "the problem is not reported: {problems:?}"
    );
}

#[tokio::test]
async fn vacuum_reclaims_the_space_deleted_rows_left_behind() {
    let dir = tempfile::tempdir().unwrap();
    let (server, owner) = start(dir.path()).await;
    server.store.create_and_note("fat").unwrap();
    let (status, _) = query(
        &server.address,
        &owner,
        "fat",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = query(
        &server.address,
        &owner,
        "fat",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<2000) \
         INSERT INTO t (v) SELECT 'padding padding padding padding padding' || x FROM c",
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = query(
        &server.address,
        &owner,
        "fat",
        "DELETE FROM t WHERE id > 10",
    )
    .await;
    assert_eq!(status, 200);

    let file = dir.path().join("fat.sqlite");
    let before = std::fs::metadata(&file).unwrap().len();
    let (status, body) = post(&server.address, "/v1/databases/fat/vacuum", &owner, None).await;
    assert_eq!(status, 200, "{body}");
    let after = std::fs::metadata(&file).unwrap().len();

    assert!(
        body["reclaimed_bytes"].as_u64().unwrap_or(0) > 0,
        "nothing was reclaimed: {body}"
    );
    let size_before = body["size_before_bytes"].as_u64().unwrap();
    let size_after = body["size_after_bytes"].as_u64().unwrap();
    assert_eq!(
        size_before - body["reclaimed_bytes"].as_u64().unwrap(),
        size_after,
        "the report does not add up: {body}"
    );
    assert!(size_after < size_before, "the file did not shrink: {body}");
    assert!(
        after < before,
        "the file on disk did not shrink: {before} -> {after}"
    );
    assert!(body["duration_ms"].as_u64().is_some(), "{body}");
}

#[tokio::test]
async fn vacuum_says_plainly_when_there_is_nothing_to_reclaim() {
    let dir = tempfile::tempdir().unwrap();
    let (server, owner) = start(dir.path()).await;
    server.store.create_and_note("small").unwrap();
    let (status, _) = query(
        &server.address,
        &owner,
        "small",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = query(
        &server.address,
        &owner,
        "small",
        "INSERT INTO t (v) VALUES ('a'), ('b')",
    )
    .await;
    assert_eq!(status, 200);

    let (status, body) = post(&server.address, "/v1/databases/small/vacuum", &owner, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["reclaimed_bytes"],
        json!(0),
        "a vacuum that reclaimed nothing must say so: {body}"
    );
    assert_eq!(
        body["size_before_bytes"].as_u64().unwrap(),
        body["size_after_bytes"].as_u64().unwrap(),
        "{body}"
    );
}

#[tokio::test]
async fn vacuum_and_integrity_refuse_a_read_only_credential() {
    let dir = tempfile::tempdir().unwrap();
    let (server, owner) = start(dir.path()).await;
    server.store.create_and_note("shop").unwrap();
    let (status, _) = query(
        &server.address,
        &owner,
        "shop",
        "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)",
    )
    .await;
    assert_eq!(status, 200);
    let reader = server
        .store
        .issue(dg::store::KeyRequest {
            name: "reader".to_string(),
            role: "database_read_only".to_string(),
            databases: vec!["shop".to_string()],
        })
        .expect("the key is issued");

    let (status, body) = get(&server.address, "/v1/databases/shop/integrity", &reader).await;
    assert_eq!(status, 403, "a read-only credential was let in: {body}");
    let (status, body) = post(&server.address, "/v1/databases/shop/vacuum", &reader, None).await;
    assert_eq!(status, 403, "a read-only credential was let in: {body}");

    // The plan is a question, and a read-only credential may ask it.
    let (status, body) = post(
        &server.address,
        "/v1/databases/shop/explain",
        &reader,
        Some(json!({"sql": "SELECT * FROM items"})),
    )
    .await;
    assert_eq!(status, 200, "a read-only credential was refused: {body}");
    assert!(!details(&body).is_empty(), "the plan is empty: {body}");
}

#[tokio::test]
async fn integrity_refuses_a_credential_that_is_not_the_owner() {
    let dir = tempfile::tempdir().unwrap();
    let (server, owner) = start(dir.path()).await;
    server.store.create_and_note("shop").unwrap();
    let reader = server
        .store
        .issue(dg::store::KeyRequest {
            name: "reader".to_string(),
            role: "database_read_only".to_string(),
            databases: vec!["shop".to_string()],
        })
        .expect("the key is issued");
    let (status, body) = get(&server.address, "/v1/databases/shop/integrity", &reader).await;
    assert_eq!(status, 403, "{body}");
    let (status, body) = get(&server.address, "/v1/databases/shop/integrity", &owner).await;
    assert_eq!(status, 200, "{body}");
}

/// The commands themselves, end to end.
///
/// The three commands are what an operator runs, so they are run here:
/// the plan, the soundness of the file, and the space the file was
/// wasting. The last one matters most -- a check that finds a damaged
/// file has to fail, because a deploy gate runs it and trusts the exit
/// code.
#[tokio::test]
async fn the_commands_answer_the_three_questions() {
    let dir = tempfile::tempdir().unwrap();
    let (server, owner) = start(dir.path()).await;
    server.store.create_and_note("shop").unwrap();
    server.store.create_and_note("broken").unwrap();
    let (status, _) = query(
        &server.address,
        &owner,
        "shop",
        "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)",
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = query(
        &server.address,
        &owner,
        "shop",
        "INSERT INTO items (name) VALUES ('a'), ('b')",
    )
    .await;
    assert_eq!(status, 200);
    corrupt_copy(dir.path(), "shop", "broken");

    let key_path = key_file(dir.path(), &owner);
    let server_url = format!("http://{}", server.address);
    let credential = Some(key_path);

    // The plan, the soundness, and the wasted space.
    cmd_db_ops::explain(
        &server_url,
        true,
        &credential,
        "shop".to_string(),
        "SELECT * FROM items WHERE name = 'a'".to_string(),
    )
    .await
    .expect("the plan is reported");
    cmd_db_ops::integrity(&server_url, true, &credential, "shop".to_string())
        .await
        .expect("a sound file is reported as sound");
    cmd_db_ops::vacuum(&server_url, true, &credential, "shop".to_string())
        .await
        .expect("the vacuum is reported");

    // A file that is not sound must not exit 0: this is the command a
    // deploy gate runs, and a gate that passes a damaged file is worse
    // than no gate.
    let failed = cmd_db_ops::integrity(&server_url, true, &credential, "broken".to_string()).await;
    assert!(
        failed.is_err(),
        "the command exited 0 for a file that is not sound"
    );
}

/// The human views, drawn.
///
/// A terminal gets these; a pipe gets the JSON. They are drawn here so
/// the wording is checked, not just the numbers.
#[test]
fn the_human_views_say_what_happened() {
    let sound = json!({
        "database": "shop",
        "ok": true,
        "complete": true,
        "integrity": ["ok"],
        "foreign_keys": [],
        "foreign_key_count": 0,
        "truncated": false,
        "duration_ms": 4,
        "failure": null,
    });
    let mut out = Vec::new();
    cmd_db_ops::write_integrity(&mut out, &sound, true).unwrap();
    let drawn = String::from_utf8(out).unwrap();
    assert!(drawn.contains("shop is sound"), "{drawn}");
    assert!(drawn.contains("no violations"), "{drawn}");

    let broken = json!({
        "database": "shop",
        "ok": false,
        "complete": true,
        "integrity": ["*** in database main ***\nTree 2 page 3: btreeInitPage() returns error code 11"],
        "foreign_keys": [{"table": "child", "rowid": 1, "parent": "parent", "fkid": 0}],
        "foreign_key_count": 1,
        "truncated": false,
        "duration_ms": 3,
        "failure": null,
    });
    let mut out = Vec::new();
    cmd_db_ops::write_integrity(&mut out, &broken, false).unwrap();
    let drawn = String::from_utf8(out).unwrap();
    assert!(drawn.contains("shop is not sound"), "{drawn}");
    // SQLite's own words, and the row that breaks the rule.
    assert!(drawn.contains("btreeInitPage"), "{drawn}");
    assert!(drawn.contains("child"), "{drawn}");

    let vacuum = json!({
        "database": "shop",
        "size_before_bytes": 40960,
        "size_after_bytes": 8192,
        "reclaimed_bytes": 32768,
        "checkpoint_complete": true,
        "duration_ms": 12,
    });
    let mut out = Vec::new();
    cmd_db_ops::write_vacuum(&mut out, &vacuum).unwrap();
    let drawn = String::from_utf8(out).unwrap();
    assert!(drawn.contains("vacuumed shop"), "{drawn}");
    assert!(drawn.contains("32.0 KiB"), "{drawn}");

    // A vacuum that reclaimed nothing says so, rather than dressing a
    // no-op up as a saving.
    let nothing = json!({
        "database": "shop",
        "size_before_bytes": 8192,
        "size_after_bytes": 8192,
        "reclaimed_bytes": 0,
        "checkpoint_complete": true,
        "duration_ms": 2,
    });
    let mut out = Vec::new();
    cmd_db_ops::write_vacuum(&mut out, &nothing).unwrap();
    let drawn = String::from_utf8(out).unwrap();
    assert!(drawn.contains("nothing to reclaim"), "{drawn}");
}
