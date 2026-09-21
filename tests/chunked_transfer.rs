//! Chunked large-database transfer (dummy data).
//!
//! Run: `timeout 300 cargo test --locked --test chunked_transfer`
//!
//! Databases past the 8 MiB single-shot ceiling move through retained
//! snapshots (download) and uploads (commit): begin → bounded 1 MiB chunks →
//! verify/commit. Proves an ~12 MiB round trip the single-shot endpoints
//! refuse, exact reassembly, resume offsets, stable error codes, creator
//! binding, TTL sweep, orphan cleanup, and transparent CLI use.

#[path = "common/mod.rs"]
mod common;

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use common::Fleet;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CHUNK: usize = 1024 * 1024;
const SINGLE_SHOT_CEILING: u64 = 8 * 1024 * 1024;

struct Server {
    child: Child,
    address: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn(data_dir: &std::path::Path) -> (Server, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", data_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let owner: String = serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--data-dir",
            data_dir.to_str().unwrap(),
        ])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut address = None;
    for line in BufReader::new(stderr).lines() {
        let line = line.unwrap();
        if let Ok(log) = serde_json::from_str::<Value>(&line)
            && let Some(addr) = log["fields"]["address"].as_str()
        {
            address = Some(addr.to_string());
            break;
        }
    }
    (
        Server {
            child,
            address: address.expect("server logged its listen address"),
        },
        owner,
    )
}

fn req(
    client: &reqwest::blocking::Client,
    address: &str,
    method: &str,
    path: &str,
    cred: &str,
    body: Option<&Value>,
) -> (u16, Value) {
    let mut request = client
        .request(
            method.parse::<reqwest::Method>().unwrap(),
            format!("http://{address}{path}"),
        )
        .bearer_auth(cred)
        .timeout(Duration::from_secs(60));
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request.send().unwrap();
    (response.status().as_u16(), response.json().unwrap())
}

/// Seed `rows` text rows of ~`width` bytes each through single statements.
/// Each request stays under the 64 KiB body cap however large the database.
fn seed_big(
    client: &reqwest::blocking::Client,
    address: &str,
    owner: &str,
    db: &str,
    rows: usize,
    width: usize,
) {
    let (status, _) = req(
        client,
        address,
        "POST",
        "/v1/databases",
        owner,
        Some(&json!({"name": db})),
    );
    assert_eq!(status, 200, "create {db}");
    let (status, _) = req(
        client,
        address,
        "POST",
        &format!("/v1/databases/{db}/query"),
        owner,
        Some(&json!({"sql": "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", "args": []})),
    );
    assert_eq!(status, 200, "schema {db}");
    for i in 0..rows {
        let value = format!("{i:08}") + &"x".repeat(width - 8);
        let (status, _) = req(
            client,
            address,
            "POST",
            &format!("/v1/databases/{db}/query"),
            owner,
            Some(&json!({"sql": "INSERT INTO t VALUES (?, ?)", "args": [i, value]})),
        );
        assert_eq!(status, 200, "seed {db} row {i}");
    }
}

fn count(client: &reqwest::blocking::Client, address: &str, owner: &str, db: &str) -> Value {
    let (status, rows) = req(
        client,
        address,
        "POST",
        &format!("/v1/databases/{db}/query"),
        owner,
        Some(&json!({"sql": "SELECT count(*) FROM t", "args": []})),
    );
    assert_eq!(status, 200, "count {db}");
    rows["rows"][0][0].clone()
}

fn row(client: &reqwest::blocking::Client, address: &str, owner: &str, db: &str, id: usize) -> Value {
    let (status, rows) = req(
        client,
        address,
        "POST",
        &format!("/v1/databases/{db}/query"),
        owner,
        Some(&json!({"sql": "SELECT v FROM t WHERE id = ?", "args": [id]})),
    );
    assert_eq!(status, 200, "row {id} {db}");
    rows["rows"][0][0].clone()
}

#[test]
fn chunked_round_trip_above_single_shot_cap() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let (server, owner) = spawn(&data_dir);
    let a = &server.address;
    let client = reqwest::blocking::Client::new();
    seed_big(&client, a, &owner, "big", 240, 50_000);

    // The single-shot envelope refuses this database; chunked is the path.
    let (status, body) = req(
        &client,
        a,
        "POST",
        "/v1/databases/big/export",
        &owner,
        Some(&json!({})),
    );
    assert_eq!(status, 413);
    assert_eq!(body["error"]["code"], "result_limit_exceeded");

    let (status, begin) = req(
        &client,
        a,
        "POST",
        "/v1/databases/big/export/begin",
        &owner,
        Some(&json!({})),
    );
    assert_eq!(status, 200);
    let snap = begin["snapshot"].as_str().unwrap().to_string();
    let size = begin["size"].as_u64().unwrap();
    let sha = begin["sha256"].as_str().unwrap().to_string();
    assert!(
        size > SINGLE_SHOT_CEILING,
        "seed must exceed the single-shot ceiling, got {size}"
    );

    let mut bytes = Vec::with_capacity(size as usize);
    let mut offset = 0u64;
    while offset < size {
        let (status, chunk) = req(
            &client,
            a,
            "GET",
            &format!("/v1/snapshots/{snap}?offset={offset}&length={CHUNK}"),
            &owner,
            None,
        );
        assert_eq!(status, 200, "chunk at {offset}");
        assert_eq!(chunk["offset"], offset);
        let part = STANDARD.decode(chunk["data_base64"].as_str().unwrap()).unwrap();
        assert!(!part.is_empty() && part.len() <= CHUNK);
        offset += part.len() as u64;
        bytes.extend_from_slice(&part);
    }
    assert_eq!(offset, size);
    assert_eq!(hex::encode(Sha256::digest(&bytes)), sha);

    // Upload the same bytes under a new name, checking resume status midway.
    let (status, begin) = req(
        &client,
        a,
        "POST",
        "/v1/databases/big/import/begin",
        &owner,
        Some(&json!({})),
    );
    assert_eq!(status, 200);
    let upload = begin["upload"].as_str().unwrap().to_string();
    let mut sent = 0usize;
    for part in bytes.chunks(CHUNK) {
        let (status, ack) = req(
            &client,
            a,
            "POST",
            &format!("/v1/uploads/{upload}/chunk"),
            &owner,
            Some(&json!({"offset": sent, "data_base64": STANDARD.encode(part)})),
        );
        assert_eq!(status, 200, "append at {sent}");
        sent += part.len();
        assert_eq!(ack["received"], sent as u64);
    }
    let (status, status_body) = req(
        &client,
        a,
        "GET",
        &format!("/v1/uploads/{upload}"),
        &owner,
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(status_body["received"], size);
    let (status, done) = req(
        &client,
        a,
        "POST",
        &format!("/v1/uploads/{upload}/commit"),
        &owner,
        Some(&json!({"name": "restored"})),
    );
    assert_eq!(status, 200);
    assert_eq!(done["imported"], "restored");

    assert_eq!(count(&client, a, &owner, "restored"), 240);
    assert_eq!(
        row(&client, a, &owner, "restored", 0),
        row(&client, a, &owner, "big", 0)
    );
    assert_eq!(
        row(&client, a, &owner, "restored", 239),
        row(&client, a, &owner, "big", 239)
    );

    // Release drops the retained snapshot; the commit already dropped the
    // upload. No transfer staging may remain on disk.
    let (status, released) = req(
        &client,
        a,
        "DELETE",
        &format!("/v1/snapshots/{snap}"),
        &owner,
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(released["released"], true);
    let (status, _) = req(
        &client,
        a,
        "DELETE",
        &format!("/v1/snapshots/{snap}"),
        &owner,
        None,
    );
    assert_eq!(status, 404);
    assert!(
        !std::fs::read_dir(&data_dir).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".transfer-")),
        "no transfer staging may leak"
    );
}

#[test]
fn chunk_errors_are_stable() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let (server, owner) = spawn(&data_dir);
    let a = &server.address;
    let client = reqwest::blocking::Client::new();
    seed_big(&client, a, &owner, "tiny", 2, 64);
    let check = |label: &str, got: (u16, Value), want: u16, code: &str| {
        assert_eq!(got.0, want, "{label}: {got:?}");
        assert_eq!(got.1["error"]["code"], code, "{label}: {got:?}");
    };

    // Unknown and malformed ids are 404 without an oracle.
    for (method, path) in [
        ("GET", "/v1/snapshots/0".to_string()),
        ("GET", format!("/v1/snapshots/{}", "0".repeat(64))),
        ("DELETE", format!("/v1/snapshots/{}", "0".repeat(64))),
    ] {
        check(
            &format!("{method} {path}"),
            req(&client, a, method, &path, &owner, None),
            404,
            "snapshot_not_found",
        );
    }
    for (method, path, body) in [
        ("GET", format!("/v1/uploads/{}", "0".repeat(64)), None),
        ("DELETE", format!("/v1/uploads/{}", "0".repeat(64)), None),
        (
            "POST",
            format!("/v1/uploads/{}/chunk", "0".repeat(64)),
            Some(json!({"offset": 0, "data_base64": ""})),
        ),
        (
            "POST",
            format!("/v1/uploads/{}/commit", "0".repeat(64)),
            Some(json!({"name": "x"})),
        ),
    ] {
        check(
            &format!("{method} {path}"),
            req(&client, a, method, &path, &owner, body.as_ref()),
            404,
            "upload_not_found",
        );
    }

    let (status, begin) = req(
        &client,
        a,
        "POST",
        "/v1/databases/tiny/export/begin",
        &owner,
        Some(&json!({})),
    );
    assert_eq!(status, 200);
    let snap = begin["snapshot"].as_str().unwrap().to_string();
    let size = begin["size"].as_u64().unwrap();

    // Malformed query params use the stable error shape, not Axum's default.
    check(
        "bad offset type",
        req(
            &client,
            a,
            "GET",
            &format!("/v1/snapshots/{snap}?offset=abc"),
            &owner,
            None,
        ),
        400,
        "invalid_request",
    );
    // Offset past the end is rejected; huge lengths clamp to one chunk.
    check(
        "offset past end",
        req(
            &client,
            a,
            "GET",
            &format!("/v1/snapshots/{snap}?offset={}", size + 1),
            &owner,
            None,
        ),
        400,
        "invalid_request",
    );
    let (status, chunk) = req(
        &client,
        a,
        "GET",
        &format!("/v1/snapshots/{snap}?offset=0&length=99999999"),
        &owner,
        None,
    );
    assert_eq!(status, 200);
    assert!(
        STANDARD
            .decode(chunk["data_base64"].as_str().unwrap())
            .unwrap()
            .len()
            <= CHUNK
    );
    // Default params fetch the first chunk.
    let (status, first) = req(
        &client,
        a,
        "GET",
        &format!("/v1/snapshots/{snap}"),
        &owner,
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(first["offset"], 0);

    // Upload discipline: exact offsets, bounded chunks.
    let (status, begin) = req(
        &client,
        a,
        "POST",
        "/v1/databases/tiny/import/begin",
        &owner,
        Some(&json!({})),
    );
    assert_eq!(status, 200);
    let upload = begin["upload"].as_str().unwrap().to_string();
    check(
        "gap offset",
        req(
            &client,
            a,
            "POST",
            &format!("/v1/uploads/{upload}/chunk"),
            &owner,
            Some(&json!({"offset": 1, "data_base64": ""})),
        ),
        400,
        "invalid_request",
    );
    check(
        "oversized chunk",
        req(
            &client,
            a,
            "POST",
            &format!("/v1/uploads/{upload}/chunk"),
            &owner,
            Some(&json!({
                "offset": 0,
                "data_base64": STANDARD.encode(vec![0u8; CHUNK + 1]),
            })),
        ),
        400,
        "invalid_request",
    );
    // Aborting twice: first ok, second 404.
    let (status, _) = req(
        &client,
        a,
        "DELETE",
        &format!("/v1/uploads/{upload}"),
        &owner,
        None,
    );
    assert_eq!(status, 200);
    check(
        "double abort",
        req(
            &client,
            a,
            "DELETE",
            &format!("/v1/uploads/{upload}"),
            &owner,
            None,
        ),
        404,
        "upload_not_found",
    );

    // Authorization: scoped keys export within scope but never upload, and
    // one principal's snapshot id is invisible to another.
    let (status, key) = req(
        &client,
        a,
        "POST",
        "/v1/keys",
        &owner,
        Some(&json!({"name": "app", "role": "database_read_write", "databases": ["tiny"]})),
    );
    assert_eq!(status, 200);
    let app = key["key"].as_str().unwrap();
    let (status, _) = req(
        &client,
        a,
        "POST",
        "/v1/databases/tiny/export/begin",
        app,
        Some(&json!({})),
    );
    assert_eq!(status, 200, "scoped export begin");
    check(
        "scoped import begin",
        req(
            &client,
            a,
            "POST",
            "/v1/databases/tiny/import/begin",
            app,
            Some(&json!({})),
        ),
        403,
        "forbidden",
    );
    check(
        "scoped upload chunk",
        req(
            &client,
            a,
            "POST",
            &format!("/v1/uploads/{}/chunk", "0".repeat(64)),
            app,
            Some(&json!({"offset": 0, "data_base64": ""})),
        ),
        403,
        "forbidden",
    );
    check(
        "foreign snapshot fetch",
        req(&client, a, "GET", &format!("/v1/snapshots/{snap}"), app, None),
        404,
        "snapshot_not_found",
    );
    // The owner may still serve anyone's snapshot chunk.
    let (status, _) = req(&client, a, "GET", &format!("/v1/snapshots/{snap}"), &owner, None);
    assert_eq!(status, 200);
}

#[test]
fn transfer_expiry_and_orphans_at_store_level() {
    let dir = tempfile::tempdir().unwrap();
    let store = dg::store::Store::open_with_ttl(dir.path(), Duration::from_secs(1)).unwrap();
    let key = store.bootstrap().unwrap();
    let principal = store.authenticate(&key).unwrap();
    store.create("ephem").unwrap();
    let path = store.path("ephem").unwrap();
    dg::execution::execute(
        &path,
        &[dg::execution::Statement {
            sql: "CREATE TABLE t (v TEXT)".into(),
            args: vec![],
        }],
        false,
        false,
        "owner",
        None,
    )
    .unwrap();

    let snap = store.export_begin(&principal, "ephem").unwrap();
    assert!(snap.size > 0);
    let upload = store.upload_begin().unwrap();
    store.upload_append(&upload.id, 0, b"partial").unwrap();
    assert_eq!(store.upload_status(&upload.id).unwrap(), 7);
    assert!(store.snapshot_chunk(&principal, &snap.id, 0, 64).is_ok());

    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(store.sweep_transfers().unwrap(), 2);
    assert_eq!(
        store
            .snapshot_chunk(&principal, &snap.id, 0, 64)
            .unwrap_err()
            .code,
        "snapshot_not_found"
    );
    assert_eq!(
        store.upload_status(&upload.id).unwrap_err().code,
        "upload_not_found"
    );
    assert!(
        !std::fs::read_dir(dir.path()).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".transfer-")),
        "sweep must remove retained dirs"
    );

    // Crash orphans (either staging prefix) are removed at open; user files stay.
    let dir2 = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir2.path().join(".transfer-aaa")).unwrap();
    std::fs::write(dir2.path().join(".transfer-aaa").join("x"), b"junk").unwrap();
    std::fs::create_dir_all(dir2.path().join(".lifecycle-bbb")).unwrap();
    std::fs::write(dir2.path().join(".lifecycle-bbb").join("y"), b"junk").unwrap();
    std::fs::write(dir2.path().join("keep.txt"), b"user").unwrap();
    dg::store::Store::open(dir2.path()).unwrap();
    assert!(!dir2.path().join(".transfer-aaa").exists());
    assert!(!dir2.path().join(".lifecycle-bbb").exists());
    assert_eq!(
        std::fs::read(dir2.path().join("keep.txt")).unwrap(),
        b"user"
    );
}

#[test]
fn cli_uses_chunked_transfer() {
    let fleet = Fleet::boot();
    let client = reqwest::blocking::Client::new();
    let owner = std::fs::read_to_string(&fleet.cli.key_file).unwrap();
    let owner = owner.trim();
    // Multi-chunk database (~3 MiB) through HTTP seeding, then CLI only.
    seed_big(&client, &fleet.cli.server, owner, "cli_big", 60, 50_000);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("cli_big.sqlite");
    let exported = fleet.cli.ok(
        "export",
        &["export", "cli_big", "--out", out.to_str().unwrap()],
    );
    let size = exported["size"].as_u64().unwrap();
    assert!(size > 2 * CHUNK as u64, "want multiple chunks, got {size}");
    assert_eq!(std::fs::metadata(&out).unwrap().len(), size);
    assert_eq!(
        hex::encode(Sha256::digest(std::fs::read(&out).unwrap())),
        exported["sha256"].as_str().unwrap()
    );

    let imported = fleet.cli.ok(
        "import",
        &["import", "cli_copy", "--from", out.to_str().unwrap()],
    );
    assert_eq!(imported["imported"], "cli_copy");
    assert_eq!(
        fleet.cli.q("count", "cli_copy", "SELECT count(*) FROM t"),
        json!([[60]])
    );
    let spot = fleet
        .cli
        .q("spot", "cli_copy", "SELECT v FROM t WHERE id = 59");
    assert_eq!(spot[0][0].as_str().unwrap().len(), 50_000);

    // No-clobber export still holds on the chunked path.
    let (success, stdout, _) = fleet
        .cli
        .run(&["export", "cli_big", "--out", out.to_str().unwrap()]);
    assert!(!success);
    assert!(stdout.contains("\"code\":\"file_exists\""), "{stdout}");
    assert_eq!(std::fs::metadata(&out).unwrap().len(), size);
}
