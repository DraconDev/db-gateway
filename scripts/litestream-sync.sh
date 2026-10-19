#!/usr/bin/env bash
# Keep the litestream config in step with the gateway's live databases.
#
# Why this exists (docs/audit-2026-10-19.md A2): litestream reads its
# config once at startup, and scripts/install-litestream.sh renders that
# config from the `dg list` output at install time. A database created
# afterwards was silently never replicated -- and snapshot-set freshness
# could still report it `fresh`, so nothing surfaced the gap. This script
# is the durable fix: it re-renders the desired config from the live
# database list, atomically replaces the config only when it changed,
# and restarts the sidecar so the new config is the one actually running.
# Run on a timer (deploy/dg-litestream-sync.timer) it self-heals any
# drift -- creates, deletes, hand edits, missed events -- within one
# timer interval, and keeps the config file the single source of truth
# (which `litestream status`, and therefore host-audit, reads).
#
# Modes:
#   (default)  apply: replace + restart if the rendered config differs
#   --check    report per-database coverage; exit 1 on any drift.
#              Output lines: litestream-coverage db=<name> state=covered|missing|extra
#   --print    print the desired config to stdout; change nothing
#
# Configuration (environment; host defaults first):
#   DG_BIN                  dg binary            (default: dg)
#   DG_SERVER               optional --server URL for `dg list`
#   DG_KEY_FILE             owner credential     (default: /etc/dg/owner.key)
#   DG_LIST_AS              run `dg list` as this user via sudo -n -u
#                           (default: dg when running as root, else empty)
#   DG_DATA_DIR             database files       (default: /var/lib/dg)
#   LITESTREAM_CONFIG       config to manage     (default: /etc/litestream.yml)
#   LITESTREAM_ENV_FILE     S3 settings source   (default: /etc/dg/litestream.env)
#   DG_REPLICA_URL_TEMPLATE if set, replica URL per database with {name}
#                           substituted, e.g. file:///srv/replicas/{name};
#                           the S3 env file is then not read. Exists for
#                           local drills/rehearsals with a file replica.
#   LITESTREAM_RESTART_CMD  command run (via bash -c) after a config change
#                           (default: systemctl restart dg-litestream.service;
#                            empty string disables the restart)
#   LITESTREAM_CONFIG_OWNER chown target for the config (default: dg:dg;
#                            empty string skips chown)
#
# The S3 access/secret keys are never inlined: the rendered config keeps
# ${LITESTREAM_S3_*} references for the two credential keys (expanded by
# litestream from the sidecar's EnvironmentFile), exactly as the
# installer's render did. Only bucket/endpoint/region -- not secrets --
# are substituted from the env file, and they are never echoed.

set -euo pipefail
SELF="litestream-sync"
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
CONFIG="${LITESTREAM_CONFIG:-/etc/litestream.yml}"
ENV_FILE="${LITESTREAM_ENV_FILE:-/etc/dg/litestream.env}"
RESTART_CMD="${LITESTREAM_RESTART_CMD-systemctl restart dg-litestream.service}"
CONFIG_OWNER="${LITESTREAM_CONFIG_OWNER-dg:dg}"
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
')" || fail "could not parse 'dg list' JSON"
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
        # Registered but fileless: cannot be rendered (litestream would
        # create an empty file at that path), but it is NOT covered --
        # --check must report it missing, never pass over it silently.
        SKIPPED+=("$name")
        log "warning: live database '$name' has no $DG_DATA_DIR/${name}.sqlite file; skipped"
    fi
done
[ "${#KEEP[@]}" -gt 0 ] || fail "no live database has a .sqlite file under $DG_DATA_DIR"

# --- 2. Replica settings. ------------------------------------------------
render() {
    echo "# Managed by litestream-sync.sh from 'dg list' -- do not hand-edit."
    echo "# A database appears here within one sync interval of being created,"
    echo "# and disappears within one interval of being deleted."
    echo "dbs:"
    for name in "${KEEP[@]}"; do
        echo "  - path: ${DG_DATA_DIR}/${name}.sqlite"
        echo "    replicas:"
        if [ -n "${DG_REPLICA_URL_TEMPLATE:-}" ]; then
            echo "      - url: ${DG_REPLICA_URL_TEMPLATE//\{name\}/$name}"
        else
            echo "      - url: s3://${BUC}/${name}?endpoint=${END}"
            echo "        region: ${REG}"
            echo "        access-key-id: \${LITESTREAM_S3_ACCESS_KEY_ID}"
            echo "        secret-access-key: \${LITESTREAM_S3_SECRET_ACCESS_KEY}"
            echo "        force-path-style: true"
        fi
    done
}
if [ -z "${DG_REPLICA_URL_TEMPLATE:-}" ]; then
    # Anchored, non-eval parse of the env file (same discipline as the
    # installer): only the exact keys, values never echoed. The secret
    # is checked for presence only -- it is never substituted anywhere.
    [ -r "$ENV_FILE" ] || fail "env file not readable: $ENV_FILE"
    BUC=$(sed -n 's/^LITESTREAM_S3_BUCKET=\(.*\)$/\1/p' "$ENV_FILE")
    END=$(sed -n 's/^LITESTREAM_S3_ENDPOINT=\(.*\)$/\1/p' "$ENV_FILE")
    REG=$(sed -n 's/^LITESTREAM_S3_REGION=\(.*\)$/\1/p' "$ENV_FILE")
    KID=$(sed -n 's/^LITESTREAM_S3_ACCESS_KEY_ID=\(.*\)$/\1/p' "$ENV_FILE")
    SEC=$(sed -n 's/^LITESTREAM_S3_SECRET_ACCESS_KEY=\(.*\)$/\1/p' "$ENV_FILE")
    [ -n "$BUC" ] || fail "LITESTREAM_S3_BUCKET missing/empty in $ENV_FILE"
    [ -n "$END" ] || fail "LITESTREAM_S3_ENDPOINT missing/empty in $ENV_FILE"
    [ -n "$REG" ] || fail "LITESTREAM_S3_REGION missing/empty in $ENV_FILE"
    [ -n "$KID" ] || fail "LITESTREAM_S3_ACCESS_KEY_ID missing/empty in $ENV_FILE"
    [ -n "$SEC" ] || fail "LITESTREAM_S3_SECRET_ACCESS_KEY missing/empty in $ENV_FILE"
    unset KID SEC
fi

DESIRED="$(render)"

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
    for name in "${KEEP[@]}"; do
        if in_list "$name" "${CURRENT_NAMES[@]+"${CURRENT_NAMES[@]}"}"; then
            echo "litestream-coverage db=$name state=covered"
        else
            echo "litestream-coverage db=$name state=missing"
            drift=1
        fi
    done
    for name in "${CURRENT_NAMES[@]+"${CURRENT_NAMES[@]}"}"; do
        if ! in_list "$name" "${KEEP[@]}" && ! in_list "$name" "${SKIPPED[@]+"${SKIPPED[@]}"}"; then
            echo "litestream-coverage db=$name state=extra"
            drift=1
        fi
    done
    for name in "${SKIPPED[@]+"${SKIPPED[@]}"}"; do
        echo "litestream-coverage db=$name state=missing detail=no-database-file"
        drift=1
    done
    # Same database set but different bytes (endpoint/region/URL change,
    # hand edit) is still drift the next apply would rewrite.
    if [ "$drift" = "0" ] && ! config_matches; then
        echo "litestream-coverage state=drift detail=config-differs-from-rendered"
        drift=1
    fi
    [ "$drift" = "0" ] || exit 1
    exit 0
fi

# --- 3. Apply: replace only on change, restart, revert on failure. ------
if config_matches; then
    log "in sync (${#KEEP[@]} database(s)); no change"
    exit 0
fi

CONFIG_DIR="$(dirname "$CONFIG")"
TMP="$(mktemp "$CONFIG_DIR/.litestream.yml.XXXXXX")" || fail "cannot create temp file in $CONFIG_DIR"
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
for name in "${KEEP[@]}"; do in_list "$name" "${CURRENT_NAMES[@]+"${CURRENT_NAMES[@]}"}" || ADDED+=("$name"); done
for name in "${CURRENT_NAMES[@]+"${CURRENT_NAMES[@]}"}"; do in_list "$name" "${KEEP[@]}" || REMOVED+=("$name"); done

# Keep the previous config until the restart succeeds. If the restart
# fails we roll the file back: otherwise the file would claim the new
# set while the daemon still runs the old one, and every later sync
# would see "in sync" and never retry the restart.
if [ -f "$CONFIG" ]; then
    BACKUP="$(mktemp "$CONFIG_DIR/.litestream.yml.bak.XXXXXX")"
    cp -p "$CONFIG" "$BACKUP"
else
    CONFIG_CREATED=1
fi
RESTART_OK=0
mv -f "$TMP" "$CONFIG"
log "config updated: ${#KEEP[@]} database(s) (added: ${ADDED[*]:-none}; removed: ${REMOVED[*]:-none})"

if [ -n "$RESTART_CMD" ]; then
    bash -c "$RESTART_CMD" \
        || fail "restart failed ($RESTART_CMD); previous config restored -- next sync will retry"
    log "sidecar restarted"
fi
RESTART_OK=1
