# DB Gateway

**Canonical roadmap:** [`docs/personal-turso-megaplan.md`](docs/personal-turso-megaplan.md)
**Current implementation checklist:** [`TASKS.md`](TASKS.md)
**Documentation index:** [`docs/README.md`](docs/README.md) — canonical vs historical-evidence vs superseded, for the whole docs tree.
**How we work:** [`CONTRIBUTING.md`](CONTRIBUTING.md) — the item loop, the gates, release flow, and the lines we don't cross.

**`dg`** is a single-owner SQLite service: one Rust gateway, many independent databases, an HTTP API and CLI, and off-host backups.

**Status (2026-10-19): M1–M3 delivered; supervised non-production deployment live on micro2; production not accepted.** The authenticated gateway (owner bootstrap, database-scoped keys, SQL query/batch over HTTP, idempotent writes, restart persistence), TypeScript client, JSON CLI, interactive shell, web console, and clone/export/import/tombstone-delete lifecycle operations exist and are tested. Database import and clone are owner-only management operations (owner decision 2026-10-10); scoped application keys can export and query only within their authorized source scope. Backups are live on the reference deployment: hourly verified snapshot sets plus continuous Litestream replication to OVH object storage (compatibility verified 12/12 gates against a live sandbox bucket, [docs/evidence/sandbox-packet-2026-10-14-ovh.log](docs/evidence/sandbox-packet-2026-10-14-ovh.log)), with backup-health status/digest and automatic replication coverage for newly created databases. A pilot database has been soaking on micro2 since 2026-10-16 ([docs/evidence/pilot-soak-log.md](docs/evidence/pilot-soak-log.md)). There is deliberately no HTTPS in the gateway: it listens on loopback only and is reached through an SSH tunnel. Production acceptance (Profile C RPO/RTO sign-off, rollback decisions) and any Turso migration remain separately approved milestones. **Do not deploy this as a production database service.**

## Quick start (local only)

One script sets everything up (builds `dg`, installs it to `~/.local/bin`, builds the web companion, verifies, and — with `--smoke` — proves `dg web` works against a disposable gateway):

```sh
scripts/setup.sh --smoke
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
dg --key-file owner.key web --port 8787   # browser view at http://127.0.0.1:8787 (loopback only)
```

Prefer an uninstalled debug build instead: `cargo build --locked` plus `export PATH="$PWD/.cargo/target/debug:$PATH"`, then the same commands. `scripts/setup.sh --help` lists options (`--prefix`, `--no-install`).

- **API reference** (routes, value encoding, error codes, write/retry semantics): [docs/api.md](docs/api.md)
- **Architecture and decision register**: [docs/architecture.md](docs/architecture.md)
- **Task list / milestones**: [TASKS.md](TASKS.md)
- **Local web console** (loopback companion, SSH tunnel workflow, data-centric database detail, guarded lifecycle controls, real headless-browser coverage, and visibility boundaries): [`web/README.md`](web/README.md)

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
  version  0.2.0
  uptime   3d 4h

admission
  acquire timeout  500 ms
  read permits     8
  write permits    4

requests
  total              1043
  capacity exceeded  0
  deadline exceeded  0
  internal errors    0

per-database
┌──────────┬──────┬─────────┬───────┬─────────┬──────────┐
│ database │ read │ written │ waits │ wait ms │ timeouts │
├──────────┼──────┼─────────┼───────┼─────────┼──────────┤
│ game_a   │    2 │       2 │     3 │       0 │        0 │
└──────────┴──────┴─────────┴───────┴─────────┴──────────┘
```

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
npm test --prefix web  # console unit/companion tests plus disposable real-browser E2E
node scripts/dg-web-smoke.mjs  # disposable end-to-end of `dg web` (no browser needed)
```

Fast loop while iterating (instead of the full workspace suite):

```sh
cargo test --locked --test <name>   # e.g. console_launch, cli, m1_e2e
node scripts/dg-web-smoke.mjs       # launcher + companion + supervision in ~30s
```

`Cargo.lock` is tracked to fix dependency resolution. CI runs the Rust checks, the TypeScript client suite, deploy-template validation, and a RustSec advisory scan (`cargo audit` equivalent) on stable Rust.

## Security posture

Loopback-only listening; credentials stored hash-only; app keys are revocable and database-scoped; read-only enforcement uses SQLite's authorizer plus statement checks (not SQL prefix matching); attach/pragma/extension/trigger attack surfaces are denied; `_dg_*` tables are protected. There is no TLS in the gateway by design: remote use rides an SSH tunnel to the loopback listener, and clients refuse plaintext non-loopback endpoints. Deployment to a public host is out of scope until a separately approved network-edge decision (see [TASKS.md](TASKS.md)).

On the reference deployment, live SQLite files reside on micro2's persistent local disk; OVH object storage holds asynchronous recoverable backups (hourly snapshot sets + Litestream WAL replication). No production credentials, servers, or buckets are touched by this repository's tests.
