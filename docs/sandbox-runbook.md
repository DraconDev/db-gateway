# replication sandbox runbook (NOT YET RUN)

Executable form of the 12 checks in `docs/backup-compatibility.md` §6. This
file is a **procedure**, not evidence: every step below is untested against
live infrastructure. Running it requires owner-provisioned sandbox inputs
(test bucket, test credentials, a 1 GiB-shaped sandbox host) plus a
separately approved live window. Use disposable data only — never production
databases, credentials, or subscriptions.

Conventions: `{{VAR}}` is a value you fill in. Never paste secrets into chat
logs or commit them; keep them in a `0600` env file sourced per command.
Record every result in a dated evidence log next to this file.

## 0. Prerequisites (owner provisions)

- `{{TEST_REGION}}`: the region string object storage provider shows for your bucket (the control
  panel's Object Storage **Endpoints** tab, and the credentials page for the key
  pair). This document does not carry a list of them on purpose: an endpoint or
  region string written from memory is exactly the kind of unverified fact this
  packet exists to catch, and the two values are two clicks away in the console
  you have to visit anyway to create the bucket.
- `{{TEST_BUCKET}}`: dedicated object storage bucket, versioning enabled per the
  retention drill's needs — nothing else lives here.
- `{{S3_ACCESS_KEY}}` / `{{S3_SECRET_KEY}}`: dedicated backup-test user keys,
  least-privilege to `{{TEST_BUCKET}}` only.
- `{{SANDBOX}}`: private transport-reachable 1 GiB-shaped Linux host (x86_64) with the
  release `dg` bundle installed per `docs/deployment.md` (test instance).
- Acceptance bar: R6 (extended run, p99 lag ≤ 5 min, 3× ≤periodic restores,
  sidecar RSS ≤ 256 MiB, full matrices, registry proven).

```sh
# On your workstation: keep secrets in one 0600 file, never in history.
umask 077 && cat > sandbox.env <<EOF
export TEST_REGION={{TEST_REGION}}
export TEST_BUCKET={{TEST_BUCKET}}
export AWS_ACCESS_KEY_ID={{S3_ACCESS_KEY}}
export AWS_SECRET_ACCESS_KEY={{S3_SECRET_KEY}}
EOF
source sandbox.env
```

## 0a. Running the packet (the executable form of §6)

Everything in this runbook that can be checked by a script is now checked by a
script, and the whole packet runs from one command. This is the step that
replaces running the sections below by hand; the sections remain as the
procedure they document, and as what to read when a gate fails.

The packet takes the bucket through `DG_SANDBOX_*` variables (it repoints
`HOME` into a throwaway tree and never reads your shell's AWS profile, so the
only credentials in its environment are the ones you pass here). With
`sandbox.env` sourced, map them and run:

```sh
umask 077
export DG_SANDBOX_BUCKET="$TEST_BUCKET"
# Endpoint and region come from YOUR object storage provider console, not from a list in this repo:
# the bucket's "Endpoints" tab in the control panel, and the credentials page for
# the key pair. The pattern is s3.<region>.io.cloud.net, and the packet
# prepends https:// -- so the variable takes the bare host, with no scheme.
export DG_SANDBOX_ENDPOINT="s3.${TEST_REGION}.io.cloud.net"
export DG_SANDBOX_REGION="$TEST_REGION"
export DG_SANDBOX_ACCESS_KEY_ID="$AWS_ACCESS_KEY_ID"
export DG_SANDBOX_SECRET_KEY="$AWS_SECRET_ACCESS_KEY"

scripts/sandbox.sh \
  --out "docs/sandbox-$(date -u +%Y-%m-%d)-object storage provider.log" \
  --i-have-owner-go-ahead
echo "exit $?   # 0 only if no gate is blocked or failed; §6's own rule"
unset DG_SANDBOX_ACCESS_KEY_ID DG_SANDBOX_SECRET_KEY
```

`--i-have-owner-go-ahead` is the owner's own authorization flag and the packet
refuses to run the bucket gates without it. It also refuses a bucket whose name
contains `prod`, `production` or `live`.

**Least privilege.** `docs/backup-compatibility.md` §6a's operation matrix is
the measured list of what a passing run issues, and the decision-2 table in
`docs/recovery-decisions.md` maps each row to an IAM action. The minimum for
replication is `s3:ListBucket`, `s3:GetObject` and `s3:PutObject` scoped to the
test bucket and, per database, to its own prefix. The packet additionally needs
`s3:CreateBucket` / `s3:DeleteBucket` / `s3:PutLifecycleConfiguration` only if
you want it to manage a disposable bucket itself.

**What a run will and will not settle.** Gates 1, 2, 5 and 7 pass anywhere and
should pass again. Gates 6 and 10 are the two that the local rehearsal could
not answer -- gate 6 because a stand-in implements no lifecycle API, gate 10
because it advertises an encryption header it does not apply -- and both need
your bucket. The remaining eight need it to be object storage provider rather than local:
endpoint and region behaviour, storage class, pricing, quota behaviour, and the
encryption header the service reports on what it stored. A §6 PASS still
requires the extended window in section 0's acceptance bar; this command produces
the evidence for the gates, not the week.

**If a gate fails**, the packet names the cause in the gate's own section
rather than in a summary, so start from the failing `## gate N:` heading in the
packet you just wrote.

## 1. Pin and record

```sh
# On {{SANDBOX}}: install a pinned sidecar release supplied by the
# operator (DG_REPLICATION_DOWNLOAD_URL / DG_REPLICATION_CHECKSUMS_URL),
# nothing floating.
curl -sSL -o replication.tar.gz \
  "$DG_REPLICATION_DOWNLOAD_URL"
tar xzf replication.tar.gz replication && sudo install -m0755 replication /usr/local/bin/
replication version   # record verbatim output + today's date
```

Record: version output, download date, config-reference fetch date. Any
upgrade re-runs this whole runbook.

## 2. WAL contention

Goal: prove replication sidecar checkpoints don't surface as gateway capacity,
deadline or internal failures, and quantify latency impact.

```sh
# Baseline first (no sidecar): periodic write loop, all-200s expected.
dg --key-file owner.key create wal_a && dg --key-file owner.key create wal_b
for db in wal_a wal_b; do
  dg --key-file owner.key query $db "CREATE TABLE w (id INTEGER PRIMARY KEY, v TEXT)" &
done; wait
START=$(date +%s); i=0
while [ $(($(date +%s) - START)) -lt 300 ]; do
  i=$((i+1))
  for db in wal_a wal_b; do
    dg --key-file owner.key query $db "INSERT INTO w (v) VALUES ('$i')" --idempotency-key "base-$db-$i" || echo "BASELINE_FAIL $db $i"
  done
done
# Then start replication (config from §3) and repeat with key prefix "repl-".
# Compare: failure counts must match (zero new busy/deadline), record p50/p99
# write latency with and without replication (time 200 sequential writes each).
```

Record: baseline vs replication failure counts, p50/p99 table, PASS/FAIL.

## 3. Explicit object storage provider endpoint config

replication sidecar does not auto-detect object storage provider: the endpoint, region, and path style
are explicit. Sketch only — conform exact keys to the the pinned sidecar release config
reference (https://replication sidecar.io/reference/config/):

```yaml
# /etc/replication sidecar-test.yml (0600; keys via env, never inline)
dbs:
  - path: /var/lib/dg-test/wal_a.sqlite
    replicas:
      - url: s3://{{TEST_BUCKET}}/wal_a?endpoint=https://s3.{{TEST_REGION}}.io.cloud.net
        region: {{TEST_REGION}}
        force-path-style: true
```

```sh
replication sidecar replicate -config /etc/replication sidecar-test.yml  # foreground first run
# Try force-path-style true/false and skip-verify variants one at a time.
```

Record: minimal working config (redacted) + each failing variant and its
error. If no variant syncs, STOP: adoption blocked.

## 4. Required-operation coverage

With replication running under a write loop, capture every S3 operation
replication sidecar issues (sidecar debug logs + bucket access logging if available),
including multipart and any versioning/lifecycle reads. Confirm each
succeeds against `{{TEST_REGION}}`.

Record: passing operation matrix (operation → observed → OK/FAIL).

## 5. Restore drill

```sh
# Latest point to a SEPARATE path, then gateway validation + exact rows.
replication sidecar restore -config /etc/replication sidecar-test.yml -o /tmp/restored.sqlite \
  s3://{{TEST_BUCKET}}/wal_a
dg --key-file owner.key import wal_a_restored --from /tmp/restored.sqlite
dg --key-file owner.key query wal_a "SELECT count(*) FROM w"
dg --key-file owner.key query wal_a_restored "SELECT count(*) FROM w"
# counts must match exactly; spot-check literal rows both sides.
# Repeat for a mid-history point (replication sidecar restore -timestamp ...).
# Negative: corrupt one byte of a copied artifact, import must fail cleanly.
```

Record: restore commands, count/row comparisons, mid-history result,
corrupt-artifact rejection. Any mismatch blocks adoption.

## 6. Retention drill

Apply the candidate lifecycle rules (extended frequent + 30/7 periodic per R1),
age/rotate test objects (upload days apart, or backdate via lifecycle rules
on prefixed test objects), and prove newest-verified-copy age plus version
counts match intent without breaking a fresh restore (§5).

Record: rule set, aged-object inventory before/after, restore-after-retention
result.

## 7. Registration / reload

Add a third database (`wal_c`) to the config and record whether replication
picks it up without restart (exact signal/command used, or restart required).
Remove it and record the removal behavior. This decides whether "add a
database without restart" may ever be promised.

## 8. Failure visibility

One at a time, with a write loop running: (a) revoke/break the S3 keys,
(b) block egress to the endpoint, (c) fill the destination toward quota.
For each, record how the failure surfaces (sidecar log line, `replication sidecar
status` output, exit code) AND confirm serving writes keep succeeding with a
visible stale-backup signal — silent stalling fails this check.

Record: per-fault surfacing evidence + serving-write outcome.

## 9. Resource headroom

On the 1 GiB-shaped host, record gateway + sidecar CPU/RSS during steady
replication and during a restore; attach a `scripts/resource-report.sh`
run and the sidecar's own usage. Compare against R6 (≤256 MiB sidecar) and
the `MemoryMax` method (R10).

## 10. Encryption

Per R3 the default is SSE-OMK: prove a full sync + restore round trip with
it, including a restore performed without ambient credentials (explicit test
keys only). If R3 is overridden to SSE-C, additionally prove the documented
key-loss handling — loss must be a policy outcome, not a surprise.

## 11. Registry coverage

Replicate `metadata.sqlite` alongside database files, restore it to a
scratch data dir, and prove scoped keys still resolve (read-only reads,
read-only write denied, unscoped denied). Restored databases without their
authorization context are not a recovery.

## 12. Cleanup

```sh
# Delete ALL sandbox residue and prove it is gone.
aws --endpoint-url https://s3.{{TEST_REGION}}.io.cloud.net \
  s3 rb s3://{{TEST_BUCKET}} --force   # plus versioned-object purge as needed
# Delete the test user/keys in the object storage provider control panel, remove configs/keys
# from {{SANDBOX}} and the workstation env file, then re-list to confirm.
```

Record: deletion commands + post-cleanup empty listings. No residue may
remain in the owner's account.

## Adoption packet

Adoption requires, attached to the deployment checklist: pinned binary +
`version` output; redacted config + credential scope; passing §4/§5/§6/§8
matrices; §9 resource report; §12 cleanup evidence; accepted R1–R6 memos.
Until that packet exists, docs keep saying **unverified**.
