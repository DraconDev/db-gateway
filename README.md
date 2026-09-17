# DB Gateway

**`dg`** is a single-owner SQLite service: one Rust gateway, many independent databases, an HTTP API and CLI, and off-host backups.

**Status: M1 — first authenticated database round trip works locally.** Owner bootstrap, database-scoped keys, SQL query/batch over HTTP, idempotent writes, restart persistence, a TypeScript client, and a JSON CLI exist and are tested. Not yet available: interactive shell, clone/export/delete lifecycle, backups, HTTPS, deployment. **Do not deploy this as a production database service** — that is the separately approved M4 milestone.

## Quick start (local only)

```sh
cargo build --locked
dg init --data-dir state/dg            # once: prints {"owner_key": …} — save it securely, shown once
dg serve --listen 127.0.0.1:7432 --data-dir state/dg
```

In another terminal (owner key saved to a `0600` file):

```sh
dg --key-file owner.key create game_a
dg --key-file owner.key query game_a "CREATE TABLE players (id INTEGER PRIMARY KEY, name TEXT)"
dg --key-file owner.key query game_a "INSERT INTO players VALUES (?, ?)" --args '[1, "aria"]'
dg --key-file owner.key query game_a "SELECT * FROM players"
```

- **API reference** (routes, value encoding, error codes, write/retry semantics): [docs/api.md](docs/api.md)
- **Architecture and decision register**: [docs/architecture.md](docs/architecture.md)
- **Task list / milestones**: [TASKS.md](TASKS.md)

## TypeScript client

`client/` ships `@dracon/db-gateway` (execute, atomic batch, idempotent replay), verified against a running server by `npm test` (from the repo root). Bots and coding agents should use scoped `database_read_only`/`database_read_write` keys — never the owner key. See docs/api.md for the credential model and retry policy.

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
npm test          # builds the TS client and runs its integration suite against a real dg serve
```

`Cargo.lock` is tracked to fix dependency resolution. CI runs the Rust checks on stable Rust.

## Security posture (M1)

Loopback-only listening; credentials stored hash-only; app keys are revocable and database-scoped; read-only enforcement uses SQLite's authorizer plus statement checks (not SQL prefix matching); attach/pragma/extension/trigger attack surfaces are denied; `_dg_*` tables are protected; server listens on loopback only until authentication-over-HTTPS ships in a later milestone. Deployment to a public host is out of scope until M4 and requires explicit approval.

Live SQLite files will reside on micro2's persistent local disk; OVH object storage will hold asynchronous recoverable backups (M3). No production credentials, servers, or buckets are touched by this repository's tests.
