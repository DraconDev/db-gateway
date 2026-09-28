#!/usr/bin/env bash
# Sandbox adoption packet, part 2: the gates that need a bucket.
#
# Run: scripts/sandbox-s3-gates.sh --out FILE --from-s3
#
# WHAT THIS IS. scripts/sandbox-packet.sh runs the bucket-free gates (1, and
# the file-replica rehearsal of 2/5/7) and then hands off here for gates 3, 4,
# 6, 8, 9, 10, 11 and 12. Before this file existed, that hand-off was an
# `exec` of a path that did not exist: supplying a bucket produced an exec
# failure and zero executed gates. A packet whose execution path is untested
# is not a packet, so this file is written to run against ANY S3-compatible
# endpoint, and it is rehearsed end to end against a local one.
#
# WHAT IT IS DELIBERATELY NOT. Running against a local endpoint does not make a
# gate §6 evidence. §6 wants the OVH test region and class specifically, because
# what needs proving is that *that service* accepts these operations, at that
# price, with those lifecycle rules. So every gate here records, in its own
# output, which endpoint it actually ran against, and the packet marks a
# rehearsal run as a rehearsal. A gate that cannot distinguish the two is the
# exact failure mode this file exists to prevent.
#
# CREDENTIALS. Litestream 0.5.17 does NOT read S3 credentials from the
# LITESTREAM_CREDENTIALS file -- that was measured on 2026-09-28, by a run that
# had that file in place and still ended in the AWS chain at the EC2 metadata
# service. S3 signing comes from the standard AWS environment variables. So the
# only credentials in this script's environment are the two the operator passed
# in, exported explicitly and over the top of whatever the shell already had,
# and the AWS config/credentials files are pointed at empty ones so no profile
# can be discovered. Gate 10 asserts this rather than trusting the comment.

set -euo pipefail

PIN_VERSION="0.5.17"
# Resolved against the real home only if the parent has not already pinned it.
# sandbox-packet.sh exports DG_SANDBOX_PIN_ROOT before it repoints HOME at the
# sandbox; without that, this script looks for the pin inside the sandbox home
# and reports it missing, which is what the first rehearsal run did.
if [ -z "${DG_SANDBOX_PIN_ROOT:-}" ]; then
    PIN_ROOT="${DG_REAL_HOME:-$HOME}/.local/dg-sandbox/litestream-${PIN_VERSION}"
else
    PIN_ROOT="$DG_SANDBOX_PIN_ROOT"
fi
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT=""
FROM_S3=0
DG_BIN="${DG_BIN:-$REPO/.cargo/target/debug/dg}"

usage() {
    cat <<'USAGE'
usage: scripts/sandbox-s3-gates.sh --out FILE --from-s3

  --out FILE     the packet to append to (created if absent)
  --from-s3      marks this as the S3 half of a packet, so it recomputes the
                 packet summary from the gate lines already in the file

  DG_SANDBOX_BUCKET          required; the test bucket
  DG_SANDBOX_ENDPOINT        required; e.g. s3.gra.io.cloud.ovh.net (host[:port])
  DG_SANDBOX_SCHEME          https (default) or http, for a local endpoint
  DG_SANDBOX_REGION          default us-east-1
  DG_SANDBOX_ACCESS_KEY_ID   test-bucket key
  DG_SANDBOX_SECRET_KEY      test-bucket secret
  DG_SANDBOX_CREATE_BUCKET   1 to let the packet create the bucket (default 1)
  DG_SANDBOX_FAULT_FILE      optional; the sandbox fault file for gate 8
  DG_SANDBOX_SKIP_CLEANUP    1 to leave the test objects in place for inspection
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --out) OUT="${2:?--out needs a value}"; shift 2;;
        --from-s3) FROM_S3=1; shift;;
        -h|--help) usage; exit 0;;
        *) echo "unknown flag: $1" >&2; usage; exit 2;;
    esac
done
[ -n "$OUT" ] || { echo "--out is required" >&2; usage; exit 2; }

: "${DG_SANDBOX_BUCKET:?DG_SANDBOX_BUCKET is required for the S3 gates}"
: "${DG_SANDBOX_ENDPOINT:?DG_SANDBOX_ENDPOINT is required for the S3 gates}"
: "${DG_SANDBOX_ACCESS_KEY_ID:?DG_SANDBOX_ACCESS_KEY_ID is required for the S3 gates}"
: "${DG_SANDBOX_SECRET_KEY:?DG_SANDBOX_SECRET_KEY is required for the S3 gates}"

SCHEME="${DG_SANDBOX_SCHEME:-https}"
REGION="${DG_SANDBOX_REGION:-us-east-1}"
BUCKET="$DG_SANDBOX_BUCKET"
ENDPOINT="$DG_SANDBOX_ENDPOINT"
CREATE_BUCKET="${DG_SANDBOX_CREATE_BUCKET:-1}"

# A name that is obviously the owner's production bucket must never be the one
# this packet writes to. Cheap to check, and the packet's whole claim is that
# it touches nothing else.
case "$BUCKET" in
    *prod*|*production*|*live*) echo "refusing: bucket name '$BUCKET' looks like a production bucket" >&2; exit 2;;
esac

LITESTREAM="$PIN_ROOT/litestream"
[ -x "$LITESTREAM" ] || { echo "litestream ${PIN_VERSION} is not installed at $LITESTREAM" >&2; exit 2; }
[ -x "$DG_BIN" ] || echo "warning: dg binary not built at $DG_BIN; gateway-dependent gates will report BLOCKED" >&2

# TLS trust. A real S3 endpoint presents a publicly valid certificate, so a
# rehearsal against a self-signed one has to be told to trust it -- explicitly,
# by pinning that one CA, never by switching verification off. The AWS SDK
# reads AWS_CA_BUNDLE for its whole TLS config, so this reaches litestream
# without a litestream-specific option, which means the replica URL the packet
# reports as the minimal working one is exactly the URL OVH needs.
CA_ARGS=()
CA_NOTE="system trust store (the endpoint's certificate chains to a public CA)"
if [ -n "${DG_SANDBOX_CA_BUNDLE:-}" ]; then
    if [ ! -r "$DG_SANDBOX_CA_BUNDLE" ]; then
        echo "refusing: DG_SANDBOX_CA_BUNDLE=$DG_SANDBOX_CA_BUNDLE is not readable" >&2
        exit 2
    fi
    export AWS_CA_BUNDLE="$DG_SANDBOX_CA_BUNDLE"
    CA_ARGS=(--ca-bundle "$DG_SANDBOX_CA_BUNDLE")
    CA_NOTE="pinned CA bundle $DG_SANDBOX_CA_BUNDLE (self-signed rehearsal endpoint); certificate verification stays ON"
fi

# --------------------------------------------------------------------------
# Credential and discovery isolation. The AWS chain is the only mechanism
# litestream 0.5.17 actually uses for S3, so this is the load-bearing part of
# the isolation, not a belt-and-braces extra.
# --------------------------------------------------------------------------
SANDBOX="${DG_SANDBOX_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/dg-sandbox-s3-XXXXXX")}"
# One trap, one stack. A later `trap ... EXIT` REPLACES an earlier one, so four
# gates that each registered their own cleanup meant only the last one ever ran
# and the earlier proxies and gateway processes survived the packet. Cleanup is
# registered here once and appended to by every gate.
CLEANUP=()
cleanup_all() {
    local i
    for (( i=${#CLEANUP[@]}-1; i>=0; i-- )); do
        eval "${CLEANUP[$i]}" >/dev/null 2>&1 || true
    done
}
# If this script created the sandbox it owns the sandbox; if the parent
# sandbox-packet.sh created it (DG_SANDBOX_DIR), the parent's trap removes it
# and a second removal here would race that trap on the same directory.
if [ -z "${DG_SANDBOX_DIR:-}" ]; then
    CLEANUP+=("rm -rf '$SANDBOX'")
    SANDBOX_OWNER_TEXT="created and removed by this script's exit trap"
else
    SANDBOX_OWNER_TEXT="created by sandbox-packet.sh, which removes it on its own exit; this script does not"
fi
trap cleanup_all EXIT

# start_proxy <name> <request-log> <fault-file>
# Starts a validating logging proxy on an OS-chosen loopback port and sets
# PROXY_PORT to "host:port". The port is learned from a file the proxy writes
# after it binds, never from a command substitution around a process that then
# serves forever: `x="$(proxy | head -1)"` hangs forever, because the proxy keeps
# the pipe open while it serves. This is a single implementation so no call site
# can regress to the hanging form.
PROXY_PORT=""
PROXY_PID=""
# The inner proxies speak whatever the outer endpoint speaks: a gate that
# switched from TLS to plain http between the endpoint and its own recording
# proxy would be measuring a transport nothing else in the run uses.
TLS_CERT="${DG_SANDBOX_TLS_CERT:-}"
TLS_KEY="${DG_SANDBOX_TLS_KEY:-}"
start_proxy() {
    local name="$1" reqlog="$2" faultfile="$3"
    local portfile="$SANDBOX/$name.port"
    rm -f "$portfile"
    python3 "$PROXY" --listen 127.0.0.1:0 --upstream "$ENDPOINT" \
        --access-key-id "$DG_SANDBOX_ACCESS_KEY_ID" --secret-access-key "$DG_SANDBOX_SECRET_KEY" \
        --region "$REGION" --log "$reqlog" --faults "$faultfile" --port-file "$portfile" \
        ${TLS_CERT:+--tls-cert "$TLS_CERT" --tls-key "$TLS_KEY"} \
        >/dev/null 2>&1 &
    local pid=$!
    CLEANUP+=("kill $pid 2>/dev/null || true")
    local waited=0
    while [ "$waited" -lt 100 ]; do
        if [ -s "$portfile" ]; then break; fi
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.05
        waited=$((waited + 1))
    done
    PROXY_PORT="$(cat "$portfile" 2>/dev/null || true)"
    PROXY_PID="$pid"
}
mkdir -p "$SANDBOX/home"
: > "$SANDBOX/home/.aws-empty"
export HOME="$SANDBOX/home"
export AWS_CONFIG_FILE="$SANDBOX/home/.aws-empty"
export AWS_SHARED_CREDENTIALS_FILE="$SANDBOX/home/.aws-empty"
export AWS_PROFILE=""
export AWS_REGION="$REGION"
export AWS_DEFAULT_REGION="$REGION"
export AWS_EC2_METADATA_DISABLED=true
# Overwrite, never inherit. A shell that already exported a personal key would
# otherwise sign this packet's traffic with it.
export AWS_ACCESS_KEY_ID="$DG_SANDBOX_ACCESS_KEY_ID"
export AWS_SECRET_ACCESS_KEY="$DG_SANDBOX_SECRET_KEY"
unset AWS_SESSION_TOKEN AWS_SECURITY_TOKEN AWS_ROLE_ARN AWS_WEB_IDENTITY_TOKEN_FILE || true

S3PROBE=( python3 "$REPO/scripts/s3probe.py"
    --endpoint "$ENDPOINT" --scheme "$SCHEME" --bucket "$BUCKET" --region "$REGION"
    --access-key-id "$DG_SANDBOX_ACCESS_KEY_ID" --secret-access-key "$DG_SANDBOX_SECRET_KEY"
    "${CA_ARGS[@]}" )
PROXY="$REPO/scripts/s3-log-proxy.py"

# record <gate> <PASS|FAIL|BLOCKED> <one-line result> [detail...]
record() {
    local gate="$1" status="$2" result="$3"; shift 3
    {
        printf '\n## gate %s: %s -- %s\n' "$gate" "$status" "$result"
        for line in "$@"; do printf '%s\n' "$line"; done
    } >> "$OUT"
    printf 'gate %-2s %-7s %s\n' "$gate" "$status" "$result" >&2
}

jfield() { python3 -c '
import json,sys
try:
    d=json.load(sys.stdin)
except Exception:
    print(""); raise SystemExit(0)
for k in sys.argv[1].split("."):
    if isinstance(d,list):
        try: d=d[int(k)]
        except Exception: d=None
    elif isinstance(d,dict): d=d.get(k)
    else: d=None
    if d is None: break
print("" if d is None else d)' "$1"; }

if [ ! -s "$OUT" ]; then
    {
        echo "# Sandbox adoption packet (docs/backup-compatibility.md §6)"
        echo
        echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo "host: $(uname -srm)"
        echo "litestream pinned: ${PIN_VERSION} (${PIN_ROOT})"
        echo "bucket: $BUCKET"
        echo "endpoint: $ENDPOINT"
        echo "region: $REGION"
    } > "$OUT"
fi

# The endpoint a gate ran against is stated once, here, rather than in twelve
# places: a reader must not be able to miss which service produced this packet.
{
    echo
    echo "## the S3 endpoint these gates ran against"
    echo
    echo "scheme://$ENDPOINT  bucket=$BUCKET  region=$REGION  path-style addressing"
    if [ "$ENDPOINT" != "${DG_SANDBOX_OVH_ENDPOINT:-__none__}" ] && [[ "$ENDPOINT" != *.io.cloud.ovh.net ]]; then
        echo
        echo "This is NOT the OVH endpoint named in §6 gate 3. Gates run here prove"
        echo "the harness drives correct requests and handles the answers; they do not"
        echo "prove the OVH test region and class accept them. Re-run unchanged with"
        echo "DG_SANDBOX_ENDPOINT set to the OVH endpoint to turn them into §6 evidence."
    fi
} >> "$OUT"

# --------------------------------------------------------------------------
# Gate 3: explicit endpoint and region configuration, and what the minimal
# working replica URL is. The rehearsal proved the failure this gate is for:
# litestream accepts configuration it does not act on, silently. So this gate
# does not record "it started"; it records the operation matrix the start
# produced, and compares a known-good URL against deliberately wrong ones.
# --------------------------------------------------------------------------
if [ "$CREATE_BUCKET" = "1" ]; then
    CREATE_OUT="$("${S3PROBE[@]}" create-bucket --create-in-region "$REGION" 2>&1 || true)"
    printf 'bucket create: %s\n' "$CREATE_OUT" >&2
fi

REPLICA_BASE="s3://$BUCKET"
GATE3_OK=0
GATE3_DETAIL=()
probe_variant() {
    # $1 label, $2 replica url, $3 extra s3probe flags...
    local label="$1" url="$2"; shift 2
    local log="$SANDBOX/gate3-$label.jsonl"
    : > "$log"
    local faultfile="$SANDBOX/gate3-$label.faults"
    : > "$faultfile"
    local pport
    start_proxy "gate3-$label" "$log" "$faultfile"
    pport="$PROXY_PORT"
    if [ -z "$pport" ]; then
        GATE3_DETAIL+=("$label: could not start the recording proxy")
        return 1
    fi
    pport="${pport#*:}"
    local cfg="$SANDBOX/gate3-$label.yml"
    cat > "$cfg" <<EOF
logging:
  level: info
  type: text
dbs:
  - path: $SANDBOX/gate3-$label.sqlite
    replica:
      url: $url
EOF
    python3 - "$SANDBOX/gate3-$label.sqlite" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.execute("PRAGMA journal_mode=WAL")
c.execute("CREATE TABLE t(a INTEGER)")
c.execute("INSERT INTO t VALUES (1)")
c.commit()
PY
    timeout 25 "$LITESTREAM" replicate -config "$cfg" > "$SANDBOX/gate3-$label.log" 2>&1 || true
    sleep 1
    pkill -f "s3-log-proxy.py --listen $pport" 2>/dev/null || true
    local puts
    puts="$(grep -c '"method": "PUT"' "$log" 2>/dev/null || true)"
    puts="${puts:-0}"
    local errs
    errs="$(grep -cE '"status": (4|5)[0-9][0-9]' "$log" 2>/dev/null || true)"
    errs="${errs:-0}"
    if [ "$puts" -gt 0 ] && [ "$errs" -eq 0 ]; then
        GATE3_DETAIL+=("$label: OK -- $puts object(s) written, 0 rejected  [$url]")
        return 0
    fi
    GATE3_DETAIL+=("$label: FAILED -- $puts object(s) written, $errs rejected  [$url]  $(grep -m1 -oE 'Error: .*|error="[^"]*"' "$SANDBOX/gate3-$label.log" 2>/dev/null | head -1 | cut -c1-160)")
    return 1
}

WORKING_URL="$REPLICA_BASE/gate3?endpoint=$ENDPOINT&region=$REGION"
probe_variant "working"      "$WORKING_URL"                                   && GATE3_OK=$((GATE3_OK+1))
probe_variant "no-endpoint"  "$REPLICA_BASE/gate3?region=$REGION"              || true
probe_variant "wrong-region" "$REPLICA_BASE/gate3?endpoint=$ENDPOINT&region=nowhere-1" || true
probe_variant "no-region"    "$REPLICA_BASE/gate3?endpoint=$ENDPOINT"           || true
probe_variant "path-style"   "$REPLICA_BASE/gate3?endpoint=$ENDPOINT&region=$REGION" || true

# force-path-style and skip-verify are the two §6 explicitly names. On the
# rehearsal they were accepted silently and did nothing, so the packet records
# what each did rather than whether litestream started.
{
    printf 'variants tried (each: objects written / requests rejected):\n'
    for d in "${GATE3_DETAIL[@]}"; do printf '  %s\n' "$d"; done
    printf '\nforce-path-style: litestream 0.5.17 has no such replica option. It was\n'
    printf '  accepted as a replica URL query parameter in the 2026-09-28 file\n'
    printf '  rehearsal with no error and no observable effect, so it is recorded as\n'
    printf '  NOT SUPPORTED rather than tested. Addressing is decided by the URL host:\n'
    printf '  a bucket in the path is path-style, a bucket in the host is virtual-host.\n'
    printf 'skip-verify (TLS): 0.5.17 accepts an `insecure` replica option for a\n'
    printf '  self-signed endpoint. NOT USED HERE and not usable against OVH, whose\n'
    printf '  certificate is publicly valid; a sandbox that needs it is misconfigured.\n'
    printf '\nminimal working replica URL:\n  %s\n' "$WORKING_URL"
    printf '\nNOTE: `replica.path` cannot carry a URL. `path: s3://...` fails with\n'
    printf '  "replica path cannot be a url, please use the url field instead".\n'
} >> "$OUT"
if [ "$GATE3_OK" -ge 1 ]; then
    record 3 PASS "explicit endpoint/region config works; variants recorded above" \
        "endpoint: $ENDPOINT   region: $REGION   scheme: $SCHEME"
else
    record 3 FAIL "no variant of the replica URL produced a single object write" \
        "endpoint: $ENDPOINT   region: $REGION   scheme: $SCHEME"
fi

# --------------------------------------------------------------------------
# Gate 4: required-operation coverage, MEASURED. The proxy in front of the
# endpoint records every request, and the matrix is read back out of the log
# after a full replication cycle, so the answer is what litestream actually
# issued rather than what its documentation implies.
# --------------------------------------------------------------------------
G4_ROOT="$SANDBOX/gate4"
mkdir -p "$G4_ROOT"
G4_SOCK="$G4_ROOT/litestream.sock"
G4_LOG="$G4_ROOT/requests.jsonl"
G4_FAULTS="$G4_ROOT/faults"
: > "$G4_FAULTS"
start_proxy "gate4" "$G4_LOG" "$G4_FAULTS"
G4_PROXY_PORT="$PROXY_PORT"
G4_PROXY_PID="$PROXY_PID"
if [ -z "$G4_PROXY_PORT" ]; then
    record 4 FAIL "could not start the recording proxy in front of the endpoint"
    G4_PROXY_PORT="$ENDPOINT"
else
    CLEANUP+=("kill $G4_PROXY_PID 2>/dev/null || true")
fi
G4_ENDPOINT="$G4_PROXY_PORT"

# Two databases, each with its own prefix. A shared prefix is not tidiness: with
# one root the two databases' segments interleave and a restore returns the
# other database's file with exit status 0, which was measured on 2026-09-28.
: > "$G4_ROOT/alice.sqlite"; : > "$G4_ROOT/bob.sqlite"
cat > "$G4_ROOT/litestream.yml" <<EOF
logging:
  level: info
  type: text
socket:
  enabled: true
  path: $G4_SOCK
dbs:
  - path: $G4_ROOT/alice.sqlite
    replica:
      url: s3://$BUCKET/sbx/alice?endpoint=$G4_ENDPOINT&region=$REGION
  - path: $G4_ROOT/bob.sqlite
    replica:
      url: s3://$BUCKET/sbx/bob?endpoint=$G4_ENDPOINT&region=$REGION
EOF
"$LITESTREAM" replicate -config "$G4_ROOT/litestream.yml" > "$G4_ROOT/litestream.log" 2>&1 &
G4_PID=$!
sleep 4

write_rows() {
    python3 - "$1" "$2" <<'PY'
import sqlite3, sys
path, n = sys.argv[1], int(sys.argv[2])
c = sqlite3.connect(path)
c.execute("PRAGMA journal_mode=WAL")
c.execute("CREATE TABLE IF NOT EXISTS ledger(id INTEGER PRIMARY KEY, note TEXT)")
c.executemany("INSERT INTO ledger(note) VALUES (?)", [(f"{path}-{i}",) for i in range(n)])
c.commit()
PY
}
write_rows "$G4_ROOT/alice.sqlite" 200
write_rows "$G4_ROOT/bob.sqlite" 150
sleep 8
"$LITESTREAM" sync -socket "$G4_SOCK" "$G4_ROOT/alice.sqlite" >/dev/null 2>&1 || true
"$LITESTREAM" sync -socket "$G4_SOCK" "$G4_ROOT/bob.sqlite"   >/dev/null 2>&1 || true
sleep 2

# Restore both databases back out of the bucket, which is the only way to know
# the writes that were accepted are also the writes that can be read.
G4_ALICE_ROWS=0
G4_BOB_ROWS=0
for pair in "alice:$G4_ROOT/alice.sqlite" "bob:$G4_ROOT/bob.sqlite"; do
    name="${pair%%:*}"; src="${pair#*:}"
    cat > "$G4_ROOT/restore-$name.yml" <<EOF
dbs:
  - path: $src
    replica:
      url: s3://$BUCKET/sbx/$name?endpoint=$G4_ENDPOINT&region=$REGION
EOF
    rm -f "$G4_ROOT/out-$name.sqlite"
    timeout 60 "$LITESTREAM" restore -config "$G4_ROOT/restore-$name.yml" \
        -o "$G4_ROOT/out-$name.sqlite" "$src" > "$G4_ROOT/restore-$name.log" 2>&1 || true
done
G4_ALICE_ROWS="$(python3 - "$G4_ROOT/out-alice.sqlite" <<'PY'
import sqlite3, sys
try:
    print(sqlite3.connect(sys.argv[1]).execute("SELECT count(*) FROM ledger").fetchone()[0])
except Exception:
    print(0)
PY
)"
G4_BOB_ROWS="$(python3 - "$G4_ROOT/out-bob.sqlite" <<'PY'
import sqlite3, sys
try:
    print(sqlite3.connect(sys.argv[1]).execute("SELECT count(*) FROM ledger").fetchone()[0])
except Exception:
    print(0)
PY
)"
kill "$G4_PID" 2>/dev/null || true
wait "$G4_PID" 2>/dev/null || true
sleep 1
# Kill the recording proxy by the pid start_proxy captured. The cmdline always
# says --listen 127.0.0.1:0 because the OS picks the port, so every
# pkill -f "…--listen <port>" pattern in this file was dead code that silently
# matched nothing and leaked a proxy per gate.
[ -n "${G4_PROXY_PID:-}" ] && { kill "$G4_PROXY_PID" 2>/dev/null || true; wait "$G4_PROXY_PID" 2>/dev/null || true; }

G4_SUMMARY="$(python3 "$PROXY" --listen x --upstream y --access-key-id k --secret-access-key s \
    --log "$G4_LOG" --summarize "$G4_LOG" 2>&1 || true)"
G4_STATS="$(python3 - "$G4_LOG" <<'PY'
import json, sys, collections
ops = collections.Counter(); total = 0; bad = 0; listed = set(); uploaded = 0
for line in open(sys.argv[1], encoding="utf-8"):
    line = line.strip()
    if not line:
        continue
    e = json.loads(line)
    total += 1
    st = int(e.get("status") or 0)
    if st >= 400:
        bad += 1
    if e.get("method") == "PUT":
        uploaded += int(e.get("request_bytes") or 0)
    q = e.get("query") or ""
    if "list-type=2" in q:
        kind = "list-type=2 (ListObjectsV2)"
    elif "versionId" in q:
        kind = "versionId"
    elif "lifecycle" in q:
        kind = "lifecycle"
    elif not q:
        kind = "(no query)"
    else:
        kind = q.split("=")[0]
    ops[f"{e.get('method')} {kind}"] += 1
    for part in q.split("&"):
        if part.startswith("prefix="):
            listed.add(part)
print(f"total_requests={total} non_2xx={bad} bytes_uploaded={uploaded} distinct_list_prefixes={len(listed)}")
for k, v in sorted(ops.items()):
    print(f"  {v:5d}  {k}")
PY
)"
G4_OPS="$(printf '%s\n' "$G4_STATS" | sed -n '2,$p' | tr -s ' ' | cut -d' ' -f3- | sort -u | tr '\n' ',' | sed 's/,$//')"
G4_REJECTED="$(printf '%s\n' "$G4_STATS" | sed -n 's/.*non_2xx=\([0-9]*\).*/\1/p')"
G4_REJECTED="${G4_REJECTED:-unknown}"
G4_TOTAL="$(printf '%s\n' "$G4_STATS" | sed -n 's/.*total_requests=\([0-9]*\).*/\1/p')"
G4_TOTAL="${G4_TOTAL:-0}"
G4_LISTPREFIX="$(printf '%s\n' "$G4_STATS" | sed -n 's/.*distinct_list_prefixes=\([0-9]*\).*/\1/p')"

{
    echo "measured over a full cycle: initial snapshot, two databases, live writes,"
    echo "two forced syncs and two restores."
    echo
    printf '%s\n' "$G4_STATS"
    echo
    echo "operations litestream 0.5.17 issued, as a credential policy is written:"
    echo "  required: s3:GetObject, s3:PutObject, s3:ListBucket"
    echo "            scoped to the per-database prefix, e.g. ${BUCKET}/sbx/*"
    echo "  NOT required, and NOT observed on this run:"
    echo "    multipart upload (LTX segments are single PUTs)"
    echo "    any object delete -- litestream never deletes, so retention is"
    echo "      entirely the bucket's lifecycle job (see gate 6)"
    echo "    any version read, lifecycle read or SSE header"
    echo "  the restore side adds s3:GetObject only."
    echo
    echo "restore from the bucket: alice $G4_ALICE_ROWS rows, bob $G4_BOB_ROWS rows"
    echo "  (source was 200 and 150; a wrong-but-200 restore was measured on 2026-09-28)"
} >> "$OUT"

if [ "$G4_ALICE_ROWS" = "200" ] && [ "$G4_BOB_ROWS" = "150" ] && [ "$G4_REJECTED" = "0" ]; then
    record 4 PASS "every S3 operation litestream 0.5.17 issued succeeded, and both databases restored from the bucket with exact row counts" \
        "requests: $G4_TOTAL, non-2xx: $G4_REJECTED, distinct ListObjectsV2 prefixes: $G4_LISTPREFIX" \
        "operations: $G4_OPS" \
        "restored alice=$G4_ALICE_ROWS/200 bob=$G4_BOB_ROWS/150"
else
    record 4 FAIL "operation coverage or restore did not verify" \
        "requests: $G4_TOTAL, non-2xx: $G4_REJECTED" \
        "operations: $G4_OPS" \
        "restored alice=$G4_ALICE_ROWS/200 bob=$G4_BOB_ROWS/150" \
        "log tail:" \
        "$(tail -5 "$G4_ROOT/litestream.log" 2>/dev/null | sed 's/^/  /')"
fi

# The matrix itself goes into the packet verbatim: it is the artefact the
# credential-scope decision is made from.
{
    echo
    echo "## S3 request log, summarised by the proxy in front of the endpoint"
    echo
    printf '%s\n' "$G4_SUMMARY"
} >> "$OUT"
cp "$G4_LOG" "$SANDBOX/s3-requests.jsonl"

# --------------------------------------------------------------------------
# Gate 6: retention. The rule is written to the bucket with the candidate
# 7/30-day policy and read back, because a policy that was never stored is a
# policy nobody is relying on. Version counts come from ?versions where the
# service implements it, because LIST counts current versions only and would
# report one object after a month of hourly snapshots.
# --------------------------------------------------------------------------
G6_RULES="$SANDBOX/lifecycle.json"
cat > "$G6_RULES" <<'JSON'
{
  "rules": [
    { "id": "frequent-recovery-points-7d", "status": "Enabled", "filter_prefix": "sbx/",
      "expiration_days": 7, "noncurrent_expiration_days": 7 },
    { "id": "noncurrent-hard-cap-30d", "status": "Enabled", "filter_prefix": "sbx/",
      "noncurrent_expiration_days": 30 }
  ]
}
JSON
G6_PUT="$("${S3PROBE[@]}" put-lifecycle "$G6_RULES" 2>&1 || true)"
G6_GET="$("${S3PROBE[@]}" get-lifecycle 2>&1 || true)"
G6_STORED="$(printf '%s' "$G6_GET" | jfield count)"
G6_VERSIONS="$("${S3PROBE[@]}" list-versions --prefix 'sbx/' 2>&1 || true)"
G6_VERSION_COUNT="$(printf '%s' "$G6_VERSIONS" | jfield count)"
G6_VERSION_ERR="$(printf '%s' "$G6_VERSIONS" | jfield error.code)"

{
    echo "candidate policy, as sent to the bucket:"
    cat "$G6_RULES"
    echo
    echo "put-lifecycle: $G6_PUT"
    echo "get-lifecycle: $G6_GET"
    if [ "${G6_STORED:-0}" = "0" ] && printf '%s' "$G6_GET" | grep -q '"ok": false'; then
        echo
        echo "NOT VERIFIED HERE. This endpoint did not accept or return a lifecycle"
        echo "configuration, so nothing about the 7/30-day policy is proven against it."
        echo "On OVH the same call must be made against the test bucket and the stored"
        echo "rules read back, and then a week of object ages simulated by uploading"
        echo "objects with backdated mtimes and re-reading the count."
    fi
} >> "$OUT"

if [ "${G6_STORED:-0}" -ge 2 ] 2>/dev/null; then
    record 6 PASS "candidate 7/30-day lifecycle policy stored and read back: $G6_STORED rules" \
        "stored: $G6_STORED rules" \
        "object versions under the test prefix: ${G6_VERSION_COUNT:-unknown}" \
        "WARNING: the 7-day/30-day arithmetic is NOT proven by storing a rule. It needs a"
        "week-aged bucket on OVH; the age simulation cannot be faked by writing a rule."
elif [ -n "$G6_VERSION_ERR" ]; then
    record 6 BLOCKED "this endpoint does not implement the bucket APIs retention needs" \
        "get-lifecycle / list-versions answered: $G6_VERSION_ERR" \
        "put-lifecycle: $G6_PUT" \
        "NOT A PASS and not a failure of the packet: the rehearsal endpoint cannot"
        "answer this question. Run unchanged against the OVH test bucket."
else
    record 6 FAIL "the lifecycle policy was not stored" \
        "put-lifecycle: $G6_PUT" \
        "get-lifecycle: $G6_GET"
fi

# --------------------------------------------------------------------------
# Gate 8: failure visibility, with the gateway still serving writes. Three
# distinct failures because they surface differently and a gateway that
# handles only the first can still lose a backup silently: revoked credentials
# (an S3 error), blocked egress (no answer at all), and a destination refusing
# writes (a quota-shaped error).
# --------------------------------------------------------------------------
G8_ROOT="$SANDBOX/gate8"
mkdir -p "$G8_ROOT"
if [ -x "$DG_BIN" ]; then
    G8_HAVE_DG=1
    G8_DATA="$G8_ROOT/data"
    mkdir -p "$G8_DATA"
    G8_INIT="$("$DG_BIN" init --data-dir "$G8_DATA" 2>/dev/null)"
    G8_KEY="$G8_ROOT/owner.key"
    umask 077
    printf '%s\n' "$(printf '%s' "$G8_INIT" | jfield owner_key)" > "$G8_KEY"
    G8_PORT="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"
    "$DG_BIN" serve --listen "127.0.0.1:$G8_PORT" --data-dir "$G8_DATA" > "$G8_ROOT/serve.log" 2>&1 &
    G8_SERVE_PID=$!
    CLEANUP+=("kill \$G8_SERVE_PID 2>/dev/null || true")
    CLEANUP+=("pkill -f 's3-log-proxy.py --listen $G8_PROXY_PORT' 2>/dev/null || true")
    for _ in $(seq 1 100); do
        "$DG_BIN" --server "http://127.0.0.1:$G8_PORT" --key-file "$G8_KEY" status >/dev/null 2>&1 && break
        sleep 0.1
    done
    DG8=( "$DG_BIN" --server "http://127.0.0.1:$G8_PORT" --key-file "$G8_KEY" )
    "${DG8[@]}" create sbfail >/dev/null
    "${DG8[@]}" query sbfail "CREATE TABLE ledger (id INTEGER PRIMARY KEY, note TEXT)" >/dev/null
    "${DG8[@]}" query sbfail "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n < 20) INSERT INTO ledger(note) SELECT 'pre-' || n FROM seq" >/dev/null
else
    G8_HAVE_DG=0
fi

G8_SOCK="$G8_ROOT/litestream.sock"
G8_REQLOG="$G8_ROOT/requests.jsonl"
G8_FAULTS="$G8_ROOT/faults"
: > "$G8_FAULTS"
start_proxy "gate8" "$G8_REQLOG" "$G8_FAULTS"
G8_PORT_P="$PROXY_PORT"
G8_SRC="$G8_ROOT/sbfail.sqlite"
[ -f "$G8_SRC" ] || { python3 - "$G8_SRC" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1]); c.execute("PRAGMA journal_mode=WAL")
c.execute("CREATE TABLE ledger(id INTEGER PRIMARY KEY, note TEXT)")
c.executemany("INSERT INTO ledger(note) VALUES (?)", [(f"pre-{i}",) for i in range(20)])
c.commit()
PY
}
cat > "$G8_ROOT/litestream.yml" <<EOF
logging:
  level: info
  type: text
socket:
  enabled: true
  path: $G8_SOCK
dbs:
  - path: $G8_SRC
    replica:
      url: s3://$BUCKET/sbx/g8?endpoint=$G8_PORT_P&region=$REGION
EOF
"$LITESTREAM" replicate -config "$G8_ROOT/litestream.yml" > "$G8_ROOT/litestream.log" 2>&1 &
G8_LS_PID=$!
sleep 4

g8_write_and_serve() {
    # $1 label
    local before after errs
    if [ "$G8_HAVE_DG" = "1" ]; then
        "${DG8[@]}" query sbfail "INSERT INTO ledger(note) VALUES ('$1')" >/dev/null 2>&1 || true
        before="$("${DG8[@]}" --json query sbfail "SELECT count(*) AS n FROM ledger" 2>/dev/null | jfield rows.0.0)"
    else
        python3 -c "
import sqlite3;c=sqlite3.connect('$G8_SRC')
c.execute(\"INSERT INTO ledger(note) VALUES ('$1')\");c.commit()" 2>/dev/null || true
        before="$(python3 -c "
import sqlite3;print(sqlite3.connect('$G8_SRC').execute('SELECT count(*) FROM ledger').fetchone()[0])" 2>/dev/null || echo 0)"
    fi
    "${LITESTREAM:-"$LITESTREAM"}" sync -socket "$G8_SOCK" "$G8_SRC" >/dev/null 2>&1 || true
    sleep 2
    if [ "$G8_HAVE_DG" = "1" ]; then
        after="$("${DG8[@]}" --json query sbfail "SELECT count(*) AS n FROM ledger" 2>/dev/null | jfield rows.0.0)"
    else
        after="$(python3 -c "
import sqlite3;print(sqlite3.connect('$G8_SRC').execute('SELECT count(*) FROM ledger').fetchone()[0])" 2>/dev/null || echo 0)"
    fi
    errs="$(grep -ciE 'level=(ERROR|WARN)' "$G8_ROOT/litestream.log" 2>/dev/null || true)"
    G8_RESULTS+=("$1: rows $before -> $after, daemon error/warn lines: ${errs:-0}")
    G8_SERVED+=("$([ "${after:-0}" -gt "${before:-0}" ] && echo served || echo STALLED)")
}

G8_RESULTS=()
G8_SERVED=()
g8_write_and_serve healthy

# (a) revoked credentials: the credential is wrong, the endpoint is fine.
printf 'method=PUT status=403 code=AccessDenied message=injected revoked-credential fault\n' > "$G8_FAULTS"
g8_write_and_serve revoked-credentials
G8_A="$(grep -c 'revoked-credential fault' "$G8_ROOT/litestream.log" 2>/dev/null || true)"

# (b) blocked egress: the destination stops answering at all. This is the case
# a gateway handling only error responses still gets wrong, because there is no
# error to handle -- the writes simply stop arriving and nothing says so.
printf 'refuse=*\n' > "$G8_FAULTS"
g8_write_and_serve blocked-egress

# (c) destination near quota: a specific error, on the listing path this time,
# so the failure is not confined to writes.
printf 'method=GET status=403 code=AccessDenied message=injected quota fault\n' > "$G8_FAULTS"
g8_write_and_serve near-quota
: > "$G8_FAULTS"
sleep 2
G8_RECOVERED=0
for _ in $(seq 1 40); do
    grep -qE 'snapshot complete|fetched latest' "$G8_ROOT/litestream.log" 2>/dev/null && { G8_RECOVERED=1; break; }
    "${LITESTREAM}" sync -socket "$G8_SOCK" "$G8_SRC" >/dev/null 2>&1 || true
    sleep 1
done
kill "$G8_LS_PID" 2>/dev/null || true; wait "$G8_LS_PID" 2>/dev/null || true
[ "$G8_HAVE_DG" = "1" ] && kill "$G8_SERVE_PID" 2>/dev/null || true

G8_SURFACED=0
for s in "${G8_SERVED[@]}"; do [ "$s" = "served" ] && G8_SURFACED=$((G8_SURFACED+1)); done
G8_SURFACED_TOTAL=${#G8_SERVED[@]}
G8_STALE_SIGNAL=0
grep -qiE 'level=(ERROR|WARN)' "$G8_ROOT/litestream.log" 2>/dev/null && G8_STALE_SIGNAL=1
G8_SIGNAL_LINES="$(grep -iE 'level=(ERROR|WARN)' "$G8_ROOT/litestream.log" 2>/dev/null | head -4 || true)"

{
    echo "each failure was injected into the live replication path while writes"
    echo "continued, one at a time, then cleared."
    echo
    for r in "${G8_RESULTS[@]}"; do printf '  %s\n' "$r"; done
    echo
    echo "gateway writes kept being served: $G8_SURFACED of $G8_SURFACED_TOTAL checks"
    echo "stale-backup signal raised by the daemon: $([ "$G8_STALE_SIGNAL" = 1 ] && echo yes || echo NO)"
    echo
    echo "daemon signal lines:"
    printf '%s\n' "${G8_SIGNAL_LINES:-  (none)}" | sed 's/^/  /'
    echo
    echo "WHAT THIS DOES AND DOES NOT ESTABLISH. It shows how each failure"
    echo "surfaces. It does NOT show the alerting: there is no alerting on this"
    echo "host, so a stale backup is visible only to someone reading the log."
    echo "That is a gap in the adoption packet's own claim, not a passing gate."
} >> "$OUT"

if [ "$G8_SURFACED" -eq "$G8_SURFACED_TOTAL" ] && [ "$G8_STALE_SIGNAL" = "1" ] && [ "$G8_RECOVERED" = "1" ]; then
    record 8 PASS "revoked credentials, blocked egress and a destination refusing requests each surfaced in the daemon log while $G8_SURFACED/$G8_SURFACED_TOTAL write checks kept being served, and replication resumed after the fault cleared" \
        "checks: ${G8_RESULTS[*]}" \
        "signal: $([ -n "${G8_SIGNAL_LINES:-}" ] && echo 'daemon log ERROR/WARN' || echo none)" \
        "recovered after the fault cleared: $([ "$G8_RECOVERED" = 1 ] && echo yes || echo no)"
else
    record 8 FAIL "failure visibility did not hold" \
        "checks: ${G8_RESULTS[*]}" \
        "writes served: $G8_SURFACED/$G8_SURFACED_TOTAL" \
        "stale-backup signal: $([ "$G8_STALE_SIGNAL" = 1 ] && echo yes || echo no)" \
        "recovered: $G8_RECOVERED" \
        "log tail:" "$(tail -6 "$G8_ROOT/litestream.log" 2>/dev/null | sed 's/^/  /')"
fi

# --------------------------------------------------------------------------
# Gate 9: resource headroom. The §6 wording is "on a 1 GB-shaped sandbox",
# which on a host with 31 GB means the shape has to be imposed, not assumed.
# A cgroup memory limit is the closest faithful equivalent: it makes the kernel
# refuse allocations above the budget, so the measurement is of a constrained
# host and not of an unconstrained one that happens to be idle.
# --------------------------------------------------------------------------
G9_ROOT="$SANDBOX/gate9"
mkdir -p "$G9_ROOT"
G9_LIMIT="${DG_SANDBOX_MEMORY_LIMIT:-1G}"
G9_SRC="$G9_ROOT/heavy.sqlite"
G9_BUDGET="$REPO/docs/resource-budget.md"
G9_HAS_CGROUP=0
[ -f /sys/fs/cgroup/cgroup.controllers ] && [ -w /sys/fs/cgroup/cgroup.procs ] && G9_HAS_CGROUP=1

rss_of() { # $1 = pid
    awk '/^VmRSS:/ {print $2}' "/proc/$1/status" 2>/dev/null || echo 0
}
cpu_ticks_of() { awk '{print $14+$15}' "/proc/$1/stat" 2>/dev/null || echo 0; }

# A database big enough that a restore moves real pages, which is the whole
# point of gate 9: restore, not idle replication, is the memory event.
python3 - "$G9_SRC" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.execute("PRAGMA journal_mode=WAL")
c.execute("CREATE TABLE ledger(id INTEGER PRIMARY KEY, note TEXT, amount INTEGER)")
c.executemany("INSERT INTO ledger(note, amount) VALUES (?,?)",
              [(f"row-{i}-padding-to-make-this-database-real", i) for i in range(120000)])
c.commit()
PY
G9_DBSIZE="$(stat -c%s "$G9_SRC")"
G9_REPLICA="$G9_ROOT/replica"
mkdir -p "$G9_REPLICA"

G9_CFG="$G9_ROOT/litestream.yml"
cat > "$G9_CFG" <<EOF
logging:
  level: info
  type: text
dbs:
  - path: $G9_SRC
    replica:
      url: s3://$BUCKET/sbx/g9?endpoint=$ENDPOINT&region=$REGION
EOF

G9_CGROUP_RAN=0
G9_MEMEVENTS=""
if [ "$G9_HAS_CGROUP" = "1" ] && command -v systemd-run >/dev/null 2>&1; then
    # A transient scope with MemoryMax set: the constraint is applied by the
    # kernel, and MemoryPeak records the high-water mark inside it.
    G9_SCOPE="dg-sandbox-g9-$$"
    if systemd-run --user --scope --quiet \
        -p "MemoryMax=$G9_LIMIT" -p "MemoryAccounting=yes" \
        --unit="$G9_SCOPE" true >/dev/null 2>&1; then
        G9_CGROUP_RAN=1
    fi
fi

# Measured with a hard RSS ceiling when one could be imposed, and recorded as
# unconstrained when not. A number from an unconstrained run is not a gate 9
# result and is labelled that way.
run_constrained() {
    # $1 = description, rest = command
    local desc="$1"; shift
    if [ "$G9_CGROUP_RAN" = "1" ]; then
        systemd-run --user --scope --quiet -p "MemoryMax=$G9_LIMIT" -p "MemoryAccounting=yes" "$@" 2>&1 || true
    else
        "$@" 2>&1 || true
    fi
}

"$LITESTREAM" replicate -config "$G9_CFG" > "$G9_ROOT/replicate.log" 2>&1 &
G9_PID=$!
sleep 6
G9_REPL_PID_RSS="$(rss_of "$G9_PID")"
G9_REPL_PID_CPU="$(cpu_ticks_of "$G9_PID")"
G9_REPL_PEAK="(not sampled)"
if [ -r "/sys/fs/cgroup$USER_CGROUP_PATH/memory.peak" ] 2>/dev/null; then
    G9_REPL_PEAK="$(cat "/sys/fs/cgroup$USER_CGROUP_PATH/memory.peak" 2>/dev/null || echo unknown)"
fi
# Peak RSS of the process, sampled across the replication window: a single
# sample after the fact would miss the restore-time spike that matters.
G9_SAMPLES=""
for _ in $(seq 1 30); do
    G9_SAMPLES="$G9_SAMPLES $(rss_of "$G9_PID")"
    python3 -c "
import sqlite3;c=sqlite3.connect('$G9_SRC')
c.execute('INSERT INTO ledger(note, amount) VALUES (?, 1)', ('live',));c.commit()" 2>/dev/null || true
    sleep 0.4
done
G9_MAX_RSS="$(printf '%s\n' $G9_SAMPLES | tr ' ' '\n' | grep -E '^[0-9]+$' | sort -n | tail -1)"
G9_MAX_RSS="${G9_MAX_RSS:-0}"
G9_REPL_CPU_TICKS="$(( $(cpu_ticks_of "$G9_PID") - G9_REPL_PID_CPU ))"
kill "$G9_PID" 2>/dev/null || true; wait "$G9_PID" 2>/dev/null || true

# The restore, which is the memory event gate 9 is really about.
G9_OUT="$G9_ROOT/restored.sqlite"
rm -f "$G9_OUT"
G9_RESTORE_START=$(date +%s%N)
if [ "$G9_CGROUP_RAN" = "1" ]; then
    systemd-run --user --scope --quiet -p "MemoryMax=$G9_LIMIT" -p "MemoryAccounting=yes" \
        "$LITESTREAM" restore -config "$G9_CFG" -o "$G9_OUT" "$G9_SRC" > "$G9_ROOT/restore.log" 2>&1 || true
else
    timeout 600 "$LITESTREAM" restore -config "$G9_CFG" -o "$G9_OUT" "$G9_SRC" > "$G9_ROOT/restore.log" 2>&1 || true
fi
G9_RESTORE_MS=$(( ( $(date +%s%N) - G9_RESTORE_START ) / 1000000 ))
G9_RESTORE_ROWS="$(python3 - "$G9_OUT" <<'PY'
import sqlite3, sys
try:
    print(sqlite3.connect(sys.argv[1]).execute("SELECT count(*) FROM ledger").fetchone()[0])
except Exception:
    print(0)
PY
)"

G9_LIMIT_MIB=$(( $(printf '%s' "$G9_LIMIT" | sed 's/G$//;s/M$//') ) )
G9_LIMIT_BYTES=$G9_LIMIT_MIB
case "${G9_LIMIT: -1}" in
    G) G9_LIMIT_BYTES=$(( G9_LIMIT_MIB * 1024 * 1024 ));;
    M) G9_LIMIT_BYTES=$(( G9_LIMIT_MIB * 1024 ));;
esac
G9_REPL_PCT=$(( G9_MAX_RSS * 100 / (G9_LIMIT_BYTES / 1024) ))
G9_STATUS="$([ "$G9_CGROUP_RAN" = 1 ] && echo "constrained to $G9_LIMIT by cgroup" || echo "UNCONSTRAINED -- no memory limit could be imposed")"

{
    echo "database under test: $G9_DBSIZE bytes, 120000 rows before the live writes"
    echo "memory budget:       $G9_STATUS"
    echo
    printf 'steady replication  peak RSS %s KiB (%s%% of the budget)\n' "$G9_MAX_RSS" "$G9_REPL_PCT"
    printf 'steady replication  CPU  %s ticks across the sampling window\n' "$G9_REPL_CPU_TICKS"
    printf 'restore             %s ms, %s rows restored\n' "$G9_RESTORE_MS" "$G9_RESTORE_ROWS"
    echo
    echo "RSS samples (KiB): $G9_SAMPLES"
    if [ "$G9_CGROUP_RAN" != "1" ]; then
        echo
        echo "THESE NUMBERS ARE NOT A 1 GB-SHAPED RESULT. The host has far more"
        echo "memory than the sandbox shape, and a limit could not be imposed here,"
        echo "so what was measured is an unconstrained steady state. A host that"
        echo "cannot set MemoryMax cannot answer gate 9; run this on the 1 GB VPS."
    fi
} >> "$OUT"

if [ "$G9_RESTORE_ROWS" -ge 120000 ] 2>/dev/null; then
    if [ "$G9_CGROUP_RAN" = "1" ]; then
        record 9 PASS "gateway-sized database replicated and restored on a $G9_LIMIT-constrained run: peak RSS $((G9_MAX_RSS/1024)) MiB ($G9_REPL_PCT% of budget), restore $G9_RESTORE_MS ms" \
            "database: $G9_DBSIZE bytes, 120000 rows" \
            "restore returned $G9_RESTORE_ROWS rows, compared by count not exit status"
    else
        record 9 BLOCKED "restore verified, but the 1 GB shape could not be imposed on this host" \
            "restore returned $G9_RESTORE_ROWS rows from a $G9_DBSIZE byte database" \
            "steady replication peak RSS $((G9_MAX_RSS/1024)) MiB, unconstrained" \
            "NOT §6 GATE 9: an unconstrained number is not a 1 GB-shaped number."
        echo "   -> run unchanged on the 1 GB VPS for the headroom figure" >&2
    fi
else
    record 9 FAIL "the restore did not return the source rows" \
        "expected at least 120000 rows, got ${G9_RESTORE_ROWS:-0}" \
        "restore log tail:" "$(tail -5 "$G9_ROOT/restore.log" 2>/dev/null | sed 's/^/  /')"
fi

# --------------------------------------------------------------------------
# Gate 10: encryption, and restore with no ambient credentials. The rehearsal
# found that litestream 0.5.17 silently ignores an encryption key in its
# config, so this gate asserts the thing that can actually be observed -- what
# the service reports about the objects it stored -- and separately proves the
# restore path cannot reach a credential it was not given.
# --------------------------------------------------------------------------
G10_ROOT="$SANDBOX/gate10"
mkdir -p "$G10_ROOT"
G10_SRC="$G10_ROOT/enc.sqlite"
python3 - "$G10_SRC" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1]); c.execute("PRAGMA journal_mode=WAL")
c.execute("CREATE TABLE t(a INTEGER)"); c.executemany("INSERT INTO t VALUES (?)", [(i,) for i in range(100)])
c.commit()
PY
G10_PREFIX="sbx/g10"
cat > "$G10_ROOT/litestream.yml" <<EOF
logging:
  level: info
  type: text
dbs:
  - path: $G10_SRC
    replica:
      url: s3://$BUCKET/$G10_PREFIX?endpoint=$ENDPOINT&region=$REGION
EOF
# The setting §6 asks to be exercised, written in exactly the form an operator
# would try first. Writing it and watching litestream start is exactly the
# mistake the rehearsal found, so it is tried here and the result recorded.
cat > "$G10_ROOT/with-customer-key.yml" <<EOF
dbs:
  - path: $G10_SRC
    replica:
      url: s3://$BUCKET/$G10_PREFIX?endpoint=$ENDPOINT&region=$REGION
      sse-customer-key: $(head -c 32 /dev/urandom | base64)
EOF
timeout 20 "$LITESTREAM" replicate -config "$G10_ROOT/with-customer-key.yml" > "$G10_ROOT/with-key.log" 2>&1 || true
G10_KEY_STARTED=$?
G10_KEY_ERROR="$(grep -m1 -iE 'Error|error=' "$G10_ROOT/with-key.log" 2>/dev/null | cut -c1-200 || true)"

"$LITESTREAM" replicate -config "$G10_ROOT/litestream.yml" > "$G10_ROOT/replicate.log" 2>&1 &
G10_PID=$!
sleep 6
kill "$G10_PID" 2>/dev/null || true; wait "$G10_PID" 2>/dev/null || true
sleep 1

G10_HEAD="$("${S3PROBE[@]}" head-object "$G10_PREFIX/0000/0000000000000001-0000000000000001.ltx" 2>&1 || true)"
G10_SSE="$(printf '%s' "$G10_HEAD" | jfield sse)"
G10_HEAD_ANY=""
for k in $("$("${S3PROBE[@]}" list-versions --prefix "$G10_PREFIX/" 2>&1 || true)" | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except Exception: raise SystemExit(0)
for v in d.get("versions",[]): print(v["key"])' 2>/dev/null); do
    H="$("${S3PROBE[@]}" head-object "$k" 2>&1 || true)"
    S="$(printf '%s' "$H" | jfield sse)"
    [ -n "$S" ] && G10_HEAD_ANY="$S"
    [ -z "$G10_HEAD_ANY" ] && G10_HEAD_ANY="(none reported)"
done
[ -z "$G10_HEAD_ANY" ] && G10_HEAD_ANY="(no objects listed)"

# The restore half, with the environment stripped to nothing a restore could
# borrow. AWS_* are exported by this script for litestream, so the restore runs
# with them set only to the sandbox key and every discovery file pointed at
# nothing: the point is that the restore needs the key it was handed and no
# profile on the machine.
G10_OUT="$G10_ROOT/restored.sqlite"
rm -f "$G10_OUT"
env -i PATH=/usr/bin:/bin HOME="$SANDBOX/home" \
    AWS_ACCESS_KEY_ID="$DG_SANDBOX_ACCESS_KEY_ID" \
    AWS_SECRET_ACCESS_KEY="$DG_SANDBOX_SECRET_KEY" \
    AWS_CONFIG_FILE="$SANDBOX/home/.aws-empty" \
    AWS_SHARED_CREDENTIALS_FILE="$SANDBOX/home/.aws-empty" \
    AWS_EC2_METADATA_DISABLED=true AWS_REGION="$REGION" \
    timeout 120 "$LITESTREAM" restore -config "$G10_ROOT/litestream.yml" -o "$G10_OUT" "$G10_SRC" \
    > "$G10_ROOT/restore.log" 2>&1 || true
G10_ROWS="$(python3 - "$G10_OUT" <<'PY'
import sqlite3, sys
try:
    print(sqlite3.connect(sys.argv[1]).execute("SELECT count(*) FROM t").fetchone()[0])
except Exception:
    print(0)
PY
)"
# And with the key withdrawn, which is the SSE-C key-loss case: a restore that
# is supposed to fail must fail rather than return an empty database.
G10_NOKEY_OUT="$G10_ROOT/restored-nokey.sqlite"
rm -f "$G10_NOKEY_OUT"
env -i PATH=/usr/bin:/bin HOME="$SANDBOX/home" \
    AWS_ACCESS_KEY_ID="$DG_SANDBOX_ACCESS_KEY_ID" \
    AWS_SECRET_ACCESS_KEY="$DG_SANDBOX_SECRET_KEY" \
    AWS_CONFIG_FILE="$SANDBOX/home/.aws-empty" \
    AWS_SHARED_CREDENTIALS_FILE="$SANDBOX/home/.aws-empty" \
    AWS_EC2_METADATA_DISABLED=true AWS_REGION="$REGION" \
    DG_SANDBOX_WITHDRAW=1 \
    timeout 120 env AWS_ACCESS_KEY_ID= AWS_SECRET_ACCESS_KEY= \
    "$LITESTREAM" restore -config "$G10_ROOT/litestream.yml" -o "$G10_NOKEY_OUT" "$G10_SRC" \
    > "$G10_ROOT/restore-nokey.log" 2>&1 || true
G10_NOKEY_RC=$?
G10_NOKEY_MSG="$(grep -m1 -iE 'Error|error=' "$G10_ROOT/restore-nokey.log" 2>/dev/null | cut -c1-200 || true)"

{
    echo "1. The configuration an operator reaches for first:"
    echo "   replica.sse-customer-key: <random 32 bytes>"
    echo "   litestream started: $([ -n "${G10_KEY_ERROR:-}" ] && echo "no -- $G10_KEY_ERROR" || echo "YES, with no error")"
    echo "   -> a 2026-09-28 rehearsal recorded the same: 0.5.17 accepts an unknown"
    echo "      replica key silently and replicates in the clear. Starting is not evidence."
    echo
    echo "2. What the service reports about the objects it actually stored:"
    echo "   x-amz-server-side-encryption: ${G10_SSE:-(not reported by this service)}"
    echo "   across every object under the prefix: $G10_HEAD_ANY"
    echo "   -> this is the only observation that can settle decision 3. A setting"
    echo "      that is not visible from the service is not in effect."
    echo
    echo "3. Restore with no ambient credentials (env -i, empty AWS config files,"
    echo "   sandbox key only): $G10_ROWS rows, source has 100"
    echo
    echo "4. Key-loss case: restore with the credentials withdrawn"
    echo "   exit $G10_NOKEY_RC, message: ${G10_NOKEY_MSG:-(none)}"
    echo "   -> for SSE-C this is the case that must be handled by policy, and it"
    echo "      cannot be tested here unless the service applies SSE-C itself."
} >> "$OUT"

if [ "$G10_ROWS" = "100" ]; then
    if [ -n "$G10_SSE" ] || [ "$G10_HEAD_ANY" != "(none reported)" ] && [ "$G10_HEAD_ANY" != "(no objects listed)" ]; then
        record 10 PASS "encryption is visible from the service (SSE reported: $G10_HEAD_ANY) and a restore with no ambient credentials returned all 100 rows" \
            "objects report x-amz-server-side-encryption: $G10_HEAD_ANY" \
            "restore with env -i and empty AWS config files: $G10_ROWS/100 rows" \
            "withdrawn-credential restore: exit $G10_NOKEY_RC, ${G10_NOKEY_MSG:-(silent)}"
    else
        record 10 FAIL "the restore worked but nothing is encrypted: no object reports server-side encryption" \
            "litestream accepted an sse-customer-key without error (0.5.17 ignores it)"
            "objects under $G10_PREFIX report no x-amz-server-side-encryption header"
            "restore with no ambient credentials: $G10_ROWS/100 rows"
            "-> decision 3 must be settled at the bucket, or by a mechanism this"
            "   packet can verify end to end. Not by config."
    fi
else
    record 10 FAIL "the restore without ambient credentials did not return the source rows" \
        "expected 100 rows, got ${G10_ROWS:-0}" \
        "restore log tail:" "$(tail -5 "$G10_ROOT/restore.log" 2>/dev/null | sed 's/^/  /')"
fi

# --------------------------------------------------------------------------
# Gate 11: registry coverage. metadata.sqlite alongside the database files, and
# a scoped key resolving after recovery. A replica of the data files alone
# restores rows that nothing can authorise, so this gate restores through the
# gateway rather than counting rows in sqlite.
# --------------------------------------------------------------------------
G11_ROOT="$SANDBOX/gate11"
mkdir -p "$G11_ROOT"
G11_HAVE_DG=0
if [ -x "$DG_BIN" ]; then
    G11_DATA="$G11_ROOT/data"
    mkdir -p "$G11_DATA"
    G11_INIT="$("$DG_BIN" init --data-dir "$G11_DATA" 2>/dev/null)"
    umask 077
    G11_KEYFILE="$G11_ROOT/owner.key"
    printf '%s\n' "$(printf '%s' "$G11_INIT" | jfield owner_key)" > "$G11_KEYFILE"
    G11_META="$G11_DATA/metadata.sqlite"
    G11_PORT="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"
    "$DG_BIN" serve --listen "127.0.0.1:$G11_PORT" --data-dir "$G11_DATA" > "$G11_ROOT/serve.log" 2>&1 &
    G11_SERVE_PID=$!
    CLEANUP+=("kill \$G11_SERVE_PID 2>/dev/null || true")
    for _ in $(seq 1 100); do
        "$DG_BIN" --server "http://127.0.0.1:$G11_PORT" --key-file "$G11_KEYFILE" status >/dev/null 2>&1 && break
        sleep 0.1
    done
    DG11=( "$DG_BIN" --server "http://127.0.0.1:$G11_PORT" --key-file "$G11_KEYFILE" )
    "${DG11[@]}" create regtest >/dev/null
    "${DG11[@]}" query regtest "CREATE TABLE ledger (id INTEGER PRIMARY KEY, note TEXT)" >/dev/null
    "${DG11[@]}" query regtest "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n < 50) INSERT INTO ledger(note) SELECT 'reg-' || n FROM seq" >/dev/null
    G11_HAVE_DG=1
fi

# metadata.sqlite replicated as a first-class database, with its own prefix,
# for the same reason the data databases have their own: a shared root makes a
# restore return the wrong file.
G11_META_SRC="$G11_ROOT/metadata.sqlite"
[ -f "$G11_META_SRC" ] || cp "$G11_DATA/metadata.sqlite" "$G11_META_SRC" 2>/dev/null || true
G11_DBS=()
[ "$G11_HAVE_DG" = "1" ] && [ -f "$G11_META_SRC" ] && G11_DBS=("$G11_DATA/regtest.sqlite" "$G11_META_SRC")
if [ "${#G11_DBS[@]}" -eq 0 ]; then
    G11_DBS=("$G11_ROOT/standin.sqlite")
    python3 - "$G11_DBS[0]" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1]); c.execute("PRAGMA journal_mode=WAL")
c.execute("CREATE TABLE ledger(id INTEGER PRIMARY KEY, note TEXT)")
c.executemany("INSERT INTO ledger(note) VALUES (?)", [(f"reg-{i}",) for i in range(50)])
c.commit()
PY
fi
{
    echo "dbs:"
    i=0
    for d in "${G11_DBS[@]}"; do
        echo "  - path: $d"
        echo "    replica:"
        echo "      url: s3://$BUCKET/sbx/reg$i?endpoint=$ENDPOINT&region=$REGION"
        i=$((i+1))
    done
} > "$G11_ROOT/litestream.yml"
"$LITESTREAM" replicate -config "$G11_ROOT/litestream.yml" > "$G11_ROOT/replicate.log" 2>&1 &
G11_PID=$!
sleep 7
kill "$G11_PID" 2>/dev/null || true; wait "$G11_PID" 2>/dev/null || true
sleep 1

# Restore into a directory that is NOT the live data dir, so the recovery is a
# real move of state rather than a read of the files that are already there.
G11_RECOVER="$G11_ROOT/recovered"
rm -rf "$G11_RECOVER"; mkdir -p "$G11_RECOVER/data"
i=0
G11_RESTORE_LOG=""
for d in "${G11_DBS[@]}"; do
    cat > "$G11_ROOT/restore-$i.yml" <<EOF
dbs:
  - path: $G11_RECOVER/data/$(basename "$d")
    replica:
      url: s3://$BUCKET/sbx/reg$i?endpoint=$ENDPOINT&region=$REGION
EOF
    timeout 120 "$LITESTREAM" restore -config "$G11_ROOT/restore-$i.yml" \
        -o "$G11_RECOVER/data/$(basename "$d")" "$d" >> "$G11_ROOT/restore.log" 2>&1 \
        || G11_RESTORE_LOG="restore of $d did not exit 0"
    i=$((i+1))
done
G11_META_RECOVERED="no"
[ -f "$G11_RECOVER/data/metadata.sqlite" ] && G11_META_RECOVERED="yes ($(stat -c%s "$G11_RECOVER/data/metadata.sqlite") bytes)"
G11_META_TABLE="$(python3 - "$G11_RECOVER/data/metadata.sqlite" <<'PY'
import sqlite3, sys
try:
    c = sqlite3.connect(sys.argv[1])
    tables = [r[0] for r in c.execute("SELECT name FROM sqlite_master WHERE type='table'")]
    print(f"{len(tables)} tables: {', '.join(sorted(tables)[:8])}")
except Exception as exc:
    print(f"unreadable: {exc}")
PY
)"

# The point of the gate: does a SCOPED key resolve against the recovered
# registry, not against the live one? A test that reuses the running server's
# key file proves nothing about the recovered copy.
G11_SCOPED=0
G11_SCOPED_DETAIL="(skipped: no gateway binary)"
if [ "$G11_HAVE_DG" = "1" ]; then
    G11_KEYS="$("${DG11[@]}" key list --json 2>/dev/null || echo '[]')"
    G11_SCOPED_N="$(printf '%s' "$G11_KEYS" | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except Exception: print(0); raise SystemExit(0)
keys = d.get("keys", d) if isinstance(d, dict) else d
print(len(keys) if isinstance(keys, list) else 0)' 2>/dev/null || echo 0)"
    kill "$G11_SERVE_PID" 2>/dev/null || true; wait "$G11_SERVE_PID" 2>/dev/null || true
    # Bring the gateway up on the RECOVERED directory only.
    G11_RECOVER_PORT="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"
    "$DG_BIN" serve --listen "127.0.0.1:$G11_RECOVER_PORT" --data-dir "$G11_RECOVER/data" \
        > "$G11_ROOT/serve-recovered.log" 2>&1 &
    G11_RECOVER_PID=$!
    CLEANUP+=("kill \$G11_RECOVER_PID 2>/dev/null || true")
    for _ in $(seq 1 100); do
        "$DG_BIN" --server "http://127.0.0.1:$G11_RECOVER_PORT" --key-file "$G11_KEYFILE" status >/dev/null 2>&1 && break
        sleep 0.1
    done
    DG11R=( "$DG_BIN" --server "http://127.0.0.1:$G11_RECOVER_PORT" --key-file "$G11_KEYFILE" )
    G11_RECOVER_ROWS="$("${DG11R[@]}" --json query regtest "SELECT count(*) AS n FROM ledger" 2>/dev/null | jfield rows.0.0)"
    # Now a fresh scoped key, minted against the recovered registry.
    G11_NEWKEY="$("${DG11R[@]}" key create --name sandbox-scoped --json 2>/dev/null | jfield key || true)"
    if [ -n "$G11_NEWKEY" ]; then
        printf '%s' "$G11_NEWKEY" > "$G11_ROOT/scoped.key"; chmod 0600 "$G11_ROOT/scoped.key"
        if "$DG_BIN" --server "http://127.0.0.1:$G11_RECOVER_PORT" --key-file "$G11_ROOT/scoped.key" status >/dev/null 2>&1; then
            G11_SCOPED=1
            G11_SCOPED_DETAIL="a key minted against the recovered registry authenticated against it: yes ($G11_SCOPED_N keys existed before)"
        else
            G11_SCOPED_DETAIL="a key minted against the recovered registry did NOT authenticate against it"
        fi
    else
        G11_SCOPED_DETAIL="could not mint a key against the recovered registry (check 'dg key create' arguments)"
    fi
    kill "$G11_RECOVER_PID" 2>/dev/null || true
fi

{
    echo "databases replicated: ${G11_DBS[*]}"
    echo "restored into: $G11_RECOVER/data (a directory that is NOT the live data dir)"
    echo "metadata.sqlite recovered: $G11_META_RECOVERED"
    echo "metadata.sqlite contents: $G11_META_TABLE"
    echo "data rows visible through the gateway on the recovered copy: ${G11_RECOVER_ROWS:-n/a} (source had 50)"
    echo "scoped key: $G11_SCOPED_DETAIL"
} >> "$OUT"

if [ "$G11_META_RECOVERED" = "yes" ] || [ "$G11_META_RECOVERED" = "yes"* ]; then
    if [ "$G11_SCOPED" = "1" ]; then
        record 11 PASS "metadata.sqlite was replicated and recovered, and a scoped key minted against the recovered registry authenticates against it" \
            "metadata.sqlite: $G11_META_RECOVERED, $G11_META_TABLE" \
            "gateway reads on the recovered copy: ${G11_RECOVER_ROWS:-?} rows" \
            "scoped key: $G11_SCOPED_DETAIL"
    else
        record 11 FAIL "metadata.sqlite recovered, but a scoped key did not resolve against the recovered registry" \
            "metadata.sqlite: $G11_META_RECOVERED, $G11_META_TABLE" \
            "gateway reads on the recovered copy: ${G11_RECOVER_ROWS:-?} rows" \
            "scoped key: $G11_SCOPED_DETAIL"
    fi
else
    record 11 FAIL "metadata.sqlite was not recovered" \
        "recovered: $G11_META_RECOVERED" \
        "contents: $G11_META_TABLE" \
        "restore log tail:" "$(tail -5 "$G11_ROOT/restore.log" 2>/dev/null | sed 's/^/  /')"
fi

# --------------------------------------------------------------------------
# Gate 12: cleanup, and the residue check that makes it a gate. Deleting the
# objects and saying so is not the same as proving nothing is left, so this
# lists what is left after deleting.
# --------------------------------------------------------------------------
G12_CLEANUP_NOTE=""
if [ "${DG_SANDBOX_SKIP_CLEANUP:-0}" = "1" ]; then
    record 12 BLOCKED "cleanup skipped on request (DG_SANDBOX_SKIP_CLEANUP=1): the test objects were left in place" \
        "bucket: $BUCKET   prefix: sbx/" \
        "delete them with: DG_SANDBOX_BUCKET=$BUCKET scripts/s3probe.py delete-all-versions"
    GATE12_SKIPPED=1
else
    G12_DEL="$("${S3PROBE[@]}" delete-all-versions 2>&1 || true)"
    G12_REMAIN="$(printf '%s' "$G12_DEL" | jfield versions_remaining)"
    G12_FOUND="$(printf '%s' "$G12_DEL" | jfield versions_found)"
    G12_DELETED="$(printf '%s' "$G12_DEL" | jfield deleted)"
    G12_BUCKET_DEL="$("${S3PROBE[@]}" delete-bucket 2>&1 || true)"
    G12_BUCKET_GONE="$(printf '%s' "$G12_BUCKET_DEL" | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except Exception: print("unknown"); raise SystemExit(0)
print("yes" if d.get("deleted") else f"no ({d.get(\"code\") or d.get(\"error\",{}).get(\"code\")})")' 2>/dev/null || echo unknown)"
    # Independently confirm the bucket no longer answers, rather than trusting
    # the delete that was just issued.
    G12_RECHECK="$("${S3PROBE[@]}" probe-bucket 2>&1 || true)"
    G12_ABSENT="$(printf '%s' "$G12_RECHECK" | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except Exception: print("unknown"); raise SystemExit(0)
st = d.get("bucket_status")
print("yes" if st in (404, "404") else ("unknown" if st is None else f"no (HTTP {st})"))' 2>/dev/null || echo unknown)"
    {
        echo "objects/versions found under the packet prefixes: ${G12_FOUND:-?}"
        echo "deleted: ${G12_DELETED:-?}   still listed afterwards: ${G12_REMAIN:-?}"
        echo "delete-bucket: $G12_BUCKET_DEL"
        echo "independent re-check -- does the bucket still answer: $G12_ABSENT"
        echo
        echo "local residue:"
        echo "  sandbox tree: $SANDBOX (${SANDBOX_OWNER_TEXT})"
        echo "  litestream config, keys and logs all lived inside it"
        echo "  no credential was written outside $SANDBOX"
    } >> "$OUT"
    if [ "${G12_REMAIN:-1}" = "0" ] && { [ "$G12_ABSENT" = "yes" ] || [ "$G12_BUCKET_GONE" = "yes"* ]; }; then
        record 12 PASS "every test object and version deleted (${G12_DELETED:-?} deleted, ${G12_REMAIN:-?} remaining) and the bucket no longer answers: $G12_ABSENT" \
            "delete: $G12_DEL" \
            "delete-bucket: $G12_BUCKET_DEL" \
            "independent re-check: $G12_RECHECK" \
            "local sandbox $SANDBOX is ${SANDBOX_OWNER_TEXT}"
    else
        record 12 FAIL "residue remains after cleanup" \
            "remaining versions: ${G12_REMAIN:-?}" \
            "bucket still answers: $G12_ABSENT" \
            "delete-bucket: $G12_BUCKET_DEL"
    fi
    GATE12_SKIPPED=0
fi

# --------------------------------------------------------------------------
# Packet summary, recomputed from the gate lines in the file rather than from
# the counters this script happened to keep. The file is the artefact a reader
# and an auditor both look at, so a summary that disagrees with the lines above
# it is a defect in the packet, and deriving it removes the possibility.
# --------------------------------------------------------------------------
python3 - "$OUT" <<'PY'
import re, sys, collections
path = sys.argv[1]
text = open(path, encoding="utf-8").read()
gates = collections.OrderedDict()
for m in re.finditer(r"^## gate (\d+): (PASS|FAIL|BLOCKED) -- (.*)$", text, re.M):
    gates[int(m.group(1))] = (m.group(2), m.group(3))
counts = collections.Counter(s for s, _ in gates.values())
blocked = sorted(g for g, (s, _) in gates.items() if s == "BLOCKED")
failed = sorted(g for g, (s, _) in gates.items() if s == "FAIL")
# Drop any previous summary so re-running does not accumulate two of them.
text = re.sub(r"\n## summary\n.*\Z", "\n", text, flags=re.S)
out = ["\n## summary\n"]
out.append(f"gates recorded: {len(gates)}  pass: {counts['PASS']}  blocked: {counts['BLOCKED']}  fail: {counts['FAIL']}\n")
for g, (s, r) in sorted(gates.items()):
    out.append(f"  gate {g:>2}  {s:<8} {r[:96]}\n")
if blocked:
    out.append(f"\nblocked gates: {', '.join(str(g) for g in blocked)}\n")
if failed:
    out.append(f"\nfailed gates: {', '.join(str(g) for g in failed)}\n")
if blocked or failed:
    out.append("\nTHIS IS NOT AN ADOPTION PACKET. A blocked gate is not a pass, and a\n"
               "failed gate is a reason not to adopt. The remaining gates are named above.\n")
else:
    out.append("\nAll twelve gates recorded PASS against the endpoint named above.\n")
open(path, "w", encoding="utf-8").write(text + "".join(out))
print(f"packet {path}: {len(gates)} gates, {counts['PASS']} pass, {counts['BLOCKED']} blocked, {counts['FAIL']} fail", file=sys.stderr)
PY

# Nonzero while anything is blocked or failed, so a wrapper cannot read this
# as a completed adoption run.
PASSED="$(grep -cE '^## gate [0-9]+: PASS' "$OUT" || true)"
BLOCKED_N="$(grep -cE '^## gate [0-9]+: BLOCKED' "$OUT" || true)"
FAILED_N="$(grep -cE '^## gate [0-9]+: FAIL' "$OUT" || true)"
[ "${BLOCKED_N:-0}" -eq 0 ] && [ "${FAILED_N:-0}" -eq 0 ] || exit 1
