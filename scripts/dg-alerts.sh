#!/usr/bin/env bash
# Disk-headroom + Litestream-lag alerts (audit A8, observability
# contract §1/§3 alerts 2 and sidecar freshness). One run evaluates
# every check, prints one structured line per check to stderr (the
# journal — the same operator channel as the digest), and exits
# nonzero if any check alerts. There is no alert stack in this
# project by design (§3a): a failed oneshot unit + a journal line is
# the delivery, visible to the same tooling that watches the digest.
#
# Checks:
#   disk    free bytes AND inodes on the data filesystem must stay
#           above the floor (default 15% free or 1 GiB, whichever is
#           larger; inode floor 10%).
#   lag     the Litestream sidecar must be active, every live
#           database must have a replica entry (litestream-sync
#           --check), and no WAL file may sit un-checkpointed longer
#           than the lag threshold (default 900s): litestream
#           checkpoints continuously, so a WAL whose mtime is old is
#           the visible symptom of replication stalled behind it.
#           This is a local proxy for lag — the replica-side proof is
#           the weekly bucket-restore-drill, not this check.
#
# Usage: scripts/dg-alerts.sh
# Env: DG_ALERTS_DATA_DIR (/var/lib/dg), DG_ALERTS_DISK_FLOOR_PCT (15),
#      DG_ALERTS_DISK_FLOOR_BYTES (1073741824), DG_ALERTS_INODE_FLOOR_PCT (10),
#      DG_ALERTS_LAG_SECS (900), DG_ALERTS_SIDECAR_UNIT (dg-litestream),
#      DG_ALERTS_SYNC_CHECK (/usr/local/bin/litestream-sync.sh; "skip"
#      disables the coverage sub-check), DG_ALERTS_NOW (epoch override
#      for drills/tests).

set -uo pipefail
SELF="dg-alerts"
DATA_DIR="${DG_ALERTS_DATA_DIR:-/var/lib/dg}"
FLOOR_PCT="${DG_ALERTS_DISK_FLOOR_PCT:-15}"
FLOOR_BYTES="${DG_ALERTS_DISK_FLOOR_BYTES:-1073741824}"
INODE_PCT="${DG_ALERTS_INODE_FLOOR_PCT:-10}"
LAG_SECS="${DG_ALERTS_LAG_SECS:-900}"
SIDECAR="${DG_ALERTS_SIDECAR_UNIT:-dg-litestream}"
SYNC_CHECK="${DG_ALERTS_SYNC_CHECK:-/usr/local/bin/litestream-sync.sh}"
NOW="${DG_ALERTS_NOW:-$(date +%s)}"
ALERTS=0

emit() { echo "dg-alert check=$1 state=$2 $3" >&2; }

# --- disk -------------------------------------------------------------
if [ -d "$DATA_DIR" ]; then
    read -r disk_total disk_avail <<<"$(df --output=size,avail -B1 "$DATA_DIR" 2>/dev/null | tail -1)"
    read -r inode_total inode_avail <<<"$(df --output=itotal,iavail "$DATA_DIR" 2>/dev/null | tail -1)"
    if [ -n "${disk_avail:-}" ] && [ -n "${inode_avail:-}" ]; then
        pct_free=$(( disk_total > 0 ? disk_avail * 100 / disk_total : 0 ))
        inode_pct_free=$(( inode_total > 0 ? inode_avail * 100 / inode_total : 0 ))
        if [ "$disk_avail" -lt "$FLOOR_BYTES" ] || [ "$pct_free" -lt "$FLOOR_PCT" ] || [ "$inode_pct_free" -lt "$INODE_PCT" ]; then
            emit disk alert "path=$DATA_DIR free_bytes=$disk_avail free_pct=$pct_free inode_free_pct=$inode_pct_free floor_pct=$FLOOR_PCT floor_bytes=$FLOOR_BYTES action='free disk or raise retention limits before writes/imports block'"
            ALERTS=1
        else
            emit disk ok "path=$DATA_DIR free_bytes=$disk_avail free_pct=$pct_free inode_free_pct=$inode_pct_free"
        fi
    else
        emit disk unknown "path=$DATA_DIR reason=df-unavailable"
        ALERTS=1
    fi
else
    emit disk unknown "path=$DATA_DIR reason=missing"
    ALERTS=1
fi

# --- litestream lag ----------------------------------------------------
lag_reasons=""
if [ "$SIDECAR" != "skip" ]; then
    if ! systemctl is-active --quiet "$SIDECAR" 2>/dev/null; then
        lag_reasons="${lag_reasons:+$lag_reasons,}sidecar-inactive"
    fi
fi
if [ "$SYNC_CHECK" != "skip" ]; then
    if [ -x "$SYNC_CHECK" ]; then
        sudo -n "$SYNC_CHECK" --check >/dev/null 2>&1 || "$SYNC_CHECK" --check >/dev/null 2>&1 \
            || lag_reasons="${lag_reasons:+$lag_reasons,}replica-coverage-gap"
    else
        lag_reasons="${lag_reasons:+$lag_reasons,}sync-check-missing"
    fi
fi
oldest_wal_age=0
if [ -d "$DATA_DIR" ]; then
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        size="$(stat -c %s "$f" 2>/dev/null || echo 0)"
        [ "$size" -gt 0 ] || continue
        mtime="$(stat -c %Y "$f" 2>/dev/null || echo "$NOW")"
        age=$(( NOW - mtime ))
        [ "$age" -gt "$oldest_wal_age" ] && oldest_wal_age="$age"
    done < <(find "$DATA_DIR" -maxdepth 1 -name '*-wal' -type f 2>/dev/null)
fi
if [ "$oldest_wal_age" -gt "$LAG_SECS" ]; then
    lag_reasons="${lag_reasons:+$lag_reasons,}wal-stale-${oldest_wal_age}s"
fi
if [ -n "$lag_reasons" ]; then
    emit lag alert "reasons=$lag_reasons lag_threshold_secs=$LAG_SECS action='check dg-litestream journal and replica coverage; Tier 1 RPO is at risk while lag persists'"
    ALERTS=1
else
    emit lag ok "oldest_wal_age_secs=$oldest_wal_age lag_threshold_secs=$LAG_SECS"
fi

[ "$ALERTS" = "0" ] && echo "[$SELF] all checks ok" >&2
exit "$ALERTS"
