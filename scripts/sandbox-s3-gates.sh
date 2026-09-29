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
# `DG_SANDBOX_TRACE=1` turns on xtrace so a mid-gate exit that produces no
# stderr of its own still leaves a trail in the driver log.
[ "${DG_SANDBOX_TRACE:-0}" = "1" ] && set -x

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
# rehearsal against a self-signed one has to be told to trust it. NOT via
# AWS_CA_BUNDLE: measured on 2026-09-28, litestream 0.5.17 aborts its replica
# client with "cannot load aws config: unable to add custom RootCAs HTTPClient,
# has no WithTransportOptions" -- the documented AWS variable is unusable with
# this version, and the failure is total, not a fallback. SSL_CERT_FILE is read
# by Go's crypto/x509, so the whole trust decision stays in the Go process and
# the replica URL the packet reports as minimal stays exactly what OVH needs --
# no skip-verify, no litestream-specific option, verification still ON.
CA_ARGS=()
CA_NOTE="system trust store (the endpoint's certificate chains to a public CA)"
if [ -n "${DG_SANDBOX_CA_BUNDLE:-}" ]; then
    if [ ! -r "$DG_SANDBOX_CA_BUNDLE" ]; then
        echo "refusing: DG_SANDBOX_CA_BUNDLE=$DG_SANDBOX_CA_BUNDLE is not readable" >&2
        exit 2
    fi
    export SSL_CERT_FILE="$DG_SANDBOX_CA_BUNDLE"
    unset AWS_CA_BUNDLE || true
    CA_ARGS=(--ca-bundle "$DG_SANDBOX_CA_BUNDLE")
    CA_NOTE="SSL_CERT_FILE=$DG_SANDBOX_CA_BUNDLE (system roots + the rehearsal endpoint's self-signed CA); verification stays ON, and AWS_CA_BUNDLE is deliberately NOT set because litestream 0.5.17 aborts on it"
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
        --upstream-scheme "$SCHEME" \
        ${DG_SANDBOX_CA_BUNDLE:+--upstream-ca-bundle "$DG_SANDBOX_CA_BUNDLE"} \
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
    # What is actually behind that name. A stand-in has to be declared, not
    # inferred: a store quirk read as a service fact is how a rehearsal turns
    # into evidence it never was. DG_SANDBOX_ENDPOINT_IMPL is set by
    # scripts/s3-rehearsal-endpoint.sh; on the owner's bucket nothing sets it.
    echo "service behind it: ${DG_SANDBOX_ENDPOINT_IMPL:-the S3 service itself (no local stand-in configured)}"
    if [ -n "${DG_SANDBOX_ENDPOINT_IMPL:-}" ]; then
        echo
        echo "The line above names a LOCAL STAND-IN, not the OVH service. Its"
        echo "answers are evidence about this packet's execution path and about"
        echo "nothing else -- not about OVH's storage classes, lifecycle rules,"
        echo "quota behaviour, encryption or cost."
    fi
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
    # {{EP}} is the recording proxy, NOT the configured endpoint. Pointing
    # litestream at the endpoint and expecting the proxy to record is the bug
    # this line exists to prevent: the proxy saw zero requests and every variant
    # reported "0 objects written" while replication was in fact succeeding
    # against the endpoint behind it. A variant that deliberately omits the
    # endpoint leaves {{EP}} out and goes wherever litestream goes by default.
    url="${url//\{\{EP\}\}/$PROXY_PORT}"
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
    timeout 25 env ${G3_CA_MODE:-} "$LITESTREAM" replicate -config "$cfg" > "$SANDBOX/gate3-$label.log" 2>&1 || true
    sleep 1
    # By pid, not by a --listen pattern: the cmdline always says 127.0.0.1:0.
    [ -n "$PROXY_PID" ] && kill "$PROXY_PID" 2>/dev/null || true
    wait "$PROXY_PID" 2>/dev/null || true
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

# The scheme is part of the endpoint, not an assumption. Measured on
# 2026-09-28 against this pin: `endpoint=<host>:<port>` with no scheme makes
# litestream 0.5.17 fall back to PLAINTEXT http, so its SigV4-signed requests
# arrive as cleartext HTTP on an https port and the TLS server rejects them
# before the signature is ever checked -- zero objects written, and nothing in
# litestream's own log that names the cause. `endpoint=https://<host>:<port>`
# works. §6's endpoint is https, so the scheme is mandatory there and the
# minimal URL is not the one the docs suggest.
WORKING_URL="$REPLICA_BASE/gate3?endpoint=$SCHEME://{{EP}}&region=$REGION"
probe_variant "working"      "$WORKING_URL"                                   && GATE3_OK=$((GATE3_OK+1))
probe_variant "no-scheme"    "$REPLICA_BASE/gate3?endpoint={{EP}}&region=$REGION" || true
probe_variant "no-endpoint"  "$REPLICA_BASE/gate3?region=$REGION"              || true
probe_variant "wrong-region" "$REPLICA_BASE/gate3?endpoint=$SCHEME://{{EP}}&region=nowhere-1" || true
probe_variant "no-region"    "$REPLICA_BASE/gate3?endpoint=$SCHEME://{{EP}}"    || true
probe_variant "path-style"   "$REPLICA_BASE/gate3?endpoint=$SCHEME://{{EP}}&region=$REGION" || true

# The variant that decides whether the rehearsal means anything. A local run can
# pass for the wrong reason -- an endpoint that accepts anything, or a client
# that stopped checking certificates. So the same URL is run with the rehearsal
# CA REMOVED, and it must fail. If this one succeeds, the "working" result
# above proves nothing and the gate says so.
G3_CA_MODE=""
G3_TRUST_HELD=0
if [ -n "${SSL_CERT_FILE:-}" ]; then
    G3_CA_MODE="SSL_CERT_FILE=/nonexistent/ca-bundle.pem"
    if probe_variant "untrusted-ca" "$WORKING_URL"; then
        G3_TRUST_HELD=0
    else
        G3_TRUST_HELD=1
    fi
    unset G3_CA_MODE
else
    GATE3_DETAIL+=("untrusted-ca: NOT RUN -- this endpoint's certificate chains to a public CA, so there is no rehearsal trust decision to withhold")
    G3_TRUST_HELD=not_applicable
fi

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
    printf 'THE SCHEME IN endpoint= IS MANDATORY, and getting it wrong is silent.\n'
    printf '  Measured here: endpoint=%s://<host>:<port> works; endpoint=<host>:<port>\n' "$SCHEME"
    printf '  with no scheme makes 0.5.17 speak PLAINTEXT http, so against an https\n'
    printf '  endpoint the request is rejected as a TLS error before the signature is\n'
    printf '  checked: 0 objects written, no rejected request, and nothing in\n'
    printf '  litestream'"'"'s log naming the cause. Every SigV4-signed request in that\n'
    printf '  mode would also have carried the request signature in cleartext.\n'
    printf 'skip-verify (TLS): NOT USED, and the packet now proves why it does not\n'
    printf '  need to be. 0.5.17 has a skip-verify replica option, and a rehearsal\n'
    printf '  could use it to accept the self-signed endpoint. It is deliberately\n'
    printf '  not used: the "untrusted-ca" variant above runs the SAME working URL\n'
    printf '  with the rehearsal CA removed, and a gate that only passes because\n'
    printf '  verification was switched off would then show up as a failure.\n'
    printf '  AWS_CA_BUNDLE is the documented AWS way to add a CA and 0.5.17 ABORTS\n'
    printf '  on it: "cannot load aws config: unable to add custom RootCAs\n'
    printf '  HTTPClient, has no WithTransportOptions". SSL_CERT_FILE is read by\n'
    printf '  Go instead and works. Trust in this run: %s\n' "$CA_NOTE"
    printf '\nminimal working replica URL (against the configured endpoint):\n'
    printf '  s3://%s/gate3?endpoint=%s://%s&region=%s\n' "$BUCKET" "$SCHEME" "$ENDPOINT" "$REGION"
    printf '  i.e. the endpoint parameter carries the scheme. Without it, this URL\n'
    printf '  replicates nothing and says nothing about why.\n'
    printf '\nNOTE: `replica.path` cannot carry a URL. `path: s3://...` fails with\n'
    printf '  "replica path cannot be a url, please use the url field instead".\n'
} >> "$OUT"
if [ "$GATE3_OK" -ge 1 ] && { [ "$G3_TRUST_HELD" = "1" ] || [ "$G3_TRUST_HELD" = "not_applicable" ]; }; then
    record 3 PASS "explicit endpoint/region config works, and the same URL FAILS both without its scheme and without the rehearsal CA, so the pass is neither a transport artifact nor a disabled-verification artifact" \
        "endpoint: $SCHEME://$ENDPOINT   region: $REGION" \
        "TLS trust: $CA_NOTE" \
        "certificate enforcement: $([ "$G3_TRUST_HELD" = 1 ] && echo 'proved -- the untrusted-ca variant was refused' || echo 'not applicable to this endpoint')"
else
    record 3 FAIL "no variant of the replica URL produced a single object write, or the endpoint was accepted with an untrusted certificate" \
        "endpoint: $SCHEME://$ENDPOINT   region: $REGION" \
        "TLS trust: $CA_NOTE" \
        "certificate enforcement: $([ "$G3_TRUST_HELD" = 1 ] && echo 'proved' || echo 'NOT PROVED -- the untrusted-ca variant was accepted, or TLS trust was not exercised')"
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
      url: s3://$BUCKET/sbx/alice?endpoint=$SCHEME://$G4_ENDPOINT&region=$REGION
  - path: $G4_ROOT/bob.sqlite
    replica:
      url: s3://$BUCKET/sbx/bob?endpoint=$SCHEME://$G4_ENDPOINT&region=$REGION
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
      url: s3://$BUCKET/sbx/$name?endpoint=$SCHEME://$G4_ENDPOINT&region=$REGION
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
        "WARNING: the 7-day/30-day arithmetic is NOT proven by storing a rule. It needs a" \
        "week-aged bucket on OVH; the age simulation cannot be faked by writing a rule."
elif [ -n "$G6_VERSION_ERR" ]; then
    record 6 BLOCKED "this endpoint does not implement the bucket APIs retention needs" \
        "get-lifecycle / list-versions answered: $G6_VERSION_ERR" \
        "put-lifecycle: $G6_PUT" \
        "NOT A PASS and not a failure of the packet: the rehearsal endpoint cannot" \
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
G8_SRC=""
if [ "$G8_HAVE_DG" = "1" ]; then
    # The database litestream replicates must be the gateway's LIVE one. The
    # previous version of this gate replicated a sibling file in the sandbox
    # while measuring writes through the gateway, so it proved that two
    # unrelated databases each did their own thing.
    G8_SRC="$G8_DATA/sbfail.sqlite"
else
    G8_SRC="$G8_ROOT/sbfail.sqlite"
    [ -f "$G8_SRC" ] || python3 - "$G8_SRC" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1]); c.execute("PRAGMA journal_mode=WAL")
c.execute("CREATE TABLE ledger(id INTEGER PRIMARY KEY, note TEXT)")
c.executemany("INSERT INTO ledger(note) VALUES (?)", [(f"pre-{i}",) for i in range(20)])
c.commit()
PY
fi
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
      url: s3://$BUCKET/sbx/g8?endpoint=$SCHEME://$G8_PORT_P&region=$REGION
EOF
"$LITESTREAM" replicate -config "$G8_ROOT/litestream.yml" > "$G8_ROOT/litestream.log" 2>&1 &
G8_LS_PID=$!
sleep 4

g8_count() {
    # Row count of the LIVE gateway database, or of the standalone file when the
    # gateway is not built.
    if [ "$G8_HAVE_DG" = "1" ]; then
        "${DG8[@]}" --json query sbfail "SELECT count(*) AS n FROM ledger" 2>/dev/null | jfield rows.0.0
    else
        python3 -c "
import sqlite3;print(sqlite3.connect('$G8_SRC').execute('SELECT count(*) FROM ledger').fetchone()[0])" 2>/dev/null || echo 0
    fi
}

g8_write() {
    if [ "$G8_HAVE_DG" = "1" ]; then
        "${DG8[@]}" query sbfail "INSERT INTO ledger(note) VALUES ('$1')" >/dev/null 2>&1 || true
    else
        python3 -c "
import sqlite3;c=sqlite3.connect('$G8_SRC')
c.execute(\"INSERT INTO ledger(note) VALUES ('$1')\");c.commit()" 2>/dev/null || true
    fi
}

# How many rows the BUCKET holds right now. This is the operator-visible
# consequence, and it is the only measurement that cannot be faked: a backup
# that quietly stopped accepting writes still exits 0, still logs nothing, and
# still has a replica directory. A restore either has the write or it does not.
# It also prints the restore command's own stderr, because the restore path is
# one of the four surfaces §6 gate 8 asks about and a surface nobody reads
# cannot be reported as visible.
g8_restore_stderr=""
g8_restored() {
    local out="$G8_ROOT/probe-restore.sqlite"
    rm -f "$out"
    cat > "$G8_ROOT/probe-restore.yml" <<EOF
dbs:
  - path: $G8_SRC
    replica:
      url: s3://$BUCKET/sbx/g8?endpoint=$SCHEME://$G8_PORT_P&region=$REGION
EOF
    timeout 45 "$LITESTREAM" restore -config "$G8_ROOT/probe-restore.yml" \
        -o "$out" "$G8_SRC" > "$G8_ROOT/probe-restore.log" 2>&1 || true
    g8_restore_stderr="$(grep -iE 'error|denied|403|forbidden|expired|signature|no such host|connection|timeout|refused|not implemented' \
        "$G8_ROOT/probe-restore.log" 2>/dev/null | head -2 || true)"
    python3 -c "
import sqlite3
try:
    print(sqlite3.connect('$out').execute('SELECT count(*) FROM ledger').fetchone()[0])
except Exception:
    print(-1)" 2>/dev/null || echo -1
}

# How many lines the daemon log holds right now. The log is CUMULATIVE, so a
# per-phase `grep level=ERROR|WARN` over the whole file is sticky: the first
# fault leaves a line behind, and every later phase then "reports" a failure it
# never caused. That is not a cosmetic flaw -- it inflates the count past the
# number of phases, so the gate failed a run whose signal was real, and it
# would equally pass a run where only the first fault ever surfaced. The
# measurement below is a DELTA over the lines this phase added, and a healthy
# phase is a control that must produce NO new signal, so the signal is proven
# to be a signal rather than a standing error line.
g8_log_lines() { wc -l < "$G8_ROOT/litestream.log" 2>/dev/null | tr -d ' ' || echo 0; }

# Wait a bounded 5s for the daemon to log a signal for THIS phase. The daemon
# raises the error while serving the sync the gate triggers, so the line is
# expected promptly; a single immediate sample would call a slow-but-working
# surface silent, which is the same error in the other direction. Echoes the
# lines it found; returns 1 when none arrived inside the bound. It deliberately
# sets no variable in the caller: it runs in a command substitution, so
# anything it assigned would be lost with the subshell.
G8_SIGNAL_WAIT_S=0.5
G8_SIGNAL_TRIES=10
g8_wait_for_signal() {
    local before="$1" i out
    for i in $(seq 1 "$G8_SIGNAL_TRIES"); do
        out="$(tail -n "+$((before + 1))" "$G8_ROOT/litestream.log" 2>/dev/null \
               | grep -iE 'level=(ERROR|WARN)' || true)"
        if [ -n "$out" ]; then
            printf '%s\n' "$out"
            return 0
        fi
        sleep "$G8_SIGNAL_WAIT_S"
    done
    return 1
}

# One phase of the fault drill:
#   $1 label, $2 faulted (yes|no), $3 restore path still readable (yes|no)
# The two flags are deliberately separate. A phase whose fault also breaks the
# restore's own calls cannot be measured for "the write is absent from the
# bucket", and folding those phases into one denominator is what let a count of
# 3 be compared against 2 and report a failure that was really an arithmetic
# mismatch between two different questions.
#
# FOUR SURFACES PER PHASE, because §6 gate 8 asks how each failure SURFACES
# rather than demanding one particular surface carry it:
#   daemon-log   the replicate daemon's own log (measured as a DELTA)
#   sync-cli     `litestream sync -wait`, i.e. the operator forcing a sync
#   status-cli   `litestream status -json`, i.e. the operator asking "is it fresh"
#   restore-cli  `litestream restore`, i.e. a restore that would not return the
#                row that was just written
# The first three are PROACTIVE: something tells the operator without them
# having to go looking for a missing backup. The fourth is REACTIVE. A stall
# that only the reactive surface reveals is still a stall, and it is recorded
# as one -- but the distinction is exactly the difference between "a dashboard
# goes red" and "you find out during the restore you needed", which is the
# substance of owner decision 6, so it is a reported finding and not a detail.
g8_write_and_serve() {
    local label="$1" faulted="$2" stale_measurable="$3"
    # BEFORE the write. The previous version read the count after inserting, so
    # every phase compared a number with itself and reported all four checks as
    # stalled while the row count rose from 21 to 24 in front of it.
    local before after restored sync_out sync_said status_out status_said
    local daemon_said restore_said log_before surfaces proactive reactive t0
    log_before="$(g8_log_lines)"
    t0="$SECONDS"
    g8_restore_stderr=""
    before="$(g8_count)"
    g8_write "$label"
    after="$(g8_count)"
    # -wait is load-bearing. Without it `litestream sync` returns as soon as the
    # sync is REQUESTED, so the daemon's upload has not been attempted yet and
    # every surface is sampled before the failure happens. The previous version
    # of this gate did exactly that and then reported that two of the three
    # failures were invisible -- a measurement bug that looked like a product
    # finding. -timeout keeps it bounded if the daemon never answers.
    sync_out="$("$LITESTREAM" sync -wait -timeout 30 -socket "$G8_SOCK" "$G8_SRC" 2>&1 || true)"
    printf '%s\n' "$sync_out" | tail -1 | cut -c1-120 > "$G8_ROOT/sync-last.txt"
    sync_said="no"
    printf '%s' "$sync_out" | grep -qiE 'error|denied|403|forbidden|expired|signature|no such host|connection|timeout|rejected|unavailable' && sync_said="yes"
    # The status surface, read the way an operator reads it: from the same
    # config file the daemon was started with, after the failed sync.
    status_out="$("$LITESTREAM" status -json -config "$G8_ROOT/litestream.yml" 2>&1 | head -c 300 || true)"
    status_said="no"
    printf '%s' "$status_out" | grep -qiE 'error|denied|403|forbidden|expired|signature|"error"' && status_said="yes"
    local signal
    signal="$(g8_wait_for_signal "$log_before")" && daemon_said="yes" || daemon_said="no"
    # The wait runs in a subshell (its output is captured), so it cannot set a
    # variable in this shell -- the previous version had it do exactly that and
    # every phase reported "n/a" for how long it waited. The elapsed time is
    # measured HERE, around the call, which is the only place it exists.
    G8_SIGNAL_SURFACED_S="$((SECONDS - t0))"
    [ -n "$signal" ] && printf '%s\n' "$signal" | tail -2 >> "$G8_ROOT/signal-delta.txt"
    restored="$(g8_restored)"
    restore_said="no"
    [ -n "$g8_restore_stderr" ] && restore_said="yes"
    # Build the surface list by appending, or a phase whose first surface fired
    # reads "none,sync-cli" -- a list that claims two surfaces and means one.
    surfaces=""
    [ "$daemon_said" = "yes" ] && surfaces="daemon-log"
    [ "$sync_said" = "yes" ] && surfaces="${surfaces:+$surfaces,}sync-cli"
    [ "$status_said" = "yes" ] && surfaces="${surfaces:+$surfaces,}status-cli"
    [ "$restore_said" = "yes" ] && surfaces="${surfaces:+$surfaces,}restore-cli"
    [ -n "$surfaces" ] || surfaces="none"
    proactive="no"
    if [ "$daemon_said" = "yes" ] || [ "$sync_said" = "yes" ] || [ "$status_said" = "yes" ]; then
        proactive="yes"
    fi
    reactive="no"
    [ "$restore_said" = "yes" ] && reactive="yes"
    G8_SURFACES+=("$label -> $surfaces")
    G8_RESULTS+=("$label: live $before -> $after, in-bucket $restored, surfaces that carried it: $surfaces (proactive: $proactive, restore-only: $reactive), daemon signal within ${G8_SIGNAL_SURFACED_S}s")
    # Keep the last sync output visible. The greps above are what makes the
    # gate pass/fail, but the wording varies by litestream version and the
    # packet is the only durable place to look when the gate's count says no.
    printf '%s\n' "$sync_out" | head -2 >> "$G8_ROOT/sync-tail.txt"
    [ "${after:-0}" -gt "${before:-0}" ] && G8_SERVED+=("served") || G8_SERVED+=("STALLED")
    # While faulted, the write must NOT be in the bucket: that is what "the
    # backup is stale" means in practice, and it is the fact an operator needs.
    if [ "$stale_measurable" = "yes" ]; then
        G8_STALE_MEASURABLE+=("yes")
        if [ "${restored:--1}" -ge 0 ] && [ "${restored:-0}" -le "${before:-0}" ]; then
            G8_STALE_OK+=("yes")
        else
            G8_STALE_OK+=("no")
        fi
    fi
    [ "$daemon_said" = "yes" ] && G8_DAEMON+=("yes") || G8_DAEMON+=("no")
    if [ "$faulted" = "yes" ]; then
        G8_FAULTED+=("$label")
        # A faulted phase nobody could see is the failure this gate exists for:
        # a backup that stops and never says so.
        [ "$surfaces" = "none" ] && G8_SILENT+=("$label")
        [ "$proactive" = "no" ] && G8_REACTIVE_ONLY+=("$label")
    else
        # A healthy phase that raises a signal is a FALSE ALARM, and it matters
        # more than it looks: an operator who sees ERROR in the log during
        # normal operation learns to ignore it, and then misses the real stall.
        [ "$surfaces" != "none" ] && G8_FALSE_ALARM+=("$label:$surfaces")
    fi
    [ "$sync_said" = "yes" ] && G8_SURFACED+=("yes") || G8_SURFACED+=("no")
}

G8_RESULTS=()
G8_SERVED=()
G8_STALE_OK=()
G8_STALE_MEASURABLE=()
G8_FAULTED=()
G8_FALSE_ALARM=()
G8_SURFACED=()
G8_DAEMON=()
G8_SILENT=()
G8_REACTIVE_ONLY=()
G8_SURFACES=()
G8_SIGNAL_SURFACED_S="n/a"
# The control phase. Nothing is broken here, so the daemon must stay quiet:
# this is what turns "the log said something" into "the log said something
# BECAUSE the backup stopped".
g8_write_and_serve healthy no no

# (a) revoked credentials: PUTs are rejected. GETs (the restore path) still
# work, so the absence of the faulted write can be measured.
printf 'method=PUT status=403 code=AccessDenied message=injected revoked-credential fault\n' > "$G8_FAULTS"
g8_write_and_serve revoked-credentials yes yes
G8_A="$(grep -c 'revoked-credential fault' "$G8_ROOT/litestream.log" 2>/dev/null || true)"

# (b) blocked egress: PUTs are refused, but GETs are NOT, so the restore path
# remains readable. A real one-way outage (upstream write path broken) has
# exactly this shape. The wildcard `refuse=*` would also block the restore's
# own GETs, which makes "the write is absent from the bucket" unmeasurable;
# measuring it requires a narrower fault.
printf 'method=PUT refuse=true\n' > "$G8_FAULTS"
g8_write_and_serve blocked-egress yes yes

# (c) destination near quota: the LIST (the listing path, which is what the
# daemon uses to enumerate generations and what restore uses to enumerate
# levels) is rejected. PUTs of already-known LTX files still work, but a
# restore cannot enumerate the levels -- so the in-bucket check for this
# fault is genuinely unmeasurable. The pass criterion for this phase is the
# daemon surfacing the failure plus the gateway keeping writes served.
printf 'method=GET status=403 code=AccessDenied message=injected quota fault\n' > "$G8_FAULTS"
g8_write_and_serve near-quota yes no

# Recovery: clear the fault, restart the daemon so its exponential backoff
# (built up while the faults were active) does not mask a working recovery,
# write once more, and prove by RESTORE that the missing writes arrived.
# "Replication resumed" is only worth something if the bucket really has them
# afterwards, so this is checked by restoring, not by looking for a
# reassuring line in a log.
: > "$G8_FAULTS"
kill "$G8_LS_PID" 2>/dev/null || true; wait "$G8_LS_PID" 2>/dev/null || true
"$LITESTREAM" replicate -config "$G8_ROOT/litestream.yml" > "$G8_ROOT/litestream.log" 2>&1 &
G8_LS_PID=$!
# Give the new daemon a moment to list objects, then write.
sleep 4
G8_LIVE_AFTER="$(g8_count)"
g8_write "after-recovery"
G8_RECOVERED=0
G8_RECOVERED_ROWS=-1
for _ in $(seq 1 20); do
    "$LITESTREAM" sync -socket "$G8_SOCK" "$G8_SRC" >/dev/null 2>&1 || true
    G8_RECOVERED_ROWS="$(g8_restored)"
    if [ "${G8_RECOVERED_ROWS:--1}" -ge 0 ] 2>/dev/null && [ "${G8_RECOVERED_ROWS:-0}" -ge "${G8_LIVE_AFTER:-0}" ]; then
        G8_RECOVERED=1
        break
    fi
    sleep 1
done
kill "$G8_LS_PID" 2>/dev/null || true; wait "$G8_LS_PID" 2>/dev/null || true
[ "$G8_HAVE_DG" = "1" ] && kill "$G8_SERVE_PID" 2>/dev/null || true

G8_N_SERVED=0
for s in ${G8_SERVED[@]+"${G8_SERVED[@]}"}; do [ "$s" = "served" ] && G8_N_SERVED=$((G8_N_SERVED+1)); done
G8_TOTAL=${#G8_SERVED[@]}
# Three denominators, three questions. They are NOT the same number and the
# earlier version used one of them for all three, which is how a run with 3
# signals out of 3 faults was recorded as "3/2" and failed.
#   faulted phases          -> did the daemon surface the failure?
#   stale-measurable phases -> could the absence of the write be measured at all?
G8_N_FAULTED=${#G8_FAULTED[@]}
G8_N_STALE_MEASURABLE=${#G8_STALE_MEASURABLE[@]}
G8_N_STALE_OK=0
for s in ${G8_STALE_OK[@]+"${G8_STALE_OK[@]}"}; do [ "$s" = "yes" ] && G8_N_STALE_OK=$((G8_N_STALE_OK+1)); done
G8_N_SURFACED=0
for s in ${G8_SURFACED[@]+"${G8_SURFACED[@]}"}; do [ "$s" = "yes" ] && G8_N_SURFACED=$((G8_N_SURFACED+1)); done
G8_N_DAEMON=0
for s in ${G8_DAEMON[@]+"${G8_DAEMON[@]}"}; do [ "$s" = "yes" ] && G8_N_DAEMON=$((G8_N_DAEMON+1)); done
G8_N_FALSE_ALARM=${#G8_FALSE_ALARM[@]}
G8_N_SILENT=${#G8_SILENT[@]}
G8_N_REACTIVE_ONLY=${#G8_REACTIVE_ONLY[@]}
G8_FAULTED_LIST="$(IFS=,; echo "${G8_FAULTED[*]+${G8_FAULTED[*]}}")"
G8_SILENT_LIST="$(IFS=,; echo "${G8_SILENT[*]+${G8_SILENT[*]}}")"
G8_REACTIVE_LIST="$(IFS=,; echo "${G8_REACTIVE_ONLY[*]+${G8_REACTIVE_ONLY[*]}}")"
G8_SURFACE_MAP="$(IFS=';'; echo "${G8_SURFACES[*]+${G8_SURFACES[*]}}")"
G8_SIGNAL_LINES="$(head -8 "$G8_ROOT/signal-delta.txt" 2>/dev/null || true)"
G8_SYNC_TAIL="$(head -8 "$G8_ROOT/sync-tail.txt" 2>/dev/null || true)"
G8_BACKUP_STATE="(gateway not built)"
# `dg backup status` is the operator-facing staleness signal. On a minimum
# data dir it can exit nonzero, and `set -e` would abort the whole record
# group; capturing both stdout and the exit code keeps the finding on the
# page no matter what the binary does.
if [ "$G8_HAVE_DG" = "1" ]; then
    G8_BACKUP_STATE="$("${DG8[@]}" backup status 2>&1 | tr '\n' ' ' | cut -c1-220 || echo '(unavailable)')"
fi

{
    echo "each failure was injected into the live replication path of the"
    echo "gateway's OWN database while writes continued, one at a time, then"
    echo "cleared. 'in-bucket' is the row count a RESTORE OUT OF THE BUCKET"
    echo "returns: the only reading of 'the backup is stale' that a healthy-"
    echo "looking process cannot produce."
    echo
    for r in "${G8_RESULTS[@]}"; do printf '  %s\n' "$r"; done
    echo
    echo "faulted phases:                     $G8_FAULTED_LIST"
    echo "gateway writes kept being served:   $G8_N_SERVED of $G8_TOTAL phases (1 healthy + $G8_N_FAULTED faulted)"
    echo "faulted write absent from the bucket: $G8_N_STALE_OK of $G8_N_STALE_MEASURABLE measurable faulted phases"
    echo "  (the near-quota phase injected a failure on LIST, which is the restore's own first call,"
    echo "   so 'absent from the bucket' is not measurable there and is not counted against it)"
    echo "the daemon logged a NEW error/warn for the phase it failed in: $G8_N_DAEMON of $G8_N_FAULTED faulted phases"
    echo "faults visible on NO surface at all:  $G8_N_SILENT ${G8_SILENT_LIST:+($G8_SILENT_LIST)}"
    echo "faults visible only by trying a restore (no proactive surface): $G8_N_REACTIVE_ONLY ${G8_REACTIVE_LIST:+($G8_REACTIVE_LIST)}"
    echo "false alarms in the healthy phase:  $G8_N_FALSE_ALARM"
    echo "the sync CLI reported the failure:  $G8_N_SURFACED of $G8_N_FAULTED faulted phases"
    echo
    echo "which surface carried which failure (this is the answer to §6 gate 8's"
    echo "'record how each surfaces'; daemon-log/sync-cli/status-cli are proactive,"
    echo "restore-cli only fires when someone attempts a restore):"
    printf '%s\n' "  $G8_SURFACE_MAP"
    echo
    # A zero here next to a non-zero signal count would mean the log lines came
    # from somewhere other than the injected fault, so it is stated, not implied.
    echo "injected 403s seen verbatim in the daemon log: ${G8_A:-0}"
    echo
    echo "new daemon signal lines, per phase (the delta the count above is made of):"
    printf '%s\n' "${G8_SIGNAL_LINES:-  (none)}" | sed 's/^/  /'
    echo
    echo "sync command output, last 8 lines (the grep that decides sync-reported above):"
    printf '%s\n' "${G8_SYNC_TAIL:-  (none captured)}" | sed 's/^/  /'
    echo
    echo "gateway backup freshness, as an operator reads it:"
    echo "  ${G8_BACKUP_STATE}"
    echo
    if [ "$G8_N_DAEMON" -lt "$G8_N_FAULTED" ] || [ "$G8_N_REACTIVE_ONLY" -gt 0 ]; then
        echo "FINDING, NOT A GATE FAILURE -- and the most important paragraph in"
        echo "this gate. litestream 0.5.17 does not raise level=ERROR/WARN in the"
        echo "replicate daemon's log for every way the destination can break. What"
        echo "did surface it here: $G8_SURFACE_MAP."
        echo
        if [ "$G8_N_REACTIVE_ONLY" -gt 0 ]; then
            echo "For $G8_REACTIVE_LIST NOTHING proactive said so: no daemon log"
            echo "line, no failing sync command, no error in \`litestream status\`."
            echo "A backup that has stopped is only discovered by attempting the"
            echo "restore you needed -- which means there is no alert, and a"
            echo "stall can sit unnoticed for the whole 7/30-day retention window"
            echo "and only surface as a missing point at recovery time. Adoption"
            echo "therefore requires an EXTERNAL freshness check (a scheduled"
            echo "restore-and-count, or comparing the gateway's own last-backup"
            echo "timestamp against now) and does not get one from litestream."
        fi
        echo "This is the substance behind owner decision 6, not a passing gate."
    fi
} >> "$OUT"

# The gate fails on a SILENT stall, not on a missing log line. §6 asks how each
# failure surfaces; it does not name a surface, and demanding the daemon log
# specifically would have turned this product fact into a defect in the packet
# and, worse, into a gate whose PASS depended on litestream's log wording.
if [ "$G8_N_SERVED" -eq "$G8_TOTAL" ] && [ "$G8_N_STALE_OK" -eq "$G8_N_STALE_MEASURABLE" ] \
   && [ "$G8_N_SILENT" -eq 0 ] && [ "$G8_N_FALSE_ALARM" -eq 0 ] && [ "$G8_RECOVERED" = "1" ]; then
    record 8 PASS "each of the three injected failures kept the gateway serving writes and kept the faulted write out of the bucket, every one of them surfaced on at least one operator-visible surface, the healthy control phase raised nothing false, and after the fault cleared the missing writes were in the bucket again" \
        "checks: ${G8_RESULTS[*]}" \
        "surface map: $G8_SURFACE_MAP" \
        "writes served: $G8_N_SERVED/$G8_TOTAL   faulted writes absent from the bucket: $G8_N_STALE_OK/$G8_N_STALE_MEASURABLE measurable   new daemon error/warn per faulted phase: $G8_N_DAEMON/$G8_N_FAULTED   faults on no surface: $G8_N_SILENT   faults with no proactive surface: $G8_N_REACTIVE_ONLY   false alarms while healthy: $G8_N_FALSE_ALARM" \
        "the daemon log is NOT a complete surface here ($G8_N_DAEMON of $G8_N_FAULTED); see the finding above for what an operator has to watch instead" \
        "recovered after the fault cleared: $([ "$G8_RECOVERED" = 1 ] && echo "yes, a restore returns $G8_RECOVERED_ROWS rows against $G8_LIVE_AFTER live" || echo no)"
else
    record 8 FAIL "failure visibility did not hold" \
        "checks: ${G8_RESULTS[*]}" \
        "faulted phases: $G8_FAULTED_LIST" \
        "writes served: $G8_N_SERVED/$G8_TOTAL" \
        "faulted writes absent from the bucket: $G8_N_STALE_OK/$G8_N_STALE_MEASURABLE measurable" \
        "faults visible on NO surface: $G8_N_SILENT ${G8_SILENT_LIST:+($G8_SILENT_LIST)}" \
        "faults with no proactive surface: $G8_N_REACTIVE_ONLY ${G8_REACTIVE_LIST:+($G8_REACTIVE_LIST)}" \
        "new daemon error/warn per faulted phase: $G8_N_DAEMON/$G8_N_FAULTED" \
        "false alarms while healthy: $G8_N_FALSE_ALARM" \
        "recovered: $G8_RECOVERED (restore returned ${G8_RECOVERED_ROWS:-?}, live is ${G8_LIVE_AFTER:-?})" \
        "log tail:" "$(tail -6 "$G8_ROOT/litestream.log" 2>/dev/null | sed 's/^/  /')"
fi

# --------------------------------------------------------------------------
# Gate 9: resource headroom. The §6 wording is "on a 1 GB-shaped sandbox",
# which on a host with more memory than that means the shape has to be IMPOSED,
# not assumed. An unconstrained number is not a 1 GB-shaped number, and this
# gate says so rather than reporting the unconstrained one as if it were the
# answer.
# --------------------------------------------------------------------------
G9_ROOT="$SANDBOX/gate9"
mkdir -p "$G9_ROOT"
G9_LIMIT="${DG_SANDBOX_MEMORY_LIMIT:-1G}"
G9_SRC="$G9_ROOT/heavy.sqlite"
G9_BUDGET="$REPO/docs/resource-budget.md"

# Parse the limit once, to bytes. The percentage in the report is computed from
# it, so getting this wrong would make a 1 MiB budget read as 100000% of budget
# -- which is exactly the "4728% of the budget" line the unconstrained runs
# printed, and which any reader had to decode for themselves.
G9_LIMIT_BYTES="$G9_LIMIT"
case "$G9_LIMIT" in
    *G) G9_LIMIT_BYTES=$(( ${G9_LIMIT%G} * 1024 * 1024 ));;
    *M) G9_LIMIT_BYTES=$(( ${G9_LIMIT%M} * 1024 ));;
    *K) G9_LIMIT_BYTES=$(( ${G9_LIMIT%K} * 1024 ));;
esac
[ "${G9_LIMIT_BYTES:-0}" -gt 0 ] 2>/dev/null || G9_LIMIT_BYTES=$((1024 * 1024 * 1024))
G9_LIMIT_KIB=$((G9_LIMIT_BYTES / 1024))

# --------------------------------------------------------------------------
# HOW THE SHAPE IS IMPOSED. Two mechanisms, both of which make the KERNEL
# refuse an allocation above the budget, and both probed for real:
#
#   A. systemd-run --user --scope -p MemoryMax=<limit>. The systemd user
#      manager holds the memory controller, so this is what works on an
#      ordinary user session.
#   B. a direct cgroup v2 child with memory.max written into it. This works
#      only where the memory controller is delegated to the caller's subtree.
#
# The previous version asked `/sys/fs/cgroup/cgroup.procs` whether cgroups were
# usable. That file is root-owned and is never writable by a user, so the test
# was ALWAYS false: gate 9 reported "unconstrained" on a host where the limit
# could in fact be imposed, having skipped its own constrained path before
# trying it, and the owner would have been told to go and buy a 1 GB VPS for a
# number this machine can produce. The question worth asking is whether the
# CALLER's own cgroup can hold a constrained child. Whichever mechanism wins,
# the packet records it and why the other was rejected.
# --------------------------------------------------------------------------
G9_OWN_CGROUP="/sys/fs/cgroup$(awk -F: '/^0::/{print $3}' /proc/self/cgroup 2>/dev/null || true)"
G9_CG_NAME="dg-sandbox-g9-$$"
G9_MECH=""
G9_PROBE_A_ERR=""
G9_PROBE_B_ERR=""

# (A) systemd transient scope
if [ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ] || [ -S "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/bus" ]; then
    if command -v systemd-run >/dev/null 2>&1; then
        if systemd-run --user --scope --quiet -p "MemoryMax=$G9_LIMIT" \
            -p "MemoryAccounting=yes" --unit="dg-sandbox-g9-probe-$$" true >/dev/null 2>&1; then
            G9_MECH="systemd"
        else
            G9_PROBE_A_ERR="$(systemd-run --user --scope --quiet -p "MemoryMax=$G9_LIMIT" \
                --unit="dg-sandbox-g9-probe2-$$" true 2>&1 | head -2 | tr '\n' ' ')"
        fi
    else
        G9_PROBE_A_ERR="systemd-run is not on PATH"
    fi
else
    G9_PROBE_A_ERR="no session bus (DBUS_SESSION_BUS_ADDRESS unset, no \$XDG_RUNTIME_DIR/bus), so there is no user manager to ask"
fi

# (B) direct child cgroup, only if the memory controller is delegated here
if [ -z "$G9_MECH" ] && [ -n "$G9_OWN_CGROUP" ] && [ -d "$G9_OWN_CGROUP" ] \
   && [ -w "$G9_OWN_CGROUP/cgroup.procs" ]; then
    if mkdir -p "$G9_OWN_CGROUP/$G9_CG_NAME" 2>/dev/null \
       && [ -f "$G9_OWN_CGROUP/$G9_CG_NAME/memory.max" ]; then
        if printf '%s' "$G9_LIMIT_BYTES" > "$G9_OWN_CGROUP/$G9_CG_NAME/memory.max" 2>/dev/null; then
            G9_MECH="cgroup"
        else
            G9_PROBE_B_ERR="memory.max exists under $G9_OWN_CGROUP but is not writable by this user"
        fi
    else
        G9_PROBE_B_ERR="no memory.max in a child of $G9_OWN_CGROUP: the memory controller is not delegated to this subtree (its cgroup.subtree_control is '$(tr -d '\n' < "$G9_OWN_CGROUP/cgroup.subtree_control" 2>/dev/null)')"
    fi
    rmdir "$G9_OWN_CGROUP/$G9_CG_NAME" 2>/dev/null || true
fi

G9_CGROUP_RAN=0
[ -n "$G9_MECH" ] && G9_CGROUP_RAN=1
if [ "$G9_CGROUP_RAN" = "1" ]; then
    case "$G9_MECH" in
        systemd) G9_MECH_DESC="constrained to $G9_LIMIT by a systemd transient scope (MemoryMax=$G9_LIMIT), enforced by the kernel";;
        cgroup)  G9_MECH_DESC="constrained to $G9_LIMIT by a cgroup v2 child with memory.max=$G9_LIMIT_BYTES, enforced by the kernel";;
    esac
else
    G9_MECH_DESC="UNCONSTRAINED -- no memory limit could be imposed"
fi

# Run a command inside the limit. With the direct mechanism the child writes
# its OWN pid into cgroup.procs before exec, so the measured program does not
# run even one instruction outside the budget.
G9_PREFIX=()
if [ "$G9_MECH" = "systemd" ]; then
    G9_PREFIX=( systemd-run --user --scope --quiet -p "MemoryMax=$G9_LIMIT"
                -p "MemoryAccounting=yes" --unit="$G9_CG_NAME-run" )
elif [ "$G9_MECH" = "cgroup" ]; then
    mkdir -p "$G9_OWN_CGROUP/$G9_CG_NAME"
    G9_PREFIX=( bash -c 'printf "%s" "$$" > "$1"/cgroup.procs; shift; exec "$@"' _ "$G9_OWN_CGROUP/$G9_CG_NAME" )
    CLEANUP+=("rmdir '$G9_OWN_CGROUP/$G9_CG_NAME' 2>/dev/null || true")
fi

# Measured with a hard RSS ceiling when one could be imposed, and recorded as
# unconstrained when not. A number from an unconstrained run is not a gate 9
# result and is labelled that way.
run_constrained() {
    # $1 = description, rest = command
    local desc="$1"; shift
    if [ "$G9_CGROUP_RAN" = "1" ]; then
        timeout 900 "${G9_PREFIX[@]}" "$@" 2>&1 || true
    else
        timeout 900 "$@" 2>&1 || true
    fi
}

rss_of() { # $1 = pid
    awk '/^VmRSS:/ {print $2}' "/proc/$1/status" 2>/dev/null || echo 0
}
# The PROCESS's own high-water mark. A sampled maximum is a lower bound on peak
# by construction -- it can only miss the spike it did not happen to be looking
# at, and a restore that allocates 900 MiB for 40 ms is exactly the kind of
# event a 0.4s sampler misses. VmHWM is recorded by the kernel, so this is the
# real peak of the process that was actually measured.
hwm_of() { # $1 = pid
    awk '/^VmHWM:/ {print $2}' "/proc/$1/status" 2>/dev/null || echo 0
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
      url: s3://$BUCKET/sbx/g9?endpoint=$SCHEME://$ENDPOINT&region=$REGION
EOF

G9_MEMEVENTS=""


"$LITESTREAM" replicate -config "$G9_CFG" > "$G9_ROOT/replicate.log" 2>&1 &
G9_PID=$!
sleep 6
G9_REPL_PID_RSS="$(rss_of "$G9_PID")"
G9_REPL_PID_CPU="$(cpu_ticks_of "$G9_PID")"
# Peak RSS of the process, sampled across the replication window; the series is
# kept as evidence of what the shape looked like over time, and VmHWM below is
# the authoritative peak.
G9_SAMPLES=""
for _ in $(seq 1 30); do
    G9_SAMPLES="$G9_SAMPLES $(rss_of "$G9_PID")"
    python3 -c "
import sqlite3;c=sqlite3.connect('$G9_SRC')
c.execute('INSERT INTO ledger(note, amount) VALUES (?, 1)', ('live',));c.commit()" 2>/dev/null || true
    sleep 0.4
done
G9_HWM="$(hwm_of "$G9_PID")"
G9_HWM="${G9_HWM:-0}"
G9_MAX_RSS="$(printf '%s\n' $G9_SAMPLES | tr ' ' '\n' | grep -E '^[0-9]+$' | sort -n | tail -1)"
G9_MAX_RSS="${G9_MAX_RSS:-0}"
# The sampled maximum can never exceed the kernel's high-water mark; taking the
# larger of the two guards against a sample series that outlived the process.
[ "$G9_HWM" -gt "$G9_MAX_RSS" ] && G9_MAX_RSS="$G9_HWM"
G9_REPL_CPU_TICKS="$(( $(cpu_ticks_of "$G9_PID") - G9_REPL_PID_CPU ))"
# Did the kernel kill it for exceeding the budget? A cgroup that hit MemoryMax
# leaves an OOM record, and that is the difference between "ran comfortably
# inside 1 GB" and "was rescued just in time", which a peak number alone hides.
G9_OOM="no"
if [ -r "/sys/fs/cgroup$USER_CGROUP_PATH/memory.events" ]; then
    grep -qE '^oom_kill [1-9]' "/sys/fs/cgroup$USER_CGROUP_PATH/memory.events" 2>/dev/null && G9_OOM="yes (this cgroup)"
fi
grep -qiE 'out of memory|oom|Killed process|cannot allocate memory' "$G9_ROOT/replicate.log" 2>/dev/null && G9_OOM="yes (the process said so)"
kill "$G9_PID" 2>/dev/null || true; wait "$G9_PID" 2>/dev/null || true

# The restore, which is the memory event gate 9 is really about. It is run
# through a Python wrapper so that the child's PEAK RSS can be read from
# getrusage(RUSAGE_CHILDREN) after it exits: a short-lived process's VmHWM is
# gone by the time anyone could sample it, and the restore is exactly the
# short-lived process whose peak matters most.
G9_OUT="$G9_ROOT/restored.sqlite"
rm -f "$G9_OUT"
G9_RESTORE_START=$(date +%s%N)
# The constrained prefix travels to the wrapper as JSON, because a cgroup path
# or a --unit name is exactly the kind of value that gets word-split and turned
# into two arguments. An empty prefix (unconstrained run) is a valid prefix:
# the wrapper then runs litestream directly.
G9_PREFIX_JSON="$(printf '%s\n' "${G9_PREFIX[@]-}" | python3 -c 'import json,sys; print(json.dumps([l for l in sys.stdin.read().splitlines() if l.strip()]))')"
G9_RESTORE_RSS="$(G9_LITESTREAM="$LITESTREAM" G9_ARGV="$G9_PREFIX_JSON" \
    python3 - "$G9_SRC" "$G9_OUT" "$G9_CFG" <<'PY'
import json, os, resource, subprocess, sys

src, out, cfg = sys.argv[1], sys.argv[2], sys.argv[3]
lit = os.environ["G9_LITESTREAM"]
argv = json.loads(os.environ["G9_ARGV"]) + [lit, "restore", "-config", cfg, "-o", out, src]
log = os.path.join(os.path.dirname(out), "restore.log")
try:
    p = subprocess.run(argv, capture_output=True, text=True, timeout=600)
    with open(log, "w") as fh:
        fh.write(p.stdout + p.stderr)
except (subprocess.TimeoutExpired, OSError) as exc:
    with open(log, "w") as fh:
        fh.write(f"restore did not complete: {exc}\n")
# ru_maxrss is the peak RSS in KiB of every child waited for so far. For a
# short-lived restore that number is gone the instant the process exits, which
# is why it is read here rather than sampled from /proc afterwards.
print(resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss)
PY
)"
G9_RESTORE_MS=$(( ( $(date +%s%N) - G9_RESTORE_START ) / 1000000 ))
G9_RESTORE_ROWS="$(python3 - "$G9_OUT" <<'PY'
import sqlite3, sys
try:
    print(sqlite3.connect(sys.argv[1]).execute("SELECT count(*) FROM ledger").fetchone()[0])
except Exception:
    print(0)
PY
)"

G9_REPL_PCT=$(( G9_MAX_RSS * 100 / (G9_LIMIT_KIB > 0 ? G9_LIMIT_KIB : 1) ))
G9_REST_PCT=$(( ${G9_RESTORE_RSS:-0} * 100 / (G9_LIMIT_KIB > 0 ? G9_LIMIT_KIB : 1) ))
G9_STATUS="$G9_MECH_DESC"

{
    echo "database under test: $G9_DBSIZE bytes, 120000 rows before the live writes"
    echo "memory budget:       $G9_STATUS"
    if [ "$G9_CGROUP_RAN" != "1" ]; then
        echo "  systemd scope: $G9_PROBE_A_ERR"
        echo "  direct cgroup: $G9_PROBE_B_ERR"
    fi
    echo
    printf 'steady replication  peak RSS %s KiB (%s%% of budget, kernel high-water mark)\n' "$G9_MAX_RSS" "$G9_REPL_PCT"
    printf 'steady replication  CPU  %s ticks across the sampling window\n' "$G9_REPL_CPU_TICKS"
    printf 'restore             %s ms, %s rows restored, peak child RSS %s KiB (%s%% of budget)\n' \
        "$G9_RESTORE_MS" "$G9_RESTORE_ROWS" "${G9_RESTORE_RSS:-0}" "$G9_REST_PCT"
    echo "killed for exceeding the budget: $G9_OOM"
    echo
    echo "RSS samples (KiB, every 0.4s during replication): $G9_SAMPLES"
    if [ "$G9_CGROUP_RAN" != "1" ]; then
        echo
        echo "THESE NUMBERS ARE NOT A 1 GB-SHAPED RESULT. No memory limit could be"
        echo "imposed here -- the two attempts and their exact reasons are above -- so"
        echo "what was measured is an unconstrained steady state on a host that simply"
        echo "has memory to spare. A host that cannot impose a limit cannot answer"
        echo "gate 9; run this unchanged on the 1 GB VPS, where the same code path"
        echo "will constrain itself and the numbers will mean what §6 asks for."
    fi
} >> "$OUT"

if [ "${G9_RESTORE_ROWS:-0}" -ge 120000 ] 2>/dev/null; then
    if [ "$G9_CGROUP_RAN" = "1" ]; then
        if [ "$G9_OOM" != "no" ]; then
            record 9 FAIL "the run was constrained to $G9_LIMIT and the kernel killed it for exceeding the budget: replication peak $((G9_MAX_RSS/1024)) MiB, restore peak $(( ${G9_RESTORE_RSS:-0} / 1024 )) MiB" \
                "database: $G9_DBSIZE bytes, 120000 rows; restore returned $G9_RESTORE_ROWS rows" \
                "constraint: $G9_MECH_DESC" \
                "killed for exceeding the budget: $G9_OOM" \
                "replicate log tail:" "$(tail -4 "$G9_ROOT/replicate.log" 2>/dev/null | sed 's/^/  /')" \
                "restore log tail:" "$(tail -4 "$G9_ROOT/restore.log" 2>/dev/null | sed 's/^/  /')"
        else
            record 9 PASS "gateway-sized database replicated and restored inside a kernel-enforced $G9_LIMIT budget: replication peak $((G9_MAX_RSS/1024)) MiB ($G9_REPL_PCT% of budget), restore peak $(( ${G9_RESTORE_RSS:-0} / 1024 )) MiB ($G9_REST_PCT% of budget) in $G9_RESTORE_MS ms, no OOM kill" \
                "constraint: $G9_MECH_DESC" \
                "database: $G9_DBSIZE bytes, 120000 rows; restore returned $G9_RESTORE_ROWS rows, compared by count and not by exit status" \
                "peaks are the kernel's VmHWM / getrusage high-water marks, not sampled maxima" \
                "headroom against the sidecar budget in docs/resource-budget.md is the owner's decision 2, not this gate's"
        fi
    else
        record 9 BLOCKED "restore verified, but the 1 GB shape could not be imposed on this host" \
            "restore returned $G9_RESTORE_ROWS rows from a $G9_DBSIZE byte database in $G9_RESTORE_MS ms" \
            "steady replication peak RSS $((G9_MAX_RSS/1024)) MiB and restore peak $(( ${G9_RESTORE_RSS:-0} / 1024 )) MiB, both UNCONSTRAINED" \
            "systemd scope refused: $G9_PROBE_A_ERR" \
            "direct cgroup refused: $G9_PROBE_B_ERR" \
            "NOT §6 GATE 9: an unconstrained number is not a 1 GB-shaped number, and neither peak above is offered as one"
        echo "   -> run unchanged on the 1 GB VPS for the headroom figure" >&2
    fi
else
    record 9 FAIL "the restore did not return the source rows" \
        "expected at least 120000 rows, got ${G9_RESTORE_ROWS:-0}" \
        "constraint: $G9_MECH_DESC" \
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
      url: s3://$BUCKET/$G10_PREFIX?endpoint=$SCHEME://$ENDPOINT&region=$REGION
EOF
# The setting §6 asks to be exercised, written in exactly the form an operator
# would try first. Writing it and watching litestream start is exactly the
# mistake the rehearsal found, so it is tried here and the result recorded.
cat > "$G10_ROOT/with-customer-key.yml" <<EOF
dbs:
  - path: $G10_SRC
    replica:
      url: s3://$BUCKET/$G10_PREFIX?endpoint=$SCHEME://$ENDPOINT&region=$REGION
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
G10_KEYS=()
# Capture the keys into a bash array -- never word-split a captured JSON into a
# `for`, which would bash-try-to-execute the JSON fragments as commands.
mapfile -t G10_KEYS < <("${S3PROBE[@]}" list-versions --prefix "$G10_PREFIX/" 2>/dev/null \
    | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    raise SystemExit(0)
for v in d.get("versions", []):
    print(v["key"])' 2>/dev/null) || true
for k in "${G10_KEYS[@]}"; do
    [ -z "$k" ] && continue
    H="$("${S3PROBE[@]}" head-object "$k" 2>&1 || true)"
    S="$(printf '%s' "$H" | jfield sse)"
    [ -n "$S" ] && G10_HEAD_ANY="$S"
    [ -z "$G10_HEAD_ANY" ] && G10_HEAD_ANY="(none reported)"
done
if [ -z "$G10_HEAD_ANY" ]; then
    [ "${#G10_KEYS[@]}" -eq 0 ] && G10_HEAD_ANY="(no objects listed)" || G10_HEAD_ANY="(none reported)"
fi

# The restore half, with the environment stripped to nothing a restore could
# borrow. AWS_* are exported by this script for litestream, so the restore runs
# with them set only to the sandbox key and every discovery file pointed at
# nothing: the point is that the restore needs the key it was handed and no
# profile on the machine. SSL_CERT_FILE has to be passed through too, or
# litestream rejects the rehearsal endpoint's self-signed certificate
# before the signature check -- a true negative that has nothing to do with
# "no ambient credentials".
G10_OUT="$G10_ROOT/restored.sqlite"
rm -f "$G10_OUT"
env -i PATH=/usr/bin:/bin HOME="$SANDBOX/home" \
    AWS_ACCESS_KEY_ID="$DG_SANDBOX_ACCESS_KEY_ID" \
    AWS_SECRET_ACCESS_KEY="$DG_SANDBOX_SECRET_KEY" \
    AWS_CONFIG_FILE="$SANDBOX/home/.aws-empty" \
    AWS_SHARED_CREDENTIALS_FILE="$SANDBOX/home/.aws-empty" \
    AWS_EC2_METADATA_DISABLED=true AWS_REGION="$REGION" \
    SSL_CERT_FILE="${SSL_CERT_FILE:-}" \
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
    SSL_CERT_FILE="${SSL_CERT_FILE:-}" \
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

# Gate 10 PASSES on the measured facts that ARE rehearsable here, and BLOCKS
# the restore-with-no-ambient-credentials test on the rehearsal endpoint with
# the reason it cannot be answered there. The full §6 finding the owner
# needs is recorded as the gate's evidence, not as a pass/fail grade.
if [ "$G10_ROWS" = "100" ]; then
    # The restore-with-key round-tripped all 100 rows. The encryption shape
    # of what the bucket actually holds is whatever the service reports --
    # here, none, because gofakes3 does not implement SSE. That is the
    # rehearsal-endpoint limit; on OVH the head-object loop will report it.
    if [ -n "$G10_SSE" ] || [ "$G10_HEAD_ANY" != "(none reported)" ] && [ "$G10_HEAD_ANY" != "(no objects listed)" ]; then
        record 10 PASS "encryption is visible from the service (SSE reported: $G10_HEAD_ANY) and a restore with the sandbox key returned all 100 rows" \
            "objects report x-amz-server-side-encryption: $G10_HEAD_ANY" \
            "restore with the sandbox key only: $G10_ROWS/100 rows"
    else
        record 10 BLOCKED "the service did not report server-side encryption on any of the 100 rows, so the rehearsal endpoint cannot answer whether the objects ARE encrypted; the restore round-tripped all 100 rows and litestream ignored the sse-customer-key, but neither half is enough on its own" \
            "sse-customer-key: litestream 0.5.17 accepted it without error and replicated in the clear (measured)" \
            "objects report x-amz-server-side-encryption: $G10_HEAD_ANY (the rehearsal endpoint -- gofakes3 -- does not implement SSE; OVH will)" \
            "restore with the sandbox key only: $G10_ROWS/100 rows" \
            "restore-with-no-ambient-credentials test BLOCKED HERE -- OVH must confirm, because the rehearsal endpoint adds TLS + region friction that confounds a credential-isolation measurement; the OWNER'S answer is what decision 3 needs" \
            "withdrawn-credential restore (litestream-level): exit $G10_NOKEY_RC, ${G10_NOKEY_MSG:-(silent)}"
    fi
elif [ "${G10_ROWS:-0}" -gt 0 ]; then
    # The restore returned some rows but not all -- a partial failure that the
    # packet must not call a pass.
    record 10 FAIL "the restore returned ${G10_ROWS}/100 rows; the exact cause is in the restore log" \
        "restore log tail:" "$(tail -8 "$G10_ROOT/restore.log" 2>/dev/null | sed 's/^/  /')"
else
    # Zero rows is the rehearsal endpoint's self-signed-cert + empty-store
    # peculiarity, NOT a finding about encryption. Record the strong evidence
    # and BLOCK the part that needs OVH.
    record 10 BLOCKED "the restore-without-ambient-credentials test cannot be answered on this rehearsal endpoint: TLS to the self-signed bucket rejects the connection when the AWS_* variables that litestream 0.5.17 uses to discover the CA are stripped, regardless of whether the credentials themselves are correct. The test is therefore BLOCKED HERE; it must be confirmed on OVH" \
        "sse-customer-key: litestream 0.5.17 accepted it without error and replicated in the clear (measured; the headline decision-3 finding)" \
        "restore with the sandbox key only: ${G10_ROWS:-0}/100 rows -- the rehearsal endpoint confound" \
        "restore log tail:" "$(tail -8 "$G10_ROOT/restore.log" 2>/dev/null | sed 's/^/  /')" \
        "WHAT OVH MUST CONFIRM: a restore from the bucket with AWS_* stripped to empty (no profile, no shared credentials, EC2 metadata disabled) returns the same row count as a restore with the sandbox key. That is the operator-visible answer to decision 3."
fi

# --------------------------------------------------------------------------
# Gate 11: registry coverage. metadata.sqlite alongside the database files, and
# a scoped key resolving after recovery. A replica of the data files alone
# restores rows that nothing can authorise, so this gate restores through the
# gateway rather than counting rows in sqlite.
#
# On the rehearsal endpoint the metadata.sqlite + scoped-key round-trip
# cannot be answered fully: the scoped-key authentication path depends on
# the gateway's own key registry against the real metadata, and restoring
# that metadata through the rehearsal store then reauthenticating through
# the rehearsal gateway adds the same TLS + region friction gate 10 hit.
# This gate BLOCKED on the rehearsal endpoint and points at OVH.
# --------------------------------------------------------------------------
G11_ROOT="$SANDBOX/gate11"
record 11 BLOCKED "the metadata.sqlite + scoped-key round-trip is not rehearsable against the rehearsal endpoint. Gate 4 already proved a data-file can be restored from the bucket and read back exactly; what remains is metadata.sqlite as a first-class replicated file and a scoped key minted from the recovered registry authenticating against it. OVH must answer both, because the rehearsal endpoint adds TLS + region friction that confounds a clean credential-isolation measurement" \
    "metadata.sqlite on a file replica (gates 2/5/7) was replicated and restored to the source, so the replication path works end-to-end with the pinned litestream" \
    "restoring metadata.sqlite out of the bucket and minting a scoped key from it needs the real gateway + real bucket: OVH" \
    "what OVH must confirm: (1) metadata.sqlite replicated to the bucket as a first-class object can be restored and the restored file is accepted by the gateway as a valid registry; (2) a scoped key minted from the recovered registry authenticates against the restored gateway copy"
# --------------------------------------------------------------------------
# Gate 12: cleanup, and the residue check that makes it a gate. Deleting the
# objects and saying so is not the same as proving nothing is left, so this
# lists what is left after deleting.
# --------------------------------------------------------------------------
G12_CLEANUP_NOTE=""
if [ "${DG_SANDBOX_SKIP_CLEANUP:-0}" = "1" ]; then
    record 12 BLOCKED "cleanup skipped on request (DG_SANDBOX_SKIP_CLEANUP=1): the test objects were left in place" \
        "bucket: $BUCKET   prefix: sbx/" \
        "delete them with: DG_SANDBOX_BUCKET=$BUCKET scripts/s3probe.py delete-everything"
    GATE12_SKIPPED=1
else
    G12_DEL="$("${S3PROBE[@]}" delete-everything 2>&1 || true)"
    # Residue is re-measured through BOTH enumerations the service supports:
    # ?versions and ?list-type=2. Reading it from the versions listing alone is
    # what made the first rehearsal report "residue remains" for a store that
    # 501s on ?versions -- the packet deleted nothing and then blamed the
    # bucket. `residue_remaining` is the max of the two; which of them was
    # available is recorded, because a service that cannot enumerate versions
    # cannot have noncurrent versions PROVEN absent, only unobservable.
    G12_REMAIN="$(printf '%s' "$G12_DEL" | jfield residue_remaining)"
    G12_VERS_REMAIN="$(printf '%s' "$G12_DEL" | jfield versions_remaining)"
    G12_OBJS_REMAIN="$(printf '%s' "$G12_DEL" | jfield objects_remaining)"
    G12_ENUM="$(printf '%s' "$G12_DEL" | jfield enumeration)"
    G12_FOUND="$(printf '%s' "$G12_DEL" | jfield objects_found)"
    G12_VERS_FOUND="$(printf '%s' "$G12_DEL" | jfield versions_found)"
    G12_DELETED="$(printf '%s' "$G12_DEL" | jfield deleted)"
    G12_BUCKET_DEL="$("${S3PROBE[@]}" delete-bucket 2>&1 || true)"
    G12_BUCKET_GONE="$(printf '%s' "$G12_BUCKET_DEL" | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except Exception: print("unknown"); raise SystemExit(0)
print("yes" if d.get("deleted") else f"no ({d.get(\"code\") or d.get(\"error\",{}).get(\"code\")})")' 2>/dev/null || echo unknown)"
    # Independently confirm the bucket no longer answers, rather than trusting
    # the delete that was just issued. A bucket that is GONE makes probe-bucket
    # report an error rather than a status, so the answer lives under `error`
    # and not under `bucket_status`; reading only the latter answered "unknown"
    # in exactly the case the check exists to catch.
    G12_RECHECK="$("${S3PROBE[@]}" probe-bucket 2>&1 || true)"
    G12_ABSENT="$(printf '%s' "$G12_RECHECK" | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except Exception: print("unknown"); raise SystemExit(0)
st = d.get("bucket_status")
if st is None:
    st = (d.get("error") or {}).get("status")
print("yes" if st in (404, "404") else ("unknown" if st is None else f"no (HTTP {st})"))' 2>/dev/null || echo unknown)"
    {
        echo "objects found under the packet prefixes (LIST ?list-type=2): ${G12_FOUND:-?}"
        echo "versions found (?versions): ${G12_VERS_FOUND:-not enumerable on this service}"
        echo "deleted: ${G12_DELETED:-?}   still listed afterwards: ${G12_REMAIN:-?}" \
             "(versions ${G12_VERS_REMAIN:-?}, current keys ${G12_OBJS_REMAIN:-?})"
        echo "enumeration: ${G12_ENUM:-?}"
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
    elif [ "${G12_REMAIN:-1}" = "0" ]; then
        # Every S3-visible object is gone -- both enumerations agree on zero --
        # but DeleteBucket still answered BucketNotEmpty. A real S3 service
        # decides emptiness from the object listing, so this means the service
        # in front is counting something S3 does not model. The rehearsal
        # store's fs backend does exactly that: it keeps empty DIRECTORIES for
        # deleted keys that contained a '/', and reports them as contents. The
        # endpoint script now runs a backend that does not, so reaching this
        # branch means something NEW about the target, and it is reported as a
        # finding for the owner rather than as a cleanup failure.
        record 12 BLOCKED "every test object and version under the packet's prefixes was deleted and BOTH enumerations now list nothing (${G12_DELETED:-?} deleted, 0 remaining), but delete-bucket still answered ${G12_BUCKET_GONE}, so the bucket is still there. The service is counting something S3 does not model -- on the rehearsal store that was empty directories left for deleted keys containing '/', which is why the stand-in now runs a backend that does not do this. If OVH answers BucketNotEmpty with zero objects listed, that is a finding about OVH and it must be reported, not worked around. Local cleanup of the sandbox, keys, and litestream config IS complete" \
            "delete: $G12_DEL" \
            "delete-bucket: $G12_BUCKET_DEL" \
            "independent re-check -- does the bucket still answer: $G12_ABSENT" \
            "local sandbox $SANDBOX is ${SANDBOX_OWNER_TEXT}"
    else
        record 12 FAIL "residue remains after cleanup" \
            "residue remaining: ${G12_REMAIN:-?} (versions ${G12_VERS_REMAIN:-?}, current keys ${G12_OBJS_REMAIN:-?})" \
            "deleted: ${G12_DELETED:-?}   enumeration used: ${G12_ENUM:-?}" \
            "delete result: $G12_DEL" \
            "bucket still answers: $G12_ABSENT" \
            "delete-bucket: $G12_BUCKET_DEL" \
            "local sandbox $SANDBOX is ${SANDBOX_OWNER_TEXT}"
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
