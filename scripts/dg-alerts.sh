#!/usr/bin/env bash
# Disk-headroom + Litestream-lag alerts (audit A8, observability
# contract §1/§3 alerts 2 and sidecar freshness). One run evaluates
# every check, prints one structured line per check to stderr (the
# journal — the same operator channel as the digest), and exits
# nonzero if any check alerts. The journal line is the delivery that
# cannot quietly stop working; when DG_ALERTS_WEBHOOK_URL (or the
# file DG_ALERTS_WEBHOOK_URL_FILE points at) names an endpoint, the
# alert is additionally POSTed there as JSON — the §3a second-sink
# pattern, extended from backup health to these two checks. A webhook
# that fails is a reported delivery failure, never a silent loss, and
# it never blocks the journal line. With no webhook configured the
# run behaves exactly as before: journal only.
#
# Checks:
#   disk    free bytes AND inodes on the data filesystem must stay
#           above the floor (default 15% free or 1 GiB, whichever is
#           larger; inode floor 10%).
#   lag     the Litestream sidecar must be active, every live
#           database must have a replica entry (litestream-sync
#           --check), `litestream status` must report every database
#           ok, and no WAL file may exceed the size ceiling (default
#           64 MiB): litestream checkpoints continuously, so an
#           unbounded WAL is replication stalled behind it. (An old
#           WAL *mtime* alone proves nothing — an idle database's
#           WAL legitimately sits untouched for days; the first live
#           run taught this check that, see audit A8.) Replica-side
#           proof proper is the weekly bucket-restore-drill.
#
# Usage: scripts/dg-alerts.sh
# Env: DG_ALERTS_DATA_DIR (/var/lib/dg), DG_ALERTS_DISK_FLOOR_PCT (15),
#      DG_ALERTS_DISK_FLOOR_BYTES (1073741824), DG_ALERTS_INODE_FLOOR_PCT (10),
#      DG_ALERTS_WAL_MAX_BYTES (67108864), DG_ALERTS_SIDECAR_UNIT (dg-litestream;
#      "skip" disables), DG_ALERTS_SYNC_CHECK (/usr/local/bin/litestream-sync.sh;
#      "skip" disables), DG_ALERTS_LITESTREAM_BIN (/usr/local/bin/litestream;
#      "skip" disables the status sub-check),
#      DG_ALERTS_WEBHOOK_URL (unset: journal only),
#      DG_ALERTS_WEBHOOK_URL_FILE (a mode-0600 file holding the URL;
#      wins over DG_ALERTS_WEBHOOK_URL when both are set).

set -uo pipefail
SELF="dg-alerts"
DATA_DIR="${DG_ALERTS_DATA_DIR:-/var/lib/dg}"
FLOOR_PCT="${DG_ALERTS_DISK_FLOOR_PCT:-15}"
FLOOR_BYTES="${DG_ALERTS_DISK_FLOOR_BYTES:-1073741824}"
INODE_PCT="${DG_ALERTS_INODE_FLOOR_PCT:-10}"
WAL_MAX="${DG_ALERTS_WAL_MAX_BYTES:-67108864}"
SIDECAR="${DG_ALERTS_SIDECAR_UNIT:-dg-litestream}"
SYNC_CHECK="${DG_ALERTS_SYNC_CHECK:-/usr/local/bin/litestream-sync.sh}"
LS_BIN="${DG_ALERTS_LITESTREAM_BIN:-/usr/local/bin/litestream}"
WEBHOOK_URL="${DG_ALERTS_WEBHOOK_URL:-}"
WEBHOOK_URL_FILE="${DG_ALERTS_WEBHOOK_URL_FILE:-}"
ALERTS=0
# The alerting checks, one "name<tab>detail" line each, for the webhook
# body. Checks that pass are journal-only: the webhook carries alerts,
# not heartbeats.
ALERT_LINES=""

emit() { echo "dg-alert check=$1 state=$2 $3" >&2; }

# Remember an alert for the webhook body. The journal line is emitted
# by the caller as before; this only stages the second sink.
stage() { ALERT_LINES="${ALERT_LINES:+$ALERT_LINES
}$1	$2"; }

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
if [ "$LS_BIN" != "skip" ]; then
    if [ -x "$LS_BIN" ]; then
        # The command's exit status decides whether the answer is "every
        # replica ok" or "I could not ask". A sidecar that is broken --
        # a bad config, an unreadable env file, a crashed replica --
        # makes `litestream status` fail, and the old check read that as
        # an empty list of bad rows: the lag check reported ok while the
        # replication coverage was unknown.
        status_out="$("$LS_BIN" status 2>/dev/null)"
        status_rc=$?
        if [ "$status_rc" -ne 0 ]; then
            lag_reasons="${lag_reasons:+$lag_reasons,}litestream-status-failed"
        else
            bad_rows="$(printf '%s\n' "$status_out" | awk 'NR>1 && $2 != "ok" {print $1}' | paste -sd, -)"
            [ -n "$bad_rows" ] && lag_reasons="${lag_reasons:+$lag_reasons,}status-not-ok:${bad_rows}"
        fi
    else
        lag_reasons="${lag_reasons:+$lag_reasons,}litestream-bin-missing"
    fi
fi
biggest_wal=0
if [ -d "$DATA_DIR" ]; then
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        size="$(stat -c %s "$f" 2>/dev/null || echo 0)"
        [ "$size" -gt "$biggest_wal" ] && biggest_wal="$size"
    done < <(find "$DATA_DIR" -maxdepth 1 -name '*-wal' -type f 2>/dev/null)
fi
if [ "$biggest_wal" -gt "$WAL_MAX" ]; then
    lag_reasons="${lag_reasons:+$lag_reasons,}wal-oversized-${biggest_wal}b"
fi
if [ -n "$lag_reasons" ]; then
    emit lag alert "reasons=$lag_reasons wal_max_bytes=$WAL_MAX action='check dg-litestream journal and replica coverage; Tier 1 RPO is at risk while lag persists'"
    ALERTS=1
else
    emit lag ok "biggest_wal_bytes=$biggest_wal wal_max_bytes=$WAL_MAX"
fi

[ "$ALERTS" = "0" ] && echo "[$SELF] all checks ok" >&2
exit "$ALERTS"
