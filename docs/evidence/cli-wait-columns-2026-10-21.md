# Per-database wait columns — what each cell actually holds

Evidence for the 2026-10-20 audit of goal `20261020152234-95edx5` (the `dg`
CLI operator-surface goal). The audit's complaint was that the per-database
table's `wait p95` cell held the database's **maximum** wait, so the screen
contradicted itself: the per-database `wait p95` and `wait max` read the same
number, and that number disagreed with the genuine percentile in the
`admission` block above it.

Recorded 2026-10-21, on commit `c7bdfe2` (the tip of `main` on `origin`,
`github` and `gitlab`).

## The fix

`src/render.rs:883` reads `queue_wait_p95_us` for the `wait p95` cell and
`src/render.rs:884` reads `queue_wait_max_us` for the `wait max` cell. The
pairing is defined once, so the two cells cannot be swapped.

## The scenario (deterministic, not a race)

The server's per-database **max** is a lifetime maximum; its **p95** is a
percentile over a rolling ring of the last 64 waits. To make the two numbers
differ, and to make them differ *every* time:

1. a slow read (a recursive CTE) holds the shared admission guard — a read
   takes the shared guard, which is exactly what a write must wait for;
2. a write arrives while the guard is held, queues behind it, and records one
   **large** wait;
3. 70 follow-up queries push that one large wait out of the rolling ring, so
   the ring's percentile is small again while the lifetime maximum still
   remembers the large wait.

The result: a database whose p95 is small and whose max is large.

## Server truth (`dg --json status`)

```json
"wait_db": {
    "last_write": 1792547253,
    "queue_timeouts": 0,
    "queue_wait_max_us": 99289,
    "queue_wait_ms": 99,
    "queue_wait_p95_us": 6,
    "queue_waits": 73,
    "rows_read": 71,
    "rows_written": 1
}
```

`queue_wait_p95_us = 6`, `queue_wait_max_us = 99289` — two different facts.

## What the terminal shows (real pty capture, ANSI stripped)

```
service
  version    0.2.0
  uptime     32s
  databases  1 live, 0 tombstoned

activity
  requests    77
  rate        144/min
  errors      0
  error rate  0%

recent activity
  req/s                                                 ▁   ▁▅███▅

admission
  acquire timeout  500 ms
  read permits     8
  write permits    4
  wait p50         4 us
  wait p95         7 us

per-database
┌──────────┬───────────┬──────────────┬───────┬──────────┬──────────┬────────────┐
│ database │ rows read │ rows written │ waits │ wait p95 │ wait max │ last write │
├──────────┼───────────┼──────────────┼───────┼──────────┼──────────┼────────────┤
│ wait_db  │        71 │            1 │    73 │     6 us │    99 ms │ 7s ago     │
└──────────┴───────────┴──────────────┴───────┴──────────┴──────────┴────────────┘
wait p95 is this database's own recent waits; wait max is its worst single wait
```

The per-database `wait p95` cell reads **6 us** — the server's
`queue_wait_p95_us` — and the `wait max` cell reads **99 ms** — the server's
`queue_wait_max_us` (99289 µs). The two cells hold their own facts.

The `admission` block's `wait p95 7 us` is the **service-wide** percentile
across every database, a different scope by design; it is not the source of
the per-database cell, so the two are allowed to differ by a microsecond or
two.

Rendered image of the same capture: [`dg-status-wait-columns-2026-10-21.png`](dg-status-wait-columns-2026-10-21.png).

## `dg inspect wait_db` (same pairing, one database)

```
activity
  rows read     71
  rows written  1
  queue waits   73
  wait p95      6 us
  wait max      99 ms
  last write    11s ago
```

## The published branch is green (not just this working tree)

The audit's second required fix was that `origin/main` still carried the red
mutation commit `6e36b4d`. The fix is published, and a **fresh clone** of the
published branch is green:

```
$ git clone --branch main git@github.com:DraconDev/db-gateway.git fresh-clone
$ cd fresh-clone && cargo test --workspace --locked
...
313 passed, 0 failed, 1 ignored        # EXIT=0
```

The clone's `src/render.rs` is byte-identical to the local file, and its
`git show HEAD:src/render.rs` matches the checked-out worktree — the
`dracon-warden` git filter changes nothing on checkout.

## Gates (all green, on `c7bdfe2`)

| Gate | Command | Result |
| --- | --- | --- |
| Unit + integration | `cargo test --workspace --locked` | 313 passed, 0 failed, 1 ignored (exit 0) |
| Published branch | same, in a fresh clone of `origin/main` | 313 passed, 0 failed, 1 ignored (exit 0) |
| Lint | `cargo clippy --workspace --all-targets --locked -- -D warnings` | exit 0 |
| Format | `cargo fmt --all -- --check` | exit 0 |
| Supply chain | `cargo deny check` | advisories ok, bans ok, licenses ok, sources ok |
| Script lint | `scripts/lint-scripts.sh` (shellcheck 0.11.0) | clean (27 shell + 8 python + 1 node) |
| Secret scan | `scripts/secret-scan.sh` | clean (297 text files scanned, 5 patterns) |
| TypeScript client | `npm test --prefix client` | 16 passed, 0 failed |
| Web console | `npm test --prefix web` | 39 passed, 0 failed |
| Documentation | relative links in `docs/api.md`, `README.md`, `docs/cli-turso-mapping.md`, `CHANGELOG.md` | 14 checked, 0 broken |

## The test that guards this

`tests/cli_views.rs::the_per_database_wait_columns_name_what_they_hold` reads
the server's own JSON, requires `p95 < max` (so a swapped pair could not pass
by accident), locates the `wait p95` and `wait max` **columns by their
headers**, and asserts each cell against the server's value. The scenario is
the deterministic one above, so the test does not depend on a scheduling race.
