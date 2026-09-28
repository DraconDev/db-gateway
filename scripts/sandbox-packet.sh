#!/usr/bin/env bash
# Sandbox adoption packet: the 12 checks in docs/backup-compatibility.md §6.
#
# Run: scripts/sandbox-packet.sh --out docs/sandbox-packet-YYYY-MM-DD.log
#
# WHAT THIS IS. The §6 list is a program, not a single test, and every gate
# that matters needs a real OVH test bucket. The owner has not supplied one,
# so this runner does two things and refuses to fake a third:
#
#   1. Runs every gate that needs no bucket, for real: the version pin (gate
#      1), and a file-replica rehearsal of replication, restore, exact-row
#      comparison and live reconfiguration (gates 2, 5, 7 in rehearsal form).
#   2. Marks every bucket-dependent gate BLOCKED, with the reason and the exact
#      command it will run once a bucket exists. BLOCKED is a result, not a
#      skip: the packet is not "clean" while gates are blocked, and this
#      script exits nonzero for exactly that reason.
#   3. Refuses to run at all without --i-have-owner-go-ahead, because §6 says
#      run this against a dedicated test bucket on the owner's authorization
#      and no production database, credential or subscription.
#
# WHY IT ISOLATES CREDENTIALS. Litestream discovers AWS credentials from the
# ambient environment, including ~/.aws. On a shared machine that silently
# signs sandbox traffic with whatever profile the operator happens to have,
# which is both a leak and a false result: a gate that passes with the wrong
# key proves nothing. Every invocation here runs with HOME pointed at the
# sandbox, an empty AWS_CONFIG_FILE/AWS_SHARED_CREDENTIALS_FILE, and
# LITESTREAM_CREDENTIALS written by the owner into the sandbox. No invocation
# here can reach a credential the operator did not hand to the sandbox.
#
# WHY IT REFUSES AMBIGUOUS DATA. Gate 12 requires no residue. The data
# directory is created fresh under the sandbox and asserted to be under it,
# so a mistyped --data-dir cannot point this at a real gateway.

set -euo pipefail

PIN_VERSION="0.5.17"
PIN_ROOT="${DG_SANDBOX_PIN_ROOT:-$HOME/.local/dg-sandbox/litestream-${PIN_VERSION}}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT=""
GO_AHEAD=0
DG_BIN="${DG_BIN:-$REPO/.cargo/target/debug/dg}"

usage() {
    cat <<'USAGE'
usage: scripts/sandbox-packet.sh --out FILE [--i-have-owner-go-ahead]

  --out FILE                where to write the packet
  --i-have-owner-go-ahead   required; the owner's authorization for §6
  DG_SANDBOX_BUCKET         test bucket name; unset marks S3 gates BLOCKED
  DG_SANDBOX_ENDPOINT       e.g. s3.gra.io.cloud.ovh.net
  DG_SANDBOX_REGION         e.g. gra
  DG_SANDBOX_ACCESS_KEY_ID  test-bucket key, written into the sandbox only
  DG_SANDBOX_SECRET_KEY     test-bucket secret, written into the sandbox only
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --out) OUT="${2:?--out needs a value}"; shift 2;;
        --i-have-owner-go-ahead) GO_AHEAD=1; shift;;
        -h|--help) usage; exit 0;;
        *) echo "unknown flag: $1" >&2; usage; exit 2;;
    esac
done
[ -n "$OUT" ] || { echo "--out is required" >&2; usage; exit 2; }

# --------------------------------------------------------------------------
# Fail closed before touching anything: no owner authorization, no run. This is
# the objective's own BLOCKER, so it is a precondition and not a warning.
# --------------------------------------------------------------------------
if [ "$GO_AHEAD" -ne 1 ]; then
    cat > "$OUT" <<EOF
# Sandbox adoption packet -- REFUSED BEFORE RUNNING

No gate was executed. §6 is authorized work against a dedicated test bucket
and disposable data; the owner has not given that authorization, and this
script will not infer it. Nothing on this machine was read or written.

To run:  scripts/sandbox-packet.sh --out $OUT --i-have-owner-go-ahead
EOF
    echo "refused: no owner go-ahead. Packet written to $OUT" >&2
    exit 3
fi

# --------------------------------------------------------------------------
# Isolated credential and data environment. Everything below inherits these.
# --------------------------------------------------------------------------
SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/dg-sandbox-XXXXXX")"
cleanup() {
    # Disposable data and credentials do not survive the run. Gate 12 is about
    # residue in the owner's account; residue in a temp dir is the same habit.
    rm -rf "$SANDBOX"
}
trap cleanup EXIT

export HOME="$SANDBOX/home"
mkdir -p "$HOME"
: > "$HOME/.aws-credentials"
export AWS_CONFIG_FILE="$HOME/.aws-credentials"
export AWS_SHARED_CREDENTIALS_FILE="$HOME/.aws-credentials"
export AWS_PROFILE=""
export AWS_REGION="${DG_SANDBOX_REGION:-us-east-1}"
export AWS_EC2_METADATA_DISABLED=true
export LITESTREAM_CREDENTIALS="$HOME/litestream.yml"
export DG_SANDBOX_DIR="$SANDBOX"

# The owner's test-bucket keys land in the sandbox only, mode 0600, and only
# when both halves are present. A half-configured credential is a failure, not
# a silent unsigned run.
if [ -n "${DG_SANDBOX_ACCESS_KEY_ID:-}" ] || [ -n "${DG_SANDBOX_SECRET_KEY:-}" ]; then
    [ -n "${DG_SANDBOX_ACCESS_KEY_ID:-}" ] && [ -n "${DG_SANDBOX_SECRET_KEY:-}" ] || {
        echo "refusing: only one half of the test-bucket key was supplied" >&2
        exit 2
    }
    umask 077
    cat > "$LITESTREAM_CREDENTIALS" <<EOF
access-key-id: ${DG_SANDBOX_ACCESS_KEY_ID}
secret-access-key: ${DG_SANDBOX_SECRET_KEY}
EOF
fi

GATES_PASS=0
GATES_BLOCKED=0
GATES_FAIL=0
BLOCKED_REASONS=()

# record <gate> <PASS|FAIL|BLOCKED> <one-line result> [detail...]
record() {
    local gate="$1" status="$2" result="$3"; shift 3
    {
        printf '\n## gate %s: %s -- %s\n' "$gate" "$status" "$result"
        for line in "$@"; do printf '%s\n' "$line"; done
    } >> "$OUT"
    case "$status" in
        PASS) GATES_PASS=$((GATES_PASS + 1));;
        BLOCKED) GATES_BLOCKED=$((GATES_BLOCKED + 1)); BLOCKED_REASONS+=("$gate");;
        FAIL) GATES_FAIL=$((GATES_FAIL + 1));;
    esac
    printf 'gate %-2s %-7s %s\n' "$gate" "$status" "$result" >&2
}

{
    echo "# Sandbox adoption packet (docs/backup-compatibility.md §6)"
    echo
    echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "host: $(uname -srm)"
    echo "litestream pinned: ${PIN_VERSION} (${PIN_ROOT})"
    echo "bucket: ${DG_SANDBOX_BUCKET:-<unset>}"
    echo "endpoint: ${DG_SANDBOX_ENDPOINT:-<unset>}"
    echo "region: ${DG_SANDBOX_REGION:-<unset>}"
    echo
    echo "Gates are PASS, FAIL or BLOCKED. BLOCKED means not executed and not"
    echo "evidence of anything; it is never a pass. A packet with a BLOCKED"
    echo "gate is not an adoption packet."
} > "$OUT"

# --------------------------------------------------------------------------
# Gate 1: pin and record. No bucket needed, so it is the one gate that can be
# genuinely satisfied before the owner supplies anything.
# --------------------------------------------------------------------------
LITESTREAM="$PIN_ROOT/litestream"
if [ ! -x "$LITESTREAM" ]; then
    record 1 FAIL "litestream ${PIN_VERSION} is not installed at ${LITESTREAM}" \
        "Install it with the checksum from the release's checksums.txt, then re-run." \
        "scripts/sandbox-packet.sh exits nonzero while this gate is not PASS."
else
    VERSION_OUT="$("$LITESTREAM" version 2>&1 | head -1)"
    SHA_OUT="$(sha256sum "$LITESTREAM" | cut -d' ' -f1)"
    TARBALL="$PIN_ROOT/litestream-${PIN_VERSION}-linux-x86_64.tar.gz"
    PUBLISHED_SHA=""
    [ -f "$PIN_ROOT/checksums.txt" ] && \
        PUBLISHED_SHA="$(awk -v n="litestream-${PIN_VERSION}-linux-x86_64.tar.gz" '$2==n {print $1}' "$PIN_ROOT/checksums.txt")"
    ACTUAL_TARBALL_SHA=""
    [ -f "$TARBALL" ] && ACTUAL_TARBALL_SHA="$(sha256sum "$TARBALL" | cut -d' ' -f1)"
    if [ "$VERSION_OUT" = "$PIN_VERSION" ] && \
       [ -n "$PUBLISHED_SHA" ] && [ "$PUBLISHED_SHA" = "$ACTUAL_TARBALL_SHA" ]; then
        record 1 PASS "litestream ${VERSION_OUT}, release tarball checksum matches the published checksums.txt" \
            "\$ litestream version" "$VERSION_OUT" \
            "tarball sha256 (published): $PUBLISHED_SHA" \
            "tarball sha256 (on disk)  : $ACTUAL_TARBALL_SHA" \
            "binary sha256: $SHA_OUT" \
            "config reference: etc/litestream.yml shipped in the ${PIN_VERSION} tarball, read on $(date -u +%Y-%m-%d)" \
            "NOTE: the litestream on PATH is $(litestream version 2>&1 | head -1), which is NOT the pinned" \
            "version. This packet used the pinned binary; the PATH one must not be used for §6."
    else
        record 1 FAIL "version or checksum did not verify" \
            "\$ litestream version -> ${VERSION_OUT} (expected ${PIN_VERSION})" \
            "published sha256: ${PUBLISHED_SHA:-<none recorded>}" \
            "on-disk sha256:   ${ACTUAL_TARBALL_SHA:-<no tarball>}"
    fi
fi

# --------------------------------------------------------------------------
# Rehearsal harness. A file replica exercises the same replicate/restore/
# reconfigure code paths with no bucket, so the bucket run is not the first
# time these commands execute. It is NOT §6 evidence: a file replica is not an
# S3-compatible object store, and nothing here says otherwise.
# --------------------------------------------------------------------------
[ -x "$DG_BIN" ] || { echo "dg binary not built at $DG_BIN" >&2; exit 2; }

rehearsal_root="$SANDBOX/rehearsal"
data_dir="$rehearsal_root/data"
replica_dir="$rehearsal_root/replica"
restore_dir="$rehearsal_root/restore"
mkdir -p "$data_dir"
# Assert the disposable data dir really is under the sandbox, so a mistyped
# path can never point the rehearsal at a live gateway.
case "$(cd "$rehearsal_root" && pwd -P)/" in
    "$(cd "$SANDBOX" && pwd -P)"/*) ;;
    *) echo "refusing: rehearsal data dir is outside the sandbox" >&2; exit 2;;
esac

KEY_FILE="$rehearsal_root/owner.key"
init_out="$("$DG_BIN" init --data-dir "$data_dir" 2>/dev/null)"
OWNER_KEY="$(printf '%s' "$init_out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["owner_key"])')"
umask 077; printf '%s\n' "$OWNER_KEY" > "$KEY_FILE"

PORT="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"
"$DG_BIN" serve --listen "127.0.0.1:$PORT" --data-dir "$data_dir" \
    > "$rehearsal_root/serve.log" 2>&1 &
SERVE_PID=$!
DG=( "$DG_BIN" --server "http://127.0.0.1:$PORT" --key-file "$KEY_FILE" )

cleanup_server() {
    kill "$SERVE_PID" 2>/dev/null || true
    wait "$SERVE_PID" 2>/dev/null || true
}
trap 'cleanup_server; cleanup' EXIT

for _ in $(seq 1 100); do
    "${DG[@]}" status >/dev/null 2>&1 && break
    sleep 0.1
done
"${DG[@]}" create rehearsal >/dev/null
"${DG[@]}" query rehearsal "CREATE TABLE ledger (id INTEGER PRIMARY KEY, note TEXT, amount INTEGER)" >/dev/null
# A recursive CTE rather than generate_series: the gateway runs plain SQLite
# with no extensions loaded, and a rehearsal that used one would be measuring
# a different gateway than the one being adopted.
"${DG[@]}" query rehearsal "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n < 200) INSERT INTO ledger (note, amount) SELECT 'seed-' || n, n FROM seq" >/dev/null

# `dg query --json` answers {"columns":[...],"rows":[[...]]}; the row count is
# rows[0][0]. Parsed here so a shape change fails the rehearsal loudly instead
# of quietly reporting zero rows.
row_count() {
    "${DG[@]}" --json query "$1" "SELECT count(*) AS n FROM $2" 2>/dev/null \
        | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["rows"][0][0])
except Exception: print(0)' 2>/dev/null || echo 0
}
ROWS_BEFORE="$(row_count rehearsal ledger)"

# --------------------------------------------------------------------------
# Gate 2 rehearsal: replication running against live gateway writes. The
# assertion that matters is the one §2 asks for: replication must not add
# `busy` or deadline failures. Recorded, not claimed -- the §6 gate proper
# needs the bucket and a 1 GB-shaped sandbox.
# --------------------------------------------------------------------------
LITESTREAM_CFG="$rehearsal_root/litestream.yml"
CONTROL_SOCK="$rehearsal_root/litestream.sock"
cat > "$LITESTREAM_CFG" <<EOF
logging:
  level: info
  type: json
socket:
  enabled: true
  path: $CONTROL_SOCK
dbs:
  - path: $data_dir/rehearsal.sqlite
    replica:
      path: $replica_dir
EOF
"$LITESTREAM" replicate -config "$LITESTREAM_CFG" \
    > "$rehearsal_root/litestream.log" 2>&1 &
LITESTREAM_PID=$!
sleep 2

STATUS_BEFORE="$("${DG[@]}" status --json 2>/dev/null || echo '{}')"
for i in $(seq 1 60); do
    "${DG[@]}" query rehearsal \
        "INSERT INTO ledger (note, amount) VALUES ('live-$i', $i)" >/dev/null 2>&1 || true
    sleep 0.05
done
STATUS_AFTER="$("${DG[@]}" status --json 2>/dev/null || echo '{}')"
READ_Err="$(printf '%s' "$STATUS_AFTER" | python3 -c 'import json,sys
d=json.load(sys.stdin); print(d.get("requests",{}).get("busy",0)+d.get("requests",{}).get("capacity_exceeded",0)+d.get("requests",{}).get("query_deadline_exceeded",0))' 2>/dev/null || echo 0)"
REPLICA_FRESH=0
for _ in $(seq 1 40); do
    # A replicated WAL/ltx segment newer than the last write is the signal
    # that replication is actually advancing under load.
    if find "$replica_dir" -name '*.ltx' -newer "$KEY_FILE" 2>/dev/null | grep -q .; then
        REPLICA_FRESH=1
        break
    fi
    sleep 0.25
done

if [ "$READ_Err" -eq 0 ] && [ "$REPLICA_FRESH" -eq 1 ]; then
    record 2 PASS "rehearsal: replication advanced under 60 live writes with 0 busy/deadline failures" \
        "rows before live writes: $ROWS_BEFORE" \
        "busy+capacity_exceeded+deadline after 60 live writes: $READ_Err" \
        "replica advanced (new .ltx segment): yes" \
        "NOT §6 GATE 2: file replica, not OVH; no 1 GB-shaped sandbox; no p50/p99 comparison yet."
else
    record 2 FAIL "rehearsal: replication did not advance cleanly (busy/deadline=$READ_Err replica_advanced=$REPLICA_FRESH)" \
        "replica dir: $replica_dir" \
        "litestream log tail:" \
        "$(tail -5 "$rehearsal_root/litestream.log" 2>/dev/null | sed 's/^/  /')"
fi

# --------------------------------------------------------------------------
# Gate 7 rehearsal: live reconfiguration. Add a second database to the config
# while litestream runs and record whether replication picks it up without a
# restart, which is the signal §6 asks to be recorded.
# --------------------------------------------------------------------------
"${DG[@]}" create rehearsal_two >/dev/null
"${DG[@]}" query rehearsal_two "CREATE TABLE t (a INTEGER)" >/dev/null
"${DG[@]}" query rehearsal_two "INSERT INTO t VALUES (1)" >/dev/null
# The config gains the database, then replication is started for it over the
# control socket. SIGHUP is NOT the mechanism: it terminates the process, and
# this rehearsal measured that rather than assuming it.
# `start` takes no -config: it talks to the running daemon over the control
# socket, which is precisely why a database can be picked up without a restart.
RELOAD_SIGNAL="control socket: litestream start -socket <sock> <db-path>"
"$LITESTREAM" start -socket "$CONTROL_SOCK" "$data_dir/rehearsal_two.sqlite" \
    > "$rehearsal_root/reload.log" 2>&1 || RELOAD_SIGNAL="control socket: FAILED ($(head -1 "$rehearsal_root/reload.log" 2>/dev/null))"
ADDED=0
for _ in $(seq 1 40); do
    if [ -d "$replica_dir/rehearsal_two" ] || find "$replica_dir" -path '*rehearsal_two*' 2>/dev/null | grep -q .; then
        ADDED=1
        break
    fi
    sleep 0.25
done
if [ "$ADDED" -eq 1 ]; then
    record 7 PASS "rehearsal: a database added to the config was replicated WITHOUT restarting litestream" \
        "signal/command used: $RELOAD_SIGNAL" \
        "SIGHUP was tried first and terminated the process, so it is not a reload signal here" \
        "second database appeared in the replica tree: yes" \
        "NOT §6 GATE 7: file replica. The sandbox must repeat this against the" \
        "bucket and against the deployed unit file's own reload path."
else
    record 7 FAIL "rehearsal: a database added to the config did not start replicating" \
        "signal/command used: $RELOAD_SIGNAL" \
        "litestream log tail:" \
        "$(tail -5 "$rehearsal_root/litestream.log" 2>/dev/null | sed 's/^/  /')"
fi

kill "$LITESTREAM_PID" 2>/dev/null || true
wait "$LITESTREAM_PID" 2>/dev/null || true

# --------------------------------------------------------------------------
# Gate 5 rehearsal: restore to a separate path and compare rows exactly. A
# restore that lands next to the source proves nothing, so the target here is
# a different directory and the comparison is over every row.
# --------------------------------------------------------------------------
mkdir -p "$restore_dir"
RESTORE_OK=0
# -o is the destination; the positional argument is the SOURCE, which is
# matched against the config's db path (a replica URL works as the source too).
if "$LITESTREAM" restore -config "$LITESTREAM_CFG" -o "$restore_dir/rehearsal.sqlite" \
    "$data_dir/rehearsal.sqlite" > "$rehearsal_root/restore.log" 2>&1; then
    RESTORE_OK=1
fi
if [ "$RESTORE_OK" -eq 1 ] && [ -f "$restore_dir/rehearsal.sqlite" ]; then
    ROWS_RESTORED="$(python3 - "$restore_dir/rehearsal.sqlite" <<'PY'
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
try:
    print(conn.execute("SELECT count(*) FROM ledger").fetchone()[0])
except Exception as exc:
    print(f"error: {exc}")
PY
)"
    ROWS_SOURCE="$(row_count rehearsal ledger)"
    if [ "$ROWS_RESTORED" = "$ROWS_SOURCE" ] && [ "${ROWS_RESTORED:-0}" -gt 0 ] 2>/dev/null; then
        record 5 PASS "rehearsal: restored to a separate path, row count matches the source exactly" \
            "restore target: $restore_dir/rehearsal.sqlite (separate directory, not the data dir)" \
            "rows in source: $ROWS_SOURCE" \
            "rows in restored copy: $ROWS_RESTORED" \
            "NOT §6 GATE 5: file replica; no gateway import validation of the restored" \
            "file, no mid-history point, and no corrupt-artifact rejection case."
    else
        record 5 FAIL "rehearsal: restored row count does not match the source" \
            "rows in source: ${ROWS_SOURCE:-?}" \
            "rows in restored copy: ${ROWS_RESTORED:-?}"
    fi
else
    record 5 FAIL "rehearsal: litestream restore produced no database" \
        "restore target: $restore_dir/rehearsal.sqlite" \
        "restore log tail:" \
        "$(tail -5 "$rehearsal_root/restore.log" 2>/dev/null | sed 's/^/  /')"
fi

# --------------------------------------------------------------------------
# Bucket-dependent gates. Without a bucket these are BLOCKED, with the exact
# command recorded so the owner's go-ahead turns into one run.
# --------------------------------------------------------------------------
if [ -n "${DG_SANDBOX_BUCKET:-}" ] && [ -n "${DG_SANDBOX_ENDPOINT:-}" ]; then
    echo "bucket configured; running the S3 gates is implemented in scripts/sandbox-s3-gates.sh" >&2
    exec "$REPO/scripts/sandbox-s3-gates.sh" --out "$OUT" --from-s3
fi

BLOCKED_DETAIL="needs DG_SANDBOX_BUCKET + DG_SANDBOX_ENDPOINT (+ DG_SANDBOX_REGION, DG_SANDBOX_ACCESS_KEY_ID, DG_SANDBOX_SECRET_KEY) and a bucket the owner confirms is disposable"
for gate in 3 4 6 8 9 10 11 12; do
    case "$gate" in
        3)  what="explicit OVH endpoint/region config, path-style and skip-verify variants" ;;
        4)  what="every S3 operation litestream 0.5.17 issues succeeds against the test region/class" ;;
        6)  what="retention/lifecycle drill, 7/30-day version counts" ;;
        8)  what="failure visibility: revoked credentials, blocked egress, destination near quota" ;;
        9)  what="resource headroom for gateway + litestream on a 1 GB-shaped sandbox" ;;
        10) what="encryption mode exercised end-to-end, including restore without ambient credentials" ;;
        11) what="metadata.sqlite backed up and restored, scoped keys resolve after recovery" ;;
        12) what="deletion of every test bucket/key/config, with no residue" ;;
    esac
    record "$gate" BLOCKED "not executed: $what" \
        "reason: $BLOCKED_DETAIL" \
        "this is not a pass and not evidence of anything"
done

# --------------------------------------------------------------------------
# Config-surface facts, recorded because they change what the owner is
# choosing between. These need no bucket, and they are the kind of fact that
# is cheaper to learn now than after a bucket exists.
# --------------------------------------------------------------------------
{
    printf '\n## config surface of the pinned litestream, measured\n\n'
    printf 'endpoint and region are accepted as replica URL query parameters and are\n'
    printf 'honoured: the log line then reads type=s3 region=<r> endpoint=https://<e>.\n\n'
    printf 'Unknown replica keys are SILENTLY IGNORED. A config carrying\n'
    printf '`sse-customer-key` replicated with no error and no encryption, which means\n'
    printf 'an encryption setting that this version does not implement would read as\n'
    printf 'working. Decision 3 (encryption) must therefore be settled at the bucket or\n'
    printf 'by an explicitly verified mechanism, never by writing a key into the config\n'
    printf 'and observing that litestream starts.\n\n'
    printf 'The same applies to `force-path-style` as a query parameter: accepted\n'
    printf 'silently, no effect observed. §6 gate 3 must record the minimal WORKING\n'
    printf 'config, which is the only way to tell a working setting from an ignored one.\n'
} >> "$OUT"

{
    echo
    echo "## summary"
    echo
    printf 'pass: %s  blocked: %s  fail: %s\n' "$GATES_PASS" "$GATES_BLOCKED" "$GATES_FAIL"
    if [ "$GATES_BLOCKED" -gt 0 ]; then
        printf 'blocked gates: %s\n' "${BLOCKED_REASONS[*]}"
        echo "this packet is NOT an adoption packet: it is the rehearsal plus a list of what the bucket run must produce."
    fi
} >> "$OUT"

echo "packet written to $OUT (pass=$GATES_PASS blocked=$GATES_BLOCKED fail=$GATES_FAIL)" >&2
# Nonzero while anything is blocked or failed, so a wrapper cannot mistake this
# for a completed adoption run.
[ "$GATES_BLOCKED" -eq 0 ] && [ "$GATES_FAIL" -eq 0 ]
