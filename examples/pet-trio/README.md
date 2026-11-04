# Pet trio — local system-workout for dg (no migration, no deployment-host)

Three tiny apps behind their own scoped keys, all against a disposable
local gateway. Proves the real TypeScript app path (`execute` / atomic
`batch` / scoped keys) plus the backup-capable export/re-import path per
database. Owner chose this scope 2026-10-16: no Turso fill, no migration —
just make sure the system works perfectly before any migrate decision.

- `todo.mjs` — CRUD: 3 todos, one marked done, scoped-key visibility check.
- `leaderboard.mjs` — atomic batch: 3 players + 4 scores, grouped leaderboard.
- `eventlog.mjs` — append-heavy: 50 time-series events, grouped counts.
- `run-pet-trio.mjs` — starts disposable `dg serve`, bootstraps owner key,
  creates one DB per app, issues one `database_read_write` key per DB,
  runs each workload through its scoped key, then exports each DB and
  re-imports as `<db>_restored` with exact row-count match.

Usage:

```sh
node examples/pet-trio/run-pet-trio.mjs
# transcript on stdout; save as docs/pet-trio-<date>.log
```

Exit 0 + `[VERDICT] pass` only if every app verifies and every restored
copy matches live. Latest transcript: `docs/evidence/pet-trio-2026-10-16.log`.

## Extensive workout

`extensive-trio.mjs` takes the same trio shape further: 8×100 concurrent
appends with zero lost writes, idempotent replay dedupe, atomic batch
rollback, all four denial codes (`sql_forbidden`, `database_forbidden`,
`invalid_credential`, `database_not_found`), two-way clone independence,
chunked-vs-full export match, SIGKILL restart with zero rows lost, a 2000
insert load burst with p50/p99, and a real versioned backup set via
`scripts/backup.sh` plus offline `dg backup verify`. Transcript:
`docs/evidence/pet-trio-extensive-2026-10-16.log`.
