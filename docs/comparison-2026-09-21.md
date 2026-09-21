# SQLite-platform comparison: how others do it, and where db-gateway stands

Date: 2026-09-21. Pricing/limit figures change — re-check vendor pages before budgeting.
Sources inline; third-party-only claims marked UNVERIFIED.

## 1. The systems, in one paragraph each

### Turso (managed libSQL)
Single-writer primary per database with read replicas; legacy edge replicas are
deprecated for new users in favour of **embedded replicas** (a local SQLite file
that syncs to/from the cloud primary). Clients speak the libSQL protocol over
HTTP and get JSON back — no file handle, no local storage. Auth is per-database
tokens (`turso db tokens create`, expiring). CLI covers create/destroy/tokens.
Backups are PITR by restoring to a **new** database at a timestamp, which means
a new connection string + new token (third-party reports; confirm on Turso docs).
PITR window and replica/location counts are plan-gated (≈10-day PITR reported,
UNVERIFIED).

### Cloudflare D1 (managed serverless SQLite)
SQLite at the edge behind Workers + HTTP API, up to 10 GB per database, global
read replication included, **Time Travel** PITR: 30 days on Paid, 7 days on
Free, max 10 restores per 10 min per DB ([D1 limits](http://developers.cloudflare.com/d1/platform/limits/)).
D1's stated philosophy is per-tenant/per-entity databases (50k DBs/account
reported by third parties, UNVERIFIED). Pricing is queries + storage, no egress.

### Litestream (open-source, sidecar replication)
Separate process tails the SQLite WAL and streams changes to S3-compatible
storage; `litestream restore` rebuilds the DB. No app code changes, pennies/day
in object storage ([litestream.io](https://litestream.io/)). Async: a host failure
can lose the most recent un-replicated writes. Single-node philosophy — it makes
one server safe, it does not make many servers one database.

### LiteFS (open-source, Fly.io)
FUSE filesystem intercepts SQLite journal/WAL writes, repackages them as
**LTX files** (checksummed, TXID-ordered, rolling whole-DB checksum), streams
them to replicas over HTTP. Leader election is **leases via Consul** (or static),
explicitly **not** Raft — ephemeral hosts can't do strong membership
([ARCHITECTURE.md](https://github.com/superfly/litefs/blob/HEAD/docs/ARCHITECTURE.md)).
Async replication: a lagging replica that wins election can lose transactions.
`.primary` file tells the app who the writer is.

### rqlite (open-source, Raft)
Every node runs SQLite + `hashicorp/raft`; HTTP API on :4001, Raft on :4002.
Writes go through Raft consensus into the FSM, then SQLite; reads have tunable
consistency levels. Ships `rqlited` + `rqlite` CLI shell + `rqbench`.
Positions itself as HA SQLite for moderate writes, replacing Postgres/MySQL
where availability matters more than write scale ([rqlite.io](https://github.com/rqlite/rqlite.io/blob/HEAD/content/en/docs/features/_index.md)).

### dqlite (open-source C library, Canonical)
Embeds in-process; C-Raft for consensus (transactional, low-latency), custom
wire protocol for network clients. Powers LXD/Juju controllers. A library, not
a service: you build your HA control plane out of it.

## 2. Comparison matrix vs db-gateway

| Axis | Turso | D1 | Litestream | LiteFS | rqlite | dqlite | **db-gateway (ours)** |
|---|---|---|---|---|---|---|---|
| Shape | Managed service | Managed service | Sidecar binary | FS + cluster | Cluster of nodes | Embedded lib | **Self-hosted gateway binary** |
| Write model | Single primary | Single primary (+Sessions) | Local file, 1 writer | Leased primary | Raft quorum | Raft quorum | **Single gateway process** |
| Read scale | Replicas / embedded | Global read replicas | N/A (1 node) | Async replicas | Quorum reads / stale reads | Quorum | **None yet — admission bottleneck (audit §) is the ceiling** |
| Consistency | Read-your-writes (embedded) | Session-bounded | N/A | Async, can lose txns on failover | Tunable (strong/stale) | Strong | **Strong (single process, SQLite)** |
| Auth model | Per-DB tokens, expiring | Cloudflare API tokens | N/A (storage creds) | N/A (network trust) | Basic-auth file (UNVERIFIED; docs stress network security) | App's problem | **Per-DB scoped keys, prefix+SHA-256, owner rotation — closest to Turso tokens** |
| Backup / PITR | PITR to new DB, plan-gated | Time Travel 30d/7d, rate-limited | Continuous WAL→S3, restore CLI | LTX stream (+ external backup) | Snapshots + Raft log | Snapshots | **Gateway-native snapshot backups (proven micro2), no PITR yet** |
| RPO on crash | Small (async) | Managed | Last un-shipped WAL | Lagging-winner txns | Zero (quorum) | Zero (quorum) | **Zero for committed txns (local disk); backup RPO = snapshot cadence** |
| Multi-DB story | 500 DBs free tier (UNVERIFIED) | Per-tenant pattern, 50k/acct (UNVERIFIED) | One config per DB file | Per-mount DB | One cluster = one logical DB | One cluster = one DB | **Hundreds of DBs, one process — our core bet, still unproven past drill scale** |
| CLI | `turso` (full lifecycle) | `wrangler d1` | `litestream replicate/restore` | `litefs` mount cmd | `rqlite` shell | `dqlite-tools` | **TS CLI 15/15 green, lifecycle+keys+backup** |
| Idempotency / resume | SDK retries | N/A | File offsets | TXID + checksum resume | Raft log index | Raft log index | **Chunked resumable upload, SHA-256 manifest — closest to LTX TXID+checksum idea** |
| Cost | Per-GB/row, plan gates | Queries+storage | Pennies/day S3 | Hosts + Consul | 3+ hosts always | Hosts | **One host + object storage (OVH deferred)** |

## 3. How we compare — honest read

**Where we're genuinely close to the field.**
- Auth looks like Turso's: per-database scoped tokens with rotation is exactly
  the managed-service shape. Nobody in the open-source cluster camp does this
  well (rqlite leans on network trust; LiteFS has no auth story).
- Chunked resumable transfer with a hash manifest is the same *idea* as LTX
  (ordered, checksummed change units) applied to whole-DB upload instead of
  per-transaction replication. Ours is simpler and easier to verify.
- Gateway-native snapshot backup + restore drills put us ahead of "just run
  Litestream" for operators who want one binary, not a sidecar per DB.

**Where we're behind and should admit it.**
- No read scaling at all: Turso/D1/LiteFS all answer "reads are hot" with
  replicas; our answer today is "the gateway takes it," and the audit already
  found the admission bottleneck. This is the single biggest architectural gap.
- No PITR: D1 gives 30 days, Turso ~10 (UNVERIFIED), Litestream gives
  WAL-granularity restore. We have snapshot cadence only — RPO equals the cron
  interval, and cron isn't even scheduled yet (pending owner approval).
- No HA story: everything in the Raft camp (rqlite, dqlite) survives a dead
  host with zero data loss; we survive a dead host only up to the last backup.
  That is the Litestream-grade guarantee, not the Turso-grade one.

**Where our bet differs from everyone (and could be right).**
- Nobody else targets *hundreds of small DBs behind one self-hosted binary*.
  Turso/D1 do many-DBs as a managed fleet; rqlite/dqlite do one logical DB per
  cluster; LiteFS does one cluster per app. If the workload really is "many
  small SQLite DBs, one operator, one box," the gateway shape has no direct
  competitor — but it also has no proof until the D1–D3 scale measurements run.

## 4. Implications / what to steal

1. **Steal D1's rate-limited restore discipline** (10 restores/10 min): cheap
   guard against restore-abuse; fits our scoped-keys model.
2. **Steal Litestream as the backup transport option**: gateway-native snapshots
   stay default, but offering WAL-shipping to the same OVH bucket later would
   cut RPO from "cron interval" toward "seconds" without changing the API.
3. **Steal rqlite's tunable read consistency** when we touch replicas: stale
   reads for dashboards, strong reads for writes — but only after the admission
   bottleneck is fixed (phased scale proposal), not before.
4. **Do not steal LiteFS's lease election** until multi-host is real: async
   replication that can lose transactions on failover is worse than our honest
   "last backup wins" story. If we ever go multi-host, prefer the Raft camp's
   zero-loss guarantee (rqlite/dqlite model), not leases.
5. **Prove the unproven column**: the matrix shows our row's weakest cell is
   multi-DB scale — exactly the pending D1–D3 measurements. Everything else is
   commentary until those numbers exist.
