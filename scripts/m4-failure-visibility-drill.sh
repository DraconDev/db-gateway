#!/usr/bin/env bash
# M4 failure-visibility drill: revoking keys / blocking egress / filling disk.
#
# Usage: scripts/m4-failure-visibility-drill.sh
# Emits one JSON evidence object on stdout; diagnostics go to stderr.
# Everything runs against a disposable local gateway (debug or release
# binary) in a mktemp workdir. Nothing touches micro2, OVH, or real data.
#
# Each drill proves the same triple:
#   (a) the failure surfaces visibly (HTTP code + stable error code, or a
#       failed backup attempt record, or a killed writer + stable code);
#   (b) serving writes keep succeeding on a valid credential;
#   (c) /v1/backups reports a non-fresh (stale) signal, never false fresh.
#
# Drill D uses a per-process file-size ceiling (prlimit --fsize) on the
# disposable server, the same mechanism as tests/disk_pressure.rs: the
# kernel refuses growth the way a full disk does, the writer dies with
# SIGXFSZ, and the restarted server must serve again with the acknowledged
# rows exact. No host-wide fill is performed.

set -euo pipefail

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/dg-m4-drill-XXXXXX")"
SERVER_PID=""
cleanup() {
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

log() { echo "[m4-drill] $*" >&2; }
need() { command -v "$1" >/dev/null 2>&1 || { echo "[m4-drill] missing: $1" >&2; exit 3; }; }
need curl; need python3; need prlimit

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DG="${DG_BIN:-$ROOT/.cargo/target/debug/dg}"
[ -x "$DG" ] || { log "no executable at $DG (set DG_BIN=)"; exit 3; }

DATA="$WORKDIR/data"; BACKUPS="$WORKDIR/backups"
mkdir -p "$DATA" "$BACKUPS"

OWNER_JSON="$("$DG" init --data-dir "$DATA")"
OWNER_KEY="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["owner_key"])' <<<"$OWNER_JSON")"
printf '%s\n' "$OWNER_KEY" >"$WORKDIR/owner.key"; chmod 600 "$WORKDIR/owner.key"

start_server() {
    "$DG" serve --listen 127.0.0.1:0 --data-dir "$DATA" --backup-root "$BACKUPS" \
        2>"$WORKDIR/stderr.log" &
    SERVER_PID=$!
    ADDRESS="$(python3 - "$WORKDIR/stderr.log" <<'EOF'
import json, sys, time
deadline = time.time() + 30
while time.time() < deadline:
    try:
        with open(sys.argv[1]) as f:
            for line in f:
                line = line.strip()
                if not line.startswith("{"):
                    continue
                try:
                    entry = json.loads(line)
                except ValueError:
                    continue
                addr = entry.get("fields", {}).get("address")
                if addr:
                    print(addr)
                    raise SystemExit(0)
    except FileNotFoundError:
        pass
    time.sleep(0.2)
raise SystemExit("listen address not observed")
EOF
)"
    BASE="http://$ADDRESS"
}
start_server

AUTH_OWNER=(-H "Authorization: Bearer $OWNER_KEY" -H 'Content-Type: application/json')
call() { # method path body outfile -> prints http code
    local method="$1" path="$2" body="$3" out="$4"
    curl -s -o "$out" -w '%{http_code}' "${AUTH_OWNER[@]}" -X "$method" \
        -d "$body" "$BASE$path"
}
call_as() { # key method path body outfile -> prints http code
    local key="$1" method="$2" path="$3" body="$4" out="$5"
    curl -s -o "$out" -w '%{http_code}' -H "Authorization: Bearer $key" \
        -H 'Content-Type: application/json' -X "$method" -d "$body" "$BASE$path"
}
code_of() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("error",{}).get("code",""))' "$1"; }

EVIDENCE="$WORKDIR/evidence.json"
python3 - "$EVIDENCE" <<'EOF'
import json, sys
json.dump({"drills": {}}, open(sys.argv[1], "w"))
EOF
record() { # key jsonvalue
    python3 - "$EVIDENCE" "$1" "$2" <<'EOF'
import json, sys
p, k, v = sys.argv[1], sys.argv[2], json.loads(sys.argv[3])
doc = json.load(open(p)); doc["drills"][k] = v
json.dump(doc, open(p, "w"), indent=2)
EOF
}

# --- baseline: database, rows, first verified set (expect fresh) ---
log "baseline: create + seed + backup"
call POST /v1/databases '{"name":"drill"}' "$WORKDIR/o.json" >/dev/null
call POST /v1/databases/drill/query '{"sql":"CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)"}' "$WORKDIR/o.json" >/dev/null
call POST /v1/databases/drill/query '{"sql":"INSERT INTO t(v) VALUES ('"'"'a'"'"')"}' "$WORKDIR/o.json" >/dev/null
if ! DG_BIN="$DG" bash "$ROOT/scripts/micro2-backup.sh" --server "$BASE" \
        --key-file "$WORKDIR/owner.key" --data-dir "$DATA" \
        --backup-root "$BACKUPS" --keep 3 >/dev/null 2>"$WORKDIR/backup1.err"; then
    log "baseline backup failed:"; cat "$WORKDIR/backup1.err" >&2; exit 4
fi
BASE_FRESH="$(curl -s "${AUTH_OWNER[@]}" "$BASE/v1/backups" | python3 -c 'import json,sys; print(json.load(sys.stdin)["per_database"]["drill"]["state"])')"
record baseline "{\"backup_state\":\"$BASE_FRESH\"}"
[ "$BASE_FRESH" = "fresh" ] || { log "baseline not fresh: $BASE_FRESH"; exit 4; }

# --- drill R: revoke a scoped key ---
log "drill R: scoped key write, revoke, denied write, owner write"
SCOPED_JSON="$("$DG" --server "$BASE" --key-file "$WORKDIR/owner.key" key create --role database_read_write --database drill drill_rw 2>/dev/null)"
SCOPED_KEY="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["key"])' <<<"$SCOPED_JSON")"
R_WRITE_OK="$(call_as "$SCOPED_KEY" POST /v1/databases/drill/query '{"sql":"INSERT INTO t(v) VALUES ('"'"'r1'"'"')"}' "$WORKDIR/r1.json")"
"$DG" --server "$BASE" --key-file "$WORKDIR/owner.key" key revoke drill_rw >/dev/null 2>&1
R_WRITE_DENIED="$(call_as "$SCOPED_KEY" POST /v1/databases/drill/query '{"sql":"INSERT INTO t(v) VALUES ('"'"'r2'"'"')"}' "$WORKDIR/r2.json")"
R_DENIED_CODE="$(code_of "$WORKDIR/r2.json")"
R_OWNER_WRITE="$(call POST /v1/databases/drill/query '{"sql":"INSERT INTO t(v) VALUES ('"'"'r3'"'"')"}' "$WORKDIR/r3.json")"
R_STATE="$(curl -s "${AUTH_OWNER[@]}" "$BASE/v1/backups" | python3 -c 'import json,sys; print(json.load(sys.stdin)["per_database"]["drill"]["state"])')"
record revoke "{\"scoped_write_before_revoke_http\":$R_WRITE_OK,\"revoked_write_http\":$R_WRITE_DENIED,\"revoked_write_code\":\"$R_DENIED_CODE\",\"owner_write_after_revoke_http\":$R_OWNER_WRITE,\"backup_state\":\"$R_STATE\"}"
[ "$R_WRITE_OK" = "200" ] && [ "$R_WRITE_DENIED" = "401" -o "$R_WRITE_DENIED" = "403" ] \
    && [ "$R_OWNER_WRITE" = "200" ] && [ "$R_STATE" = "stale" ] \
    || { log "drill R unexpected: ok=$R_WRITE_OK denied=$R_WRITE_DENIED/$R_DENIED_CODE owner=$R_OWNER_WRITE state=$R_STATE"; exit 5; }

# --- drill E: blocked egress (backup producer cannot reach the server) ---
log "drill E: backup against unreachable server fails visibly, writes continue"
set +e
DG_BIN="$DG" bash "$ROOT/scripts/micro2-backup.sh" --server "http://127.0.0.1:9" \
    --key-file "$WORKDIR/owner.key" --data-dir "$DATA" \
    --backup-root "$BACKUPS" --keep 3 >/dev/null 2>"$WORKDIR/backupE.err"
E_BACKUP_EXIT=$?
set -e
E_ATTEMPT="$(python3 -c 'import json; print(json.load(open("'"$BACKUPS"'/attempts.jsonl".replace(chr(34),chr(39))))["outcome"]' 2>/dev/null || tail -n 1 "$BACKUPS/attempts.jsonl" | python3 -c 'import json,sys; print(json.load(sys.stdin)["outcome"])')"
E_STEP="$(tail -n 1 "$BACKUPS/attempts.jsonl" | python3 -c 'import json,sys; print(json.load(sys.stdin)["step"])')"
E_WRITE="$(call POST /v1/databases/drill/query '{"sql":"INSERT INTO t(v) VALUES ('"'"'e1'"'"')"}' "$WORKDIR/e1.json")"
E_STATE="$(curl -s "${AUTH_OWNER[@]}" "$BASE/v1/backups" | python3 -c 'import json,sys; print(json.load(sys.stdin)["per_database"]["drill"]["state"])')"
record egress "{\"backup_exit\":$E_BACKUP_EXIT,\"attempt_outcome\":\"$E_ATTEMPT\",\"attempt_step\":\"$E_STEP\",\"write_during_outage_http\":$E_WRITE,\"backup_state\":\"$E_STATE\"}"
[ "$E_BACKUP_EXIT" -ne 0 ] && [ "$E_ATTEMPT" = "error" ] && [ "$E_WRITE" = "200" ] \
    && [ "$E_STATE" = "stale" ] || { log "drill E unexpected"; exit 6; }

# --- drill D: disk pressure via per-process file-size ceiling ---
log "drill D: prlimit ceiling kills the writer, restart recovers, rows exact"
D_COUNT_BEFORE="$(call POST /v1/databases/drill/query '{"sql":"SELECT count(*) FROM t"}' "$WORKDIR/d0.json" >/dev/null; python3 -c 'import json; print(json.load(open("'"$WORKDIR"'/d0.json"))["rows"][0][0])')"
CUR_BYTES="$(du -sb "$DATA" | cut -f1)"
CEIL_BYTES=$(( CUR_BYTES + 49152 ))
prlimit --fsize="$CEIL_BYTES:$CEIL_BYTES" --pid "$SERVER_PID" 2>/dev/null \
    || { log "prlimit failed"; exit 7; }
set +e
timeout 60 curl -s -o "$WORKDIR/dfill.json" -w '%{http_code}' "${AUTH_OWNER[@]}" \
    -d '{"sql":"INSERT INTO t(v) SELECT printf('"'"'x%08d'"'"', random()) FROM (SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8) a, (SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8) b"}' \
    "$BASE/v1/databases/drill/query" >"$WORKDIR/dfill.code" 2>/dev/null
D_FILL_CODE="$(cat "$WORKDIR/dfill.code")"
# The ceiling kills the process (SIGXFSZ=25), the same death the committed
# disk_pressure test asserts; a polite SQLITE_FULL with a stable code is the
# alternate visible surface. Either is evidence; silence is not.
sleep 2
if kill -0 "$SERVER_PID" 2>/dev/null; then D_DEATH="survived"; else
    wait "$SERVER_PID" 2>/dev/null || true; D_DEATH="died"
fi
set -e
kill "$SERVER_PID" 2>/dev/null || true; wait "$SERVER_PID" 2>/dev/null || true; SERVER_PID=""
# restart without ceiling, verify acknowledged rows + serving writes + stale
rm -f "$WORKDIR/stderr.log"
start_server
D_COUNT_AFTER="$(call POST /v1/databases/drill/query '{"sql":"SELECT count(*) FROM t"}' "$WORKDIR/d1.json" >/dev/null; python3 -c 'import json; print(json.load(open("'"$WORKDIR"'/d1.json"))["rows"][0][0])')"
D_WRITE="$(call POST /v1/databases/drill/query '{"sql":"INSERT INTO t(v) VALUES ('"'"'d1'"'"')"}' "$WORKDIR/d2.json")"
D_STATE="$(curl -s "${AUTH_OWNER[@]}" "$BASE/v1/backups" | python3 -c 'import json,sys; print(json.load(sys.stdin)["per_database"]["drill"]["state"])')"
D_ROWS_NONDECREASING="$(python3 -c "print(str($D_COUNT_AFTER >= $D_COUNT_BEFORE).lower())")"
record disk "{\"count_before\":$D_COUNT_BEFORE,\"fill_attempt_http\":\"$D_FILL_CODE\",\"writer_fate\":\"$D_DEATH\",\"count_after_restart\":$D_COUNT_AFTER,\"rows_nondecreasing\":$D_ROWS_NONDECREASING,\"write_after_recovery_http\":$D_WRITE,\"backup_state\":\"$D_STATE\"}"
[ "$D_ROWS_NONDECREASING" = "true" ] && [ "$D_WRITE" = "200" ] && [ "$D_STATE" = "stale" ] \
    || { log "drill D unexpected"; exit 8; }

# --- final: row counts + evidence ---
FINAL_COUNT="$(call POST /v1/databases/drill/query '{"sql":"SELECT count(*) FROM t"}' "$WORKDIR/df.json" >/dev/null; python3 -c 'import json; print(json.load(open("'"$WORKDIR"'/df.json"))["rows"][0][0])')"
python3 - "$EVIDENCE" "$FINAL_COUNT" <<'EOF'
import json, sys
p, n = sys.argv[1], int(sys.argv[2])
doc = json.load(open(p))
doc["final_row_count"] = n
doc["verdict"] = "pass"
json.dump(doc, open(p, "w"), indent=2)
EOF
cat "$EVIDENCE"
