# Turso vs `dg`: what we would be missing

**Read live 2026-10-29.** This revision replaces the 2026-10-26 inspection
audit with a complete surface comparison. It answers one question: if the
operator moved a workload from Turso to `dg` today, what would actually be
missing?

- **`dg` side**: 0.9.x working tree, including the 2026-10-28 TUI audit
  (`docs/audits/tui-audit.md`). Checked against the live local gateway on
  `127.0.0.1:7432` — 7 databases — and against `dg --help` for every verb.
- **Turso side**: the `dracondev` console read live through the Chrome bridge
  on 2026-10-29, plus the installed `turso` CLI v1.0.15 inventory from
  `--help`. Turso was read only: no database was created, destroyed, imported,
  replicated or re-tokened, and no token was issued or invalidated.
- **Evidence**: the captures listed at the end. The October 2026 captures are
  retained unchanged as history and are marked as such wherever they are
  cited.

The `turso` CLI is **not authenticated** in this environment (`turso auth
whoami` answers "You are not logged in"), so Turso behaviour is evidenced from
the live console plus the CLI's `--help` surface. Where a fact could only come
from a logged-in CLI call, this document says so rather than guessing.

---

## 1. Surface matrix

Status key: **have** = `dg` answers the same question; **partial** = it answers
part of it, or answers it only in one surface; **missing** = no `dg`
counterpart; **n/a** = structurally inapplicable to a single-operator,
single-node box.

### 1.1 Database lifecycle

| Turso surface | `dg` equivalent | Status | Note |
|---|---|---|---|
| `turso db list` | `dg list` | have | Names visible to the credential. |
| `turso db show <db>` | `dg info <db>` | have | Size, tables + row counts, created_at, tombstone state. |
| `turso db inspect <db>` | `dg inspect <db>` | partial | On a terminal it renders database/activity/backup/tables sections; off a terminal it degrades to the plain info document. See gap 1. |
| `turso db inspect --queries` | `dg queries --database <db>` | have | Per-statement cost. `dg` answers it globally or per database, not as a flag on `inspect`. |
| `turso db create` | `dg create` | have | |
| `turso db create --from-file/--from-dump/--from-csv` | `dg import --from <file>` | partial | Only SQLite files. No CSV or SQL-dump import path. See gap 9. |
| `turso db destroy` | `dg delete --confirm <name>` | have | `dg` tombstones then purges after retention; requires the name typed twice. |
| `turso db update` | — | n/a | Deploy is a binary swap; `docs/deployment.md`. |
| `turso db unarchive` / `wakeup` | — | n/a | No archival tier; local files are always present. |
| `turso db locations` | — | n/a | Single node. |
| `turso db replicate` | — | n/a | No regions or read replicas. Recovery is `dg restore` + `dg backup verify`. |
| `turso db config delete-protection` | — | missing | Nothing prevents `dg delete` beyond the typed confirmation. See gap 7. |
| `turso db config attach` | — | n/a | No cross-database ATTACH; each database is its own file behind its own route. |
| `turso db export` | `dg export --out <file>` | have | Atomically published `.sqlite` snapshot. |
| `turso db import` | `dg import --from <file>` | have | 128 MiB ceiling. |
| `turso db tokens create/list/invalidate` | `dg key create/list/revoke` | partial | One-at-a-time revoke only; no per-database bulk invalidation. See gap 6. |

### 1.2 Database page (console per-database tabs)

Console tabs: **Overview**, **Branches**, **Analytics**, **Top Queries**,
**Edit Data**.

| Turso surface | `dg` equivalent | Status | Note |
|---|---|---|---|
| Overview → Activity (rows read/written, storage, syncs) | `dg inspect`, `dg status`, TUI service/databases tabs | have | `dg` adds queue-wait percentiles Turso does not expose. |
| Overview → Connect → Database URL | `dg status` → `listen`, SSH tunnel | n/a | `dg` reports the bound loopback address; there is no public URL to mint. Remote access is an SSH tunnel (§4). |
| Overview → Create Token | `dg key create --role … --database …` | have | |
| Overview → Export → Download SQLite File | `dg export --out` | have | |
| Overview → Block Reads / Block Writes | — | missing | No per-database read/write kill switch. See gap 5. |
| Overview → Restrict Access (CIDR / VPC allow-list) | — | missing | Loopback-only listener; remote access is an SSH tunnel. See gap 5. |
| Overview → Invalidate All Tokens | `dg key revoke <name>` (one at a time) | partial | See gap 6. |
| Branches → Create From Now | `dg branch create <parent> <name>` | have | |
| Branches → Create From Point-in-Time | `dg branch create --from <ISO8601>` | have | Reports the backup window when the moment falls outside it. |
| Branches → list | `dg branch list`, `dg branch show` | have | |
| Analytics (per-database, 30-day series) | — | missing | See gap 2. |
| Top Queries (per-database) | `dg queries --database <db>` | partial | Same ranking facts; `dg` shows statement SHAPES, Turso shows literal text. See gap 3. |
| Edit Data → DATA grid | `dg rows`, TUI rows browser | partial | Read-only; no cell editing, no Add record. See gap 4. |
| Edit Data → Filters / Sort / Columns | `dg rows --where/--sort`, TUI `f`/`o`/`c` | partial | Capability parity, ergonomics differ: flags and typed conditions instead of builders. See gap 4. |
| Edit Data → STRUCTURE | `dg schema <db> [table]` | have | Columns with declared types and flags, indexes, foreign keys, `CREATE` text. |
| Edit Data → SQL console | `dg shell`, `dg query` | have | Different transport (authenticated HTTP API vs direct console), same capability. |

### 1.3 Organisation-level console pages

| Turso surface | `dg` equivalent | Status | Note |
|---|---|---|---|
| Databases list (rows read/written, bytes synced, storage, group, status) | `dg list`, `dg contents`, TUI databases tab | partial | `dg` has rows read/written, storage, and contents from the files. "Bytes synced" is replication accounting (§4) and the per-database status is a provider-imposed write block, both inapplicable; the storage figure and counters are order-of-magnitude differences only, not gaps. See gap 2 for the missing trend. |
| Analytics (org): quota cards | — | n/a | No quotas to meter against. |
| Analytics (org): Latency / Rows Read / Rows Written / Inbound / Outbound / Syncs / Storage 30-day charts | — | missing | See gap 2. |
| Analytics (org): Top Databases ranked by rows read | TUI databases tab, `dg status` | partial | Present-tense ranking only; no trend. See gap 2. |
| Analytics (org): Top Queries ranked by rows read | `dg queries` | partial | See gap 3. |
| Audit Logs (Author, Event, Origin, Timestamp) | `dg audit` | have | `dg` adds the action, the database, and the statement shape. Different retention bounds — see gap 8. |
| Billing / Usage / Upgrade / plan / invoices | — | n/a | No billing. |
| Settings, Support | — | n/a | No account. |

### 1.4 Service, backup and terminal surface (`dg` only)

No Turso equivalent. This is the largest area where `dg` leads rather than
lags; see §3.

| `dg` surface | What it answers |
|---|---|
| `dg serve` | Loopback-only authenticated server; remote via SSH tunnel. |
| `dg status`, `dg watch` | Version, uptime, admission config, request totals/rate, error counts, wait percentiles, per-database traffic. |
| `dg tui` | The six-tab operator view (databases, service, backups, keys, queries, locations) plus the database page and rows browser. |
| `dg backup run/status/digest/verify` | Backup freshness, the three-state `fresh`/`behind`/`stale` judgement against an approved cadence, offline manifest/hash-chain verification. |
| `dg restore list`, `dg restore plan` | The point-in-time window the off-host replica covers, and what a restore to a moment would reach. |
| `dg vacuum`, `dg integrity`, `dg explain`, `dg diff` | Space reclamation, file soundness, query plan, and cross-database comparison. |
| `dg config list/show/set/remove/path` | Named contexts (server + key file paths). |
| `dg mcp` | The gateway served to an MCP client over stdio. |
| `dg execute`, `dg batch` | A SQL file as one atomic batch (32-statement cap); an atomic JSON batch file. |

### 1.5 CLI verbs with no `dg` counterpart

| Turso verb | Why `dg` has none |
|---|---|
| `turso auth login/logout/signup/token/whoami/api-tokens` | No cloud account. Identity is the local owner key file. |
| `turso group …` (create/list/show/rename/destroy/transfer/update/locations/tokens/config/aws-migration/unarchive) | No groups; no regions. |
| `turso org …` (create/list/members/invites/invoice/jwks/audit-logs/switch/db-transfer/destroy) | Single owner, no organisations. |
| `turso plan show` | No plan. |
| `turso dev` | `dg serve` already runs locally against a real data directory. |
| `turso quickstart`, `turso update`, `turso relax` | Onboarding/upgrade/joke verbs with no local meaning. |

---

## 2. Ranked gaps (operator impact first)

Ranked by what it would cost an operator in the first week. Each gap names the
evidence, and whether it is fixable inside the current architecture.

### Gap 1 — `dg inspect` still degrades off a terminal (high; smallest fix)

`dg inspect` renders a rich database/activity/backup/tables view **only when
stdout is a terminal**. Piped or `--json`, it returns the plain info document
and drops the activity and backup sections entirely.

Reproduced live on 2026-10-29 against the local gateway:

```
$ dg inspect --json inventory
{"created_at":1792931768,"name":"inventory","size_bytes":90240,
 "tables":[{"name":"_litestream_lock","rows":0}, … ],
 "tombstoned":false}
```

That is the `GET /v1/databases/{name}` document — no backup state, no traffic.
A script that wants what the operator sees must rejoin `/v1/databases/{name}`,
`/v1/status` and `/v1/backups` by hand.

**Carried forward unchanged from the 2026-10-26 audit, and still true.** Cost:
automation cannot read the operator's own view. Fix: compose the three reads the
TTY path already makes into the non-TTY answer. No API change needed.

### Gap 2 — No usage history, only present-tense counters (high)

Turso's Analytics is 30-day time series for latency (with p50–p99 selectors),
rows read, rows written, inbound and outbound traffic, syncs and storage —
per organisation and per database.

`dg` has none, by explicit design. `src/store/metrics.rs` keeps a **60-second
ring** (`WINDOW_BUCKETS = 60`), and its own comment says: "This is deliberately
a WINDOW, not a time series … It answers 'how busy was it just now' without
becoming a metrics database." `dg queries` and `dg audit` are bounded logs of
individual events, not aggregates over time.

Cost: "what changed last week" is unanswerable. You can say what the box is
doing now and which statements were expensive, but not that writes tripled on
Tuesday. Fix: a rolled-up per-hour counter series on disk. That is a storage
and retention decision, not a view change — the largest architectural gap here.

### Gap 3 — Top Queries shows literal values in Turso and shapes in `dg` (medium; `dg` is better, not worse)

Read live: Turso's Top Queries for `dracon-music-dev` shows statements with
literals intact, e.g. `WHERE status='settled'` and
`WHERE idempotency_key LIKE 'aig-session-%'`.

`dg queries` deliberately replaces every literal with `?`, showing the SHAPE:
`SELECT * FROM "products" ORDER BY rowid LIMIT ? OFFSET ?`.

This is **not a gap to close** — it is a privacy property the operator gains by
owning the box (`docs/tui.md`, "Which query is heavy"). Recorded here because a
comparison that lists only Turso's wins would be dishonest in the other
direction.

One real caveat found live: Turso's list includes the **console's own
introspection queries** (the first entry on `dracon-music-dev` was Drizzle
Studio's `sqlite_master`/`pragma_index_list` scan), so its "top query" is
measured over a different population than `dg`'s API traffic.

### Gap 4 — Edit Data is read-write; `dg` is read-only by design (medium; deliberate)

The Edit Data grid is a full editor: `+ Add record`, editable cells, a Columns
visibility chooser, a Sort builder, a Filters builder, and an attached SQL
console. Read live through its shadow DOM (the page is a Drizzle Studio app
inside `<drizzle-studio>`, so it is invisible to `document.body.innerText`).

`dg rows` and the TUI rows browser give the same three reading capabilities —
sort, filter, column data — as `--sort`/`--where` flags and `o`/`f`/`c` prompts.
They never write. Every mutation in `dg` is an explicit statement (`dg query`,
`dg execute`) or a confirmed lifecycle action.

Cost: an operator cannot click a cell and fix a typo. That is intentional —
`docs/tui.md` states the mouse "deliberately stops at reading", and every write
needs its own statement so it lands in the audit log with its shape. Recorded
as a deliberate non-goal, not a missing feature.

### Gap 5 — No per-database network access control (medium)

Turso's per-database Configuration offers **Block Reads**, **Block Writes**,
**Allow ATTACH**, **Delete Protection**, and **Restrict Access** by CIDR block
or AWS VPC endpoint. All are live controls, read on 2026-10-29.

`dg` has none of these. Its model is: one loopback listener, remote access via
SSH tunnel, and authorization by credential scope and role. There is no
per-database switch to refuse writes, and no per-database IP allow-list.

Cost: during an incident you cannot say "this one database is now read-only" —
you can only revoke credentials or stop the service. Fix: a per-database
read-only flag in the registry, enforced at the route. In-architecture.

### Gap 6 — No bulk credential invalidation per database (medium)

Turso offers "Invalidate All Tokens" for one database — the first move in a
leaked-key incident.

`dg key revoke` takes exactly one `<NAME>` (verified from `--help` and
`src/api.rs::revoke`). `dg key list` shows every key and its scope, so an
operator can find the affected keys, but must revoke them one command at a time.

Cost: a leaked database token on a database with many scoped keys is a manual
loop under time pressure. Fix: `dg key revoke --database <name>` with a
confirmation matching the existing destructive-action rule. In-architecture.

### Gap 7 — No delete protection (low–medium)

Turso's Delete Protection prevents a database from being deleted at all. `dg`
has only the typed `--confirm <name>` (and the TUI's typed confirmation), which
protects against a fat-fingered command but not against a deliberate one.

Cost: low — the confirmation is strong — but a "do not delete this" flag is
cheap insurance for a database you cannot lose. Fix: a registry flag checked by
the delete route. In-architecture.

### Gap 8 — Audit retention differs, and Turso's is time-based (low; `dg` is better)

Read live on 2026-10-29: Turso's Audit Logs page reports **"Showing 1-6 of 6
items"** with all paging buttons disabled. The 2026-10-26 capture of the same
page listed **23 entries**, including 21 CLI-created tokens from Oct 24. Those
five-day-old entries are gone, and the console does not say it dropped them.

`dg audit` is bounded differently: `src/store/audit.rs` keeps `MAX_ENTRIES` =
2000 entries and rewrites its trail file above `MAX_FILE_BYTES` = 4 MiB,
dropping the oldest. That is a **count/size** bound, not a time bound, and the
trail is durable across restarts (`audit.jsonl` in the data directory).

So: on a quiet box `dg` keeps its whole trail indefinitely; on a busy one it
turns over by count. Either way it does not silently expire entries by age the
way the console just did. Not a gap to close — a property to know.

### Gap 9 — No CSV or SQL-dump import (low)

`turso db create` accepts `--from-csv`, `--from-dump`, `--from-dump-url` and
`--from-file`. `dg import --from` accepts a regular SQLite file only, with a
128 MiB ceiling. Cost: a CSV or dump has to be converted locally first. Fix:
small, but outside the "database file" model `dg` has chosen.

### Gap 10 — Documentation drift in the mapping doc (low, but it is the contract's map)

Two errors the October audit found are **still present**:

- `docs/cli-turso-mapping.md:25` maps `turso db execute <name> "<sql>"`, but the
  installed `turso` v1.0.15 has no `db execute` verb — the command falls
  through to `turso db --help`. Verified 2026-10-29.
- The same doc's credential table describes the owner-rotation verb without
  noting that `dg key --help` spells it `rotate-owner`.

Not fixed here: this goal is a comparison, not a docs-edit pass on a different
document. Recorded so the next mapping-doc revision picks them up.

---

## 3. Where `dg` is ahead

Read from the live local gateway and the code, not asserted.

| Advantage | Evidence |
|---|---|
| **No quotas, no throttling, no invoice.** | The console shows "Quota Critical 99% used … your account may be throttled soon" and two databases in **Writes Blocked** status on every page. `dg` has no metering path at all. |
| **The operator owns the audit trail.** | Turso dropped five-day-old audit entries by age (gap 8). `dg audit` is a 2000-entry durable JSONL file on the operator's disk, with action, database and statement shape. |
| **Statement shapes, never literal values.** | Turso's Top Queries prints `LIKE 'aig-session-%'` and `status='settled'`; `dg queries` prints `LIKE ?` and `status = ?`. |
| **Backups are first-class and verifiable offline.** | `dg backup status/digest/verify` judge each database `fresh`/`behind`/`stale` against an approved cadence, and `dg backup verify` re-checks a published set's manifest and hash chain with the gateway stopped. Turso's console offers download and point-in-time branches, not an offline verifier. |
| **Restore window is inspectable and planned before it is taken.** | `dg restore list` publishes the window (`oldest` → `newest`) per database from the live replica; `dg restore plan --to <ISO>` says what a restore would reach. |
| **Local SQLite tooling on the same box.** | `dg vacuum`, `dg integrity`, `dg explain`, `dg diff` — no counterpart in the console. |
| **Credentials are files, not bearer tokens in a cloud account.** | `dg key list/create/revoke/rotate-owner` against a 0600 key file; no `turso auth` surface to compromise. |
| **MCP server built in.** | `dg mcp` serves the gateway to an MCP client over stdio. No Turso equivalent. |
| **Admission control is visible.** | `dg status` and the TUI service tab publish read/write permits, acquire timeout, and per-database p95/max admission waits. The console publishes no such queue facts. |
| **One atomic batch verb.** | `dg execute` runs a SQL file as one transaction (split by SQLite's own `sqlite3_complete`, 32-statement cap) and `dg batch` runs an atomic JSON batch. |

---

## 4. Not applicable by architecture

Stated so a parity table cannot be mistaken for a to-do list.

- **Multi-region replication, locations, groups (`turso group …`, `turso db
  replicate`, `turso db locations`).** Single node by design. Durability is
  `dg backup` + the off-host replica, and recovery is `dg restore`.
- **Organisations, members, invites, plan, billing, invoices, usage quotas,
  `turso org …`, `turso plan`, `turso auth …`.** One operator, no tenancy, no
  metering-for-money.
- **Token signing infrastructure (`turso org jwks`, API tokens).** Credentials
  are hashed rows in the local registry, not signed JWTs.
- **`turso dev`, `turso quickstart`, `turso update`, `turso relax`.** The local
  binary already runs a real server against a real data directory.
- **Archival tiers (`turso db unarchive`).** Files are always present on disk.
- **Cross-database ATTACH.** Each database is an isolated file behind its own
  route pair; there is no shared connection to attach from.

---

## Evidence

Live captures made 2026-10-29 (this revision):

- `docs/evidence/turso-live-2026-10-29-databases.txt` — the Databases list,
  all 19 rows with rows read/written, bytes synced, storage, group, status.
- `docs/evidence/turso-live-2026-10-29-analytics.txt` — the org Analytics page:
  quota cards, the seven 30-day chart families, Top Databases, Top Queries.
- `docs/evidence/turso-live-2026-10-29-per-database.txt` — the per-database tab
  set, Overview/Configuration controls, Branches, per-database Analytics and
  Top Queries.
- `docs/evidence/turso-live-2026-10-29-edit-data.txt` — the Edit Data browser
  read through its shadow DOM: toolbar, type-annotated headers, sampled rows.
- `docs/evidence/turso-edit-data-live-2026-10-29.png` — the Edit Data grid as
  rendered (screenshot of the same view the text record describes).
- `docs/evidence/turso-live-2026-10-29-audit-logs.txt` — the Audit Logs page and
  the retention finding (6 items today vs 23 on 2026-10-26).

Historical captures, retained unchanged and **not** re-read this revision:

- `docs/evidence/turso-analytics-2026-10-26.txt`
- `docs/evidence/turso-databases-2026-10-26.txt`
- `docs/evidence/turso-audit-logs-2026-10-26.txt`
- `docs/evidence/turso-edit-data-2026-10-26.png`
- `docs/evidence/turso-edit-data-grid-2026-10-27.png`
- `docs/evidence/turso-structure-2026-10-27.txt`
- `docs/evidence/turso-structure-view-2026-10-27.png`
- `docs/evidence/turso-structure-indexes-2026-10-27.png`

`dg`-side evidence: `docs/evidence/rows-browser-live-2026-10-27.txt`,
`docs/evidence/schema-live-2026-10-27.txt`, `docs/audits/tui-audit.md`, and the
live local gateway (`GET /v1/status`, `/v1/databases`, `/v1/backups`, `/v1/audit`,
`/v1/queries`) read on 2026-10-29.

Turso CLI surface: `turso` v1.0.15, its `--help` output for every verb family
inventoried in §1.5. The CLI is not authenticated here, so no live
`show`/`inspect`/`shell` ran against Turso and no Turso data was read through
it.

## How to re-read this

The Turso side is a moving target: the console changed its audit retention
between 2026-10-26 and 2026-10-29. Re-read the console before relying on a gap
being open, and re-run the `dg` side against a current build. Two of the ten
gaps (1 and 10) are carried forward from October *because they were re-verified
today* — do not carry a gap forward again without re-checking it.

## Related

- `docs/cli-turso-mapping.md` — the per-verb mapping table (see gap 10).
- `docs/tui.md` — the terminal view this comparison's `dg` column describes.
- `docs/audits/tui-audit.md` — the 2026-10-28 TUI audit.
- `docs/recovery-contract.md`, `docs/recovery-policy.md` — the backup and
  restore model that replaces Turso's replication and PITR.

---

## Appendix: the 2026-10-26 revision (historical)

The body below is the previous revision of this document, kept verbatim so the
earlier inspection work is not lost. It is **historical**: its ranked gaps have
been superseded by §2 above, its October figures are superseded by the live
2026-10-29 captures, and its claim that the console listed 23 audit entries is
now known to have been a snapshot before time-based expiry. Read §1–§4 for
anything current.

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

- **A paged, sortable, filterable row browser now exists** (added
  2026-10-27): `dg rows` and the TUI's rows browser read a table a page
  of ten at a time, sort by any real column and filter with a read-only
  condition, with the footer counting the filtered set. What is still
  absent from that browser: the `Columns` picker (showing a chosen subset
  of columns), per-row checkboxes, and `+ Add record`.
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
