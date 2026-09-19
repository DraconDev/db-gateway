//! Dummy fleet: cart/orders (products / orders / line items).
//!
//! Run: `timeout 300 cargo test --locked --test fleet_shop`
//!
//! A small store driven through the real `dg` binary only: order
//! totals in cents, revenue-by-status and best-seller aggregates,
//! a keyed new-order flow with byte-identical replays (double-submit
//! safety), stock decrement on checkout, cancel flow moving the
//! rollup, FK-violation rejection, and exact finale counts.
//! Zero direct HTTP.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::json;

const DB: &str = "fleet_shop";

fn apply(fleet: &Fleet, sql_file: &str) {
    for stmt in sql_file.split(";\n") {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        fleet.cli.q("seed stmt", DB, stmt);
    }
}

#[test]
fn shop_checks_out_like_a_real_store() {
    let fleet = Fleet::boot();
    assert_eq!(fleet.cli.ok("create", &["create", DB])["name"], DB);
    apply(&fleet, include_str!("../fleet/shop_schema.sql"));
    apply(&fleet, include_str!("../fleet/shop_seed.sql"));

    // Receipt for order 1: 2x1999 + 5x499 = 6493 cents.
    assert_eq!(
        fleet.cli.q(
            "receipt",
            DB,
            "SELECT sum(qty * unit_price_cents) FROM line_items WHERE order_id = 1"
        ),
        json!([[6493]])
    );

    // Revenue by status: cancelled 1999, open 4999, paid 23888.
    assert_eq!(
        fleet.cli.q(
            "revenue",
            DB,
            "SELECT o.status, sum(li.qty * li.unit_price_cents) FROM orders o JOIN line_items li ON li.order_id = o.id GROUP BY o.status ORDER BY o.status"
        ),
        json!([["cancelled", 1999], ["open", 4999], ["paid", 23888]])
    );

    // Best sellers by units moved.
    assert_eq!(
        fleet.cli.q(
            "best sellers",
            DB,
            "SELECT p.name, sum(li.qty) FROM products p JOIN line_items li ON li.product_id = p.id GROUP BY p.name ORDER BY sum(li.qty) DESC, p.name"
        ),
        json!([
            ["Doodad", 5],
            ["Widget", 4],
            ["Whatchamacallit", 3],
            ["Gadget", 1],
            ["Thingamajig", 1]
        ])
    );

    // New order flow, double-submitted: one order row, one line row each.
    let order = fleet.cli.qkey(
        "keyed order",
        DB,
        "INSERT INTO orders(customer, status, placed_at) VALUES ('dan', 'open', '2026-03-05')",
        "fleet-shop-order-1",
    );
    let order_replay = fleet.cli.qkey(
        "keyed order replay",
        DB,
        "INSERT INTO orders(customer, status, placed_at) VALUES ('dan', 'open', '2026-03-05')",
        "fleet-shop-order-1",
    );
    assert_eq!(order, order_replay, "order replay bytes drifted");
    let line = fleet.cli.qkey(
        "keyed line",
        DB,
        "INSERT INTO line_items(order_id, product_id, qty, unit_price_cents) VALUES (5, 2, 2, 4999)",
        "fleet-shop-line-1",
    );
    let line_replay = fleet.cli.qkey(
        "keyed line replay",
        DB,
        "INSERT INTO line_items(order_id, product_id, qty, unit_price_cents) VALUES (5, 2, 2, 4999)",
        "fleet-shop-line-1",
    );
    assert_eq!(line, line_replay, "line replay bytes drifted");
    assert_eq!(
        fleet.cli.q(
            "new receipt",
            DB,
            "SELECT sum(qty * unit_price_cents) FROM line_items WHERE order_id = 5"
        ),
        json!([[9998]])
    );

    // Checkout decrements stock: 20 - 2 = 18 gadgets.
    fleet.cli.q(
        "checkout",
        DB,
        "UPDATE products SET stock = stock - 2 WHERE id = 2",
    );
    assert_eq!(
        fleet.cli.q(
            "gadget stock",
            DB,
            "SELECT stock FROM products WHERE id = 2"
        ),
        json!([[18]])
    );

    // Bob cancels: his open order leaves the open bucket.
    fleet.cli.q(
        "cancel",
        DB,
        "UPDATE orders SET status = 'cancelled' WHERE id = 2",
    );
    assert_eq!(
        fleet.cli.q(
            "revenue after cancel",
            DB,
            "SELECT o.status, sum(li.qty * li.unit_price_cents) FROM orders o JOIN line_items li ON li.order_id = o.id GROUP BY o.status ORDER BY o.status"
        ),
        json!([["cancelled", 6998], ["open", 9998], ["paid", 23888]])
    );

    // Phantom line items are rejected, never stored.
    fleet.cli.err(
        "line bad product",
        &[
            "query",
            DB,
            "INSERT INTO line_items(order_id, product_id, qty, unit_price_cents) VALUES (1, 999, 1, 1)",
        ],
        "invalid_sql",
    );

    // Finale: everything exact.
    assert_eq!(
        fleet.cli.q("products", DB, "SELECT count(*) FROM products"),
        json!([[5]])
    );
    assert_eq!(
        fleet.cli.q("orders", DB, "SELECT count(*) FROM orders"),
        json!([[5]])
    );
    assert_eq!(
        fleet.cli.q("lines", DB, "SELECT count(*) FROM line_items"),
        json!([[8]])
    );
}
