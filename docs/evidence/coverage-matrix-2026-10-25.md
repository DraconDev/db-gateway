# Coverage matrix — 2026-10-25 (micro2, dg 0.5.0 → 0.6.0)

What was actually run, against what, and what the answer was. Every
number here came from a run in this session; nothing is carried over
from an earlier session without saying so.

## Why this exists

The box held two databases and one statement shape. Every surface that
shows traffic, cost or history had nothing to show, so "is it working?"
was unanswerable from the box itself. This is what was run to answer
it, and what came back.

## 1. Load (against the repo's own harnesses, isolated from micro2)

| Harness | Result | Numbers |
|---|---|---|
| `scripts/soak.py` (300 s) | **PASS** | 13,003 requests, **0 errors**, p50 11.6 ms, p99 102.6 ms, RSS 21.2 MB → 12.5 MB (shrank) |
| `scripts/soak_starvation.py` (run 1) | **MARGINAL** (exit 1) | control 0/1311 writes rejected; flood **1/1011** rejected (0.10 %) |
| `scripts/soak_starvation.py` (run 2) | **MARGINAL** (exit 1) | control 0/1233; flood **1/1063** rejected (0.09 %) |
| `scripts/noisy_neighbor_cross_db.py` | **COLD_OK** | cold reject 0.000 % → 0.000 % under a hot flood; cold p99 299.9 → 428.9 → 181.4 ms; hot writers 383/387, readers 1118/1127 (4 and 9 sheds) |

**The honest reading of MARGINAL.** Under a read flood aimed at one hot
database, roughly **one write in a thousand is shed**. The flood arm
also had 5–6 queue timeouts the control arm never had. The harness
grades that as MARGINAL — degraded, not rejected — and its exit code
says so.

**How that compares.** The same harness on 2026-10-11 graded **STARVED**
(0.19 % → 1.82 %, ×9.4). 0.5.0 is materially better than that, but it is
not zero, and it is not NO_STARVATION.

## 2. Function (against the live gateway on micro2, through the tunnel)

| Surface | Result |
|---|---|
| `scripts/live_exercise.py` (fresh prefix) | **73/73 checks** |
| `scripts/live_exercise.py --reuse` (against the real databases) | **42/42 checks, 7 skipped** (the skips are the credential checks: a taken name cannot be re-read) |
| `dg mcp` (all six tools, driven as an agent would) | **17/17 checks**, incl. UPDATE / DELETE / DROP / INSERT all refused, unknown tool refused, missing database refused, undeclared argument refused |
| Credential scope | read-only key cannot write (403 `sql_forbidden`), cannot reach another database (403 `database_forbidden`), can read its own |
| `dg diff` | finds the divergence and exits 1 (`databases_differ`) |
| `dg branch` | create → list → show → diverge → diff → delete, all clean |
| `dg integrity` | clean on all five exercise databases |
| `dg explain`, `dg vacuum`, `dg restore plan/list`, `dg queries`, `dg audit`, `dg inspect`, `dg info`, `dg key list/tokens`, `dg status`, `dg watch`, `dg backup run` | all answered |

## 3. Recovery (the part that was never proven before)

| Check | Result |
|---|---|
| `dg backup verify --backup-root … latest` (on the host) | **ok: true**, `has_registry: true`, set `20261025T124933Z`, 8 databases with sha256 each |
| `scripts/bucket-restore-drill.sh` (on the host, run by hand) | **PASS** — 8 databases **plus the registry** restored from the off-host bucket and verified table-by-table against the live gateway |
| Live data after the 0.5.0 restart | intact — `verify_probe` = 5 rows, all five exercise databases intact |

The drill is the one that matters: it restores every live database from
the bucket into a scratch directory and compares per-table row counts
against what the gateway is serving. It had last run on 2026-10-19, on
two databases. It ran today on eight.

## 4. The terminal view (captured live, one frame per tab)

All six tabs captured against micro2 and rebuilt with the repo's own
`scripts/render-tui-capture.py`: databases (8 live, with real traffic),
service (version, uptime, 18.8k requests, 0 errors), backups, keys
(5 credentials), queries (33 shapes across the five real databases),
locations (8 files plus the tombstone), and the palette.

## 5. The state the box was left in

`dg 0.6.0` deployed and active, with `dg`, `dg-litestream`,
`dg-restore-points.timer` and `dg-backup.timer` all running.

**Kept** (by the operator's choice): `inventory`, `orders`, `ledger`,
`sessions`, `notes`, and the three scoped credentials `warehouse`
(read-only on inventory), `checkout` (read-write on orders) and
`finance` (read-only on ledger). The query log holds 34 statement
shapes across the five real databases; the audit trail holds 593
entries across fifteen principals.

**Left behind:** 29 tombstones from the test runs (`t1…` through
`t4…`, plus the branch probes). The locations tab lists them, because
it lists every file on disk and says which are live. They purge
themselves after the 24-hour retention window; there is no early
purge, by design — a tombstone holds its name so it cannot be
recreated with different content behind it.

## 6. What this exercise found and fixed

| Defect | Fix | Release |
|---|---|---|
| The palette ran a title into its hint — "rotate owner keythe old owner key dies…" | the title column is measured from the titles on screen | 0.5.0 |
| A branch whose database was deleted another way could not be removed (409, record stranded in the registry) | the record is removed either way, and the answer says the database had already gone | 0.5.0 |
| The live exercise could only run once per box | the branch name is unique per run; the write replaces its row | 0.6.0 |
| A credential the exercise could not issue was reported as a failure | it is now a skip, with the reason | 0.6.0 |

Both 0.5.0 fixes were proven on the running service: the palette now
shows a gap on every row, and `dg branch delete` on an
already-tombstoned branch returns `{"deleted": true, "already_deleted":
true}` and empties the registry.

## 7. What is still not proven

- **CI never ran.** GitHub Actions fails in ~2 s on an account billing
  gate ("recent account payments have failed or your spending limit
  needs to be increased"). Every CI step was run locally instead. The
  code is untested *by CI*, not untested.
- **`cargo audit` cannot reach the network here** (it times out on a
  live fetch). `cargo deny check` passes, and `cargo audit --no-fetch`
  passes against the cached RustSec database.
- **One write in a thousand is shed under a read flood.** See §1. It is
  a rate, not a failure, but it is not zero.
- **The query log lives in memory**, so it starts empty when the service
  restarts. That is a deliberate choice, but it means the queries tab
  says nothing until traffic happens.
- **The queries tab shows tombstoned databases.** The log keeps a
  tombstoned database's shapes until the service restarts, so the tab
  can show a database the databases tab no longer lists.
- **No TLS.** Remote use rides an SSH tunnel to the loopback listener,
  and clients refuse plaintext non-loopback endpoints. Nothing here
  tests a TLS-terminated edge.
- **The off-host drill is weekly.** It ran by hand today; the timer
  still runs it on Sundays.
- **No fault injection against the bucket or the sidecar** beyond the
  drill itself — no S3 outage, no corrupted object, no sidecar crash
  mid-write.
- **One host.** micro2 is the only deployment exercised. A second host
  with a different filesystem, a different bucket, or a different clock
  is untested.
