# dg CLI vs Turso CLI mapping

Goal: Turso-dashboard ergonomics for a personal single-node SQLite
service — the CLI and its terminal view are the operator's surface, no
billing.
`dg` keeps its flat command layout (no `dg db ...` regroup); this doc
maps each Turso CLI verb to its `dg` equivalent.

> **Current comparison:** this table is the per-verb map. The full surface
> comparison — every Turso CLI verb, console page and feature, with each gap
> ranked by operator impact — is `docs/turso-surface-gap.md`, last read live
> against the Turso console on 2026-10-29. Read that for what is missing; read
> this for which `dg` command answers which Turso verb.

## Database lifecycle

| Turso | dg | Notes |
|---|---|---|
| `turso db list` | `dg list` | Names visible to the credential; owner sees all. |
| `turso db create <name>` | `dg create <name>` | Owner key only. |
| `turso db show <name>` | `dg info <name>` | Size, tables + row counts, created_at, tombstone state. Owner only. |
| `turso db destroy <name>` | `dg delete <name> --confirm <name>` | Tombstones; file purged after retention. Double-type confirm. |
| `turso db config delete-protection [--off]` | `dg protect <name> [--off]` | A durable registry guard. `dg delete` refuses a protected database with `database_protected` before anything is written, so the guard stops the mistake it exists for: a stray delete on the wrong name. `dg inspect` reports `protected`, so the guard is visible before someone tries the delete it would refuse. Gap 5 CLOSED in 0.10.0. |
| — (console: "Restore deleted databases") | `dg undelete <name>` | The undo half of `delete`. Clears the tombstone while the published retention window is open and the original file still exists. Refusals are distinct: `not_deleted` (nothing to undo), `retention_expired` (window closed), `database_file_missing`, `database_purged` (already unlinked), `database_not_found` (never existed). It cannot resurrect a name a live database has since taken. Gap 5 CLOSED in 0.10.0. The console gives you a *list* of deleted databases to pick from; `dg` has no such list, so you must already know the name. `dg status` reports how many are tombstoned but not which, and `dg list` shows live names only. See `docs/turso-surface-gap.md` gap 8. |
| `turso db inspect <name>` | `dg inspect <name>` | What one database holds AND what it has been doing: size, tables, rows, traffic, queue waits, last write, and the freshness of its backup. `dg info` is what it IS; `dg inspect` is what it has been DOING. |
| Edit Data → STRUCTURE | `dg schema <name> [table]` | What a table is MADE OF: each column's declared type, its not-null and primary-key flags, the table's indexes and foreign keys, and its `CREATE` statement. Read from the gateway's own SQLite file with SQLite's own PRAGMAs, so the answer is the schema's own words, not a parsed guess. Read-only, and the gateway's JSON document is what a pipe reads. |

## SQL access

| Turso | dg | Notes |
|---|---|---|
| `turso db shell <name>` | `dg shell [name]` | Optional preselect; `use <db>` switches inside. Piped stdin works. The operator's dot-commands (`.status`, `.keys`, `.backups`, `.tables`, `.schema`) draw the same views as the one-shot commands. |
| — (no `turso db execute` verb in CLI v1.0.15; the equivalent is `turso db shell <name> "<sql>"`) | `dg query <name> "<sql>"` | Interactive terminals get a table (numbers right-aligned, `NULL` distinct, wide characters measured), else JSON. `--args`, `--idempotency-key`. |
| — | `dg rows <name> <table> [page]` | A table's ROWS, a page of ten at a time, with `--sort <column>[:asc\|:desc]` and `--where <condition>`. The sorting and filtering are applied by the gateway, so the footer's `rows 11-20 of N` counts the filtered set. Both controls are validated against the schema before they reach a statement. |
| — | `dg contents` | How much each database HOLDS: its tables and total rows, read from the files. `dg list` says which databases exist and the counters say what they have been doing since boot; neither says how much data is in them. |
| — | `dg execute <name> <file.sql>` | Runs a SQL FILE, reporting each statement. The file is split by SQLite's own `sqlite3_complete`, so a semicolon inside a string, a comment, or a trigger body is not a statement boundary. The statements run as one atomic batch. A file of more than 32 statements is refused before it is sent, with the count, the cap and the fact that nothing was sent — a longer file is not split, because that would give up the atomicity. |
| — | `dg batch <name> <file>` | Atomic JSON batch from file. No Turso equivalent; kept. |

## Seeing what the service did

| Turso | dg | Notes |
|---|---|---|
| dashboard "Top Queries" | `dg queries` | The statement shapes the service has run, per database: how often, the rows read and written, the total and the slowest time, and the credentials that ran it. A statement is kept as its SHAPE — its SQL with every literal replaced by a placeholder — so the answer never quotes the data. Turso shows this per tenant because it has to; here the gateway is the only thing between the operator and SQLite, so it can. |
| dashboard "Top Queries" | `dg queries --database <name>` | The same answer for one database. |
| dashboard "Audit Logs" | `dg audit` | Which credential ran which action, and when: statements, key issuance and revocation, creates, clones, imports, deletes. Actions and shapes, never values and never a key. |
| dashboard "Audit Logs" | `dg audit --principal <name> --since <epoch>` | One credential's actions, or what happened after a moment. |
| — | `dg diff <a> <b>` | What differs between two databases, compared on the gateway so no table data travels to answer a question about the data. Exits nonzero when they differ. No Turso equivalent; kept. |

## Forks, copies and maintenance

| Turso | dg | Notes |
|---|---|---|
| dashboard "Branches" (Create From Now) | `dg branch create <parent> <name>` | A fork that can diverge from the database it came from. The copy is a real, writable database in the store, and the parent and the moment are recorded durably. |
| dashboard "Branches" (Create From Point-in-Time) | `dg branch create <parent> <name> --from <ISO8601>` | The same, at a moment inside the backup window. A moment before the oldest set is refused, and the window that IS available is named. |
| dashboard "Branches" | `dg branch list`, `dg branch show <name>`, `dg branch delete <name> --confirm <name>` | The forks, where each came from, and their removal. |
| dashboard "Overview" | `dg integrity <name>` | SQLite's own integrity check and foreign-key check, on a read-only connection, reported verbatim. It never repairs anything. |
| dashboard "Overview" | `dg backup run` | Take a backup set now, from inside the gateway: the same set the periodic producer writes, published atomically and under the producer's own lock. |
| — | `dg vacuum <name>` | Reclaims the space a file is wasting, under the same write fence every other writer takes, and reports the bytes it actually reclaimed. |
| — | `dg explain <name> "<sql>"` | What SQLite would do with a statement: its query plan, in plain words, with no invented cost model. |

## Where the gateway is

| Turso | dg | Notes |
|---|---|---|
| `turso auth login/logout/token/whoami` | — | No direct equivalent: `dg init` creates a local owner key and `dg config set` stores named server/key-file contexts; `dg` has no cloud-account login. |
| — | `dg mcp` | The gateway's read views to an MCP client over stdio: the same facts an agent would otherwise have to scrape, and only the read ones. |

## Tokens / credentials

| Turso | dg | Notes |
|---|---|---|
| `turso db tokens create <db> ...` | `dg key create <name> --role database_read_only\|database_read_write --database <db>...` | Both print the raw key once. Turso also supports expiry, table-action permissions, ATTACH and group claims; `dg` grants non-expiring database-wide roles. These provider-governance controls are not-applicable for the single operator (§4.1 in `docs/turso-surface-gap.md`). |
| — (no `turso db tokens list` verb in CLI v1.0.15) | `dg key list` | Lists key names, roles and database scopes; hashes never shown. Turso's CLI help exposes create/invalidate only. |
| — (no Turso token-list command) | `dg key tokens <db>` | The credentials that can act on ONE database: the owner key reaches everything, an application key reaches the databases it was scoped to. The first question of a leaked-key incident. |
| `turso db tokens invalidate <db>` | `dg key revoke --database <db> --confirm <db>` | Cuts all application credentials scoped to one database in a single transaction; the owner key is explicitly excluded and the JSON response says `owner_key_excluded: true`. Gap 4 CLOSED in 0.10.0. `dg key revoke <name>` remains the one-credential form. |
| `turso auth login/logout/token` | `dg init --data-dir ...` | Local-first: no cloud account. Owner credential lives in a 0600 key file (`--key-file`, `DG_KEY_FILE`, or `~/.config/dg/key`). |
| — | `dg key rotate-owner` | Prints the new owner key once; old dies immediately. Turso's `db tokens invalidate` rotates database-token signing keys, not the local owner credential. |

## Service, backup, terminal view (dg extras)

No Turso equivalent; kept as the single-node operational surface:

- `dg serve` — local-only server; remote via private transport.
- `dg status` — the service dashboard: version, uptime, live and
  tombstoned database counts, request totals with a rate, the error
  rate, the admission queue, and a per-database table. The table's
  `wait p95` is each database's OWN percentile of its own recent
  admission waits; `wait max` is its worst single wait. The two are
  different facts and are labelled as such.
- `dg watch` — the same dashboard, read once. It exits rather than
  refreshing in place, so it stays pipeable and diffable.
- `dg backup status` — versioned sets + per-db fresh/stale/unknown, and the
  three fields that answer "are we doing the bucket backup?": `replica` (the
  off-host target, endpoint, retention, snapshot interval and whether the
  copying sidecar is actually running), `drill` (the last bucket restore,
  restored and compared against the live gateway) and `restore_points` (the
  moments the replica can be taken back to). The committed 2026-10-29 console
  capture shows Turso's per-database *branch from a point in time* and its
  **Export Database → Download SQLite File**, but it does not show a console
  page reporting replica health or a restore window; whether Turso exposes one
  is unverified here, so this row claims no comparison for it. What `dg` does
  have is one command answering all three for the whole host. See
  `docs/recovery-policy.md` for the observed state.
- `dg backup verify --backup-root <dir> [set]` — offline re-verification, JSON report.
- `dg clone/export/import` — SQLite backup-API clone, `.sqlite` snapshot export/import.
  `dg export --out` refuses an existing file and has no WAL or metadata option,
  where Turso's export also writes `<db>.db-wal`, accepts `--overwrite` and
  `--with-metadata`; see gap 7 in `docs/turso-surface-gap.md`.
- `dg tui` — the terminal operator view: the service, its databases,
  their backup freshness and its credentials on one live screen, with
  the lifecycle, credential and SQL actions a keystroke away.
  Talks to the same gateway over the same API as every other command.

## Terminal view trust model

`dg tui` runs on the operator's machine and reaches the gateway
through the same client as every other command, so the transport rules
are the ones above: a literal local origin or HTTPS, never a public
listener. The credential is read from the `0600` key file into memory
and is never drawn — the one key the view shows is a newly issued one,
which the gateway shows once and never again.

The view takes the terminal over (raw mode and the alternate screen)
and always gives it back, including when it panics, so the operator's
shell survives it. No billing, no metered rows: the counters in the
view are visibility only, exactly as they are in `dg status`.

## Deliberately out of scope

Per the project brief (no billing, no multi-user fluff):

- `turso org ...` — single owner, no organizations.
- `turso group ...` / locations / replicas — single node, no regions.
- `turso plan` / billing / usage invoices — no metering-for-money.
  Operational row counters exist (`dg status`, `dg watch` and the
  per-database table in `dg tui`) but are visibility only, never invoiced.
- `turso db update` (version upgrades, extensions) — deploy is a binary
  swap; see `docs/deployment.md`.
