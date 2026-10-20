# dg CLI vs Turso CLI mapping

Goal: Turso-dashboard ergonomics for a personal single-node SQLite
service — CLI primary, browser console for human visibility, no billing.
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
| — | `dg execute <name> <file.sql>` | Runs a SQL FILE, reporting each statement. The file is split by SQLite's own `sqlite3_complete`, so a semicolon inside a string, a comment, or a trigger body is not a statement boundary. The statements run as one atomic batch. |
| — | `dg batch <name> <file>` | Atomic JSON batch from file. No Turso equivalent; kept. |

## Tokens / credentials

| Turso | dg | Notes |
|---|---|---|
| `turso db tokens create <db> ...` | `dg key create <name> --role database_read_only\|database_read_write --database <db>...` | Prints raw key once. |
| `turso db tokens list <db>` | `dg key list` | Name, role, databases. Hashes never shown. |
| `turso db tokens list <db>` | `dg key tokens <db>` | The credentials that can act on ONE database: the owner key reaches everything, an application key reaches the databases it was scoped to. The first question of a leaked-key incident. |
| `turso db tokens revoke ...` | `dg key revoke <name>` | Immediate. |
| `turso auth login/logout/token` | `dg init --data-dir ...` | Local-first: no cloud account. Owner credential lives in a 0600 key file (`--key-file`, `DG_KEY_FILE`, or `~/.config/dg/key`). |
| — | `dg key rotate-owner` | Prints the new owner key once; old dies immediately. |

## Service, backup, console (dg extras)

No Turso equivalent; kept as the single-node operational surface:

- `dg serve` — loopback-only server; remote via SSH tunnel.
- `dg status` — the service dashboard: version, uptime, live and
  tombstoned database counts, request totals with a rate, the error
  rate, the admission queue, and a per-database table.
- `dg watch` — the same dashboard, read once. It exits rather than
  refreshing in place, so it stays pipeable and diffable.
- `dg backup status` — versioned sets + per-db fresh/stale/unknown.
- `dg backup verify --backup-root <dir> [set]` — offline re-verification, JSON report.
- `dg clone/export/import` — SQLite backup-API clone, `.sqlite` snapshot export/import.
- `dg web` — loopback console on `:8787`; prints URL, no auto-open.
  Starts its own gateway automatically when `--server` is unreachable
  (`--data-dir`, default `state/dg`); an already-running gateway is
  used as-is and never touched on shutdown.

## Console trust model

`dg web` binds loopback only (remote access via SSH tunnel, never a
public listener) and holds the owner credential server-side in the
companion process — the browser never receives it. The session cookie
is CSRF defense (SameSite=Strict + loopback origin check), not
authentication: any local process can mint a session by design, so the
console trusts the local machine exactly as the `0600` key file does.
`Secure` is deliberately absent from the cookie (plain HTTP on
loopback; the localhost exception does not cover `127.0.0.1`). No
billing, no metered rows: the usage counters in `dg status` and the
console are visibility only.

## Deliberately out of scope

Per the project brief (no billing, no multi-user fluff):

- `turso org ...` — single owner, no organizations.
- `turso group ...` / locations / replicas — single node, no regions.
- `turso plan` / billing / usage invoices — no metering-for-money.
  Operational row counters exist (`dg status`, console overview) but are
  visibility only, never invoiced.
- `turso db update` (version upgrades, extensions) — deploy is a binary
  swap; see `docs/deployment.md`.
