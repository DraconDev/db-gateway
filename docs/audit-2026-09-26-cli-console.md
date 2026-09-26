# Audit: dg CLI + web console (2026-09-26)

Scope: the Turso-parity surface — `src/main.rs`, `src/shell.rs`,
`src/client.rs`, `src/api.rs`, `web/src/*.ts`, `web/public/index.html`,
`web/public/app.js`, plus the web/Rust test suites by status (not a
full-repo audit; store/backup/recovery internals were covered by prior
sessions). Method: line-by-line read of every file in scope, claims
grounded by grep and test runs. No live-browser pass: the Pi Chrome
Connector companion was unresponsive (see P1).

Severity: M = medium, L = low. No critical/high findings. The security
posture of the audited surface is solid; findings are robustness, UX,
and consistency items.

## Findings

### Robustness

- F1 (M) `web/src/server.ts:180-192` — `GET /api/databases` fans out one
  `api.info()` per database under a single `Promise.all`. One failing
  database (or one slow one exhausting the 15s companion timeout) fails
  the entire Databases view instead of degrading that row.
- F2 (M) `web/src/server.ts:147-168` — `GET /api/overview` joins five
  gateway calls under one `Promise.all`; a single failure (e.g. keys)
  blanks Status, usage, tunnel, and backup panels together.
- F3 (M) `web/src/server.ts:169-172` — detail route joins `info` +
  `schema` under `Promise.all`; a schema-query failure breaks the whole
  detail view although overview/tables/backup could still render.
- F4 (M) `web/public/app.js:163-174` — `selectDatabase` has no
  generation guard. Navigating A→B quickly can let A's slower fetch
  resolve last and render the wrong database's detail.
- F5 (M) `web/public/app.js:152` — data grid pages with
  `LIMIT 51 OFFSET n` and no `ORDER BY`, so rows can shift or repeat
  between pages on a live database. (`ORDER BY rowid` covers all but
  `WITHOUT ROWID` tables.)

### UX gaps

- F6 (M) `web/public/app.js:50,227` — `runForm` routes every form error
  to `#result`, which lives on the Data tab; delete success goes there
  too. From Databases/Overview/Tokens the failure/success is invisible
  (the per-form status is cleared in `finally`).
- F7 (L) `src/main.rs:836`, `src/shell.rs:render` — only `dg query`
  renders an aligned table on terminals; `list`/`info`/`status` always
  emit JSON. Turso-parity enhancement: reuse the table renderer.
- F8 (L) `src/main.rs:423-426` — `dg info` shows size/tables/creation
  but not the new per-db `rows_read`/`rows_written`; the console joins
  them but the CLI does not. Fixable client-side by joining
  `/v1/status`, no server change.
- F9 (L) `web/src/tunnel.ts:92` — tunnel reports `running` on process
  spawn, not on an established forward; no `ConnectTimeout` /
  `ServerAliveInterval`, so an unreachable host looks healthy while
  ssh hangs. The status pill over-claims.
- F10 (L) `src/main.rs:300-324` — `dg web` readiness probes hardcoded
  `127.0.0.1:{port}`. With `DG_CONSOLE_HOST=::1` (accepted by the
  companion's loopback check) the probe never succeeds: 30s hang, then
  the companion is killed as failed.

### Performance

- F11 (L) `web/src/api.ts:103-106` — `redactCredential` compiles
  `new RegExp(credential, 'gi')` once per string value in every
  response. A 10k-cell query result pays 10k+ compilations; the
  case-insensitive flag can also mask user data that merely matches
  the hex key case-insensitively. Hoist one regex per response and
  consider case-sensitive matching.
- F12 (L) `src/client.rs:83-88` — a fresh reqwest `Client` (new
  connection pool) is built per request; a chunked export builds one
  per chunk. Correct, just wasteful.

### Consistency / dead code

- F13 (L) `web/src/server.ts:209-218,231-234` — three console routes
  are called by neither `app.js` nor any test: `POST
  /api/databases/:x/schema`, single-shot `POST .../export`, single-shot
  `POST .../import` (which also ignores its path name and reads
  `input.name` instead). Wire them, cover them, or remove them.
- F14 (L) `web/public/index.html:34` — only the create input carries
  the `[a-z0-9][a-z0-9_]{0,63}` pattern; clone-destination,
  import-name, key-name/key-db have no client hint (server still
  rejects; UX-only).
- F15 (L) `src/main.rs:794-806` — `dg key create` skips the
  client-side `validate_name` every sibling command applies; bad names
  fail server-side instead of locally.
- F16 (L) `src/api.rs:146,324,358` — read-only `GET /v1/status`,
  `/v1/backups`, `/v1/keys` take the write lane (`work`, not
  `work_read`), so the observability endpoints 429 exactly when write
  saturation makes them most needed.

## Verified OK (checked, no action)

- V1 Loopback trust model is coherent: the console session cookie is
  CSRF defense (SameSite=Strict + origin check), not authentication —
  any local process can mint a session by design. `Secure` is
  correctly absent (no https on loopback; the localhost exception does
  not cover 127.0.0.1). Worth one doc sentence, not a code change.
- V2 Grid SQL is injection-safe: table names come from the validated
  gateway `tables` list and are quoted by doubling `"`.
- V3 Transport discipline holds end to end: companion accepts only
  loopback-http gateway origins; CLI requires https-or-loopback, sets
  `no_proxy`, follows no redirects, bounds every response class.
- V4 Credential handling: 0600 enforced on both sides, 64-hex shape
  enforced, owner key redacted from console JSON/logs/errors, tunnel
  status exposes no secrets. (`dg init` emits JSON; operators must
  extract `owner_key` into the 0600 file — bit this auditor once.)
- V5 Transfers: canonical-base64 round-trip checks, per-chunk bounds,
  offset/size/sha assertions, whole-file hash verify, atomic
  no-clobber publish (CLI), resumable pause/abort (console).
- V6 Contract conformance (`tsconfig.contract.json`) passes and covers
  QueryResult/KeyInfo/TableInfo/DatabaseInfo. StatusResponse/usage is
  intentionally outside it, so the client package lacking usage types
  is a parity note (T9), not drift.

## Task list (priority order)

1. T1 (F1,F2,F3) Degrade, don't fail: `allSettled` on the companion
   joins; per-entry `unknown` states for list rows, overview sections,
   and detail schema. Add server tests with one failing upstream.
2. T2 (F6) Route form errors and delete/clone/key results to the
   visible per-form status or a same-tab result panel, never the
   hidden Data-tab `#result`.
3. T3 (F4) Generation guard or AbortController in `selectDatabase` so
   late resolutions cannot paint a stale database.
4. T4 (F5) Stable grid paging: `ORDER BY rowid` (document the
   `WITHOUT ROWID` exception).
5. T5 (F16) Move `status`, `backups`, `keys list` to `work_read`.
6. T6 (F10) Probe the configured console host in `dg web` readiness,
   not hardcoded 127.0.0.1.
7. T7 (F11) Hoist the redaction regex; reassess the `gi` flag.
8. T8 (F7,F8) Terminal tables for `list`/`info`; join usage into
   `dg info` output.
9. T9 (F13,F14,F15,V6) Consistency sweep: decide the fate of the
   three unused console routes; add name patterns to the remaining
   inputs; client-side validate `key create`; add usage types to the
   TS client package if external consumers need them.
10. T10 (F9,F12) Honest tunnel readiness (established-forward probe,
    ssh timeouts); reuse one reqwest Client per CLI invocation.
11. T11 (V1,V4) One doc paragraph each: loopback trust model, and
    `init`-JSON → 0600 key-file extraction.
12. P1 (external) Live visual audit in the audit-tab browser once the
    Pi Chrome Connector companion responds; reload the extension and
    re-run `check.mjs`.
