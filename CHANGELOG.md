# Changelog

All notable changes to db-gateway are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**Release practice.** One version identifies the whole project: the
workspace `Cargo.toml`, `crates/dg-client/Cargo.toml`, and the root /
`client/` `package.json` files move together in a single bump commit
(lockfiles refreshed in the same commit: `cargo update -w`, and the npm
lockfiles' root entries). The release commit is tagged
`vX.Y.Z` (annotated) — the tag, `dg --version`, and the version
`scripts/release-bundle.sh` stamps into every release bundle are the
same number, so a deployed bundle can always be traced to exactly one
tagged state. Deployments and rollbacks follow
`docs/upgrade-rollback.md`. While the project is pre-production
(0.x), minor versions mark feature growth and patch versions mark
fixes; 1.0.0 waits for production acceptance.

## [Unreleased]

### Added

- **`dg tui` — the terminal operator view.** One screen that holds the
  service, its databases, their backup freshness and its credentials at
  once, read again every two seconds, with the actions that change
  them a keystroke away: create, delete, clone, export and import a
  database, issue, revoke and rotate a key, and run a query, a SQL
  file or a batch. It asks for the same API routes the CLI and the
  TypeScript client already use, and changes none of them.
- **One view layer, two surfaces.** The CLI's tables and the view's
  panes are now produced by the same functions (`src/views.rs`), so the
  two cannot tell an operator different stories about the same
  gateway — the defect that put "not run" on a row the gateway never
  reported. The view's requests are one catalogue
  (`src/tui/actions.rs`), the destructive ones are confirmed by the
  same rule the CLI's `--confirm` flag uses (`src/confirmation.rs`),
  and the 32-statement batch cap is enforced by the same function on
  both sides (`src/execution.rs`).
- **The view gives the terminal back.** The alternate screen and raw
  mode are restored however the view ends, including when it panics
  (`src/tui/screen.rs`), and its layout is tested without a terminal
  through ratatui's `TestBackend`.

- **`dg watch`** — the service dashboard, read once. It exits rather
  than refreshing in place, so it stays pipeable and diffable.
- **`dg inspect <db>`** — one database: what it holds and what it has
  been doing. `dg info` is what a database IS; `dg inspect` is what it
  has been DOING.
- **`dg key tokens <db>`** — the credentials that can act on one
  database. The owner key reaches everything, an application key
  reaches the databases it was scoped to: the first question of a
  leaked-key incident.
- **`dg execute <db> <file.sql>`** — runs a SQL file and reports each
  statement. The file is split by SQLite's own `sqlite3_complete`, so
  a semicolon inside a string, a comment or a trigger body is not a
  statement boundary, and the statements run as one atomic batch.
- **The shell is the control surface.** `dg shell` gained `.status`,
  `.keys` and `.backups` beside `.tables` and `.schema`, and they draw
  the same views as the one-shot commands.
- **The service dashboard answers questions.** `dg status` now shows
  live and tombstoned database counts, a request rate, an error rate,
  admission-wait percentiles, a sparkline of recent activity and a
  per-database table with each database's traffic, waits and last
  write. The server gained the facts it needs, including a bounded
  rolling window of recent request counts.
- **A per-database wait percentile.** `/v1/status` now reports
  `queue_wait_p95_us` per database: that database's OWN 95th
  percentile of its own recent admission waits. The service-wide
  percentile mixes every database, so one contended database can hide
  inside it; this is the figure that names the slow one.

### Removed

- **The web console is retired.** `dg web`, its Node companion tree
  (`web/`), its launch tests and its CI gates are gone: the terminal
  view is the operator's surface. The published TypeScript client
  (`client/`) and the HTTP API are untouched.

### Fixed

- **The admission tests no longer depend on the speed of the machine they
  run on.** The suite's premise was a hand-picked row count — a 30 000-row
  insert, a 300 000-row recursive CTE — that had been calibrated on one
  machine: on a slower or busier one the same count outgrew the server's
  own statement deadline, and the test failed with a 408 (or a 429 from
  the admission bound) for a reason that had nothing to do with admission.
  The work is now measured on the machine that runs the test, so the
  premise holds everywhere: the work still overlaps the requests that
  have to wait for it, and it still finishes inside the deadline.
- **A waiter is turned away only after waiting.** The admission bound is
  a promise that a burst queues up instead of failing instantly, and a
  read arriving during a write relies on it. Nothing asserted that
  promise directly, and the one assertion that came close was relative
  to `ACQUIRE_TIMEOUT` — a change to the constant would have made it
  vacuous. The bound is now pinned by an absolute window (a waiter is
  shed after at least 400 ms and well under 5 s), and a reader that
  waits for a writer which releases inside the bound is pinned as
  admitted (`src/store/tests.rs`).
- **The clients give a write more room than a read before they give up.**
  Ten seconds was the whole timeout, so a gateway that was busy answering
  turned a write into "outcome unknown" — the one state an operator has
  to reconcile by hand. A read still gives up in ten seconds (a retry
  costs nothing else); a write waits a minute, because the answer is
  what removes the doubt. The CLI and the Rust client
  (`crates/dg-client`) now follow that rule; the TypeScript client
  (`client/`) is untouched.
- **The notification test no longer races the delivery it waits for.**
  It waited for a digest note to arrive — and the collector already
  held the digests from before the sink started rejecting, so the wait
  returned at once and the assertion that followed raced the next
  delivery instead of waiting for it. It now waits for the count to
  grow.
- **The contention tests no longer depend on who wins a race.** The
  probe that looks for the `429` polled back to back, and a read shares
  the guard with the other reads: the probe kept the read batch busy
  enough that the holder's writer never got its turn — the
  writer-starvation gap the design documents, turned into a test that
  could not see the contention it was looking for. The probe now leaves
  a gap between polls, so the read batch drains and the holder gets in.
- **The global lane's bound is pinned by a test that cannot race.** It
  was checked over HTTP, where filling the lane needs as many slow
  writes as the lane has permits, and whether they overlap is something
  the machine decides (four CPU-bound holders starve each other's
  admission, so on a busy box the last one is admitted after the first
  has finished). The bound is now checked where the contention is
  exact — the permits are held directly (`src/api.rs`) — and the HTTP
  test keeps what it can decide: every holder is rolled back at the
  deadline and the service works normally again afterwards.
- **`dg execute` now refuses an over-long script before it sends it.**
  The batch API accepts 32 statements and the whole file is one atomic
  batch, so a longer file was sent anyway and came back as the
  server's bare rejection, which the CLI drew as "rolled back" on
  every row — a rollback that never happened, for statements that
  never ran. The CLI now counts the statements, refuses locally with
  the count, the cap and the fact that nothing was sent, and a batch
  the server rejects as a whole is drawn as "not run" rather than as a
  rollback. The cap is one number, read by the client and the server.
- **The documents no longer contradict the unit about the gateway's
  memory cap.** One pass corrected the number in three documents and
  left a fourth claiming 96 MiB twenty lines below the correction, so
  the file contradicted itself. The deploy gate now checks the live
  documents against the unit: a document that names the gateway unit
  must state the cap the unit sets, and the sidecar's own cap, the
  sandbox's 1 GiB scope and the dated measurement records are left
  alone.
- **The host drift check no longer reports a false drift on a healthy
  host.** It compares the unit installed on a host against the repo
  template rendered with the reference-host values, and the render
  left the placeholders it did not know in place. A template that
  gained a placeholder — the hourly backup unit, which now passes the
  paths it was rendered with — was therefore hashed as text no
  correctly rendered host can have, so every healthy host reported a
  permanent mismatch and the check could no longer find real drift.
  The render is strict now: a placeholder with no value fails the
  check, naming it, until the host's value for it is stated, and both
  the drift check and the deploy gate render through the same
  renderer.
- **The per-database table no longer shows a maximum under a "p95"
  header.** The wait-latency column held each database's longest
  single wait while claiming to be a percentile, so on a contended
  database it overstated the p95 and disagreed with the genuine p95
  in the admission block of the same screen. The server now computes a
  real per-database percentile, the table shows it as `wait p95` and
  shows the worst single wait separately as `wait max`, and `dg
  inspect` names both.
- **`dg key tokens <db>` narrows the answer for a script as well as
  for a person.** The machine form was the raw credential list, so a
  script asking which credentials can reach a database got every
  credential the gateway holds. Both forms now answer the same
  question.
- **Every command has a human view.** `create`, `delete`, `export`,
  `import`, `clone`, `key create`, `key revoke`, `key rotate-owner`,
  `backup verify`, `batch` and `init` used to print raw JSON on a
  terminal. They now report what happened, through the same renderer
  the read commands use. One rule, in one place: a terminal gets the
  human view, a pipe or `--json` gets the JSON.

### Fixed

- **A wire value can no longer repaint the terminal.** The escaping of
  control characters moved from the JSON result path into the renderer
  itself, so it now covers every cell, column header, table footer and
  section title -- a database name, a key name, a service version or a
  backup state included. The dashboards introduced with the pretty-print
  layer had briefly drawn wire values unescaped.
- **A malformed backup-set stamp no longer panics the CLI.** The stamp
  is validated on its bytes before it is interpreted, so a stamp whose
  prefix is multi-byte is shown as it arrived instead of crashing the
  command.
- **A failing script says which statement broke it.** A batch runs as a
  whole, so without that the operator had to bisect the script by hand.
  A single statement keeps the error envelope it has always had.

## [0.2.0] — 2026-10-19

The first release-identified state: the feature set that has been
running as a supervised non-production deployment on micro2, plus the
2026-10-19 audit hardening. Production is **not** accepted; see
`README.md` for the live status.

### Added

- Authenticated SQLite gateway: owner bootstrap, database-scoped API
  keys, SQL query/batch over HTTP with idempotent writes, interactive
  shell, JSON CLI, and a workstation web console.
- Database lifecycle: clone, export/import (validated, atomic), and
  tombstone delete with a typed-name confirmation gate.
- Backups: hourly verified snapshot sets, continuous Litestream
  replication to OVH S3, backup-health status/digest, and restore
  drills wired to systemd timers.
- Access edge: loopback-only service reached via SSH tunnel or the
  WireGuard mesh with socket-activated proxies; public binds refused.
- Observability: request-pressure counters, backup-freshness signals,
  and alert notices under `docs/observability-contract.md`.
- Release identity: this changelog, the `vX.Y.Z` tag practice, and
  `SECURITY.md`. Every tracked script is lint-gated in CI
  (`scripts/lint-scripts.sh`), and the docs tree is indexed
  (`docs/README.md`) with dated evidence under `docs/evidence/`.

### Changed

- The deployed micro2 binary still reports `dg 0.1.0` until the next
  upgrade window; 0.2.0 is the repository state that window will
  deploy.
- **The CLI now draws its terminal output through one shared renderer**
  (`src/render.rs`): box-drawn tables with numbers right-aligned under
  their headers, a `NULL` that is distinct from the text `"NULL"`,
  humanised byte counts, durations, epochs and backup-set stamps, column
  widths measured by display width so wide characters do not shear the
  grid, and colour as decoration only (`NO_COLOR` turns it off; every
  coloured state is also spelled out in words). `dg status` and
  `dg backup status` became dashboards (titled sections for service,
  admission, requests and per-database state) instead of one long
  `property | value` list. Piped output and `--json` remain
  byte-identical JSON, so scripts are unaffected.

## [0.1.0] — 2026-09-18

First supervised deployment to micro2 (retroactive entry, recorded
2026-10-19 when this changelog was adopted): the authenticated gateway
and CLI from milestones M1–M3, before the audit hardening above.
