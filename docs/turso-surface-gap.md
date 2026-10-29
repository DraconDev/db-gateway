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
| `turso db create --from-file/--from-dump/--from-csv` | `dg import --from <file>` | partial | Only SQLite files. No CSV or SQL-dump import path. |
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
| Overview → Connect → Database URL | `dg status` → `listen`, SSH tunnel | partial | `dg` reports the bound address; it does not mint a public URL because there is none. |
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
| Databases list (rows read/written, bytes synced, storage, group, status) | `dg list`, `dg contents`, TUI databases tab | partial | `dg` has rows read/written, storage, and contents from the files; it has no "bytes synced" (no replication) and no server-imposed per-database status. |
| Analytics (org): quota cards | — | n/a | No quotas to meter against. |
| Analytics (org): Latency / Rows Read / Rows Written / Inbound / Outbound / Syncs / Storage 30-day charts | — | missing | See gap 2. |
| Analytics (org): Top Databases ranked by rows read | TUI databases tab, `dg status` | partial | Present-tense ranking only. |
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
- `.pi/chrome-screenshots/turso-edit-data-live-2026-10-29.png` — the Edit Data
  grid as rendered.
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
