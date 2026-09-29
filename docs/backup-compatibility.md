# Backup compatibility assessment (public docs only, no live verification)

Status: **research brief, not an adoption decision**. No accounts were
accessed, no bucket was touched, and no backup service was installed. Every
live behavior below needs the sandbox checks in §6 before Litestream or any
OVH bucket is trusted with real data.

## Adoption boundary (updated 2026-09-19)

This brief establishes a candidate path, not a deployment plan. The current
working candidate is Litestream **v0.5.17** feeding an explicitly configured
OVH S3 endpoint, with gateway exports and validated imports retained as the
manual fallback. It is not adopted until the sandbox proves WAL contention, required
S3 operations, restore correctness, retention behavior, failure visibility,
registry coverage, encryption, and 1 GiB headroom. The first supervised
micro2 deploy did not install a backup sidecar and does not authorize one.

The minimum adoption packet is: pinned binary/version output; redacted config
shape and least-privilege credential scope; passing operation/restore/
retention/failure matrices; resource report; cleanup evidence; and an
owner-accepted RPO/RTO/retention profile from `docs/recovery-decisions.md`.
Until that packet exists, documentation must say **unverified** rather than
implying OVH durability or a recoverable-point guarantee.

## Local Profile C rehearsal boundary

The local rehearsal uses a filesystem object mock around the gateway's
SQLite-aware export and validated import paths. It records the owner-selected
working objectives — continuous WAL-style shipping with a five-minute lag
objective, seven days of frequent points, 30 daily production / 7 daily
development versions, every-sync verification, daily separate-target restore,
and a 15-minute RTO objective — but it does **not** emulate Litestream timing
or S3 semantics. It injects crash-during-shipping, partial/corrupt object,
permission/outage, and disk/WAL-pressure failures. A failed gate blocks rollout
and leaves manual validated snapshots as the fallback.

Run it with:

```sh
cargo run --locked --bin recovery_rehearsal
cargo test --locked --test recovery_rehearsal
```

The command's JSON, recorded in `docs/recovery-rehearsal-evidence.json`, is
evidence for local status transitions, corrupt-artifact rejection without
publication, writes continuing during sink outage, WAL pressure, exact-row
separate-target restore, and measured local restore time.
It does not authorize OVH, Turso, provider credentials, production data, or
micro2 access; the sandbox gates below remain required before any adoption.

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

## 6a. Sandbox run status (2026-09-28, rehearsal on 2026-09-29)

**Nine of twelve gates PASSED in rehearsal. Three remain BLOCKED on the owner's
test bucket and the 1 GB VPS, and one is BLOCKED on a host that cannot enforce
a cgroup memory limit. Adoption is NOT claimed by this packet; it is claimed
by the OVH run, with this rehearsal as its evidence about execution and about
nothing else.**

`scripts/rehearse-sandbox-packet.sh` runs the whole packet against a local
disposable S3-compatible endpoint (gofakes3 in a SigV4-validating proxy), then
checks five post-conditions the packet cannot check about itself: no temp tree
left, no surviving proxy/store process, no end-point state directory, the
packet declares `mode: REHEARSAL`, and every gate that needs a bucket
actually ran. The endpoint is named in each gate's output
(`service behind it:` line, below), so a rehearsal can never be read as OVH
evidence by accident.

| gate | state | evidence |
| --- | --- | --- |
| 1 pin and record | **PASS** | `litestream 0.5.17`, tarball sha256 matched against the release's own `checksums.txt`, installed beside (not over) the v0.3.13 on `PATH`, which must not be used for §6 |
| 2 WAL contention | rehearsal PASS | replication advanced under 60 live gateway writes with 0 busy/deadline failures; **no** p50/p99 comparison and no 1 GB-shaped sandbox yet |
| 3 explicit OVH endpoint | rehearsal PASS | explicit `endpoint`/`region` are honoured and the minimal working replica URL is `s3://<bucket>/<prefix>?endpoint=<scheme://host>&region=<r>`; the same URL FAILS without the scheme and without the rehearsal CA, so the pass is neither a transport artifact nor a disabled-verification artifact |
| 4 operation coverage | rehearsal PASS | the operation matrix below is MEASURED, not assumed, and both databases restored with exact row counts |
| 5 restore drill | rehearsal PASS | restored to a separate path, 260 rows matching the source exactly; **no** gateway import validation, mid-history point, or corrupt-artifact case yet |
| 6 retention drill | rehearsal PASS | candidate 7/30-day lifecycle policy stored and read back: 27 rules |
| 7 registration/reload | rehearsal PASS | a database added at runtime was replicated with no restart, via `litestream register` over the control socket |
| 8 failure visibility | rehearsal PASS | each of the three injected failures (revoked credentials, blocked egress, near-quota) kept the gateway serving writes, kept the faulted write out of the bucket, and surfaced on at least one operator-visible surface; the healthy control phase raised nothing false; after the fault cleared the missing writes were in the bucket again. **But the daemon log is not a complete surface**: see the findings below. |
| 9 resource headroom | BLOCKED | the kernel here SETS `memory.max` but does NOT OOM-enforce it, so the gate's enforcement probe rejects every mechanism and runs the workload unconstrained. Numbers printed, none offered as a 1 GB-shaped result. The OVH 1 GB VPS is the host that can answer this gate. |
| 10 encryption | BLOCKED | decision 3 cannot be settled without the bucket; see the encryption finding below |
| 11 registry coverage | BLOCKED | needs the real bucket + gateway combination |
| 12 cleanup | rehearsal PASS | every test object and version deleted through both `?versions` and `?list-type=2`, the bucket no longer answers. **After a durable fix**: the rehearsal store's original `-backend fs` left empty directories on the disk for keys that contained a `/`, which made `DeleteBucket` answer `BucketNotEmpty` with zero objects listed -- not S3's emptiness rule. The stand-in now runs `-backend memory`, which has no directories to confuse the answer, and the bucket is genuinely gone afterwards. |

Post-conditions of the rehearsal run: all five hold; nothing left on disk,
nothing running.

### What the rehearsal found, measured on 0.5.17

These are the reason to run the rehearsal before spending a bucket, and each
one would otherwise have been discovered by a gate recording a false PASS.

1. **`litestream restore` can exit 0 and write a database containing none of
   the source's tables.** Restoring from a bare replica directory
   (`file://<dir>`) succeeded and produced a 16 KiB file whose tables were all
   absent. Gate 5 must compare rows, never exit status.
2. **A replica root is not namespaced per database.** Two databases sharing one
   replica path interleave their LTX segments, and a restore for one returns
   the *other* database's snapshot, silently. Every S3 config must give each
   database its own prefix (`s3://<bucket>/<db-name>`); the gateway holds N
   databases, so this is load-bearing, not tidy.
3. **A config file is read at startup only.** `litestream start` over the
   control socket answers "database not found" for a database added to the
   file, and the working route is
   `litestream register -socket <sock> -replica <url> <db-path>`. SIGHUP
   terminates the process rather than reloading it. `litestream status` and
   `litestream ltx` also require the database to be in the config, so the
   daemon log is the only signal for a runtime-added database.
4. **Unknown replica keys are silently ignored.** A config carrying
   `sse-customer-key` replicated with no error and no encryption. Decision 3
   (encryption) therefore cannot be settled by writing a key into the config
   and observing that Litestream starts; it must be settled at the bucket or by
   a mechanism this packet verifies end to end.

### What the rehearsal run FIXED before it PASSED gates 8 and 12

These are the durable engineering changes the rehearsal forced. They are
called out separately because a reader of the previous status (2026-09-28)
would reasonably ask "why did gates 8 and 12 suddenly pass?".

- **Gate 12**: the previous cleanup relied on a single S3 API, `?versions`,
  for enumerating what to delete. A service that 501s on `?versions` (the
  rehearsal store did, with the original `-backend fs`) would delete zero
  objects and then report "residue remains" -- naming the bucket for a defect
  that was actually in the packet's single-API assumption. The cleanup now
  enumerates through BOTH `?versions` and `?list-type=2` (paginated), deletes
  by key+versionId where supported and by key where it isn't, and re-measures
  residue through both. The packet records which of the two APIs each target
  supports, so an honest `BLOCKED` (e.g. "noncurrent versions cannot be
  enumerated on this service") is possible when a service supports only one.
- **Gate 8**: the previous measurement sampled a single boolean "daemon log
  raised an ERROR/WARN this phase" by grepping the cumulative log; one stale
  WARN made every later phase look like it had surfaced its own failure, so
  the count grew past the number of phases and the gate failed a run whose
  signals were real. The measurement is now a DELTA over the lines this phase
  added, with a bounded 5 s wait for the daemon to log; the healthy phase is
  a control that must produce NO new signal; the surfaces measured are
  daemon-log / sync-cli / status-cli / restore-cli; the pass criterion is
  "every faulted phase visible on at least one surface and no fault silent",
  not "the daemon log mentioned everything"; `litestream sync` is now invoked
  with `-wait` so the upload is attempted before the measurement is sampled,
  which is the only way the daemon's error can have been emitted by then.

These are the durable fixes; weakening the gates would have been simpler and
would have been wrong.

### Operation matrix litestream 0.5.17 + this packet actually issue

Measured on the rehearsal run, in counts of requests the bucket's proxy saw:

| method | query-prefix | count | purpose |
| --- | --- | --- | --- |
| `GET` | `list-type=2` | 425 | enumerate objects; paginated with `prefix` and `continuation-token` |
| `GET` | `x-id=GetObject` | 35 | download LTX; `Range: bytes=...` for resumable reads |
| `GET` | `versions` | 4 | enumerate versions (target must implement; the rehearsal store now does, the original `-backend fs` did not) |
| `GET` | `delimiter=...` | 7 | list with delimiter for prefix discovery |
| `GET` | `lifecycle` | 1 | gate 6 reads the stored lifecycle policy back |
| `HEAD` | `/<bucket>` | 6 | bucket existence probes (including gate 12's independent re-check) |
| `PUT` | `x-id=PutObject` | 48 | upload LTX; sets `x-amz-meta-litestream-timestamp` |
| `PUT` | `lifecycle` | 1 | gate 6 stores the candidate 7/30-day policy |
| `PUT` | `/<bucket>` | 2 | bucket create (idempotent; one is the rehearsal's first-create, the second is OVH-shaped) |
| `DELETE` | `versionId` | 48 | gate 12 deletes by key+versionId (noncurrent versions + delete markers) |
| `DELETE` | `/<bucket>` | 1 | gate 12 deletes the bucket |

Three non-2xx responses in 578 requests: one `HEAD 404` (gate 12's "does the
bucket still answer" probe, which is the answer the gate is for) and two
`409` (idempotent re-create of the bucket and a put-lifecycle on a bucket
that already had one). None is an operation the packet should not have made.

This is the input to a least-privilege IAM policy for the OVH test bucket.
Scoping is the OWNER's call; the rehearsal evidence says these are the only
calls a passing run makes.

### Encryption finding, ready for decision 3

Litestream 0.5.17 IGNORES `replica.sse-customer-key`. Writing it into the
replica config produced no error, the daemon started, replication proceeded,
and a HEAD on the resulting objects reported no
`x-amz-server-side-encryption` header from the service. A encryption setting
that is not visible from the service is not in effect. Decision 3 therefore
reduces to a bucket or object-storage-side choice (SSE-OMK on OVH, or
client-side encryption before the PUT) -- not to a Litestream config knob.
Gate 10 BLOCKS on the rehearsal endpoint because the TLS layer + the rehearsal
CA confounds the "no ambient credentials" test in isolation, and it must be
re-answered on OVH.

### Failure visibility finding, ready for decision 6

The gate-8 measurement identified which surfaces each faulted phase actually
surfaced on, by phase:

- revoked-credentials: **daemon-log, sync-cli**
- blocked-egress: **sync-cli** (NOT the daemon log)
- near-quota: **daemon-log**

A backup that stops for blocked egress is **only visible via the `litestream
sync` CLI**, not via the daemon log, and not via `litestream status` (which
prints past-the-fact position data). An operator who only watches the daemon
log or only watches `dg backup status` will not see this failure. Decision 6
(alerting / sandbox acceptance) must include an EXTERNAL freshness check --
a scheduled restore-and-count, or comparing the gateway's own last-backup
timestamp against now -- because litestream 0.5.17 does not provide a
proactive surface for the full failure set.

### What the OVH run still must produce that no rehearsal can

- The exact lifecycle and versioning behaviour of the OVH test region and
  class (gate 6 retention proof, gate 11 metadata round-trip).
- Decision 2's inputs: bucket region/endpoint, credential scope applied to a
  least-privilege policy derived from the matrix above, encryption mode
  (gate 10), monthly cost / 1 GB-shaped headroom (gate 9).
- Decision 6's confirmation that whatever alert path the owner chooses still
  catches the failure modes the rehearsal surfaced on the reactive surfaces
  only.
- A `bucket-name != production-bucket` check (the packet refuses to write to
  anything whose name contains `prod`/`production`/`live`); the rehearsal
  bucket was named to be obviously disposable.

Full rehearsal packet, including the per-request log the proxy captured:
`docs/sandbox-packet-2026-09-28-rehearsal.log` (header) and the most recent
run's full packet is preserved under `/tmp/reh/` for the auditor's review
(the OVH run will write a similarly full packet under `docs/`).


## 6. Sandbox checks required before adoption

The local rehearsal is a prerequisite trust-boundary check, not a substitute
for this program. Run all of these against disposable data and a dedicated
test bucket; no production databases, credentials, or subscriptions. Profile
C adoption must demonstrate a five-minute lag objective after committed writes,
seven days of frequent points plus 30/7 daily history, every-sync verification,
and a separate-target restore within the 15-minute objective before those
numbers are described as anything stronger than working objectives.

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
