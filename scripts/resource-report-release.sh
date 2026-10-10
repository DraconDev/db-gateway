#!/usr/bin/env bash
# Bounded release-mode resource measurement for the DB gateway.
#
# Usage: scripts/resource-report-release.sh
# Emits one JSON report on stdout; diagnostics go to stderr. Cleans up the
# server process and its disposable workdir on exit (trap).
#
# This is the release-profile companion to scripts/resource-report.sh,
# which builds the debug profile. Together they cover item 1 of
# docs/resource-budget.md (release-profile RSS under a realistic workload,
# idle and sustained) and item 3 (target OS baseline RSS and page-cache
# behavior with the data dir hot), on the dev host. The supervised micro2
# sample (8292 KiB) is the on-target fact for the same workload; this
# harness exists so the release profile is measured too and so the OS
# baseline and page-cache growth are observed rather than guessed.
#
# What it does NOT do: measure litestream/OVH sidecar (item 2, already
# measured in docs/sandbox-packet-2026-09-29-rehearsal.log gate 9, peaks
# 44 MiB replication / 33 MiB restore inside a 1 GiB enforced scope), the
# target-host burst test (item 4) or the disk growth projection (item 5).
# A 1 GB fit claim is the supervised micro2 run with this harness's
# workload carried over; that is the next step, not this script.

set -euo pipefail

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/dg-resource-release-XXXXXX")"
SERVER_PID=""
cleanup() {
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

log() { echo "[resource-report-release] $*" >&2; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
log "building (release, locked)"
cargo build --release --locked --quiet --manifest-path "$ROOT/Cargo.toml" >&2
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/.cargo/target}"
DG="$TARGET_DIR/release/dg"

# Memory sampler: VmRSS in KiB for a PID, plus VmSize and VmHWM for trend.
rss_kb() {
    awk '/^VmRSS:/ {print $2}' "/proc/$1/status"
}
vm_kb() {
    awk -v k="$1" '$1==k":" {print $2}' "/proc/$PID/status"
}

# OS memory snapshot: total/available/buff/cache, used by the kernel's own
# accounting, with units in KiB.
os_mem_kb() {
    awk '
        /^MemTotal:/     {t=$2}
        /^MemAvailable:/ {a=$2}
        /^Buffers:/      {b=$2}
        /^Cached:/       {c=$2}
        /^Shmem:/        {s=$2}
        END {print t, a, b, c, s}
    ' /proc/meminfo
}
os_mem_field() {
    # 1=MemTotal 2=MemAvailable 3=Buffers 4=Cached 5=Shmem
    os_mem_kb | awk -v f="$1" '{print $f}'
}

# Page-cache footprint of the data dir: best-effort estimate by walking
# the dir and summing mincore'd pages. Without root, /proc/self/page_tables
# is unreliable, so we measure the Cached column delta between cold and
# hot states instead. This is what the kernel actually charges to the
# page cache, and it includes shared pages (which /proc/<pid>/smaps does
# not double-count).
page_cache_growth_kb() {
    os_mem_field 4
}

OWNER_JSON="$("$DG" init --data-dir "$WORKDIR/data")"
OWNER_KEY="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["owner_key"])' <<<"$OWNER_JSON")"
printf '%s\n' "$OWNER_KEY" >"$WORKDIR/cli.key"
chmod 600 "$WORKDIR/cli.key"

# Cold-state OS sample: data dir just initialized, no reads have warmed
# the page cache on the gateway's database files.
OS_COLD_TOTAL="$(os_mem_field 1)"
OS_COLD_AVAIL="$(os_mem_field 2)"
OS_COLD_CACHED="$(page_cache_growth_kb)"

log "starting dg serve (release profile, hot workload follows)"
"$DG" serve --listen 127.0.0.1:0 --data-dir "$WORKDIR/data" 2>"$WORKDIR/stderr.log" &
SERVER_PID=$!
PID="$SERVER_PID"

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
    local http_code
    http_code="$(curl -s -o /dev/null -w '%{http_code}' "${AUTH[@]}" "$@")"
    if [ "$http_code" -ge 400 ] || [ "$http_code" -eq 000 ]; then
        log "request failed: $* -> HTTP $http_code"
        curl -s "${AUTH[@]}" "$@" >&2 || true
        echo >&2
        return 22
    fi
}

# Idle: just after the server prints its listen line. The 2 s settle gives
# axum/tokio a chance to fully initialize the worker pool without warming
# the page cache (no requests have hit the database files yet).
log "settling idle (2 s)"
sleep 2
IDLE_RSS="$(rss_kb "$PID")"
IDLE_VMSIZE="$(vm_kb VmSize)"
IDLE_VMHWM="$(vm_kb VmHWM)"

# Loaded: same workload as the debug harness (3 DBs x 100 rows) so the
# release and debug reports are directly comparable. Each insert forces
# the gateway to mmap the database file and pull pages into the kernel's
# page cache, which is what "data dir hot" means for the OS measurement.
log "loading workload: 3 databases x 100 rows"
for i in 1 2 3; do
    api -d "{\"name\":\"w$i\"}" "$BASE/v1/databases"
    api -d '{"sql":"CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT)"}' \
        "$BASE/v1/databases/w$i/query"
    VALUES="$(python3 -c 'print(",".join(f"(\x27k{n}\x27,\x27v{n:04d}\x27)" for n in range(100)))')"
    api -d "{\"sql\":\"INSERT INTO kv VALUES $VALUES\"}" \
        "$BASE/v1/databases/w$i/query"
done

# Force the page cache to settle on the just-written pages by reading each
# file back; the kernel reclaims a working set, and a follow-up measurement
# of MemAvailable/Cached is a fair "data dir hot" snapshot. This is the
# point the OS baseline half (item 3) wants to capture.
log "warming page cache on the data dir"
find "$WORKDIR/data" -type f -print0 | xargs -0 cat >/dev/null 2>&1 || true
sync

LOADED_RSS="$(rss_kb "$PID")"
LOADED_VMSIZE="$(vm_kb VmSize)"
LOADED_VMHWM="$(vm_kb VmHWM)"
OS_HOT_CACHED="$(page_cache_growth_kb)"
OS_HOT_AVAIL="$(os_mem_field 2)"

# Burst: hammer the server with concurrent INSERTs (up to 50 in flight)
# for a fixed window. The point is to measure peak RSS under sustained
# capacity pressure, not to record a passing latency test -- capacity
# rejects (HTTP 429) are expected and do not fail the harness. The peak
# here is the candidate "burst headroom" input the budget needs.
log "sustained burst: 50 concurrent INSERTs, 4 s window"
PEAK_RSS="$LOADED_RSS"
PEAK_VMHWM="$LOADED_VMHWM"
( for _ in $(seq 1 50); do
    ( for _ in $(seq 1 200); do
          curl -sf -o /dev/null "${AUTH[@]}" \
              -d "{\"sql\":\"INSERT INTO kv VALUES ('k$(date +%N)','v$(date +%N)')\"}" \
              "$BASE/v1/databases/w1/query" 2>/dev/null || true
      done ) &
  done; wait ) &
BURST_PID=$!
SAMPLE_END=$(( $(date +%s) + 4 ))
while [ "$(date +%s)" -lt "$SAMPLE_END" ]; do
    if [ -d "/proc/$PID" ]; then
        R="$(rss_kb "$PID" 2>/dev/null || echo 0)"
        H="$(vm_kb VmHWM 2>/dev/null || echo 0)"
        if [ "$R" -gt "$PEAK_RSS" ]; then PEAK_RSS="$R"; fi
        if [ "$H" -gt "$PEAK_VMHWM" ]; then PEAK_VMHWM="$H"; fi
    fi
    sleep 0.1
done
wait "$BURST_PID" 2>/dev/null || true

# Settle back to idle: the burst is over; the gateway should release the
# pooled work and shrink back. If it does not, the 1 GB cap is wrong.
log "settling post-burst (2 s)"
sleep 2
POST_BURST_RSS="$(rss_kb "$PID")"

# Latency: 20 sequential single-row SELECTs, same shape as the debug
# harness so the two reports are diffable. The release binary should
# show lower latency than the debug binary, and the report records the
# actual numbers rather than claiming a ratio.
log "probing latency: 20 sequential SELECTs"
LAT="$(for _ in $(seq 1 20); do
    curl -sf -o /dev/null "${AUTH[@]}" -w '%{time_total}\n' \
        -d '{"sql":"SELECT v FROM kv WHERE k = ?","args":["k42"]}' \
        "$BASE/v1/databases/w2/query"
done)"

DISK_BYTES="$(du -sb "$WORKDIR/data" | cut -f1)"
STAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
RELEASE_BIN_BYTES="$(stat -c '%s' "$DG")"

# Carry the per-connection cache ceiling into the report so a reader
# auditing the page-cache term does not have to grep the source: the
# gateway's read+write admission lanes are 8+4 concurrent connections,
# each pinned to cache_size=-2048 (2 MiB), so the upper bound on
# per-process SQLite cache is 12 * 2 MiB = 24 MiB, not the 10 MiB a
# single-lane reading would imply. The value is inlined because the
# audit is "is the budget row right", and pulling it from the source
# each run would invite drift.
CACHE_PER_CONN_KIB=2048
READ_PERMITS=8
WRITE_PERMITS=4
SQLITE_CACHE_CEILING_KIB=$(( (READ_PERMITS + WRITE_PERMITS) * CACHE_PER_CONN_KIB ))

# Page cache growth attributable to the gateway's data dir = hot Cached
# minus cold Cached. A negative result means the kernel evicted pages
# during the workload (unlikely with a 30+ GiB host, but reported as a
# signed delta for honesty). On the 1 GB micro2 host the same delta is
# the binding term.
PAGE_CACHE_DELTA_KIB=$(( OS_HOT_CACHED - OS_COLD_CACHED ))

# Sidecar ceiling: item 2 from the rehearsal packet, committed and
# recomputable. Pulled from the packet-facts output so a reader does
# not have to re-derive; the source path is recorded so a future run
# that supersedes it can update the report.
SIDECAR_PEAK_REPLICATION_KIB=45632
SIDECAR_PEAK_RESTORE_KIB=34576

# Pre-compute the suggested MemoryMax in shell so the JSON below can
# re-emit the same numbers and a reader can grep the script for the
# rationale without re-running Python.
#
# Formula: take the max of three measured terms, double it for headroom,
# floor at 64 MiB so a tiny workload does not result in a cap the
# process outgrows on first WAL write.
#   1. measured gateway peak RSS under burst (release profile)
#   2. SQLite per-connection cache ceiling (8+4 conns x 2 MiB)
#   3. sidecar peak (replication, the larger of the two)
binding_term_kib="$PEAK_RSS"
[ "$SQLITE_CACHE_CEILING_KIB" -gt "$binding_term_kib" ] && binding_term_kib="$SQLITE_CACHE_CEILING_KIB"
[ "$SIDECAR_PEAK_REPLICATION_KIB" -gt "$binding_term_kib" ] && binding_term_kib="$SIDECAR_PEAK_REPLICATION_KIB"
suggested_memory_max_kib=$(( binding_term_kib * 2 ))
[ "$suggested_memory_max_kib" -lt 65536 ] && suggested_memory_max_kib=65536

python3 - "$LAT" "$binding_term_kib" "$suggested_memory_max_kib" <<EOF
import json, statistics, sys

lat_blob = """$LAT""".split()
binding_term_kib = int(sys.argv[2])
suggested_memory_max_kib = int(sys.argv[3])

samples_ms = [float(line) * 1000.0 for line in lat_blob]
ordered = sorted(samples_ms)

report = {
    "schema": "dg-resource-report-release/1",
    "timestamp": "$STAMP",
    "host": {
        "os": "$(uname -s)",
        "arch": "$(uname -m)",
        "kernel": "$(uname -r)",
        "cpu_count": $(nproc),
        "mem_total_kib": $(awk '/^MemTotal:/ {print $2}' /proc/meminfo),
    },
    "build": {
        "profile": "release",
        "locked": True,
        "binary_bytes": $RELEASE_BIN_BYTES,
    },
    "workload": {
        "description": "3 databases, each one table with 100 small rows, then 20 sequential single-row SELECTs, then a 4 s 50-way concurrent INSERT burst on w1",
        "databases": 3,
        "rows_per_db": 100,
        "latency_probes": len(samples_ms),
        "burst_concurrent_workers": 50,
        "burst_window_seconds": 4,
    },
    "memory_rss_kib": {
        "idle": $IDLE_RSS,
        "loaded": $LOADED_RSS,
        "peak_under_burst": $PEAK_RSS,
        "post_burst_settle": $POST_BURST_RSS,
        "vm_size_idle_kib": $IDLE_VMSIZE,
        "vm_hwm_idle_kib": $IDLE_VMHWM,
        "vm_size_loaded_kib": $LOADED_VMSIZE,
        "vm_hwm_loaded_kib": $LOADED_VMHWM,
        "vm_hwm_under_burst_kib": $PEAK_VMHWM,
    },
    "os_memory_kib": {
        "cold_total": $OS_COLD_TOTAL,
        "cold_available": $OS_COLD_AVAIL,
        "cold_cached": $OS_COLD_CACHED,
        "hot_cached": $OS_HOT_CACHED,
        "hot_available": $OS_HOT_AVAIL,
        "data_dir_page_cache_delta": $PAGE_CACHE_DELTA_KIB,
    },
    "sqlite_cache_ceiling_kib": $SQLITE_CACHE_CEILING_KIB,
    "sidecar_peak_kib": {
        "replication": $SIDECAR_PEAK_REPLICATION_KIB,
        "restore": $SIDECAR_PEAK_RESTORE_KIB,
        "source": "docs/sandbox-packet-2026-09-29-rehearsal.log gate 9",
    },
    "binding_term_kib": $binding_term_kib,
    "suggested_memory_max_kib": $suggested_memory_max_kib,
    "latency_ms": {
        "min": round(min(samples_ms), 3),
        "p50": round(statistics.median(samples_ms), 3),
        "max": round(max(samples_ms), 3),
    },
    "disk_bytes": $DISK_BYTES,
    "workdir": "$WORKDIR",
    "limitations": [
        "Dev host has 32 GiB RAM and 16 CPUs; the binding terms on a 1 GiB / 2 vCPU micro2 target will differ.",
        "Burst is single-process, loopback, no TLS terminator, no real workload, no realistic schema.",
        "Page cache delta is whole-host Cached minus cold; includes any unrelated files the kernel happened to cache during the run, not just the data dir.",
        "Single run, not a statistical sample; rerun to compare, not to certify.",
        "Item 2 (sidecar) is sourced from the rehearsal packet, not re-measured here. Re-run scripts/sandbox-s3-gates.sh on a new packet to update the sidecar row.",
        "Item 4 (target-host burst under capacity-reject regime) and item 5 (disk growth projection) are not in scope; the supervised micro2 run is the right place to record them.",
    ],
    "budget_note": "This report is one input to the MemoryMax value, not a claim of fit. A 1 GB fit claim requires the supervised micro2 run with the same workload and the same page-cache methodology; the suggested_memory_max_kib field is a starting ceiling, not a number the host has run under.",
}
print(json.dumps(report, indent=2))
EOF
