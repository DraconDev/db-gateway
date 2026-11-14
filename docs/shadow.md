# Shadow mode: testing dg against a real application

Pre-production development note. This gateway is not ready for production
use or production data; shadow mode is how it earns that, without putting
an application at risk.

## What it is

The application keeps running on its current database (Turso/libSQL, or
anything with the `@libsql/client` `execute` / `batch` shape). Every write
the current database acknowledges is mirrored to a dg database, in the
same order, in the background. The current database stays the source of
truth: the application's calls return its answers and its errors,
unchanged, and nothing the application awaits waits on dg. When the dg
copy disagrees, the disagreement is reported, never raised.

After a few weeks of identical copies under real traffic, moving the
application is a configuration change instead of a leap.

## 1. Seed the dg copy

The two databases must start equal. Take a snapshot of the current
database as a SQLite file and import it:

```sh
turso db shell <db> .dump > app.sql     # or an existing SQLite file
sqlite3 app.sqlite < app.sql            # a plain SQLite file from the dump
dg --key-file owner.key import app --from app.sqlite
```

(`dg execute` cannot replay a `.dump` directly: a dump opens with
`PRAGMA foreign_keys=OFF` and `BEGIN TRANSACTION`, which the gateway
refuses. Building the file locally and importing it sidesteps both.)

Writes that land between the snapshot and turning the shadow on are
missing from the copy; take the snapshot with writes paused, or expect the
first comparison to show exactly those rows.

Issue a read-write key scoped to that one database for the shadow, not
the owner key:

```sh
dg --key-file owner.key key create --role database_read_write --database app app-shadow
```

## 2. Wrap the client

```ts
import { createClient } from '@libsql/client';
import { Gateway } from '@dracon/db-gateway';
import { withShadow } from '@dracon/db-gateway/shadow';

const turso = createClient({ url: process.env.TURSO_URL!, authToken: process.env.TURSO_TOKEN });
const dg = new Gateway(process.env.DG_URL!, process.env.DG_KEY!).database('app');

export const db = withShadow(turso, dg, {
  sampleReads: 0.05,   // also re-run 5% of reads on dg and compare the rows
  onDivergence: (d) => logger.warn({ shadow: d }, 'dg shadow divergence'),
});
```

`db.execute` and `db.batch` are drop-in replacements for the client's
own. `db.flush()` resolves once everything queued so far has reached dg
(useful in tests and before a comparison); `db.stats` counts mirrored
writes, compared reads, divergences and drops.

## 3. Compare

```ts
import { compareDatabases } from '@dracon/db-gateway/shadow';

const report = await compareDatabases(turso, dg);
// { identical, schemaDifferences: [...], tables: [{ table, primaryRows, shadowRows, identical, firstDifferences }] }
```

Every table is read from both sides in the same order (every column
ascending), page by page, so a table of any size compares within dg's
per-response row cap. Run it nightly; a run that is not `identical` names
the tables and the first rows that differ.

## Rust services (`dg_client::shadow`)

The same shadow exists for Rust, in `dg-client` behind the `shadow`
feature: `Shadow::spawn(database, options)` starts one background task on
the current Tokio runtime, and `shadow.mirror(statements, rows_affected)`
queues a write the primary acknowledged, without blocking or awaiting.
Several statements are applied as one atomic dg batch. Divergences go to
`options.on_divergence`; a time-dependent statement is reported once per
distinct text, not on every run.

For the platform's libsql services the hook lives in `api_core::db::SharedDb`
(feature `dg-shadow`), so every `execute` Turso acknowledges is mirrored,
and a transaction is mirrored as one batch only after its commit succeeds.
It is configured by environment, and does nothing when unset:

```sh
DG_SHADOW_URL=http://127.0.0.1:7432          # loopback (an SSH/WireGuard tunnel) or https://
DG_SHADOW_KEY_FILE=/etc/dracon/dg-shadow.key # 0600, a key scoped to the one database
DG_SHADOW_DATABASE=email
```

and compared with:

```sh
cargo run -p api-core --features dg-shadow --bin dg-shadow-compare -- \
  --url-env TURSO_EMAIL_URL --token-env TURSO_EMAIL_TOKEN \
  --ignore created_at,updated_at,processed_at,claimed_until,confirmed_at
```

`--ignore` leaves out columns each database fills from its own clock
(`datetime('now')`, including column `DEFAULT`s, which no statement text
reveals). Writes made before the shadow is attached -- the service's
startup migrations -- are not mirrored: seed the dg copy from a snapshot
taken after the service has started once.

## What the divergences mean

| Kind | Meaning | Usually |
|---|---|---|
| `shadow_error` | The primary accepted a write dg refused or failed on, or the queue overflowed. | A real gap: an unsupported statement, a SQL rule dg enforces, or dg unreachable. Investigate every one. |
| `rows_affected_mismatch` | Same write, different number of rows changed. | The copies had already drifted before this write. |
| `read_mismatch` | A sampled read returned different rows. | Drift, or a read that is not deterministic (no `ORDER BY`). |
| `unsupported` | The statement could not be sent as is: named parameters (`:name`), or an integer beyond ±2^53, which dg's JSON numbers cannot carry exactly. | Switch the call to positional `?` arguments; store huge integers as TEXT. Not mirrored, so the copies drift from here. |
| `nondeterministic` | The statement uses `random()`, `datetime('now')`, `CURRENT_TIMESTAMP` and the like. | Expected drift, not a dg fault. Compute the value in the application and bind it, and the copies stay equal. |

## Limits worth knowing

- Writes reach dg in the order the primary acknowledged them **as seen by
  one process**. Several application instances each mirror their own
  writes; concurrent writes from different instances can reach dg in a
  different order than they reached the primary. Order-sensitive drift
  from that shows up in the comparison; a single instance has none.
- Interactive transactions (`client.transaction()`) are not wrapped; only
  `execute` and `batch` are. Statements run inside a transaction object
  are not mirrored.
- Mirrored writes are not keyed. If dg is unreachable, the write is
  reported as `shadow_error` and the copy is behind from then on: reseed
  it (step 1) rather than replaying by hand.
