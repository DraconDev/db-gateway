# Turso surface vs `dg`: per-table inspection gap audit

Date: 2026-10-26. `dg` side tested: 0.9.0 debug build (same tree as the
v0.9.0 release) against a disposable local gateway. Turso side: the
`dracondev` web console read through the browser bridge (Analytics page,
Databases list, per-database Edit Data browser for `ai-model-list`), plus
the `turso` CLI verb inventory from `--help`. The CLI is not logged in in
this environment, so no live `turso db show/inspect` output was pulled;
the web captures cover the same facts. Billing, plans, orgs, invoices,
and subscription changes are out of scope and appear only in §5.

## Headline answer

`dg` **can** check the tables. The gap the seed feared does not exist as
a missing capability:

- `dg info shop` → `{"tables":[{"name":"orders","rows":0},
  {"name":"products","rows":1}], "size_bytes":…, "tombstoned":false}`.
  Table names with row counts, per database. (Observed 2026-10-26
  against the disposable gateway.)
- `dg inspect shop` on a terminal renders `database / activity / backup /
  tables` sections: size, live state, rows read/written, queue waits,
  wait p95/max, last write, backup state, and a per-table row-count
  table. Observed output quoted in §3.
- `dg shell`: `.tables` lists user tables, `.schema [table]` prints
  `CREATE` statements, and arbitrary `SELECT` runs over the HTTP API.
  `.help` output observed 2026-10-26 (see §3).

What is real is narrower: three partials and the places where
single-host architecture answers "not applicable". Ranked by
operator impact in §4.

## 1. Analytics usage cards

Turso shows, per organisation, rows read/written against quota, sync
bytes, storage, a latency chart (p50–p99 + average), and 30-day charts
for rows read, rows written, inbound traffic, outbound traffic, syncs,
and storage. Observed values 2026-10-26: rows read 2.20B/2.50B, rows
written 24.70M/25M, storage 1.15GB/9GB.

`dg` equivalent: `dg status` / `dg watch` (version, uptime, admission
config, request-pressure counters) and the TUI service tab. Observed on
the disposable gateway: per-database `rows_read`, `rows_written`,
`queue_waits`, `queue_wait_p95_us`, `queue_wait_max_us`, `last_write`,
plus deployment disk figures.

Verdict: PARTIAL. `dg` has the counters but no history: no time-series
charts, no per-database over-time view, no latency percentiles over time.
Quota cards are out of scope (single owner, no metering).

## 2. Top databases

Turso ranks databases by rows read and, per database, shows average
query time, average rows read/written, query count, synced bytes,
inbound/outbound bytes, storage, and region (all `aws-eu-west-1` here).

`dg` equivalent: `dg status` per-database traffic (rows, waits, p95/max,
last write), `dg info` sizes, `dg queries` per-database shapes with
counts and microsecond totals/maxima (averages derivable), TUI databases
tab (all of the above in columns).

Verdict: PARTIAL. Two facts have no `dg` equivalent: per-database
inbound/outbound bytes, and synced bytes (no replicas to sync).
Region/location is not applicable: single host by architecture
(`docs/architecture.md`).

## 3. Top queries

Turso ranks statement shapes by rows read with average query time,
average rows read/written, execution count, and the full SQL text.
Observed shapes 2026-10-26 include a 104 ms `SELECT … FROM jobs ORDER
BY …` (3 executions, 6.19K rows avg) and a 59-execution multi-row
`INSERT INTO jobs`.

`dg` equivalent: `dg queries [--database <name>]` returns shapes with
`count`, `total_micros`, `max_micros`, `rows_read`, `rows_written`,
`first_seen`/`last_seen`, and principals. Observed 2026-10-26 on the
disposable gateway: six shapes including `INSERT INTO products VALUES
(?, ?, ?)` with `rows_written: 1`, and the shell's own
`sqlite_master` introspection queries appearing as shapes (self-noise,
also true of any query log).

Verdict: COVERED. Averages are `total/count`; the shape redaction
(`?` placeholders) matches Turso's literal-free display.

## 4. Databases list and the Edit Data browser

Turso lists every database with rows read, rows written, bytes synced,
storage, group, and status (`Active` / `Writes Blocked`), plus an
"Edit Data" browser per database: SQL console, schema panel, table
list, row grid, create-table. Observed: 19 databases; the `ai-model-list`
browser shows "No items in database" with a Create table action.

`dg` equivalents, all observed 2026-10-26:

- `dg list` → names visible to the credential. The TUI databases tab
  carries the traffic columns (rows read/written, waits, wait p95/max,
  last write, size) with a footer defining the wait columns.
- Edit Data's SQL console ≡ `dg shell` (interactive, piped stdin works)
  and one-shot `dg query`. Its schema panel ≡ `.tables` + `.schema`.
  Its row grid ≡ `SELECT` in either; writes are SQL, not clicks.
- `dg inspect shop` on a terminal renders the per-database page quoted
  below (trimmed of ANSI):

```text
database
  name     shop
  size     20.0 KiB
  state    live
  created  2026-10-26 12:50 UTC (1793019028)

activity
  rows read     4
  rows written  1
  queue waits   6
  wait p95      24 us
  wait max      24 us
  last write    31s ago

backup
  state       unknown
  newest set  -
  age         unknown

tables
  table     rows
  orders    0
  products  1
```

Verdict: COVERED with one PARTIAL inside it. The partial: `dg inspect`
renders the rich view **only on a terminal**. Piped or `--json` output
falls back to the raw `/v1/databases/{name}` document -- the same facts
as `dg info`, with no traffic, no backup freshness, and no tables
section wrapper. `src/cmd_data.rs::inspect` branches on
`IsTerminal::is_terminal`; the non-TTY arm returns the plain info
request. Consequence: scripts cannot read the activity/backup/tables
view that an operator sees; anything automated must rejoin three
endpoints (`/v1/databases/{name}`, `/v1/status`, `/v1/backups`) by hand.
That is the sharpest scriptability gap this audit found.

## 5. Turso CLI verbs (inventory from `turso db --help`)

| Turso verb | `dg` equivalent | Verdict |
|---|---|---|
| `db list` | `dg list` | COVERED |
| `db create` | `dg create` | COVERED (per mapping doc) |
| `db destroy` | `dg delete --confirm` (tombstone + purge) | COVERED (per mapping doc) |
| `db show` (URLs, instances, branches) | `dg info` (size/tables/created/tombstone) + `dg config` contexts | PARTIAL: no URLs/instances/branches -- single host has none |
| `db shell [sql]` | `dg shell` / `dg query` / piped stdin | COVERED |
| `db execute` | -- | N/A: no such verb; one-shot SQL is `turso db shell <db> "sql"`. `docs/cli-turso-mapping.md` names a `turso db execute` row that does not exist and should say `shell` |
| `db inspect` (usage) | `dg inspect` (TTY) | PARTIAL: see §4 non-TTY fallback |
| `db export` / `db import` | `dg export` / `dg import` | COVERED (verbs exist; outputs not exercised here) |
| `db tokens` | `dg key` (create/list/revoke/rotate) | COVERED (per mapping doc) |
| `db locations` / `group` / `replicate` / branches | -- | NOT APPLICABLE: single-host architecture, documented non-goal |
| `db config` / `update` / `unarchive` | `dg config` contexts; n/a | PARTIAL/N/A: contexts cover connection config; server update and unarchive have no single-host meaning |
| `auth` / `org` / `plan` | -- | OUT OF SCOPE (billing/identity) |

## 6. Ranked gaps (operator impact first)

1. `dg inspect` degrades off-terminal (§4). A scripted fleet check
   cannot read what the operator sees. Smallest fix with the largest
   automation payoff; no API change needed (compose the three reads the
   TTY path already makes).
2. No usage history (§1). No rows-read/written, latency, or storage
   over time, per database or service-wide. Turso's charts answer "what
   changed last week"; `dg` answers "what is true now".
3. No per-database network bytes (§2). Inbound/outbound per database
   does not exist as a counter; queue waits are the only pressure
   signal per database.
4. Mapping-doc correction: the `turso db execute` row (§5). Cosmetic,
   but the doc is the CLI contract's map and currently maps a verb
   that does not exist.

## Out of scope, untouched

Billing, plans, organisations, invoices, subscriptions, auth tokens for
third parties, and any change to `dg` code, templates, or units. This
audit read the Turso web console and `--help` text only; the `turso` CLI
is not authenticated here, no live `show`/`inspect`/`shell` ran against
Turso, and no Turso data was created, modified, or deleted. The
disposable `dg` gateway and its temp dir were removed afterwards.
