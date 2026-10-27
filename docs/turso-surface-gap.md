# Turso surface vs `dg`: per-table inspection gap audit

Date: 2026-10-26. `dg` side tested: 0.9.0 debug build (same tree as the
v0.9.0 release) against disposable local gateways, all removed
afterwards. Turso side: the `dracondev` web console read through the
browser bridge, with the live captures saved as audit evidence:

- `docs/evidence/turso-analytics-2026-10-26.txt` — Analytics page text
  (usage cards, latency chart labels, Top Databases, Top Queries).
- `docs/evidence/turso-databases-2026-10-26.txt` — Databases list text
  (all 19 rows with figures).
- `docs/evidence/turso-edit-data-2026-10-26.png` — Edit Data browser
  screenshot (`dracon-music-dev`, a database with tables).
- `docs/evidence/turso-audit-logs-2026-10-26.txt` — Audit Logs page
  text (columns, all 23 entries).
- `docs/evidence/turso-structure-view-2026-10-27.png` — the Edit Data
  → STRUCTURE tab (`dracon-music-dev`, table `music_users`): the
  columns with their declared types and flags, the constraints and the
  indexes sections.
- `docs/evidence/turso-structure-indexes-2026-10-27.png` — the same
  view with the index list open.
- `docs/evidence/turso-edit-data-grid-2026-10-27.png` — the Edit Data
  → DATA tab of the same table: the type-annotated grid headers and
  the Filters / Sort / Columns toolbar.
- `docs/evidence/schema-live-2026-10-27.txt` — the live record:
  the route's answer for `inventory.products`, the CLI on a pipe and
  on a terminal, and the per-database table counts after the deploy.
- `docs/evidence/turso-structure-2026-10-27.txt` — the capture note
  for the three captures above, with the read-only record.

The `turso` CLI is not logged in in this environment
(`turso db show …` answers "You are not logged in, please login with
turso auth login"), so no live `show`/`inspect`/`shell` ran against
Turso; the CLI surface below is inventoried from `--help` output quoted
verbatim, and the web captures cover the same facts. Key Turso-side
figures are quoted in each section; the full text lives in the files
above. Account-management surfaces are out of scope for this audit;
the final section names them.

## Headline answer

`dg` **can** check the tables. The gap the seed feared does not exist as
a missing capability:

- `dg info shop` →
  `{"created_at":1793019443,"name":"shop","size_bytes":16384,`
  `"tables":[{"name":"products","rows":1}],"tombstoned":false}`.
  Table names with row counts, per database. Observed 2026-10-26
  against the disposable gateway.
- `dg inspect shop` on a terminal renders `database / activity / backup
  / tables` sections (full output quoted in §7). Observed 2026-10-26.
- `dg shell`: `.tables` lists user tables, `.schema [table]` prints
  `CREATE` statements, arbitrary `SELECT` runs over the HTTP API
  (outputs quoted in §5).

What is real is narrower: partials ranked by operator impact in §8, plus
the places where single-host architecture answers "not applicable".

## 1. Analytics usage cards

Turso shows, per organisation, rows read/written against quota, sync
bytes, storage, a latency chart (p50–p99 + average), and 30-day charts
for rows read, rows written, inbound traffic, outbound traffic, syncs,
and storage. Evidence file §1 quotes, captured 2026-10-26:

- `Rows Read 2.20B / 2.50B`, `Rows Written 24.70M / 25M`,
  `Syncs 1.51 MB / 10 GB`, `Storage 1.15 GB / 9 GB`.
  (The capture's remaining elements are account-management surfaces,
  out of scope — see the final section.)

`dg` exposes the numerators but not the denominators. `dg status`
returned per-database `rows_read: 3, rows_written: 1` (full block
quoted below): raw lifetime counters with no plan quota to ratio them
against. The `/ 2.50B`, `/ 25M`, `/ 10 GB`, `/ 9 GB` denominators are
therefore MISSING on the `dg` side — Turso-side evidence is the usage
figures above; `dg` has no metering or plan limits (single owner, no
quotas), so a percent-of-quota card has nothing to divide by.
- Chart axes: rows read `0 … 1B`, rows written `0 … 8M`, inbound
  `0 B … 600 MB`, outbound `0 B … 1.2 GB`, syncs `0 B … 1.6 MB`,
  storage `0 B … 1.2 GB`; latency `0 ms … 80 ms` with
  `p50 p75 p90 p95 p99 Average`. (The page's account banners are out
  of scope and are listed in the final section, not quoted here.)

`dg` equivalent tested: `dg status` on the disposable gateway returned
(full output observed 2026-10-26, trimmed to the per-database block):

```json
"shop": {
  "last_write": 1793019443, "queue_timeouts": 0,
  "queue_wait_max_us": 11, "queue_wait_ms": 0,
  "queue_wait_p95_us": 11, "queue_waits": 5,
  "rows_read": 3, "rows_written": 1
}
```

plus admission permits and deployment disk figures.

Verdict: PARTIAL. `dg` has the counters but no history: no time-series
charts, no per-database over-time view, no latency percentiles over time.
Quota cards are out of scope (single owner, no metering).

## 2. Top databases

Turso ranks databases by rows read and, per database, shows average
query time, average rows read/written, query count, synced bytes,
inbound/outbound bytes, storage, and region. Evidence file quotes,
captured 2026-10-26 (Today UTC):

- `dracon-studio-dev / aws-eu-west-1`: 90.61K queries, 815.47 KB in,
  724.86 KB out, 805.42 MB storage.
- `dracon-jobs-dev`: `Query time (avg) 4.92 ms`, `Rows read (avg)
  45.48`, `Queries 476`, `Inbound 2.58 MB`, `Outbound 6.99 MB`.
- `dracon-studio`: `Writes Blocked`, 296.54 MB storage (also visible in
  the databases list, §4).

`dg` equivalents tested: `dg status` per-database traffic (rows, waits,
p95/max, last write — output above), `dg info` sizes,
`dg queries --database shop` shapes with `count`, `total_micros`,
`max_micros`, `rows_read` (full shape output quoted in §3; averages are
`total/count`), and the TUI databases tab columns.

Verdict: PARTIAL. Two facts have no `dg` equivalent: per-database
inbound/outbound bytes, and synced bytes (no replicas to sync).
Region/location is not applicable: single host by architecture
(`docs/architecture.md`).

## 3. Top queries

Turso ranks statement shapes by rows read with average query time,
average rows read/written, execution count, and the full SQL text.
Evidence file quotes, captured 2026-10-26:

- `104 ms` avg, `6.19K` rows avg, `3` queries:
  `SELECT slug, title, … FROM jobs ORDER BY COALESCE(datetime(posted_at),
  '') DESC, …`.
- `17.7 ms` avg, `59` queries: 9-row multi-VALUES `INSERT INTO jobs`.
- `608 µs` avg, `395` queries: `DELETE FROM jobs WHERE slug = ?`.

`dg` equivalent tested: `dg queries --database shop` on the disposable
gateway returned, verbatim (trimmed to two of four shapes):

```json
{"count": 1, "first_seen": 1793019448, "last_seen": 1793019448,
 "max_micros": 71, "principals": {"owner": 1}, "rows_read": 1,
 "rows_written": 0, "shape": "SELECT * FROM products", "total_micros": 71}
{"count": 1, "max_micros": 124, "principals": {"owner": 1},
 "rows_read": 0, "rows_written": 0,
 "shape": "CREATE TABLE products (id INTEGER PRIMARY KEY, name TEXT)",
 "total_micros": 124}
```

Verdict: COVERED. Count, totals, maxima, rows read/written, first/last
seen, and principals are all present; averages are `total/count`; the
`?` shape redaction matches Turso's literal-free display.

## 4. Databases list

Turso lists every database with rows read, rows written, bytes synced,
storage, group, and status, plus a per-row "Edit Data" link. Full text
in `docs/evidence/turso-databases-2026-10-26.txt`; headlines quoted:

- Columns: `Name Type Rows Read Rows Written Bytes Synced Storage Group
  Status`.
- `dracon-studio-dev | 2,165,085,739 | 19,599,101 | 0 B | 805.42 MB |
  default | Writes Blocked`
- `dracon-studio | 46,796,361 | 5,437,201 | 0 B | 296.54 MB |
  default | Writes Blocked`
- `ai-books | 37,032,892 | 2 | 0 B | 29.52 MB | default | Active`
- 19 databases total; page chrome offers `Restore deleted databases`
  and `Show branches`.

`dg` equivalent tested: `dg list` on the disposable gateway returned
`{"databases":["shop"]}` (observed 2026-10-26). Traffic-per-database
lives in `dg status` (output in §1) and the TUI databases tab carries
the same columns (rows read/written, waits, wait p95/max, last write,
size) with a footer defining the wait columns.

Verdict: COVERED for names + per-database traffic + sizes. No `dg`
equivalent for `Writes Blocked`-style status (single host has no
throttle state), deleted-database restore (tombstones purge after
retention instead), or branches.

## 5. Per-database Edit Data browser

The browser (Drizzle Studio) has three panes, observed in
`docs/evidence/turso-edit-data-2026-10-26.png` (`dracon-music-dev`,
captured 2026-10-26):

- Left: `SQL console` entry, `Schema` panel listing tables
  (`music_follows`, `music_likes`, `music_playlist_songs`,
  `music_playlists`, `music_users`), search box.
- Main: `DATA | STRUCTURE` tabs, `Filters`, `Sort`, `Columns`,
  `Add record` actions, pagination `1 - 50 of 50+`, last run `183ms`.
- Grid rows show two columns (`subject text`, `created_at integer`)
  with values such as `alt-token | 1788275886575`,
  `finalcheck | 1788276443309`.
- An earlier capture of the empty `ai-model-list` browser showed the
  zero-state: `No items in database` with a `Create table` action.
- Tab strip above the browser: `Overview | Branches | Analytics |
  Top Queries | Edit Data` — the per-database Analytics and Top Queries
  tabs scope §1–§3 to one database.

`dg` equivalents tested on the disposable gateway, all observed
2026-10-26:

- Schema panel ≡ `dg shell` dot-commands. Piped session:
  `use shop` → `using shop`; `.tables` →
  `┌──────────┐ │ name │ ├──────────┤ │ products │ └──────────┘ 1 row`;
  `.schema products` → the `CREATE TABLE products (id INTEGER PRIMARY
  KEY, name TEXT)` row. `.help` lists `.tables`, `.schema [table]`,
  `.status`, `.keys`, `.backups`, `:batch`, `:key`, `.json`.
- SQL console ≡ `dg shell` interactive/piped plus one-shot `dg query`:
  `dg query shop "SELECT * FROM products"` →
  `{"columns":["id","name"],"rows":[[1,"widget"]],"rows_affected":0}`.
- Row grid reads ≡ `SELECT` in either; the grid's Add record ≡ an
  `INSERT` statement, tested exactly (observed 2026-10-26):
  `dg query shop "INSERT INTO products (id, name) VALUES (1,
  'widget')"` → `{"columns":[],"rows":[],"rows_affected":1}`;
  read-back `dg query shop "SELECT * FROM products"` →
  `{"columns":["id","name"],"rows":[[1,"widget"]],
  "rows_affected":0}`; `dg info shop` then reports the table with
  `"rows":1`. Table creation is `CREATE TABLE`, tested the same way
  (`rows_affected: 0`, then visible in `.tables` and `dg info`).

Verdict: COVERED with one PARTIAL: no click-grid row browser exists,
but every operation the grid performs (list tables, show schema, read
rows, add record, create table) is expressible and was exercised above
with the exact commands and outputs quoted.
The per-database Analytics/Top Queries scoping has no one-shot `dg`
equivalent beyond `dg queries --database <name>` and `dg status`.

### 5.1 The STRUCTURE tab, and what dg now answers of it

Turso side, captured 2026-10-27 in the browser, read-only:

- `docs/evidence/turso-structure-view-2026-10-27.png` — the
  `STRUCTURE` tab of `dracon-music-dev` → table `music_users`. On
  screen: `Table name: music_users`; **COLUMNS** —
  `subject TEXT PRIMARY KEY` and `created_at INTEGER NOT NULL`, each
  column's declared type and flags printed beside its name;
  **CONSTRAINTS** (empty for this table, with an `Add constraint`
  affordance); **INDEXES** (an `Add index` affordance and an
  `AUTO-INDEXES` disclosure, collapsed); and a `Copy create
  statements` action beside the tab.
- `docs/evidence/turso-structure-indexes-2026-10-27.png` — the same
  view with the index list open: `UNIQUE INDEX
  sqlite_autoindex_music_users_1 ... (subject)`.
- `docs/evidence/turso-edit-data-grid-2026-10-27.png` — the `DATA` tab
  of the same table, kept because it is the surface the structure is
  compared against: the type-annotated grid headers (`subject text`,
  `created_at integer` — the declared type in a dimmer style beside
  each column name), the `Filters` / `Sort` / `Columns` toolbar, the
  `+ Add record` action, the pager (`1 - 50 of 50+`) and the per-row
  checkboxes.
- `docs/evidence/turso-structure-2026-10-27.txt` — the capture note:
  what was open, what each capture shows, and the read-only record
  (no record was added, edited or deleted; no SQL was run against
  Turso; the `Add column` / `Add constraint` / `Add index`
  affordances were left untouched).

What `dg` covers now, added 2026-10-27 and exercised against the
seeded live gateway (outputs quoted verbatim in
`docs/evidence/schema-live-2026-10-27.txt`):

- **The per-column facts**: `dg schema <db> [table]` gives each
  column's name, its **declared** type, and its `not_null` and
  `primary_key` flags — the same facts Turso prints beside the column
  name, read from SQLite's own `PRAGMA table_info` rather than parsed
  out of the `CREATE` statement, so a composite key and a `UNIQUE`
  constraint's index are facts and not guesses.
- **The index list**: `PRAGMA index_list` / `index_info` give each
  index's name, its uniqueness and the columns it covers — the
  `AUTO-INDEXES` list, in the same facts.
- **The foreign keys**: `PRAGMA foreign_key_list` gives the target
  table, both columns and the `on_update` / `on_delete` actions. Turso
  shows these under CONSTRAINTS; `dg` shows them read-only.
- **The `CREATE` statement**: kept verbatim per table, so
  `Copy create statements` is a copy away.
- **One surface, two faces**: the TUI's `schema <db> [table]` palette
  action draws the same structure through the same shared view layer
  (`src/views.rs`), so the terminal view and the terminal view cannot
  tell an operator different stories — the same rule the rest of the
  TUI already keeps.
- **Read-only, and machine-readable**: the new surface performs no
  DDL of any kind; a pipe reads the gateway's own JSON document
  unchanged.

What `dg` still lacks against Turso's Edit Data browser, unchanged by
this work:

- **The grid's own browser**: `Filters`, `Sort`, the `Columns` picker,
  the per-row checkboxes, `+ Add record` and the `1 - 50 of 50+` pager
  are still the row grid's business, and the grid's behaviour is
  deliberately unchanged. `dg` reads rows a page at a time through
  `dg query` / the TUI's `open table`; it has no click grid over them.
- **Type-annotated grid headers**: Turso prints the declared type in
  the row grid's header, beside the column name. `dg`'s row grid shows
  the column names; the type now lives in the structure view instead.
- **Editing the structure**: Turso's panel edits columns, constraints
  and indexes in place (`Add column`, `Add constraint`, `Add index`).
  `dg`'s structure view is read-only by design; changing a schema is
  a `CREATE` / `ALTER` statement run through the query path, which is
  where `dg` keeps every write.
- **The remaining §1–§3 and account surfaces** are unchanged: usage
  history, per-database network bytes, and everything about billing,
  plans, organisations and invoices stays out of scope.

## 6. Turso CLI verbs

Turso side: `turso db --help`, captured 2026-10-26 (the CLI is not
logged in, so this is verb inventory, not live output):

```text
Available Commands:
  config      Manage db config
  create      Create a database.
  destroy     Destroy a database.
  export      Export a database snapshot and WAL from Turso to SQLite files.
  import      Import a SQLite database file to Turso.
  inspect     Inspect database.
  list        List databases.
  locations   List available database locations.
  replicate   Replicate a database.
  shell       Start a SQL shell.
  show        Show information from a database.
  tokens      Manage db tokens
  unarchive   Unarchive a database
  update      Updates the database to the latest turso version
```

Top level (`turso --help`) adds `group` (database groups — mapped
below), `dev`, `config`, and `quickstart`, plus account-management
verbs that are out of scope (see the final section). The in-scope
audit surface is the web Audit Logs page: columns Author/Event/Origin/
Timestamp, 23 items on 2026-10-26, all `Created Database Token` by
`dracondev` via `dashboard` (2) or `cli` (21) — full text in
`docs/evidence/turso-audit-logs-2026-10-26.txt`. (The CLI exposes the
same logs as `turso org audit-logs list [--limit N] [--page N]
[--verbose]`; that verb lives under the excluded `turso org` namespace
— see the final section — so the §6 row below is anchored on the web
page, not the CLI verb.) `group` manages
(`create/destroy/list/locations/…`); `replicate` usage is
`turso db replicate <database-name> <location-code>`; `tokens` manages
`create` (bearer token) and `invalidate` (rotate signing keys).

| Turso verb | `dg` command tested (observed 2026-10-26) | Verdict |
|---|---|---|
| `db list` | `dg list` → `{"databases":["shop"]}` | COVERED |
| `db create` | `dg create shop` → exit 0; `dg list` then shows `shop` | COVERED |
| `db destroy` | `dg delete shop2 --confirm shop2` → `{"deleted":true,"purge_after_seconds":86400}`; `dg list` confirms removal | COVERED |
| `db show` | `dg info shop` → `{"created_at":1793019443,"name":"shop","size_bytes":16384,"tables":[{"name":"products","rows":1}],"tombstoned":false}` | PARTIAL: no URLs/instances/branches — single host has none |
| `db shell` (interactive) | `dg shell` `.tables`/`.schema` session quoted in §5 | COVERED |
| `db shell <db> "sql"` (one-shot) | `dg query shop "SELECT * FROM products"` → `{"columns":["id","name"],"rows":[[1,"widget"]],"rows_affected":0}` | COVERED |
| `db execute` | MISSING on the Turso side: no `execute` verb in the help above, and `turso db execute --help` falls through to the parent `db` help. `docs/cli-turso-mapping.md:24` claims a `` `turso db execute <name> "<sql>"` `` → `dg query` row that does not match the installed CLI; one-shot SQL on Turso is `turso db shell <db> "sql"`. The `dg` side of that mapping (`dg query`, output above) is verified. | MISSING (doc correction, see §8.4) |
| `db inspect` | `dg inspect shop` on a terminal renders database/activity/backup/tables (full output in §7); piped output falls back to the `dg info` document | PARTIAL: see §7 non-TTY fallback |
| `db export` | `dg export --out /tmp/…/shop.sqlite shop` → `{"name":"shop","out":"…","sha256":"add77d17…","size":16384}` | COVERED |
| `db import` | `dg import --from /tmp/…/shop.sqlite shop2` → `{"imported":"shop2"}`; `dg info shop2` shows the table with 1 row | COVERED |
| `db tokens` | `dg key list` → `{"keys":[{"databases":[],"name":"owner","role":"owner"}]}`; `dg key create app1 --role database_read_write --database shop` printed a raw key; `dg key revoke app1` → `{"revoked":true}` | COVERED (create/list/revoke exercised; `rotate-owner` exists per `dg key --help`, not exercised) |
| `db locations`, `turso group …`, `db replicate` | No `dg` equivalent | NOT APPLICABLE: single-host architecture, documented non-goal (`docs/architecture.md`). Turso-side evidence: `locations List available database locations`, `turso db replicate <database-name> <location-code>`, `turso group [create/destroy/list/locations/…]` |
| `db config`, `db update`, `db unarchive` | `dg config list` → `{"config_path":"/home/dracon/.config/dg/config.toml","contexts":[],"default":null}` (local contexts exist) | PARTIAL/N/A: contexts cover connection config; server update and unarchive have no single-host meaning |
| Account-management verbs | None | OUT OF SCOPE — named in the final section |
| Turso web Audit Logs page (CLI equivalent lives under the excluded `turso org` namespace) | `dg audit` → `{"entries":[{"action":"create","at":1793019443,"database":"shop","principal":"owner","role":"owner"},{"action":"query","database":"shop","detail":"CREATE TABLE products (id INTEGER PRIMARY KEY, name TEXT)","principal":"owner"},{"action":"query","database":"shop","detail":"INSERT INTO products VALUES (?, ?)","principal":"owner"},{"action":"query","database":"shop","detail":"SELECT * FROM products","principal":"owner"}]}` (trimmed; observed 2026-10-26) | PARTIAL: who ran what and when, with statement shapes and no values — covers Author/Event/Timestamp, but `dg audit` entries carry no Origin: Turso distinguishes `dashboard` from `cli` (evidence file: 2 dashboard + 21 cli entries) while every `dg` entry differs only by principal/role |

## 7. `dg inspect`: the terminal-only rich view

`dg inspect shop` on a terminal (ANSI stripped), observed 2026-10-26:

```text
database
  name     shop
  size     48.0 KiB
  state    live
  created  2026-10-26 12:57 UTC (1793019443)

activity
  rows read     3
  rows written  1
  queue waits   5
  wait p95      11 us
  wait max      11 us
  last write    15s ago

backup
  state       unknown
  newest set  -
  age         unknown

tables
┌──────────┬──────┐
│ table    │ rows │
├──────────┼──────┤
│ products │    1 │
└──────────┴──────┘
```

Piped or `--json`, the same command returns the raw
`/v1/databases/{name}` document — the same facts as `dg info`, with no
activity, no backup freshness, and no tables section.
`src/cmd_data.rs::inspect` branches on `IsTerminal::is_terminal`; the
non-TTY arm returns the plain info request. Consequence: scripts cannot
read the activity/backup/tables view an operator sees; automation must
rejoin `/v1/databases/{name}`, `/v1/status`, and `/v1/backups` by hand.
Sharpest scriptability gap found.

## 8. Ranked gaps (operator impact first)

1. `dg inspect` degrades off-terminal (§7). Smallest fix with the
   largest automation payoff; no API change needed (compose the three
   reads the TTY path already makes).
2. No usage history (§1). Turso's charts answer "what changed last
   week"; `dg` answers "what is true now".
3. No per-database network bytes (§2). Queue waits are the only
   pressure signal per database.
4. Mapping-doc corrections (§6): the `turso db execute` row
   (`docs/cli-turso-mapping.md:24`) names a verb the installed CLI
   does not have; and `dg key --help` shows the owner-rotation
   subcommand is `rotate-owner`, not bare `rotate`. Cosmetic, but the
   doc is the CLI contract's map.

## Out of scope, untouched

Billing, plans, organisations, invoices, subscriptions, and auth tokens
for third parties. Excluded CLI verbs: `turso auth`, `turso org` (including
`turso org audit-logs list`), and `turso plan` with all their subcommands. Concretely observed but excluded: the Analytics
page's `Next Invoice $5.99` card with its `View Invoice` action, the
`Quota Critical 99% used … See plans` banners, the `Vegas Blackout:
Usage was erased on October 11th` notice, and the sidebar's
Billing/Upgrade entries. This audit read the Turso web console and `--help`
text only; the `turso` CLI is not authenticated here, no live
`show`/`inspect`/`shell` ran against Turso, and no Turso data was
created, modified, or deleted. All `dg` testing ran against disposable
local gateways (temp dirs removed afterwards).

This report was updated on 2026-10-27, when `dg` gained the structure
surface described in §5.1: the `GET /v1/databases/{name}/schema` route,
the `dg schema <database> [table]` command and the TUI's `schema` action.
That is the only `dg` change this report's second edition covers; the
repository changes are the new surface (route, command, action, shared
view, client typing, tests), this report, and the four evidence files
cited at the top. The Turso captures were taken read-only, and `dg` has
no outbound Turso contact: the new surface reads the gateway's own
SQLite file and nothing else.
