# DB Gateway — implementation task list

**Document role:** Current implementation checklist
**Canonical roadmap:** [`docs/personal-turso-megaplan.md`](docs/personal-turso-megaplan.md)
**Status rule:** This file records implementation status; the canonical
roadmap records forward sequencing, architecture direction, gates, and non-goals.
Neither document authorizes live, deployment, subscription,
or production-data actions.

Project: **DB Gateway** · CLI: **`dg`**

## Agreed scope

A personal, single-owner SQLite database service with a Rust gateway and CLI. Applications remain stateless and connect through the API. One gateway manages many independent databases and development copies.

Deployment target: a single-owner server with persistent local disk. Live SQLite files reside on persistent local disk; persistent database backups go to object storage. Compatibility and backup behaviour must be verified, not assumed.

One owner/admin credential plus database-scoped read-only and read/write application credentials. No multi-organization system, billing, SSO, or hosted/public web dashboard in the initial release. The operator's surface is the terminal: the one-shot CLI, the interactive shell, and the full-screen view (`dg tui`), which reach a remote gateway through a private transport and are not a prerequisite for the core service. Personal scope does not imply relaxed durability or correctness.

This file is a plan, not authorization to deploy, migrate production data, delete databases, or change subscriptions. Commands below describe proposed behaviour.

## Execution order and current status

This is the current implementation checklist, subordinate to the canonical
Personal Turso roadmap. Mark items complete only with implementation or
documentation evidence and the relevant passing checks. The phases below are a
status ledger, not a second competing roadmap.

**Checkbox semantics:** a marker states how far
*this line's own work* has gone — never the state of other work the line
points at.

- `[x]` **Executed** — the work the line describes was performed; the
  evidence is in the line.
- `[ ]` **Open** — execution remains to be done.
- `[p]` **Plan-complete** — only the plan/gate/design text for the work
  exists (linked in the line); the execution it governs has not happened,
  and the line names what that execution waits on.
- `[d]` **Deferred by decision** — consciously parked; the line links the
  decision record and its re-open criteria. Parked items are not
  scheduled work.

A line whose evidence mixes performed and unperformed work is split into
one line per state, so every marker is true on its own.

### Foundation

The foundation is present in [Cargo.toml](Cargo.toml), [src/lib.rs](src/lib.rs), [tests/cli.rs](tests/cli.rs), and [CI](.github/workflows/ci.yml).

- [x] Add a Rust workspace exposing the `dg` executable.
- [x] Provide `dg --help`, `dg --version`, and a local-only `dg serve` with a minimal liveness endpoint; no SQL or management routes yet.
- [x] Validate startup configuration and provide structured errors/logging without credentials.
- [x] Add automated tests, formatting/lint checks, CI, and reproducible local instructions.
- [x] Record initial architecture and non-goals in `docs/architecture.md`.

Acceptance: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, and `cargo test --workspace --locked` pass. A local server returns the documented liveness response. This milestone is not a usable database service.

### Core service

- [ ] Authenticated gateway with scoped database credentials.
- [ ] Database lifecycle operations for independent SQLite databases.
- [ ] SQL execution surface with bounded work and idempotent writes.
- [ ] Versioned backup and verified restore from local and off-host copies.
- [ ] Status, freshness, and failure visibility for operators.

### Client interfaces

- [ ] One-shot CLI commands for database and backup operations.
- [ ] Interactive shell and full-screen view for operators.
- [ ] Typed client libraries that mirror gateway semantics.
- [ ] Local examples and setup documentation.

### Deployment and release

- [ ] Service templates for local and server deployment.
- [ ] Release bundle with verified checksums and build information.
- [ ] Upgrade and rollback procedures.
- [ ] Release checks for formatting, lints, tests, and dependencies.
