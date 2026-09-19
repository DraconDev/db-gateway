//! Schema-shape torture (dummy data).
//!
//! Run: `timeout 300 cargo test --locked --test schema_shapes`
//!
//! Twenty tables cover every common column affinity (INTEGER, REAL,
//! TEXT, BLOB, NUMERIC, VARCHAR, DECIMAL, defaults, NOT NULL, UNIQUE,
//! CHECK, composite keys, WITHOUT ROWID, and a FOREIGN KEY chain that
//! is enforced at runtime). Forbidden objects are pinned to their
//! fail-closed codes (CREATE INDEX included: the authorizer falls
//! through to deny, so M1 has no indexes). 5000 mixed rows go in via
//! atomic batches; every table is dumped, the server is killed and
//! restarted on the same data dir, and every dump must be identical.

use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Server {
    child: Child,
    dir: tempfile::TempDir,
    address: String,
    owner: String,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_on(dir: &tempfile::TempDir, owner: &str) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["serve", "--listen", "127.0.0.1:0", "--data-dir"])
        .arg(dir.path())
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
    Server {
        child,
        dir: tempfile::tempdir().unwrap(),
        address: address.expect("server logged its listen address"),
        owner: owner.to_string(),
    }
}

fn post(server: &Server, method: &str, path: &str, body: &Value) -> (u16, Value) {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let response = reqwest::blocking::Client::new()
        .request(
            method.parse::<reqwest::Method>().unwrap(),
            format!("http://{}{path}", server.address),
        )
        .bearer_auth(&server.owner)
        .header("Idempotency-Key", format!("shape-{n}"))
        .json(body)
        .timeout(Duration::from_secs(30))
        .send()
        .unwrap();
    (response.status().as_u16(), response.json().unwrap())
}

fn query(server: &Server, db: &str, sql: &str) -> (u16, Value) {
    post(server, "POST", &format!("/v1/databases/{db}/query"), &json!({"sql": sql}))
}

const DDLS: &[&str] = &[
    "CREATE TABLE t00(id INTEGER PRIMARY KEY, a INTEGER, b BIGINT, c SMALLINT)",
    "CREATE TABLE t01(id INTEGER PRIMARY KEY, x REAL, y DOUBLE, z FLOAT)",
    "CREATE TABLE t02(id INTEGER PRIMARY KEY, s TEXT, v VARCHAR(32), ch CHAR(4))",
    "CREATE TABLE t03(id INTEGER PRIMARY KEY, p BLOB, q VARBINARY(16))",
    "CREATE TABLE t04(id INTEGER PRIMARY KEY, n NUMERIC, d DECIMAL(10,2), b BOOLEAN)",
    "CREATE TABLE t05(id INTEGER PRIMARY KEY, req TEXT NOT NULL, def TEXT DEFAULT 'dflt', num INTEGER DEFAULT -7)",
    "CREATE TABLE t06(id INTEGER PRIMARY KEY, u TEXT UNIQUE, w INTEGER UNIQUE)",
    "CREATE TABLE t07(id INTEGER PRIMARY KEY, pos INTEGER CHECK(pos > 0), name TEXT CHECK(length(name) <= 8))",
    "CREATE TABLE t08(a INTEGER, b TEXT, c REAL, PRIMARY KEY(a, b))",
    "CREATE TABLE t09(k TEXT PRIMARY KEY, v BLOB NOT NULL) WITHOUT ROWID",
    "CREATE TABLE t10(parent INTEGER PRIMARY KEY, label TEXT)",
    "CREATE TABLE t11(child INTEGER PRIMARY KEY, parent INTEGER REFERENCES t10(parent), note TEXT)",
    "CREATE TABLE t12(id INTEGER PRIMARY KEY, ts TEXT, dt DATE, t TIME)",
    "CREATE TABLE t13(id INTEGER PRIMARY KEY, j TEXT, big TEXT)",
    "CREATE TABLE t14(id INTEGER PRIMARY KEY AUTOINCREMENT, v INTEGER)",
    "CREATE TABLE t15(a INT, b UNSIGNED BIG INT, c MEDIUMINT, d TINYINT)",
    "CREATE TABLE t16(id INTEGER PRIMARY KEY, e TEXT DEFAULT '', f REAL DEFAULT 0.0, g BLOB)",
    "CREATE TABLE t17(id INTEGER PRIMARY KEY, m TEXT, n TEXT, o TEXT)",
    "CREATE TABLE t18(id INTEGER PRIMARY KEY, low INTEGER DEFAULT -9223372036854775808, high INTEGER DEFAULT 9223372036854775807)",
    "CREATE TABLE t19(id INTEGER PRIMARY KEY, uni TEXT, em TEXT, nl TEXT)",
];

/// Fail-closed objects: (sql, status, code). Pinned by probe 2026-09-19.
const FORBIDDEN: &[(&str, u16, &str)] = &[
    ("CREATE VIEW fv AS SELECT 1", 403, "sql_forbidden"),
    ("CREATE TRIGGER ftr AFTER INSERT ON t00 BEGIN SELECT 1; END", 403, "sql_forbidden"),
    ("CREATE TEMP TABLE ftt(x)", 403, "sql_forbidden"),
    ("ALTER TABLE t00 ADD COLUMN extra TEXT", 403, "sql_forbidden"),
    ("CREATE INDEX fidx ON t00(a)", 403, "sql_forbidden"),
    ("CREATE UNIQUE INDEX fuidx ON t01(x)", 403, "sql_forbidden"),
    ("INSERT INTO t11(child, parent) VALUES (1, 99999)", 400, "invalid_sql"),
];

#[test]
fn twenty_shapes_survive_restart_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let owner: String = serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string();

    let server = spawn_on(&dir, &owner);
    let (status, body) = post(&server, "POST", "/v1/databases", &json!({"name": "shapes"}));
    assert_eq!(status, 200, "create: {body}");

    for ddl in DDLS {
        let (status, body) = query(&server, "shapes", ddl);
        assert_eq!(status, 200, "{ddl}: {body}");
    }
    // Scratch table proves DROP TABLE is allowed, then leaves no trace.
    let (status, _) = query(&server, "shapes", "CREATE TABLE t_scratch(x)");
    assert_eq!(status, 200);
    let (status, _) = query(&server, "shapes", "DROP TABLE t_scratch");
    assert_eq!(status, 200);

    for (sql, status, code) in FORBIDDEN {
        let (got_status, got_body) = query(&server, "shapes", sql);
        assert_eq!(got_status, *status, "{sql}: {got_body}");
        assert_eq!(got_body["error"]["code"], *code, "{sql}");
    }

    // Seed parents before children (FK chain t10 -> t11 enforced).
    let (status, body) = query(&server, "shapes", "INSERT INTO t10(parent, label) VALUES (1, 'one')");
    assert_eq!(status, 200, "parent seed: {body}");

    // 5000 mixed rows: 250 per table, NULLs, extremes, unicode, blobs.
    let mut stmts: Vec<Value> = Vec::new();
    for (t, ddl) in DDLS.iter().enumerate() {
        let table = &ddl[13..16];
        for r in 0..250 {
            let sql = match t % 10 {
                0 => format!("INSERT INTO {table}(a, b, c) VALUES ({}, {}, {})", r as i64 - 125, r, r % 7),
                1 => format!("INSERT INTO {table}(x, y, z) VALUES ({}, 1e308, -1e-10)", r as f64 * 0.5),
                2 => format!("INSERT INTO {table}(s, v, ch) VALUES ('txt-{r}-ünïcödé-🎉', 'v{r}', 'c{r}')"),
                3 => format!("INSERT INTO {table}(p, q) VALUES (randomblob({}), x'00FF{r:02X}')", 8 + r % 64),
                4 => format!("INSERT INTO {table}(n, d, b) VALUES ({r}.5, {r}.99, {})", r % 2),
                5 => format!("INSERT INTO {table}(req) VALUES ({})", if r % 5 == 0 { "NULL" } else { "'present'" }),
                6 => format!("INSERT INTO {table}(u, w) VALUES ('u-{t}-{r}', {r})"),
                7 => format!("INSERT INTO {table}(pos, name) VALUES ({}, 'n{r}')", r + 1),
                8 => format!("INSERT INTO {table}(a, b, c) VALUES ({r}, 'k-{t}-{r}', {r})"),
                _ => format!("INSERT INTO {table} VALUES ({}, {}, {})", r + 1, r, r),
            };
            // Tables whose shape differs get tailored rows.
            let sql = match t {
                9 => format!("INSERT INTO {table}(k, v) VALUES ('k-{r}', randomblob(24))"),
                11 => format!("INSERT INTO {table}(child, parent, note) VALUES ({r}, 1, {})", if r % 4 == 0 { "NULL" } else { "'ok'" }),
                13 => format!("INSERT INTO {table}(j, big) VALUES ('{{\"r\":{r}}}', zeroblob({}))", 100 + r),
                14 => format!("INSERT INTO {table}(v) VALUES ({r})"),
                16 => format!("INSERT INTO {table}(g) VALUES ({})", if r % 3 == 0 { "NULL" } else { "x'DEAD'" }),
                17 => format!("INSERT INTO {table}(m, n, o) VALUES ('', NULL, 'x')"),
                19 => format!("INSERT INTO {table}(uni, em, nl) VALUES ('héllo-{r}', '😀😀', NULL)"),
                _ => sql,
            };
            stmts.push(json!({"sql": sql}));
        }
    }
    // t11 ids start at 0; child PK allows 0. t08 composite rows distinct.
    let mut done = 0;
    for chunk in stmts.chunks(32) {
        let (status, body) = post(
            &server,
            "POST",
            "/v1/databases/shapes/batch",
            &json!({"statements": chunk, "atomic": true}),
        );
        assert_eq!(status, 200, "seed chunk at {done}: {body}");
        done += chunk.len();
    }
    assert_eq!(done, 5000);

    // sqlite_master: exactly our 20 tables, no views/triggers/indexes.
    let (status, body) = query(
        &server,
        "shapes",
        "SELECT type, name FROM sqlite_master WHERE name NOT LIKE '_dg_%' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    );
    assert_eq!(status, 200, "master: {body}");
    let types: Vec<String> = body["rows"].as_array().unwrap().iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    assert_eq!(types.len(), 20, "unexpected objects: {body}");
    assert!(types.iter().all(|t| t == "table"), "non-table objects: {body}");

    // Full ordered dump of every table (blobs hex-encoded for stability).
    let dump = |server: &Server| -> Vec<Value> {
        let mut out = Vec::new();
        for ddl in DDLS {
            let table = &ddl[13..16];
            // Column list from the live schema keeps the dump exact.
            let (status, cols) = query(&server, "shapes", &format!("SELECT name FROM pragma_table_info('{table}') ORDER BY cid"));
            assert_eq!(status, 200, "cols {table}: {cols}");
            let select: Vec<String> = cols["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| {
                    let c = r[0].as_str().unwrap();
                    format!("hex({c})")
                })
                .collect();
            let (status, body) = query(
                &server,
                "shapes",
                &format!("SELECT {} FROM {table} ORDER BY 1", select.join(", ")),
            );
            assert_eq!(status, 200, "dump {table}: {body}");
            out.push(body["rows"].clone());
        }
        out
    };
    let before = dump(&server);
    let total: usize = before.iter().map(|r| r.as_array().unwrap().len()).sum();
    assert_eq!(total, 5000 + 1, "row total (5000 + 1 parent seed)");

    // Restart on the same data dir: everything must be identical.
    drop(server);
    let dir2 = dir;
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir"])
        .arg(dir2.path())
        .output()
        .unwrap();
    assert!(!output.status.success(), "re-init must refuse a bootstrapped dir");
    let server2 = spawn_on(&dir2, &owner);
    let after = dump(&server2);
    assert_eq!(before, after, "post-restart dump differs");
}
