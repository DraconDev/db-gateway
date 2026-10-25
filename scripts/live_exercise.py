#!/usr/bin/env python3
"""Exercise a DEPLOYED gateway: give it real work, then ask it real questions.

Usage: live_exercise.py [--server URL] [--key-file PATH] [--prefix P]
                        [--cleanup] [--out FILE]

The load harnesses in this repo (soak.py, soak_starvation.py,
noisy_neighbor_cross_db.py) start their own `dg serve` on a temp
directory and never touch a deployment. That is deliberate: a load test
on the box that holds the data is a load test on the data. But it also
means those harnesses say nothing about whether a DEPLOYED gateway --
the one an operator actually reads -- still answers, and whether the
surfaces it shows (the query log, the audit trail, the backup
freshness, the agent surface) have anything real in them.

This script is the other half. It talks to a running gateway through
the real CLI, so the paths an operator types are the paths that get
exercised:

  * it creates databases with real schemas and real rows,
  * it runs a spread of statement shapes, so the query log holds a
    spread rather than one shape used once,
  * it issues scoped credentials and uses them, so the audit trail
    holds more than one principal,
  * it walks every read-only surface an operator has,
  * it checks the refusals as carefully as the successes -- a gateway
    that answers everything is not a gateway, it is a hole,
  * it drives the MCP surface the way an agent would.

It never prints a credential: the scoped keys are written to a
0600 temp file, used, and deleted with the rest of the temp dir.

The databases it creates are named so an operator can recognise them
and delete them; --cleanup does that for you. Run it with --prefix to
work beside an existing set instead of on the default names.

Exit nonzero when any check fails. Prints a one-line JSON summary.
"""

import argparse
import atexit
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

# The five databases, their schemas and their rows. The names are
# ordinary enough to read in a query log and distinct enough to
# recognise and delete.
NAMES = ["inventory", "orders", "ledger", "sessions", "notes"]

SCHEMA = {
    "inventory": [
        "CREATE TABLE products (id INTEGER PRIMARY KEY, sku TEXT, name TEXT, price_cents INTEGER, stock INTEGER, category TEXT, updated_at TEXT)",
        "CREATE TABLE suppliers (id INTEGER PRIMARY KEY, name TEXT, country TEXT, lead_days INTEGER)",
        "CREATE TABLE stock_moves (id INTEGER PRIMARY KEY, product_id INTEGER, delta INTEGER, reason TEXT, at TEXT)",
    ],
    "orders": [
        "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT, email TEXT, country TEXT, since TEXT)",
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER, placed_at TEXT, total_cents INTEGER, status TEXT)",
        "CREATE TABLE order_items (id INTEGER PRIMARY KEY, order_id INTEGER, product_id INTEGER, qty INTEGER, price_cents INTEGER)",
    ],
    "ledger": [
        "CREATE TABLE accounts (id INTEGER PRIMARY KEY, name TEXT, kind TEXT, currency TEXT)",
        "CREATE TABLE entries (id INTEGER PRIMARY KEY, account_id INTEGER, amount_cents INTEGER, memo TEXT, at TEXT)",
        "CREATE TABLE budgets (id INTEGER PRIMARY KEY, account_id INTEGER, month TEXT, limit_cents INTEGER)",
    ],
    "sessions": [
        "CREATE TABLE sessions (id INTEGER PRIMARY KEY, started_at TEXT, ended_at TEXT, source TEXT, agent TEXT)",
        "CREATE TABLE events (id INTEGER PRIMARY KEY, session_id INTEGER, name TEXT, at TEXT, duration_ms INTEGER)",
    ],
    "notes": [
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT, body TEXT, pinned INTEGER, updated_at TEXT)",
        "CREATE TABLE tags (id INTEGER PRIMARY KEY, note_id INTEGER, tag TEXT)",
    ],
}

ROWS = {
    "inventory": [
        "INSERT INTO products (id, sku, name, price_cents, stock, category, updated_at) VALUES"
        + ",".join(
            f"({i},'SKU-{1000 + i}','item {i}',{500 + i * 37},{i * 3 % 97},'cat-{i % 7}','2026-10-2{i % 9}T10:00:00Z')"
            for i in range(1, 61)
        ),
        "INSERT INTO suppliers (id, name, country, lead_days) VALUES"
        + ",".join(
            f"({i},'supplier {i}','{'DE' if i % 2 else 'PL'}',{3 + i % 20})" for i in range(1, 21)
        ),
        "INSERT INTO stock_moves (id, product_id, delta, reason, at) VALUES"
        + ",".join(
            f"({i},{i % 60 + 1},{(i % 7) - 3},'{'receipt' if i % 2 else 'issue'}','2026-10-2{i % 9}T12:00:00Z')"
            for i in range(1, 121)
        ),
    ],
    "orders": [
        "INSERT INTO customers (id, name, email, country, since) VALUES"
        + ",".join(
            f"({i},'customer {i}','c{i}@example.test','{'UK' if i % 3 else 'DE'}','2026-0{1 + i % 9}-01')"
            for i in range(1, 41)
        ),
        "INSERT INTO orders (id, customer_id, placed_at, total_cents, status) VALUES"
        + ",".join(
            f"({i},{i % 40 + 1},'2026-10-{10 + i % 15}T09:{i % 60:02d}:00Z',{1000 + i * 91},'{'paid' if i % 3 else 'open'}')"
            for i in range(1, 91)
        ),
        "INSERT INTO order_items (id, order_id, product_id, qty, price_cents) VALUES"
        + ",".join(
            f"({i},{i % 90 + 1},{i % 60 + 1},{1 + i % 5},{500 + i * 37})" for i in range(1, 181)
        ),
    ],
    "ledger": [
        "INSERT INTO accounts (id, name, kind, currency) VALUES"
        + ",".join(
            f"({i},'account {i}','{'asset' if i % 2 else 'expense'}','EUR')" for i in range(1, 16)
        ),
        "INSERT INTO entries (id, account_id, amount_cents, memo, at) VALUES"
        + ",".join(
            f"({i},{i % 15 + 1},{-(i * 173) if i % 2 else i * 173},'entry {i}','2026-10-{10 + i % 15}T08:30:00Z')"
            for i in range(1, 121)
        ),
        "INSERT INTO budgets (id, account_id, month, limit_cents) VALUES"
        + ",".join(
            f"({i},{i % 15 + 1},'2026-10',{50000 + i * 100})" for i in range(1, 16)
        ),
    ],
    "sessions": [
        "INSERT INTO sessions (id, started_at, ended_at, source, agent) VALUES"
        + ",".join(
            f"({i},'2026-10-2{i % 9}T07:00:00Z','2026-10-2{i % 9}T07:{10 + i % 40:02d}:00Z','web','agent-{i % 4}')"
            for i in range(1, 71)
        ),
        "INSERT INTO events (id, session_id, name, at, duration_ms) VALUES"
        + ",".join(
            f"({i},{i % 70 + 1},'event-{i % 9}','2026-10-2{i % 9}T07:{10 + i % 40:02d}:00Z',{i % 900})"
            for i in range(1, 201)
        ),
    ],
    "notes": [
        "INSERT INTO notes (id, title, body, pinned, updated_at) VALUES"
        + ",".join(
            f"({i},'note {i}','body of note {i}',{1 if i % 5 == 0 else 0},'2026-10-2{i % 9}T15:00:00Z')"
            for i in range(1, 51)
        ),
        "INSERT INTO tags (id, note_id, tag) VALUES"
        + ",".join(f"({i},{i % 50 + 1},'tag-{i % 11}')" for i in range(1, 91)),
    ],
}

# The spread of shapes the query log should end up holding. These are
# ordinary questions about ordinary data: point reads, filtered reads,
# aggregates, joins, and the shape the gateway sees most often.
QUERIES = [
    ("inventory", "SELECT id, sku, name FROM products WHERE id = ?", [7]),
    ("inventory", "SELECT id, name, price_cents FROM products WHERE category = ? ORDER BY price_cents LIMIT 20", ["cat-3"]),
    ("inventory", "SELECT count(*) FROM products", []),
    ("inventory", "SELECT category, count(*), sum(stock) FROM products GROUP BY category", []),
    ("inventory", "SELECT p.name, s.name, s.lead_days FROM products p JOIN suppliers s ON s.id = p.id % 20 + 1 WHERE p.stock < ?", [50]),
    ("inventory", "SELECT id, delta, reason FROM stock_moves WHERE product_id = ? ORDER BY at DESC LIMIT 10", [12]),
    ("inventory", "SELECT * FROM stock_moves WHERE delta < 0", []),
    ("inventory", "SELECT max(id) FROM stock_moves", []),
    ("orders", "SELECT id, total_cents, status FROM orders WHERE id = ?", [42]),
    ("orders", "SELECT status, count(*), sum(total_cents) FROM orders GROUP BY status", []),
    ("orders", "SELECT c.name, count(o.id) FROM customers c JOIN orders o ON o.customer_id = c.id GROUP BY c.name ORDER BY count(o.id) DESC LIMIT 10", []),
    ("orders", "SELECT o.id, o.total_cents, sum(i.qty) FROM orders o JOIN order_items i ON i.order_id = o.id GROUP BY o.id ORDER BY o.total_cents DESC LIMIT 10", []),
    ("orders", "SELECT id, qty FROM order_items WHERE order_id = ?", [13]),
    ("orders", "SELECT count(*) FROM order_items WHERE price_cents > ?", [2000]),
    ("orders", "SELECT * FROM orders WHERE placed_at > ? ORDER BY placed_at DESC LIMIT 5", ["2026-10-20"]),
    ("ledger", "SELECT id, amount_cents, memo FROM entries WHERE id = ?", [55]),
    ("ledger", "SELECT kind, sum(amount_cents) FROM entries e JOIN accounts a ON a.id = e.account_id GROUP BY kind", []),
    ("ledger", "SELECT account_id, sum(amount_cents) FROM entries GROUP BY account_id HAVING sum(amount_cents) < 0", []),
    ("ledger", "SELECT a.name, b.limit_cents FROM budgets b JOIN accounts a ON a.id = b.account_id WHERE b.month = ?", ["2026-10"]),
    ("ledger", "SELECT count(*) FROM entries WHERE at BETWEEN ? AND ?", ["2026-10-10", "2026-10-20"]),
    ("sessions", "SELECT id, source, agent, ended_at FROM sessions WHERE id = ?", [33]),
    ("sessions", "SELECT source, count(*), avg(duration_ms) FROM events e JOIN sessions s ON s.id = e.session_id GROUP BY source", []),
    ("sessions", "SELECT name, count(*) FROM events GROUP BY name ORDER BY count(*) DESC", []),
    ("sessions", "SELECT id, duration_ms FROM events WHERE duration_ms > ? ORDER BY duration_ms DESC LIMIT 10", [700]),
    ("sessions", "SELECT count(*) FROM events WHERE name = ?", ["event-3"]),
    ("notes", "SELECT id, title FROM notes WHERE pinned = ? ORDER BY updated_at DESC", [1]),
    ("notes", "SELECT tag, count(*) FROM tags GROUP BY tag ORDER BY count(*) DESC LIMIT 10", []),
    ("notes", "SELECT n.title, t.tag FROM notes n JOIN tags t ON t.note_id = n.id WHERE n.id = ?", [9]),
    ("notes", "SELECT count(*) FROM notes WHERE updated_at > ?", ["2026-10-15"]),
]

MUTATIONS = [
    ("inventory", "UPDATE products SET stock = stock + 5 WHERE category = ?", ["cat-2"]),
    ("inventory", "INSERT INTO stock_moves (id, product_id, delta, reason, at) VALUES (9999, 1, 7, 'adjustment', '2026-10-25T12:00:00Z')", []),
    ("ledger", "UPDATE entries SET memo = ? WHERE id = ?", ["reviewed", 3]),
    ("notes", "DELETE FROM tags WHERE id = ?", [2]),
]

# The credentials the exercise issues, and what each may do.
CREDENTIALS = [
    ("warehouse", "database_read_only", "inventory"),
    ("checkout", "database_read_write", "orders"),
    ("finance", "database_read_only", "ledger"),
]


class Report:
    """What happened, and whether it was supposed to."""

    def __init__(self):
        self.checks = []

    def add(self, name, good, detail=""):
        self.checks.append({"name": name, "ok": bool(good), "detail": str(detail)[:400]})
        print(f"[{'PASS' if good else 'FAIL'}] {name} {str(detail)[:160]}")
        return good

    @property
    def failures(self):
        return [check for check in self.checks if not check["ok"]]


class Client:
    """The real CLI, against a real gateway."""

    def __init__(self, dg, server, key_file):
        self.dg = dg
        self.server = server
        self.key_file = key_file

    def run(self, args, key_file=None, timeout=180):
        base = [
            self.dg,
            "--server",
            self.server,
            "--key-file",
            key_file or self.key_file,
        ]
        try:
            result = subprocess.run(
                base + args, capture_output=True, text=True, timeout=timeout
            )
        except subprocess.TimeoutExpired:
            return 124, "", f"TIMEOUT after {timeout}s"
        return result.returncode, result.stdout, result.stderr

    def json(self, args, key_file=None, timeout=180):
        code, out, err = self.run(args, key_file, timeout)
        try:
            return code, json.loads(out)
        except (json.JSONDecodeError, TypeError):
            return code, None


def read_key(path):
    """The credential itself, read from the file the operator pointed at."""
    try:
        return Path(path).read_text(encoding="utf-8").strip()
    except OSError:
        return ""


def exercise_mcp(client, report, key_file):
    """Drive the MCP surface the way an agent would."""
    process = subprocess.Popen(
        [client.dg, "--server", client.server, "--key-file", key_file, "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )

    def send(message):
        process.stdin.write(json.dumps(message) + "\n")
        process.stdin.flush()

    def recv():
        line = process.stdout.readline()
        if not line:
            return None
        try:
            return json.loads(line)
        except json.JSONDecodeError:
            return {"raw": line.strip()}

    try:
        send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
              "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                         "clientInfo": {"name": "live-exercise", "version": "1"}}})
        init = recv()
        report.add("mcp initialize", bool(init))
        send({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})

        send({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
        listing = recv()
        tools = listing.get("result", {}).get("tools", []) if listing else []
        names = [tool["name"] for tool in tools]
        report.add(
            "mcp advertises six read-only tools",
            len(names) == 6,
            ", ".join(names),
        )
        report.add(
            "mcp refuses arguments the schema does not declare",
            all(tool.get("inputSchema", {}).get("additionalProperties") is False
                for tool in tools),
        )

        calls = [
            ("mcp dg_status", "dg_status", {}),
            ("mcp dg_list", "dg_list", {}),
            ("mcp dg_info", "dg_info", {"name": "inventory"}),
            ("mcp dg_inspect", "dg_inspect", {"name": "inventory"}),
            ("mcp dg_backups", "dg_backups", {}),
            ("mcp dg_query", "dg_query",
             {"name": "inventory", "sql": "SELECT count(*) FROM products"}),
        ]
        request_id = 10
        for label, tool, arguments in calls:
            send({"jsonrpc": "2.0", "id": request_id, "method": "tools/call",
                  "params": {"name": tool, "arguments": arguments}})
            response = recv()
            request_id += 1
            report.add(
                label,
                response is not None and "error" not in response,
                json.dumps(response)[:200] if response else "no response",
            )

        # The refusals. An agent surface that will write is not a
        # read-only surface.
        for label, sql in [
            ("mcp refuses an UPDATE", "UPDATE products SET stock = 0"),
            ("mcp refuses a DELETE", "DELETE FROM products"),
            ("mcp refuses a DROP", "DROP TABLE products"),
            ("mcp refuses an INSERT", "INSERT INTO products (id) VALUES (1)"),
        ]:
            send({"jsonrpc": "2.0", "id": request_id, "method": "tools/call",
                  "params": {"name": "dg_query",
                             "arguments": {"name": "inventory", "sql": sql}}})
            response = recv()
            request_id += 1
            report.add(
                label,
                response is not None and "error" in response,
                json.dumps(response)[:200] if response else "no response",
            )

        send({"jsonrpc": "2.0", "id": request_id, "method": "tools/call",
              "params": {"name": "dg_delete_database", "arguments": {}}})
        response = recv()
        report.add(
            "mcp refuses an unknown tool",
            response is not None and "error" in response,
            json.dumps(response)[:200] if response else "no response",
        )

        send({"jsonrpc": "2.0", "id": request_id + 1, "method": "tools/call",
              "params": {"name": "dg_info", "arguments": {"name": "no_such_database"}}})
        response = recv()
        report.add(
            "mcp reports a missing database",
            response is not None and "error" in response,
            json.dumps(response)[:200] if response else "no response",
        )
    finally:
        try:
            process.stdin.close()
            process.wait(timeout=15)
        except (subprocess.TimeoutExpired, OSError):
            process.kill()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", default=os.environ.get("DG_SERVER", "http://127.0.0.1:7432"),
                        help="the gateway to exercise")
    parser.add_argument("--key-file", default=os.path.expanduser("~/.config/dg/key"),
                        help="the credential to talk to the gateway with")
    parser.add_argument("--dg", default="dg", help="the dg binary to drive")
    parser.add_argument("--prefix", default="",
                        help="prefix for the database and credential names")
    parser.add_argument("--cleanup", action="store_true",
                        help="tombstone the databases and revoke the credentials afterwards")
    parser.add_argument("--out", default="", help="write the JSON summary here as well")
    args = parser.parse_args()

    work = Path(tempfile.mkdtemp(prefix="dg-live-exercise-"))
    atexit.register(shutil.rmtree, work, True)

    client = Client(args.dg, args.server, args.key_file)
    report = Report()
    started = time.time()

    def named(name):
        return f"{args.prefix}{name}"

    names = [named(name) for name in NAMES]
    credentials = [(named(label), role, named(db)) for label, role, db in CREDENTIALS]
    # The base name of every database, mapped to the name it has on
    # this run. The statement lists below are written in base names,
    # because they are about the shapes of questions rather than about
    # one deployment.
    prefixed = {base: named(base) for base in NAMES}

    # Refuse to run against names that already exist: the exercise
    # inserts rows, and a name that is already there belongs to
    # somebody else. --reuse says the caller knows better: the
    # databases are already there with the data in them, and the point
    # is to give them work and check the surfaces, not to build them.
    code, listing = client.json(["list"])
    existing = set(listing.get("databases", [])) if isinstance(listing, dict) else set()
    clash = [name for name in names if name in existing]
    if clash and not args.reuse:
        print(f"these names already exist: {', '.join(clash)}", file=sys.stderr)
        print("pass --prefix to work beside them, or --reuse to use them", file=sys.stderr)
        return 2
    if args.reuse:
        missing = [name for name in names if name not in existing]
        if missing:
            print(f"these databases do not exist: {', '.join(missing)}", file=sys.stderr)
            return 2
        report.add("reuse: the databases are already there", True, ", ".join(names))
    else:
        print("== the databases ==")
        for name in names:
            code, _, err = client.run(["create", name])
            report.add(f"create {name}", code == 0, err.strip()[:160])

        for name in names:
            base = name[len(args.prefix):]
            path = work / f"{base}-schema.sql"
            path.write_text(";\n".join(SCHEMA[base]) + ";\n")
            code, _, err = client.run(["execute", name, str(path)])
            report.add(f"schema {name}", code == 0, err.strip()[:160])

            path = work / f"{base}-data.sql"
            path.write_text(";\n".join(ROWS[base]) + ";\n")
            code, _, err = client.run(["execute", name, str(path)])
            report.add(f"rows {name}", code == 0, err.strip()[:160])

    print("== the statement spread ==")
    for base, sql, bind in QUERIES:
        name = prefixed[base]
        code, _, err = client.run(["query", name, sql, "--args", json.dumps(bind)])
        if code != 0:
            report.add(f"read {base}", False, err.strip()[:160])
    report.add(f"{len(QUERIES)} read shapes ran", True)

    for base, sql, bind in MUTATIONS:
        name = prefixed[base]
        code, _, err = client.run(["query", name, sql, "--args", json.dumps(bind)])
        report.add(f"write {base}", code == 0, err.strip()[:160])

    print("== the scoped credentials ==")
    key_files = {}
    for label, role, database in credentials:
        code, out, err = client.run(["key", "create", label, "--role", role,
                                     "--database", database])
        if code != 0:
            report.add(f"issue {label}", False, err.strip()[:160])
            continue
        try:
            raw = json.loads(out.strip().splitlines()[-1])["key"]
        except (json.JSONDecodeError, KeyError, IndexError):
            report.add(f"issue {label}", False, "no key in the answer")
            continue
        path = work / f"{label}.key"
        path.write_text(raw)
        path.chmod(0o600)
        key_files[label] = str(path)
        report.add(f"issue {label}", True, f"{role} on {database}")

    print("== the credentials do some work ==")
    for label, _, database in credentials:
        path = key_files.get(label)
        if not path:
            continue
        code, out, _ = client.run(
            ["query", database, "SELECT count(*) FROM sqlite_master"], key_file=path
        )
        report.add(f"{label} reads {database}", code == 0, out.strip()[:120])

    print("== the refusals ==")
    read_only = key_files.get(named("warehouse"))
    if read_only:
        code, out, _ = client.run(
            ["query", named("inventory"), "UPDATE products SET stock = 0 WHERE id = 1"],
            key_file=read_only,
        )
        report.add("a read-only credential cannot write", code != 0, out.strip()[:160])
        code, out, _ = client.run(
            ["query", named("ledger"), "SELECT count(*) FROM entries"],
            key_file=read_only,
        )
        report.add("a read-only credential cannot reach another database",
                   code != 0, out.strip()[:160])
        code, out, _ = client.run(
            ["query", named("inventory"), "SELECT count(*) FROM products"],
            key_file=read_only,
        )
        report.add("a read-only credential can read its own database",
                   code == 0, out.strip()[:160])

    print("== the read-only surfaces ==")
    surfaces = [
        ("list", ["list"]),
        ("status", ["status"]),
        ("watch", ["watch"]),
        ("queries", ["queries"]),
        ("audit", ["audit"]),
        ("inspect", ["inspect", names[0]]),
        ("info", ["info", names[0]]),
        ("key list", ["key", "list"]),
        ("key tokens", ["key", "tokens", names[0]]),
        ("integrity", ["integrity", names[0]]),
        ("vacuum", ["vacuum", names[-1]]),
        # The explain takes a statement, not a statement with bind
        # parameters: it explains the SQL as written.
        ("explain", ["explain", names[0], "SELECT id, name FROM products WHERE stock < 50"]),
        ("restore plan", ["restore", "plan"]),
        ("restore list", ["restore", "list"]),
        ("backup run", ["backup", "run"]),
    ]
    for label, argv in surfaces:
        code, _, err = client.run(argv)
        report.add(label, code == 0, err.strip()[:160])

    print("== the branch lifecycle ==")
    branch = named("branch_probe")
    code, _, err = client.run(["branch", "create", names[1], branch])
    report.add("branch create", code == 0, err.strip()[:160])
    code, _, err = client.run(["branch", "list"])
    report.add("branch list", code == 0, err.strip()[:160])
    code, _, err = client.run(["branch", "show", branch])
    report.add("branch show", code == 0, err.strip()[:160])
    code, _, err = client.run(["query", branch, "UPDATE orders SET status = 'refunded' WHERE id = 1"])
    report.add("write into the branch", code == 0, err.strip()[:160])
    code, out, _ = client.run(["diff", names[1], branch])
    # `dg diff` exits nonzero when the two are not the same: that is the
    # answer, not a failure.
    report.add("diff finds the divergence", code != 0 and '"identical":false' in out,
               out.strip()[:160])
    # A branch is deleted by typing its name again: the confirmation
    # is not optional, and the exercise does not get to skip it.
    code, _, err = client.run(["branch", "delete", branch, "--confirm", branch])
    report.add("branch delete", code == 0, err.strip()[:160])

    print("== the agent surface ==")
    exercise_mcp(client, report, args.key_file)

    if args.cleanup:
        print("== cleaning up ==")
        for label, _, _ in credentials:
            code, _, err = client.run(["key", "revoke", label])
            report.add(f"revoke {label}", code == 0, err.strip()[:160])
        # A database is tombstoned by typing its name again. The exit
        # code is checked, because a cleanup that quietly did nothing
        # is worse than one that failed loudly: the operator would
        # believe the box was clean.
        for name in names:
            code, _, err = client.run(["delete", name, "--confirm", name])
            report.add(f"tombstone {name}", code == 0, err.strip()[:160])

    summary = {
        "server": args.server,
        "seconds": round(time.time() - started, 1),
        "checks": len(report.checks),
        "passed": len(report.checks) - len(report.failures),
        "failed": len(report.failures),
        "failures": [check["name"] for check in report.failures],
        "databases": names,
        "credentials": [label for label, _, _ in credentials],
        "detail": report.checks,
    }
    print(json.dumps({k: v for k, v in summary.items() if k != "detail"}))
    if args.out:
        Path(args.out).write_text(json.dumps(summary, indent=2) + "\n")
    return 1 if report.failures else 0


if __name__ == "__main__":
    sys.exit(main())
