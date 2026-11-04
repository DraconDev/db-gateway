# DB Gateway

**Canonical roadmap:** [`docs/personal-turso-megaplan.md`](docs/personal-turso-megaplan.md)
**Current implementation checklist:** [`TASKS.md`](TASKS.md)
**Documentation index:** [`docs/README.md`](docs/README.md) — canonical vs historical-evidence vs superseded, for the whole docs tree.
**How we work:** [`CONTRIBUTING.md`](CONTRIBUTING.md) — the item loop, the gates, release flow, and the lines we don't cross.

**`dg`** is a single-owner SQLite service: one Rust gateway, many independent databases, an HTTP API and CLI, and off-host backups.

**Status: pre-production (0.x).** Interfaces and data formats may change. Do not treat this as a production database system.

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

Want material to look at instead of a single hand-made table? `scripts/seed-demo.py` fills seven
linked databases (inventory, orders, ledger, sessions, notes, plus the billing and probe seeds) with
27 tables and roughly 1,500 rows: a composite primary key (`product_prices(product_id, valid_from)`,
`warehouse_stock`, `fx_rates(rate_date, currency)`), a self-referencing tree (`category_tree`),
declared foreign keys, three explicit indexes beside the ones the keys imply, a UNIQUE tracking
number, a NULL-heavy `shipments` table where most rows have not shipped, unicode note bodies, and
one deliberately wide 14-column extract (`legacy_stock_extract`) with long column names. It is
idempotent — re-running it inserts nothing — and `tests/seed_demo.rs` holds it to that against a
fresh gateway.

```sh
python3 scripts/seed-demo.py --server http://127.0.0.1:7432 --key-file owner.key
```

See the API reference and terminal-view docs for the full command surface.

Prefer an uninstalled debug build instead: `cargo build --locked` plus `export PATH="$PWD/.cargo/target/debug:$PATH"`, then the same commands. `scripts/setup.sh --help` lists options (`--prefix`, `--no-install`, `--strict`). It also reports when the `dg` already installed is older than the source tree — that is how a binary that predates a redesign keeps being run without anyone noticing — and `--strict` turns that warning into a failure. The install does not need the running `dg` closed first: it replaces the binary's path rather than the file, so a view that is already open keeps running on the code it started with, and the next launch picks up the new build.

- **API reference** (routes, value encoding, error codes, write/retry semantics): [docs/api.md](docs/api.md)
- **Architecture and decision register**: [docs/architecture.md](docs/architecture.md)
- **Task list / milestones**: [TASKS.md](TASKS.md)
- **The terminal view** (bare `dg`, or `dg tui`: the service, its databases, their backups and its credentials on one live screen, and the actions that change them): [docs/tui.md](docs/tui.md)

## TypeScript client

`client/` provides a TypeScript client for the HTTP API. Run `npm test` from the repository root to verify it against a local server.

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
  version    0.10.0
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

Additional local harnesses are available under `scripts/`; see each script's `--help` for its options.

`Cargo.lock` is tracked to fix dependency resolution. CI runs the Rust checks, the TypeScript client suite, deploy-template validation, and a RustSec advisory scan (`cargo audit` equivalent) on stable Rust.

## Security posture

The service is designed for trusted local or private-network use; do not expose it directly to the public internet. Owner credentials should be kept separate from application credentials, and application credentials should be revocable. Backups are operator-configured.
