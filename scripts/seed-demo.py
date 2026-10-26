#!/usr/bin/env python3
"""Seed the gateway's databases with realistic demo data.

Usage: seed-demo.py [--server URL] [--key-file PATH] [--dg PATH]

Fills the seven live databases (inventory, orders, ledger, sessions,
notes, billing_dev_pilot, verify_probe) with small readable demo
schemas and rows: the kind of data that makes the TUI, the query log,
and the audit trail worth reading. It is idempotent: tables are
created with IF NOT EXISTS and rows are inserted only into tables
that are still empty, so re-running it changes nothing.

Exit nonzero when any step fails. Prints a one-line JSON summary.
"""

import argparse
import json
import subprocess
import sys

TABLES = {
    "inventory": [
        """CREATE TABLE IF NOT EXISTS products
           (id INTEGER PRIMARY KEY, sku TEXT, name TEXT,
            price_cents INTEGER, stock INTEGER, category TEXT)""",
        """CREATE TABLE IF NOT EXISTS suppliers
           (id INTEGER PRIMARY KEY, name TEXT, country TEXT, lead_days INTEGER)""",
    ],
    "orders": [
        """CREATE TABLE IF NOT EXISTS customers
           (id INTEGER PRIMARY KEY, name TEXT, email TEXT,
            country TEXT, since TEXT)""",
        """CREATE TABLE IF NOT EXISTS orders
           (id INTEGER PRIMARY KEY, customer_id INTEGER, placed_at TEXT,
            total_cents INTEGER, status TEXT)""",
    ],
    "ledger": [
        """CREATE TABLE IF NOT EXISTS accounts
           (id INTEGER PRIMARY KEY, name TEXT, kind TEXT, currency TEXT)""",
        """CREATE TABLE IF NOT EXISTS entries
           (id INTEGER PRIMARY KEY, account_id INTEGER, amount_cents INTEGER,
            memo TEXT, at TEXT)""",
    ],
    "sessions": [
        """CREATE TABLE IF NOT EXISTS sessions
           (id INTEGER PRIMARY KEY, user_name TEXT, started_at TEXT,
            source TEXT)""",
        """CREATE TABLE IF NOT EXISTS events
           (id INTEGER PRIMARY KEY, session_id INTEGER, name TEXT,
            at TEXT, duration_ms INTEGER)""",
    ],
    "notes": [
        """CREATE TABLE IF NOT EXISTS notes
           (id INTEGER PRIMARY KEY, title TEXT, body TEXT, pinned INTEGER)""",
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

ROWS = {
    ("inventory", "products"): """(1,'SKU-1042','canvas tote',2900,34,'bags'),
        (2,'SKU-1043','wool scarf',4500,21,'apparel'),
        (3,'SKU-2041','desk lamp',3900,12,'home'),
        (4,'SKU-2042','stoneware mug',1800,58,'home'),
        (5,'SKU-3055','trail socks',1200,87,'apparel'),
        (6,'SKU-3056','paper notebook',900,43,'stationery'),
        (7,'SKU-4070','brass keyring',1400,19,'accessories'),
        (8,'SKU-4071','leather cardholder',3200,15,'accessories')""",
    ("inventory", "suppliers"): """(1,'Northwind Traders','PT',9),
        (2,'Baltic Linen Co','LV',14),
        (3,'Alpenwerk GmbH','DE',6),
        (4,'Douro Ceramics','PT',11)""",
    ("orders", "customers"): """(1,'Ada Meyer','ada@example.test','DE','2024-03-11'),
        (2,'Jonasz Pelo','jp@example.test','PL','2024-06-02'),
        (3,'Marta Vidal','marta@example.test','ES','2025-01-19'),
        (4,'Tomas Berg','tomas@example.test','SE','2025-04-30'),
        (5,'Ines Rocha','ines@example.test','PT','2025-09-14')""",
    ("orders", "orders"): """(1,1,'2026-10-20T09:14:00Z',7400,'paid'),
        (2,3,'2026-10-21T11:02:00Z',3200,'paid'),
        (3,2,'2026-10-22T15:47:00Z',5800,'open'),
        (4,1,'2026-10-23T08:20:00Z',1400,'paid'),
        (5,4,'2026-10-24T13:55:00Z',9000,'open'),
        (6,5,'2026-10-25T10:11:00Z',4500,'paid')""",
    ("ledger", "accounts"): """(1,'operating','asset','EUR'),
        (2,'supplier payouts','expense','EUR'),
        (3,'shipping','expense','EUR'),
        (4,'sales','revenue','EUR')""",
    ("ledger", "entries"): """(1,4,7400,'order 1 settled','2026-10-20T09:15:00Z'),
        (2,2,-2900,'tote restock','2026-10-21T10:00:00Z'),
        (3,4,3200,'order 2 settled','2026-10-21T11:03:00Z'),
        (4,3,-420,'october parcels','2026-10-22T09:00:00Z'),
        (5,4,1400,'order 4 settled','2026-10-23T08:21:00Z'),
        (6,4,4500,'order 6 settled','2026-10-25T10:12:00Z')""",
    ("sessions", "sessions"): """(1,'ada','2026-10-25T07:02:00Z','web'),
        (2,'marta','2026-10-25T07:40:00Z','cli'),
        (3,'jonasz','2026-10-25T08:15:00Z','web'),
        (4,'tomas','2026-10-26T06:58:00Z','cli')""",
    ("sessions", "events"): """(1,1,'login','2026-10-25T07:02:01Z',210),
        (2,1,'browse','2026-10-25T07:03:44Z',1840),
        (3,1,'checkout','2026-10-25T07:09:12Z',640),
        (4,2,'login','2026-10-25T07:40:00Z',190),
        (5,2,'export','2026-10-25T07:44:31Z',2210),
        (6,3,'login','2026-10-25T08:15:00Z',205),
        (7,4,'login','2026-10-26T06:58:01Z',175),
        (8,4,'status','2026-10-26T06:58:20Z',88)""",
    ("notes", "notes"): """(1,'launch checklist','wrap invoices, confirm shipper',1),
        (2,'supplier visit','Douro kiln tour in november',0),
        (3,'pricing review','tote margin after restock',1),
        (4,'holiday rota','cover dec 24-26 support',0)""",
    ("notes", "tags"): """(1,1,'ops'),(2,1,'launch'),(3,2,'suppliers'),
        (4,3,'pricing'),(5,4,'team'),(6,3,'ops')""",
    ("billing_dev_pilot", "invoices"): """(1,'Ada Meyer',7400,'paid','2026-10-20'),
        (2,'Marta Vidal',3200,'paid','2026-10-21'),
        (3,'Jonasz Pelo',5800,'open','2026-10-22'),
        (4,'Tomas Berg',9000,'open','2026-10-24')""",
    ("billing_dev_pilot", "payments"): """(1,1,7400,'2026-10-20'),
        (2,2,3200,'2026-10-21')""",
    ("verify_probe", "checks"): """(1,'gateway answers','/v1/status',60),
        (2,'backups fresh','/v1/backups',3600),
        (3,'query log alive','/v1/queries',300)""",
    ("verify_probe", "probe_runs"): """(1,1,'2026-10-26T06:00:00Z',1,4),
        (2,2,'2026-10-26T06:00:00Z',1,11),
        (3,3,'2026-10-26T06:00:00Z',1,7),
        (4,1,'2026-10-26T07:00:00Z',1,3)""",
}


def run(dg, server, key_file, *args):
    cmd = [dg, "--server", server, "--key-file", key_file, *args]
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    if result.returncode != 0:
        raise RuntimeError(f"{' '.join(args)} failed: {result.stderr.strip()}")
    return json.loads(result.stdout)


def table_rows(dg, server, key_file, db, table):
    out = run(dg, server, key_file, "query", db, f"SELECT count(*) AS n FROM {table}")
    return out["rows"][0][0]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--server", default="http://127.0.0.1:7432")
    parser.add_argument("--key-file", default=None)
    parser.add_argument("--dg", default="dg")
    args = parser.parse_args()

    summary = {"server": args.server, "databases": {}, "inserted_tables": 0}
    for db, statements in TABLES.items():
        filled = []
        for statement in statements:
            run(args.dg, args.server, args.key_file, "query", db, statement)
        # Table name is the token after CREATE TABLE [IF NOT EXISTS].
        for statement in statements:
            tokens = statement.split()
            name = tokens[tokens.index("TABLE") + 1]
            if name.upper() == "IF":
                name = tokens[tokens.index("TABLE") + 3]
            if table_rows(args.dg, args.server, args.key_file, db, name) == 0:
                key = (db, name)
                run(
                    args.dg,
                    args.server,
                    args.key_file,
                    "query",
                    db,
                    f"INSERT INTO {name} VALUES {ROWS[key]}",
                )
                filled.append(name)
        summary["databases"][db] = {
            "tables": len(statements),
            "filled": filled,
        }
        summary["inserted_tables"] += len(filled)
    print(json.dumps(summary))


if __name__ == "__main__":
    try:
        main()
    except RuntimeError as error:
        print(f"seed-demo: {error}", file=sys.stderr)
        sys.exit(1)
