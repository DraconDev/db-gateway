# Backup compatibility assessment (public docs only, no live verification)

Status: **research brief, not an adoption decision**. No accounts were
accessed, no bucket was touched, and no backup service was installed. Every
live behavior below needs the sandbox checks in §6 before Litestream or any
OVH bucket is trusted with real data.

## 1. Examined releases and sources (accessed 2026-09-18)

| Component | Examined | Source |
| --- | --- | --- |
| Litestream docs, actively maintained line | v0.5.x (“Latest — actively maintained”), previous line v0.3.14 | https://litestream.io/ |
| Litestream latest release tag | v0.5.17, published 2026-08-31 (GitHub releases API) | https://github.com/benbjohnson/litestream/releases/tag/v0.5.17 |
| Litestream S3-compatible replica config | URL `s3://bucket/path?endpoint=…` or explicit `type/bucket/path/endpoint/region/access-key-id/secret-access-key` fields; provider auto-detection list; `force-path-style`, `skip-verify` options | https://litestream.io/guides/s3-compatible/ |
| Litestream S3 guide | IAM user, bucket, CLI/file usage, access-point and custom-endpoint notes | https://litestream.io/guides/s3/ |
| Litestream config reference | Global settings, retention, snapshots, L0 retention, compaction, per-database snapshot settings, `sync-interval`, replica settings | https://litestream.io/reference/config/ |
| Litestream operational caveats | WAL-only operation, `busy_timeout` recommendation, checkpoint write locks | https://litestream.io/tips/ |
| OVH Object Storage S3 getting started | S3 API for new projects, project + user + access/secret key, per-bucket endpoint URL | https://docs.ovhcloud.com/en/guides/storage-and-backup/object-storage/s3-getting-started-with-object-storage (page `lastUpdated: 2026-06-01`) |
| OVH endpoints/regions | `https://s3.<region>.io.cloud.ovh.net`, virtual-host style, region/storage-class table, SigV4 | https://docs.ovhcloud.com/en/guides/storage-and-backup/object-storage/s3-location (page `lastUpdated: 2026-03-06`) |
| OVH S3 FAQ | S3-compatible classes vs legacy Swift, storage classes, compatibility-guide pointer | https://docs.ovhcloud.com/en/guides/storage-and-backup/object-storage/s3-faq (page `lastUpdated: 2026-03-06`) |
| OVH technical limitations | 1 Gbps/connection, 300 PUT/s + 900 GET/s soft limits, bucket/object/multipart quotas | https://docs.ovhcloud.com/en/guides/storage-and-backup/object-storage/s3-limitations (page `lastUpdated: 2026-03-13`) |
| OVH encryption | SSE-C (customer key per request) vs SSE-OMK (OVH-managed, transparent) vs CSE comparison | https://docs.ovhcloud.com/en/guides/storage-and-backup/object-storage/s3-encrypt-your-objects-with-sse-c (page `lastUpdated: 2026-07-03`) |
| OVH lifecycle/retention | Expiration/transition rules, `.io`-endpoint-only support, versioning-dependent behavior | https://docs.ovhcloud.com/en/guides/storage-and-backup/object-storage/s3-bucket-lifecycle (page `lastUpdated: 2026-07-29`) |
| OVH identity/access | Object Storage users, access/secret keys, per-bucket profiles, private-by-default resources | https://docs.ovhcloud.com/en/guides/storage-and-backup/object-storage/s3-identity-and-access-management (page `lastUpdated: 2026-06-01`) |

The frequently quoted S3/Swift REST matrix
(`pcs-object-storage-standard-s3-and-swift-rest-api-compatibility`, dated
2022-03-23) describes **legacy Swift**, not the current S3 classes — it is
**not** evidence for this project. The current S3 compliancy guide exists
(`s3-s3-compliancy`, linked from the FAQ) but its operation matrix was not
extracted here; required-operation coverage is a sandbox check (§6.4).

## 2. SQLite / WAL constraints (Litestream side)

- Litestream **only replicates databases in WAL journal mode** and will
  enable WAL itself if the application has not (`/tips/`). The gateway
  already runs WAL (`src/execution.rs`), so the modes align — but Litestream
  becomes an extra process holding periodic **short write locks during
  checkpoints**, and SQLite returns lock errors to writers unless they wait
  (`/tips/`).
- Litestream documents `PRAGMA busy_timeout = 5000;` (5s) for application
  connections. The gateway sets a **250ms** busy timeout (`src/execution.rs`)
  with a 2s query deadline. Whether checkpoint contention surfaces as extra
  `busy`/`capacity_exceeded` pressure on this host is **unverified** and must
  be measured (§6.2), not assumed away.
- Application guidance also recommends `PRAGMA foreign_keys = ON` per
  connection; the gateway already sets it. No conflict found at doc level.
- Litestream ships `restore`, `status`, `sync`, `info`, `list` commands; the
  restore path must land on a **separate target** and then pass the gateway’s
  own `integrity_check` + foreign-key + protected-table validation (§6.5).

## 3. Findings by contract area

| Area | What the docs say | Fit / risk for this project |
| --- | --- | --- |
| Documented version | v0.5.x maintained; v0.5.17 tagged 2026-08-31; v0.3.14 is the previous line | Pin `v0.5.17` (or newer only after re-reading the changelog) for any sandbox; never float on “latest” |
| WAL constraints | WAL-only; auto-enables WAL; checkpoint write locks; 5s `busy_timeout` recommended | Mode-compatible; contention against the gateway’s 250ms timeout + 2s deadline is the main measured risk |
| Endpoints/regions | One endpoint pattern `https://s3.<region>.io.cloud.ovh.net` for all buckets/objects in both Standard and High Performance; virtual-host style; region table with 1-AZ vs 3-AZ and per-tier support; SigV4 | Clear convention; region/storage-class choice is still an owner decision (see `docs/recovery-decisions.md` §6) |
| Authentication | Public Cloud project + Object Storage user + access key/secret key; per-bucket access profiles; resources private by default; Litestream takes `access-key-id`/`secret-access-key` via config or environment | No conflict at doc level; least-privilege bucket/key scoping is a sandbox check, and no keys exist yet |
| Encryption | SSE-C (caller supplies 256-bit key per request; loss = unrecoverable), SSE-OMK (per-object OVH-managed keys, transparent), CSE (client-side, out of scope of that guide); no price difference stated | Choice is open: SSE-C gives key ownership at operator-risk cost; SSE-OMK is zero-effort. Whether any encryption is on by default is **unverified** |
| Retention | Bucket lifecycle rules support expiration/transition, incomplete-MPU cleanup, cost-tier transitions; `.io` endpoint only; behavior varies with bucket versioning state | Mechanism exists for the 7–30 daily-version retention in the recommended profile; exact rule/versioning interplay for Litestream’s LTX layout is **unverified** |
| Multi-database | Litestream config takes a `dbs:` list (one entry per SQLite path, each with its replica), plus global replica defaults and directory replication | Matches the gateway’s many-independent-databases layout, including the registry `metadata.sqlite` as one more path — but per-db registration/reload behavior is a sandbox check |
| Reload support | Config reference was fetched, but reload-without-restart behavior (signal, `register`/`unregister` IPC semantics for adding a database) was **not** confirmed from the fetched pages | **Unverified** — must be proven in the sandbox before promising “add a database without restart” |
| Throughput quotas | 1 Gbps/connection, 300 PUT/s + 900 GET/s soft defaults (sharding raises them), multipart part/part-count limits | Far above this project’s small-DB snapshot traffic; not a selection risk, but restore-egress cost still needs the pricing the owner has not set |
| Pricing | Not extracted in this pass | **Unverified** — storage class × region × versions retained must be priced before the retention decision |

## 4. What is explicitly NOT claimed

- That Litestream v0.5.17 works against the owner’s actual OVH region/class
  (OVH is **absent** from Litestream’s endpoint auto-detection list, so an
  explicit `endpoint` + `region` + path-style configuration is required and
  untested).
- That checkpoint locking, `sync-interval` traffic, or compaction fits the
  1 GB host alongside serving load.
- That lifecycle/versioning rules implement the chosen retention without
  deleting Litestream segments early.
- That restore output passes gateway import validation and row-level checks.
- Any pricing, SLA, durability, or region-availability promise.

## 5. Relation to the existing gateway path

The gateway’s tested export (online backup API) → validated import
(`integrity_check`, foreign-key check, protected-table screening, atomic
publication) already covers **manual snapshot recovery**. Litestream would
add *frequency* (minutes-scale RPO objective), not a new trust root: every
Litestream-restored file must still go through restore-to-separate-target +
integrity + application-data verification before it is trusted. The
`_dg_replay` table travels inside each database file; the registry
(`metadata.sqlite`, credential hashes, database rows) needs its own backup
coverage or restored databases lose their authorization context.

## 6. Sandbox checks required before adoption

Run all of these against disposable data and a dedicated test bucket; no
production databases, credentials, or subscriptions.

1. **Pin and record**: install exactly `v0.5.17` in the sandbox, record
   `litestream version` output and the fetched config-reference date. Any
   upgrade re-runs this list.
2. **WAL contention**: replicate 2+ live gateway databases under a small
   write loop; assert no new `busy`/deadline failures vs baseline and record
   p50/p99 write latency with and without replication.
3. **Explicit OVH endpoint config**: connect with explicit `endpoint`
   (`https://s3.<test-region>.io.cloud.ovh.net`), `region`, bucket, and
   `force-path-style`/`skip-verify` variants documented; record the minimal
   working config and the failing variants.
4. **Required-operation coverage**: confirm every S3 operation Litestream
   v0.5.17 issues (including multipart and any versioning/lifecycle reads)
   succeeds against the test region/class; save the passing matrix.
5. **Restore drill**: `litestream restore` to a separate path, then gateway
   import validation + exact-row comparison against the source; also restore
   a mid-history point and a corrupt-artifact rejection case.
6. **Retention drill**: apply the candidate lifecycle rules, age/rotate test
   objects, and prove newest-verified-copy age plus version counts match the
   intended 7/30-day policy without breaking restore.
7. **Registration/reload**: add and remove a database from the Litestream
   config and record whether replication picks it up without restart (and
   the exact signal/command used).
8. **Failure visibility**: revoke/break credentials, block egress, and fill
   the destination toward quota; record how each surfaces (log, status,
   metric) and that serving writes keep working with a visible stale-backup
   signal.
9. **Resource headroom**: record gateway + Litestream CPU/RSS during steady
   replication and during restore on a 1 GB-shaped sandbox; keep the report
   with the measurement (see the resource-measurement queue item).
10. **Encryption choice**: exercise the selected mode (SSE-C with a test key
    vs SSE-OMK) end-to-end including a restore without ambient credentials;
    for SSE-C, prove key loss is handled by policy, not surprise.
11. **Registry coverage**: back up and restore `metadata.sqlite` alongside
    database files and prove scoped keys resolve after recovery.
12. **Cleanup**: delete all test buckets/keys/configs and record the
    deletion; no residue in the owner’s account.
