#!/usr/bin/env bash
# Disk-headroom + Replication-lag alerts (audit A8, observability
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
#   lag     the Replication sidecar must be active, every live
#           database must have a replica entry (replication-sync
#           --check), `replication status` must report every database
#           ok, and no WAL file may exceed the size ceiling (default
#           64 MiB): replication checkpoints continuously, so an
#           unbounded WAL is replication stalled behind it. (An old
#           WAL *mtime* alone proves nothing — an idle database's
#           WAL legitimately sits untouched for days; the first live
#           run taught this check that, see audit A8.) Replica-side
#           proof proper is the weekly bucket-restore-drill.
#
# Usage: scripts/dg-alerts.sh
# Env: DG_ALERTS_DATA_DIR (/var/lib/dg), DG_ALERTS_DISK_FLOOR_PCT (15),
#      DG_ALERTS_DISK_FLOOR_BYTES (1073741824), DG_ALERTS_INODE_FLOOR_PCT (10),
#      DG_ALERTS_WAL_MAX_BYTES (67108864), DG_ALERTS_SIDECAR_UNIT (dg-replication;
#      "skip" disables), DG_ALERTS_SYNC_CHECK (/usr/local/bin/replication-sync.sh;
#      "skip" disables), DG_ALERTS_LITESTREAM_BIN (/usr/local/bin/replication;
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
SIDECAR="${DG_ALERTS_SIDECAR_UNIT:-dg-replication}"
SYNC_CHECK="${DG_ALERTS_SYNC_CHECK:-/usr/local/bin/replication-sync.sh}"
LS_BIN="${DG_ALERTS_LITESTREAM_BIN:-/usr/local/bin/replication}"
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
            detail="path=$DATA_DIR free_bytes=$disk_avail free_pct=$pct_free inode_free_pct=$inode_pct_free floor_pct=$FLOOR_PCT floor_bytes=$FLOOR_BYTES action='free disk or raise retention limits before writes/imports block'"
            emit disk alert "$detail"
            stage disk "$detail"
            ALERTS=1
        else
            emit disk ok "path=$DATA_DIR free_bytes=$disk_avail free_pct=$pct_free inode_free_pct=$inode_pct_free"
        fi
    else
        emit disk unknown "path=$DATA_DIR reason=df-unavailable"
        stage disk "path=$DATA_DIR reason=df-unavailable"
        ALERTS=1
    fi
else
    emit disk unknown "path=$DATA_DIR reason=missing"
    stage disk "path=$DATA_DIR reason=missing"
    ALERTS=1
fi

# --- replication lag ----------------------------------------------------
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
        # makes `replication status` fail, and the old check read that as
        # an empty list of bad rows: the lag check reported ok while the
        # replication coverage was unknown.
        status_out="$("$LS_BIN" status 2>/dev/null)"
        status_rc=$?
        if [ "$status_rc" -ne 0 ]; then
            lag_reasons="${lag_reasons:+$lag_reasons,}replication-status-failed"
        else
            bad_rows="$(printf '%s\n' "$status_out" | awk 'NR>1 && $2 != "ok" {print $1}' | paste -sd, -)"
            [ -n "$bad_rows" ] && lag_reasons="${lag_reasons:+$lag_reasons,}status-not-ok:${bad_rows}"
        fi
    else
        lag_reasons="${lag_reasons:+$lag_reasons,}replication-bin-missing"
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
    detail="reasons=$lag_reasons wal_max_bytes=$WAL_MAX action='check dg-replication journal and replica coverage; Tier 1 RPO is at risk while lag persists'"
    emit lag alert "$detail"
    stage lag "$detail"
    ALERTS=1
else
    emit lag ok "biggest_wal_bytes=$biggest_wal wal_max_bytes=$WAL_MAX"
fi

# --- webhook second sink ---------------------------------------------
# Journal-only when nothing alerted or no endpoint is configured: the
# webhook carries alerts, not heartbeats, and an unconfigured run is
# exactly the old behaviour.
if [ "$ALERTS" = "0" ]; then
    echo "[$SELF] all checks ok" >&2
    exit 0
fi
if [ -n "$WEBHOOK_URL_FILE" ]; then
    if [ -r "$WEBHOOK_URL_FILE" ]; then
        WEBHOOK_URL="$(head -n 1 "$WEBHOOK_URL_FILE" | tr -d '\r\n[:space:]')"
    else
        emit webhook skipped "reason=url-file-unreadable"
        exit 1
    fi
fi
if [ -z "$WEBHOOK_URL" ]; then
    # Unconfigured means the old behaviour exactly: the alert lines
    # above are the whole delivery, with no new line saying so.
    exit 1
fi
# The same rule as dg::notify::WebhookSink: https, or plain http on a
# literal loopback address. A notification that silently detoured
# through a proxy or a redirect would be delivered somewhere nobody
# reviewed, so curl gets no proxy and no redirects below.
if ! printf '%s' "$WEBHOOK_URL" | grep -Eq '^https://|^http://127\.[0-9.]+([/:]|$)|^http://\[::1\]([/:]|$)'; then
    emit webhook refused "reason=scheme-must-be-https-or-loopback-http"
    exit 1
fi
if ! command -v curl >/dev/null 2>&1; then
    emit webhook skipped "reason=no-curl"
    exit 1
fi
json_escape() { printf '%s' "$1" | tr -d '\000-\010\012-\037' | sed 's/\\/\\\\/g; s/"/\\"/g'; }
checks_json=""
while IFS="$(printf '\t')" read -r name detail; do
    [ -n "$name" ] || continue
    checks_json="${checks_json:+$checks_json,}{\"check\":\"$(json_escape "$name")\",\"detail\":\"$(json_escape "$detail")\"}"
done <<<"$ALERT_LINES"
body="{\"source\":\"dg-alerts\",\"host\":\"$(json_escape "$(hostname 2>/dev/null || echo unknown)")\",\"at\":\"$(date -u +%Y-%m-%dT%H:%M:%SZ)\",\"alerts\":[$checks_json]}"
# The URL is never printed: it usually carries a token.
if curl -fsS -m 10 --noproxy '*' --max-redirs 0 -X POST \
    -H 'Content-Type: application/json' --data-binary "$body" \
    "$WEBHOOK_URL" >/dev/null 2>&1; then
    emit webhook sent "alerts=$(printf '%s' "$ALERT_LINES" | grep -c .)"
else
    emit webhook failed "curl_exit=$? action='the journal line above is the delivery; fix the endpoint'"
fi
exit 1
