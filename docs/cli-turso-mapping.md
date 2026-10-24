# dg CLI vs Turso CLI mapping

Goal: Turso-dashboard ergonomics for a personal single-node SQLite
service — the CLI and its terminal view are the operator's surface, no
billing.
`dg` keeps its flat command layout (no `dg db ...` regroup); this doc
maps each Turso CLI verb to its `dg` equivalent.

## Database lifecycle

| Turso | dg | Notes |
|---|---|---|
| `turso db list` | `dg list` | Names visible to the credential; owner sees all. |
| `turso db create <name>` | `dg create <name>` | Owner key only. |
| `turso db show <name>` | `dg info <name>` | Size, tables + row counts, created_at, tombstone state. Owner only. |
| `turso db destroy <name>` | `dg delete <name> --confirm <name>` | Tombstones; file purged after retention. Double-type confirm. |
| `turso db inspect <name>` | `dg inspect <name>` | What one database holds AND what it has been doing: size, tables, rows, traffic, queue waits, last write, and the freshness of its backup. `dg info` is what it IS; `dg inspect` is what it has been DOING. |

## SQL access

| Turso | dg | Notes |
|---|---|---|
| `turso db shell <name>` | `dg shell [name]` | Optional preselect; `use <db>` switches inside. Piped stdin works. The operator's dot-commands (`.status`, `.keys`, `.backups`, `.tables`, `.schema`) draw the same views as the one-shot commands. |
| `turso db execute <name> "<sql>"` | `dg query <name> "<sql>"` | Interactive terminals get a table (numbers right-aligned, `NULL` distinct, wide characters measured), else JSON. `--args`, `--idempotency-key`. |
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
| — | `dg vacuum <name>` | Reclaims the space a file is wasting, under the same write fence every other writer takes, and reports the bytes it actually reclaimed. |
| — | `dg explain <name> "<sql>"` | What SQLite would do with a statement: its query plan, in plain words, with no invented cost model. |

## Where the gateway is

| Turso | dg | Notes |
|---|---|---|
| `turso auth login/logout/token` | `dg config set <name> --server <url> --key-file <path>` | Named contexts: a name for a server and a credential FILE, kept in `$HOME/.config/dg/config.toml` (0600). The file holds paths, never a key. `dg --context <name> list` talks to that one now. |
| — | `dg mcp` | The gateway's read views to an MCP client over stdio: the same facts an agent would otherwise have to scrape, and only the read ones. |

## Tokens / credentials

| Turso | dg | Notes |
|---|---|---|
| `turso db tokens create <db> ...` | `dg key create <name> --role database_read_only\|database_read_write --database <db>...` | Prints raw key once. |
| `turso db tokens list <db>` | `dg key list` | Name, role, databases. Hashes never shown. |
| `turso db tokens list <db>` | `dg key tokens <db>` | The credentials that can act on ONE database: the owner key reaches everything, an application key reaches the databases it was scoped to. The first question of a leaked-key incident. |
| `turso db tokens revoke ...` | `dg key revoke <name>` | Immediate. |
| `turso auth login/logout/token` | `dg init --data-dir ...` | Local-first: no cloud account. Owner credential lives in a 0600 key file (`--key-file`, `DG_KEY_FILE`, or `~/.config/dg/key`). |
| — | `dg key rotate-owner` | Prints the new owner key once; old dies immediately. |

## Service, backup, terminal view (dg extras)

No Turso equivalent; kept as the single-node operational surface:

- `dg serve` — loopback-only server; remote via SSH tunnel.
- `dg status` — the service dashboard: version, uptime, live and
  tombstoned database counts, request totals with a rate, the error
  rate, the admission queue, and a per-database table. The table's
  `wait p95` is each database's OWN percentile of its own recent
  admission waits; `wait max` is its worst single wait. The two are
  different facts and are labelled as such.
- `dg watch` — the same dashboard, read once. It exits rather than
  refreshing in place, so it stays pipeable and diffable.
- `dg backup status` — versioned sets + per-db fresh/stale/unknown.
- `dg backup verify --backup-root <dir> [set]` — offline re-verification, JSON report.
- `dg clone/export/import` — SQLite backup-API clone, `.sqlite` snapshot export/import.
- `dg tui` — the terminal operator view: the service, its databases,
  their backup freshness and its credentials on one live screen, with
  the lifecycle, credential and SQL actions a keystroke away.
  Talks to the same gateway over the same API as every other command.

## Terminal view trust model

`dg tui` runs on the operator's machine and reaches the gateway
through the same client as every other command, so the transport rules
are the ones above: a literal loopback origin or HTTPS, never a public
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
