# Turso vs `dg`: what we would be missing

**Turso surface read live 2026-10-29; `dg` side re-checked against 0.10.0 on
2026-10-30.** This revision replaces the 2026-10-26 inspection audit with a
complete surface comparison. It answers one question: if the operator moved a
workload from Turso to `dg` today, what would actually be missing?

- **`dg` side**: the 0.10.0 release (`v0.10.0`), which closed gaps 1, 4 and 5.
  Checked against `dg --help` for every verb and against the released binary's
  own help text; the live local gateway on `127.0.0.1:7432` is used for the
  operational rows. The TUI side rests on the 2026-10-28 TUI audit
  (`docs/audits/tui-audit.md`) plus its six tabs.
- **Turso side**: the `dracondev` console read live through the Chrome bridge
  on 2026-10-29, plus the installed `turso` CLI v1.0.15 inventory from
  `--help`. Turso was read only: no database was created, destroyed, imported,
  replicated or re-tokened, and no token was issued or invalidated.
- **What this revision did not re-read**: the Turso side. The 0.10.0 pass that
  produced this revision was bound to the committed 2026-10-29 captures — no
  `turso` command ran and no console page was opened — so every Turso fact
  below is as of 2026-10-29. A Turso capability shipped after that date is
  invisible here; where a row could have changed since, it says so.
- **Evidence**: the captures listed at the end. The October 2026 captures are
  retained unchanged as history and are marked as such wherever they are
  cited.
- **Which `dg` binary the comparisons were checked against**: the 0.10.0 build
  in this repository (`.cargo/target/release/dg`). The `dg` on `PATH` in this
  environment is still **0.9.0**, which predates `protect`, `undelete` and the
  composed `inspect` document — so a reader re-running a `dg --help` check
  against `PATH` will see those verbs missing and conclude, wrongly, that a row
  is overstated. Build or use the 0.10.0 binary to reproduce the CLI rows.
- **Tree state**: this revision changed documentation only — no source, API,
  storage-layout or TUI change, and no new `dg` feature. The `// MUTATION:`
  regression recorded under "A regression found and repaired during this pass"
  belongs to the 2026-10-29 edition, not to this one; that section is kept as
  history and no longer describes the tree.

The `turso` CLI is **not authenticated** in this environment (`turso auth
whoami` answers "You are not logged in"), so Turso behaviour is evidenced from
the live console plus the CLI's `--help` surface. Where a fact could only come
from a logged-in CLI call, this document says so rather than guessing.

---

## 1. Surface matrix

Two different verdicts, on purpose:

**Status** — the parity verdict. **have** = `dg` answers the same question;
**partial** = it answers part of it, or answers it only in one surface;
**missing** = no `dg` counterpart; **not-applicable** = structurally
inapplicable to a single-operator, single-node box; **unverified** = the
committed captures do not show this Turso surface, so no parity claim is made.

**Disposition** — the decision verdict, and the one this document is really
for. **already-gap** = a numbered gap already carries this, so the row defers
to it and adds no new work; **worth doing** = it clears the bar; **not worth
doing** = it does not, and the note says why.

The bar is deliberately not provider parity. A row is **worth doing** only if
it does one of two things for a lone operator: protects them from a realistic
mistake, or saves real time in their first week. Parity with a
multi-tenant cloud console is not a reason on its own — half this matrix is
`not-applicable` precisely because Turso is solving problems `dg` does not
have. Every `worth doing` and `not worth doing` names which side of the bar it
landed on.

Each row's evidence column distinguishes the live 2026-10-29 console read from
the installed CLI's 2026-10-29 `--help` read. Historical October captures are
called out by date wherever they support a comparison.

### 1.1 Database CLI and lifecycle

| Turso surface | `dg` equivalent | Status | Note | Turso evidence | Disposition |
|---|---|---|---|---|---|
| `turso db list` | `dg list` | have | Names visible to the credential. | CLI `--help` (2026-10-29). | not worth doing — already at parity; no work outstanding. |
| `turso db show <db>` | `dg info <db>` | have | Size, tables + row counts, created_at, tombstone state. | CLI `--help` (2026-10-29). | not worth doing — already at parity; no work outstanding. |
| `turso db inspect <db>` | `dg inspect <db>` | have | `dg inspect` composes database, activity, backup and tables in both the terminal and JSON forms; failed auxiliary reads are explicit. Gap 1 CLOSED in 0.10.0. | CLI `--help` (2026-10-29); `dg` 0.10.0. | not worth doing — closed by gap 1; at parity in 0.10.0. |
| `turso db inspect --queries` | `dg queries --database <db>` | have | Per-statement cost. `dg` answers it globally or per database, not as a flag on `inspect`. | CLI `--help` (2026-10-29). | not worth doing — already at parity; the cost question is answered by `dg queries`. |
| `turso db create` | `dg create` | have | Includes single-node database creation; Turso's location/group options do not apply here (§4). | CLI `--help` (2026-10-29). | not worth doing — already at parity; location/group options are inapplicable (§4). |
| `turso db create --size-limit/--schema/--type/--enable-extensions` | `dg create` + `dg execute` | partial | Turso can set a per-database size limit, choose schema-only mode, and enable extensions; `dg` creates an ordinary SQLite file and can initialize it with SQL, but has no corresponding per-database size or extension switch. See gap 6. | CLI `--help` (2026-10-29). | already-gap (6). |
| `turso db create --from-db [--timestamp]` | `dg branch create` | have | Copy now or from a point in time. | CLI `--help` (2026-10-29). | not worth doing — already at parity. |
| `turso db create --from-file` | `dg import --from <file>` | have | A local SQLite3-compatible file, the same source `dg import` takes (128 MiB ceiling). | CLI `--help` (2026-10-29). | not worth doing — already at parity. |
| `turso db create --from-dump` / `--from-csv` | — | missing | A local SQL dump or a CSV, loaded as a new database. `dg import` accepts a SQLite file only. See gap 6. | CLI `--help` (2026-10-29). | already-gap (6). |
| `turso db create --from-dump-url` | — | missing | Fetches a remote SQL dump over the network and creates the database from it. `dg` has no URL fetch path at all — every import source is a local file. See gap 6. | CLI `--help` (2026-10-29). | already-gap (6). |
| `turso db destroy` | `dg delete --confirm <name>` | have | `dg` tombstones then purges after retention; requires the name typed twice. | CLI `--help` (2026-10-29). | not worth doing — already at parity, and safer: typed confirmation plus tombstone retention. |
| `turso db update` | — | not-applicable | Deploy is a binary swap; `docs/deployment.md`. | CLI `--help` (2026-10-29). | not worth doing — structurally inapplicable; deploy is a binary swap. |
| `turso db unarchive` | — | not-applicable | No archival tier; local files are always present. | CLI `--help` (2026-10-29). | not worth doing — structurally inapplicable; there is no archival tier. |
| `turso db locations` | — | not-applicable | Single node. | CLI `--help` (2026-10-29). | not worth doing — structurally inapplicable; single node. |
| `turso db replicate` | — | not-applicable | No regions or read replicas. Recovery is `dg restore` + `dg backup verify`. | CLI `--help` (2026-10-29). | not worth doing — structurally inapplicable; no regions or replicas. |
| `turso db config delete-protection` | `dg protect <name>` | have | Persistent registry guard; `dg delete` refuses until `dg protect <name> --off`. Gap 5 CLOSED in 0.10.0. | CLI `--help` (2026-10-29); `dg` 0.10.0. | not worth doing — closed by gap 5; at parity in 0.10.0. |
| `turso db config attach` | — | not-applicable | No cross-database ATTACH; each database is its own file behind its own route. | CLI `--help` (2026-10-29). | not worth doing — structurally inapplicable; isolated files, no ATTACH. |
| `turso db export` | `dg export --out <file>` | partial | Both write a local SQLite snapshot, but the surfaces differ: Turso also exports the WAL (`<db>.db-wal`), offers `--overwrite` and `--with-metadata`, and defaults the output name. `dg export` refuses an existing file outright — `--out` is required and "an existing file is refused and the download is published atomically" — and has no WAL or metadata option. See gap 7. | CLI `--help` (2026-10-29). | already-gap (7). |
| `turso db import` | `dg import --from <file>` | partial | Both take a local SQLite file. `dg import` requires the target name, refuses one that already exists, and enforces a 128 MiB ceiling; Turso takes the filename positionally and offers `--group` placement (not applicable here, §4). See gap 7. | CLI `--help` (2026-10-29). | already-gap (7). |
| `turso db tokens create/invalidate <db>` | `dg key create`, `dg key revoke --database <db>` | have | The operator can issue database-scoped keys and cut all scoped application keys in one confirmed command; the owner key is explicitly excluded. Gap 4 CLOSED in 0.10.0. | CLI `--help` (2026-10-29); `dg` 0.10.0. | not worth doing — closed by gap 4; at parity in 0.10.0. |
| `turso db tokens create --expiration/--permissions/--attach/--group` | `dg key create --role/--database` | partial | Turso supports expiry and table-action permissions; `dg` grants a non-expiring database-wide role. Expiry, per-table scopes and ATTACH/group claims are provider credential governance, not a single-operator gap (§4). | CLI `--help` (2026-10-29). | not worth doing — fails the bar: expiry and per-table scopes are provider credential governance for a multi-tenant service. The lone operator already holds the database files, so a scoped token protects them from nothing they cannot already do. |
| `turso db shell <db>` | `dg shell [db]` | have | Optional preselection and piped SQL; `dg shell` also has operator dot-commands. | CLI `--help` (2026-10-29). | not worth doing — already at parity. |

### 1.2 Database page (console per-database tabs)

Console tabs: **Overview**, **Branches**, **Analytics**, **Top Queries**,
**Edit Data**.

| Turso surface | `dg` equivalent | Status | Note | Turso evidence | Disposition |
|---|---|---|---|---|---|
| Overview → Activity (rows read/written, storage, syncs) | `dg inspect`, `dg status`, TUI service/databases tabs | have | `dg` adds queue-wait percentiles Turso does not expose. | Console (2026-10-29). | not worth doing — already at parity, and `dg` reports wait percentiles Turso does not. |
| Overview → Connect → Database URL | `dg status` → `listen`, SSH tunnel | not-applicable | `dg` reports the bound loopback address; there is no public URL to mint. Remote access is an SSH tunnel (§4). | Console (2026-10-29). | not worth doing — structurally inapplicable; there is no public URL to mint. |
| Overview → Create Token | `dg key create --role … --database …` | partial | Database-scoped roles match; Turso token expiry and table-action controls are provider governance, not a single-operator gap (§4). | Console + CLI `--help` (2026-10-29). | not worth doing — fails the bar: the owner already mints database-scoped keys directly, so console token UI is convenience, not protection. |
| Overview → Export → Download SQLite File | `dg export --out` | have | | Console (2026-10-29). | not worth doing — already at parity. |
| Overview → Delete Database | `dg delete --confirm <name>` | have | Typed confirmation and tombstone retention. | Console (2026-10-29). | not worth doing — already at parity. |
| Overview → Allow ATTACH | — | not-applicable | Cross-database ATTACH is outside `dg`'s isolated-file model (§4). | Console (2026-10-29). | not worth doing — structurally inapplicable; isolated-file model (§4). |
| Overview → Delete Protection | `dg protect <name>` | have | A persistent guard blocks `dg delete`; `dg inspect` reports whether it is on. Gap 5 CLOSED in 0.10.0. | Console (2026-10-29); `dg` 0.10.0. | not worth doing — closed by gap 5; at parity in 0.10.0. |
| Overview → Block Writes | — | missing | No per-database write switch; stopping the service or revoking credentials takes every database down. See gap 3. | Console (2026-10-29). | already-gap (3) — carried there as *worth doing*: it protects a lone operator from self-inflicted corruption during an incident. |
| Overview → Block Reads | — | not-applicable | A read cannot damage a database, so blocking reads addresses a party you do not trust with the data — a tenant, not the operator who owns the file. §4. | Console (2026-10-29). | not worth doing — structurally inapplicable: a read cannot damage a database, so this guards a tenant, not the operator who owns the file. |
| Overview → Restrict Access (CIDR / VPC allow-list) | — | not-applicable | Provider firewall for shared infrastructure. `dg` is loopback-only with remote access by SSH tunnel, so network admission is answered by the host and the credential; §4. | Console (2026-10-29). | not worth doing — structurally inapplicable: loopback-only plus SSH tunnel puts network admission on the host. |
| Overview → Invalidate All Tokens | `dg key revoke --database <db>` | have | Revokes every application credential scoped to the database in one confirmed command; the owner key is never revoked. Gap 4 CLOSED in 0.10.0. | Console (2026-10-29); `dg` 0.10.0. | not worth doing — closed by gap 4; at parity in 0.10.0. |
| Branches → Create From Now | `dg branch create <parent> <name>` | have | | Console (2026-10-29). | not worth doing — already at parity. |
| Branches → Create From Point-in-Time | `dg branch create --from <ISO8601>` | have | Reports the backup window when the moment falls outside it. | Console (2026-10-29). | not worth doing — already at parity. |
| Branches → list | `dg branch list`, `dg branch show` | have | | Console (2026-10-29). | not worth doing — already at parity. |
| Analytics (per-database, 30-day series) | — | missing | See gap 2. | Console (2026-10-29). | already-gap (2) — carried there as *not worth doing yet*: a counter series is a storage/retention decision with no first-week question forcing it. |
| Top Queries (per-database) | `dg queries --database <db>` | partial | Same ranking facts; `dg` shows statement SHAPES, Turso shows literal text. See §4.1 (deliberate: shapes are shown, not values). | Console (2026-10-29). | not worth doing — fails the bar: `dg` already answers the ranking question. Showing statement shapes instead of literal text is a deliberate privacy choice (§4.1), not a defect to fix. |
| Edit Data → DATA grid | `dg rows`, TUI rows browser | partial | Read-only; no cell editing, no Add record. See §4.1 (deliberate: `dg` reads, never edits). | Console (2026-10-29). | not worth doing — fails the bar: `dg` reads and never edits by design (§4.1). Cell editing on a live operator database is a way to make the mistake this gateway exists to prevent, not a time-saver. |
| Edit Data → Filters / Sort / Columns | `dg rows --where/--sort`, TUI `f`/`o`/`c` | partial | Capability parity, ergonomics differ: flags and typed conditions instead of builders. See §4.1 (deliberate: `dg` reads, never edits). | Console (2026-10-29). | not worth doing — fails the bar: `dg rows --where/--sort` and the TUI's `f`/`o`/`c` already cover filter, order and column choice. The remaining difference is builder-vs-flags ergonomics, which saves no real time. |
| Edit Data → STRUCTURE | `dg schema <db> [table]` | have | Columns with declared types and flags, indexes, foreign keys, `CREATE` text. | Console (2026-10-29). | not worth doing — already at parity. |
| Edit Data → SQL console | `dg shell`, `dg query` | have | Different transport (authenticated HTTP API vs direct console), same capability. | Console (2026-10-29). | not worth doing — already at parity. |

### 1.3 Organisation-level console pages

| Turso surface | `dg` equivalent | Status | Note | Turso evidence | Disposition |
|---|---|---|---|---|---|
| Sidebar → Create Database | `dg create <name>` | have | Both create an empty database the credential can then reach; `dg` takes the name on the command line rather than opening a dialog, and offers no size/group/location options (§4). | Console (2026-10-29). | not worth doing — already at parity. |
| Sidebar → Create Group | — | not-applicable | A group is a placement-and-replication container that databases are created into. `dg` has no groups, regions or replicas (§4); every database is one local file, so there is no group to create. | Console (2026-10-29). | not worth doing — structurally inapplicable; a group is a placement-and-replication container and `dg` has neither. |
| Databases list (rows read/written, bytes synced, storage, group, status) | `dg list`, `dg contents`, TUI databases tab | partial | `dg` has rows read/written, storage, and contents from the files. Groups are a cloud construct (§4); "bytes synced" is replication accounting and provider-imposed write-block status is inapplicable. The storage/counter views are present-tense only; See gap 2 for missing trends. | Console (2026-10-29). | already-gap (2) for trends — carried there as *not worth doing yet*; the present-tense columns are at parity. |
| Databases list → Restore deleted databases | `dg undelete <name>` | partial | A tombstone can be restored by name inside the retention window. The console offers a list you click; `dg` requires you to know the name: `dg status` reports how many databases are tombstoned but not which, `dg list` shows live names only, and `dg inspect <name>` reports the state of a name you already hold. See gap 8. | Console (2026-10-29); `dg` 0.10.0. | already-gap (8). |
| Analytics (org): quota cards | — | not-applicable | No quotas to meter against. | Console (2026-10-29). | not worth doing — structurally inapplicable; no quotas to meter. |
| Analytics (org): Latency / Rows Read / Rows Written / Inbound / Outbound / Syncs / Storage 30-day charts | — | missing | See gap 2. | Console (2026-10-29). | already-gap (2) — carried there as *not worth doing yet*: this is the measured trend gap itself. |
| Analytics (org): Top Databases ranked by rows read | TUI databases tab, `dg status` | partial | Present-tense ranking only; no trend. See gap 2. | Console (2026-10-29). | already-gap (2) — carried there as *not worth doing yet*: ranking exists; only the trend is missing. |
| Analytics (org): Top Queries ranked by rows read | `dg queries` | partial | See §4.1 (deliberate: shapes are shown, not values). | Console (2026-10-29). | not worth doing — fails the bar: `dg queries` already ranks by cost, and shapes-not-values is deliberate (§4.1). |
| Audit Logs (Author, Event, Origin, Timestamp) | `dg audit` | have | `dg` adds the action, the database, and the statement shape. The observed visible history differs; Turso's retention rule is not exposed — See §4.1 (Turso retention undisclosed). | Console (2026-10-29). | not worth doing — already at parity, and `dg` adds the action, database and statement shape. |
| Billing / Usage / Upgrade / plan / invoices | — | not-applicable | No billing. | Console (2026-10-29). | not worth doing — structurally inapplicable; no billing. |
| Settings, Support | — | not-applicable | No account. | Console (2026-10-29). | not worth doing — structurally inapplicable; no account. |

### 1.4 Service, backup and terminal surface (`dg` only)

No Turso equivalent. This is the largest area where `dg` leads rather than
lags; see §3.

| `dg` surface | What it answers | Disposition |
|---|---|---|
| `dg serve` | Loopback-only authenticated server; remote via SSH tunnel. | not worth doing — the bar cannot weigh it: there is no comparable Turso surface, and this is a `dg` lead (§3), not a gap to close. |
| `dg status`, `dg watch` | Version, uptime, admission config, request totals/rate, error counts, wait percentiles, per-database traffic. | not worth doing — the bar cannot weigh it: Turso's nearest page is captured (§1.3) but this dashboard is a `dg` lead (§3), not a gap. |
| `dg tui` | The six-tab operator view (databases, service, backups, keys, queries, locations) plus the database page and rows browser. | not worth doing — the bar cannot weigh a whole interface against nothing; the per-tab mapping is §1.7, and the asymmetries there are already carried by gaps 2 and 4. |
| `dg backup run/status/digest/verify` | Backup freshness, the three-state `fresh`/`behind`/`stale` judgement against an approved cadence, offline manifest/hash-chain verification. | not worth doing — already shipped, and the bar is already satisfied: `fresh`/`behind`/`stale` against a cadence protects the operator from believing a stale copy is good. No Turso counterpart exists, so nothing is outstanding. |
| `dg restore list`, `dg restore plan` | The point-in-time window the off-host replica covers, and what a restore to a moment would reach. | not worth doing — the bar cannot weigh it: publishing the restorable window before a restore is attempted is a `dg` lead (§3), and no Turso equivalent is captured. |
| `dg vacuum`, `dg integrity`, `dg explain`, `dg diff` | Space reclamation, file soundness, query plan, and cross-database comparison. | not worth doing — the bar cannot weigh it: these answer questions only a file-owning operator can ask, and none has a captured Turso counterpart. |
| `dg config list/show/set/remove/path` | Named contexts (server + key file paths). | not worth doing — already at parity in substance (`turso config path/set`, §1.5); the extra verbs are `dg` lead, not a gap. |
| `dg mcp` | The gateway served to an MCP client over stdio. | not worth doing — the bar cannot weigh it: no Turso surface is captured, and exposing the gateway to an MCP client is a `dg` lead (§3), not a gap. |
| `dg execute`, `dg batch` | A SQL file as one atomic batch (32-statement cap); an atomic JSON batch file. | not worth doing — already at parity for the capability (§1.1 SQL access rows); atomicity is `dg` lead rather than a missing Turso feature. |

### 1.5 Remaining CLI command families

The database verbs are itemized in §1.1. The rest of the installed CLI v1.0.15
command tree is below; the `help` and `--version` entries are CLI metadata, not
product capabilities.

| Turso CLI verb(s) | `dg` counterpart | Status | Turso evidence | Note | Disposition |
|---|---|---|---|---|---|
| `turso config path/set` | `dg config path/set` | have | CLI `--help` (2026-10-29). | Both configure local CLI contexts. `dg` additionally supports list/show/remove. | not worth doing — already at parity, and `dg` adds list/show/remove. |
| `turso config cache` | — | not-applicable | CLI `--help` (2026-10-29). | Turso CLI's own cache management; not a gateway feature. | not worth doing — structurally inapplicable; CLI-internal cache, not a gateway feature. |
| `turso auth login/logout/signup/token/whoami/api-tokens` | `dg init`, `dg key`, `dg config` | not-applicable | CLI `--help` (2026-10-29). | Cloud account identity differs structurally from local owner keys and named contexts (§4). | not worth doing — structurally inapplicable; cloud account identity vs local owner keys and contexts (§4). |
| `turso group create/destroy/list/locations/rename/show/transfer/update/config/tokens/aws-migration/unarchive` | — | not-applicable | CLI `--help` (2026-10-29). | No groups, regions, remote transfers, or archival tier (§4). | not worth doing — structurally inapplicable; no groups, regions, transfers or archival tier (§4). |
| `turso org audit-logs/create/db-transfer/destroy/invites/invoice/jwks/list/members/switch` | — | not-applicable | CLI `--help` (2026-10-29). | Single owner, no organisations (§4). | not worth doing — structurally inapplicable; one owner, no organisations (§4). |
| `turso plan show` | — | not-applicable | CLI `--help` (2026-10-29). | No plan or usage billing. | not worth doing — structurally inapplicable; no plan or usage billing. |
| `turso dev` | `dg serve` | not-applicable | CLI `--help` (2026-10-29). | Turso's development emulator and `dg`'s durable local service are different operating modes; no separate throwaway-emulation mode is in scope for this single-node gateway (§4). | not worth doing — fails the bar: a throwaway emulator is not a task the operator performs on their own durable service (§4). |
| `turso quickstart`, `turso update`, `turso relax` | — | not-applicable | CLI `--help` (2026-10-29). | Onboarding, CLI self-update, and a joke command are not gateway features. | not worth doing — fails the bar: onboarding, CLI self-update and a joke command are not gateway capabilities. |
| `turso help`, `turso --version` | — | not-applicable | CLI `--help` (2026-10-29). | CLI metadata, not database/service capabilities. | not worth doing — fails the bar: CLI metadata, not a database or service capability. |

### 1.6 HTTP API surface (Turso platform / Hrana vs `dg` `/v1`)

Objective item 1 asks for the HTTP API each side exposes. This is the one
section where almost every Turso cell is **unverified**, and it is worth
being blunt about why: the committed 2026-10-29 baseline is a CLI `--help`
read plus a console screenshot/transcript read. Neither exercises an HTTP
request. The captures *name* the surface — `turso auth token` is described as
"the token used to authenticate you to Turso platform API", `turso dev`'s
`--auth-jwt-key-file` flag says the key "authenticate[s] clients in the Hrana
and HTTP APIs", and the console's Connect card shows a "Database URL" — but
naming a surface is not observing its routes, verbs, payloads or error
shapes. Those cells are marked unverified rather than guessed.

**How the no-Turso-contact bound was actually held (inspectable, not asserted).**
No Turso data call was made and no credential was used: `turso auth whoami`
answers "You are not logged in", and this host holds no Turso token store
(`~/.turso/` contains only the `sqld` bundle and the binary), so no authenticated
call could have succeeded. One disclosure for accuracy: the binary itself *was*
executed — twice, as the unauthenticated `whoami` above — while assembling the
trace, so the precise claim is "no data call, no credential, no console read"
rather than "never invoked". The commands, their output, and proof that the
`docs/evidence/` baseline is byte-identical to HEAD and predates this goal are
recorded in `docs/verification/no-turso-contact-2026-10-31.txt`.

The `dg` side is read from this repository's router, so every route below is
checkable against `src/api.rs` and the routers it merges.

| Turso HTTP surface | `dg` HTTP equivalent | Status | Turso evidence | Note | Disposition |
|---|---|---|---|---|---|
| Platform API token (`turso auth token`, `turso auth api-tokens`) | `GET /v1/keys`, `POST /v1/keys`, `DELETE /v1/keys/{name}`, `DELETE /v1/database-keys/{name}`, `POST /v1/keys/owner/rotate` | partial | CLI `--help` (2026-10-29): "the token used to authenticate you to Turso platform API" | Both issue and revoke bearer credentials. On the Turso side only the verb names are captured (`auth api-tokens`, `auth token`); whether platform tokens carry org/account scope or an expiry is **unverified** — the `--expiration` flag in this capture belongs to `turso db tokens create` (`turso-cli-help-2026-10-29.txt:291`), a different command. `dg` keys are owner-or-app, database-scoped and non-expiring (§1.1), which is verified in `dg key create --help`. Route shapes on the Turso side are **unverified** — the capture names the token, not its management API. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Platform API: create/list/delete database | `GET /v1/databases`, `POST /v1/databases`, `GET /v1/databases/{name}`, `DELETE /v1/databases/{name}` | unverified | None — no capture shows a platform API route | The `dg` verbs mirror `dg list`/`create`/`info`/`delete` over HTTP. Turso's platform API certainly exists behind the CLI and console, but the committed baseline does not show it, so the mapping is inferred, not observed. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Platform API: delete protection | `PUT /v1/databases/{name}/protection` (set), `DELETE` (clear) | unverified | None | `dg protect` is reachable over HTTP; Turso's equivalent is a console toggle in the capture (§1.2) with no API route shown. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Platform API: undelete / restore deleted | `POST /v1/databases/{name}/undelete` | unverified | Console only: "Restore deleted databases" sidebar entry (`turso-live-2026-10-29-databases.txt`) | The console affordance is captured; the API behind it is not. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Hrana over HTTP (versioned pipeline endpoints) | `POST /v1/databases/{name}/batch` | unverified | CLI `--help` (2026-10-29): `--auth-jwt-key-file` "authenticate[s] clients in the Hrana and HTTP APIs" | The pre-0.10 gateway in this repo had an Hrana-shaped `pipeline` route; 0.10 serves one JSON batch shape instead. Whether today's Turso Hrana versions differ is **unverified** — the capture names Hrana without describing it. Do not read this row as protocol parity. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Hrana over WebSocket | — | unverified | None | `dg` has no WebSocket surface at all. Whether Turso still exposes one is not shown by the baseline, so the difference is stated as unverified rather than as a `dg` gap. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| SQL execution (`/v2/pipeline` statement) | `POST /v1/databases/{name}/query` | unverified | None | `dg query` and `dg shell` use this route. Turso's request/response envelope is not captured. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Schema read | `GET /v1/databases/{name}/schema` | unverified | None | Backs `dg schema` and the TUI STRUCTURE page (console STRUCTURE *is* captured, §1.2). | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Table contents / row counts | `GET /v1/contents` | unverified | None | Backs `dg contents` and the databases tab's per-table row counts. The console shows the same facts through Edit Data → STRUCTURE/DATA (§1.2), but the route those pages use is not captured. | not worth doing — unverified: the captures show the console view, not the API behind it, so there is nothing to act on. |
| Inline/batched statements | `POST /v1/databases/{name}/query` (batch), `POST /v1/databases/{name}/batch` | unverified | None | `dg batch` takes an atomic JSON batch file (32-statement cap). Turso's batch envelope is not captured. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| File export/download | `POST /v1/databases/{name}/export`, `POST /v1/databases/{name}/export/begin`, `GET /v1/uploads/{id}`, `DELETE /v1/uploads/{id}` (abort), `POST /v1/uploads/{id}/chunk`, `POST /v1/uploads/{id}/commit`, `GET /v1/snapshots/{id}`, `DELETE /v1/snapshots/{id}` (release) | unverified | Console only: "Export Database — Download SQLite File" (`turso-live-2026-10-29-per-database.txt`) | The console download is captured; the HTTP transfer it performs is not. `dg`'s flow is begin/chunk/commit plus a snapshot handle. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| File import/upload | `POST /v1/databases/{name}/import`, `POST /v1/databases/{name}/import/begin`, `POST /v1/uploads/{id}/commit`, `DELETE /v1/uploads/{id}` (abort) | unverified | CLI `--help` (2026-10-29): `turso db create --from-file` exists | `dg import` is a local-file path with a 128 MiB ceiling (§1.1). Turso's upload API is not captured. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Clone / branch from a database | `POST /v1/databases/{name}/clone`, `GET /v1/branches`, `GET /v1/branches/{name}`, `DELETE /v1/branches/{name}`, `POST /v1/branch` | unverified | Console only: Branches → Create From Now / From Point-in-Time (`turso-live-2026-10-29-per-database.txt`) | The console buttons are captured; their API is not. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Service status / health | `GET /v1/status` | unverified | None | `dg status` reads it: version, uptime, admission config, traffic, wait percentiles, per-database counters, disk. No Turso health route is captured. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Backup / point-in-time state | `GET /v1/backups`, `POST /v1/backups/run` | unverified | None | `dg backup status` reads `/v1/backups` (replica, drill, restore_points). Turso's backup API is not captured at all — the console capture shows branches-from-a-moment, not a backup route. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Query statistics | `GET /v1/queries` | unverified | Console only: per-database Analytics and Top Queries (`turso-live-2026-10-29-per-database.txt`) | The console analytics pages are captured; any API behind them is not. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Audit log | `GET /v1/audit` | unverified | Console only: Audit Logs page (`turso-live-2026-10-29-audit-logs.txt`) | The console page is captured with its four columns; an org audit-log API is named only as `turso org audit-logs` in CLI help. Route and payload **unverified**. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Database maintenance (integrity/vacuum/explain) | `GET /v1/databases/{name}/integrity`, `POST /v1/databases/{name}/vacuum`, `POST /v1/databases/{name}/explain` | unverified | None | These are `dg`-only operator surfaces (§1.4); no Turso counterpart is captured, so this row records absence of evidence rather than a missing product feature. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Cross-database diff | `POST /v1/diff` | unverified | None | `dg`-only (§1.4). | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Database file placement / disk | `GET /v1/status` → `disk`, `databases` | unverified | None | Placement is a `dg`-only concept on a single node (§4); the Turso analogue is region/group, which is structurally inapplicable. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| API authentication model | Bearer credential in a header; owner vs database-scoped app key | unverified | CLI `--help` (2026-10-29): JWT key flag for Hrana/HTTP; `turso auth whoami` | `dg` uses one opaque bearer credential with an owner/app distinction and generation binding. Turso uses platform tokens for management and JWTs for data-plane Hrana. The two models are **not** equivalent, but the Turso side is unverified beyond the flag text. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Error envelope | `{"error":{"code":…,"message":…}}` with a stable `code` (`invalid_request`, `database_protected`, `retention_expired`, …) | unverified | None | `dg` error codes are a documented contract (`docs/api.md`). Turso's error shape is not captured. | not worth doing — unverified: the captures name this surface but show no route, verb or payload, so there is nothing to act on. Revisit only with a capture that exercises it. |
| Transport / exposure | Loopback only; remote access via SSH tunnel | unverified | Console only: "Connect → Database URL" (`turso-live-2026-10-29-per-database.txt`) | `dg` binds loopback and has no public URL to mint; Turso exposes a per-database "Database URL" and is reachable over the internet. That structural difference stands. The URL *shape* itself is **unverified** — the capture records the label "Database URL" (`turso-live-2026-10-29-per-database.txt:14`), not the string, and no capture contains a `libsql://` value. | not worth doing — fails the bar as a `dg` action: loopback-plus-SSH-tunnel is the safer configuration for a single-operator box, and adopting a public URL would add exposure to close a gap nobody has. |

**What this section is not.** It is not evidence that Turso's API is absent or
weak — the captures simply do not cover it. A future pass that is allowed to
call Turso could replace every `unverified` above with a real observation; until
then, claiming agreement or a gap in either direction would be inventing
evidence, which is exactly what the objective's bound forbids.

### 1.7 TUI tabs vs console pages (per-tab)

Objective item 1 names the tabs specifically — databases / tables / rows / keys
/ queries / backups / analytics — so this section maps them one for one instead
of leaving them as incidental mentions in §1.2–§1.3. The `dg` side is the real
six-tab `Focus` enum plus the database page and rows browser; the Turso side is
what the committed 2026-10-29 console captures actually show.

| `dg` tab | What it answers | Turso console counterpart | Status | Turso evidence | Note | Disposition |
|---|---|---|---|---|---|---|
| Databases | Which databases exist, what they hold and how big | Databases list | have | `turso-live-2026-10-29-databases.txt` | Both give names, storage and per-row counters. The console adds group and sync columns, which are cloud constructs (§4). | not worth doing — already at parity; groups and sync accounting do not exist here. |
| Database page → tables/ROWS | What one table holds, row by row | Edit Data → DATA grid | partial | `turso-live-2026-10-29-edit-data.txt`, `turso-edit-data-live-2026-10-29.png` | `dg` reads; the console reads *and writes*. Read-only is deliberate (§4.1), so the missing half is the half that can damage data. | not worth doing — fails the bar: cell editing on a live operator database creates the mistake this gateway prevents. |
| Database page → STRUCTURE | Columns, types, indexes, foreign keys, `CREATE` | Edit Data → STRUCTURE | have | `turso-structure-2026-10-27.txt` (+ `-view`, `-indexes` PNGs) | Captured STRUCTURE read shows the same facts: declared types, PK/NOT NULL flags, indexes, foreign keys. | not worth doing — already at parity. |
| Rows browser (filters/sort/columns) | Which rows match, in what order, which columns | Edit Data → Filters / Sort / Columns | partial | `turso-live-2026-10-29-edit-data.txt` ("Filters; Sort; Columns") | Same three questions; `dg` answers with `--where`/`--sort` and the `f`/`o`/`c` keys instead of builders. | not worth doing — fails the bar: flags already cover it, and the remaining difference is ergonomics, not time saved. |
| Keys | Which credentials exist, what they may reach, revocation | Create Token / Invalidate All Tokens | have | `turso-live-2026-10-29-per-database.txt` | `dg` shows scope and lets a whole database's app keys be revoked in one confirmed action; the owner key is excluded by design. Gap 4 CLOSED. | not worth doing — closed by gap 4; at parity. |
| Queries | Which statement shapes the service has run and what they cost | Top Queries / per-database Analytics | partial | `turso-live-2026-10-29-per-database.txt` | Both rank by cost. `dg` shows statement SHAPES; Turso shows literal text. Deliberate privacy choice (§4.1). | already-gap (2) for trends; otherwise not worth doing — ranking is at parity and shapes-not-values is intentional. |
| Backups | Are the copies any good — per-database fresh/behind/stale against an approved cadence, plus the off-host replica and last restore drill | — (none shown in the captures) | unverified | None — no Turso backups page is in the committed captures | Reading the captures alone, this tab is a `dg` lead (§3): it reads `replica`, `drill` and `restore_points` (`docs/recovery-policy.md`). But "not in the capture" is not "not in the product" — Turso may well have a backup surface the 2026-10-29 read never opened, so the comparison is marked unverified rather than claimed as a `dg`-only win. Any claim that Turso lacks backup tooling would need a capture that visits it. | not worth doing — unverified: there is nothing to act on until a capture covers Turso's backup surface, and `dg`'s own tab needs no work. |
| Locations | Where each database file lives and whether it is there | — (none shown in the captures) | unverified | Adjacent only: the Databases list shows a `Group` column (`turso-live-2026-10-29-databases.txt`) | This tab reports **file** placement on one host and missing/tombstoned state; it is **not** a tombstone list despite the name resemblance (see gap 8). The captures show a `Group` column but no per-database location page, and group/region placement is a cloud construct this single-node box does not have (§4) — so the counterpart is unverified rather than declared absent. | not worth doing — unverified: group/region placement is structurally inapplicable (§4), and nothing on the `dg` side needs work. |
| Service | Version, uptime, admission, traffic, queue waits | Overview → Activity / org Analytics | partial | `turso-live-2026-10-29-analytics.txt` | Present-tense in both; `dg` adds queue-wait percentiles. Trends are the missing half. | already-gap (2) — carried there as *not worth doing yet*: present-tense status is at parity; only the trend is missing. |
| — (no tab) | Trend over time: latency, rows read/written, traffic, syncs, storage | Analytics (org + per-database, 30-day series) | missing | `turso-live-2026-10-29-analytics.txt` | The measured gap, carried by gap 2 rather than duplicated here. | already-gap (2) — carried there as *not worth doing yet*: this is the measured trend gap, not a second one. |

**What this adds over §1.2/§1.3.** Those sections answer "does `dg` have this
surface?" row by row across the whole product. This one answers "if I am looking
at tab N, what am I looking at in the console?" — the question an operator
actually asks when moving between them. Two tabs (Backups, Locations) show no
console counterpart **in the committed captures** — which is an absence of
evidence, not evidence of absence, so both are marked unverified rather than
scored as `dg` wins. One console page (Analytics trends) has no tab. Those
asymmetries are the real content of the comparison. Only the Turso side is
under-observed here; the `dg` side is read from source.

---

## 2. Ranked gaps (operator impact first)

Ranked by what it would cost the operator in the first week. Each gap names the
evidence and whether it is fixable inside the current architecture.

**The lens this section is judged by.** This is one operator's own gateway on
their own machine, not a provider serving tenants. That distinction is not
cosmetic — it is what decides whether a missing control belongs here at all.
Turso offers Block Writes, CIDR allow-lists, per-table token scopes and group
placement because its operator is not the only party with access to the
database. `dg`'s operator already owns the machine, the data directory and the
key file. A control that exists to govern someone else is therefore not a gap
here, however prominent it looks in Turso's console.

A missing piece qualifies as a gap only if it does one of two things:

1. it protects the operator from a mistake or incident they could realistically
   have on their own box — self-inflicted corruption, a leaked credential, a
   fat-fingered delete, a script that cannot read what a human can; or
2. it would cost them real time in their first week.

Everything else is either provider ceremony (recorded as not-applicable in §4)
or a **deliberate difference** — a place where `dg` behaves unlike Turso on
purpose, which belongs in §4.1 rather than being counted as a defect. The
distinction matters: a difference that is a considered design choice is not a
debt, and listing it as a "gap" quietly promises a fix that should never come.

### Gap 1 — `dg inspect` degraded off a terminal (CLOSED in 0.10.0)

`dg inspect` rendered a rich database/activity/backup/tables view **only when
stdout was a terminal**. Piped or `--json`, it returned the plain info document
and dropped the activity and backup sections entirely.

Reproduced live on 2026-10-29 against the local gateway:

```
$ dg inspect --json inventory
{"created_at":1792931768,"name":"inventory","size_bytes":90240,
 "tables":[{"name":"_litestream_lock","rows":0}, … ],
 "tombstoned":false}
```

That was the `GET /v1/databases/{name}` document — no backup state, no traffic.
A script that wanted what the operator saw had to rejoin
`/v1/databases/{name}`, `/v1/status` and `/v1/backups` by hand.

**Disposition: worth doing, and done.** It passed the bar on the first test —
a script that cannot read what a human can is a mistake the operator makes
every time they try to automate the gateway — and it needed no API change.

**Closed in 0.10.0.** The same live read now returns one composed document:

```
$ dg inspect --json inventory
{"activity":{"rows_read":1765,"rows_written":4,"queue_waits":341,…},
 "activity_available":true,
 "backup":{"age_secs":2703,"newest_set":"20261030T081702Z","state":"fresh"},
 "backup_available":true,
 "created_at":1792931768,"name":"inventory","purge_after":null,
 "size_bytes":90240,"tables":[…],"tombstoned":false}
```

One struct (`Inspected` in `src/cmd_data.rs`) fetches the three reads once and
renders both forms from it, so the terminal view and the JSON view cannot drift
apart. The auxiliary-fetch rule is unchanged and now applies to both: a failed
`/v1/status` or `/v1/backups` degrades that section instead of failing the
command, and `activity_available` / `backup_available` let a script tell "nothing
happened" from "we could not ask" — a distinction the old degradation could not
express. Regression: `tests/cli_inspect_json.rs` (3 of its 5 tests fail against
the pre-0.10.0 function).

### Gap 2 — No usage history, only present-tense counters (high)

Turso's Analytics is 30-day time series for latency (with p50–p99 selectors),
rows read, rows written, inbound and outbound traffic, syncs and storage —
per organisation and per database.

`dg` has none, by explicit design. `src/store/metrics.rs` keeps a **60-second
ring** (`WINDOW_BUCKETS = 60`), and its own comment says: "This is deliberately
a WINDOW, not a time series … It answers 'how busy was it just now' without
becoming a metrics database." `dg queries` and `dg audit` are bounded logs of
individual events, not aggregates over time.

Cost: "what changed last week" is unanswerable. You can say what the box is
doing now and which statements were expensive, but not that writes tripled on
Tuesday.

**Disposition (2026-10-30): not worth doing yet.** It is the largest gap here
and it fails the bar for the opposite reason to the closed ones: the cost of a
rolled-up per-hour counter series on disk is a storage and retention decision,
while the benefit — answering "what changed last week" — arrives only once an
operator has a reason to ask it. Nothing in the first week forces the question,
and the present-tense counters plus `dg queries` answer "what is expensive right
now", which is the question an incident actually asks. Revisit if a regression
ever has to be reconstructed after the fact; that is the trigger, not a date.

### Gap 3 — No way to make one database read-only during an incident (medium)

Turso's per-database Configuration offers **Block Reads**, **Block Writes**,
and **Restrict Access** by CIDR block or AWS VPC endpoint. All are live
controls, read on 2026-10-29. (Allow ATTACH is not applicable to `dg`'s
isolated-file model; Delete Protection is gap 5.)

The CIDR/VPC allow-list is provider machinery, not a personal gap: `dg` listens
on loopback and reaches remote gateways through an SSH tunnel, so "who may
connect" is already answered by the network and the credential. Recorded in §4
rather than counted as a loss.

What does matter on a single-operator box is the write switch. If a bug in one
application starts corrupting its own database — plausible without any adversary
— `dg` offers no way to say "this database is read-only now" short of stopping
the whole service or revoking credentials, both of which take every other
database down with it.

Cost: a per-database read-only flag is a small, in-architecture addition
(a registry flag enforced at the route).

**Disposition: worth doing** for the self-inflicted-corruption case, because it
protects a lone operator from a realistic mistake — holding a database still
during an incident without taking the whole service down. **Not worth doing**
to imitate a firewall, which is a tenant boundary this box does not have.

### Gap 4 — Bulk credential revocation (CLOSED in 0.10.0)

Turso can invalidate every database token at once. Its CLI can also create
tokens that expire, restrict permissions to table/actions, and carry ATTACH or
group claims. `dg` credentials remain non-expiring and grant one database-wide
read-only or read-write role. The single-name `dg key revoke <NAME>` still
revokes exactly one key; the closed bulk form below cuts all application
credentials for one database in one confirmed command. `dg key list` shows every
key and its scope.

Judged for a personal box: expiry is mostly ceremony here — a key file on your
own disk does not benefit much from a calendar, and a non-expiring key is one
fewer thing to rotate. Table/action-level scopes and ATTACH/group claims are
provider concepts (§4). What survives that filter is the incident path: if a
single credential leaks, revoking it already works in one command, but if you
ever need to revoke *everything scoped to one database* you are looping, and the
whole point of that moment is speed.

Cost: before 0.10.0, bulk revocation was a bounded, in-architecture addition;
expiry and per-table grants would need an explicit policy and credential-schema
change, and are not recommended for a single-operator install.

**Disposition: worth doing, and done** (the bulk revoke only); *not worth
doing* for expiry and per-table grants. It cleared the bar as mistake
protection: an operator revoking a leaked database's credentials needed one
confirmed command, not a loop over key names. Expiry and per-table scopes are
provider credential governance, and the owner already holds the file.

**Closed in 0.10.0 — the bulk revoke only.** `dg key revoke --database <name>`
resolves the scope server-side in one transaction (`revoke_scoped_to` in
`src/store/auth.rs`, `DELETE /v1/database-keys/{name}`) and reports the names it
cut, not just a count. It is the write-side counterpart of `dg key tokens
--database`, which already answered "who can reach this database".

The one thing it must get right is the thing that would have made it dangerous:
**the owner key is never revoked by the bulk form.** An owner credential reaches
every database by construction, so it is always *in scope* for "who can reach
this database" — a literal sweep would revoke the credential the operator is
holding and lock them out of the gateway they were protecting. Both the SELECT
and the DELETE filter on `role != 'owner'`, and a store-level test
(`bulk_revoke_never_takes_the_owner_key_even_when_it_is_scoped`) writes an owner
row that *does* carry the scope, to prove the protection holds against the shape
it has to survive rather than only the shape `issue` happens to produce.
Removing either filter makes that test fail with `the bulk sweep took an owner
credential: ["app", "owner2"]`. Bulk revocation requires repeating the exact
database name with `--confirm`; the refusal text spells out that the owner key
is never revoked, the TTY success report repeats "owner key: not revoked", and
the JSON response includes `"owner_key_excluded": true`.

Credential expiry, per-table/action scopes and ATTACH/group claims remain
deliberately out of scope (§4) — that part of the original gap is unchanged.

### Gap 5 — Delete protection and undelete (CLOSED in 0.10.0)

Turso offers both Delete Protection and a **Restore deleted databases** control
in the database list. Before 0.10.0, `dg` required the typed `--confirm <name>`
(and the TUI's typed confirmation), then tombstoned the file until retention
expired; there was no command to undelete it. `dg restore list/plan` described
the off-host backup window but was not an undelete action.

Cost: confirmation protects against fat-fingers, but after a deliberate delete
or an incident there was no one-step way to bring the tombstone back.

**Disposition: worth doing, and done.** Both halves clear the mistake-protection
bar: the guard stops a stray delete, and the undo brings a name back without
reaching for the off-host backup workflow a tombstone does not need.

**Closed in 0.10.0.** `dg protect <name>` sets a durable registry guard;
`dg protect <name> --off` clears it. `dg delete` checks the guard inside its
registry transaction and refuses with `database_protected` before tombstoning.
`dg inspect` reports `protected` in JSON and the terminal view so the operator
can see the guard before acting.

`dg undelete <name>` clears `deleted_at` and `purge_after` only while the
published retention window is open and the original SQLite file still exists.
It refuses a live name (`not_deleted`), an expired tombstone
(`retention_expired`), a file that has already disappeared
(`database_file_missing`), and a name whose tombstone was physically purged
(`database_purged`) with distinct errors. The deadline is judged against the
clock read *inside* the registry transaction it writes under: a request that
arrives inside the window but waits out another writer is measured by the
instant it reached the row, so a lock cannot carry an undo past its own
deadline. `delete` and `purge_expired` read their clocks in the same place,
so the published window is neither shortened by a busy moment nor purged
early. Purge receipts retain only the
historical name and purge time, not database contents; if a name is later
reused, its live row wins and the old receipt cannot resurrect the old file.
The protection column migrates existing registries as unprotected, so an
upgrade never silently blocks a delete the operator did not guard. Regression
coverage is in `tests/cli_delete_protect.rs` and `src/store/tests.rs`.

### Gap 6 — Import sources are narrower than Turso's (low)

Turso `db create` accepts a local SQLite file, a local SQL dump (`--from-dump`),
a CSV (`--from-csv`), and a **remote** dump fetched by URL (`--from-dump-url`),
plus a per-database size limit, schema-only type and experimental extension
enablement. `dg import --from` accepts a regular SQLite file only (128 MiB
ceiling); `dg create` makes an ordinary SQLite file, initialisable with explicit
SQL, with no per-database size or extension switch.

Judged for a lone operator, most of this is convenience rather than harm: a
one-off migration means converting a CSV or downloading a dump to disk first,
which is a few minutes once, not a recurring cost. The size limit and extension
enablement are host/filesystem concerns and deliberately not exposed.

The one part worth weighing is `--from-dump-url`: `dg` has no network-fetch path
anywhere in its import surface, so importing a remote dump is a two-step
download-then-import. That is deliberate — an import source is a local file the
operator can inspect and hash before it enters the store — and it costs one extra
step in exchange for not asking the gateway to fetch arbitrary URLs.

Cost: low, one-off.

**Disposition: not worth doing.** It fails the bar: dump/CSV import is a source
format the operator can produce locally and import in one extra step, so the
time saved in the first week is minutes; and the URL-fetch path is deliberately
absent because asking the gateway to fetch arbitrary URLs trades a real safety
property for that step. Recorded so the parity claim is not overstated. Evidence:
`docs/evidence/turso-cli-help-2026-10-29.txt`.

### Gap 7 — `dg export`/`import` are narrower than Turso's (low)

Both sides move a database to and from a local SQLite file, which is why the
matrix once called these plain parity. Read against the captured help, they are
not the same surface:

- **Export.** Turso also exports the WAL (`<db>.db-wal` alongside the main
  file), accepts `--overwrite`, offers `--with-metadata`, and defaults the
  output name. `dg export --out` is required, and an existing file is refused
  rather than overwritten — the help says so verbatim.
- **Import.** Turso takes the filename positionally and accepts `--group`
  placement. `dg import` requires the target name, refuses a name that already
  exists, and enforces a 128 MiB ceiling.

For a lone operator the difference that matters is the export shape: a
WAL-inclusive or metadata-bearing export has no `dg` equivalent, so a
byte-for-byte replica of Turso's export cannot be produced. If a recovery plan
ever depended on restoring from a Turso-shaped export, that plan would need
adjusting.

Refusing to overwrite is deliberate and worth keeping: a snapshot that silently
replaced a previous export would destroy the very artifact an operator keeps for
recovery. But it is a real workflow difference — re-exporting to the same path
means deleting the old file first.

Cost: low.

**Disposition: not worth doing.** It fails the bar: a WAL-inclusive or
metadata-bearing export is not a task a lone operator performs, and refusing to
overwrite an existing export protects the artifact they keep for recovery.
Recorded so parity is not overstated. Evidence:
`docs/evidence/turso-cli-help-2026-10-29.txt`, `dg export --help`,
`dg import --help`.

### Gap 8 — Nothing lists which databases are tombstoned (low, newly noticed)

The 2026-10-29 console capture shows a **"Restore deleted databases"** entry in
the sidebar: a list of deleted databases you pick from to bring one back.

`dg undelete <name>` can bring a tombstone back, and `dg inspect <name>` reports
whether that specific name is tombstoned — but only for a name you already
hold. Nothing answers "what did I delete?". `dg status` publishes
`database_counts.tombstoned`, which is a **number**, not the names; `dg list`
returns live names only (`store::list` filters `deleted_at IS NULL`). So an
operator who knows something is missing, but not what it was called, cannot
enumerate the tombstones to find it.

This is deliberately **not** folded into gap 5: gap 5 closed the *protection*
and *undo* halves, and both remain closed. What is missing is the index between
them, which is why it gets its own number rather than reopening a closed one.

**Disposition: worth doing.** It clears the mistake-protection bar: the whole
point of tombstone retention is that a mistaken delete is recoverable, and the
recovery is only reachable if the operator can name the database they deleted.
It is also bounded — the registry already stores the tombstone rows, so this is
a read (`deleted_at IS NOT NULL`) rather than new state. Since this pass is
documentation-only, it is recorded here and no feature was added.

**Correction made in this pass:** the matrix previously credited
`dg locations` with listing tombstones. It does not — `dg locations` is not a
subcommand at all (the CLI has no `locations` verb; `Locations` is a *TUI tab*,
and that tab reports **file placement**, not deletion state). The row now says
what is actually true.

---

## 3. Where `dg` is ahead

Read from the live local gateway and the code, not asserted.

| Advantage | Evidence |
|---|---|
| **No quotas, no throttling, no invoice.** | The console shows "Quota Critical 99% used … your account may be throttled soon" and two databases in **Writes Blocked** status on every page. `dg` has no metering path at all. |
| **The operator owns the audit trail.** | Turso's live page showed 6 entries versus 23 in the Oct 26 capture, with 21 Oct 24 entries no longer visible and no disclosed retention rule (§4.1). `dg audit` is a 2000-entry durable JSONL file on the operator's disk, with action, database and statement shape. |
| **Statement shapes, never literal values.** | Turso's Top Queries prints `LIKE 'aig-session-%'` and `status='settled'`; `dg queries` prints `LIKE ?` and `status = ?`. |
| **Backups are first-class and verifiable offline.** | `dg backup status/digest/verify` judge each database `fresh`/`behind`/`stale` against an approved cadence, and `dg backup verify` re-checks a published set's manifest and hash chain with the gateway stopped. Turso's console offers download and point-in-time branches, not an offline verifier. |
| **Restore window is inspectable and planned before it is taken.** | `dg restore list` publishes the window (`oldest` → `newest`) per database from the live replica; `dg restore plan --to <ISO>` says what a restore would reach. |
| **Local SQLite tooling on the same box.** | `dg vacuum`, `dg integrity`, `dg explain`, `dg diff` — no counterpart in the console. |
| **Credentials are files, not bearer tokens in a cloud account.** | `dg key list/create/revoke/rotate-owner` against a 0600 key file; no `turso auth` surface to compromise. |
| **MCP server built in.** | `dg mcp` serves the gateway to an MCP client over stdio. No Turso equivalent. |
| **Admission control is visible.** | `dg status` and the TUI service tab publish read/write permits, acquire timeout, and per-database p95/max admission waits. The console publishes no such queue facts. |
| **One atomic batch verb.** | `dg execute` runs a SQL file as one transaction (split by SQLite's own `sqlite3_complete`, 32-statement cap) and `dg batch` runs an atomic JSON batch. |

---

## 4. Not applicable by architecture

Stated so a parity table cannot be mistaken for a to-do list.

- **Multi-region replication, locations, groups (`turso group …`, `turso db
  replicate`, `turso db locations`).** Single node by design. Durability is
  `dg backup` + the off-host replica, and recovery is `dg restore`.
- **Organisations, members, invites, plan, billing, invoices, usage quotas,
  `turso org …`, `turso plan`, `turso auth …`.** One operator, no tenancy, no
  metering-for-money.
- **Token signing infrastructure (`turso org jwks`, API tokens).** Credentials
  are hashed rows in the local registry, not signed JWTs.
- **`turso dev`, `turso quickstart`, `turso update`, `turso relax`.** The local
  binary already runs a real server against a real data directory.
- **Archival tiers (`turso db unarchive`).** Files are always present on disk.
- **Cross-database ATTACH.** Each database is an isolated file behind its own
  route pair; there is no shared connection to attach from.
- **CIDR / VPC network allow-lists.** These exist so a provider can decide which
  of its customers' machines reach a shared database. `dg` is a loopback
  listener, and remote access is an SSH tunnel, so the network already answers
  this and a per-database allow-list would be a second, weaker copy of the
  answer.
- **Token expiry, per-table/action scopes, group tokens.** Governance for
  credentials issued to other people. A key file owned by the same operator who
  owns the data directory gains little from a calendar or a per-table grant.
  Bulk revocation is different and is kept in gap 4, because it shortens an
  incident.
- **Block Reads.** Blocking **writes** protects a database from damage, which is
  why a per-database read-only switch is gap 3. Blocking **reads** protects no
  data at all — a SELECT cannot corrupt a file. It exists to stop a party you do
  not trust from *seeing* rows, which is a tenancy question: Turso's operator is
  answering it about its customers. The operator here is the only party with a
  key to the box, and if they distrust a reader the answer is `dg key revoke`,
  not a per-database read lock. Already-covered case: a leaked key is revoked
  (gap 4), not masked.

### 4.1 Deliberate differences

These are recorded here rather than in §2 because they are **not gaps**: each is
a considered design choice or an observation about Turso that asks nothing of
`dg`. Keeping them in the ranked list would have implied a debt that should
never be paid.

#### `dg queries` shows statement shapes where Turso shows literal values

Read live: Turso's Top Queries for `dracon-music-dev` shows statements with
literals intact, e.g. `WHERE status='settled'` and
`WHERE idempotency_key LIKE 'aig-session-%'`.

`dg queries` deliberately replaces every literal with `?`, showing the SHAPE:
`SELECT * FROM "products" ORDER BY rowid LIMIT ? OFFSET ?`.

This is **not a gap to close** — it is a privacy property the operator gains by
owning the box (`docs/tui.md`, "Which query is heavy"). Recorded here because a
comparison that lists only Turso's wins would be dishonest in the other
direction.

One real caveat found live: Turso's list includes the **console's own
introspection queries** (the first entry on `dracon-music-dev` was Drizzle
Studio's `sqlite_master`/`pragma_index_list` scan), so its "top query" is
measured over a different population than `dg`'s API traffic.

#### Edit Data is read-write; `dg` is read-only by design

The Edit Data grid is a full editor: `+ Add record`, editable cells, a Columns
visibility chooser, a Sort builder, a Filters builder, and an attached SQL
console. Read live through its shadow DOM (the page is a Drizzle Studio app
inside `<drizzle-studio>`, so it is invisible to `document.body.innerText`).

`dg rows` and the TUI rows browser give the same three reading capabilities —
sort, filter, column data — as `--sort`/`--where` flags and `o`/`f`/`c` prompts.
They never write. Every mutation in `dg` is an explicit statement (`dg query`,
`dg execute`) or a confirmed lifecycle action.

Cost: an operator cannot click a cell and fix a typo. That is intentional —
`docs/tui.md` states the mouse "deliberately stops at reading", and every write
needs its own statement so it lands in the audit log with its shape. Recorded
as a deliberate non-goal, not a missing feature.

#### Turso's audit retention is undisclosed

Read live on 2026-10-29: Turso's Audit Logs page reports **"Showing 1-6 of 6
items"** with all paging buttons disabled. The 2026-10-26 capture of the same
page listed **23 entries**, including 21 CLI-created tokens from Oct 24. Those
entries are no longer visible, but the console does not expose a retention
setting or explain their disappearance; the observation is consistent with
age-based expiry, not proof of its exact policy or cutoff.

`dg audit` is bounded differently: `src/store/audit.rs` keeps `MAX_ENTRIES` =
2000 entries and rewrites its trail file above `MAX_FILE_BYTES` = 4 MiB,
dropping the oldest. That is a **count/size** bound, not a time bound, and the
trail is durable across restarts (`audit.jsonl` in the data directory).

So: on a quiet box `dg` keeps its whole trail indefinitely; on a busy one it
turns over by count. Turso's observed view showed fewer entries five days later,
but the cause is undisclosed.

**Disposition: not worth doing.** It fails the bar: `dg`'s own trail is durable
and bounded by count, and nothing an operator does in their first week depends
on matching a retention rule even Turso does not disclose. Recorded so the
retention claim is not overstated; operators should not assume a Turso retention
period from these two snapshots.

#### CLI mapping drift (already corrected)

The October mapping had stale or invented CLI spellings. The installed v1.0.15
help tree confirms there is no `turso db execute`, `turso db tokens list`, or
`turso db tokens revoke`; the SQL command is `turso db shell <db> <sql>`, and
tokens have `create` and `invalidate` (bulk invalidation) only. The mapping doc
now says so and distinguishes Turso's bulk token invalidation from `dg`'s
single-key revoke. It also records `dg key rotate-owner` exactly as spelled by
`dg key --help`.

**Disposition: not worth doing** as runtime work — corrected as documentation
instead. The stale rows implied no behavior difference, so the only defect was
the text; the correction lives in `docs/cli-turso-mapping.md`. The CLI help capture
is `docs/evidence/turso-cli-help-2026-10-29.txt`.

A note on what this section is *not*: it is not a claim that `dg` is complete.
Gaps 1–8 are real, and the two that protect a lone operator from ordinary
mistakes — making one database read-only mid-incident (gap 3) and undoing a
delete (gap 5) — survive this filter precisely because they are not about
governing other people.

---

## Evidence

Live captures made 2026-10-29 (this revision):

- `docs/evidence/turso-cli-help-2026-10-29.txt` — the installed Turso CLI v1.0.15
  version and top-level / command-family help inventory, captured locally.
- `docs/evidence/turso-live-2026-10-29-databases.txt` — the Databases list,
  all 19 rows with rows read/written, bytes synced, storage, group, status.
- `docs/evidence/turso-live-2026-10-29-analytics.txt` — the org Analytics page:
  quota cards, the seven 30-day chart families, Top Databases, Top Queries.
- `docs/evidence/turso-live-2026-10-29-per-database.txt` — the per-database tab
  set, Overview/Configuration controls, Branches, per-database Analytics and
  Top Queries. **Its closing "GENUINE GAPS" list is superseded** by §2 and §4
  of this document; see the note below.
- `docs/evidence/turso-live-2026-10-29-edit-data.txt` — the Edit Data browser
  read through its shadow DOM: toolbar, type-annotated headers, sampled rows.
- `docs/evidence/turso-edit-data-live-2026-10-29.png` — the Edit Data grid as
  rendered (screenshot of the same view the text record describes).
- `docs/evidence/turso-live-2026-10-29-audit-logs.txt` — the Audit Logs page:
  6 entries today versus 23 in the Oct 26 capture; retention policy undisclosed.
- `docs/evidence/cargo-test-full-suite-2026-10-29.txt` — the verbatim per-suite
  output of `cargo test --workspace --all-targets` on this tree (91 suites, 664
  passed, 0 failed), so the green-suite claim is inspectable rather than
  asserted.

Historical captures, retained unchanged and **not** re-read this revision:

- `docs/evidence/turso-analytics-2026-10-26.txt`
- `docs/evidence/turso-databases-2026-10-26.txt`
- `docs/evidence/turso-audit-logs-2026-10-26.txt`
- `docs/evidence/turso-edit-data-2026-10-26.png`
- `docs/evidence/turso-edit-data-grid-2026-10-27.png`
- `docs/evidence/turso-structure-2026-10-27.txt`
- `docs/evidence/turso-structure-view-2026-10-27.png`
- `docs/evidence/turso-structure-indexes-2026-10-27.png`

`dg`-side evidence: `docs/evidence/rows-browser-live-2026-10-27.txt`,
`docs/evidence/schema-live-2026-10-27.txt`, `docs/audits/tui-audit.md`, and the
live local gateway (`GET /v1/status`, `/v1/databases`, `/v1/backups`, `/v1/audit`,
`/v1/queries`) read on 2026-10-29.

Turso CLI surface: `turso` v1.0.15, captured from local `--help` output in
`docs/evidence/turso-cli-help-2026-10-29.txt` (no account authentication or
mutation). The CLI is not authenticated here, so no live `show`/`inspect`/`shell`
ran against Turso and no Turso data was read through it.

## Superseded assessment in the per-database capture

`docs/evidence/turso-live-2026-10-29-per-database.txt` ends with a section
headed **"GENUINE GAPS, in operator terms"** that lists remote access control
(Block Reads, Block Writes, Allow ATTACH, Delete Protection, CIDR/VPC
allow-list) as gap number one. **That assessment is superseded and should not be
used.** The capture is retained unmodified as the record of what the console
showed on 2026-10-29; it is evidence of Turso's surface, not a judgment about
`dg`.

The capture's own text already undercuts its heading: it observes that `dg`
"listens on loopback only and reaches remote gateways through an SSH tunnel, so
per-database network admission is not part of its model." That is the right
observation with the wrong label. A control that answers a question `dg`'s
architecture has already answered is not a gap, which is exactly the
provider-shaped reasoning this document was re-audited to remove.

Read those five controls as follows:

| Capture's claim | Current classification |
|---|---|
| Block Reads | not applicable (§4) — a read cannot damage a database |
| Block Writes | **gap 3** — a genuine operator harm, self-inflicted corruption |
| Allow ATTACH | not applicable (§4) — each database is an isolated file |
| Delete Protection | **gap 5 CLOSED in 0.10.0** — now guarded by `dg protect` |
| CIDR / VPC allow-list | not applicable (§4) — loopback plus SSH tunnel |

The other two entries in that list survived the mindset audit: the time-series
gap is **gap 2**, and bulk token invalidation was **gap 4**, now **CLOSED in
0.10.0**. The capture's remaining descriptive content
— the tab inventory, the configuration controls, Branches, Analytics and Top
Queries — is unaffected and is cited above as read.

## A regression found and repaired during this pass

The first attempt at this refresh ended with a red suite: three TUI tests failed
(`leaving_a_tab_clears_a_one_time_credential_outcome`,
`manual_refresh_clears_a_one_time_key_result`,
`a_failed_contents_read_shows_unknown_counts_and_retries`). They were **not**
caused by the documentation work, but they were also not left alone.

Each failure pointed at a `// MUTATION:` marker in `src/tui/mod.rs` (lines 731,
1821, 1911), all introduced by commits dated 2026-10-28T23:01–23:02Z
(`95407a8`, `405b82c`) — about an hour before this goal started. Checking out
`563177e`, the commit that first added the correct behaviour, showed all three
tests passing there. The decisive evidence: `clear_secret_outcome` had been left
**defined but never called**, and `src/tui/draw.rs` renders `outcome.facts`
verbatim — so with the call removed, a raw one-time credential stayed on screen
after a tab switch or a manual refresh. Two of the three failures were a
credential-display defect, not a cosmetic test detail.

What those commits were for is not knowable from the repository, and this
document does not claim to know. `MUTATION` is the conventional word for a
deliberately-injected fault in mutation testing, so a targeted experiment is one
plausible explanation; an agent misreading "what is this guard for?" as "remove
this guard" is another. The commits carry only auto-generated messages, so
neither story can be confirmed. What is checkable is the state they left: a guard
that nothing called, and three tests asserting the behaviour those calls
provided.

Because a visible raw credential is a real defect regardless of why the calls
vanished, the three lines were restored to their known-good form rather than
documented as a known failure. The document is unchanged in scope; the code
change restores the calls and adds nothing new.

```
cargo test --workspace --all-targets   (after the repair)
  exit 0 — 91 suites, 664 passed, 0 failed
```

The raw per-suite output is committed as
`docs/evidence/cargo-test-full-suite-2026-10-29.txt`, so the full-suite claim
can be checked without re-running it.

One caveat is recorded in that file rather than smoothed over: this repo's two
process-lifecycle tests (`orphan_guard`, `crash_restart`) are not robust to a
host that is simultaneously running another project's full test suite. On this
tree the suite went green four times and failed twice, once in each of those two
files, both times while a concurrent `cargo test --workspace` from a different
checkout was saturating the machine (load above 140, zero memory available).
Each failing test passes in isolation, and this refresh never touched either
file. Making them load-tolerant is a source change belonging to its own goal.

## How to re-read this

The Turso side is a moving target: the Audit Logs view showed 6 entries on
2026-10-29 versus 23 in the 2026-10-26 capture, but did not disclose why older
entries disappeared. Re-read the console before relying on a gap being open,
and re-run the `dg` side against a current build. Gap 1 was carried forward
from October, then closed in 0.10.0; the mapping drift it used to sit beside is
recorded in §4.1 as already corrected. The Turso side remains a 2026-10-29
capture, not a live assertion.

## Mindset audit, 2026-10-29

This document was re-audited against one question: does every ranked gap name a
harm that a single operator on their own box could actually suffer, or does it
only exist because Turso, a provider, offers the same control for reasons of its
own? The answer changed the structure.

**What was wrong.** The ranked list held eleven entries, and four of them were
not gaps:

- `dg queries` showing statement shapes instead of literal values was described
  in its own text as "not a gap to close" — a privacy property the operator
  gains — yet it was numbered among the defects.
- Edit Data being read-only was likewise called "a deliberate non-goal, not a
  missing feature" while sitting in the same list.
- The difference in audit retention asks nothing of `dg`: Turso's rule is
  undisclosed, which is an observation about Turso, not a debt here.
- The CLI mapping drift was already corrected in `docs/cli-turso-mapping.md`
  before this audit ran, so it recorded a past fix rather than an open gap.

Those four moved to §4.1. Listing a considered design choice as a "gap" quietly
promises a fix that should never come, and it inflates the apparent distance
between `dg` and Turso.

**What was re-judged.** The remaining seven were re-tested against the
lone-operator standard, and two were re-graded downward:

- Credential expiry, per-table scopes and group tokens are governance for
  credentials handed to other people; a key file on the operator's own disk
  gains little from a calendar. Only bulk revocation survives, because it
  shortens a real incident.
- The CIDR/VPC allow-list is a provider firewall. `dg` is loopback-only behind
  an SSH tunnel, so the network already answers that question; it is now
  not-applicable in §4 rather than a missing feature.

Two were kept for the opposite reason — they look small next to Turso's console
but protect the operator from an ordinary mistake: making one database read-only
mid-incident so a single misbehaving application cannot keep writing, and being
able to undo a delete.

**What this audit did not do.** It did not re-read the Turso console or change
how `dg` behaves. Every claim was checked against the committed 2026-10-29
captures and `dg --help` — including the two flags the matrix leans on most,
`dg export` refusing an existing output file and `dg key revoke` taking a single
NAME, both confirmed verbatim.

**Correction to this audit, same day.** The first pass renumbered the gaps but
left the old numbers behind in prose and in the matrix, so the document
contradicted itself: §3 still pointed at the retired numbers for items that had
moved to §4.1, §4 still described the read-only and undelete gaps by their old
positions, and the closing line still said every gap remained real after only
seven survived. Every reference now resolves to one of the eight defined gaps, or
to §4.1. (Gap 8 was added on 2026-10-30, so this historical paragraph's
"seven" describes the October count it was written against.)

One row was also re-judged rather than just re-pointed. **Block Reads** had been
folded in with Block Writes as "the same surface", citing the delete-protection
gap — which was wrong on both counts. Blocking *writes* protects data from
damage, and that is genuinely gap 3. Blocking *reads* protects no data: a SELECT
cannot corrupt a file. It exists so a party you distrust cannot *see* rows, which
is a tenancy question Turso's operator answers about its customers. Here the
operator holds the only key to the box, and distrust of a reader is resolved by
revoking it (gap 4), not by masking what it reads. The row is now
not-applicable with that rationale in §4.

**Placement correction, same day.** The four deliberate differences were first
parked in a separate §2b. That satisfied the reasoning but not the structure: the
matrix has to be able to point every partial row at a ranked gap or a §4
rationale, and §2b was neither. They are now §4.1 inside "Not applicable by
architecture", which is what they actually are — dispositions of
not-applicable, distinguished from the architectural entries above them only by
the reason. The four rows that cite them now read `§4.1`, and the matrix check
passes under the strict rule with no change to the rule itself.

**Superseded in the evidence.** The per-database capture closes with a list
headed "GENUINE GAPS" that ranks remote access control first. That heading is
the provider-shaped thinking this audit exists to remove, and the capture's own
text contradicts it by noting that network admission "is not part of its model".
The evidence file is preserved byte-for-byte as the record of what the console
showed; the comparison now carries an explicit supersession table mapping each
of its five entries to its current classification, three of them not-applicable.

## Related

- `docs/cli-turso-mapping.md` — the per-verb mapping table (mapping drift is
  recorded in §4.1).
- `docs/tui.md` — the terminal view this comparison's `dg` column describes.
- `docs/audits/tui-audit.md` — the 2026-10-28 TUI audit.
- `docs/recovery-contract.md`, `docs/recovery-policy.md` — the backup and
  restore model that replaces Turso's replication and PITR.

---

## Appendix: the 2026-10-26 revision (historical)

The body below is the previous revision of this document, kept verbatim so the
earlier inspection work is not lost. It is **historical**: its ranked gaps have
been superseded by §2 above, its October figures are superseded by the live
2026-10-29 captures. Its claim that the 23-to-6 audit-log difference proves
time-based expiry is an inference not supported by the captures; the current
report preserves that uncertainty. Read §1–§4 for anything current.

### Headline answer

`dg` **can** check the tables. The gap the seed feared does not exist as
a missing capability:

- `dg info shop` →
  `{"created_at":1793019443,"name":"shop","size_bytes":16384,`
  `"tables":[{"name":"products","rows":1}],"tombstoned":false}`.
  Table names with row counts, per database. Observed 2026-10-26
  against the disposable gateway.
- `dg inspect shop` on a terminal renders `database / activity / backup
  / tables` sections (full output quoted in §7). Observed 2026-10-26.
- `dg shell`: `.tables` lists user tables, `.schema [table]` prints
  `CREATE` statements, arbitrary `SELECT` runs over the HTTP API
  (outputs quoted in §5).

What is real is narrower: partials ranked by operator impact in §8, plus
the places where single-host architecture answers "not applicable".

### 1. Analytics usage cards

Turso shows, per organisation, rows read/written against quota, sync
bytes, storage, a latency chart (p50–p99 + average), and 30-day charts
for rows read, rows written, inbound traffic, outbound traffic, syncs,
and storage. Evidence file §1 quotes, captured 2026-10-26:

- `Rows Read 2.20B / 2.50B`, `Rows Written 24.70M / 25M`,
  `Syncs 1.51 MB / 10 GB`, `Storage 1.15 GB / 9 GB`.
  (The capture's remaining elements are account-management surfaces,
  out of scope — see the final section.)

`dg` exposes the numerators but not the denominators. `dg status`
returned per-database `rows_read: 3, rows_written: 1` (full block
quoted below): raw lifetime counters with no plan quota to ratio them
against. The `/ 2.50B`, `/ 25M`, `/ 10 GB`, `/ 9 GB` denominators are
therefore MISSING on the `dg` side — Turso-side evidence is the usage
figures above; `dg` has no metering or plan limits (single owner, no
quotas), so a percent-of-quota card has nothing to divide by.
- Chart axes: rows read `0 … 1B`, rows written `0 … 8M`, inbound
  `0 B … 600 MB`, outbound `0 B … 1.2 GB`, syncs `0 B … 1.6 MB`,
  storage `0 B … 1.2 GB`; latency `0 ms … 80 ms` with
  `p50 p75 p90 p95 p99 Average`. (The page's account banners are out
  of scope and are listed in the final section, not quoted here.)

`dg` equivalent tested: `dg status` on the disposable gateway returned
(full output observed 2026-10-26, trimmed to the per-database block):

```json
"shop": {
  "last_write": 1793019443, "queue_timeouts": 0,
  "queue_wait_max_us": 11, "queue_wait_ms": 0,
  "queue_wait_p95_us": 11, "queue_waits": 5,
  "rows_read": 3, "rows_written": 1
}
```

plus admission permits and deployment disk figures.

Verdict: PARTIAL. `dg` has the counters but no history: no time-series
charts, no per-database over-time view, no latency percentiles over time.
Quota cards are out of scope (single owner, no metering).

### 2. Top databases

Turso ranks databases by rows read and, per database, shows average
query time, average rows read/written, query count, synced bytes,
inbound/outbound bytes, storage, and region. Evidence file quotes,
captured 2026-10-26 (Today UTC):

- `dracon-studio-dev / aws-eu-west-1`: 90.61K queries, 815.47 KB in,
  724.86 KB out, 805.42 MB storage.
- `dracon-jobs-dev`: `Query time (avg) 4.92 ms`, `Rows read (avg)
  45.48`, `Queries 476`, `Inbound 2.58 MB`, `Outbound 6.99 MB`.
- `dracon-studio`: `Writes Blocked`, 296.54 MB storage (also visible in
  the databases list, §4).

`dg` equivalents tested: `dg status` per-database traffic (rows, waits,
p95/max, last write — output above), `dg info` sizes,
`dg queries --database shop` shapes with `count`, `total_micros`,
`max_micros`, `rows_read` (full shape output quoted in §3; averages are
`total/count`), and the TUI databases tab columns.

Verdict: PARTIAL. Two facts have no `dg` equivalent: per-database
inbound/outbound bytes, and synced bytes (no replicas to sync).
Region/location is not applicable: single host by architecture
(`docs/architecture.md`).

### 3. Top queries

Turso ranks statement shapes by rows read with average query time,
average rows read/written, execution count, and the full SQL text.
Evidence file quotes, captured 2026-10-26:

- `104 ms` avg, `6.19K` rows avg, `3` queries:
  `SELECT slug, title, … FROM jobs ORDER BY COALESCE(datetime(posted_at),
  '') DESC, …`.
- `17.7 ms` avg, `59` queries: 9-row multi-VALUES `INSERT INTO jobs`.
- `608 µs` avg, `395` queries: `DELETE FROM jobs WHERE slug = ?`.

`dg` equivalent tested: `dg queries --database shop` on the disposable
gateway returned, verbatim (trimmed to two of four shapes):

```json
{"count": 1, "first_seen": 1793019448, "last_seen": 1793019448,
 "max_micros": 71, "principals": {"owner": 1}, "rows_read": 1,
 "rows_written": 0, "shape": "SELECT * FROM products", "total_micros": 71}
{"count": 1, "max_micros": 124, "principals": {"owner": 1},
 "rows_read": 0, "rows_written": 0,
 "shape": "CREATE TABLE products (id INTEGER PRIMARY KEY, name TEXT)",
 "total_micros": 124}
```

Verdict: COVERED. Count, totals, maxima, rows read/written, first/last
seen, and principals are all present; averages are `total/count`; the
`?` shape redaction matches Turso's literal-free display.

### 4. Databases list

Turso lists every database with rows read, rows written, bytes synced,
storage, group, and status, plus a per-row "Edit Data" link. Full text
in `docs/evidence/turso-databases-2026-10-26.txt`; headlines quoted:

- Columns: `Name Type Rows Read Rows Written Bytes Synced Storage Group
  Status`.
- `dracon-studio-dev | 2,165,085,739 | 19,599,101 | 0 B | 805.42 MB |
  default | Writes Blocked`
- `dracon-studio | 46,796,361 | 5,437,201 | 0 B | 296.54 MB |
  default | Writes Blocked`
- `ai-books | 37,032,892 | 2 | 0 B | 29.52 MB | default | Active`
- 19 databases total; page chrome offers `Restore deleted databases`
  and `Show branches`.

`dg` equivalent tested: `dg list` on the disposable gateway returned
`{"databases":["shop"]}` (observed 2026-10-26). Traffic-per-database
lives in `dg status` (output in §1) and the TUI databases tab carries
the same columns (rows read/written, waits, wait p95/max, last write,
size) with a footer defining the wait columns.

Verdict: COVERED for names + per-database traffic + sizes. No `dg`
equivalent for `Writes Blocked`-style status (single host has no
throttle state), deleted-database restore (tombstones purge after
retention instead), or branches.

### 5. Per-database Edit Data browser

The browser (Drizzle Studio) has three panes, observed in
`docs/evidence/turso-edit-data-2026-10-26.png` (`dracon-music-dev`,
captured 2026-10-26):

- Left: `SQL console` entry, `Schema` panel listing tables
  (`music_follows`, `music_likes`, `music_playlist_songs`,
  `music_playlists`, `music_users`), search box.
- Main: `DATA | STRUCTURE` tabs, `Filters`, `Sort`, `Columns`,
  `Add record` actions, pagination `1 - 50 of 50+`, last run `183ms`.
- Grid rows show two columns (`subject text`, `created_at integer`)
  with values such as `alt-token | 1788275886575`,
  `finalcheck | 1788276443309`.
- An earlier capture of the empty `ai-model-list` browser showed the
  zero-state: `No items in database` with a `Create table` action.
- Tab strip above the browser: `Overview | Branches | Analytics |
  Top Queries | Edit Data` — the per-database Analytics and Top Queries
  tabs scope §1–§3 to one database.

`dg` equivalents tested on the disposable gateway, all observed
2026-10-26:

- Schema panel ≡ `dg shell` dot-commands. Piped session:
  `use shop` → `using shop`; `.tables` →
  `┌──────────┐ │ name │ ├──────────┤ │ products │ └──────────┘ 1 row`;
  `.schema products` → the `CREATE TABLE products (id INTEGER PRIMARY
  KEY, name TEXT)` row. `.help` lists `.tables`, `.schema [table]`,
  `.status`, `.keys`, `.backups`, `:batch`, `:key`, `.json`.
- SQL console ≡ `dg shell` interactive/piped plus one-shot `dg query`:
  `dg query shop "SELECT * FROM products"` →
  `{"columns":["id","name"],"rows":[[1,"widget"]],"rows_affected":0}`.
- Row grid reads ≡ `SELECT` in either; the grid's Add record ≡ an
  `INSERT` statement, tested exactly (observed 2026-10-26):
  `dg query shop "INSERT INTO products (id, name) VALUES (1,
  'widget')"` → `{"columns":[],"rows":[],"rows_affected":1}`;
  read-back `dg query shop "SELECT * FROM products"` →
  `{"columns":["id","name"],"rows":[[1,"widget"]],
  "rows_affected":0}`; `dg info shop` then reports the table with
  `"rows":1`. Table creation is `CREATE TABLE`, tested the same way
  (`rows_affected: 0`, then visible in `.tables` and `dg info`).

Verdict: COVERED with one PARTIAL: no click-grid row browser exists,
but every operation the grid performs (list tables, show schema, read
rows, add record, create table) is expressible and was exercised above
with the exact commands and outputs quoted.
The per-database Analytics/Top Queries scoping has no one-shot `dg`
equivalent beyond `dg queries --database <name>` and `dg status`.

#### 5.1 The STRUCTURE tab, and what dg now answers of it

Turso side, captured 2026-10-27 in the browser, read-only:

- `docs/evidence/turso-structure-view-2026-10-27.png` — the
  `STRUCTURE` tab of `dracon-music-dev` → table `music_users`. On
  screen: `Table name: music_users`; **COLUMNS** —
  `subject TEXT PRIMARY KEY` and `created_at INTEGER NOT NULL`, each
  column's declared type and flags printed beside its name;
  **CONSTRAINTS** (empty for this table, with an `Add constraint`
  affordance); **INDEXES** (an `Add index` affordance and an
  `AUTO-INDEXES` disclosure, collapsed); and a `Copy create
  statements` action beside the tab.
- `docs/evidence/turso-structure-indexes-2026-10-27.png` — the same
  view with the index list open: `UNIQUE INDEX
  sqlite_autoindex_music_users_1 ... (subject)`.
- `docs/evidence/turso-edit-data-grid-2026-10-27.png` — the `DATA` tab
  of the same table, kept because it is the surface the structure is
  compared against: the type-annotated grid headers (`subject text`,
  `created_at integer` — the declared type in a dimmer style beside
  each column name), the `Filters` / `Sort` / `Columns` toolbar, the
  `+ Add record` action, the pager (`1 - 50 of 50+`) and the per-row
  checkboxes.
- `docs/evidence/turso-structure-2026-10-27.txt` — the capture note:
  what was open, what each capture shows, and the read-only record
  (no record was added, edited or deleted; no SQL was run against
  Turso; the `Add column` / `Add constraint` / `Add index`
  affordances were left untouched).

What `dg` covers now, added 2026-10-27 and exercised against the
seeded live gateway (outputs quoted verbatim in
`docs/evidence/schema-live-2026-10-27.txt`):

- **The per-column facts**: `dg schema <db> [table]` gives each
  column's name, its **declared** type, and its `not_null` and
  `primary_key` flags — the same facts Turso prints beside the column
  name, read from SQLite's own `PRAGMA table_info` rather than parsed
  out of the `CREATE` statement, so a composite key and a `UNIQUE`
  constraint's index are facts and not guesses.
- **The index list**: `PRAGMA index_list` / `index_info` give each
  index's name, its uniqueness and the columns it covers — the
  `AUTO-INDEXES` list, in the same facts.
- **The foreign keys**: `PRAGMA foreign_key_list` gives the target
  table, both columns and the `on_update` / `on_delete` actions. Turso
  shows these under CONSTRAINTS; `dg` shows them read-only.
- **The `CREATE` statement**: kept verbatim per table, so
  `Copy create statements` is a copy away.
- **One surface, two faces**: the TUI's `schema <db> [table]` palette
  action draws the same structure through the same shared view layer
  (`src/views.rs`), so the terminal view and the terminal view cannot
  tell an operator different stories — the same rule the rest of the
  TUI already keeps.
- **Read-only, and machine-readable**: the new surface performs no
  DDL of any kind; a pipe reads the gateway's own JSON document
  unchanged.

What `dg` still lacks against Turso's Edit Data browser, unchanged by
this work:

- **A paged, sortable, filterable row browser now exists** (added
  2026-10-27): `dg rows` and the TUI's rows browser read a table a page
  of ten at a time, sort by any real column and filter with a read-only
  condition, with the footer counting the filtered set. What is still
  absent from that browser: the `Columns` picker (showing a chosen subset
  of columns), per-row checkboxes, and `+ Add record`.
- **Type-annotated grid headers**: Turso prints the declared type in
  the row grid's header, beside the column name. `dg`'s row grid shows
  the column names; the type now lives in the structure view instead.
- **Editing the structure**: Turso's panel edits columns, constraints
  and indexes in place (`Add column`, `Add constraint`, `Add index`).
  `dg`'s structure view is read-only by design; changing a schema is
  a `CREATE` / `ALTER` statement run through the query path, which is
  where `dg` keeps every write.
- **The remaining §1–§3 and account surfaces** are unchanged: usage
  history, per-database network bytes, and everything about billing,
  plans, organisations and invoices stays out of scope.

### 6. Turso CLI verbs

Turso side: `turso db --help`, captured 2026-10-26 (the CLI is not
logged in, so this is verb inventory, not live output):

```text
Available Commands:
  config      Manage db config
  create      Create a database.
  destroy     Destroy a database.
  export      Export a database snapshot and WAL from Turso to SQLite files.
  import      Import a SQLite database file to Turso.
  inspect     Inspect database.
  list        List databases.
  locations   List available database locations.
  replicate   Replicate a database.
  shell       Start a SQL shell.
  show        Show information from a database.
  tokens      Manage db tokens
  unarchive   Unarchive a database
  update      Updates the database to the latest turso version
```

Top level (`turso --help`) adds `group` (database groups — mapped
below), `dev`, `config`, and `quickstart`, plus account-management
verbs that are out of scope (see the final section). The in-scope
audit surface is the web Audit Logs page: columns Author/Event/Origin/
Timestamp, 23 items on 2026-10-26, all `Created Database Token` by
`dracondev` via `dashboard` (2) or `cli` (21) — full text in
`docs/evidence/turso-audit-logs-2026-10-26.txt`. (The CLI exposes the
same logs as `turso org audit-logs list [--limit N] [--page N]
[--verbose]`; that verb lives under the excluded `turso org` namespace
— see the final section — so the §6 row below is anchored on the web
page, not the CLI verb.) `group` manages
(`create/destroy/list/locations/…`); `replicate` usage is
`turso db replicate <database-name> <location-code>`; `tokens` manages
`create` (bearer token) and `invalidate` (rotate signing keys).

| Turso verb | `dg` command tested (observed 2026-10-26) | Verdict |
|---|---|---|
| `db list` | `dg list` → `{"databases":["shop"]}` | COVERED |
| `db create` | `dg create shop` → exit 0; `dg list` then shows `shop` | COVERED |
| `db destroy` | `dg delete shop2 --confirm shop2` → `{"deleted":true,"purge_after_seconds":86400}`; `dg list` confirms removal | COVERED |
| `db show` | `dg info shop` → `{"created_at":1793019443,"name":"shop","size_bytes":16384,"tables":[{"name":"products","rows":1}],"tombstoned":false}` | PARTIAL: no URLs/instances/branches — single host has none |
| `db shell` (interactive) | `dg shell` `.tables`/`.schema` session quoted in §5 | COVERED |
| `db shell <db> "sql"` (one-shot) | `dg query shop "SELECT * FROM products"` → `{"columns":["id","name"],"rows":[[1,"widget"]],"rows_affected":0}` | COVERED |
| `db execute` | MISSING on the Turso side: no `execute` verb in the help above, and `turso db execute --help` falls through to the parent `db` help. `docs/cli-turso-mapping.md:24` claims a `` `turso db execute <name> "<sql>"` `` → `dg query` row that does not match the installed CLI; one-shot SQL on Turso is `turso db shell <db> "sql"`. The `dg` side of that mapping (`dg query`, output above) is verified. | MISSING (doc correction, see §8.4) |
| `db inspect` | `dg inspect shop` on a terminal renders database/activity/backup/tables (full output in §7); piped output falls back to the `dg info` document | PARTIAL: see §7 non-TTY fallback |
| `db export` | `dg export --out /tmp/…/shop.sqlite shop` → `{"name":"shop","out":"…","sha256":"add77d17…","size":16384}` | COVERED |
| `db import` | `dg import --from /tmp/…/shop.sqlite shop2` → `{"imported":"shop2"}`; `dg info shop2` shows the table with 1 row | COVERED |
| `db tokens` | `dg key list` → `{"keys":[{"databases":[],"name":"owner","role":"owner"}]}`; `dg key create app1 --role database_read_write --database shop` printed a raw key; `dg key revoke app1` → `{"revoked":true}` | COVERED (create/list/revoke exercised; `rotate-owner` exists per `dg key --help`, not exercised) |
| `db locations`, `turso group …`, `db replicate` | No `dg` equivalent | NOT APPLICABLE: single-host architecture, documented non-goal (`docs/architecture.md`). Turso-side evidence: `locations List available database locations`, `turso db replicate <database-name> <location-code>`, `turso group [create/destroy/list/locations/…]` |
| `db config`, `db update`, `db unarchive` | `dg config list` → `{"config_path":"/home/dracon/.config/dg/config.toml","contexts":[],"default":null}` (local contexts exist) | PARTIAL/N/A: contexts cover connection config; server update and unarchive have no single-host meaning |
| Account-management verbs | None | OUT OF SCOPE — named in the final section |
| Turso web Audit Logs page (CLI equivalent lives under the excluded `turso org` namespace) | `dg audit` → `{"entries":[{"action":"create","at":1793019443,"database":"shop","principal":"owner","role":"owner"},{"action":"query","database":"shop","detail":"CREATE TABLE products (id INTEGER PRIMARY KEY, name TEXT)","principal":"owner"},{"action":"query","database":"shop","detail":"INSERT INTO products VALUES (?, ?)","principal":"owner"},{"action":"query","database":"shop","detail":"SELECT * FROM products","principal":"owner"}]}` (trimmed; observed 2026-10-26) | PARTIAL: who ran what and when, with statement shapes and no values — covers Author/Event/Timestamp, but `dg audit` entries carry no Origin: Turso distinguishes `dashboard` from `cli` (evidence file: 2 dashboard + 21 cli entries) while every `dg` entry differs only by principal/role |

### 7. `dg inspect`: the terminal-only rich view

`dg inspect shop` on a terminal (ANSI stripped), observed 2026-10-26:

```text
database
  name     shop
  size     48.0 KiB
  state    live
  created  2026-10-26 12:57 UTC (1793019443)

activity
  rows read     3
  rows written  1
  queue waits   5
  wait p95      11 us
  wait max      11 us
  last write    15s ago

backup
  state       unknown
  newest set  -
  age         unknown

tables
┌──────────┬──────┐
│ table    │ rows │
├──────────┼──────┤
│ products │    1 │
└──────────┴──────┘
```

Piped or `--json`, the same command returns the raw
`/v1/databases/{name}` document — the same facts as `dg info`, with no
activity, no backup freshness, and no tables section.
`src/cmd_data.rs::inspect` branches on `IsTerminal::is_terminal`; the
non-TTY arm returns the plain info request. Consequence: scripts cannot
read the activity/backup/tables view an operator sees; automation must
rejoin `/v1/databases/{name}`, `/v1/status`, and `/v1/backups` by hand.
Sharpest scriptability gap found.

### 8. Ranked gaps (operator impact first)

1. `dg inspect` degrades off-terminal (§7). Smallest fix with the
   largest automation payoff; no API change needed (compose the three
   reads the TTY path already makes).
2. No usage history (§1). Turso's charts answer "what changed last
   week"; `dg` answers "what is true now".
3. No per-database network bytes (§2). Queue waits are the only
   pressure signal per database.
4. Mapping-doc corrections (§6): the `turso db execute` row
   (`docs/cli-turso-mapping.md:24`) names a verb the installed CLI
   does not have; and `dg key --help` shows the owner-rotation
   subcommand is `rotate-owner`, not bare `rotate`. Cosmetic, but the
   doc is the CLI contract's map.

### Out of scope, untouched

Billing, plans, organisations, invoices, subscriptions, and auth tokens
for third parties. Excluded CLI verbs: `turso auth`, `turso org` (including
`turso org audit-logs list`), and `turso plan` with all their subcommands. Concretely observed but excluded: the Analytics
page's `Next Invoice $5.99` card with its `View Invoice` action, the
`Quota Critical 99% used … See plans` banners, the `Vegas Blackout:
Usage was erased on October 11th` notice, and the sidebar's
Billing/Upgrade entries. This audit read the Turso web console and `--help`
text only; the `turso` CLI is not authenticated here, no live
`show`/`inspect`/`shell` ran against Turso, and no Turso data was
created, modified, or deleted. All `dg` testing ran against disposable
local gateways (temp dirs removed afterwards).

This report was updated on 2026-10-27, when `dg` gained the structure
surface described in §5.1: the `GET /v1/databases/{name}/schema` route,
the `dg schema <database> [table]` command and the TUI's `schema` action.
That is the only `dg` change this report's second edition covers; the
repository changes are the new surface (route, command, action, shared
view, client typing, tests), this report, and the four evidence files
cited at the top. The Turso captures were taken read-only, and `dg` has
no outbound Turso contact: the new surface reads the gateway's own
SQLite file and nothing else.
