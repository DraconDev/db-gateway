#!/usr/bin/env bash
# Steady check heartbeat: one write + read-back per run.
#
# The steady check needs an advancing txid to mean anything, and C-dev
# has no ongoing application writer. This script is the stand-in: it
# inserts one heartbeat row through the demo's scoped key and reads
# it back, proving the write path, the read path, and replication
# liveness on every fire. It never touches Turso and never uses the
# owner key.
#
# Usage: steady-heartbeat.sh [--server URL] [--key-file PATH]
#                          [--database NAME] [--dg PATH]
# Defaults: the deployment-host tunnel, ~/.config/dg/steady.key,
# billing_demo, and `dg` from PATH (DG_BIN overrides).
# Prints one line; exit nonzero on any failure.
set -euo pipefail

SERVER="http://127.0.0.1:7432"
KEY_FILE="$HOME/.config/dg/steady.key"
DATABASE="billing_demo"
DG_BIN="${DG_BIN:-dg}"
while [ $# -gt 0 ]; do
  case "$1" in
    --server)
      [ $# -ge 2 ] || { echo "--server requires a value" >&2; exit 2; }
      SERVER="$2"; shift 2;;
    --key-file)
      [ $# -ge 2 ] || { echo "--key-file requires a value" >&2; exit 2; }
      KEY_FILE="$2"; shift 2;;
    --database)
      [ $# -ge 2 ] || { echo "--database requires a value" >&2; exit 2; }
      DATABASE="$2"; shift 2;;
    --dg)
      [ $# -ge 2 ] || { echo "--dg requires a value" >&2; exit 2; }
      DG_BIN="$2"; shift 2;;
    *) echo "unknown flag: $1" >&2; exit 2;;
  esac
done

fail() { echo "[steady-heartbeat] FAIL: $*" >&2; exit 1; }
[ -r "$KEY_FILE" ] || fail "unreadable key file: $KEY_FILE"
command -v "$DG_BIN" >/dev/null 2>&1 || fail "no such dg binary: $DG_BIN"

NOW="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
BEFORE="$("$DG_BIN" --server "$SERVER" --key-file "$KEY_FILE" query "$DATABASE" \
    "SELECT count(*) FROM steady_heartbeat" 2>/dev/null \
    | python3 -c "import json,sys; print(json.load(sys.stdin)['rows'][0][0])")" \
    || fail "pre-write read failed"
"$DG_BIN" --server "$SERVER" --key-file "$KEY_FILE" query "$DATABASE" \
    "INSERT INTO steady_heartbeat (ts, note) VALUES ('$NOW', 'hourly demo heartbeat')" >/dev/null 2>&1 \
    || fail "heartbeat insert failed"
AFTER="$("$DG_BIN" --server "$SERVER" --key-file "$KEY_FILE" query "$DATABASE" \
    "SELECT count(*), max(ts) FROM steady_heartbeat" 2>/dev/null \
    | python3 -c "import json,sys; r=json.load(sys.stdin)['rows'][0]; print(f'{r[0]} {r[1]}')")" \
    || fail "post-write read failed"
COUNT="$(printf '%s' "$AFTER" | cut -d' ' -f1)"
LATEST="$(printf '%s' "$AFTER" | cut -d' ' -f2-)"
[ "$COUNT" -eq "$((BEFORE + 1))" ] || fail "count went $BEFORE -> $COUNT, expected +1"
[ "$LATEST" = "$NOW" ] || fail "latest row is $LATEST, expected $NOW"
echo "[steady-heartbeat] $NOW ok (rows=$COUNT)"
