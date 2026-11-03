//! The audit log: who ran what, and when.
//!
//! Run: `timeout 300 cargo test --locked --test audit`
//!
//! The log records ACTIONS and never the data that moved, so the tests
//! here are mostly about what must NOT be in it: the credential's
//! secret, and the values of a statement. A log that leaks either is
//! worse than no log, because an operator trusts it.
//!
//! The gateway is served in-process rather than as a spawned binary:
//! the audit route is a router of its own, and merging it into the
//! API's router is the integration's job, not this test's.
//!
//! The module under test is the library's own source file, the way the
//! CLI's source file is the module under test in `cmd_config.rs`, so
//! the tests exercise the code the gateway runs. The two `mod` lines
//! below give that source file the library's own modules under the
//! names it expects.

#[path = "../src/cmd_audit.rs"]
mod cmd_audit;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use dg::api::Deployment;
use dg::store::{AuditLog, KeyRequest, Principal, Store};
use serde_json::Value;

/// A distinctive literal: if this string turns up in the log, the
/// values leaked.
const LITERAL: &str = "a-very-distinctive-literal";

/// A gateway on a loopback port, with an owner key, a scoped app key
/// and one database.
struct Gateway {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    owner: String,
    app: String,
    address: SocketAddr,
}

impl Gateway {
    /// Start a gateway and hand back its keys.
    fn start() -> Gateway {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Arc::new(Store::open(dir.path()).expect("the store opens"));
        let owner = store.bootstrap().expect("the owner key is issued");
        store
            .create_and_note("shop")
            .expect("the database is created");
        let app = store
            .issue(KeyRequest {
                name: "checkout".to_string(),
                role: "database_read_only".to_string(),
                databases: vec!["shop".to_string()],
            })
            .expect("the app key is issued");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("the listener binds");
        let address = listener.local_addr().expect("the address is known");
        listener
            .set_nonblocking(true)
            .expect("the listener is non-blocking");
        let deployment = Deployment {
            listen: address,
            host: None,
        };
        // The API router already carries the audit route: the audit log
        // is part of the service, not a second service bolted beside it.
        let router = dg::api::router(store.clone(), deployment);
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("the listener is the runtime's");
                let _ = axum::serve(listener, router).await;
            });
        });
        Gateway {
            _dir: dir,
            store,
            owner,
            app,
            address,
        }
    }

    /// The origin of the gateway, the way a client is given it.
    fn origin(&self) -> String {
        format!("http://{}", self.address)
    }

    /// A credential file holding the owner key, private like the real
    /// one.
    fn key_file(&self) -> PathBuf {
        let path = self._dir.path().join("owner.key");
        std::fs::write(&path, &self.owner).expect("the key file is written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("the key file is private");
        }
        path
    }

    /// The principal behind a credential.
    fn principal(&self, key: &str) -> Principal {
        self.store
            .authenticate(key)
            .expect("the credential authenticates")
    }
}

/// The audit log, as the endpoint answers it.
fn get(gateway: &Gateway, key: Option<&str>) -> (u16, String) {
    let mut request =
        reqwest::blocking::Client::new().get(format!("{}/v1/audit", gateway.origin()));
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    let response = request.send().expect("the request goes through");
    let status = response.status().as_u16();
    let body = response.text().expect("the body is read");
    (status, body)
}

/// The entries of an audit document.
fn entries(body: &str) -> Vec<Value> {
    let document: Value = serde_json::from_str(body).expect("the body is JSON");
    document["entries"]
        .as_array()
        .expect("the document holds the entries")
        .clone()
}

#[test]
fn an_action_is_recorded_with_the_credential_label_not_its_secret() {
    let gateway = Gateway::start();
    let app = gateway.principal(&gateway.app);
    assert_eq!(app.name, "checkout");

    dg::audit::record(&gateway.store, &app, "query", Some("shop"), None);

    let (status, body) = get(&gateway, Some(&gateway.owner));
    assert_eq!(status, 200);
    let entries = entries(&body);
    assert_eq!(entries.len(), 1, "the action is in the log: {body}");
    assert_eq!(entries[0]["principal"], "checkout", "the LABEL: {body}");
    assert_eq!(entries[0]["role"], "database_read_only", "{body}");
    assert_eq!(entries[0]["action"], "query", "{body}");
    assert_eq!(entries[0]["database"], "shop", "{body}");
    assert!(
        !body.contains(&gateway.app),
        "the credential's secret is not in the log: {body}"
    );
    assert!(
        !body.contains(&gateway.owner),
        "the owner's secret is not in the log: {body}"
    );
}

#[test]
fn a_statement_is_recorded_by_its_shape_and_never_by_its_values() {
    let gateway = Gateway::start();
    let app = gateway.principal(&gateway.app);

    dg::audit::record_statement(
        &gateway.store,
        &app,
        "query",
        Some("shop"),
        &format!("SELECT * FROM shop WHERE token = '{LITERAL}'"),
        None,
    );

    let (status, body) = get(&gateway, Some(&gateway.owner));
    assert_eq!(status, 200);
    assert!(
        !body.contains(LITERAL),
        "the statement's values are not in the log: {body}"
    );
    assert!(
        body.contains("SELECT * FROM shop WHERE token = ?"),
        "the statement's shape is in the log: {body}"
    );
}

/// A recorded statement carries how long it took: the audit trail is
/// the owner's answer to "which credential ran the slow query".
#[test]
fn a_recorded_statement_carries_its_execution_time() {
    let gateway = Gateway::start();
    let app = gateway.principal(&gateway.app);

    dg::audit::record_statement(
        &gateway.store,
        &app,
        "query",
        Some("shop"),
        "SELECT * FROM shop WHERE token = ?",
        Some(12_345),
    );

    let (status, body) = get(&gateway, Some(&gateway.owner));
    assert_eq!(status, 200);
    assert!(
        body.contains("\"elapsed_us\":12345"),
        "the statement's execution time is in the log: {body}"
    );
}

/// Anything that is not a statement carries no execution time: a
/// `create` did not run, so there is nothing to time.
#[test]
fn actions_that_are_not_statements_carry_no_execution_time() {
    let gateway = Gateway::start();
    let owner = gateway.principal(&gateway.owner);
    dg::audit::record(
        &gateway.store,
        &owner,
        "create",
        Some("shop"),
        None,
    );

    let (status, body) = get(&gateway, Some(&gateway.owner));
    assert_eq!(status, 200);
    assert!(
        !body.contains("elapsed_us"),
        "a non-statement entry carries no execution time: {body}"
    );
}

/// The end-to-end version of the same promise: a statement that really
/// ran through the gateway leaves no trace of its values.
#[test]
fn a_statement_that_ran_through_the_gateway_leaves_no_values_behind() {
    let gateway = Gateway::start();
    // A table to read: the owner makes it, the app key reads it.
    let created = reqwest::blocking::Client::new()
        .post(format!("{}/v1/databases/shop/query", gateway.origin()))
        .bearer_auth(&gateway.owner)
        .json(&serde_json::json!({ "sql": "CREATE TABLE tokens (token TEXT)" }))
        .send()
        .expect("the create goes through");
    assert_eq!(created.status().as_u16(), 200);

    let statement = format!("SELECT * FROM tokens WHERE token = '{LITERAL}'");
    let response = reqwest::blocking::Client::new()
        .post(format!("{}/v1/databases/shop/query", gateway.origin()))
        .bearer_auth(&gateway.app)
        .json(&serde_json::json!({ "sql": statement }))
        .send()
        .expect("the query goes through");
    assert_eq!(response.status().as_u16(), 200);

    let (_, body) = get(&gateway, Some(&gateway.owner));
    assert!(
        !body.contains(LITERAL),
        "the statement's values are not in the log: {body}"
    );
}

/// A statement that really ran leaves its execution time behind: the
/// query path passes the measured microseconds to the audit log.
#[test]
fn a_statement_that_ran_carries_its_measured_execution_time() {
    let gateway = Gateway::start();
    let created = reqwest::blocking::Client::new()
        .post(format!("{}/v1/databases/shop/query", gateway.origin()))
        .bearer_auth(&gateway.owner)
        .json(&serde_json::json!({ "sql": "CREATE TABLE t (a INTEGER)" }))
        .send()
        .expect("the create goes through");
    assert_eq!(created.status().as_u16(), 200);

    let (_, body) = get(&gateway, Some(&gateway.owner));
    assert!(
        body.contains("\"elapsed_us\":"),
        "the statement that ran carries a measured time: {body}"
    );
}

#[test]
fn a_scoped_credential_is_refused_the_audit_log() {
    let gateway = Gateway::start();
    dg::audit::record(
        &gateway.store,
        &gateway.principal(&gateway.app),
        "query",
        Some("shop"),
        None,
    );

    let (status, _) = get(&gateway, Some(&gateway.app));
    assert_eq!(
        status, 403,
        "a scoped app key must not read the whole box's log"
    );
    let (status, _) = get(&gateway, None);
    assert_eq!(status, 401, "an unauthenticated request is refused");
}

#[test]
fn the_log_is_bounded_and_drops_the_oldest_entries() {
    let gateway = Gateway::start();
    let owner = gateway.principal(&gateway.owner);
    let cap = AuditLog::MAX_ENTRIES;
    for index in 0..(cap + 5) {
        dg::audit::record(
            &gateway.store,
            &owner,
            "query",
            Some("shop"),
            Some(format!("entry-{index}")),
        );
    }

    let recorded = gateway.store.audit.entries();
    assert_eq!(recorded.len(), cap, "the log stops at its cap");
    assert_eq!(
        recorded.first().and_then(|entry| entry.detail.clone()),
        Some("entry-5".to_string()),
        "the oldest entries are the ones dropped"
    );
    assert_eq!(
        recorded.last().and_then(|entry| entry.detail.clone()),
        Some(format!("entry-{}", cap + 4)),
        "the newest entry is kept"
    );

    let (status, body) = get(&gateway, Some(&gateway.owner));
    assert_eq!(status, 200);
    assert_eq!(entries(&body).len(), cap, "the endpoint serves the cap too");
}

#[test]
fn the_endpoint_never_returns_a_credential_secret() {
    let gateway = Gateway::start();
    let owner = gateway.principal(&gateway.owner);
    dg::audit::record(&gateway.store, &owner, "rotate_owner", None, None);

    let (_, body) = get(&gateway, Some(&gateway.owner));
    assert!(
        !body.contains(&gateway.owner),
        "the owner key is not served"
    );
    assert!(!body.contains(&gateway.app), "the app key is not served");
}

/// The endpoint's filters are the ones the operator asks for.
#[test]
fn the_endpoint_filters_by_credential_and_by_time() {
    let gateway = Gateway::start();
    let owner = gateway.principal(&gateway.owner);
    let app = gateway.principal(&gateway.app);
    dg::audit::record(&gateway.store, &app, "query", Some("shop"), None);
    dg::audit::record(&gateway.store, &owner, "create", Some("shop"), None);

    let (status, body) = get(&gateway, Some(&gateway.owner));
    assert_eq!(status, 200);
    assert_eq!(entries(&body).len(), 2, "both actions are in the log");

    // Only one credential's actions.
    let response = reqwest::blocking::Client::new()
        .get(format!("{}/v1/audit?principal=checkout", gateway.origin()))
        .bearer_auth(&gateway.owner)
        .send()
        .expect("the request goes through");
    assert_eq!(response.status().as_u16(), 200);
    let body = response.text().expect("the body is read");
    let filtered = entries(&body);
    assert_eq!(
        filtered.len(),
        1,
        "the other credential's action is not: {body}"
    );
    assert_eq!(filtered[0]["principal"], "checkout");

    // Nothing happened after the future.
    let future = u64::MAX / 2;
    let response = reqwest::blocking::Client::new()
        .get(format!("{}/v1/audit?since={future}", gateway.origin()))
        .bearer_auth(&gateway.owner)
        .send()
        .expect("the request goes through");
    assert_eq!(response.status().as_u16(), 200);
    let body = response.text().expect("the body is read");
    assert!(
        entries(&body).is_empty(),
        "nothing happened after the future: {body}"
    );
}

/// The command asks the endpoint for exactly what the operator asked
/// for, and prints what the endpoint said.
#[test]
fn the_command_asks_the_endpoint_for_what_the_operator_asked_for() {
    let gateway = Gateway::start();
    let owner = gateway.principal(&gateway.owner);
    dg::audit::record(&gateway.store, &owner, "query", Some("shop"), None);
    let key_file = gateway.key_file();
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");

    let request = runtime
        .block_on(cmd_audit::list(
            &gateway.origin(),
            true,
            &Some(key_file.clone()),
            Some("owner"),
            Some(1_700_000_000),
            Some(5),
        ))
        .expect("the command builds its request")
        .expect("the request is the shared tail's");
    let (method, path, body, idempotency, semantics) = request;
    assert_eq!(method, reqwest::Method::GET);
    assert_eq!(
        path, "/v1/audit?principal=owner&since=1700000000&limit=5",
        "the filters are the endpoint's own"
    );
    assert!(body.is_none(), "a read has no body");
    assert!(idempotency.is_none(), "a read has no idempotency key");
    assert!(matches!(semantics, dg::client::Semantics::Read));

    let key = dg::client::read_key(&key_file).expect("the key is read");
    let document = runtime
        .block_on(dg::client::request_with_semantics(
            &gateway.origin(),
            &key,
            method,
            &path,
            body,
            idempotency.as_deref(),
            semantics,
        ))
        .expect("the request goes through");
    assert_eq!(
        document["entries"].as_array().expect("entries").len(),
        1,
        "the endpoint answered: {document}"
    );
}

/// The human view says WHEN in a form a person reads, and keeps the
/// epoch seconds out of it.
#[test]
fn the_human_table_says_when_in_a_form_a_person_reads() {
    let gateway = Gateway::start();
    let app = gateway.principal(&gateway.app);
    dg::audit::record(&gateway.store, &app, "query", Some("shop"), None);
    let (_, body) = get(&gateway, Some(&gateway.owner));
    let document: Value = serde_json::from_str(&body).expect("the body is JSON");
    let epoch = document["entries"][0]["at"].as_u64().expect("the stamp");

    let table = cmd_audit::table(&document);
    let mut out = Vec::new();
    table
        .write(&mut out, &dg::render::Theme::fixed(false, false))
        .expect("the table is drawn");
    let text = String::from_utf8(out).expect("the table is text");
    assert!(text.contains("checkout"), "the credential is named: {text}");
    assert!(text.contains("UTC"), "the time is readable: {text}");
    assert!(
        !text.contains(&epoch.to_string()),
        "the raw epoch is not printed: {text}"
    );
}

/// The trail is on disk, so a restart does not erase it.
///
/// The question this log answers is asked after something unexpected,
/// and a restart is often the unexpected thing: a gateway that comes
/// back up with an empty trail cannot say which credential was doing
/// what when it went down.
#[test]
fn the_trail_survives_a_restart() {
    let dir = tempfile::tempdir().expect("a temp dir");
    {
        let store = Store::open(dir.path()).expect("the store opens");
        let key = store.bootstrap().expect("the owner key is issued");
        let owner = store.authenticate(&key).expect("the owner authenticates");
        dg::audit::record(
            &store,
            &owner,
            "query",
            Some("shop"),
            Some("before-the-restart".to_string()),
        );
        // The process goes away with the entry still in its memory.
    }

    // The gateway comes back up on the same data directory.
    let store = Store::open(dir.path()).expect("the store opens again");
    let recorded = store.audit.entries();
    assert_eq!(
        recorded.len(),
        1,
        "the entry outlived the restart: {recorded:?}"
    );
    assert_eq!(
        recorded[0].detail.as_deref(),
        Some("before-the-restart"),
        "and it is the same entry: {recorded:?}"
    );

    // The trail is a file, and it is private.
    let trail = dir.path().join("audit.jsonl");
    assert!(trail.exists(), "the trail file is there");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&trail)
            .expect("the trail's metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the trail is private to its owner");
    }
}

/// The file on disk keeps the promise the endpoint makes: shapes, never
/// values, and never a credential's secret.
#[test]
fn the_trail_on_disk_holds_no_values_and_no_secrets() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::open(dir.path()).expect("the store opens");
    let key = store.bootstrap().expect("the owner key is issued");
    let owner = store.authenticate(&key).expect("the owner authenticates");
    dg::audit::record_statement(
        &store,
        &owner,
        "query",
        Some("shop"),
        &format!("SELECT * FROM shop WHERE token = '{LITERAL}'"),
        None,
    );

    let text = std::fs::read_to_string(dir.path().join("audit.jsonl")).expect("the trail is read");
    assert!(
        !text.contains(LITERAL),
        "the statement's values are not on disk: {text}"
    );
    assert!(
        text.contains("SELECT * FROM shop WHERE token = ?"),
        "the statement's shape is on disk: {text}"
    );
    assert!(
        !text.contains(&key),
        "the credential's secret is not on disk: {text}"
    );
}
