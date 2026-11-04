#!/usr/bin/env bash
# One composed resource sample for the 1 GB budget (audit A7): gateway +
# sidecar + backup + OS, as a single JSON line appended to a JSONL log.
# Run on the host by dg-resource-sample.timer (6-hourly); a multi-day
# series of these is the composed trend gate 5 of
# docs/deployment-readiness.md asks for — single samples prove
# steady-state (already known), the *series* proves growth rates.
#
# Read-only: systemctl show, du, journalctl --disk-usage, /proc. Emits
# "unknown" for anything unavailable rather than failing the series —
# a gap in the log is itself evidence, a dead timer is none.
#
# Usage: scripts/resource-sample.sh [--out FILE]   (default: stdout)
# Env: DG_SAMPLE_UNITS (space list, default "dg.service
#      dg-replication.service"), DG_SAMPLE_PATHS (space list of
#      label=path, default the live layout), JOURNAL_GLOB (dg*).

set -uo pipefail
SELF="resource-sample"

UNITS="${DG_SAMPLE_UNITS:-dg.service dg-replication.service}"
PATHS="${DG_SAMPLE_PATHS:-data=/var/lib/dg backups=/var/lib/dg-backups}"
OUT=""
[ "${1:-}" = "--out" ] && OUT="${2:-}"

num_or_unknown() { case "$1" in ''|*[!0-9]*) echo '"unknown"' ;; *) echo "$1" ;; esac; }

ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
epoch="$(date -u +%s)"

units_json=""
for u in $UNITS; do
    cur="$(systemctl show "$u" -p MemoryCurrent --value 2>/dev/null || echo '')"
    peak="$(systemctl show "$u" -p MemoryPeak --value 2>/dev/null || echo '')"
    [ "$cur" = "[not set]" ] && cur=""
    [ "$peak" = "[not set]" ] && peak=""
    units_json="${units_json:+$units_json,}\"${u%.service}\":{\"memory_current\":$(num_or_unknown "$cur"),\"memory_peak\":$(num_or_unknown "$peak")}"
done

paths_json=""
for kv in $PATHS; do
    label="${kv%%=*}"; path="${kv#*=}"
    bytes="$(du -sb "$path" 2>/dev/null | cut -f1 || echo '')"
    paths_json="${paths_json:+$paths_json,}\"${label}\":$(num_or_unknown "$bytes")"
done

wal_bytes=0; wal_seen=0
for kv in $PATHS; do
    path="${kv#*=}"
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        sz="$(stat -c %s "$f" 2>/dev/null || echo 0)"
        wal_bytes=$((wal_bytes + sz)); wal_seen=1
    done < <(find "$path" -maxdepth 2 -name '*-wal' -type f 2>/dev/null)
done
[ "$wal_seen" = "1" ] || wal_bytes='"unknown"'

journal_bytes="$(journalctl --disk-usage 2>/dev/null | grep -oE '[0-9]+(\.[0-9]+)?[KMGT]?' | head -1 || echo '')"
journal_num='"unknown"'
case "$journal_bytes" in
    *K) journal_num="$(awk -v v="${journal_bytes%K}" 'BEGIN{printf "%d", v*1024}')" ;;
    *M) journal_num="$(awk -v v="${journal_bytes%M}" 'BEGIN{printf "%d", v*1024*1024}')" ;;
    *G) journal_num="$(awk -v v="${journal_bytes%G}" 'BEGIN{printf "%d", v*1024*1024*1024}')" ;;
    ''|*[!0-9.]*) : ;;
    *) journal_num="$journal_bytes" ;;
esac

mem_total="$(grep MemTotal /proc/meminfo 2>/dev/null | awk '{print $2 * 1024}')"
mem_avail="$(grep MemAvailable /proc/meminfo 2>/dev/null | awk '{print $2 * 1024}')"
disk_avail="$(df --output=avail -B1 /var/lib 2>/dev/null | tail -1 | tr -d ' ')"
disk_used="$(df --output=used -B1 /var/lib 2>/dev/null | tail -1 | tr -d ' ')"

line="{\"ts\":\"$ts\",\"epoch\":$epoch,\"units\":{$units_json},\"paths_bytes\":{$paths_json},\"wal_bytes\":$wal_bytes,\"journal_bytes\":$journal_num,\"host\":{\"mem_total\":$(num_or_unknown "$mem_total"),\"mem_available\":$(num_or_unknown "$mem_avail"),\"disk_used\":$(num_or_unknown "$disk_used"),\"disk_available\":$(num_or_unknown "$disk_avail")}}"

if [ -n "$OUT" ]; then
    mkdir -p "$(dirname "$OUT")"
    echo "$line" >> "$OUT"
    echo "[$SELF] appended sample to $OUT" >&2
else
    echo "$line"
fi
