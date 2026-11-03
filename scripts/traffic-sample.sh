#!/usr/bin/env bash
# One traffic sample for the query-observability series: service totals,
# per-database rows, and the latency percentiles the status endpoint
# serves, as a single JSON line appended to a JSONL log. Run on the
# host by dg-traffic-sample.timer (hourly); a multi-day series of
# these is the Analytics-lite trend no single status call can show —
# which database got slow, and when.
#
# Read-only against the gateway (one status read through the owner
# key); never row contents, only counters. Emits a line marked
# "unreachable" when the gateway does not answer rather than failing
# the series — a gap in the log is itself evidence, a dead timer is
# none. Exit status is always 0 for the same reason.
#
# Usage: scripts/traffic-sample.sh [--out FILE] [--server URL]
#                                  [--key-file PATH] [--dg PATH]
# Env: DG_TRAFFIC_SERVER (http://127.0.0.1:7432),
#      DG_TRAFFIC_KEY_FILE (/etc/dg/owner.key), DG_BIN (dg).

set -uo pipefail
SELF="traffic-sample"

SERVER="${DG_TRAFFIC_SERVER:-http://127.0.0.1:7432}"
KEY_FILE="${DG_TRAFFIC_KEY_FILE:-/etc/dg/owner.key}"
DG_BIN="${DG_BIN:-dg}"
OUT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --out)
      [ $# -ge 2 ] || { echo "--out requires a value" >&2; exit 2; }
      OUT="$2"; shift 2;;
    --server)
      [ $# -ge 2 ] || { echo "--server requires a value" >&2; exit 2; }
      SERVER="$2"; shift 2;;
    --key-file)
      [ $# -ge 2 ] || { echo "--key-file requires a value" >&2; exit 2; }
      KEY_FILE="$2"; shift 2;;
    --dg)
      [ $# -ge 2 ] || { echo "--dg requires a value" >&2; exit 2; }
      DG_BIN="$2"; shift 2;;
    *) echo "unknown flag: $1" >&2; exit 2;;
  esac
done

ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
epoch="$(date -u +%s)"
status="$("$DG_BIN" --json --server "$SERVER" --key-file "$KEY_FILE" status 2>/dev/null)" || status=""
if [ -z "$status" ]; then
    line="{\"ts\":\"$ts\",\"epoch\":$epoch,\"status\":\"unreachable\"}"
else
    line="$(printf '%s' "$status" | DG_TS="$ts" DG_EPOCH="$epoch" python3 /dev/fd/3 3<<'PY'
import json, os, sys
try:
    d = json.load(sys.stdin)
except Exception:
    print(json.dumps({"ts": os.environ["DG_TS"], "epoch": int(os.environ["DG_EPOCH"]), "status": "unparseable"}))
    raise SystemExit
r = d.get("requests", {})
dbs = {}
for name, e in (d.get("databases") or {}).items():
    dbs[name] = {
        "rows_read": e.get("rows_read"),
        "rows_written": e.get("rows_written"),
        "query_p95_us": e.get("query_p95_us"),
        "queue_waits": e.get("queue_waits"),
    }
print(json.dumps({
    "ts": os.environ["DG_TS"],
    "epoch": int(os.environ["DG_EPOCH"]),
    "total": r.get("total"),
    "query_p50_us": r.get("query_p50_us"),
    "query_p95_us": r.get("query_p95_us"),
    "query_samples": r.get("query_samples"),
    "wait_p95_us": r.get("wait_p95_us"),
    "capacity_exceeded": r.get("capacity_exceeded"),
    "databases": dbs,
}))
PY
)"
fi

if [ -n "$OUT" ]; then
    mkdir -p "$(dirname "$OUT")"
    echo "$line" >> "$OUT"
    echo "[$SELF] appended sample to $OUT" >&2
else
    echo "$line"
fi
exit 0
