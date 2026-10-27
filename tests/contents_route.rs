//! `GET /v1/contents`: how much data each database holds.
//!
//! The databases tab shows traffic counters -- rows read, rows written,
//! waits, last write -- and every one of them is in memory, reset when
//! the gateway restarts. So a gateway that has just restarted shows
//! seven databases with nothing beside them, and seven databases full
//! of data look exactly like seven empty ones. That is the answer this
//! route exists to give instead: how many tables and how many rows,
//! read from the files.
//!
//! Run: `timeout 300 cargo test --locked --test contents_route`

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::Value;

/// A database with real data in it: two tables, 63 rows between them.
fn seed(fleet: &Fleet) {
    fleet.cli.ok("create", &["create", "shop"]);
    fleet.cli.q(
        "create products",
        "shop",
        "CREATE TABLE products (id INTEGER PRIMARY KEY, sku TEXT)",
    );
    fleet.cli.q(
        "create orders",
        "shop",
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, note TEXT)",
    );
    fleet.cli.q(
        "insert products",
        "shop",
        "INSERT INTO products VALUES (1,'a'),(2,'b'),(3,'c')",
    );
    fleet.cli.q(
        "insert orders",
        "shop",
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<60) \
         INSERT INTO orders SELECT i, 'note' FROM n",
    );
}

fn contents(fleet: &Fleet) -> Value {
    fleet.cli.ok("contents", &["contents", "--json"])
}

#[test]
fn a_database_with_data_reports_its_tables_and_its_rows() {
    // The whole point: this database is full, and the answer says so.
    // A traffic counter says zero here, because no query has run since
    // the gateway booted; the files say 2 tables and 63 rows.
    let fleet = Fleet::boot();
    seed(&fleet);
    let document = contents(&fleet);
    let shop = &document["databases"]["shop"];
    assert_eq!(shop["tables"], 2, "the table count is wrong: {document}");
    assert_eq!(shop["rows"], 63, "the row count is wrong: {document}");
}

#[test]
fn every_live_database_is_listed() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create one", &["create", "one"]);
    fleet.cli.ok("create two", &["create", "two"]);
    fleet
        .cli
        .q("create t", "one", "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    let document = contents(&fleet);
    let mut names: Vec<&str> = document["databases"]
        .as_object()
        .expect("the answer has databases")
        .keys()
        .map(String::as_str)
        .collect();
    names.sort();
    assert_eq!(names, vec!["one", "two"]);
    // A database with no tables is zero, and zero is a real answer:
    // it holds nothing, and that is not a fault.
    assert_eq!(document["databases"]["two"]["tables"], 0);
    assert_eq!(document["databases"]["two"]["rows"], 0);
    assert_eq!(document["databases"]["one"]["tables"], 1);
}

#[test]
fn the_gateways_own_tables_are_not_counted_as_the_operators_data() {
    // A replicated database carries the replication's bookkeeping, and
    // the gateway carries its own. Neither is the operator's data, so
    // neither is counted in what the operator is told they hold.
    let fleet = Fleet::boot();
    seed(&fleet);
    fleet.cli.q(
        "replication bookkeeping",
        "shop",
        "CREATE TABLE _litestream_seq (id INTEGER PRIMARY KEY, seq INTEGER)",
    );
    let document = contents(&fleet);
    let shop = &document["databases"]["shop"];
    assert_eq!(
        shop["tables"], 2,
        "a replication table was counted as the operator's: {document}"
    );
    assert_eq!(
        shop["rows"], 63,
        "a replication table's rows were counted: {document}"
    );
}

#[test]
fn a_database_named_contents_still_works() {
    // `contents` is a legal database name. The route that reports
    // contents lives outside `/v1/databases/` precisely so that a
    // database called `contents` keeps its own info and delete routes:
    // a route under that prefix would be matched first, and the
    // database would become unreachable.
    let fleet = Fleet::boot();
    fleet.cli.ok("create contents", &["create", "contents"]);
    fleet.cli.q(
        "create t",
        "contents",
        "CREATE TABLE t (id INTEGER PRIMARY KEY)",
    );
    let info = fleet.cli.ok("info contents", &["info", "contents"]);
    assert_eq!(
        info["name"], "contents",
        "a database named contents lost its info route: {info}"
    );
    let document = contents(&fleet);
    assert_eq!(
        document["databases"]["contents"]["tables"], 1,
        "a database named contents is not in the contents answer: {document}"
    );
}

#[test]
fn only_the_owner_may_ask() {
    // The answer is a view of every database in the service, which is
    // operator information: a scoped application key may not read it.
    // Asked over HTTP directly, because the CLI harness pins the owner
    // key file and this test needs a different credential.
    let fleet = Fleet::boot();
    seed(&fleet);
    let scoped = fleet.cli.ok(
        "scoped key",
        &[
            "key",
            "create",
            "reader",
            "--role",
            "database_read_only",
            "--database",
            "shop",
        ],
    );
    let key = scoped["key"].as_str().unwrap().to_string();
    let response = reqwest::blocking::Client::new()
        .get(format!("{}/v1/contents", fleet.cli.server))
        .bearer_auth(&key)
        .send()
        .expect("the route answers");
    assert_eq!(
        response.status().as_u16(),
        403,
        "a scoped key read every database's contents"
    );
    let body: Value = response.json().expect("the refusal is JSON");
    assert_eq!(
        body["error"]["code"], "forbidden",
        "the refusal does not name the reason: {body}"
    );
}

/// The databases tab shows what each database HOLDS, and says which of
/// its columns are since-restart counters.
///
/// This is the whole point of the work: seven full databases used to
/// read as seven empty ones, because every column on the tab was an
/// in-memory counter that a restart zeroes.
#[test]
fn the_databases_tab_shows_contents_and_labels_its_counters() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let document = contents(&fleet);
    let list = fleet.cli.ok("list", &["list", "--json"]);
    let status = fleet.cli.ok("status", &["status", "--json"]);
    let table = dg::views::databases(&list, &status, &document);
    let mut out = Vec::new();
    table
        .write(&mut out, &dg::render::Theme::fixed(false, true))
        .expect("the table writes");
    let text = String::from_utf8(out).expect("the table is text");
    // What the database holds, from its file.
    let headings = text.lines().next().unwrap_or("");
    for column in ["tables", "rows"] {
        assert!(
            headings.contains(column),
            "the tab has no {column} column: {headings}"
        );
    }
    // The counts are real: this database has 2 tables and 63 rows.
    let shop = text
        .lines()
        .find(|line| line.contains("shop") && !line.contains("database"))
        .unwrap_or_else(|| panic!("the shop row is missing:\n{text}"));
    assert!(shop.contains("63"), "the row count is not drawn: {shop}");
    // Every counter that a restart zeroes is marked, and the mark is
    // explained: a star nobody explains is a worse hint than none.
    assert!(
        headings.contains("rows read*"),
        "the since-restart counters are not marked: {headings}"
    );
    assert!(
        text.contains("since the gateway"),
        "the mark is not explained:\n{text}"
    );
}

/// The status dashboard keeps its own columns, unstarred.
///
/// The traffic table is shared, but the star is the databases tab's:
/// the dashboard is the traffic view and carries no contents, so its
/// footnote would have nothing to explain.
#[test]
fn the_status_dashboard_keeps_its_unstarred_columns() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let status = fleet.cli.ok("status", &["status", "--json"]);
    let table = dg::views::per_database_table(&status);
    let mut out = Vec::new();
    table
        .write(&mut out, &dg::render::Theme::fixed(false, true))
        .expect("the table writes");
    let text = String::from_utf8(out).expect("the table is text");
    let headings = text.lines().next().unwrap_or("");
    assert!(
        headings.contains("rows read"),
        "the dashboard lost its read column: {headings}"
    );
    assert!(
        !headings.contains("rows read*"),
        "the dashboard took a mark it does not explain: {headings}"
    );
}
