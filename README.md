# DB Gateway

**Canonical roadmap:** [`docs/personal-turso-megaplan.md`](docs/personal-turso-megaplan.md)
**Current implementation checklist:** [`TASKS.md`](TASKS.md)
**Documentation index:** [`docs/README.md`](docs/README.md) — canonical vs historical-evidence vs superseded, for the whole docs tree.
**How we work:** [`CONTRIBUTING.md`](CONTRIBUTING.md) — the item loop, the gates, release flow, and the lines we don't cross.

**`dg`** is a single-owner SQLite service: one Rust gateway, many independent databases, an HTTP API and CLI, and off-host backups.

**Status (2026-10-21): M1–M3 delivered; supervised non-production deployment live on micro2; production not accepted.** The authenticated gateway (owner bootstrap, database-scoped keys, SQL query/batch over HTTP, idempotent writes, restart persistence), TypeScript client, JSON CLI, interactive shell, terminal operator view, and clone/export/import/tombstone-delete lifecycle operations exist and are tested. Database import and clone are owner-only management operations (owner decision 2026-10-10); scoped application keys can export and query only within their authorized source scope. Backups are live on the reference deployment: hourly verified snapshot sets plus continuous Litestream replication to OVH object storage (compatibility verified 12/12 gates against a live sandbox bucket, [docs/evidence/sandbox-packet-2026-10-14-ovh.log](docs/evidence/sandbox-packet-2026-10-14-ovh.log)), with backup-health status/digest and automatic replication coverage for newly created databases. A pilot database was launched on micro2 on 2026-10-16 and the first 14 hourly entries of its soak are recorded ([docs/evidence/pilot-soak-log.md](docs/evidence/pilot-soak-log.md), 2026-10-16T09:24Z through 2026-10-16T21:17Z); the 7-day soak the pilot plan calls for is **not yet evidenced** — the committed log ends at 2026-10-16T21:17Z and the interval after that has not been collected into it, so the soak gate stays open rather than passed. There is deliberately no HTTPS in the gateway: it listens on loopback only and is reached through an SSH tunnel. Production acceptance (Profile C RPO/RTO sign-off, rollback decisions) and any Turso migration remain separately approved milestones. **Do not deploy this as a production database service.**

## Quick start (local only)

One script sets everything up (builds `dg`, installs it to `~/.local/bin`, and verifies it):

```sh
scripts/setup.sh
```

Then, with the owner key saved to a `0600` file:

```sh
dg init --data-dir state/dg            # once: prints {"owner_key": …} — save it securely, shown once
dg serve --listen 127.0.0.1:7432 --data-dir state/dg
```

In another terminal:

```sh
dg --key-file owner.key create game_a
dg --key-file owner.key query game_a "CREATE TABLE players (id INTEGER PRIMARY KEY, name TEXT)"
dg --key-file owner.key query game_a "INSERT INTO players VALUES (?, ?)" --args '[1, "aria"]'
dg --key-file owner.key query game_a "SELECT * FROM players"
dg --key-file owner.key inspect game_a      # one database: what it holds and what it has been doing
dg --key-file owner.key watch                # the service dashboard, read once
dg --key-file owner.key key tokens game_a    # only the credentials that can reach game_a
dg --key-file owner.key execute game_a script.sql   # run a SQL file, statement by statement
dg --key-file owner.key tui                # the terminal view: service, databases, backups and keys, live
```

Prefer an uninstalled debug build instead: `cargo build --locked` plus `export PATH="$PWD/.cargo/target/debug:$PATH"`, then the same commands. `scripts/setup.sh --help` lists options (`--prefix`, `--no-install`, `--strict`). It also reports when the `dg` already installed is older than the source tree — that is how a binary that predates a redesign keeps being run without anyone noticing — and `--strict` turns that warning into a failure. The install does not need the running `dg` closed first: it replaces the binary's path rather than the file, so a view that is already open keeps running on the code it started with, and the next launch picks up the new build.

- **API reference** (routes, value encoding, error codes, write/retry semantics): [docs/api.md](docs/api.md)
- **Architecture and decision register**: [docs/architecture.md](docs/architecture.md)
- **Task list / milestones**: [TASKS.md](TASKS.md)
- **The terminal view** (bare `dg`, or `dg tui`: the service, its databases, their backups and its credentials on one live screen, and the actions that change them): [docs/tui.md](docs/tui.md)

## TypeScript client

`client/` ships `@dracon/db-gateway` (execute, atomic batch, idempotent replay), verified against a running server by `npm test` (from the repo root). Bots and coding agents should use scoped `database_read_only`/`database_read_write` keys — never the owner key. See docs/api.md for the credential model and retry policy.

## Reading the output

On a terminal the CLI draws tables and dashboards; piped output and
`--json` stay machine-readable JSON, so scripts are unaffected.

```
$ dg query game_a "SELECT id, name, score FROM players ORDER BY id"
┌────┬─────────┬───────┐
│ id │ name    │ score │
├────┼─────────┼───────┤
│  1 │ aria    │  93.5 │
│  2 │ octavia │  88.0 │
└────┴─────────┴───────┘
2 rows
```

```
$ dg status
service
  version    0.4.0
  uptime     3d 4h
  databases  12 live, 2 tombstoned

activity
  requests    48,213        1.2k/min
  errors      3             0.0062%
  error rate  0.0062%

recent activity
  req/s   ▁▂▃▅▇▆▄▂

admission
  acquire timeout  500 ms
  read permits     8
  write permits    4
  wait p50         8 us
  wait p95         14 us

per-database
┌──────────┬───────────┬──────────────┬───────┬──────────┬──────────┬────────────┐
│ database │ rows read │ rows written │ waits │ wait p95 │ wait max │ last write │
├──────────┼───────────┼──────────────┼───────┼──────────┼──────────┼────────────┤
│ game_a   │     12004 │         8120 │    31 │    14 us │  1.2 ms  │ 2m ago     │
└──────────┴───────────┴──────────────┴───────┴──────────┴──────────┴────────────┘
```

`wait p95` is that database's own recent waits; `wait max` is its worst
single wait.

`dg watch` is the same dashboard read once, and `dg inspect game_a` is
one database — what it holds and what it has been doing. `dg key tokens
game_a` lists only the credentials that can reach `game_a`, and `dg
execute game_a script.sql` runs a SQL file statement by statement.
Every command follows the same rule: a terminal gets a human view, a
pipe or `--json` gets JSON.

The rules the drawing follows are in
[docs/api.md](docs/api.md#how-the-terminal-output-is-drawn): numbers are
right-aligned, `NULL` is distinct from the text `"NULL"`, byte counts
and durations are humanised, column widths are measured by display width
(so CJK text and emoji do not shear the grid), colour is decoration only
(`NO_COLOR` turns it off, and every coloured state is also spelled out),
and a value's control characters are escaped so a cell can never repaint
the terminal (the renderer owns that rule, for every view).

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
npm test          # builds the TS client and runs its integration suite against a real dg serve
```

Fast loop while iterating (instead of the full workspace suite):

```sh
cargo test --locked --test <name>   # e.g. tui_layout, cli, m1_e2e
```

`Cargo.lock` is tracked to fix dependency resolution. CI runs the Rust checks, the TypeScript client suite, deploy-template validation, and a RustSec advisory scan (`cargo audit` equivalent) on stable Rust.

## Security posture

Loopback-only listening; credentials stored hash-only; app keys are revocable and database-scoped; read-only enforcement uses SQLite's authorizer plus statement checks (not SQL prefix matching); attach/pragma/extension/trigger attack surfaces are denied; `_dg_*` tables are protected. There is no TLS in the gateway by design: remote use rides an SSH tunnel to the loopback listener, and clients refuse plaintext non-loopback endpoints. Deployment to a public host is out of scope until a separately approved network-edge decision (see [TASKS.md](TASKS.md)).

On the reference deployment, live SQLite files reside on micro2's persistent local disk; OVH object storage holds asynchronous recoverable backups (hourly snapshot sets + Litestream WAL replication). No production credentials, servers, or buckets are touched by this repository's tests.
