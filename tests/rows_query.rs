//! The rows browser's statement builder: what may reach SQL.
//!
//! The rows browser turns an operator's choices -- a table, a page, a
//! sort column, a filter -- into a statement. That is the one place in
//! the browser where something a person typed could become SQL, so the
//! rules are tested here rather than through the surfaces that use it.
//!
//! Run: `timeout 300 cargo test --locked --test rows_query`

use dg::views::{ROWS_PAGE, RowsRefusal, SortDirection, rows_query};
use serde_json::{Value, json};

/// The gateway's structure answer for one table with a few columns.
fn structure() -> Value {
    json!({
        "database": "shop",
        "tombstoned": false,
        "created_at": 1_700_000_000,
        "tables": [
            {
                "name": "products",
                "rows": 60,
                "columns": [
                    {"name": "id", "type": "INTEGER", "not_null": false, "primary_key": true, "default": null},
                    {"name": "sku", "type": "TEXT", "not_null": true, "primary_key": false, "default": null},
                    {"name": "stock", "type": "INTEGER", "not_null": false, "primary_key": false, "default": null}
                ],
                "indexes": [], "foreign_keys": [],
                "sql": "CREATE TABLE products (id INTEGER PRIMARY KEY, sku TEXT NOT NULL, stock INTEGER)"
            },
            {
                "name": "orders",
                "rows": 0,
                "columns": [
                    {"name": "id", "type": "INTEGER", "not_null": false, "primary_key": true, "default": null}
                ],
                "indexes": [], "foreign_keys": [],
                "sql": "CREATE TABLE orders (id INTEGER PRIMARY KEY)"
            }
        ]
    })
}

#[test]
fn a_plain_page_reads_the_table_in_its_own_order() {
    let query = rows_query(&structure(), "products", 0, None, None).expect("the table exists");
    assert_eq!(
        query.sql,
        "SELECT * FROM \"products\" ORDER BY rowid LIMIT 10 OFFSET 0"
    );
    // The page size is a constant, so the footer can say what one page
    // holds without asking the gateway.
    assert_eq!(ROWS_PAGE, 10);
}

#[test]
fn each_page_asks_for_its_own_slice() {
    // Page 2 must ask for a different slice, or paging shows page 1
    // twice and the operator concludes the table has 10 rows.
    let first = rows_query(&structure(), "products", 0, None, None).unwrap();
    let second = rows_query(&structure(), "products", 1, None, None).unwrap();
    let third = rows_query(&structure(), "products", 7, None, None).unwrap();
    assert!(first.sql.contains("OFFSET 0"), "{}", first.sql);
    assert!(second.sql.contains("OFFSET 10"), "{}", second.sql);
    assert!(third.sql.contains("OFFSET 70"), "{}", third.sql);
    // A page number whose offset would overflow is refused rather than
    // saturated: a saturated offset reads an arbitrary slice of the
    // table and calls it page N, which is worse than saying no.
    let refused = rows_query(&structure(), "products", u64::MAX, None, None).unwrap_err();
    assert_eq!(refused, RowsRefusal::BadFilter);
}

#[test]
fn a_table_that_does_not_exist_is_refused_before_any_sql() {
    // The name is checked against the schema, not a pattern, so a name
    // that is not a table never reaches the statement at all.
    let refused = rows_query(&structure(), "nosuch", 0, None, None).unwrap_err();
    assert_eq!(refused, RowsRefusal::NoSuchTable("nosuch".to_string()));
    // Including a name that looks like SQL.
    let refused =
        rows_query(&structure(), "products\"; DROP TABLE x; --", 0, None, None).unwrap_err();
    assert_eq!(
        refused,
        RowsRefusal::NoSuchTable("products\"; DROP TABLE x; --".to_string())
    );
}

#[test]
fn a_sort_column_has_to_be_a_column_of_that_table() {
    let ascending = rows_query(
        &structure(),
        "products",
        0,
        Some(("stock", SortDirection::Ascending)),
        None,
    )
    .expect("stock is a column");
    assert!(
        ascending.sql.contains("ORDER BY \"stock\" ASC, rowid"),
        "{}",
        ascending.sql
    );
    let descending = rows_query(
        &structure(),
        "products",
        0,
        Some(("stock", SortDirection::Descending)),
        None,
    )
    .expect("stock is a column");
    assert!(
        descending.sql.contains("ORDER BY \"stock\" DESC, rowid"),
        "{}",
        descending.sql
    );
    // A column of a DIFFERENT table is not a column of this one.
    let refused = rows_query(
        &structure(),
        "orders",
        0,
        Some(("stock", SortDirection::Ascending)),
        None,
    )
    .unwrap_err();
    assert_eq!(refused, RowsRefusal::NoSuchColumn("stock".to_string()));
    // And neither is an expression.
    let refused = rows_query(
        &structure(),
        "products",
        0,
        Some(("stock; DROP TABLE x", SortDirection::Ascending)),
        None,
    )
    .unwrap_err();
    assert_eq!(
        refused,
        RowsRefusal::NoSuchColumn("stock; DROP TABLE x".to_string())
    );
}

#[test]
fn a_sort_is_stable_so_a_page_cannot_repeat_a_row() {
    // Two rows with the same sort value are ordered by rowid as well,
    // so they cannot swap between two queries of the same page.
    let query = rows_query(
        &structure(),
        "products",
        0,
        Some(("sku", SortDirection::Ascending)),
        None,
    )
    .unwrap();
    assert!(
        query.sql.contains(", rowid"),
        "a non-unique sort has no tie-break: {}",
        query.sql
    );
}

#[test]
fn a_filter_narrows_the_statement() {
    let query = rows_query(&structure(), "products", 0, None, Some("stock < 20")).expect("safe");
    assert!(
        query.sql.contains("WHERE (stock < 20)"),
        "the filter is not in the statement: {}",
        query.sql
    );
    // The filter comes before the order and the limit, or it would not
    // narrow the right thing.
    let where_at = query.sql.find("WHERE").unwrap();
    let order_at = query.sql.find("ORDER BY").unwrap();
    assert!(
        where_at < order_at,
        "the clause order is wrong: {}",
        query.sql
    );
    // An empty filter is no filter, not a syntax error.
    let plain = rows_query(&structure(), "products", 0, None, Some("   ")).unwrap();
    assert!(!plain.sql.contains("WHERE"), "{}", plain.sql);
}

#[test]
fn a_filter_that_is_a_write_is_refused() {
    // The whole point of the browser being read-only: a condition is an
    // expression, and anything statement-shaped is refused.
    for bad in [
        "1=1; DROP TABLE products",
        "1=1 -- comment out the rest",
        "1=1 /* comment */",
        "1=1 DELETE FROM products",
        "1=1 UNION SELECT 1",
    ] {
        let refused = rows_query(&structure(), "products", 0, None, Some(bad));
        assert_eq!(
            refused.unwrap_err(),
            RowsRefusal::BadFilter,
            "this filter was accepted: {bad:?}"
        );
    }
}

#[test]
fn an_over_long_filter_is_refused() {
    // A condition is code, and code has a length limit: a pasted
    // document must not become a statement.
    let long = format!("1=1 AND {} < 1", "a".repeat(600));
    let refused = rows_query(&structure(), "products", 0, None, Some(&long)).unwrap_err();
    assert_eq!(refused, RowsRefusal::BadFilter);
}

#[test]
fn a_filter_that_mentions_a_write_as_a_string_is_still_a_read() {
    // The keyword scan skips literals, exactly as the read-only rule
    // does: `sku = 'DELETE'` is a comparison, not a deletion.
    let query = rows_query(&structure(), "products", 0, None, Some("sku = 'DELETE'"))
        .expect("a literal is not a write");
    assert!(query.sql.contains("sku = 'DELETE'"), "{}", query.sql);
}

#[test]
fn every_refusal_says_which_question_was_wrong() {
    // A refusal that says "invalid" tells the operator nothing about
    // what to fix.
    assert!(
        RowsRefusal::NoSuchTable("x".into())
            .message()
            .contains("no table named x")
    );
    assert!(
        RowsRefusal::NoSuchColumn("y".into())
            .message()
            .contains("no column named y")
    );
    assert!(RowsRefusal::BadFilter.message().contains("read-only"));
}
