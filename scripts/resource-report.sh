#!/usr/bin/env bash
# Bounded local resource-measurement harness for the DB gateway.
#
# Usage: scripts/resource-report.sh
# Emits one JSON report on stdout; diagnostics go to stderr. Cleans up the
# server process and its disposable workdir on exit (trap).
#
# Workload (fixed, documented): 3 databases, each with one table holding 100
# small rows, followed by 20 sequential single-row SELECTs for latency.
# This measures idle + small-workload behavior on the current host only. It
# does NOT demonstrate compliance with any production memory budget.

set -euo pipefail

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/dg-resource-XXXXXX")"
SERVER_PID=""
cleanup() {
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

log() { echo "[resource-report] $*" >&2; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
log "building (debug, locked)"
cargo build --locked --quiet --manifest-path "$ROOT/Cargo.toml" >&2
DG="$ROOT/target/debug/dg"

rss_kb() {
    awk '/^VmRSS:/ {print $2}' "/proc/$1/status"
}

OWNER_JSON="$("$DG" init --data-dir "$WORKDIR/data")"
OWNER_KEY="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["owner_key"])' <<<"$OWNER_JSON")"
printf '%s\n' "$OWNER_KEY" >"$WORKDIR/cli.key"
chmod 600 "$WORKDIR/cli.key"

"$DG" serve --listen 127.0.0.1:0 --data-dir "$WORKDIR/data" 2>"$WORKDIR/stderr.log" &
SERVER_PID=$!
# Poll for the listen address with a deadline: a single-shot read races
# server boot and flakes under parallel load (the server wins the race on a
# quiet box, loses it when dozens of suites spawn at once).
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
AUTH=(-H "Authorization: Bearer $OWNER_KEY" -H 'Content-Type: application/json')

api() {
    local out http_code
    out="$(curl -s -o /dev/null -w '\n%{http_code}' "${AUTH[@]}" "$@")"
    http_code="$(tail -n1 <<<"$out")"
    if [ "$http_code" -ge 400 ] || [ "$http_code" -eq 000 ]; then
        log "request failed: $* -> HTTP $http_code"
        curl -s "${AUTH[@]}" "$@" >&2 || true
        echo >&2
        return 22
    fi
}

log "settling idle"
sleep 2
IDLE_RSS="$(rss_kb "$SERVER_PID")"

log "loading workload: 3 databases x 100 rows"
for i in 1 2 3; do
    api -d "{\"name\":\"w$i\"}" "$BASE/v1/databases"
    api -d '{"sql":"CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT)"}' \
        "$BASE/v1/databases/w$i/query"
    VALUES="$(python3 -c 'print(",".join(f"(\x27k{n}\x27,\x27v{n:04d}\x27)" for n in range(100)))')"
    api -d "{\"sql\":\"INSERT INTO kv VALUES $VALUES\"}" \
        "$BASE/v1/databases/w$i/query"
done
LOADED_RSS="$(rss_kb "$SERVER_PID")"

log "probing latency: 20 sequential SELECTs"
LAT="$(for _ in $(seq 1 20); do
    curl -sf -o /dev/null "${AUTH[@]}" -w '%{time_total}\n' \
        -d '{"sql":"SELECT v FROM kv WHERE k = ?","args":["k42"]}' \
        "$BASE/v1/databases/w2/query"
done)"

DISK_BYTES="$(du -sb "$WORKDIR/data" | cut -f1)"
STAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

python3 - "$LAT" <<EOF
import json, statistics
samples_ms = [float(line) * 1000.0 for line in """$LAT""".split()]
ordered = sorted(samples_ms)
report = {
    "schema": "dg-resource-report/1",
    "timestamp": "$STAMP",
    "host": {
        "os": "$(uname -s)",
        "arch": "$(uname -m)",
        "kernel": "$(uname -r)",
        "cpu_count": $(nproc),
        "mem_total_kb": $(awk '/^MemTotal:/ {print $2}' /proc/meminfo),
    },
    "build": {"profile": "debug", "locked": True},
    "workload": {
        "description": "3 databases, each one table with 100 small rows, then 20 sequential single-row SELECTs",
        "databases": 3,
        "rows_per_db": 100,
        "latency_probes": len(samples_ms),
    },
    "memory_rss_kb": {"idle": $IDLE_RSS, "loaded": $LOADED_RSS},
    "latency_ms": {
        "min": round(min(samples_ms), 3),
        "p50": round(statistics.median(samples_ms), 3),
        "max": round(max(samples_ms), 3),
    },
    "disk_bytes": $DISK_BYTES,
    "workdir": "$WORKDIR",
    "limitations": [
        "Unrestricted development host: no cgroup/memory limits were applied.",
        "Debug build: release-profile RSS will differ (usually lower).",
        "Unmeasured: continuous-backup sidecars, OS page cache, WAL checkpoint timing, TLS/reverse-proxy overhead.",
        "Loopback latency only; no network, contention, or multi-client load.",
        "Single run, not a statistical sample; rerun to compare, not to certify.",
    ],
    "budget_note": "This report is NOT a claim of compliance with any 1 GB production budget. It is a local baseline for trend comparison only.",
}
print(json.dumps(report, indent=2))
EOF
