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

**Ten of twelve gates PASSED in rehearsal. Two remain BLOCKED on the owner's
test bucket. Adoption is NOT claimed by this packet; it is claimed by the
OVH run, with this rehearsal as its evidence about execution and about
nothing else.**

`scripts/rehearse-sandbox-packet.sh` runs the whole packet against a local
disposable S3-compatible endpoint (gofakes3 in a SigV4-validating proxy), then
checks six post-conditions the packet cannot check about itself: no temp tree
left, no stray `dg-sandbox*` tree newer than the packet, no surviving
proxy/store process, no end-point state directory, the packet declares
`mode: REHEARSAL`, and every gate that needs a bucket actually ran. The
endpoint is named in each gate's output
(`service behind it:` line, below), so a rehearsal can never be read as OVH
evidence by accident.

| gate | state | evidence |
| --- | --- | --- |
| 1 pin and record | **PASS** | `litestream 0.5.17`, tarball sha256 matched against the release's own `checksums.txt`, installed beside (not over) the v0.3.13 on `PATH`, which must not be used for §6 |
| 2 WAL contention | rehearsal PASS | replication advanced under 60 live gateway writes with 0 busy/deadline failures; **no** p50/p99 comparison and no 1 GB-shaped sandbox yet |
| 3 explicit OVH endpoint | rehearsal PASS | explicit `endpoint`/`region` are honoured and the minimal working replica URL is `s3://<bucket>/<prefix>?endpoint=<scheme://host>&region=<r>`; the same URL FAILS without the scheme and without the rehearsal CA, so the pass is neither a transport artifact nor a disabled-verification artifact |
| 4 operation coverage | rehearsal PASS | the operation matrix below is MEASURED, not assumed, and both databases restored with exact row counts |
| 5 restore drill | rehearsal PASS | latest restore is digest-identical to the source (70/2485/70, `integrity_check` ok), and a mid-history `-txid` and a point-in-time `-timestamp` each reproduce the content **captured at that position while it was live** (30/465/30, integrity ok) -- compared against that capture, not merely against each other. All 4 corrupt-artifact cases over the bucket's own objects (5 artifacts downloaded) were REFUSED loudly (nonzero exit, no output file, `Error: no matching backup files available`) and none produced a database that was not the source. The gate BLOCKS if no artifact was downloadable, so it cannot pass by skipping the case §6 asks for. Gateway import validation of the restored file is gate 11. |
| 6 retention drill | **BLOCKED** (rehearsal endpoint) | the rehearsal store does not implement `?lifecycle`: `PutBucketLifecycleConfiguration` answers **409** and `GetBucketLifecycleConfiguration` answers **200 with a `ListBucketResult`**. The gate reports the capability as unimplemented rather than counting the children of a document that is not a lifecycle configuration. The arithmetic half still measured: 12 versions enumerated, 12 kept as-is, 0 kept if every age were 31 days older, and a restore after the retention step returns the source. **OVH's bucket must answer the storage half.** |
| 7 registration/reload | rehearsal PASS | a database added at runtime was replicated with no restart, via `litestream register` over the control socket |
| 8 failure visibility | rehearsal PASS | each of the three injected failures (revoked credentials, blocked egress, near-quota) kept the gateway serving writes, kept the faulted write out of the bucket, and surfaced on at least one operator-visible surface; the healthy control phase raised nothing false; after the fault cleared the missing writes were in the bucket again. Both injected 403s appear verbatim in the daemon log, which the gate now checks over the whole drill rather than one phase. **But the daemon log is not a complete surface**: see the findings below. |
| 9 resource headroom | **rehearsal PASS** | kernel-enforced 1 GiB scope (`MemoryMax=1G`, enforcement proved by an OOM-kill probe): replication peak 45 MiB (45 900 KiB, 4% of budget), restore peak 36 MiB (36 672 KiB), restore 2548 ms, 120 030 rows restored, no OOM kill. Headroom against the sidecar budget in `docs/resource-budget.md` is the owner's decision 2 input, not this gate's. |
| 10 encryption | **BLOCKED** (rehearsal endpoint) | litestream's half is **measured**: it signs `x-amz-server-side-encryption-customer-algorithm/-key/-key-md5` on its uploads, so SSE-C is available to the owner as a client-side lever. The service's half is proved, not read: the canary test writes with SSE-C and reads back **without** the key, and the rehearsal endpoint returned the plaintext canary while echoing `customer-algorithm=AES256` on HEAD -- an echo is a claim, and this one is false. The gate passes only on a refused keyless read, and BLOCKED here because the stand-in implements neither mode. Restore with no ambient credential measured clean (100/100 rows). See the encryption finding below. |
| 11 registry coverage | rehearsal PASS | the registry and the data file were both restored out of the bucket into an empty directory, and a scoped key minted BEFORE the loss authenticated against the recovered registry, read the exact row count, and was still refused for a database it was never scoped to. The `metadata.sqlite` round-trip and scoped-key resolution both hold; only the OVH service's own behaviour is unanswered. |
| 12 cleanup | rehearsal PASS | every test object and version deleted through both `?versions` and `?list-type=2`, the bucket no longer answers. **After a durable fix**: the rehearsal store's original `-backend fs` left empty directories on the disk for keys that contained a `/`, which made `DeleteBucket` answer `BucketNotEmpty` with zero objects listed -- not S3's emptiness rule. The stand-in now runs `-backend memory`, which has no directories to confuse the answer, and the bucket is genuinely gone afterwards. |

Post-conditions of the rehearsal run: all six hold; nothing left on disk,
nothing running. The packet for this run is
`docs/sandbox-packet-2026-09-29-rehearsal.log` (945 endpoint requests folded
in at the end of the full packet, of which the per-record JSONL is elided from
the committed copy for size; `scripts/rehearse-sandbox-packet.sh` regenerates
it).

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
4. **Starting cleanly is not evidence that a config key did anything.** A
   first pass wrote `sse-customer-key` into the replica config, watched the
   daemon start without an error, and recorded that litestream 0.5.17
   "replicated in the clear" -- which the run's own request log then refuted:
   all three uploads carried `x-amz-server-side-encryption-customer-algorithm`,
   `-key` and `-key-md5` inside the signature. litestream SENDS SSE-C; the
   rehearsal store simply neither enforces nor echoes it. Both halves have to
   be observed separately -- what the client put on the wire, and what the
   service reports about what it stored -- because only the second one says
   whether the bytes at rest are encrypted. Decision 3 must be settled at the
   bucket, and gate 10 now checks both halves rather than either.
5. **The first row of `litestream ltx` is the base snapshot, and restoring it
   is a correct restore of an EMPTY database.** Its `max_txid` is the very
   start of history, so `-txid` against it succeeds with exit 0 and 0 rows.
   The packet read that row as the "mid-history point" and reported a broken
   drill for a replica where `-txid 0000000000000002` restored the full first
   batch. A mid-history drill that takes its selector from the newest artifact
   is not a mid-history drill; it is a restore of the beginning, wearing a
   mid-history label. The packet now takes the second segment and requires the
   `-txid` and `-timestamp` restores of that one position to agree byte for
   byte, since they are the same point named two ways.
6. **A `200` from `GetBucketLifecycleConfiguration` is not a lifecycle
   configuration.** The rehearsal store answers `GET ?lifecycle` with a plain
   object listing, so a parser that walked the response's children reported
   "30 rules" -- while the `PUT` that was supposed to have stored the policy
   had failed with **409**. The count was a bucket listing, and the packet
   published it as a stored, read-back policy. Rules are now compared to the
   rules sent (id, status, and both day counts), and a document whose root
   element is not `<LifecycleConfiguration>` is reported as the capability
   being unimplemented, never as zero rules.
7. **A restore is all-or-nothing: damage to a REDUNDANT artifact still fails
   the restore.** With every artifact but the newest byte-flipped, the newest
   artifact was intact and sufficient on its own, and `litestream restore`
   still refused -- nonzero exit, no output file, `Error: no matching backup
   files available`. So corruption anywhere in a replica takes a restore with
   it, and a partial replica is not a partial restore. The practical
   consequence: keep a second copy rather than trusting redundancy inside one,
   and never read a restore's exit status as proof of a good copy -- gate 5
   compares `(rows, sum, distinct)` of the restored file, because a 200 and a
   file on disk is exactly what a silently-wrong restore looks like.

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

Measured on the 2026-09-29 rehearsal run, in counts of requests the bucket's
proxy saw (945 requests in total; the per-record log is folded into the full
packet and elided from the committed copy). **Every number in this table is
recomputable from the committed packet** with `python3 scripts/packet-facts.py
docs/sandbox-packet-2026-09-29-rehearsal.log`, which is also what the reviewer's
recheck used. The rows sum to the total, which is checked rather than hoped:

| method | query-prefix | count | purpose | issued by |
| --- | --- | --- | --- | --- |
| `GET` | `list-type=2` | 270 | enumerate objects; paginated with `prefix` and `continuation-token` | litestream |
| `GET` | `list-type=2` | 450 | the same enumeration, for the packet's own version/retention/residue work | packet |
| `GET` | `x-id=GetObject` | 22 | download LTX; `Range: bytes=...` for resumable reads | litestream |
| `GET` | `x-id=GetObject` | 29 | gate 5's artifact downloads for the corruption cases | packet |
| `GET` | object key, no `x-id` | 6 | gate 5 re-reads what it downloaded; gate 10's canary read-back | packet |
| `GET` | `versions` | 4 | enumerate versions for the retention arithmetic and the residue check | packet |
| `GET` | `lifecycle` | 1 | gate 6 reads the stored lifecycle policy back | packet |
| `HEAD` | object key | 10 | litestream's own existence checks during replication | litestream |
| `HEAD` | object key | 4 | gate 10's SSE check and gate 5's artifact inspection | packet |
| `HEAD` | `/<bucket>` | 1 | gate 12's "does the bucket still answer" probe -- the 404 that IS this gate's answer | packet |
| `PUT` | `x-id=PutObject` | 71 | upload LTX; sets `x-amz-meta-litestream-timestamp`. One of them signs the three `x-amz-server-side-encryption-customer-*` headers | litestream |
| `PUT` | object key, no `x-id` | 1 | the SSE canary | packet |
| `PUT` | `/<bucket>` | 2 | bucket create (idempotent: one first-create, one OVH-shaped re-create) | packet |
| `PUT` | `lifecycle` | 1 | gate 6 stores the candidate 7/30-day policy | packet |
| `DELETE` | `versionId` | 72 | gate 12 deletes by key+versionId (noncurrent versions + delete markers) | packet |
| `DELETE` | `/<bucket>` | 1 | gate 12 deletes the bucket | packet |

Three non-2xx responses in 945 requests, and all three are named: one `HEAD 404`
(gate 12's "does the bucket still answer" probe, which is the answer the gate
is for), one `409` from the idempotent re-create, and one `409` from
`put-lifecycle` on the store that does not implement it. None is an operation
the packet should not have made.

**The litestream-only half is what a credential policy has to allow**: `GET`
and `PUT` object, plus `GET` bucket, scoped to the per-database prefix. The
per-request attribution in the last column is MEASURED rather than assumed:
litestream stamps every upload with `x-amz-meta-litestream-timestamp` and signs
with the Go SDK, which leaves `amz-sdk-*` markers in the signed header set --
neither of which `s3probe.py` produces, so the two clients are separable in the
log. Everything in the `packet` rows is the harness driving the bucket, and
`CreateBucket` / `DeleteBucket` / `PutLifecycleConfiguration` are optional: they
exist so the packet can manage a disposable test bucket, and none is needed on a
steady-state retention run. litestream never issues a delete at all.

This is the input to a least-privilege IAM policy for the OVH test bucket.
Scoping is the OWNER's call; the rehearsal evidence says these are the only
calls a passing run makes.

### Encryption finding, ready for decision 3

**What litestream does, measured on the wire.** With
`replica.sse-customer-key` set, litestream 0.5.17 starts without an error and
**signs SSE-C on every upload**: the packet's validating proxy recorded
`x-amz-server-side-encryption-customer-algorithm`, `-key` and `-key-md5` in
`SignedHeaders` of every PUT under that prefix. This corrects an earlier
finding in this document, which claimed 0.5.17 ignores the key and replicates
in the clear; that claim was inferred from a clean start and the request log
refuted it. The client-side lever exists -- the owner is not forced into
SSE-OMK because SSE-C is unavailable.

**What the service does, which is the part that decides the question -- and a
header is not an answer.** A HEAD on the stored objects reported no
`x-amz-server-side-encryption`, and the SSE-C `customer-algorithm` echo came
back as `AES256`. Taken at face value that would be the answer, and it is worth
being precise about why it is not: **a header is the service's claim about
itself.** The rehearsal store was then checked directly, and it echoes the
customer-algorithm while storing the object body in plaintext -- a canary
written with SSE-C was read back with no key at all and came back intact. The
echo proves the service recorded the metadata, and nothing about the bytes.

Gate 10 therefore proves it instead of reading it. `s3probe.py sse-canary`
writes a canary with SSE-C, reads it back **without** the customer key, then
reads it back with it. A keyless read refused for an encryption reason is
ENFORCED, and that is a proof the stored bytes are ciphertext; the canary
coming back in the clear is NOT-ENCRYPTED; anything else is INDETERMINATE. The
gate passes only on ENFORCED, and reports a claim without proof as a claim. The
measurement is as much about the packet as about the store: a harness that
trusted the header would have passed a stand-in that stores plaintext, and the
same code would have passed a real bucket carrying the same defect.

Note the limit, which the gate states rather than hides: this settles SSE-C and
says nothing about SSE-S3/SSE-OMK, where the service decrypts transparently and
a round-trip succeeds whatever is stored at rest. For those modes the header is
all a client can see, and OVH's own documentation of default encryption is what
the owner relies on.

The unresolved question, which no rehearsal can answer:

> on the OVH bucket, does the same canary read back REFUSED without the customer
> key under SSE-C, and does a restore then still work with the key present and
> fail cleanly without it?

The key-loss half is the operational consequence of choosing SSE-C, and it
cannot be exercised until the service actually applies SSE-C. That is a reason
to decide the mode deliberately, not a reason to avoid SSE-C.

**The other half of gate 10 has been measured and is clean.** With every
ambient credential removed (empty `AWS_*` config files, the real ones
unreadable, `AWS_EC2_METADATA_DISABLED`, and a verification that no profile is
discoverable), a restore from the bucket returned all 100 rows. So a bucket
that enforces encryption at rest does not depend on the client holding
credentials, and an operator can prove the property from a cold start. What
BLOCKS the gate is narrower than the 2026-09-28 note said: the rehearsal
endpoint reports no server-side encryption on any object, so "is the stored
data encrypted" has no answer there. The TLS layer and the rehearsal CA are no
longer the reason -- the credential-isolation test now runs and passes
against them. Re-answered on OVH, with the same command unchanged.

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
`docs/sandbox-packet-2026-09-28-rehearsal.log` (the earlier header) and
`docs/sandbox-packet-2026-09-29-rehearsal.log` (the current run, 12 gates,
10 pass / 2 blocked / 0 fail, all six post-conditions holding). The OVH run
will write a similarly full packet under `docs/`.


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
