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

### Fixed

- **Idempotency-key cap raised from 256 to 10 000** per principal per
  database. With a 24-hour retention, 256 allowed one keyed write every
  ~6 minutes, so an application that keys every write (the documented
  way to retry safely) hit `429 capacity_exceeded` within hours. 10 000
  allows one every ~9 s, sustained, and still bounds the replay table.
- **REAL values are stored exactly as sent.** JSON was parsed with
  serde_json's default float parser, which is fast but not exact: a
  value such as `-2006.7142857142858` arrived as `-2006.7142857142856`,
  one unit in the last place off, and was stored that way without any
  error. `float_roundtrip` is now enabled in the gateway and in
  `dg-client`, so the server, the CLI's `--args` and the Rust client all
  parse floats exactly. Found by the new differential test; pinned by
  `reals_round_trip_exactly_through_the_api`.
- **SQLite updated for RUSTSEC-2026-0459** (`rusqlite` 0.41, bundled
  SQLite 3.54.1), and `rustix` 1.1.6 for RUSTSEC-2026-0486.

- **`EXPLAIN ANALYZE` took the read lane.** `ANALYZE` was missing from
  the write-keyword list the server and both clients share, so the
  statement was classified as a read although SQLite reports it as
  writing. It was still refused at execution (the authorizer does not
  allow `ANALYZE`); now it is classified correctly too, in all three
  tokenizers. Found by the new SQL fuzzer.

### Added

- **Shadow mode** (`@dracon/db-gateway/shadow`, `docs/shadow.md`):
  `withShadow(primary, dgDatabase)` wraps a `@libsql/client`-shaped
  client so every write the primary acknowledges is mirrored to dg in
  order, in the background, with divergences reported and never raised;
  `compareDatabases` checks both copies table by table, paged.
- **Soak test** (`tests/soak.rs`): concurrent writers, readers and a
  migrator while the server is SIGKILLed and restarted on a rhythm; every
  acknowledged write must survive exactly once, observed counts never
  shrink, keyed retries replay, and exports pass `integrity_check`.
  `DG_SOAK_SECS` / `DG_SOAK_KILL_EVERY` scale it.
- **Property fuzzing of the SQL scanners and the authorizer**
  (`src/execution_fuzz.rs`): generated adversarial statements check that
  nothing which prepares under the authorizer touches a `_dg_*` object,
  that a read-only credential's statements only read, that the read lane
  only gets statements SQLite calls read-only, that every valid `CREATE
  INDEX` can build, that scripts split into their statements, that the
  query log keeps no literal or comment content, and that no scanner
  panics. `DG_FUZZ_CASES` / `DG_FUZZ_SEED` scale and reproduce it.
- **Schema migrations.** A write credential may `ALTER TABLE` (add,
  rename and drop a column, rename the table), create and drop views and
  triggers; any credential may read the schema with the introspection
  PRAGMAs (`table_info`, `index_list`, `foreign_key_list` and kin).
  Value-setting PRAGMAs, `ATTACH`, temp objects, user transactions and
  every `_dg_*` name stay refused; a RENAME onto a `_dg_` name is rolled
  back. `quick_check` is on the introspection list because SQLite runs
  it itself inside `ALTER TABLE ADD COLUMN` on a STRICT or constrained
  table (found by the differential test, seed 1004). Covered by
  `tests/migration_ddl.rs`.
- **Differential testing against plain SQLite** (`tests/differential.rs`):
  seeded random schemas, migrations, writes, batches and queries run
  through the gateway and through a reference SQLite file opened with the
  gateway's settings; every outcome, both full dumps and a clone must
  agree. `DG_DIFF_SEEDS` / `DG_DIFF_OPS` scale it up for long runs.

### Changed

- **Public front door trimmed.** `README.md` no longer links to the
  internal task ledger or contributor/operator runbooks, and
  `SECURITY.md` / `CONTRIBUTING.md` now state only generic boundaries
  (pre-production, local, private transport, least-privilege
  credentials, operator-configured off-host backups). The publication
  runbook records the whole-tree audit disposition; no behavior changed.

### Added

- **`CREATE INDEX`, `CREATE UNIQUE INDEX` and `REINDEX` now work.** They
  were refused with `sql_forbidden`, and not by a decision: SQLite asks
  the authorizer about a `REINDEX` of the index it is building, that
  action was missing from the gateway's allow list, and the fail-closed
  default denied the whole statement. The visible symptom was a gateway
  that could `DROP INDEX` an index it refused to create — `DropIndex`
  only needs actions that were already listed. The surface is now
  consistent: a write credential may create, drop and rebuild indexes,
  and `dg schema` reports them with their columns and uniqueness
  (`tests/index_surface.rs`, `tests/schema_shapes.rs`). Everything else
  stays refused, including `ALTER`, views, triggers, pragmas, a
  `REINDEX` that names an object (only the bare sweep is offered) and any
  `_dg_`-prefixed object, and a read-only credential still cannot touch
  an index. The bare sweep does rebuild the gateway's own implicit index
  along with everything else — SQLite reports it under a synthetic
  `sqlite_autoindex_*` name, so no statement can be told apart by name —
  which reads nothing back and changes nothing. `scripts/seed-demo.py`
  seeds three explicit indexes again as part of its demo schema.
- **Per-database traffic blocks.** `dg block-writes <db>` /
  `dg block-reads <db>` (each with `--off`) set durable,
  owner-only kill switches enforced at the query path with stable
  `403 writes_blocked` / `reads_blocked` codes — the enforcement half
  of a cutover writer-freeze. The flags bind every credential
  including the owner's, stay visible in `dg inspect`, migrate open
  for existing databases, and leave management reads (inspect,
  schema, export) available (`tests/cli_traffic_blocks.rs`).
- **Latency percentiles and an periodic traffic series
  (Analytics-lite).** `GET /v1/status` now reports service-wide
  `query_p50_us` / `query_p95_us` execution percentiles with
  `query_samples`, plus a per-database `query_p95_us` that names the
  slow database instead of averaging it away — all bounded rings
  over the recent past, `null` until the first execution
  (`src/store/metrics.rs`, `src/store/admission.rs`,
  `tests/cli_json_contract.rs`). `scripts/traffic-sample.sh` with the
  `deploy/dg-traffic-sample.{service,timer}` units (periodic, :50)
  appends one JSON sample of totals, per-database rows, and those
  percentiles to `traffic-samples.jsonl`, so a climbing p95 in one
  database is visible as a trend rather than a moment
  (`docs/observability-contract.md` signal "Traffic trend").
- **Audit entries for statements carry their execution time.** Every
  `query` / `batch` entry in `GET /v1/audit` now reports the measured
  `elapsed_us`, so the trail answers "which credential ran the slow
  query" — the per-action half of top-queries-lite, next to the
  per-shape costs in `GET /v1/queries`. Anything that is not a
  statement omits the field, old trail lines still read, and
  `dg audit` shows the cost in a human `elapsed` column
  (`tests/audit.rs`).
- **Tombstone listing (gap 8 closed).** `GET /v1/tombstones`
  (owner-only) lists every tombstoned database with its retention
  deadline, oldest purge first, and `dg list --tombstoned` is its
  CLI — the index between delete and undelete, so an operator who
  knows something is missing but not what it was called can find
  it. The live list stays live-only, and scoped keys are refused
  (`tests/cli_delete_protect.rs`).
- **`dg-alerts.sh` can POST alerts to a webhook.** The journal line stays
  unconditional; when `DG_ALERTS_WEBHOOK_URL` (or the file
  `DG_ALERTS_WEBHOOK_URL_FILE` points at) names an endpoint, disk and
  replication-lag alerts are additionally delivered there as JSON — the
  §3a second-sink pattern, extended from backup health. Same URL rule
  as `--backup-notify-webhook` (HTTPS, or HTTP on a literal local
  address), no proxy, no redirects; a failed delivery is reported, and
  unconfigured runs behave exactly as before.
- **Snapshot sets now have an automated off-host second leg.**
  The backup-pull tooling copies the newest verified set to an operator
  machine, re-verifies every manifest hash, sidecar and SQLite integrity
  check the producer performed, and prunes old pulls; the scheduled pull
  runs periodic. The continuous replica remains the live replication path;
  this is the file-level copy a rebuild starts from.
- **`Enter` in the terminal view opens the database under the scroll.**
  The keyboard's doorway used to list the open-table action with the
  database prefilled into its prompt — a different depth than the
  mouse's click, which opens the database's page. Both gestures now
  open the same page, on the databases tab and on the locations tab;
  tabs whose rows do not name databases are unaffected, and the
  open-table action stays in the actions list for direct use
  (`tests/tui_tables.rs`).

### Fixed

- **The repository can be published without leaking what private-ness was
  hiding.** Two real leaks were sitting in the tracked tree, and neither
  carried a credential-shaped string, so `scripts/secret-scan.sh` reported
  the tree clean with both of them in it. A decoded session transcript
  (`pi-session-2026-10-22T..._01a16557-....md`) held this operator's email
  address, their home directory path, a public server address and
  production database names; the 2026-09-21 audit had untracked the
  **`.html`** export of that session and swept the one extension it had
  seen, so the `.md` twin added later walked past it. Separately, the
  managed `.gitignore` block un-ignores `.dracon/data/` so the owner
  **public** key can be tracked, which re-included everything under it —
  an age private key (`.age`) or a plaintext age identity (`.agekey`)
  dropped in `.dracon/data/keys/` was committable, and nothing in the tree
  was one only because nobody had put one there. `.gitignore` now ignores
  `pi-session-*`, `conversation-*`, `rollout-*` and `trajectory-*` (every
  format, not the one a previous audit happened to notice) and `*.age` /
  `*.agekey` **below** the managed block, so it wins over the block's
  blanket un-ignores; the tracked public key still tracks.

- **Publication is a gate, not a habit.** `scripts/public-readiness.sh`
  runs every pre-publication check in one command and fails loudly:
  session exhaust and credential files in the index, build/runtime output,
  tracked blobs over 256 KiB, credential-shaped material in the tree, and
  — in the full run — session exhaust, `age`-encrypted secret material and
  credential-**formed** material anywhere in the history, with the commit
  and path for each. Identity disclosure (home directory paths, personal
  addresses, host IPs, a cloud account id) is reported as an advisory
  split into live docs, which can be scrubbed, and frozen records, which
  cannot. CI runs the `--tree` half on every push; the history half runs
  before publication, because history cannot be un-published and a rewrite
  is an owner decision. The matcher stays in `scripts/secret-scan.sh`:
  shaped hits block and name-based heuristic hits only advise, since the
  fixtures in `tests/secret_scan.rs` exist to contain those shapes.
  `tests/public_readiness.rs` pins the verdicts (11 contract tests, each
  fixture a throwaway git repo), and `docs/publication-readiness.md` is
  the runbook plus the record of what the gate found. The `.html` session
  export and the `age`-encrypted values are still in history; those
  decisions, and the reasoning behind each, are recorded there.

- **One upload's chunks can no longer overwrite each other.** An
  append read the acknowledged offset under the transfer registry lock,
  released it, and wrote the file — so two chunks submitted at the same
  offset both passed the offset check and both wrote. Whichever finished
  second then found the offset already advanced, took its "the transfer
  moved on" branch, and truncated the file back to the old offset:
  bytes the first request had already acknowledged were deleted, and the
  offset the client was told about no longer described the file. A
  chunked import could therefore lose data while both requests reported
  sensibly. Each transfer now carries its own operation lock — appends,
  commit and abort of one upload take turns, while the registry mutex is
  still released before every file operation so no other transfer waits
  on a disk write. A commit also waits for an in-flight append before it
  reads the file, so a half-written upload is never published. A file
  left longer than the acknowledged offset by a partial write is trimmed
  back to it under that lock — the bytes past it were never acknowledged
  — while a file that has lost acknowledged bytes is refused with
  `upload_out_of_sync` (409) instead of being published short.
  `tests/upload_append_lock.rs` covers the race, the commit ordering, the
  trim and the refusal; run against the implementations that shipped
  them, the race, the commit-ordering and the trim tests all fail.
- **The 64 KiB request-body cap now holds on every route.** The cap was
  applied to the gateway's own routes before the diff, branch, audit,
  db-ops and backup-run routers were merged in, and a router merge does
  not re-apply the outer router's layers — so `/v1/diff` and
  `/v1/branch` fell back to the framework's own 2 MiB default and
  accepted a body 32x the documented bound. The cap is now applied
  after every merge, and the import's own 12 MiB and the chunk body's
  2 MiB still win on their routes.
- **A row read is not reported as a write.** `dg rows` classified its
  SELECTs as writes, so a timeout against an unreachable gateway was
  reported as `outcome_unknown` — "writes may have committed, replay
  only with the same idempotency key" — for a query that committed
  nothing and for which no key is ever sent. It now reads as what it
  is: an unavailable gateway.
- **The audit trail no longer records one action twice.** The action
  that pushed the trail past its 4 MiB bound was written once by the
  rewrite and once by the append that followed it, so the operator read
  it as two actions.
- **The database page shows its row counts.** The table list was sized
  against the table names alone, so the count at the end of every line
  was clipped off the right edge and never drawn.
- **The shell and the one-shot commands agree.** `.backups` drew the
  `behind` backup state as an alarm while `dg backup status` drew it
  as a quiet line; `.schema` returned the CREATE text alone where
  `dg schema` returns the columns, types, flags, indexes and foreign
  keys. Both now use the shared view.
- **An upload chunk no longer blocks every other transfer.** The
  chunk append held the transfer registry lock across the disk write,
  so status checks, aborts and snapshot reads queued behind it.
- **`dg execute` refuses a file it cannot use.** The SQL file was read
  whole and without a ceiling, and a FIFO with no writer blocked it
  forever. It is now read under the request body's own bound and
  refused if it is not a regular file.
- **The "open table" action has one implementation.** The one-shot
  grid it used to draw was unreachable — the action opens the rows
  browser — and it drew a different shape from the live one. It is
  gone.
- **The deploy guide says to create the backup root.** The gateway's
  unit carries it in `ReadWritePaths=` and both backup units run with
  `WorkingDirectory=`, so a host that followed the guide exactly could
  not start the gateway and could not publish a backup set. The guide
  now has a step that creates it, mode `0700`, owned by the service
  user.
- **The two halves of the snapshot cadence are compared.** The
  gateway's freshness alarm and the periodic digest's staleness verdict
  are judged by the same number, but the number was written twice, in
  two unit files, and nothing compared them. The deploy gate now reads
  both numbers out of the two units and fails when they differ, which
  is what makes the guide's "the two halves cannot disagree" true.
- **The restore-point publisher can fail.** A missing backup root made
  the whole publish a silent no-op behind a green unit, so the window
  the gateway reports froze at its last value with no failure signal.
  It now fails with a named reason, and a replica listing that could
  not be answered is published as "unknown" for that database instead
  of claiming the replica holds nothing.
- **The recovery policy names the flag the digest takes.** It is
  `--cadence-secs` on `dg backup digest`; `--backup-cadence-secs` is
  `dg serve`'s spelling, and copying it across is a usage error.
- **The deployment guide names the sidecar units it ships.** It said no
  unit was provided while three were: the replication sidecar and the
  re-render service and timer.
- **The documentation index registers every file under `docs/`.** It
  opens with "every file is one of: canonical, historical evidence,
  superseded", and 49 of the 135 files were named nowhere — including
  the release receipts and the Turso captures the surface comparison
  cites. They are registered now, and a deploy gate fails when a file
  is not, because the earlier one-off listing did not hold.
- **The surface comparison no longer documents a command that cannot
  be run.** `dg key tokens` takes the database positionally, and
  `docs/turso-surface-gap.md` wrote it as `--database`.
- **The surface comparison no longer claims a column chooser.** The
  Edit Data row was closed as "already at parity" on `f`/`o`/`c`
  covering "column choice"; `c` clears the filter, and neither the CLI
  nor the browser can choose a column. The Columns half is now an
  open difference.
- **The release receipts say which one carries the artifact hash.**
  Only the newer receipt records it; the older one records the tree it
  tested, which is what it does establish.
- **The TypeScript client tells a severed export from a write that may
  have committed.** The transfer phase moves bytes and commits nothing
  while still using POST and DELETE, so it now has its own semantics
  — the same third state the CLI and the Rust client carry — and a
  severed export is an interrupted transfer, not an unknown outcome.
- **The Rust client no longer prints the credential in its debug
  output.** The derived `Debug` on `Gateway` carried the full key; it
  is now hand-written and redacted.
- **Both clients carry the deployment fields `/v1/status` publishes.**
  The disk reading, the databases' total size and the retention window
  were dropped on the way out, so neither client could surface them.
- **The TypeScript client accepts every literal local origin.** It
  allowed only `127.0.0.1` and `[::1]`; the CLI and the Rust crate
  accept any literal local address, so the three clients no longer
  answer the same question differently.
- **The CI job's backstop covers the steps that follow the suite.** A
  30-minute timeout against a 1500-second suite budget left five
  minutes for the release build, the container build, the TypeScript
  suite and the deploy gates, so a suite that ran over its budget was
  cancelled instead of warned about.

## [0.10.0] — 2026-10-30

### Added

- **Protection and undo for an ordinary delete mistake.** `dg protect <db>` sets a persistent per-database deletion guard, and `dg protect <db> --off` clears it explicitly. `dg delete` refuses a protected database. `dg undelete <db>` clears a tombstone only while its retention window is open and the original file remains; a purged database is reported distinctly from a name that never existed.
- **One-command revocation for a leaked database scope.** `dg key revoke --database <db> --confirm <db>` removes every application key scoped to that database in one server-side transaction, names the keys it removed, and explicitly leaves the owner key untouched. The single-key `dg key revoke <name>` form is unchanged.
- **Machine-readable `dg inspect` matches the terminal.** Piped and `--json` output now includes database details, activity, backup state and tables; unavailable status or backup reads are marked unavailable rather than failing the whole read.
- **A database's contents, and a table's rows.** Two questions were
  missing from the terminal, and their absence made seven full databases
  look like seven empty ones:

  `dg contents` (and `GET /v1/contents`) answers **what a database
  holds**: its table count and its total row count, read from the file.
  The databases tab now leads with those two columns. Every column it
  used to show — `rows read`, `rows written`, `waits`, `last write` — is
  an in-memory counter that a gateway restart zeroes, so a freshly
  restarted gateway drew every database with nothing beside it. Those
  counters are still there, now marked and footnoted as **since
  restart**, and the counts that come from the files sit first. A
  database whose file cannot be read reports `unknown` rather than `0`:
  a fault must not be drawn as an empty database.

  `dg rows <database> <table> [page]` answers **what is in a table**:
  the data, ten rows a page, with `--sort <column>[:asc|:desc]` and
  `--where <condition>`. The TUI's browser is the same read with keys —
  `n`/`p` (or the arrows and the wheel) for pages, `o` to name the sort
  column, `f` to edit the filter, `c` to clear it, `s` for the table's
  structure, `esc` to leave — and its
  footer says `rows 21-30 of 62` with the sort and filter named beside
  it. Clicking a table in an opened database now lands in the rows,
  which is what "open" means; the structure is one key away.

  Opening a database opens a **page**: its tables down the left, the
  selected table's values filling the rest of the screen, with arrow keys or a click on a name to select a table and `enter` opening one in the
  rows browser. `esc` returns to the dashboard.

  A page rather than a pane, because a database is a place you go and a
  place needs room for the thing you came to look at: the earlier shapes
  — a note under the dashboard, then a full-width grid with no table list
  — either had room only for a summary or made the reader go back to the
  dashboard to change table.

  Sorting and filtering happen **in the statement, on the gateway**,
  never by discarding rows after they arrive: otherwise page 2 of a
  filtered view would be empty while the footer promised more. The sort
  column is validated against the table's real columns before it is
  written into the SQL, and the filter is checked by the same read-only
  rule the query endpoint uses — a filter that would modify data is
  refused, not run. Both surfaces draw through the shared view layer, so
  they cannot tell different stories.

  The sort column is **named, not assumed**: it used to cycle off the
  first column, which on a typical table is the `id` (the rowid alias),
  so "sorted by id asc" reordered nothing and every other column was
  unreachable from the view. The counts are also re-read on `r` and when
  a database is opened: the 30-second cadence keeps an idle view from
  counting every row on every poll, but it is not a promise that the
  figures are fresh when the operator asks.

- **The pointer opens databases, and the row under it is marked.** A
  click on a row that names a database opens it: the database's tables,
  with their row and column counts, appear in the last action. Clicking
  a table in that list opens that table — its columns, their declared
  types and their flags — so the mouse walks from a database to a table
  to what the table is made of. The keyboard's doorway stays where it
  was: Enter on a database row opens a table's *rows* with the database
  already typed in, so only the table name is left to type. The mark
  follows the pointer so the operator can see which row a click would
  take before taking it; the mark is the row's own background, the cells
  keep the colours that carry meaning, and nothing shifts when the
  pointer moves. The keys and queries tabs open nothing, and a click
  never reaches a destructive action.

### Fixed

- **A busy registry can no longer carry an undo past its deadline.** `dg undelete` compared the retention deadline against a clock read *before* it acquired the registry transaction, so a request that arrived inside the window, waited out another writer, and reached the row afterwards still cleared the tombstone. The clock is now read inside the same immediate transaction that observes and writes the row; `delete` and `purge_expired` read theirs in the same place so the published window is neither shortened by contention nor purged early. Covered by a deterministic test that holds the registry lock across the deadline.

- **The action list no longer panics on a terminal with no room.** The
  popup's size was clamped into the room the terminal has, and the
  clamp's bounds were not ordered before it was used: a pty with no
  window size (or a window narrower than the popup's minimum) made the
  range empty, and `u16::clamp` panics on an empty range. The action
  list is opened with one keystroke, so the view could die — and take
  the operator's terminal with it — before anything was read. The
  bounds are ordered now, and the popup is drawn in the room there is,
  which may be none.

### Added

- **`dg schema` says what a table is made of.** The row grid says
  which columns a table has and what is in them; nothing said what each
  column *is*. The new `GET /v1/databases/{name}/schema` route reports,
  per table, every column's name, its **declared** type, its not-null
  and primary-key flags and its default, the table's indexes (name,
  uniqueness, the columns each covers), its foreign keys (both columns
  and the update/delete actions), and the `CREATE` statement verbatim.
  `?table=<name>` narrows the answer to one table, and the narrowing
  happens on the server, so the document a pipe reads is the API's own
  document either way.
- **The structure is read from the file with SQLite's own PRAGMAs**,
  not parsed out of the `CREATE` statement: a client cannot run a
  PRAGMA (the query endpoint refuses it, because a PRAGMA can change a
  connection's behaviour), and SQLite knows things a parser has to
  guess -- which column of a composite key is the key, which index came
  from a `UNIQUE` constraint, and what the declared type really is.
  The gateway already opens the file read-only for `info`, so it reads
  the schema the same way.
- **One command, one view, two faces.** `dg schema <database>
  [table]` draws the structure in a terminal and prints the gateway's
  JSON unchanged on a pipe, and the TUI's new `schema <database>
  [table]` palette action draws the same structure through the same
  shared view layer (`src/views.rs`) -- the rule the rest of the TUI
  already keeps, so the two surfaces cannot tell an operator different
  stories about the same gateway.
- **The structure is read-only.** No DDL of any kind: no column,
  constraint or index is created, changed or removed by the new
  surface, and the row grid's own behaviour is unchanged. The tables
  listed are the operator's own; the gateway's `_dg_*` bookkeeping and
  the replication's internal tables stay out of the structure
  view, while `dg info` still lists every table in the file.
- **The route is typed in the TypeScript client**
  (`client/src/index.ts`): `gateway.schema(name, table?)` returns a
  validated `SchemaResponse` — columns, indexes, foreign keys and the
  `CREATE` statement — with its own test.

## [0.9.0] — 2026-10-26

### Added

- **A database written since the last publication is `behind`, not
  `stale`.** The backup-health state had no third value: a change
  written after the newest verified set was reported as a finding, so on
  an periodic producer every write made the periodic digest red for the hour
  after it, and a box that was merely busy was indistinguishable from a
  box whose backups had failed. An alarm that is red whenever anything
  happens says nothing about the backup, and an operator who reads it
  periodic learns to skip it.
  The owner now approves a snapshot cadence -- `dg serve
  --backup-cadence-secs`, `dg backup digest --cadence-secs` -- and a
  change that has waited no longer than that is `behind`: the producer
  is between publications. A change that has waited longer is `stale`,
  and only then does it ask for attention. With no cadence approved
  there is no `behind` and every unrepresented change is reported, which
  is the strict reading a deployment has before it has made the
  decision.
- **The lag is shown next to the state.** `lag_secs` says how long the
  newest unrepresented change has waited, so a row is a measurement
  rather than a verdict, and `cadence_secs` is published beside it. The
  terminal view gained a `lag` column and a `snapshot cadence` field for
  the same reason.
- **The deployment templates carry the cadence.** `dg.service` and
  `dg-backup-digest.service` pass it, so the gateway and the digest
  judge the same change by the same number and cannot disagree.

- **The rule is written down.** `docs/tui.md` states the backups-tab layout rule -- the freshness table is reserved its full row count first, the published-sets table keeps its minimum, and the field list takes the remainder and scrolls -- and the tab's own table entry
  names the snapshot cadence. `docs/api.md` documents `cadence_secs`,
  `lag_secs` and the `behind` state, `docs/observability-contract.md`
  gained section 2a with the three-state vocabulary, and
  `docs/recovery-policy.md` and `docs/deployment.md` record the Tier-2
  number and the units that carry it.

### Fixed

- **The backups tab could lose a database's state.** The freshness table
  has a row per database, and the facts above it took their room first,
  so on a short terminal the table lost a row: a database could be named
  as missing in the coverage block while its state was clipped off the
  bottom, and the operator was told a fact about a database they could
  not see. The freshness table is now reserved its full row count first,
  the published-sets table keeps its minimum promise, and the field list
  takes the remainder and scrolls -- on a wide deployment the thirty rows
  all stay on screen and the summary's facts are a keystroke away instead
  of gone.

## [0.8.0] — 2026-10-25

### Fixed

- **The published restore window was hours shorter than the replica's real
  one.** The bucket holds one prefix per database, and inside it the whole
  chain: the snapshot at level 9 and the compacted write-ahead log at
  levels 1, 2 and 3, which is what covers the time between snapshots. The
  publisher asked the replication tool for its **default** level, and the default
  is level 0 — one database's newest segment. So it published one row per
  database and a window that was a single instant. `example_database`
  was published as "Oct 16 07:30:03 to Oct 16 07:30:03" while the bucket
  held six files going back to 07:30:00 and forward to a snapshot on the
  17th; `inventory` was published as a point, and the bucket held
  twenty-one files spanning six hours.

  Every moment inside the real window was therefore reported as *"no —
  the window opens later"*, including moments the replica restores
  without complaint. The window now reaches back across every level the
  bucket holds, so the published window spans the full replicated history
  instead of the newest segment.

- **The window's end is now the moment the replica is current to, not the
  last time it happened to write a file.** A database that has not
  changed since its snapshot can still be restored to any later moment:
  the replica restores the snapshot, and the bytes are the same ones the
  replica holds now. When the sidecar is not running the state is frozen
  at the newest file, so that is the end. This changes what the published
  facts mean, which is why it is a minor release rather than a patch.

Proven against the replica's own answers, moment by moment: the plan
now says *yes* for the moments the replica restores and *no* for the one
it refuses, and a real restore to a mid-window moment produced a copy
with the same tables as the live database.

## [0.7.0] — 2026-10-25

### Added

- **The backups tab says where the off-host copy is and when it was last
  synced.** The facts were in the API all along -- bucket, endpoint,
  region, how long the bucket keeps its history, the snapshot cadence,
  whether the registry is replicated, when the host published them, the
  last attempt, the last restore drill and the point-in-time window --
  but the tab could not show them: the per-database table took the room
  and the summary was pushed under the fold. The summary now keeps its
  room and the tables take what is left, and a table that does not fit
  scrolls.
- **The databases tab says how big each database is.** The traffic
  columns say what a database has been doing; the size says what it
  costs to keep. A hosted console puts the two side by side, and an
  operator watching a disk wants both.

### Fixed

- **A deleted database was reported as a stale backup.** The backup
  health judged every database that appeared in a published set,
  including the ones that had been deleted. Those were counted as
  `stale`, so the periodic digest exited `backup_unhealthy` over databases
  that had been deleted on purpose, and the per-database table filled
  with rows for things that no longer exist. A deleted database is not
  stale, it is gone: it is counted in `deleted` and never asks for the
  operator's attention.
- **The query log kept showing deleted databases.** The log lives in
  memory, so it outlives a deletion for as long as the process runs. The
  shapes of a database that is gone are history, not traffic, and the
  audit trail is where that history belongs; the log now leaves them
  out and counts them.
- **The last failure and the last drill each spent a heading of their
  own.** The last failure is about the last run, so it is a field of
  it; the drill's outcome and when it was reached are one thing said
  once. A tab that holds both on screen is a tab that fits.

## [0.6.0] — 2026-10-25

### Added

- **The live exercise can work against databases that are already there.**
  `--reuse` skips the creation and the schema and the rows, and gives the
  databases that are on the box the same spread of statements and the same
  walk of the surfaces. The reason it matters: the query log lives in
  memory, so it starts empty when the service restarts — and the first
  thing an operator wants from it is the names of the databases they
  actually have, not the names of a test set.

### Fixed

- **The live exercise could only be run once against a box.** A tombstone
  holds its name until it is purged, so the branch the exercise forked had
  to be named differently every time — the second run against the same
  gateway failed at "branch create" and took the rest of the branch
  lifecycle with it. The name is unique per run now.
- **A write in the live exercise could not be repeated.** The row it
  inserted had a fixed key, so a second run failed on a key that was
  already there. It replaces the row instead.
- **A credential the exercise could not issue was reported as a failure.**
  A credential whose name is taken cannot be re-read — its raw key was
  shown once and is not in the registry — so the checks that need it are
  now skipped, with the reason, rather than counted as failures.

## [0.5.0] — 2026-10-25

### Added

- **`scripts/live_exercise.py`: a harness for a DEPLOYED gateway.** The load
  harnesses in this repo all start their own `dg serve` on a temp directory,
  which is right — a load test on the box that holds the data is a load test
  on the data — but it left one question unanswered: does the gateway an
  operator actually reads still answer, and do the surfaces it shows have
  anything real in them? This one drives a *running* gateway through the
  real CLI. It creates databases with real schemas and rows, runs a spread
  of statement shapes so the query log holds a spread rather than one shape
  used once, issues scoped credentials and uses them so the audit trail holds
  more than one principal, walks every read-only surface, and checks the
  refusals as carefully as the successes — a gateway that answers everything
  is not a gateway, it is a hole. It never prints a credential, refuses to
  run against names that already exist, and `--cleanup` tombstones what it
  created.

### Fixed

- **The palette ran a title into its hint.** The title column was a fixed
  sixteen characters, and "rotate owner key" is exactly sixteen, so that row
  read "rotate owner keythe old owner key dies the moment the new one is
  issued" — two pieces of text mashed into one unreadable word. The column
  is measured from the titles actually on screen now, so a longer action
  added later cannot reintroduce the collision.
- **A branch could be left in the registry with nothing to delete it.** The
  branch delete tombstones the database first, and a database that was
  already tombstoned — by `dg delete`, say — answered that with 409, so the
  registry record was never removed: `dg branch list` went on showing a
  branch whose database was gone, and `dg branch delete` could not clear it.
  The record is removed either way now, and the answer says the database had
  already gone.

## [0.4.2] — 2026-10-25

### Fixed

- **The view said "unreachable" while the gateway was answering.** The
  connectivity indicator compared the number of failed reads against a
  threshold written down as "fewer than four". That was right when the view
  read four things and wrong the moment a fifth was added in 0.3.0, so a
  gateway that answered one read out of five was reported as down. The rule
  is counted from the answers now — reachable when it answered anything at
  all — and it cannot fall behind the number of reads again.
- **A gateway that hung froze the view for fifty seconds.** The five reads
  were made one after another, each with a ten-second budget, and the loop
  drew nothing until the last one came back. They are made together now, so
  a hung gateway costs one budget rather than five.
- **A tab could be scrolled into nothing.** The scroll offset was never
  clamped to the content, so scrolling past the last row left the offset
  running — ten presses down on a two-row table needed ten presses up to
  come back — and the two text tabs, the dashboard and the backups, could
  be scrolled into an empty screen, which reads as *there is nothing here*.
  The last row now stays on screen and a table that already fits does not
  scroll at all.
- **The actions list did not mark the irreversible ones.** *Delete
  database*, *revoke key* and *rotate owner key* sat beside *refresh* with
  the same weight, which tells the operator that the two are the same kind
  of thing. They now carry a `!` and the theme's colour for a destructive
  act — the colour that existed for this and had no use until now. The
  marker is a character and not only a colour, so it survives a monochrome
  terminal.

### Changed

- The bar's error says what happened rather than listing every failure —
  *four of five reads failed: forbidden* — because five refusals of the
  same kind are one fact, not five, and the bar has one line for it. A
  message too long for the line is cut with an ellipsis, so a message that
  runs off the edge says that it did.
- `docs/tui.md` said `1-5` tabs in the command-bar section after the bar
  said `1-6`. It says `1-6` now, and it records what the top bar claims
  about the gateway and what the scroll is allowed to do.

### Tests

- New: `tests/tui_refresh.rs` — the view says *connected* when one read
  of five answered, *unreachable* when none did, the reads are made
  together, and the error summarises the failures. The first two fail
  against the old rule and the third against the old order.
- New: `the_palette_marks_the_actions_that_cannot_be_taken_back`,
  `a_tab_cannot_be_scrolled_into_nothing` and
  `a_long_error_is_cut_and_says_so`.

## [0.4.1] — 2026-10-25

### Fixed

- **A narrow terminal no longer loses a tab.** The strip promises that every
  tab is named whether or not it is showing, and the sixth tab made the
  strip longer than an eighty-column terminal: the last tab was cut off,
  so a tab nobody could see was a tab nobody opened. When the line is too
  narrow for the whole strip, the **counts go and the names stay** — the
  counts are the summary, and the names are the promise. A hundred columns
  is enough for both, so nothing is lost on a normal terminal.

### Tests

- New: `a_narrow_terminal_still_names_every_tab` — the strip names all six
  tabs at eighty, a hundred, a hundred and twenty, and a hundred and
  ninety-one columns.

## [0.4.0] — 2026-10-24

### Added

- **Where the databases live is its own tab.** The placement table was a
  second box under the databases tab's traffic table, which split the body
  in two and squeezed both answers: the placement table took whatever the
  traffic table left, and on a screen with enough databases it was down to
  a header and a row. Placement is the sixth tab now (`6`), and each tab
  answers one question and shows one answer — which is the rule the view's
  own guide already states and the code did not keep. The tab answers "is
  everything live and working": a live registry row whose file is gone does
  no work at all, and no counter would say so.

### Changed

- **The tabs are written down once.** The list of tabs was written in six
  places — the strip, the number keys, the `tab`/`shift-tab` cycle, the
  command bar's range, the scroll offsets and the draw dispatch — and they
  drifted. The command bar said `1-4` while a fifth tab was on the screen,
  because only one of the six lists had been updated. Everything is now
  built from `Focus::ALL`, and the range in the bar is counted from it, so
  a list that is written once cannot drift from itself.
- `dg tui --help` names the six tabs and the `1-6` keys that reach them.
- `docs/tui.md` states the one-question-per-tab contract, says which
  question each tab answers, and records that the tab list lives in one
  place.

### Tests

- The placement assertions moved to the locations tab, and the
  "no placement reported" wording moved with them: a claim is made on the
  tab that answers the question it is about.
- New: `every_tab_answers_its_own_question_and_only_its_own` — a tab that
  shows another tab's answer is the tiling problem wearing a different hat,
  and it is invisible in a screenshot unless you know what to look for.
- New: `the_tabs_are_written_down_once_and_everything_is_built_from_that_list`
  — every tab is in the list, in the strip's order, and its number is its
  place in that list.

## [0.3.1] — 2026-10-24

### Fixed

- **The audit trail survives a restart.** The trail was held in the
  gateway's memory, so a restart emptied it — and a restart is very often
  the thing an operator is asking about after an incident: the question
  the trail exists to answer is "which credential was doing what when it
  went down", and the old answer was "nothing, the process died". The
  trail is now appended to `<data-dir>/audit.jsonl` (mode 0600, inside
  the data directory's 0700) as each entry is recorded, and the file is
  read back at startup, so the gateway comes back up still able to say
  what the credentials on it were doing. The endpoint, the filters and
  the CLI are unchanged; they simply answer across a restart now. The
  file is bounded like the log it serves: 2000 entries in memory, and the
  file is rewritten from those entries when it passes 4 MiB, so neither
  grows without limit. The query log stays in memory on purpose — it
  answers a present-tense question ("which statement is heavy right
  now"), and a restart costs it nothing.

### Changed

- `docs/api.md` and `docs/tui.md` said the terminal view had **four**
  answers after it grew a fifth tab; both now say five.
- The audit trail's lifetime is stated where the trail is described:
  durable across a restart, bounded in size, and never holding a
  credential's secret or a statement's values.

## [0.3.0] — 2026-10-24

### Added

- **The gateway knows which statement is the expensive one.** Every
  statement passes through the gateway on its way to SQLite, so the gateway
  is the only place that can say what each one cost. A hosted provider
  cannot answer that question without holding the data of every tenant, and
  nothing in their business depends on the answer; here there is no
  middleman and the operator owns the box. What is recorded is the
  statement's **shape** — its SQL with every literal replaced by a
  placeholder — so the log names a statement without quoting the data it
  carried: `SELECT * FROM users WHERE email = 'ada@example.com'` is kept as
  `SELECT * FROM users WHERE email = ?`. `GET /v1/queries` (owner-only)
  reports each shape's run count, rows read and written, total and slowest
  time, and the credentials that ran it; `dg queries` prints the same
  answer, and the view grows a fifth tab for it. The log is bounded per
  database — 200 shapes, 16 credentials each — because a query log that
  fills the disk is the same failure as any other disk exhaustion.
- **A backup set can be taken now, not only on the hour.** `dg backup
  run` asks the gateway for a backup set and the gateway takes one: the
  same directory, the same manifest, the same checksums and the same
  registry copy the host's periodic producer writes, so the two are
  indistinguishable to every reader. Each database is copied through
  SQLite's online backup API, every file is checked before the set is
  published, and the set is published with an atomic rename — a reader
  never sees a half-written set. The run takes the SAME lock the periodic
  producer takes, so the two producers exclude each other instead of
  publishing at once; a run that finds the lock held fails rather than
  waiting. Retention stays the producer's business: one policy, one
  place to change it.
- **A fork of a database that can diverge from it.** `dg branch create`
  takes a copy of a database — now, or at a moment in the past when that
  moment falls inside the backup window — and the copy is a real, writable
  database in the store, not a description of one. This is the thing a
  hosted provider will not give an operator, and it is cheap here because
  the fork is made from the same files the live database uses. The branch's
  parent and the moment it was taken are recorded durably in the data
  directory, so a fork made last week is still a fork today.
- **What differs between two databases.** `dg diff a b` compares two
  databases where they live: the schema (tables, columns, indexes) and,
  where the schemas allow it, the rows as multisets of their canonical
  text. The comparison is bounded and says so — a table compared only
  part-way is reported as truncated, never as equal. It exits nonzero when
  the two are not the same, so it can gate a deploy. It runs on the
  gateway, so no table data travels over the wire to answer a question
  about the data.
- **Which credential ran what, and when.** `dg audit` answers the question
  an operator asks after something unexpected. It records *actions* — a
  credential ran a statement, created a key, deleted a database — and never
  the data that moved: a statement is recorded by its shape, and a
  credential by its label, never by its key. The log is bounded, and a
  scoped application key cannot read it: the shapes and the actions are a
  view of every credential's traffic, which is the owner's information.
- **Three cheap questions about a database.** `dg integrity` runs SQLite's
  own integrity check and foreign-key check on a read-only connection and
  reports the messages verbatim — it never repairs anything, because a
  diagnosis that quietly changes the file is not a diagnosis. `dg vacuum`
  reclaims the space a file is wasting and reports the bytes it actually
  reclaimed, under the same write fence every other writer takes.
  `dg explain` says what SQLite would do with a statement: its query plan,
  in plain words, with no invented cost model, because SQLite's plan has
  none.
- **Named contexts.** `dg config set staging --server … --key-file …`
  gives a name to a server and a credential FILE, kept in
  `$HOME/.config/dg/config.toml` (0600, in a 0700 directory). Nothing
  secret is ever written down: the file holds paths, never a key. The
  context sits between the flag and the environment in the resolution
  order, because the environment says "where do I usually talk" and a
  context says "talk to THIS one now" — and a `--context` that names a
  context the file does not define is an error, not a silent fallback.
- **An agent surface.** `dg mcp` serves the gateway's read views to an MCP
  client over stdio, speaking JSON-RPC 2.0 by hand. It is read-only by
  construction: the catalogue is `dg_status`, `dg_list`, `dg_info`,
  `dg_inspect`, `dg_backups` and `dg_query`, and `dg_query` rejects a
  statement that would write before it sends anything. The credential is
  the one the CLI already resolved; an agent never supplies one. Stdout is
  the transport, so every diagnostic goes to stderr.
- **A plan for a restore.** `dg restore plan --to <ISO8601>` says what a
  restore to a moment would reach, database by database. The window is
  per database — a database whose replica starts later than the others
  cannot be taken back to a moment they can — and the plan names which is
  which before anyone asks for the restore.
- **The data can be taken back to a moment, not only to the last
  snapshot.** The off-host replica keeps seven days of write-ahead log,
  so the copy can be restored to any point inside that window — the
  difference between "there is a backup" and "you can go back to last
  Tuesday". `scripts/restore-point.sh` is the host-side interface: `--list`
  publishes the window the replica offers, `--plan` prints what a restore
  to a moment would fetch, and `--apply` restores one database into a
  *fresh* directory, verifies the copy (integrity, tables, rows) and
  publishes what it produced. It never writes over a live database: a
  restore is a copy you compare and then import, not a silent
  replacement. The host publishes the window and the result; the gateway
  reports them — `GET /v1/backups` carries `restore_points` and
  `last_restore`, the backups tab shows the window beside the last
  restore, and `dg restore list` prints them. The bucket's credentials
  never leave the gateway environment file, and the published facts are a
  whitelist, so what is published names a place without opening it.
- **The view now says whether your copies can be trusted.** The backups tab
  answers the question that comes after "where is it": the last backup run
  and whether it worked, what the newest published set carries (and whether
  it carries the registry, without which the copies hold files that no key
  can open), where the off-host copy is — bucket, endpoint, region, retention,
  coverage — whether the sidecar that does the copying is running, and
  whether the off-host copy has ever been restored and compared against the
  live gateway. The service tab adds the room left on the disk the data
  lives on, and how long a deleted name is held. `GET /v1/status` carries
  the placement, the disk headroom and the retention; `GET /v1/backups`
  carries the run, the coverage, the replica and the drill. The facts about
  the off-host copy come from the host, which publishes them where the
  gateway reads them (the replication sync script and
  `scripts/bucket-restore-drill.sh`); a host that publishes nothing says so
  in words rather than leaving a hole where a fact should be.
- **The view now says where the gateway and its databases actually are.**
  The service tab reports the host, the address the listener *actually*
  bound, the data directory and the backup root; the databases tab adds a
  locations table under the traffic table, naming each database's file, its
  size, and whether it is live, tombstoned or missing. A gateway reached
  through a tunnel cannot be located by looking at the tunnel — and it is
  the machine on the other end that holds the data — so the gateway reports
  it: `GET /v1/status` carries an owner-only, additive `deployment` block,
  and `dg status` prints the same facts. A gateway too old to report its
  placement says so in words instead of showing an empty table, which would
  have claimed the opposite.
- **A stale install is now reported, not silent.** `scripts/setup.sh`
  replaces its old `tui --help | grep -q terminal` check — which passed
  against any build whose help mentioned "terminal", including one from
  before the view was redesigned — with a comparison of the installed
  binary's mtime against the newest source file. It warns, and `--strict`
  fails. The help text and module docs also stopped describing the tiled
  screen that no longer exists.
- **The terminal view is tabbed, themed and legible.** The four answers
  are tabs (`1`-`4`, or `tab`/`shift-tab`) instead of tiles, so a wide
  table gets the whole width instead of a fraction of it. The palette is
  a semantic dark theme in `src/tui/theme.rs`: colours are assigned to
  meanings once, every text colour meets WCAG AA against the background
  (the emphasised ones reach AAA) with the ratios asserted in tests, and
  colour is never the only signal. The command bar names the keys that
  work — `1-4`, `tab`, `↑↓`, `:`, `r`, `q` — because the palette behind
  `:` is invisible to anyone who has not already found it.
- **Bare `dg` is the terminal view.** With no command, `dg` opens the
  operator's screen rather than printing the subcommand list, so the
  program's name is the way to the surface it describes. It only does so
  where there is a terminal to take over: a pipe, a redirected stdin or
  `--json` behaves exactly as it did with a required subcommand — the
  usage on stderr, stdout empty, plain text (no colour escapes, even
  under `CLICOLOR_FORCE`), exit 2 — because a script reading the command
  list cannot drive a full-screen view, and a `--json` pipeline must not
  receive help prose where it saw silence. `dg tui` remains the
  explicit spelling of the same view.
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

### Changed

- **The gateway now runs the 0.3.0 line.** The gateway was upgraded
  under `docs/upgrade-rollback.md`: a per-database export snapshot, a
  byte-exact copy of the stopped data directory, and the previous binary
  kept for rollback. The upgrade was verified by comparing
  the user-table content of every database before and after: identical.
  The deployed binary now reports `0.3.0`, which is the tag `v0.3.0`, so
  the deployed bundle traces to exactly one tagged state again.
- **The service unit makes the backup root writable.** `dg backup run`
  publishes a set from inside the gateway, and `ProtectSystem=strict`
  makes every path outside `ReadWritePaths` read-only to the process.
  The unit now names the backup root in `ReadWritePaths` and passes it
  with `--backup-root`, so the gateway publishes where the operator
  expects. Without it, every on-demand run failed with an internal
  error — a confusing way to learn that the unit, not the command, was
  the problem.
- **The connection order now has a context in it.** `--server` and
  `--key-file` are read by the CLI rather than by the argument parser, so
  that a named context can sit between the flag and the environment: the
  environment says "where do I usually talk", a context says "talk to
  THIS one now", and the flag is the operator's word at the command line,
  which outranks both. The order is `--server`/`--key-file`, then
  `--context`, then `DG_SERVER`/`DG_KEY_FILE`, then the config file's
  default context, then the built-in defaults. The built-in defaults are
  unchanged, so a box with no config file behaves exactly as before.
- **The terminal view has a fifth tab.** The queries tab sits beside
  databases, service, backups and keys, and answers which statement is
  the expensive one. The number keys reach tabs `1-5` now.
- **The freshness reference is no longer labelled as an age limit.** The
  backups tab and `dg backup status` called it "stale after", which read
  as a threshold on the age of a set — and then showed sets older than it
  marked `fresh`, because freshness is decided by whether a committed
  change is covered by a verified copy, not by wall time (observability
  contract §2). It is now the **freshness objective**, the contract's own
  word for it: the reference that decides when an unrepresented change
  deserves an operator's attention, not an age at which a set expires.
  The API field is unchanged.
- **The gateway now runs the 0.2.0 line.** The gateway was upgraded from
  0.1.0 to this tree's 0.2.0 under `docs/upgrade-rollback.md`
  (per-database export snapshot plus a byte-exact copy of the stopped
  data directory, previous binary kept for rollback), and the host
  replication and restore-drill scripts were replaced with the versions
  that publish replica status and drill verdicts. The upgrade was verified by
  comparing the user-table content of every database before and after:
  identical. The deployed binary still reports `0.2.0`, which is now
  behind the `v0.2.0` tag; the next version bump is what restores
  "a deployed bundle traces to exactly one tagged state".

### Fixed

- **The backups tab shows each database's freshness again.** The shared view
  builds two tables for it — the published sets, and each database's
  freshness — and the view kept "the table" rather than "the tables", so the
  freshness table was drawn nowhere. It is the one the tab exists for, and
  `docs/tui.md` has always promised it. The test that should have caught
  this looked for the substring "fresh", which every screen contains in the
  word "refresh" on the command bar, so it passed on a screen that showed no
  freshness at all. It now checks the table and each state against its own
  row.
- **The tab's name is no longer said twice.** The strip names every tab and
  what is in it — "1 databases (2)" — and the body's frame title said the
  same words again, one row apart. The strip is the summary of every tab,
  including the ones you cannot see, so it keeps the name and the count; the
  body shows the answer and lets the strip say what it is.
- **The view now shows what its wait columns mean.** The shared view layer
  writes a footer on the traffic table — "wait p95 is this database's own
  recent waits; wait max is its worst single wait" — and the CLI prints it.
  The terminal view dropped it, so a column whose meaning was on screen
  nowhere was on screen. It is now a caption directly under the rows, as the
  CLI prints it.
- **The service tab no longer repeats the databases tab.** The tabbed view
  gave every answer a whole tab, and the service tab ended by drawing the
  per-database traffic table — the same rows the databases tab draws, under
  a second heading, on a second tab. It was the tiling problem returning in
  a new shape: a narrow pane squeezes a table, so the table was split
  across two tabs to show every column. The service tab is now about the
  service, and the per-database traffic lives in the databases tab alone —
  which now has the whole width, and therefore shows every figure, the
  `waits` count and the worst single wait included. Nothing is lost and
  nothing is said twice. The CLI's dashboard still carries the table:
  `dg status` has no other way to show it.
- **Installing no longer fails while a `dg` is running.** The install copied
  over the installed binary in place, and a running `dg` holds that file open
  for execution — so the copy died with a bare "Text file busy" at exactly the
  moment an operator is most likely to install one: their own view on screen.
  The install now replaces the *path* rather than the inode: the new binary is
  staged beside the old one, the old path is unlinked and the new one renamed
  into place. The running view keeps working on the code it started with, and
  the next launch gets the new binary. `scripts/install-binary.sh` does it, and
  `tests/operations_scripts.rs` pins the behaviour against a real running
  binary — including the precondition that a plain `cp` over it still fails,
  so the test cannot pass by never reaching the busy-file case.
- **The host audit no longer leaves a temp file behind on every run.** The
  syntax check redirected its output to a temp file and removed it only when
  the check *failed*, so every successful audit left a zero-byte file in
  `TMPDIR` — one per run, accumulating forever on a host that audits on a
  schedule. Both files are now removed on both paths, and a test pins it by
  pointing `TMPDIR` at a scratch directory and asserting it comes back empty.
- **The orphan-guard test can no longer report a survivor that died.** It
  asked whether a pid was alive by looking for its `/proc` entry, which a
  zombie still has: a server that had exited and was only waiting to be
  reaped looked exactly like a server that had ignored its parent-death
  signal. It also killed the launcher by assumption rather than by reading
  the server's actual parent from `/proc`, and discarded the kill's own exit
  status — so a kill that never ran looked like a broken signal. All three
  are now explicit: a zombie is not alive, the process that dies is the one
  `PR_SET_PDEATHSIG` actually watches, and a kill that fails says so.
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
- **A write waits for its answer; a read does not.** Ten seconds was the
  whole timeout, so a gateway that was busy answering turned a write
  into "outcome unknown" — the one state an operator has to reconcile by
  hand. A read still gives up in ten seconds (a retry costs nothing
  else); a write waits a minute, because the answer is what removes the
  doubt. The CLI, the Rust client (`crates/dg-client`) and the
  TypeScript client (`client/`) all follow that rule.
- **The TypeScript client's write budget was restored, and now pinned.**
  The rule above reached `client/` for four hours on 2026-10-22 and was
  then removed again by an unrelated commit: both constants
  (`WRITE_TIMEOUT_MS`/`READ_TIMEOUT_MS`) were deleted from
  `client/src/index.ts` and every request went back to one hardcoded
  ten-second timeout, so a busy gateway turned a TypeScript write into
  `outcome_unknown` exactly as before. Nothing failed, because no test
  asserted the budget. Both budgets are back, the TypeScript client now
  also classifies a request by the statements it sends (mirroring
  `dg_client::semantics_of`, so a read-only query can never be reported
  as a write that may have committed), and `client/test/timeouts.test.mjs`
  asserts the two budgets and both failure codes, so the regression
  fails a test instead of hiding in a diff.
- **A `dg serve` no longer outlives the process that started it.** Six
  leaked servers were found running on the maintainer's box, the oldest
  a day and eight hours old, every one reparented to systemd with its
  data directory already deleted; they came from test binaries and audit
  harnesses that were killed rather than reaped. `dg serve` now arms
  `PR_SET_PDEATHSIG` (and refuses to start if its launcher is already
  gone), `--detach-safe` opts out for a server that genuinely should
  outlive its launcher, and `scripts/check-orphans.sh` — run in CI and
  pinned by `tests/orphan_guard.rs` — fails on any orphaned server or
  leftover scratch directory, reporting without killing.
- **A read-only query is answered as a read.** The HTTP method cannot
  tell a read from a write — a query is a POST either way — so the
  statements decide, with the same classification the server uses. The
  CLI, the terminal view and the Rust client (`crates/dg-client`) all
  do it. A read-only query now gives up in ten seconds and is reported
  as an unavailable gateway, instead of waiting a minute and being
  reported as a write that may have committed when nothing could have
  committed. The terminal view says the same thing the CLI says: a
  query it cannot send is an unavailable gateway, and only a statement
  that writes is reported as a write whose outcome is in doubt.
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
  gained a placeholder — the periodic backup unit, which now passes the
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
running as a supervised non-production deployment, plus the 2026-10-19
audit hardening. Production is **not** accepted; see
`README.md` for the live status.

### Added

- Authenticated SQLite gateway: owner bootstrap, database-scoped API
  keys, SQL query/batch over HTTP with idempotent writes, interactive
  shell, JSON CLI, and a workstation web console.
- Database lifecycle: clone, export/import (validated, atomic), and
  tombstone delete with a typed-name confirmation gate.
- Backups: periodic verified snapshot sets, continuous replication to
  object storage, backup-health status/digest, and restore drills wired
  to systemd timers.
- Access edge: private-network service reached through an authenticated
  private path; public binds refused.
- Observability: request-pressure counters, backup-freshness signals,
  and alert notices under `docs/observability-contract.md`.
- Release identity: this changelog, the `vX.Y.Z` tag practice, and
  `SECURITY.md`. Every tracked script is lint-gated in CI
  (`scripts/lint-scripts.sh`), and the docs tree is indexed
  (`docs/README.md`) with dated evidence under `docs/evidence/`.

### Changed

- The deployed binary still reports `dg 0.1.0` until the next
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

First supervised deployment (retroactive entry, recorded
2026-10-19 when this changelog was adopted): the authenticated gateway
and CLI from milestones M1–M3, before the audit hardening above.
