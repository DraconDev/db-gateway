#!/usr/bin/env bash
# Keep the replication config in step with the gateway's live databases.
#
# Why this exists (docs/audit-2026-10-19.md A2): replication reads its
# config once at startup, and scripts/install-replication.sh renders that
# config from the `dg list` output at install time. A database created
# afterwards was silently never replicated -- and snapshot-set freshness
# could still report it `fresh`, so nothing surfaced the gap. This script
# is the durable fix: it re-renders the desired config from the live
# database list, atomically replaces the config only when it changed,
# and restarts the sidecar so the new config is the one actually running.
# Run on a timer (deploy/dg-replication-sync.timer) it self-heals any
# drift -- creates, deletes, hand edits, missed events -- within one
# timer interval, and keeps the config file the single source of truth
# (which `replication status`, and therefore host-audit, reads).
#
# Modes:
#   (default)  apply: replace + restart if the rendered config differs
#   --check    report per-database coverage; exit 1 on any drift.
#              Output lines: replication-coverage db=<name> state=covered|missing|extra
#   --print    print the desired config to stdout; change nothing
#
# Configuration (environment; host defaults first):
#   DG_BIN                  dg binary            (default: dg)
#   DG_SERVER               optional --server URL for `dg list`
#   DG_KEY_FILE             owner credential     (default: /etc/dg/owner.key)
#   DG_LIST_AS              run `dg list` as this user via sudo -n -u
#                           (default: dg when running as root, else empty)
#   DG_DATA_DIR             database files       (default: /var/lib/dg)
#   REPLICATION_CONFIG       config to manage     (default: /etc/replication.yml)
#   REPLICATION_ENV_FILE     S3 settings source   (default: /etc/dg/replication.env)
#   DG_REPLICA_URL_TEMPLATE if set, replica URL per database with {name}
#                           substituted, e.g. file:///srv/replicas/{name};
#                           the S3 env file is then not read. Exists for
#                           local drills/rehearsals with a file replica.
#   REPLICATION_RESTART_CMD  command run (via bash -c) after a config change
#                           (default: systemctl restart dg-replication.service;
#                            empty string disables the restart)
#   REPLICATION_CONFIG_OWNER chown target for the config (default: dg:dg;
#                            empty string skips chown)
#   DG_REPLICATION_INCLUDE_REGISTRY  replicate metadata.sqlite as the fixed
#                            `metadata` entry (default: 1; 0 opts out)
#   DG_REPLICATION_RETENTION replica retention  (default: 168h)
#   DG_REPLICATION_SNAPSHOT_INTERVAL              (default: 24h)
#   DG_BACKUP_ROOT           where the replica status is published
#                           (default: /var/lib/dg-backups)
#   DG_REPLICATION_STATUS_CMD command that reports the sidecar's state
#                           (default: systemctl is-active dg-replication.service;
#                            empty string disables the check)
#
# The S3 access/secret keys are never inlined: the rendered config keeps
# ${REPLICATION_S3_*} references for the two credential keys (expanded by
# replication from the sidecar's EnvironmentFile), exactly as the
# installer's render did. Only bucket/endpoint/region -- not secrets --
# are substituted from the env file, and they are never echoed.
#
# The published replica status carries the same non-secret facts: where
# the copy is, how long it is kept, what it covers, and whether the
# sidecar is running. It is written where the gateway reads it, so the
# operator can ask the gateway where their data is and get the host's
# own answer. Credentials are never written there either.

set -euo pipefail
SELF="replication-sync"
fail() { echo "[$SELF] FAIL: $*" >&2; exit 1; }
log() { echo "[$SELF] $*" >&2; }

MODE="apply"
case "${1:-}" in
    "") ;;
    --check) MODE="check" ;;
    --print) MODE="print" ;;
    *) fail "usage: $SELF [--check|--print]" ;;
esac

DG_BIN="${DG_BIN:-dg}"
DG_KEY_FILE="${DG_KEY_FILE:-/etc/dg/owner.key}"
DG_DATA_DIR="${DG_DATA_DIR:-/var/lib/dg}"
CONFIG="${REPLICATION_CONFIG:-/etc/replication.yml}"
ENV_FILE="${REPLICATION_ENV_FILE:-/etc/dg/replication.env}"
BACKUP_ROOT="${DG_BACKUP_ROOT:-/var/lib/dg-backups}"
STATUS_CMD="${DG_REPLICATION_STATUS_CMD-systemctl is-active dg-replication.service}"
RESTART_CMD="${REPLICATION_RESTART_CMD-systemctl restart dg-replication.service}"
CONFIG_OWNER="${REPLICATION_CONFIG_OWNER-dg:dg}"
if [ -z "${DG_LIST_AS+x}" ]; then
    # On the host this runs as root (timer) and the owner key is 0600 dg,
    # so `dg list` must run as dg. Anywhere else, run it directly.
    if [ "$(id -u)" = "0" ]; then DG_LIST_AS="dg"; else DG_LIST_AS=""; fi
fi

# --- 1. Live database names, from the gateway itself. -------------------
# Asking the gateway (not globbing the data dir) is what keeps tombstoned
# or unregistered leftover files out of the config -- the same rule the
# installer's render used. JSON is parsed with python3 when available
# (present on the host; the installer already relies on it), with a
# sed fallback for a python-less environment.
list_cmd=("$DG_BIN")
[ -n "${DG_SERVER:-}" ] && list_cmd+=(--server "$DG_SERVER")
[ -n "$DG_KEY_FILE" ] && list_cmd+=(--key-file "$DG_KEY_FILE")
list_cmd+=(list)
if [ -n "$DG_LIST_AS" ]; then
    LIST_JSON="$(sudo -n -u "$DG_LIST_AS" "${list_cmd[@]}" 2>/dev/null)" \
        || fail "'dg list' as $DG_LIST_AS failed (gateway down? key unreadable?)"
else
    LIST_JSON="$("${list_cmd[@]}" 2>/dev/null)" \
        || fail "'dg list' failed (gateway down? key unreadable?)"
fi
if command -v python3 >/dev/null 2>&1; then
    # Capture into a variable first: a process substitution's exit status
    # is invisible to `mapfile ... || fail`, so malformed JSON would
    # otherwise surface as a misleading "no live databases".
    PARSED_NAMES="$(printf '%s' "$LIST_JSON" | python3 -c '
import json, sys
for name in json.load(sys.stdin).get("databases", []):
    print(name)
' 2>/dev/null)" || fail "could not parse 'dg list' JSON"
    LIVE=()
    [ -z "$PARSED_NAMES" ] || mapfile -t LIVE <<< "$PARSED_NAMES"
else
    NAMES="$(printf '%s' "$LIST_JSON" | sed -n 's/.*"databases":\[\(.*\)\].*/\1/p' | tr -d '" ' | tr ',' ' ')"
    # shellcheck disable=SC2206 # deliberate word split of the parsed list
    LIVE=($NAMES)
fi
[ "${#LIVE[@]}" -gt 0 ] || fail "no live databases (dg list returned none); refusing to render an empty config"

# Names land in a YAML file and in file paths; refuse anything outside
# the conservative charset rather than quote-and-hope. The gateway
# validates names at create time too -- this is belt and braces for a
# file a daemon parses.
SORTED=()
while IFS= read -r name; do SORTED+=("$name"); done < <(printf '%s\n' "${LIVE[@]}" | LC_ALL=C sort -u)
KEEP=()
SKIPPED=()
for name in "${SORTED[@]}"; do
    [[ "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] \
        || fail "live database name '$name' is outside the safe charset; refusing to render"
    if [ -f "$DG_DATA_DIR/${name}.sqlite" ]; then
        KEEP+=("$name")
    else
        # Registered but fileless: cannot be rendered (replication would
        # create an empty file at that path), but it is NOT covered --
        # --check must report it missing, never pass over it silently.
        SKIPPED+=("$name")
        log "warning: live database '$name' has no $DG_DATA_DIR/${name}.sqlite file; skipped"
    fi
done
[ "${#KEEP[@]}" -gt 0 ] || fail "no live database has a .sqlite file under $DG_DATA_DIR"

# The registry (metadata.sqlite: credential hashes, scopes, database
# rows) replicates as one fixed extra entry so the bucket alone can
# rebuild a working gateway -- data files without it lose every scoped
# key (docs/recovery-policy.md). Registry connections in the gateway
# are per-operation, like the application databases already replicated
# under a live gateway; restored-pair skew is handled by the recovery
# runbook in that policy. DG_REPLICATION_INCLUDE_REGISTRY=0 opts out.
INCLUDE_REGISTRY="${DG_REPLICATION_INCLUDE_REGISTRY:-1}"
REGISTRY_PRESENT=0
[ -f "$DG_DATA_DIR/metadata.sqlite" ] && REGISTRY_PRESENT=1
EXPECTED=("${KEEP[@]}")
if [ "$INCLUDE_REGISTRY" = "1" ] && [ "$REGISTRY_PRESENT" = "1" ]; then
    EXPECTED+=(metadata)
fi
# Explicit retention: with no replica retention configured, the tool
# default silently decides how much history the bucket keeps, and the
# Profile C 7-day frequent-points objective was not met by the old
# render. 168h of WAL + daily snapshots is the frequent tier; the
# 30/7 daily-version classes are bucket-lifecycle territory (policy
# doc records their production state separately).
RETENTION="${DG_REPLICATION_RETENTION:-168h}"
SNAPSHOT_INTERVAL="${DG_REPLICATION_SNAPSHOT_INTERVAL:-24h}"

# Where the copy is. Empty until the replica settings are read below; the
# file-replica mode has no bucket, and says so rather than naming one.
TARGET=""
BUCKET=""
ENDPOINT=""
REGION=""

# --- 2. Replica settings. ------------------------------------------------
render_one() { # render_one <name> -- one dbs: entry; name may be "metadata"
    local name="$1"
    echo "  - path: ${DG_DATA_DIR}/${name}.sqlite"
    echo "    replicas:"
    if [ -n "${DG_REPLICA_URL_TEMPLATE:-}" ]; then
        echo "      - url: ${DG_REPLICA_URL_TEMPLATE//\{name\}/$name}"
    else
        echo "      - url: s3://${BUC}/${name}?endpoint=${END}"
        echo "        region: ${REG}"
        echo "        access-key-id: \${REPLICATION_S3_ACCESS_KEY_ID}"
        echo "        secret-access-key: \${REPLICATION_S3_SECRET_ACCESS_KEY}"
        echo "        force-path-style: true"
    fi
    echo "        retention: ${RETENTION}"
    echo "        snapshot-interval: ${SNAPSHOT_INTERVAL}"
}
render() {
    echo "# Managed by replication-sync.sh from 'dg list' -- do not hand-edit."
    echo "# A database appears here within one sync interval of being created,"
    echo "# and disappears within one interval of being deleted. The metadata"
    echo "# entry is the registry (credentials/scopes) -- see recovery-policy."
    echo "dbs:"
    for name in "${EXPECTED[@]}"; do
        render_one "$name"
    done
}
if [ -z "${DG_REPLICA_URL_TEMPLATE:-}" ]; then
    # Anchored, non-eval parse of the env file (same discipline as the
    # installer): only the exact keys, values never echoed. The secret
    # is checked for presence only -- it is never substituted anywhere.
    [ -r "$ENV_FILE" ] || fail "env file not readable: $ENV_FILE"
    BUC=$(sed -n 's/^REPLICATION_S3_BUCKET=\(.*\)$/\1/p' "$ENV_FILE")
    END=$(sed -n 's/^REPLICATION_S3_ENDPOINT=\(.*\)$/\1/p' "$ENV_FILE")
    REG=$(sed -n 's/^REPLICATION_S3_REGION=\(.*\)$/\1/p' "$ENV_FILE")
    KID=$(sed -n 's/^REPLICATION_S3_ACCESS_KEY_ID=\(.*\)$/\1/p' "$ENV_FILE")
    SEC=$(sed -n 's/^REPLICATION_S3_SECRET_ACCESS_KEY=\(.*\)$/\1/p' "$ENV_FILE")
    [ -n "$BUC" ] || fail "REPLICATION_S3_BUCKET missing/empty in $ENV_FILE"
    [ -n "$END" ] || fail "REPLICATION_S3_ENDPOINT missing/empty in $ENV_FILE"
    [ -n "$REG" ] || fail "REPLICATION_S3_REGION missing/empty in $ENV_FILE"
    [ -n "$KID" ] || fail "REPLICATION_S3_ACCESS_KEY_ID missing/empty in $ENV_FILE"
    [ -n "$SEC" ] || fail "REPLICATION_S3_SECRET_ACCESS_KEY missing/empty in $ENV_FILE"
    unset KID SEC
    BUCKET="$BUC"
    ENDPOINT="$END"
    REGION="$REG"
    TARGET="s3://${BUC}"
else
    # A file replica has no bucket: the template names the target, and
    # saying "no bucket" is truer than naming an empty one.
    TARGET="${DG_REPLICA_URL_TEMPLATE:-}"
fi

DESIRED="$(render)"

# --- 3. What the host knows about the replica. ---------------------------
# The gateway cannot know where the bucket is: this script is what knows.
# Publishing it means the operator asks the gateway "where is my data" and
# gets the host's own answer rather than a blank.
#
# The credentials are never written: only the bucket, endpoint and region,
# which name a place rather than open it. The file is written atomically,
# so a reader never sees half of it.
sidecar_state() {
    if [ -z "$STATUS_CMD" ]; then echo "unknown"; return 0; fi
    local out
    out="$(bash -c "$STATUS_CMD" 2>/dev/null)" || out=""
    case "$out" in
        active) echo "active" ;;
        inactive|failed) echo "$out" ;;
        *) echo "unknown" ;;
    esac
}

publish_replica() {
    [ -d "$BACKUP_ROOT" ] || return 0
    local published_at sidecar
    published_at="$(date -u +%s)"
    sidecar="$(sidecar_state)"
    # The databases the replica covers, without the registry: the
    # registry is reported on its own line, because it is not a database
    # an operator would look for among them.
    local covered=()
    for name in "${EXPECTED[@]}"; do
        [ "$name" = "metadata" ] && continue
        covered+=("$name")
    done
    if ! python3 - "$BACKUP_ROOT/replica.json" "$published_at" "$TARGET" "$BUCKET" \
        "$ENDPOINT" "$REGION" "$RETENTION" "$SNAPSHOT_INTERVAL" "$sidecar" \
        "$INCLUDE_REGISTRY" "${covered[@]}" <<'PY'
import json
import os
import sys

(path, published_at, target, bucket, endpoint, region, retention, interval,
 sidecar, registry, *databases) = sys.argv[1:]
record = {
    "published_at": int(published_at),
    "target": target,
    "bucket": bucket or None,
    "endpoint": endpoint or None,
    "region": region or None,
    "retention": retention,
    "snapshot_interval": interval,
    "databases": databases,
    "registry": registry == "1",
    "sidecar": sidecar,
}
tmp = path + ".tmp"
with open(tmp, "w", encoding="utf-8") as stream:
    json.dump(record, stream, sort_keys=True)
    stream.write("\n")
    stream.flush()
    os.fsync(stream.fileno())
# The gateway reads this as another user, so it must be readable by it.
os.chmod(tmp, 0o644)
os.replace(tmp, path)
PY
    then
        log "warning: could not publish the replica status to $BACKUP_ROOT/replica.json"
        return 0
    fi
    # The publisher may run as root while the gateway reads as another
    # user, so the file is handed to the same owner the config has.
    [ -z "$CONFIG_OWNER" ] || chown "$CONFIG_OWNER" "$BACKUP_ROOT/replica.json" 2>/dev/null || true
}

if [ "$MODE" = "print" ]; then
    printf '%s\n' "$DESIRED"
    exit 0
fi

# Current config's database names, for the coverage report and the
# added/removed log lines. An unreadable config (permissions) is a
# hard error in apply mode, but --check should still say so plainly.
CURRENT_NAMES=()
if [ -f "$CONFIG" ]; then
    [ -r "$CONFIG" ] || fail "config exists but is not readable: $CONFIG"
    while IFS= read -r p; do
        CURRENT_NAMES+=("$(basename "$p" .sqlite)")
    done < <(sed -n 's/^[[:space:]]*- path:[[:space:]]*//p' "$CONFIG")
fi

in_list() { local x="$1"; shift; local y; for y in "$@"; do [ "$y" = "$x" ] && return 0; done; return 1; }
# Byte-exact config comparison (command substitution would strip a
# trailing-newline difference; cmp does not).
config_matches() { [ -f "$CONFIG" ] && printf '%s\n' "$DESIRED" | cmp -s - "$CONFIG"; }

if [ "$MODE" = "check" ]; then
    drift=0
    for name in "${EXPECTED[@]}"; do
        if in_list "$name" "${CURRENT_NAMES[@]+"${CURRENT_NAMES[@]}"}"; then
            echo "replication-coverage db=$name state=covered"
        else
            echo "replication-coverage db=$name state=missing"
            drift=1
        fi
    done
    for name in "${CURRENT_NAMES[@]+"${CURRENT_NAMES[@]}"}"; do
        # A configured registry whose file vanished is reported once, as
        # missing/no-registry-file below -- not also as a spurious extra.
        if [ "$name" = "metadata" ] && [ "$INCLUDE_REGISTRY" = "1" ] && [ "$REGISTRY_PRESENT" != "1" ]; then
            continue
        fi
        if ! in_list "$name" "${EXPECTED[@]}" && ! in_list "$name" "${SKIPPED[@]+"${SKIPPED[@]}"}"; then
            echo "replication-coverage db=$name state=extra"
            drift=1
        fi
    done
    for name in "${SKIPPED[@]+"${SKIPPED[@]}"}"; do
        echo "replication-coverage db=$name state=missing detail=no-database-file"
        drift=1
    done
    if [ "$INCLUDE_REGISTRY" = "1" ] && [ "$REGISTRY_PRESENT" != "1" ]; then
        echo "replication-coverage db=metadata state=missing detail=no-registry-file"
        drift=1
    fi
    # Same database set but different bytes (endpoint/region/URL change,
    # hand edit) is still drift the next apply would rewrite.
    if [ "$drift" = "0" ] && ! config_matches; then
        echo "replication-coverage state=drift detail=config-differs-from-rendered"
        drift=1
    fi
    [ "$drift" = "0" ] || exit 1
    exit 0
fi

# --- 3. Apply: replace only on change, restart, revert on failure. ------
if config_matches; then
    publish_replica
    log "in sync (${#EXPECTED[@]} database(s) incl. registry if present); no change"
    exit 0
fi

CONFIG_DIR="$(dirname "$CONFIG")"
TMP="$(mktemp "$CONFIG_DIR/.replication.yml.XXXXXX")" || fail "cannot create temp file in $CONFIG_DIR"
BACKUP=""
CONFIG_CREATED=0
RESTART_OK=1
# Single exit path owns the rollback: between the mv below and a
# successful restart, ANY exit (restart failure, or this process being
# signalled in that window) restores the previous config -- or removes
# a newly created one -- so the file never claims coverage the running
# daemon does not have while later syncs report "in sync". (SIGKILL
# cannot be trapped; that residue is irreducible in a shell script.)
on_exit() {
    rm -f "$TMP"
    if [ "$RESTART_OK" != "1" ]; then
        if [ -n "$BACKUP" ]; then mv -f "$BACKUP" "$CONFIG" 2>/dev/null || true;
        elif [ "$CONFIG_CREATED" = "1" ]; then rm -f "$CONFIG"; fi
    else
        rm -f ${BACKUP:+"$BACKUP"}
    fi
}
trap on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
printf '%s\n' "$DESIRED" >"$TMP"
chmod 0600 "$TMP"
[ -z "$CONFIG_OWNER" ] || chown "$CONFIG_OWNER" "$TMP" || fail "chown $CONFIG_OWNER failed"

ADDED=(); REMOVED=()
for name in "${EXPECTED[@]}"; do in_list "$name" "${CURRENT_NAMES[@]+"${CURRENT_NAMES[@]}"}" || ADDED+=("$name"); done
for name in "${CURRENT_NAMES[@]+"${CURRENT_NAMES[@]}"}"; do in_list "$name" "${EXPECTED[@]}" || REMOVED+=("$name"); done

# Keep the previous config until the restart succeeds. If the restart
# fails we roll the file back: otherwise the file would claim the new
# set while the daemon still runs the old one, and every later sync
# would see "in sync" and never retry the restart.
if [ -f "$CONFIG" ]; then
    BACKUP="$(mktemp "$CONFIG_DIR/.replication.yml.bak.XXXXXX")"
    cp -p "$CONFIG" "$BACKUP"
else
    CONFIG_CREATED=1
fi
RESTART_OK=0
mv -f "$TMP" "$CONFIG"
log "config updated: ${#EXPECTED[@]} database(s) incl. registry if present (added: ${ADDED[*]:-none}; removed: ${REMOVED[*]:-none})"

if [ -n "$RESTART_CMD" ]; then
    bash -c "$RESTART_CMD" \
        || fail "restart failed ($RESTART_CMD); previous config restored -- next sync will retry"
    log "sidecar restarted"
fi
RESTART_OK=1
# The sidecar is running the new config now, so this is what the replica
# is: publish it where the gateway can answer with it.
publish_replica
