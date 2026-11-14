//! Differential testing: the gateway against plain SQLite.
//!
//! Run: `timeout 600 cargo test --locked --test differential`
//! Long run: `DG_DIFF_SEEDS=200 DG_DIFF_OPS=1000 cargo test --release --locked --test differential -- --nocapture`
//! Reproduce one failure: `DG_DIFF_SEED=<seed> DG_DIFF_OPS=<n> cargo test --locked --test differential`
//!
//! The promise `dg` makes is that a database behind it behaves like the
//! SQLite file it is. This test holds it to that on inputs nobody wrote by
//! hand: a seeded generator produces random schemas, migrations (ALTER,
//! views, triggers, indexes), writes (with awkward values: extreme
//! integers and floats, empty and unicode text, blobs, NULL) and queries,
//! and every operation runs twice -- once through the gateway's HTTP API
//! and once on a reference SQLite file opened with the gateway's own
//! connection settings. The two must agree on every outcome: success or
//! failure, the returned rows, the column names and `rows_affected`. Every
//! so often, and at the end, the whole database is dumped from both sides
//! (the schema and every table) and compared, and a clone made through the
//! gateway must match the reference too.
//!
//! The generator reads the CURRENT schema from the reference database
//! before each operation, so it never needs its own model of what exists:
//! whatever the last migration left is what the next operation is drawn
//! from. A failure prints the seed, the operation index and the last
//! operations, which reproduce it exactly.

use rusqlite::{
    Connection,
    limits::Limit,
    types::{Value as SqlValue, ValueRef},
};
use serde_json::{Number, Value, json};
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

// ---------------------------------------------------------------- server

struct Server {
    child: Child,
    address: String,
    owner: String,
    http: reqwest::blocking::Client,
    _dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn boot() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let output = Command::new(env!("CARGO_BIN_EXE_dg"))
            .args(["init", "--data-dir"])
            .arg(&data)
            .output()
            .unwrap();
        assert!(output.status.success(), "init failed");
        let owner = serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
            .as_str()
            .unwrap()
            .to_string();
        let mut child = Command::new(env!("CARGO_BIN_EXE_dg"))
            .args(["serve", "--listen", "127.0.0.1:0", "--data-dir"])
            .arg(&data)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let mut address = None;
        for line in lines.by_ref() {
            let line = line.unwrap();
            if let Ok(log) = serde_json::from_str::<Value>(&line)
                && let Some(addr) = log["fields"]["address"].as_str()
            {
                address = Some(addr.to_string());
                break;
            }
        }
        std::thread::spawn(move || for _ in lines {});
        Server {
            child,
            address: address.expect("server never logged its address"),
            owner,
            http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
            _dir: dir,
        }
    }

    fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        let response = self
            .http
            .post(format!("http://{}{path}", self.address))
            .bearer_auth(&self.owner)
            .json(body)
            .send()
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().unwrap_or(Value::Null))
    }
}

// ------------------------------------------------------------- reference

/// A plain SQLite file opened the way the gateway opens its databases
/// (`open` and `initialize` in src/execution.rs), so a difference in
/// outcome is the gateway's doing and not a configuration difference.
fn reference(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL; \
         PRAGMA trusted_schema=OFF; PRAGMA cache_size=-2048; PRAGMA temp_store=MEMORY; \
         CREATE TABLE IF NOT EXISTS _dg_replay (principal TEXT NOT NULL, key TEXT NOT NULL, \
         hash TEXT NOT NULL, result TEXT NOT NULL, created INTEGER NOT NULL, \
         PRIMARY KEY(principal,key));",
    )
    .unwrap();
    conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, 1024 * 1024)
        .unwrap();
    conn.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024)
        .unwrap();
    conn.set_limit(Limit::SQLITE_LIMIT_COLUMN, 128).unwrap();
    conn.set_limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 256)
        .unwrap();
    conn.set_limit(Limit::SQLITE_LIMIT_VDBE_OP, 100_000)
        .unwrap();
    conn
}

/// The reference's value, in the encoding the API documents
/// (docs/api.md, "Arguments and returned values map JSON <-> SQLite").
/// Written here independently of src/values.rs, so a bug there cannot hide
/// by being applied to both sides. The two lossy coercions are applied
/// because the API documents them, and only those two: a non-finite REAL
/// has no JSON number and comes back `null`, and TEXT that is not UTF-8
/// comes back with replacement characters.
fn sql_to_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => Number::from_f64(f).map_or(Value::Null, Value::Number),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
        ValueRef::Blob(b) => json!(b),
    }
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Ok {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
        rows_affected: u64,
    },
    Err(String),
}

impl Outcome {
    fn to_json(&self) -> Value {
        match self {
            Outcome::Ok {
                columns,
                rows,
                rows_affected,
            } => json!({"columns": columns, "rows": rows, "rows_affected": rows_affected}),
            Outcome::Err(e) => json!({"error": e}),
        }
    }
}

fn run_reference(conn: &Connection, sql: &str, args: &[Arg]) -> Outcome {
    let result = (|| -> rusqlite::Result<Outcome> {
        let mut stmt = conn.prepare(sql)?;
        let readonly = stmt.readonly();
        let columns: Vec<String> = stmt.column_names().map(str::to_string).collect();
        let params: Vec<SqlValue> = args.iter().map(Arg::to_sql).collect();
        let mut cursor = stmt.query(rusqlite::params_from_iter(params))?;
        let mut rows = Vec::new();
        while let Some(row) = cursor.next()? {
            let mut out = Vec::with_capacity(columns.len());
            for i in 0..columns.len() {
                out.push(sql_to_json(row.get_ref(i)?));
            }
            rows.push(out);
        }
        drop(cursor);
        Ok(Outcome::Ok {
            columns,
            rows,
            rows_affected: if readonly { 0 } else { conn.changes() },
        })
    })();
    result.unwrap_or_else(|e| Outcome::Err(e.to_string()))
}

/// The gateway opens a connection per request, and SQLite's change counter
/// starts at zero on a fresh connection. The reference keeps one
/// connection, so before each request it completes a no-op DELETE to put
/// the counter where a fresh connection would have it.
fn fresh_change_counter(conn: &Connection) {
    conn.execute("DELETE FROM _dg_replay WHERE 0", []).unwrap();
}

fn gateway_outcome(status: u16, body: &Value) -> Outcome {
    if status == 200 {
        Outcome::Ok {
            columns: serde_json::from_value(body["columns"].clone()).unwrap(),
            rows: serde_json::from_value(body["rows"].clone()).unwrap(),
            rows_affected: body["rows_affected"].as_u64().unwrap(),
        }
    } else {
        Outcome::Err(body["error"]["code"].as_str().unwrap_or("?").to_string())
    }
}

/// Success and failure must agree; on success everything must match. The
/// error texts are not compared: the gateway reports a code, SQLite a
/// message, and agreeing that a statement fails is the contract.
fn agree(a: &Outcome, b: &Outcome) -> bool {
    match (a, b) {
        (Outcome::Err(_), Outcome::Err(_)) => true,
        _ => a == b,
    }
}

// -------------------------------------------------------------- generator

/// SplitMix64: tiny, seedable, and stable across dependency upgrades, so a
/// printed seed reproduces the same run forever.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

#[derive(Clone, Debug)]
enum Arg {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
    Bool(bool),
}

impl Arg {
    fn to_json(&self) -> Value {
        match self {
            Arg::Null => Value::Null,
            Arg::Int(i) => json!(i),
            Arg::Real(f) => json!(f),
            Arg::Text(t) => json!(t),
            Arg::Blob(b) => json!(b),
            Arg::Bool(b) => json!(b),
        }
    }
    fn to_sql(&self) -> SqlValue {
        match self {
            Arg::Null => SqlValue::Null,
            Arg::Int(i) => SqlValue::Integer(*i),
            Arg::Real(f) => SqlValue::Real(*f),
            Arg::Text(t) => SqlValue::Text(t.clone()),
            Arg::Blob(b) => SqlValue::Blob(b.clone()),
            // The API documents a JSON boolean as the integer 1 or 0.
            Arg::Bool(b) => SqlValue::Integer(i64::from(*b)),
        }
    }
    /// The same value written into the SQL text instead of bound.
    fn literal(&self) -> String {
        match self {
            Arg::Null => "NULL".into(),
            Arg::Int(i) => i.to_string(),
            Arg::Real(f) => format!("{f:?}"),
            Arg::Text(t) => format!("'{}'", t.replace('\'', "''")),
            Arg::Blob(b) => format!("X'{}'", hex::encode(b)),
            Arg::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        }
    }
}

const TEXTS: &[&str] = &[
    "",
    "a",
    "NULL",
    "null",
    "it's",
    "\"quoted\"",
    "ünïcödé",
    "日本語のテキスト",
    "🦀🔥",
    "line\nbreak",
    "tab\tstop",
    "%_wild",
    "  padded  ",
    "123",
    "-4.5",
    "0x1F",
    "\u{0}nul",
    "\u{1b}[31mred",
];

fn value(rng: &mut Rng) -> Arg {
    match rng.below(12) {
        0 => Arg::Null,
        1 => Arg::Int(*rng.pick(&[0, 1, -1, i64::MIN, i64::MAX, 42, 1 << 53])),
        2 | 3 => Arg::Int(rng.below(2001) as i64 - 1000),
        4 => Arg::Real(*rng.pick(&[
            0.0,
            -0.0,
            0.5,
            -1.5,
            1e308,
            -1e308,
            5e-324,
            std::f64::consts::PI,
            1.0,
            9_007_199_254_740_993.0,
        ])),
        5 => Arg::Real((rng.below(200_000) as f64 - 100_000.0) / 7.0),
        6..=8 => Arg::Text(rng.pick(TEXTS).to_string()),
        9 => {
            let n = rng.below(40);
            Arg::Text(
                (0..n)
                    .map(|_| (b'a' + rng.below(26) as u8) as char)
                    .collect(),
            )
        }
        10 => {
            let n = rng.below(16);
            Arg::Blob((0..n).map(|_| rng.below(256) as u8).collect())
        }
        _ => Arg::Bool(rng.chance(50)),
    }
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[derive(Clone)]
struct Column {
    name: String,
    pk: bool,
}

struct Table {
    name: String,
    columns: Vec<Column>,
    rows: i64,
}

/// What exists now, read from the reference database.
struct Schema {
    tables: Vec<Table>,
    views: Vec<(String, usize)>,
    indexes: Vec<String>,
    triggers: Vec<String>,
}

fn user_objects(conn: &Connection, kind: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type=?1 AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
             AND lower(name) NOT LIKE '\\_dg\\_%' ESCAPE '\\' ORDER BY name",
        )
        .unwrap();
    stmt.query_map([kind], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn read_schema(conn: &Connection) -> Schema {
    let tables = user_objects(conn, "table")
        .into_iter()
        .map(|name| {
            let mut stmt = conn
                .prepare("SELECT name, pk FROM pragma_table_info(?1) ORDER BY cid")
                .unwrap();
            let columns = stmt
                .query_map([&name], |r| {
                    Ok(Column {
                        name: r.get(0)?,
                        pk: r.get::<_, i64>(1)? > 0,
                    })
                })
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            let rows = conn
                .query_row(&format!("SELECT count(*) FROM {}", quote(&name)), [], |r| {
                    r.get(0)
                })
                .unwrap_or(0);
            Table {
                name,
                columns,
                rows,
            }
        })
        .collect();
    // A view whose definition no longer resolves (a column it named was
    // renamed away) cannot be prepared; it is left out of the draw.
    let views = user_objects(conn, "view")
        .into_iter()
        .filter_map(|name| {
            conn.prepare(&format!("SELECT * FROM {}", quote(&name)))
                .ok()
                .map(|s| (name, s.column_count()))
        })
        .collect();
    Schema {
        tables,
        views,
        indexes: user_objects(conn, "index"),
        triggers: user_objects(conn, "trigger"),
    }
}

/// Fresh identifiers. Some are awkward on purpose: spaces, unicode, a
/// keyword, a double quote -- all legal when quoted, which the generator
/// always does.
fn identifier(rng: &mut Rng, prefix: &str, counter: &mut u32) -> String {
    *counter += 1;
    let n = *counter;
    match rng.below(10) {
        0 => format!("{prefix} spaced {n}"),
        1 => format!("{prefix}_ünï_{n}"),
        2 => format!("select{n}"),
        3 => format!("{prefix}\"q{n}"),
        _ => format!("{prefix}{n}"),
    }
}

const TYPES: &[&str] = &[
    "INTEGER",
    "REAL",
    "TEXT",
    "BLOB",
    "NUMERIC",
    "",
    "VARCHAR(10)",
    "BOOLEAN",
    "ANY",
];

struct Op {
    kind: &'static str,
    statements: Vec<(String, Vec<Arg>)>,
}

fn order_by_all(n: usize) -> String {
    (1..=n)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn where_clause(rng: &mut Rng, table: &Table, args: &mut Vec<Arg>) -> String {
    let column = quote(&rng.pick(&table.columns).name);
    match rng.below(7) {
        0 => format!("{column} IS NULL"),
        1 => format!("{column} IS NOT NULL"),
        2 => {
            args.push(value(rng));
            format!("{column} > ?")
        }
        3 => {
            args.push(value(rng));
            format!("{column} = ?")
        }
        4 => format!("{column} LIKE '{}%'", (b'a' + rng.below(26) as u8) as char),
        5 => format!("rowid % {} = 0", 2 + rng.below(4)),
        _ => format!(
            "{column} IN ({}, {})",
            value(rng).literal(),
            value(rng).literal()
        ),
    }
}

fn write_statement(rng: &mut Rng, schema: &Schema) -> Option<(String, Vec<Arg>)> {
    let table = schema.tables.get(rng.below(schema.tables.len().max(1)))?;
    let name = quote(&table.name);
    let mut args = Vec::new();
    let sql = match rng.below(10) {
        // Bounded tables keep every result well under the gateway's
        // response caps, which this test is not about.
        0..=4 if table.rows < 150 => {
            let columns: Vec<&Column> = table
                .columns
                .iter()
                .filter(|c| !c.pk || rng.chance(30))
                .collect();
            if columns.is_empty() {
                format!("INSERT INTO {name} DEFAULT VALUES")
            } else {
                let rows = 1 + rng.below(3);
                let mut groups = Vec::new();
                for _ in 0..rows {
                    let mut slots = Vec::new();
                    for column in &columns {
                        // A rowid column never gets i64::MAX: once a table
                        // holds that rowid, SQLite draws later rowids at
                        // random, so no two files could agree. Every other
                        // column still sees the extreme values.
                        let v = if column.pk {
                            match rng.below(3) {
                                0 => Arg::Null,
                                _ => Arg::Int(rng.below(400) as i64 - 50),
                            }
                        } else {
                            value(rng)
                        };
                        if rng.chance(25) {
                            slots.push(v.literal());
                        } else {
                            args.push(v);
                            slots.push("?".into());
                        }
                    }
                    groups.push(format!("({})", slots.join(", ")));
                }
                let verb = *rng.pick(&["INSERT", "INSERT OR IGNORE", "INSERT OR REPLACE"]);
                let names: Vec<String> = columns.iter().map(|c| quote(&c.name)).collect();
                format!(
                    "{verb} INTO {name}({}) VALUES {}",
                    names.join(", "),
                    groups.join(", ")
                )
            }
        }
        5..=7 => {
            let column = rng.pick(&table.columns).clone();
            // The same rowid rule as INSERT: no i64::MAX into a rowid.
            args.push(if column.pk {
                Arg::Int(rng.below(400) as i64 - 50)
            } else {
                value(rng)
            });
            let column = quote(&column.name);
            let filter = where_clause(rng, table, &mut args);
            format!("UPDATE {name} SET {column} = ? WHERE {filter}")
        }
        _ => {
            let filter = where_clause(rng, table, &mut args);
            format!("DELETE FROM {name} WHERE {filter}")
        }
    };
    Some((sql, args))
}

fn next_op(rng: &mut Rng, schema: &Schema, counter: &mut u32) -> Op {
    let one = |kind, sql: String| Op {
        kind,
        statements: vec![(sql, Vec::new())],
    };
    let have_tables = !schema.tables.is_empty();
    let roll = rng.below(100);
    if !have_tables || roll < 6 {
        let table = identifier(rng, "t", counter);
        let mut columns = Vec::new();
        if rng.chance(60) {
            columns.push(format!(
                "{} INTEGER PRIMARY KEY",
                quote(&identifier(rng, "id", counter))
            ));
        }
        for _ in 0..1 + rng.below(4) {
            let mut column = format!(
                "{} {}",
                quote(&identifier(rng, "c", counter)),
                rng.pick(TYPES)
            );
            if rng.chance(15) {
                column.push_str(" UNIQUE");
            }
            if rng.chance(10) {
                column.push_str(&format!(" NOT NULL DEFAULT {}", value(rng).literal()));
            } else if rng.chance(10) {
                column.push_str(&format!(" DEFAULT {}", value(rng).literal()));
            }
            columns.push(column);
        }
        let strict = if rng.chance(10) { " STRICT" } else { "" };
        return one(
            "create table",
            format!(
                "CREATE TABLE {} ({}){strict}",
                quote(&table),
                columns.join(", ")
            ),
        );
    }
    let table = rng.pick(&schema.tables);
    let name = quote(&table.name);
    match roll {
        6..=40 => Op {
            kind: "write",
            statements: vec![write_statement(rng, schema).unwrap()],
        },
        41..=62 => {
            let mut args = Vec::new();
            let n = table.columns.len();
            let sql = match rng.below(6) {
                0 | 1 => {
                    let filter = where_clause(rng, table, &mut args);
                    format!(
                        "SELECT * FROM {name} WHERE {filter} ORDER BY {}",
                        order_by_all(n)
                    )
                }
                2 => {
                    let c = quote(&rng.pick(&table.columns).name);
                    format!(
                        "SELECT count(*), count({c}), sum({c}), total({c}), min({c}), max({c}), \
                         typeof(max({c})) FROM {name}"
                    )
                }
                3 => {
                    let c = quote(&rng.pick(&table.columns).name);
                    format!(
                        "SELECT {c}, count(*), group_concat(typeof({c})) FROM {name} \
                         GROUP BY {c} ORDER BY 1"
                    )
                }
                4 => {
                    let other = rng.pick(&schema.tables);
                    let m = other.columns.len();
                    format!(
                        "SELECT a.*, b.* FROM {name} a JOIN {} b ON a.rowid = b.rowid \
                         ORDER BY {}",
                        quote(&other.name),
                        order_by_all(n + m)
                    )
                }
                _ => {
                    let c = quote(&rng.pick(&table.columns).name);
                    format!(
                        "SELECT {c}, typeof({c}), quote({c}), length({c}), CAST({c} AS TEXT), \
                         json_quote({c}) FROM {name} ORDER BY 1, 2, 3"
                    )
                }
            };
            Op {
                kind: "select",
                statements: vec![(sql, args)],
            }
        }
        63..=67 => {
            if let Some((view, n)) = schema.views.get(rng.below(schema.views.len().max(1))) {
                one(
                    "select view",
                    format!(
                        "SELECT * FROM {} ORDER BY {}",
                        quote(view),
                        order_by_all(*n)
                    ),
                )
            } else {
                one("select", format!("SELECT count(*) FROM {name}"))
            }
        }
        68..=71 => {
            let column = quote(&identifier(rng, "c", counter));
            let default = if rng.chance(40) {
                format!(" DEFAULT {}", value(rng).literal())
            } else {
                String::new()
            };
            one(
                "alter add",
                format!(
                    "ALTER TABLE {name} ADD COLUMN {column} {}{default}",
                    rng.pick(TYPES)
                ),
            )
        }
        72..=73 => {
            let column = quote(&rng.pick(&table.columns).name);
            let renamed = quote(&identifier(rng, "c", counter));
            one(
                "alter rename column",
                format!("ALTER TABLE {name} RENAME COLUMN {column} TO {renamed}"),
            )
        }
        74..=75 => {
            let column = quote(&rng.pick(&table.columns).name);
            one(
                "alter drop column",
                format!("ALTER TABLE {name} DROP COLUMN {column}"),
            )
        }
        76 => one(
            "alter rename table",
            format!(
                "ALTER TABLE {name} RENAME TO {}",
                quote(&identifier(rng, "t", counter))
            ),
        ),
        77..=79 => {
            let column = quote(&rng.pick(&table.columns).name);
            let unique = if rng.chance(30) { "UNIQUE " } else { "" };
            one(
                "create index",
                format!(
                    "CREATE {unique}INDEX {} ON {name}({column})",
                    quote(&identifier(rng, "i", counter))
                ),
            )
        }
        80 if !schema.indexes.is_empty() => one(
            "drop index",
            format!("DROP INDEX {}", quote(rng.pick(&schema.indexes))),
        ),
        81..=83 => {
            let column = quote(&rng.pick(&table.columns).name);
            let mut args = Vec::new();
            let filter = where_clause(rng, table, &mut args);
            // A view is stored SQL, so bound parameters cannot appear in it.
            let filter = args
                .iter()
                .fold(filter, |f, a| f.replacen('?', &a.literal(), 1));
            one(
                "create view",
                format!(
                    "CREATE VIEW {} AS SELECT {column}, rowid AS r FROM {name} WHERE {filter}",
                    quote(&identifier(rng, "v", counter))
                ),
            )
        }
        84 if !schema.views.is_empty() => one(
            "drop view",
            format!("DROP VIEW {}", quote(&rng.pick(&schema.views).0)),
        ),
        85..=87 => {
            let target = rng.pick(&schema.tables);
            let event = *rng.pick(&["INSERT", "UPDATE", "DELETE"]);
            let row = if event == "DELETE" { "old" } else { "new" };
            let column = &rng.pick(&target.columns).name;
            let body = match rng.below(3) {
                0 if target.name != table.name => format!(
                    "INSERT INTO {}({}) VALUES ({row}.rowid)",
                    quote(&target.name),
                    quote(column)
                ),
                1 if target.name != table.name => format!(
                    "UPDATE {} SET {} = {row}.rowid WHERE rowid = 1",
                    quote(&target.name),
                    quote(column)
                ),
                _ => "SELECT 1".to_string(),
            };
            one(
                "create trigger",
                format!(
                    "CREATE TRIGGER {} AFTER {event} ON {name} BEGIN {body}; END",
                    quote(&identifier(rng, "tr", counter))
                ),
            )
        }
        88 if !schema.triggers.is_empty() => one(
            "drop trigger",
            format!("DROP TRIGGER {}", quote(rng.pick(&schema.triggers))),
        ),
        89 if schema.tables.len() > 2 => one("drop table", format!("DROP TABLE {name}")),
        _ => {
            // An atomic batch: all of it lands or none of it does. One in
            // four carries a statement that must fail, which must take the
            // whole batch down with it.
            let mut statements: Vec<_> = (0..2 + rng.below(4))
                .filter_map(|_| write_statement(rng, schema))
                .collect();
            if rng.chance(25) {
                let at = rng.below(statements.len() + 1);
                statements.insert(at, ("INSERT INTO no_such_table VALUES (1)".into(), vec![]));
            }
            Op {
                kind: "batch",
                statements,
            }
        }
    }
}

// ----------------------------------------------------------------- driver

struct Run<'a> {
    server: &'a Server,
    db: String,
    seed: u64,
    reference: Connection,
    log: Vec<String>,
}

impl Run<'_> {
    fn fail(&self, index: usize, what: &str) -> ! {
        let tail = self
            .log
            .iter()
            .rev()
            .take(25)
            .rev()
            .cloned()
            .collect::<Vec<_>>();
        panic!(
            "differential mismatch: seed {} op {index}: {what}\n\
             reproduce: DG_DIFF_SEED={} DG_DIFF_OPS={} cargo test --locked --test differential\n\
             last operations:\n  {}",
            self.seed,
            self.seed,
            index + 1,
            tail.join("\n  ")
        );
    }

    fn apply(&mut self, index: usize, op: &Op) {
        self.log.push(format!(
            "[{index}] {}: {}",
            op.kind,
            op.statements
                .iter()
                .map(|(sql, args)| format!(
                    "{sql} {:?}",
                    args.iter().map(Arg::to_json).collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>()
                .join(" ; ")
        ));
        let path = format!("/v1/databases/{}", self.db);
        fresh_change_counter(&self.reference);
        if op.kind == "batch" {
            let body = json!({
                "atomic": true,
                "statements": op.statements.iter().map(|(sql, args)| {
                    json!({"sql": sql, "args": args.iter().map(Arg::to_json).collect::<Vec<_>>()})
                }).collect::<Vec<_>>(),
            });
            let (status, response) = self.server.post(&format!("{path}/batch"), &body);
            self.check_status(index, status, &response);
            self.reference.execute_batch("BEGIN IMMEDIATE").unwrap();
            let mut outcomes = Vec::new();
            let mut failed = false;
            for (sql, args) in &op.statements {
                let outcome = run_reference(&self.reference, sql, args);
                failed = matches!(outcome, Outcome::Err(_));
                outcomes.push(outcome);
                if failed {
                    break;
                }
            }
            self.reference
                .execute_batch(if failed { "ROLLBACK" } else { "COMMIT" })
                .unwrap();
            match (status == 200, failed) {
                (true, false) => {
                    let got: Vec<Outcome> = response["results"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|r| gateway_outcome(200, r))
                        .collect();
                    if got != outcomes {
                        self.fail(
                            index,
                            &format!(
                                "batch results differ\n  gateway:   {}\n  reference: {}",
                                json!(got.iter().map(Outcome::to_json).collect::<Vec<_>>()),
                                json!(outcomes.iter().map(Outcome::to_json).collect::<Vec<_>>())
                            ),
                        );
                    }
                }
                (false, true) => {}
                _ => self.fail(
                    index,
                    &format!(
                        "batch: gateway {} but reference {}\n  gateway:   {response}\n  reference: {}",
                        if status == 200 { "succeeded" } else { "failed" },
                        if failed { "failed" } else { "succeeded" },
                        json!(outcomes.iter().map(Outcome::to_json).collect::<Vec<_>>())
                    ),
                ),
            }
        } else {
            let (sql, args) = &op.statements[0];
            let body =
                json!({"sql": sql, "args": args.iter().map(Arg::to_json).collect::<Vec<_>>()});
            let (status, response) = self.server.post(&format!("{path}/query"), &body);
            self.check_status(index, status, &response);
            let got = gateway_outcome(status, &response);
            let want = run_reference(&self.reference, sql, args);
            if !agree(&got, &want) {
                self.fail(
                    index,
                    &format!(
                        "outcomes differ\n  gateway:   {} (HTTP {status})\n  reference: {}",
                        got.to_json(),
                        want.to_json()
                    ),
                );
            }
        }
    }

    /// Whatever the reference says, a 5xx is the gateway's own failure, and
    /// a refusal the SQL rules do not explain is a gateway difference.
    fn check_status(&self, index: usize, status: u16, body: &Value) {
        let code = body["error"]["code"].as_str().unwrap_or("");
        if status >= 500 || matches!(code, "sql_forbidden" | "result_limit_exceeded") {
            self.fail(
                index,
                &format!("gateway refused with HTTP {status}: {body}"),
            );
        }
    }

    /// The whole database, both sides: schema rows and every table's rows.
    fn compare_dumps(&self, index: usize, db: &str) {
        let schema_sql = "SELECT type, name, tbl_name, sql FROM sqlite_master \
             WHERE lower(name) NOT LIKE '\\_dg\\_%' ESCAPE '\\' ORDER BY type, name";
        let mut queries = vec![schema_sql.to_string()];
        for table in read_schema(&self.reference).tables {
            queries.push(format!(
                "SELECT * FROM {} ORDER BY {}",
                quote(&table.name),
                order_by_all(table.columns.len())
            ));
        }
        for sql in queries {
            let (status, response) = self
                .server
                .post(&format!("/v1/databases/{db}/query"), &json!({"sql": sql}));
            let got = gateway_outcome(status, &response);
            let want = run_reference(&self.reference, &sql, &[]);
            if got != want {
                self.fail(
                    index,
                    &format!(
                        "dump of {db} differs for `{sql}`\n  gateway:   {}\n  reference: {}",
                        got.to_json(),
                        want.to_json()
                    ),
                );
            }
        }
    }
}

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .map(|v| {
            v.parse()
                .unwrap_or_else(|_| panic!("{name} must be a number"))
        })
        .unwrap_or(default)
}

fn run_seed(server: &Server, seed: u64, ops: usize) {
    let db = format!("diff{seed}");
    let (status, body) = server.post("/v1/databases", &json!({"name": db}));
    assert_eq!(status, 200, "create {db}: {body}");
    let dir = tempfile::tempdir().unwrap();
    let mut run = Run {
        server,
        db: db.clone(),
        seed,
        reference: reference(&dir.path().join("reference.sqlite")),
        log: Vec::new(),
    };
    let mut rng = Rng(seed);
    let mut counter = 0;
    for index in 0..ops {
        let schema = read_schema(&run.reference);
        let op = next_op(&mut rng, &schema, &mut counter);
        run.apply(index, &op);
        if index % 100 == 99 {
            run.compare_dumps(index, &db);
        }
    }
    run.compare_dumps(ops, &db);
    // A clone made through the gateway is the same database as well.
    let clone = format!("{db}clone");
    let (status, body) = server.post(
        &format!("/v1/databases/{db}/clone"),
        &json!({"destination": clone}),
    );
    assert_eq!(status, 200, "clone {db}: {body}");
    run.compare_dumps(ops, &clone);
}

#[test]
fn gateway_matches_plain_sqlite_on_random_workloads() {
    let ops = env_number("DG_DIFF_OPS", 400) as usize;
    let seeds: Vec<u64> = match std::env::var("DG_DIFF_SEED") {
        Ok(seed) => vec![seed.parse().expect("DG_DIFF_SEED must be a number")],
        Err(_) => {
            let base = env_number("DG_DIFF_BASE", 1);
            (base..base + env_number("DG_DIFF_SEEDS", 4)).collect()
        }
    };
    let server = Server::boot();
    for seed in seeds {
        run_seed(&server, seed, ops);
        eprintln!("differential: seed {seed}: {ops} operations agree");
    }
}

/// The first thing this test found. serde_json's default float parser is
/// fast but not exact: `-2006.7142857142858` arrived as
/// `-2006.7142857142856`, so a REAL written through the API was silently
/// stored one unit in the last place off. The `float_roundtrip` feature
/// (Cargo.toml) makes parsing exact; this pins it with values that the
/// fast parser gets wrong, both as bound arguments and read back.
#[test]
fn reals_round_trip_exactly_through_the_api() {
    let server = Server::boot();
    let (status, body) = server.post("/v1/databases", &json!({"name": "reals"}));
    assert_eq!(status, 200, "{body}");
    let query = |sql: &str, args: Value| {
        let (status, body) = server.post(
            "/v1/databases/reals/query",
            &json!({"sql": sql, "args": args}),
        );
        assert_eq!(status, 200, "{sql}: {body}");
        body
    };
    query("CREATE TABLE r(id INTEGER PRIMARY KEY, x REAL)", json!([]));
    let values = [
        -2006.7142857142858,
        -13574.142857142857,
        0.1 + 0.2,
        1e308,
        5e-324,
        2.2250738585072014e-308,
        9_007_199_254_740_993.0,
        std::f64::consts::E,
    ];
    for (id, x) in values.iter().enumerate() {
        query("INSERT INTO r VALUES (?, ?)", json!([id, x]));
    }
    // Compared inside SQLite too, against the value written as a literal
    // in the SQL text: SQLite parses that exactly, so the comparison does
    // not route through the JSON parser it is checking.
    for (id, x) in values.iter().enumerate() {
        let body = query(
            &format!("SELECT x, x = {x:?} FROM r WHERE id = ?"),
            json!([id]),
        );
        assert_eq!(body["rows"][0][0].as_f64(), Some(*x), "read back {x:?}");
        assert_eq!(body["rows"][0][1], json!(1), "stored exactly {x:?}");
    }
}
