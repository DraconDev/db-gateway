#!/usr/bin/env python3
"""Seed the gateway's databases with realistic demo data.

Usage: seed-demo.py [--server URL] [--key-file PATH] [--dg PATH]

Fills the seven live databases (inventory, orders, ledger, sessions,
notes, billing_dev_pilot, verify_probe) with small readable demo
schemas and rows: the kind of data that makes the TUI, the query log,
and the audit trail worth reading. It is idempotent: tables are
created with IF NOT EXISTS and rows are inserted only into tables
that are still empty, so re-running it changes nothing.

The generators below are the source of the live demo shape: 60
products / 121 stock moves / 20 suppliers / 80 price-history rows /
4 warehouses / 160 stock positions / 12 category nodes / 20 wide
extract rows, 40 customers / 90 orders / 180 order items / 60
shipments / 15 refunds / 40 order notes, 15 accounts / 15 budgets /
120 entries / 42 fx rates / 14 reconciliations, 70 sessions / 200
events, 50 notes / 89 tags, plus the small billing and probe seeds.
A fresh gateway seeded by this script holds exactly those counts; the
live gateway already does, so re-running it inserts nothing.

The demo schema is deliberately more intricate than a tutorial's: a
composite primary key (`product_prices`, `warehouse_stock`,
`fx_rates`), a self-referencing tree (`category_tree`), declared
foreign keys, index definitions so `dg schema` has something to
show, a NULL-heavy table (`shipments` — most rows have not shipped),
long unicode text (`order_notes`), and one wide, awkward,
denormalized extract (`inventory.legacy_stock_extract`, 14 columns
with long names and mixed NULL rates) that exercises the table and
row panes against a shape they cannot lay out neatly.

Exit nonzero when any step fails. Prints a one-line JSON summary.
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

TABLES = {
    "inventory": [
        """CREATE TABLE IF NOT EXISTS products
           (id INTEGER PRIMARY KEY, sku TEXT, name TEXT,
            price_cents INTEGER, stock INTEGER, category TEXT,
            updated_at TEXT)""",
        """CREATE TABLE IF NOT EXISTS stock_moves
           (id INTEGER PRIMARY KEY, product_id INTEGER, delta INTEGER,
            reason TEXT, at TEXT)""",
        """CREATE TABLE IF NOT EXISTS suppliers
           (id INTEGER PRIMARY KEY, name TEXT, country TEXT, lead_days INTEGER)""",
        # A composite primary key: one price per product per effective date,
        # which is the shape a reader has to reason about rather than scan.
        """CREATE TABLE IF NOT EXISTS product_prices
           (product_id INTEGER NOT NULL REFERENCES products(id),
            valid_from TEXT NOT NULL, price_cents INTEGER NOT NULL,
            currency TEXT NOT NULL DEFAULT 'EUR',
            PRIMARY KEY (product_id, valid_from))""",
        """CREATE TABLE IF NOT EXISTS warehouses
           (id TEXT PRIMARY KEY, city TEXT NOT NULL, region TEXT,
            capacity_pallets INTEGER)""",
        # Two-column key over two foreign keys, plus a nullable reservation.
        """CREATE TABLE IF NOT EXISTS warehouse_stock
           (warehouse_id TEXT NOT NULL REFERENCES warehouses(id),
            product_id INTEGER NOT NULL REFERENCES products(id),
            qty INTEGER NOT NULL, reserved_qty INTEGER,
            PRIMARY KEY (warehouse_id, product_id))""",
        # A self-referencing tree: parent_id points back into this table.
        """CREATE TABLE IF NOT EXISTS category_tree
           (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES category_tree(id),
            name TEXT NOT NULL, depth INTEGER NOT NULL)""",
        # The awkward one: 14 columns, long header names, mixed NULL rates.
        # A denormalized nightly extract is exactly how tables like this are
        # born, and it is the shape most likely to break a pane that assumes
        # it can lay columns out neatly.
        """CREATE TABLE IF NOT EXISTS legacy_stock_extract
           (extract_row_id INTEGER PRIMARY KEY,
            source_system_identifier TEXT NOT NULL,
            sku_normalized_at_extraction_time TEXT,
            product_description_as_stored_in_the_legacy_erp_system TEXT,
            warehouse_code TEXT, quantity_on_hand_units INTEGER,
            quantity_reserved_units INTEGER,
            quantity_available_units INTEGER,
            unit_cost_micros_at_last_revaluation INTEGER,
            revaluation_timestamp_utc TEXT, currency_iso_code TEXT,
            discontinued_flag INTEGER, notes_from_the_merchandising_team TEXT,
            extract_generated_at_utc TEXT NOT NULL)""",
        "CREATE INDEX IF NOT EXISTS products_by_sku ON products(sku)",
        "CREATE INDEX IF NOT EXISTS stock_moves_by_product "
        "ON stock_moves(product_id)",
        "CREATE INDEX IF NOT EXISTS product_prices_by_valid_from "
        "ON product_prices(valid_from)",
    ],
    "orders": [
        """CREATE TABLE IF NOT EXISTS customers
           (id INTEGER PRIMARY KEY, name TEXT, email TEXT,
            country TEXT, since TEXT)""",
        """CREATE TABLE IF NOT EXISTS orders
           (id INTEGER PRIMARY KEY, customer_id INTEGER, placed_at TEXT,
            total_cents INTEGER, status TEXT)""",
        """CREATE TABLE IF NOT EXISTS order_items
           (id INTEGER PRIMARY KEY, order_id INTEGER, product_id INTEGER,
            qty INTEGER, price_cents INTEGER)""",
        # Mostly NULL: only the shipped orders carry a carrier and a tracking
        # number, so a reader that renders NULLs as blanks has to be right.
        """CREATE TABLE IF NOT EXISTS shipments
           (id INTEGER PRIMARY KEY, order_id INTEGER NOT NULL REFERENCES orders(id),
            carrier TEXT, tracking_number TEXT, shipped_at TEXT,
            delivered_at TEXT, status TEXT NOT NULL)""",
        """CREATE TABLE IF NOT EXISTS refunds
           (id INTEGER PRIMARY KEY, order_id INTEGER NOT NULL REFERENCES orders(id),
            amount_cents INTEGER NOT NULL, currency TEXT NOT NULL,
            reason TEXT, refunded_at TEXT NOT NULL)""",
        # Long unicode bodies: wrapping, truncation and column width all fail
        # visibly here rather than silently in a screenshot.
        """CREATE TABLE IF NOT EXISTS order_notes
           (id INTEGER PRIMARY KEY, order_id INTEGER NOT NULL REFERENCES orders(id),
            body TEXT NOT NULL, author TEXT NOT NULL, created_at TEXT NOT NULL)""",
        "CREATE INDEX IF NOT EXISTS orders_by_customer ON orders(customer_id)",
        "CREATE INDEX IF NOT EXISTS order_items_by_order ON order_items(order_id)",
    ],
    "ledger": [
        """CREATE TABLE IF NOT EXISTS accounts
           (id INTEGER PRIMARY KEY, name TEXT, kind TEXT, currency TEXT)""",
        """CREATE TABLE IF NOT EXISTS budgets
           (id INTEGER PRIMARY KEY, account_id INTEGER, month TEXT,
            limit_cents INTEGER)""",
        """CREATE TABLE IF NOT EXISTS entries
           (id INTEGER PRIMARY KEY, account_id INTEGER, amount_cents INTEGER,
            memo TEXT, at TEXT)""",
        # Composite key again, but over a date and a currency rather than ids.
        """CREATE TABLE IF NOT EXISTS fx_rates
           (rate_date TEXT NOT NULL, currency TEXT NOT NULL,
            rate_micros INTEGER NOT NULL,
            PRIMARY KEY (rate_date, currency))""",
        # A nullable close date: an open reconciliation has no closed_at.
        """CREATE TABLE IF NOT EXISTS reconciliations
           (id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL REFERENCES accounts(id),
            period TEXT NOT NULL, expected_cents INTEGER NOT NULL,
            actual_cents INTEGER NOT NULL, closed_at TEXT)""",
        "CREATE INDEX IF NOT EXISTS entries_by_account ON entries(account_id)",
    ],
    "sessions": [
        """CREATE TABLE IF NOT EXISTS sessions
           (id INTEGER PRIMARY KEY, started_at TEXT, ended_at TEXT,
            source TEXT, agent TEXT)""",
        """CREATE TABLE IF NOT EXISTS events
           (id INTEGER PRIMARY KEY, session_id INTEGER, name TEXT,
            at TEXT, duration_ms INTEGER)""",
    ],
    "notes": [
        """CREATE TABLE IF NOT EXISTS notes
           (id INTEGER PRIMARY KEY, title TEXT, body TEXT, pinned INTEGER,
            updated_at TEXT)""",
        """CREATE TABLE IF NOT EXISTS tags
           (id INTEGER PRIMARY KEY, note_id INTEGER, tag TEXT)""",
    ],
    "billing_dev_pilot": [
        """CREATE TABLE IF NOT EXISTS invoices
           (id INTEGER PRIMARY KEY, customer TEXT, amount_cents INTEGER,
            status TEXT, issued_at TEXT)""",
        """CREATE TABLE IF NOT EXISTS payments
           (id INTEGER PRIMARY KEY, invoice_id INTEGER, amount_cents INTEGER,
            paid_at TEXT)""",
    ],
    "verify_probe": [
        """CREATE TABLE IF NOT EXISTS checks
           (id INTEGER PRIMARY KEY, name TEXT, target TEXT,
            interval_secs INTEGER)""",
        """CREATE TABLE IF NOT EXISTS probe_runs
           (id INTEGER PRIMARY KEY, check_id INTEGER, at TEXT,
            ok INTEGER, latency_ms INTEGER)""",
    ],
}


def _day(index):
    # Demo dates cycle through late October so the freshness views have
    # something to chew on without any date ever looking generated.
    return f"2026-10-{20 + (index % 9):02d}"


def gen_products():
    return [
        f"({i},'SKU-{1000 + i}','item {i}',{500 + 37 * i},"
        f"{(i * 29) % 115 + 3},'cat-{i % 7}','{_day(i)}T10:00:00Z')"
        for i in range(1, 61)
    ]


def gen_stock_moves():
    # 121 moves: ids 1..121. (The live gateway also carries one
    # pre-existing manual probe row at id 9999 from an earlier live
    # exercise; a fresh seed holds the 121 generated rows.)
    rows = []
    for i in range(1, 122):
        delta = (i % 7) - 3
        reason = "receipt" if i % 2 == 1 else "issue"
        rows.append(
            f"({i},{(i % 60) + 1},{delta},'{reason}','{_day(i)}T12:00:00Z')"
        )
    return rows


def gen_suppliers():
    return [
        f"({i},'supplier {i}','{'DE' if i % 2 == 1 else 'PL'}',{3 + (i % 20)})"
        for i in range(1, 21)
    ]


def gen_customers():
    return [
        f"({i},'customer {i}','c{i}@example.test',"
        f"'{'DE' if i % 3 == 0 else 'UK'}',"
        f"'2026-{((i - 1) % 12) + 1:02d}-01')"
        for i in range(1, 41)
    ]


def gen_orders():
    statuses = ["paid", "settled", "open"]
    return [
        f"({i},{(i % 40) + 1},'2026-10-{10 + (i % 15):02d}T09:{i % 60:02d}:00Z',"
        f"{1000 + 91 * i},'{statuses[(i - 1) % 3]}')"
        for i in range(1, 91)
    ]


def gen_order_items():
    return [
        f"({i},{(i % 90) + 1},{(i % 60) + 1},{(i % 5) + 1},{500 + 37 * i})"
        for i in range(1, 181)
    ]


def gen_accounts():
    return [
        f"({i},'account {i}','{'asset' if i % 2 == 1 else 'expense'}','EUR')"
        for i in range(1, 16)
    ]


def gen_budgets():
    return [
        f"({i},{(i % 15) + 1},'2026-10',{50000 + 100 * i})" for i in range(1, 16)
    ]


def gen_entries():
    rows = []
    for i in range(1, 121):
        amount = 173 * i
        if i % 2 == 1:
            amount = -amount
        rows.append(
            f"({i},{(i % 15) + 1},{amount},'entry {i}',"
            f"'2026-10-{10 + (i % 15):02d}T08:30:00Z')"
        )
    return rows


def gen_sessions():
    return [
        f"({i},'{_day(i)}T07:00:00Z','{_day(i)}T07:{10 + (i % 40):02d}:00Z',"
        f"'web','agent-{i % 4}')"
        for i in range(1, 71)
    ]


def gen_events():
    return [
        f"({i},{(i % 70) + 1},'event-{i % 9}',"
        f"'{_day(i)}T07:{10 + (i % 40):02d}:00Z',{i})"
        for i in range(1, 201)
    ]


def gen_notes():
    return [
        f"({i},'note {i}','body of note {i}',{1 if i % 5 == 0 else 0},"
        f"'{_day(i)}T15:00:00Z')"
        for i in range(1, 51)
    ]


def gen_tags():
    # 89 tags: ids 1..90 skipping 2. (Id 2 was deleted by an earlier
    # live exercise; the generator keeps the gap so a fresh seed holds
    # the same 89 rows the live gateway does.)
    return [
        f"({i},{(i % 50) + 1},'tag-{i % 11}')"
        for i in range(1, 91)
        if i != 2
    ]


def gen_product_prices():
    # 80 rows: two effective dates for each of the first forty products. The
    # key is (product_id, valid_from), so the ids alone are not unique -- a
    # reader has to show both columns to make the row addresses make sense.
    rows = []
    for i in range(1, 41):
        rows.append(f"({i},'2026-09-01',{500 + 37 * i},'EUR')")
        rows.append(f"({i},'2026-10-15',{520 + 41 * i},'EUR')")
    return rows


def gen_warehouses():
    return [
        "('wh-ber','Berlin','DE',4200)",
        "('wh-ham','Hamburg','DE',2600)",
        "('wh-poz','Poznan','PL',1800)",
        "('wh-kra','Krakow','PL',NULL)",
    ]


def gen_warehouse_stock():
    # 160 positions: four warehouses x the first forty products. Products
    # 41-60 deliberately sit in no warehouse, so a join against this table
    # has gaps to notice. `reserved_qty` is NULL where nothing is reserved.
    warehouses = ["wh-ber", "wh-ham", "wh-poz", "wh-kra"]
    rows = []
    for warehouse in warehouses:
        for product in range(1, 41):
            qty = (product * 3 + len(warehouse)) % 90
            reserved = "NULL" if product % 4 != 0 else str((product * 2) % 20)
            rows.append(f"('{warehouse}',{product},{qty},{reserved})")
    return rows


def gen_category_tree():
    # 12 nodes: three roots (parent_id NULL) and nine children. The parents
    # are always smaller ids than the children, so the tree can be walked
    # either direction without meeting a cycle.
    roots = [(1, "hardware"), (2, "consumables"), (3, "services")]
    rows = [f"({node},NULL,'{name}',0)" for node, name in roots]
    for node in range(4, 13):
        parent = 1 + (node % 3)
        rows.append(f"({node},{parent},'group {node}',1)")
    return rows


def gen_legacy_stock_extract():
    # 20 rows over 14 columns, written the way a nightly ERP extract really
    # is: half the columns are NULL in half the rows, one description is
    # ~120 characters, and the currency column is quietly inconsistent.
    rows = []
    for i in range(1, 21):
        sku = "NULL" if i % 5 == 0 else f"'SKU-{1000 + (i % 60)}'"
        description = (
            "NULL"
            if i % 3 == 0
            else "'Item with a description long enough to overflow any "
            "reasonable column width, kept verbatim from the ERP'"
        )
        warehouse = "NULL" if i % 7 == 0 else "'wh-ber'" if i % 2 else "'wh-poz'"
        on_hand = "NULL" if i % 6 == 0 else str(i * 11)
        reserved = "NULL" if i % 4 == 0 else str(i)
        available = "NULL" if i % 6 == 0 else str(i * 10)
        unit_cost = "NULL" if i % 9 == 0 else str(125_000 + 1_500 * i)
        revalued = f"'2026-10-{14 + (i % 9):02d}T02:00:00Z'"
        currency = "'EUR'" if i % 4 else "'eur'"
        discontinued = "1" if i % 8 == 0 else "0"
        notes = "NULL" if i % 2 else f"'checked by hand, batch {i}'"
        rows.append(
            f"({i},'erp-nightly',{sku},{description},{warehouse},{on_hand},"
            f"{reserved},{available},{unit_cost},{revalued},{currency},"
            f"{discontinued},{notes},'2026-10-28T03:00:00Z')"
        )
    return rows


def gen_shipments():
    # 60 shipments over 90 orders. Only the first twenty have left the
    # warehouse with a carrier and a tracking number; the rest are booked and
    # waiting, which is why four of the six columns are NULL in most rows.
    rows = []
    for i in range(1, 61):
        booked = f"'2026-10-{10 + (i % 15):02d}T11:00:00Z'"
        if i <= 20:
            carrier = f"'carrier-{i % 3}'"
            tracking = f"'TRK{i:06d}'"
            shipped = booked
            delivered = (
                f"'2026-10-{11 + (i % 15):02d}T16:00:00Z'" if i % 2 else "NULL"
            )
            status = "'delivered'" if i % 2 else "'in-transit'"
        else:
            carrier, tracking, shipped, delivered = "NULL", "NULL", "NULL", "NULL"
            status = "'booked'"
        rows.append(
            f"({i},{(i % 90) + 1},{carrier},{tracking},{shipped},{delivered},{status})"
        )
    return rows


def gen_refunds():
    # 15 partial refunds in mixed currencies: the amounts are cents in the
    # refund's own currency, not the order's, which is the kind of thing a
    # total-by-currency query has to account for.
    rows = []
    for i in range(1, 16):
        currency = "'GBP'" if i % 3 == 0 else "'EUR'"
        reason = "'damaged'" if i % 2 else "'wrong item'"
        rows.append(
            f"({i},{(i % 90) + 1},{250 + 80 * i},{currency},{reason},"
            f"'2026-10-{18 + (i % 10):02d}T12:00:00Z')"
        )
    return rows


def gen_order_notes():
    # 40 notes with long unicode bodies: wrapping, truncation and column
    # widths all fail visibly here rather than silently in a screenshot.
    bodies = [
        "customer asked for delivery to the side door — gate code 4417, "
        "ring the bell twice",
        "Zahlung per Überweisung eingegangen; Rechnung wurde storniert, "
        "kunde möchte die Bestellung dennoch behalten",
        "アドレスが古いため再確認が必要 — 連絡済み（10月21日）",
        "split shipment approved: the fragile items go separately so nothing "
        "arrives broken this time",
    ]
    rows = []
    for i in range(1, 41):
        body = bodies[i % len(bodies)].replace("'", "''")
        rows.append(
            f"({i},{(i % 90) + 1},'{body}','agent-{i % 4}',"
            f"'2026-10-{12 + (i % 16):02d}T09:30:00Z')"
        )
    return rows


def gen_fx_rates():
    # 42 rates: fourteen business days x three currencies. The key is the
    # date and the currency together -- no single column identifies a row.
    currencies = [("EUR", 1_000_000), ("GBP", 1_168_000), ("USD", 917_000)]
    rows = []
    for day in range(14):
        date = f"2026-10-{10 + day:02d}"
        for currency, base in currencies:
            # A stable per-day wobble so the series is not a flat line.
            rate = base + (day * 1_700) - (day % 3) * 2_100
            rows.append(f"('{date}','{currency}',{rate})")
    return rows


def gen_reconciliations():
    # 14 reconciliations, the last four still open (closed_at NULL).
    rows = []
    for i in range(1, 15):
        expected = 50_000 + 1_000 * i
        actual = expected if i % 5 else expected - 250
        closed = (
            f"'2026-10-{20 + (i % 8):02d}T18:00:00Z'" if i <= 10 else "NULL"
        )
        rows.append(f"({i},{(i % 15) + 1},'2026-10',{expected},{actual},{closed})")
    return rows


GENERATORS = {
    ("inventory", "products"): gen_products,
    ("inventory", "stock_moves"): gen_stock_moves,
    ("inventory", "suppliers"): gen_suppliers,
    ("inventory", "product_prices"): gen_product_prices,
    ("inventory", "warehouses"): gen_warehouses,
    ("inventory", "warehouse_stock"): gen_warehouse_stock,
    ("inventory", "category_tree"): gen_category_tree,
    ("inventory", "legacy_stock_extract"): gen_legacy_stock_extract,
    ("orders", "customers"): gen_customers,
    ("orders", "orders"): gen_orders,
    ("orders", "order_items"): gen_order_items,
    ("orders", "shipments"): gen_shipments,
    ("orders", "refunds"): gen_refunds,
    ("orders", "order_notes"): gen_order_notes,
    ("ledger", "accounts"): gen_accounts,
    ("ledger", "budgets"): gen_budgets,
    ("ledger", "entries"): gen_entries,
    ("ledger", "fx_rates"): gen_fx_rates,
    ("ledger", "reconciliations"): gen_reconciliations,
    ("sessions", "sessions"): gen_sessions,
    ("sessions", "events"): gen_events,
    ("notes", "notes"): gen_notes,
    ("notes", "tags"): gen_tags,
    ("billing_dev_pilot", "invoices"): lambda: [
        "(1,'Ada Meyer',7400,'paid','2026-10-20')",
        "(2,'Marta Vidal',3200,'paid','2026-10-21')",
        "(3,'Jonasz Pelo',5800,'open','2026-10-22')",
        "(4,'Tomas Berg',9000,'open','2026-10-24')",
    ],
    ("billing_dev_pilot", "payments"): lambda: [
        "(1,1,7400,'2026-10-20')",
        "(2,2,3200,'2026-10-21')",
    ],
    ("verify_probe", "checks"): lambda: [
        "(1,'gateway answers','/v1/status',60)",
        "(2,'backups fresh','/v1/backups',3600)",
        "(3,'query log alive','/v1/queries',300)",
    ],
    ("verify_probe", "probe_runs"): lambda: [
        "(1,1,'2026-10-26T06:00:00Z',1,4)",
        "(2,2,'2026-10-26T06:00:00Z',1,11)",
        "(3,3,'2026-10-26T06:00:00Z',1,7)",
        "(4,1,'2026-10-26T07:00:00Z',1,3)",
    ],
}

# How many rows one INSERT carries. The gateway bounds a single
# statement, so a 200-row insert goes in slices rather than one giant
# string that risks tripping the bound on a slower box.
INSERT_CHUNK = 50


def ensure_db(dg, server, key_file, db):
    # A fresh gateway holds no databases; the live one already holds
    # all seven. Creating unconditionally and ignoring the
    # already-exists refusal keeps both paths identical.
    cmd = [dg, "--server", server]
    if key_file:
        cmd += ["--key-file", key_file]
    cmd += ["create", db]
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    if result.returncode != 0:
        # An existing database refuses with a database_exists document
        # on stdout; anything else is a real failure.
        if "database_exists" in result.stdout:
            return
        raise RuntimeError(f"create {db} failed: {result.stderr.strip()}")


def run(dg, server, key_file, *args):
    cmd = [dg, "--server", server]
    # The CLI resolves the key itself (DG_KEY_FILE, then the config
    # file's default context, then ~/.config/dg/key) when the flag is
    # absent, so an unset --key-file means "omit the flag", not "pass
    # None". Passing the literal None used to crash with a TypeError
    # before a single row was seeded.
    if key_file:
        cmd += ["--key-file", key_file]
    cmd += list(args)
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    if result.returncode != 0:
        raise RuntimeError(f"{' '.join(args)} failed: {result.stderr.strip()}")
    return json.loads(result.stdout)


def table_rows(dg, server, key_file, db, table):
    out = run(dg, server, key_file, "query", db, f"SELECT count(*) AS n FROM {table}")
    return out["rows"][0][0]


def resolve_dg(explicit):
    # A bare `dg` on PATH may be an older release (the box still has
    # 0.8.0 installed) while the repo just built 0.9.0. Prefer the
    # repo's own debug binary when it exists so a bare re-run seeds
    # with a version-matched client.
    if explicit:
        return explicit
    here = Path(__file__).resolve().parent
    candidate = here.parent / ".cargo" / "target" / "debug" / "dg"
    if candidate.is_file():
        return str(candidate)
    return "dg"


def table_name(statement):
    """The table a CREATE TABLE statement defines, or None for anything else.

    The demo now mixes table definitions with index definitions, so the
    caller cannot assume every statement is a table: an index has no rows to
    count and no generator, and treating one as a table would insert into a
    name that does not exist.
    """
    tokens = statement.split()
    if len(tokens) < 2 or tokens[0].upper() != "CREATE" or tokens[1].upper() != "TABLE":
        return None
    at = 2
    if tokens[at].upper() == "IF":
        at += 3  # skip IF NOT EXISTS
    return tokens[at]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--server", default="http://127.0.0.1:7432")
    parser.add_argument("--key-file", default=None)
    parser.add_argument("--dg", default=None)
    args = parser.parse_args()
    dg = resolve_dg(args.dg)

    summary = {
        "server": args.server,
        "databases": {},
        "tables": 0,
        "indexes": 0,
        "inserted_tables": 0,
    }
    for db in TABLES:
        ensure_db(dg, args.server, args.key_file, db)
    for db, statements in TABLES.items():
        filled = []
        for statement in statements:
            run(dg, args.server, args.key_file, "query", db, statement)
        tables = []
        indexes = 0
        for statement in statements:
            name = table_name(statement)
            if name is None:
                indexes += 1
                continue
            tables.append(name)
            if table_rows(dg, args.server, args.key_file, db, name) == 0:
                values = GENERATORS[(db, name)]()
                for start in range(0, len(values), INSERT_CHUNK):
                    chunk = ",".join(values[start : start + INSERT_CHUNK])
                    run(
                        dg,
                        args.server,
                        args.key_file,
                        "query",
                        db,
                        f"INSERT INTO {name} VALUES {chunk}",
                    )
                filled.append(name)
        summary["databases"][db] = {
            "tables": len(tables),
            "indexes": indexes,
            "filled": filled,
        }
        summary["tables"] += len(tables)
        summary["indexes"] += indexes
        summary["inserted_tables"] += len(filled)
    print(json.dumps(summary))


if __name__ == "__main__":
    try:
        main()
    except RuntimeError as error:
        print(f"seed-demo: {error}", file=sys.stderr)
        sys.exit(1)
