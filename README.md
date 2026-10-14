# DB Gateway

**Canonical roadmap:** [`docs/personal-turso-megaplan.md`](docs/personal-turso-megaplan.md)
**Current implementation checklist:** [`TASKS.md`](TASKS.md)

**`dg`** is a single-owner SQLite service: one Rust gateway, many independent databases, an HTTP API and CLI, and off-host backups.

**Status: M2 — authenticated round trip + safe lifecycle operations work locally.** Owner bootstrap, database-scoped keys, SQL query/batch over HTTP, idempotent writes, restart persistence, a TypeScript client, a JSON CLI, the interactive shell, and clone/export/import/tombstone-delete lifecycle operations exist and are tested. Database import and clone are owner-only management operations (owner decision 2026-10-10); scoped application keys can export and query only within their authorized source scope. Not yet available: OVH backups and HTTPS. A first supervised non-production deploy is recorded in [docs/first-deploy-micro2.md](docs/first-deploy-micro2.md); production deployment remains a separately approved milestone. **Do not deploy this as a production database service.**

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

## Security posture (M1)

Loopback-only listening; credentials stored hash-only; app keys are revocable and database-scoped; read-only enforcement uses SQLite's authorizer plus statement checks (not SQL prefix matching); attach/pragma/extension/trigger attack surfaces are denied; `_dg_*` tables are protected; server listens on loopback only until authentication-over-HTTPS ships in a later milestone. Deployment to a public host is out of scope until M4 and requires explicit approval.

Live SQLite files will reside on micro2's persistent local disk; OVH object storage will hold asynchronous recoverable backups (M3). No production credentials, servers, or buckets are touched by this repository's tests.
