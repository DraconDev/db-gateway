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
REHEARSAL=0
DG_BIN="${DG_BIN:-$REPO/.cargo/target/debug/dg}"

usage() {
    cat <<'USAGE'
usage: scripts/sandbox-packet.sh --out FILE [--i-have-owner-go-ahead | --rehearsal]

  --out FILE                where to write the packet
  --i-have-owner-go-ahead   required; the owner's authorization for §6
  --rehearsal               run the S3 gates against a LOOPBACK endpoint with
                            disposable local data, without owner authorization.
                            Refuses if DG_SANDBOX_ENDPOINT is not loopback, and
                            its result is a rehearsal, never §6 evidence.
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
        --rehearsal) REHEARSAL=1; shift;;
        -h|--help) usage; exit 0;;
        *) echo "unknown flag: $1" >&2; usage; exit 2;;
    esac
done
[ -n "$OUT" ] || { echo "--out is required" >&2; usage; exit 2; }

# --------------------------------------------------------------------------
# Fail closed before touching anything: no owner authorization, no run. This is
# the objective's own BLOCKER, so it is a precondition and not a warning.
# --------------------------------------------------------------------------
if [ "$REHEARSAL" = "1" ]; then
    # A rehearsal is authorized by nothing and needs nothing from the owner: it
    # touches a loopback endpoint this script starts state for itself, on
    # disposable data, with keys the operator supplies. The one thing that would
    # need the owner is a NON-loopback endpoint -- that is someone else's
    # account -- so this check is the whole of the rehearsal's safety story and
    # it fails closed.
    case "${DG_SANDBOX_ENDPOINT:-}" in
        127.0.0.1:*|localhost:*|\[::1\]:*|127.0.0.1|localhost) ;;
        *)  cat > "$OUT" <<EOF
# Sandbox adoption packet -- REFUSED BEFORE RUNNING

--rehearsal requires DG_SANDBOX_ENDPOINT to be a LOOPBACK address; it is
'${DG_SANDBOX_ENDPOINT:-<unset>}'. A rehearsal is only safe because it cannot
reach the owner's account, and any other endpoint is a different account.

To run against a real bucket, the owner's authorization is required instead:
  scripts/sandbox-packet.sh --out $OUT --i-have-owner-go-ahead
EOF
            echo "refused: --rehearsal with a non-loopback endpoint ($DG_SANDBOX_ENDPOINT)" >&2
            exit 3
            ;;
    esac
elif [ "$GO_AHEAD" -ne 1 ]; then
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
# Preflight: refuse to run a harness that is known to be broken. Two defects
# shipped in this file and both passed `bash -n`: a quoted argument whose line
# ended without a backslash (so the next line ran as a command, killing the
# packet with exit 127 in the middle of gate 10), and an optional environment
# variable read under `set -u` (which aborted the packet after gates 3, 4, 6 and
# 8 had already been recorded). Neither is visible by reading the script, and
# both were found by running it, so the check is mechanical now.
# --------------------------------------------------------------------------
if [ "${DG_SANDBOX_SKIP_LINT:-0}" != "1" ] && [ -x "$REPO/scripts/lint-packet-scripts.sh" ]; then
    if ! lint_out="$("$REPO/scripts/lint-packet-scripts.sh" 2>&1)"; then
        cat > "$OUT" <<EOF
# Sandbox adoption packet -- REFUSED BEFORE RUNNING

The packet scripts do not pass their own lint, so running them would produce a
packet that dies partway through with a shell error instead of gate results:

$lint_out

Fix the reported lines, or set DG_SANDBOX_SKIP_LINT=1 if you are deliberately
exercising a broken harness.
EOF
        echo "refused: scripts/lint-packet-scripts.sh failed" >&2
        printf '%s\n' "$lint_out" >&2
        exit 2
    fi
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
# The litestream pin lives in the operator's real home, but HOME now points
# inside the sandbox. Anything resolved against $HOME after this line looks for
# the pin in a directory that never had it, which is how the first rehearsal run
# died with "litestream 0.5.17 is not installed" four gates in. Pin the path
# once, against the real home, and hand it to the child.
export DG_SANDBOX_PIN_ROOT="$PIN_ROOT"
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
    if [ "$REHEARSAL" = "1" ]; then
        echo "mode: REHEARSAL against a loopback endpoint (--rehearsal)"
    else
        echo "mode: owner-authorized run against a real endpoint"
    fi
    echo
    echo "Gates are PASS, FAIL or BLOCKED. BLOCKED means not executed and not"
    echo "evidence of anything; it is never a pass. A packet with a BLOCKED"
    echo "gate is not an adoption packet."
    if [ "$REHEARSAL" = "1" ]; then
        echo
        echo "THIS IS A REHEARSAL. It proves the packet drives correct S3 requests,"
        echo "reads the answers, restores the right database and cleans up after"
        echo "itself. It proves nothing about the OVH test region, class, pricing,"
        echo "lifecycle implementation or TLS. Re-run the same script with"
        echo "--i-have-owner-go-ahead and DG_SANDBOX_ENDPOINT pointing at the OVH"
        echo "endpoint to produce §6 evidence."
    fi
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
# ONE REPLICA ROOT PER DATABASE, deliberately. A file replica directory is not
# namespaced per database: with two databases sharing one path their LTX
# segments interleave and a restore for one returns the other's snapshot,
# silently and with exit status 0. An S3 config must give every database its
# own prefix for the same reason, and the first version of this rehearsal
# sharing one directory restored a database with no `ledger` table at all.
mkdir -p "$replica_dir/rehearsal" "$replica_dir/rehearsal_two"
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
      path: $replica_dir/rehearsal
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
# Gate 7 asks a yes/no question and wants the exact command recorded, so this
# measures both documented routes instead of assuming one:
#
#   (a) edit the config, then `litestream start` over the control socket. The
#       running daemon holds its config in memory, so a database added to the
#       FILE is invisible to it: `start` answers "database not found". That is
#       the finding, not a failure of this script.
#   (b) `litestream register -replica <url>` over the same socket, which
#       registers a database with the running daemon without a restart and
#       without editing the config.
#
# SIGHUP is not a reload mechanism here: it terminates the process, and this
# rehearsal measured that rather than assuming it.
cat >> "$LITESTREAM_CFG" <<EOF
  - path: $data_dir/rehearsal_two.sqlite
    replica:
      path: $replica_dir/rehearsal_two
EOF
CONFIG_EDIT_START="(not attempted)"
"$LITESTREAM" start -socket "$CONTROL_SOCK" "$data_dir/rehearsal_two.sqlite" \
    > "$rehearsal_root/start-after-config-edit.log" 2>&1 \
    && CONFIG_EDIT_START="config edit + litestream start: WORKED" \
    || CONFIG_EDIT_START="config edit + litestream start: FAILED -- $(head -1 "$rehearsal_root/start-after-config-edit.log" 2>/dev/null)"

REGISTER_CMD="litestream register -socket <sock> -replica file://<dir> <db-path>"
# -replica must be a URL with a scheme (file:// for a directory), not a bare
# path; the daemon rejects a scheme-less value outright.
"$LITESTREAM" register -socket "$CONTROL_SOCK" -replica "file://$replica_dir/rehearsal_two" \
    "$data_dir/rehearsal_two.sqlite" > "$rehearsal_root/register.log" 2>&1 \
    && REGISTER_RESULT="WORKED" || REGISTER_RESULT="FAILED -- $(head -1 "$rehearsal_root/register.log" 2>/dev/null)"

# A second write after registering, so there is something to replicate: the
# evidence that matters is the daemon making progress for THIS database, and a
# database with no writes can look idle rather than broken.
"${DG[@]}" query rehearsal_two "INSERT INTO t VALUES (2)" >/dev/null 2>&1 || true
# The signal is the daemon's own log for that database. `litestream status`
# and `litestream ltx` both refuse a database that is registered but absent
# from the config file, so neither can be used as a liveness probe here --
# which is itself part of the answer.
ADDED=0
SIGNAL_LINES=""
# Generous window: the first signal for a database with no prior replica state
# can take tens of seconds, and a rehearsal that declares FAIL on a slow log
# line would record a false negative about the tool.
for _ in $(seq 1 120); do
    # `|| true`: an empty grep is the normal state on the first polls, and
    # under `set -e` an assignment that fails would kill the whole packet.
    SIGNAL_LINES="$(grep -F 'rehearsal_two.sqlite' "$rehearsal_root/litestream.log" 2>/dev/null \
        | grep -E 'fetched latest L0|snapshot complete|replicating to|compaction complete' | head -4 || true)"
    if [ -n "$SIGNAL_LINES" ] || find "$replica_dir/rehearsal_two" -name '*.ltx' 2>/dev/null | grep -q .; then
        [ -n "$SIGNAL_LINES" ] || SIGNAL_LINES="(no log line; replica root $replica_dir/rehearsal_two now holds .ltx segments for it)"
        ADDED=1
        break
    fi
    sleep 0.25
done

# The remove side, which §7 also asks for: stop replicating a database and
# confirm the daemon honours it.
UNREGISTER_RESULT="(not attempted)"
if [ "$ADDED" -eq 1 ]; then
    "$LITESTREAM" unregister -socket "$CONTROL_SOCK" "$data_dir/rehearsal_two.sqlite" \
        > "$rehearsal_root/unregister.log" 2>&1 \
        && UNREGISTER_RESULT="litestream unregister: WORKED" \
        || UNREGISTER_RESULT="litestream unregister: FAILED -- $(head -1 "$rehearsal_root/unregister.log" 2>/dev/null)"
fi

if [ "$ADDED" -eq 1 ]; then
    record 7 PASS "rehearsal: a new database was replicated WITHOUT restarting litestream, via the control socket" \
        "route (a) $CONFIG_EDIT_START" \
        "route (b) register over the control socket: $REGISTER_RESULT" \
        "remove side: $UNREGISTER_RESULT" \
        "signal observed in the daemon log:" \
        "$(printf '%s\n' "$SIGNAL_LINES" | sed 's/^/  /')" \
        "ANSWER for §6: the config FILE is read at startup only. Adding a database" \
        "to a running daemon means 'litestream register -replica <url> <db-path>'" \
        "over the control socket (or a restart). SIGHUP is not a reload signal:" \
        "it terminates the process." \
        "NOT §6 GATE 7: file replica. The sandbox must repeat this against the" \
        "bucket and confirm the deployed unit's own socket path and permissions."
else
    record 7 FAIL "rehearsal: no documented route started replication for a new database without a restart" \
        "route (a) $CONFIG_EDIT_START" \
        "route (b) register over the control socket: $REGISTER_RESULT" \
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
# A dedicated single-database config for the restore. The main config has two
# databases by now, and a source chosen out of a multi-db config restored the
# WRONG database without complaining -- so the source is named in a config that
# has exactly one entry rather than relied on to be picked correctly.
cat > "$rehearsal_root/restore.yml" <<EOF
dbs:
  - path: $data_dir/rehearsal.sqlite
    replica:
      path: $replica_dir/rehearsal
EOF
if "$LITESTREAM" restore -config "$rehearsal_root/restore.yml" \
    -o "$restore_dir/rehearsal.sqlite" "$data_dir/rehearsal.sqlite" \
    > "$rehearsal_root/restore.log" 2>&1; then
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
    # The bucket-free half has nothing left to do. Its gateway and its
    # litestream are still running at this point -- they are only killed by the
    # EXIT trap -- so they would be replicating to a file replica for the whole
    # S3 half, inflating the process count the resource gate measures and
    # making "nothing survived this run" impossible to read. Stop them here.
    cleanup_server
    kill "${LITESTREAM_PID:-0}" 2>/dev/null || true
    wait "${LITESTREAM_PID:-0}" 2>/dev/null || true
    echo "bucket configured; handing off to scripts/sandbox-s3-gates.sh" >&2
    # A child, not an exec: `exec` replaces this process, the EXIT trap never
    # runs, and the sandbox holding the owner's test key, the litestream config
    # and every log survives the run. The child deliberately does not delete
    # DG_SANDBOX_DIR -- this process owns it and this trap is what removes it.
    # The child's exit status is this packet's exit status.
    set +e
    DG_SANDBOX_REHEARSAL="$REHEARSAL" "$REPO/scripts/sandbox-s3-gates.sh" --out "$OUT" --from-s3
    S3_STATUS=$?
    set -e
    echo "S3 gates exited $S3_STATUS" >&2
    exit "$S3_STATUS"
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
    printf 'config, which is the only way to tell a working setting from an ignored one.\n\n'
    printf '`litestream restore` can EXIT 0 AND PRODUCE A DATABASE WITH NONE OF THE\n'
    printf 'SOURCE TABLES. Restoring from a bare replica directory ("file://<dir>")\n'
    printf 'succeeded and wrote a 16 KiB file whose tables were all absent. A gate that\n'
    printf 'checked only the exit code would record that as a passing restore. Every\n'
    printf 'restore in §6 gate 5 must compare rows, not statuses.\n\n'
    printf '`litestream status` and `litestream ltx` require the database to be present in\n'
    printf 'the CONFIG FILE, so neither can probe a database that was added with\n'
    printf '`litestream register`. For a database added at runtime the daemon log is the\n'
    printf 'only signal available.\n\n'
    printf 'A REPLICA ROOT IS NOT NAMESPACED PER DATABASE. Two databases sharing one\n'
    printf 'replica path interleave their LTX segments, and `litestream restore` for one\n'
    printf 'then returns the OTHER database snapshot: exit status 0, wrong file. Every\n'
    printf 'S3 config must therefore give each database its own prefix, e.g.\n'
    printf 's3://<bucket>/<db-name>. The gateway holds N databases, so this is not\n'
    printf 'optional bookkeeping; it is what makes gate 5 restore the right database.\n\n'
    printf 'A config file is read at STARTUP. Adding a database to the file while the\n'
    printf 'daemon runs does nothing; `litestream start` then answers "database not\n'
    printf 'found". Adding one at runtime requires `litestream register -socket <sock>\n'
    printf '-replica <url> <db-path>`. SIGHUP is not a reload signal: it terminates the\n'
    printf 'process.\n'
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
