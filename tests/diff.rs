//! What differs between two databases.
//!
//! The comparison is proved against a real gateway: the real store, the
//! real API router with the diff route merged into it, served over a
//! loopback socket. The engine is compiled into this test rather than
//! reached through the library, so these tests do not wait on the
//! library's wiring; `crate::error` and `crate::store` are re-exported
//! below so the included module resolves the same paths it uses inside
//! the crate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

// The re-exports the included engine resolves `crate::error` and
// `crate::store` through.
use dg::{error, store};

#[path = "../src/diff.rs"]
#[allow(dead_code)]
mod diff;

#[path = "../src/cmd_diff.rs"]
mod cmd_diff;

/// A gateway serving the real API with the diff route merged into it.
struct Gateway {
    address: String,
    owner: String,
    store: Arc<store::Store>,
}

impl Gateway {
    /// The file a database lives in.
    fn file(&self, name: &str) -> PathBuf {
        self.store.path(name).expect("database path")
    }

    /// A database with the given schema and rows.
    fn database(&self, name: &str, statements: &[&str]) {
        self.store.create(name).expect("database created");
        let path = self.file(name);
        let conn = rusqlite::Connection::open(&path).expect("database opens");
        for statement in statements {
            conn.execute_batch(statement).expect("statement runs");
        }
    }

    /// A database with one table and the given number of rows.
    fn database_with_rows(&self, name: &str, rows: u64) {
        self.database(name, &["CREATE TABLE t (v INTEGER)"]);
        let path = self.file(name);
        let mut conn = rusqlite::Connection::open(&path).expect("database opens");
        let transaction = conn.transaction().expect("transaction");
        for value in 0..rows {
            transaction
                .execute("INSERT INTO t (v) VALUES (?1)", [value as i64])
                .expect("row inserted");
        }
        transaction.commit().expect("committed");
    }

    /// The diff of two databases, as the endpoint answers it.
    fn diff(&self, key: &str, left: &str, right: &str) -> (u16, Value) {
        post(
            &self.address,
            key,
            "/v1/diff",
            json!({"left": left, "right": right}),
        )
    }
}

/// A gateway on a loopback socket.
fn gateway(data_dir: &Path) -> Gateway {
    let store = Arc::new(store::Store::open(data_dir).expect("store opens"));
    let owner = store.bootstrap().expect("owner key");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    // The listener is taken inside the runtime: registering a socket with
    // the reactor needs a reactor to register it with.
    let listener = runtime.block_on(async {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        std_listener.set_nonblocking(true).expect("nonblocking");
        tokio::net::TcpListener::from_std(std_listener).expect("tokio listener")
    });
    let address = listener.local_addr().expect("address").to_string();
    let deployment = dg::api::Deployment {
        listen: address.parse().expect("socket address"),
        host: None,
    };
    // The API router already carries the diff route: the comparison is
    // part of the service, not a second service beside it.
    let app = dg::api::router(store.clone(), deployment);
    std::thread::spawn(move || {
        runtime.block_on(async move {
            axum::serve(listener, app).await.expect("serves");
        });
    });
    Gateway {
        address: format!("http://{address}"),
        owner,
        store,
    }
}

/// A request, and the answer as a status and a body.
fn post(address: &str, key: &str, path: &str, body: Value) -> (u16, Value) {
    let client = reqwest::blocking::Client::new();
    let response = client
        .post(format!("{address}{path}"))
        .bearer_auth(key)
        .json(&body)
        .send()
        .expect("request");
    let status = response.status().as_u16();
    let body = response.json::<Value>().unwrap_or(Value::Null);
    (status, body)
}

/// The credential file the CLI reads.
fn key_file(path: &Path, key: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let file = path.join("key");
    std::fs::write(&file, key).expect("key written");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("permissions");
    file
}

#[test]
fn the_same_schema_and_the_same_rows_are_identical() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = gateway(dir.path());
    let schema = [
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO people (name) VALUES ('ada'),('grace')",
    ];
    gateway.database("alpha", &schema);
    gateway.database("beta", &schema);

    let (status, report) = gateway.diff(&gateway.owner, "alpha", "beta");
    assert_eq!(status, 200);
    assert_eq!(report["identical"], json!(true));
    assert_eq!(report["schema"]["identical"], json!(true));
    assert_eq!(report["tables"][0]["name"], json!("people"));
    assert_eq!(report["tables"][0]["status"], json!("equal"));
    assert_eq!(report["tables"][0]["left_rows"], json!(2));
    assert_eq!(report["tables"][0]["right_rows"], json!(2));
    assert_eq!(report["truncated"], json!(false));
}

#[test]
fn the_order_of_the_rows_does_not_matter() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = gateway(dir.path());
    gateway.database(
        "alpha",
        &[
            "CREATE TABLE people (name TEXT)",
            "INSERT INTO people (name) VALUES ('ada'),('grace')",
        ],
    );
    gateway.database(
        "beta",
        &[
            "CREATE TABLE people (name TEXT)",
            "INSERT INTO people (name) VALUES ('grace'),('ada')",
        ],
    );

    let (status, report) = gateway.diff(&gateway.owner, "alpha", "beta");
    assert_eq!(status, 200);
    assert_eq!(report["identical"], json!(true));
    assert_eq!(report["tables"][0]["status"], json!("equal"));
}

#[test]
fn different_rows_are_reported_and_the_table_is_named() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = gateway(dir.path());
    gateway.database(
        "alpha",
        &[
            "CREATE TABLE people (name TEXT)",
            "INSERT INTO people (name) VALUES ('ada')",
        ],
    );
    gateway.database(
        "beta",
        &[
            "CREATE TABLE people (name TEXT)",
            "INSERT INTO people (name) VALUES ('grace')",
        ],
    );

    let (status, report) = gateway.diff(&gateway.owner, "alpha", "beta");
    assert_eq!(status, 200);
    assert_eq!(report["identical"], json!(false));
    let table = &report["tables"][0];
    assert_eq!(table["name"], json!("people"));
    assert_eq!(table["status"], json!("different"));
    assert_eq!(table["differing_rows"], json!(1));
    assert_eq!(report["compared_rows"], json!(1));
}

#[test]
fn a_different_row_count_is_a_difference() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = gateway(dir.path());
    gateway.database(
        "alpha",
        &[
            "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)",
            "INSERT INTO people (name) VALUES ('ada'),('grace')",
        ],
    );
    gateway.database(
        "beta",
        &[
            "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)",
            "INSERT INTO people (name) VALUES ('ada')",
        ],
    );

    let (status, report) = gateway.diff(&gateway.owner, "alpha", "beta");
    assert_eq!(status, 200);
    assert_eq!(report["identical"], json!(false));
    assert_eq!(report["tables"][0]["status"], json!("different"));
    assert_eq!(report["tables"][0]["left_rows"], json!(2));
    assert_eq!(report["tables"][0]["right_rows"], json!(1));
}

#[test]
fn a_different_schema_says_what_is_missing_on_each_side() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = gateway(dir.path());
    gateway.database("alpha", &["CREATE TABLE only_left (id INTEGER)"]);
    gateway.database("beta", &["CREATE TABLE only_right (id INTEGER)"]);

    let (status, report) = gateway.diff(&gateway.owner, "alpha", "beta");
    assert_eq!(status, 200);
    assert_eq!(report["identical"], json!(false));
    assert_eq!(report["schema"]["identical"], json!(false));
    assert_eq!(report["schema"]["only_left"][0]["name"], json!("only_left"));
    assert_eq!(report["schema"]["only_left"][0]["kind"], json!("table"));
    assert_eq!(
        report["schema"]["only_right"][0]["name"],
        json!("only_right")
    );
}

#[test]
fn a_changed_column_list_is_not_compared() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = gateway(dir.path());
    gateway.database("alpha", &["CREATE TABLE people (id INTEGER)"]);
    gateway.database("beta", &["CREATE TABLE people (id INTEGER, name TEXT)"]);

    let (status, report) = gateway.diff(&gateway.owner, "alpha", "beta");
    assert_eq!(status, 200);
    assert_eq!(report["identical"], json!(false));
    // The schema difference is reported as a difference...
    assert_eq!(report["schema"]["changed"][0]["name"], json!("people"));
    let detail = report["schema"]["changed"][0]["detail"]
        .as_str()
        .expect("detail");
    assert!(detail.contains("columns differ"), "{detail}");
    // ...and the rows are NOT compared, which is not the same as equal
    // and not the same as different.
    let table = &report["tables"][0];
    assert_eq!(table["name"], json!("people"));
    assert_eq!(table["status"], json!("not_compared"));
    assert_ne!(table["status"], json!("equal"));
    assert_ne!(table["status"], json!("different"));
    let reason = table["reason"].as_str().expect("reason");
    assert!(reason.contains("column"), "{reason}");
    assert_eq!(table["compared_rows"], json!(0));
}

#[test]
fn a_credential_scoped_to_one_side_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = gateway(dir.path());
    gateway.database("alpha", &["CREATE TABLE people (id INTEGER)"]);
    gateway.database("beta", &["CREATE TABLE people (id INTEGER)"]);
    let scoped = gateway
        .store
        .issue(store::KeyRequest {
            name: "leftonly".to_string(),
            role: "database_read_only".to_string(),
            databases: vec!["alpha".to_string()],
        })
        .expect("scoped key");

    let (status, report) = gateway.diff(&scoped, "alpha", "beta");
    assert_eq!(status, 403);
    assert_eq!(report["error"]["code"], json!("database_forbidden"));
}

#[test]
fn a_table_over_the_cap_is_truncated_and_not_identical() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = gateway(dir.path());
    let rows = diff::MAX_ROWS_PER_TABLE as u64 + 1;
    gateway.database_with_rows("alpha", rows);
    gateway.database_with_rows("beta", rows);

    let (status, report) = gateway.diff(&gateway.owner, "alpha", "beta");
    assert_eq!(status, 200);
    assert_eq!(report["truncated"], json!(true));
    assert_eq!(report["identical"], json!(false));
    let table = &report["tables"][0];
    assert_eq!(table["name"], json!("t"));
    assert_eq!(table["truncated"], json!(true));
    assert_eq!(table["status"], json!("not_compared"));
    assert_eq!(
        table["compared_rows"],
        json!(diff::MAX_ROWS_PER_TABLE as u64)
    );
    assert_eq!(table["left_rows"], json!(rows));
}

#[test]
fn the_endpoint_does_not_modify_the_databases_it_compares() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = gateway(dir.path());
    gateway.database(
        "alpha",
        &[
            "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)",
            "INSERT INTO people (name) VALUES ('ada')",
        ],
    );
    gateway.database(
        "beta",
        &[
            "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)",
            "INSERT INTO people (name) VALUES ('grace')",
        ],
    );
    let before: Vec<(u64, u64)> = ["alpha", "beta"]
        .iter()
        .map(|name| stamp(&gateway.file(name)))
        .collect();

    let (status, _) = gateway.diff(&gateway.owner, "alpha", "beta");
    assert_eq!(status, 200);

    let after: Vec<(u64, u64)> = ["alpha", "beta"]
        .iter()
        .map(|name| stamp(&gateway.file(name)))
        .collect();
    assert_eq!(before, after);
}

/// The size and the modification time of a file.
fn stamp(path: &Path) -> (u64, u64) {
    let metadata = std::fs::metadata(path).expect("metadata");
    let modified = metadata
        .modified()
        .expect("modified")
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_nanos() as u64;
    (metadata.len(), modified)
}

#[test]
fn the_cli_exits_non_zero_when_the_databases_differ() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = gateway(dir.path());
    gateway.database(
        "alpha",
        &[
            "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)",
            "INSERT INTO people (name) VALUES ('ada')",
        ],
    );
    gateway.database(
        "beta",
        &[
            "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)",
            "INSERT INTO people (name) VALUES ('grace')",
        ],
    );
    let key = key_file(dir.path(), &gateway.owner);

    // The runtime lives here and not inside the test: the gateway's own
    // runtime is already running, and one runtime inside another is a
    // panic rather than a test.
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let outcome = runtime.block_on(cmd_diff::diff(
        &gateway.address,
        true,
        &Some(key.clone()),
        "alpha",
        "beta",
        false,
    ));
    assert!(outcome.is_err(), "differing databases must not exit zero");
    let outcome = runtime.block_on(cmd_diff::diff(
        &gateway.address,
        true,
        &Some(key),
        "alpha",
        "alpha",
        false,
    ));
    assert!(outcome.is_ok(), "a database must exit zero against itself");
}

#[test]
fn the_human_view_names_the_table_that_differs() {
    let report: cmd_diff::Report = serde_json::from_value(json!({
        "left": "alpha",
        "right": "beta",
        "identical": false,
        "compared_rows": 2,
        "truncated": false,
        "schema": {
            "identical": true,
            "only_left": [],
            "only_right": [],
            "changed": []
        },
        "tables": [
            {
                "name": "people",
                "status": "different",
                "left_rows": 1,
                "right_rows": 1,
                "compared_rows": 1,
                "truncated": false,
                "differing_rows": 1,
                "reason": null
            }
        ]
    }))
    .expect("report");
    let mut out: Vec<u8> = Vec::new();
    dg::render::write_sections(
        &mut out,
        &dg::render::Theme::for_stream(false),
        &cmd_diff::human_view(&report, false),
    )
    .expect("drawn");
    let text = String::from_utf8(out).expect("utf-8");
    assert!(text.contains("differ"), "{text}");
    assert!(text.contains("people"), "{text}");
}
