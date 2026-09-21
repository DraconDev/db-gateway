# OVH/Litestream sandbox runbook (NOT YET RUN)

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

- `{{TEST_REGION}}`: OVH region for the sandbox bucket (see R3).
- `{{TEST_BUCKET}}`: dedicated sandbox bucket, versioning enabled per the
  retention drill's needs — nothing else lives here.
- `{{S3_ACCESS_KEY}}` / `{{S3_SECRET_KEY}}`: dedicated backup-test user keys,
  least-privilege to `{{TEST_BUCKET}}` only.
- `{{SANDBOX}}`: SSH-reachable 1 GiB-shaped Linux host (x86_64) with the
  release `dg` bundle installed per `docs/deployment.md` (test instance).
- Acceptance bar: R6 (7-day run, p99 lag ≤ 5 min, 3× ≤15 min restores,
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

## 1. Pin and record

```sh
# On {{SANDBOX}}: install exactly v0.5.17, nothing floating.
curl -sSL -o litestream.tar.gz \
  https://github.com/benbjohnson/litestream/releases/download/v0.5.17/litestream-v0.5.17-linux-amd64.tar.gz
tar xzf litestream.tar.gz litestream && sudo install -m0755 litestream /usr/local/bin/
litestream version   # record verbatim output + today's date
```

Record: version output, download date, config-reference fetch date. Any
upgrade re-runs this whole runbook.

## 2. WAL contention

Goal: prove Litestream checkpoints don't surface as gateway `busy`/deadline
failures, and quantify latency impact.

```sh
# Baseline first (no sidecar): 5-minute write loop, all-200s expected.
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

## 3. Explicit OVH endpoint config

Litestream does not auto-detect OVH: the endpoint, region, and path style
are explicit. Sketch only — conform exact keys to the v0.5.17 config
reference (https://litestream.io/reference/config/):

```yaml
# /etc/litestream-test.yml (0600; keys via env, never inline)
dbs:
  - path: /var/lib/dg-test/wal_a.sqlite
    replicas:
      - url: s3://{{TEST_BUCKET}}/wal_a?endpoint=https://s3.{{TEST_REGION}}.io.cloud.ovh.net
        region: {{TEST_REGION}}
        force-path-style: true
```

```sh
litestream replicate -config /etc/litestream-test.yml  # foreground first run
# Try force-path-style true/false and skip-verify variants one at a time.
```

Record: minimal working config (redacted) + each failing variant and its
error. If no variant syncs, STOP: adoption blocked.

## 4. Required-operation coverage

With replication running under a write loop, capture every S3 operation
Litestream issues (sidecar debug logs + bucket access logging if available),
including multipart and any versioning/lifecycle reads. Confirm each
succeeds against `{{TEST_REGION}}`.

Record: passing operation matrix (operation → observed → OK/FAIL).

## 5. Restore drill

```sh
# Latest point to a SEPARATE path, then gateway validation + exact rows.
litestream restore -config /etc/litestream-test.yml -o /tmp/restored.sqlite \
  s3://{{TEST_BUCKET}}/wal_a
dg --key-file owner.key import wal_a_restored --from /tmp/restored.sqlite
dg --key-file owner.key query wal_a "SELECT count(*) FROM w"
dg --key-file owner.key query wal_a_restored "SELECT count(*) FROM w"
# counts must match exactly; spot-check literal rows both sides.
# Repeat for a mid-history point (litestream restore -timestamp ...).
# Negative: corrupt one byte of a copied artifact, import must fail cleanly.
```

Record: restore commands, count/row comparisons, mid-history result,
corrupt-artifact rejection. Any mismatch blocks adoption.

## 6. Retention drill

Apply the candidate lifecycle rules (7-day frequent + 30/7 daily per R1),
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
For each, record how the failure surfaces (sidecar log line, `litestream
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
aws --endpoint-url https://s3.{{TEST_REGION}}.io.cloud.ovh.net \
  s3 rb s3://{{TEST_BUCKET}} --force   # plus versioned-object purge as needed
# Delete the test user/keys in the OVH control panel, remove configs/keys
# from {{SANDBOX}} and the workstation env file, then re-list to confirm.
```

Record: deletion commands + post-cleanup empty listings. No residue may
remain in the owner's account.

## Adoption packet

Adoption requires, attached to the deployment checklist: pinned binary +
`version` output; redacted config + credential scope; passing §4/§5/§6/§8
matrices; §9 resource report; §12 cleanup evidence; accepted R1–R6 memos.
Until that packet exists, docs keep saying **unverified**.
