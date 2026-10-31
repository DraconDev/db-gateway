#!/usr/bin/env python3
"""Seed a deliberately awkward database collection, to stress the view.

Usage: seed-stress.py [--server URL] [--key-file PATH] [--dg PATH]

`seed-demo.py` fills the live databases with data that is pleasant to
read. This script does the opposite: it builds a collection whose every
table is an edge the terminal view has to survive, so that "the UI holds
up" is something you can look at rather than assume.

The edges, one per table where possible:

* many tables in one database -- does the page's table list scroll, or
  silently hide the rest?
* a wide table (18 columns) -- does the header overflow, or are columns
  allocated and cut?
* a table with thousands of rows -- does the footer page it honestly?
* values far longer than the screen -- wrapped, cut, or written over the
  border?
* non-ASCII and double-width text (CJK, emoji, combining accents) -- is
  the width math measured in characters or in cells? A list padded by
  `.chars().count()` misaligns on wide glyphs.
* a table with no rows -- an empty state, not an empty box
* a table name longer than the list is wide
* a database name longer than the list is wide
* a deliberately tiny database, so "one table" is visible next to
  "forty"

It is idempotent in the same way `seed-demo.py` is: tables are created
with IF NOT EXISTS and rows are only inserted into tables that are still
empty, so re-running it changes nothing.

Run: timeout 300 python3 scripts/seed-stress.py --server URL --key-file KEY
"""

import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path

BATCH_LIMIT = 32

# A name long enough to be wider than the page's table list.
LONG_TABLE = "a_very_long_table_name_that_keeps_going_for_a_while_and_then_some"
LONG_DATABASE = "stress_a_database_name_that_is_long_enough_to_matter"

MANY_TABLES = 40
WIDE_COLUMNS = 18
MANY_ROWS = 5000
LONG_VALUE = "L" * 2400


class Gateway:
    """Run `dg` against one gateway, and fail loudly when it refuses."""

    def __init__(self, dg, server, key_file):
        self.dg = dg
        self.server = server
        self.key_file = key_file

    def run(self, *args, **kwargs):
        command = [
            self.dg,
            "--server",
            self.server,
            "--key-file",
            self.key_file,
            *args,
        ]
        done = subprocess.run(command, capture_output=True, text=True, timeout=120)
        if done.returncode != 0 and not kwargs.get("allow_failure"):
            raise SystemExit(
                f"{' '.join(command)} failed ({done.returncode}):\n"
                f"{done.stderr.strip() or done.stdout.strip()}"
            )
        return done

    def query(self, database, sql):
        return self.run("query", database, sql)

    def count(self, database, table):
        """Rows in a table, or None when the table is not there yet."""
        done = self.run(
            "--json", "query", database, f"SELECT count(*) FROM {table}",
            allow_failure=True,
        )
        if done.returncode != 0:
            return None
        try:
            document = json.loads(done.stdout)
        except ValueError:
            return None
        rows = document.get("rows") or []
        if rows and rows[0]:
            return int(rows[0][0])
        return None

    def batch(self, database, statements):
        """Send statements in the 32-at-a-time batches the API accepts."""
        for start in range(0, len(statements), BATCH_LIMIT):
            chunk = statements[start : start + BATCH_LIMIT]
            with tempfile.NamedTemporaryFile(
                "w", suffix=".json", delete=False, encoding="utf-8"
            ) as handle:
                json.dump(
                    {
                        "statements": [{"sql": sql, "args": []} for sql in chunk],
                        # `atomic` is a required field of the batch shape:
                        # `read_batch_file` deserialises this struct with
                        # `deny_unknown_fields`, and a file without it is
                        # refused as a bare "invalid_request".
                        "atomic": True,
                    },
                    handle,
                )
                path = handle.name
            try:
                self.run("batch", database, path)
            finally:
                Path(path).unlink(missing_ok=True)


def sql_text(value):
    """A SQL string literal holding the real text.

    Not `json.dumps`: that escapes non-ASCII by default, so the database
    would store the six characters `\\u65e5` instead of the glyph and the
    view would look like it cannot draw CJK when the data was never CJK.
    Doubling the quote is SQLite's own escaping rule.
    """
    escaped = value.replace("'", "''")
    return f"'{escaped}'"


def fill_only_when_empty(gateway, database, table, statements):
    """Insert only into an empty table, so a re-run changes nothing."""
    if (gateway.count(database, table) or 0) > 0:
        return False
    gateway.batch(database, statements)
    return True


def wide_metrics():
    """18 columns: more than a terminal can show side by side."""
    columns = ", ".join(f"metric_{index:02d} INTEGER" for index in range(WIDE_COLUMNS))
    values = ", ".join(f"i * {index + 1}" for index in range(WIDE_COLUMNS))
    return [
        "CREATE TABLE IF NOT EXISTS wide_metrics (id INTEGER PRIMARY KEY, "
        f"label TEXT, {columns})",
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 300) "
        f"INSERT INTO wide_metrics (id, label, {', '.join(f'metric_{index:02d}' for index in range(WIDE_COLUMNS))}) "
        f"SELECT i, 'sample-' || i, {values} FROM n",
    ]


def many_rows():
    return [
        "CREATE TABLE IF NOT EXISTS many_rows (id INTEGER PRIMARY KEY, note TEXT)",
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < "
        f"{MANY_ROWS}) "
        "INSERT INTO many_rows (id, note) "
        "SELECT i, 'row ' || i || ' of ' || "
        f"{MANY_ROWS} FROM n",
    ]


def unicode_labels():
    """Double-width and combining text: padding by char count misaligns."""
    rows = [
        (1, "日本語のラベル", "全角の値です"),
        (2, "emoji 🚀 launch", "🙂🙂🙂"),
        (3, "combining é vs é", "Zürich, Kraków"),
        (4, "short", "ค่า"),
        (5, "한국어", "테스트"),
        (6, "mixed 日本 abc", "𝒜𝒷𝒸"),
        (7, "zero width\u200bjoiner", "a\u200bb"),
        (8, "tabs\tand\nnewlines", "plain"),
    ]
    statements = [
        "CREATE TABLE IF NOT EXISTS unicode_labels "
        "(id INTEGER PRIMARY KEY, label TEXT, value TEXT)"
    ]
    for identifier, label, value in rows:
        statements.append(
            "INSERT INTO unicode_labels (id, label, value) VALUES "
            f"({identifier}, {sql_text(label)}, {sql_text(value)})"
        )
    return statements


def build_stress_wide(gateway):
    """One database holding every value and width edge."""
    database = "stress_wide"
    gateway.run("create", database, allow_failure=True)

    created = []
    if fill_only_when_empty(gateway, database, "wide_metrics", wide_metrics()):
        created.append("wide_metrics")
    if fill_only_when_empty(gateway, database, "many_rows", many_rows()):
        created.append("many_rows")
    if fill_only_when_empty(gateway, database, "unicode_labels", unicode_labels()):
        created.append("unicode_labels")

    leftovers = [
        "CREATE TABLE IF NOT EXISTS empty_table (id INTEGER PRIMARY KEY, note TEXT)",
        "CREATE TABLE IF NOT EXISTS long_text_payload (id INTEGER PRIMARY KEY, body TEXT)",
        "CREATE TABLE IF NOT EXISTS nulls_everywhere "
        "(id INTEGER PRIMARY KEY, a TEXT, b INTEGER, c REAL, d BLOB)",
        f"CREATE TABLE IF NOT EXISTS {LONG_TABLE} (id INTEGER PRIMARY KEY, note TEXT)",
    ]
    gateway.batch(database, leftovers)

    if fill_only_when_empty(
        gateway,
        database,
        "long_text_payload",
        [
            "INSERT INTO long_text_payload (id, body) VALUES "
            f"(1, {sql_text(LONG_VALUE)})",
            "INSERT INTO long_text_payload (id, body) VALUES "
            f"(2, {sql_text(LONG_VALUE * 4)})",
        ],
    ):
        created.append("long_text_payload")
    if fill_only_when_empty(
        gateway,
        database,
        "nulls_everywhere",
        [
            "INSERT INTO nulls_everywhere (id, a, b, c, d) VALUES (1, NULL, NULL, NULL, NULL)",
            "INSERT INTO nulls_everywhere (id, a, b, c, d) VALUES (2, 'set', 7, 1.5, NULL)",
            "INSERT INTO nulls_everywhere (id) VALUES (3)",
        ],
    ):
        created.append("nulls_everywhere")
    if fill_only_when_empty(
        gateway,
        database,
        LONG_TABLE,
        [f"INSERT INTO {LONG_TABLE} (id, note) VALUES (1, 'one'), (2, 'two')"],
    ):
        created.append(LONG_TABLE)
    return database, created


def build_many_tables(gateway):
    """One database whose table list is longer than the screen."""
    database = "stress_many_tables"
    gateway.run("create", database, allow_failure=True)
    statements = []
    for index in range(1, MANY_TABLES + 1):
        name = f"t{index:02d}_events"
        statements.append(
            f"CREATE TABLE IF NOT EXISTS {name} "
            "(id INTEGER PRIMARY KEY, kind TEXT, at TEXT)"
        )
        statements.append(
            f"INSERT INTO {name} (id, kind, at) "
            f"SELECT 1, 'seed', '2026-10-31' WHERE NOT EXISTS (SELECT 1 FROM {name})"
        )
    gateway.batch(database, statements)
    return database, [f"{MANY_TABLES} tables"]


def build_tiny(gateway):
    database = "stress_tiny"
    gateway.run("create", database, allow_failure=True)
    gateway.batch(
        database,
        [
            "CREATE TABLE IF NOT EXISTS only_table (id INTEGER PRIMARY KEY, note TEXT)",
            "INSERT INTO only_table (id, note) SELECT 1, 'the only row' "
            "WHERE NOT EXISTS (SELECT 1 FROM only_table)",
        ],
    )
    return database, ["only_table"]


def build_long_database_name(gateway):
    gateway.run("create", LONG_DATABASE, allow_failure=True)
    gateway.batch(
        LONG_DATABASE,
        [
            "CREATE TABLE IF NOT EXISTS first_table (id INTEGER PRIMARY KEY, note TEXT)",
            "CREATE TABLE IF NOT EXISTS second_table (id INTEGER PRIMARY KEY, note TEXT)",
            "INSERT INTO first_table (id, note) SELECT 1, 'first' "
            "WHERE NOT EXISTS (SELECT 1 FROM first_table)",
        ],
    )
    return LONG_DATABASE, ["first_table", "second_table"]


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", required=True)
    parser.add_argument("--key-file", required=True)
    parser.add_argument("--dg", default="dg")
    options = parser.parse_args(argv)

    gateway = Gateway(options.dg, options.server, options.key_file)
    built = [
        build_stress_wide(gateway),
        build_many_tables(gateway),
        build_tiny(gateway),
        build_long_database_name(gateway),
    ]
    for database, notes in built:
        detail = ", ".join(notes) if notes else "already seeded"
        print(f"{database}: {detail}")
    total_tables = 0
    for database, _ in built:
        structure = gateway.run(
            "--json", "query", database,
            "SELECT count(*) FROM sqlite_master WHERE type = 'table'",
        )
        document = json.loads(structure.stdout or "{}")
        rows = document.get("rows") or []
        if rows and rows[0]:
            total_tables += int(rows[0][0])
    print(f"{len(built)} databases, {total_tables} tables")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
