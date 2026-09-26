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
| `turso db inspect <name>` | `dg info <name>` + `dg status` | Facts from `info`; per-db `rows_read`/`rows_written` from `status`. |

## SQL access

| Turso | dg | Notes |
|---|---|---|
| `turso db shell <name>` | `dg shell [name]` | Optional preselect; `use <db>` switches inside. Piped stdin works. |
| `turso db execute <name> "<sql>"` | `dg query <name> "<sql>"` | Interactive terminals get an aligned table, else JSON. `--args`, `--idempotency-key`. |
| — | `dg batch <name> <file>` | Atomic JSON batch from file. No Turso equivalent; kept. |

## Tokens / credentials

| Turso | dg | Notes |
|---|---|---|
| `turso db tokens create <db> ...` | `dg key create <name> --role database_read_only\|database_read_write --database <db>...` | Prints raw key once. |
| `turso db tokens list <db>` | `dg key list` | Name, role, databases. Hashes never shown. |
| `turso db tokens revoke ...` | `dg key revoke <name>` | Immediate. |
| `turso auth login/logout/token` | `dg init --data-dir ...` | Local-first: no cloud account. Owner credential lives in a 0600 key file (`--key-file`, `DG_KEY_FILE`, or `~/.config/dg/key`). |
| — | `dg key rotate-owner` | Prints the new owner key once; old dies immediately. |

## Service, backup, console (dg extras)

No Turso equivalent; kept as the single-node operational surface:

- `dg serve` — loopback-only server; remote via SSH tunnel.
- `dg status` — version, uptime, admission config, request counters, per-db usage.
- `dg backup status` — versioned sets + per-db fresh/stale/unknown.
- `dg backup verify --backup-root <dir> [set]` — offline re-verification, JSON report.
- `dg clone/export/import` — SQLite backup-API clone, `.sqlite` snapshot export/import.
- `dg web` — loopback console on `:8787`; prints URL, no auto-open.

## Deliberately out of scope

Per the project brief (no billing, no multi-user fluff):

- `turso org ...` — single owner, no organizations.
- `turso group ...` / locations / replicas — single node, no regions.
- `turso plan` / billing / usage invoices — no metering-for-money.
  Operational row counters exist (`dg status`, console overview) but are
  visibility only, never invoiced.
- `turso db update` (version upgrades, extensions) — deploy is a binary
  swap; see `docs/deployment.md`.
