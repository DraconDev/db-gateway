//! `dg rows`: one table's rows, a page at a time.
//!
//! Run: `timeout 300 cargo test --locked --test cli_rows`
//!
//! The rows browser answers "what is in this table". The contract these
//! tests pin: a pipe prints the gateway's own query document unchanged,
//! a terminal draws a grid of the rows' own values, the footer says
//! which slice of the ANSWER is on screen (the filtered count, not the
//! table's total), and every control that could reach SQL is either
//! validated against the schema or refused.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::Value;

/// A table with 62 rows, so paging has something to page over, and a
/// few rows that a filter can separate.
fn seed(fleet: &Fleet) {
    fleet.cli.ok("create", &["create", "shop"]);
    fleet.cli.q(
        "create products",
        "shop",
        "CREATE TABLE products (id INTEGER PRIMARY KEY, sku TEXT, stock INTEGER)",
    );
    fleet.cli.q(
        "insert",
        "shop",
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<62) \
         INSERT INTO products SELECT i, 'SKU-'||i, i*3 FROM n",
    );
}

/// Run `dg rows` and return the JSON document a pipe reads.
fn rows(fleet: &Fleet, label: &str, args: &[&str]) -> Value {
    let mut full = vec!["rows", "shop", "products"];
    full.extend(args);
    fleet.cli.ok(label, &full)
}

/// The rows of a document, as a list of id values.
fn ids(document: &Value) -> Vec<i64> {
    document["rows"]
        .as_array()
        .expect("the document has rows")
        .iter()
        .map(|row| row[0].as_i64().unwrap_or(0))
        .collect()
}

#[test]
fn a_pipe_reads_the_gateways_own_document() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let document = rows(&fleet, "rows", &[]);
    // The document is the query endpoint's own: columns, rows, and the
    // rows-affected counter the endpoint always carries.
    assert_eq!(
        document["columns"],
        serde_json::json!(["id", "sku", "stock"]),
        "the columns are not the table's: {document}"
    );
    assert_eq!(document["rows_affected"], 0);
    assert_eq!(ids(&document), (1..=10).collect::<Vec<i64>>());
}

#[test]
fn each_page_is_a_different_slice() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let first = ids(&rows(&fleet, "page 0", &[]));
    let second = ids(&rows(&fleet, "page 1", &["1"]));
    let last = ids(&rows(&fleet, "page 6", &["6"]));
    assert_eq!(first, (1..=10).collect::<Vec<i64>>());
    assert_eq!(second, (11..=20).collect::<Vec<i64>>());
    // The last page is a partial one: 62 rows is six full pages and two.
    assert_eq!(last, vec![61, 62]);
}

#[test]
fn a_sort_orders_the_rows_and_narrows_nothing() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let ascending = ids(&rows(&fleet, "asc", &["0", "--sort", "stock"]));
    let descending = ids(&rows(&fleet, "desc", &["0", "--sort", "stock:desc"]));
    // stock is id*3, so ascending ids run 1..10 and descending runs from
    // the largest down. The point is that the ORDER changed.
    assert_eq!(ascending, (1..=10).collect::<Vec<i64>>());
    assert_eq!(descending, (53..=62).rev().collect::<Vec<i64>>());
}

#[test]
fn a_filter_narrows_the_set_on_the_gateway() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let document = rows(&fleet, "filtered", &["0", "--where", "stock < 40"]);
    // stock < 40 is ids 1..13 (stock 3..39). One page of ten comes back.
    assert_eq!(ids(&document), (1..=10).collect::<Vec<i64>>());
    // And the second page of the filtered set continues it, which is what
    // proves the filter was applied before the limit rather than after.
    let second = rows(&fleet, "filtered page 2", &["1", "--where", "stock < 40"]);
    assert_eq!(ids(&second), vec![11, 12, 13]);
}

#[test]
fn a_sort_column_that_is_not_a_column_is_refused() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let (success, stdout, stderr) = fleet
        .cli
        .run(&["rows", "shop", "products", "0", "--sort", "nosuch"]);
    assert!(
        !success,
        "a sort on a missing column was accepted: {stdout}"
    );
    assert!(
        stdout.contains("no column named nosuch") || stderr.contains("no column named nosuch"),
        "the refusal does not name the column: stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn a_filter_that_would_write_is_refused() {
    // The rows browser is read-only, and the refusal is the read-only
    // rule's: a condition may not be a statement.
    let fleet = Fleet::boot();
    seed(&fleet);
    let (success, stdout, stderr) = fleet.cli.run(&[
        "rows",
        "shop",
        "products",
        "0",
        "--where",
        "1=1; DROP TABLE products",
    ]);
    assert!(!success, "a writing filter was accepted: {stdout}");
    assert!(
        stdout.contains("read-only") || stderr.contains("read-only"),
        "the refusal does not say why: stdout={stdout} stderr={stderr}"
    );
    // The table is still there afterwards.
    let info = fleet.cli.ok("info", &["info", "shop"]);
    assert_eq!(
        info["tables"][0]["name"], "products",
        "the table was dropped: {info}"
    );
}

#[test]
fn a_table_that_does_not_exist_is_refused() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let (success, stdout, stderr) = fleet.cli.run(&["rows", "shop", "nosuch"]);
    assert!(!success, "a missing table was read: {stdout}");
    assert!(
        stdout.contains("no table named nosuch") || stderr.contains("no table named nosuch"),
        "the refusal does not name the table: stdout={stdout} stderr={stderr}"
    );
}

/// Strip ANSI escapes so a terminal's colouring does not break a text
/// assertion.
fn plain(screen: &str) -> String {
    let mut out = String::with_capacity(screen.len());
    let mut chars = screen.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
        }
        for c in chars.by_ref() {
            if ('\u{40}'..='\u{7e}').contains(&c) {
                break;
            }
        }
    }
    out
}

/// Draw `dg rows` on a real terminal and read what an operator reads.
fn screen(fleet: &Fleet, args: &str) -> String {
    let command = format!(
        "{} --server {} --key-file {} rows shop products {args}",
        env!("CARGO_BIN_EXE_dg"),
        fleet.cli.server,
        fleet.cli.key_file.to_str().unwrap()
    );
    let output = std::process::Command::new("script")
        .args(["-qec", &command, "/dev/null"])
        .output()
        .unwrap();
    plain(&String::from_utf8_lossy(&output.stdout))
}

#[test]
fn a_terminal_draws_the_rows_and_says_which_slice_it_is() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let text = screen(&fleet, "");
    // The table's own column names, and the rows' own values.
    for expected in ["id", "sku", "stock", "SKU-1", "SKU-10"] {
        assert!(
            text.contains(expected),
            "the drawing does not show {expected:?}: {text}"
        );
    }
    // The footer says which slice of the ANSWER, and offers the keys.
    assert!(
        text.contains("rows 1-10 of 62"),
        "the footer does not say the slice: {text}"
    );
    assert!(
        text.contains("[n] next"),
        "the footer does not offer the next page: {text}"
    );
    // The structure is one key away, and the footer says so.
    assert!(
        text.contains("[s] structure"),
        "the footer does not offer the structure: {text}"
    );
}

#[test]
fn the_footer_counts_the_filtered_set_not_the_table() {
    // This is the number that makes a filtered view trustworthy: the
    // total has to be what the filter selects, or every page looks like
    // a table that shrank.
    let fleet = Fleet::boot();
    seed(&fleet);
    let text = screen(&fleet, "0 --where 'stock < 40'");
    assert!(
        text.contains("of 13"),
        "the footer counts the table, not the filtered set: {text}"
    );
    assert!(
        !text.contains("of 62"),
        "the footer claims the whole table under a filter: {text}"
    );
    assert!(
        text.contains("filter: stock < 40"),
        "the footer does not say what is narrowing the rows: {text}"
    );
}

#[test]
fn the_last_page_has_no_next_page_to_offer() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let text = screen(&fleet, "6");
    assert!(
        text.contains("rows 61-62 of 62"),
        "the last page's slice is wrong: {text}"
    );
    assert!(
        !text.contains("[n] next"),
        "the last page offers a next page that does not exist: {text}"
    );
    assert!(
        text.contains("[p] prev"),
        "the last page does not offer the previous one: {text}"
    );
}
