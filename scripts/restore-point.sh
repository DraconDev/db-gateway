#!/usr/bin/env bash
# The point-in-time restore points the off-host replica offers, and the
# result of the last restore taken from it.
#
# Usage:
#   scripts/restore-point.sh --list
#   scripts/restore-point.sh --plan  --to <ISO8601> --database <name>
#   scripts/restore-point.sh --apply --to <ISO8601> --database <name> [--out DIR]
#
# WHAT THIS IS FOR. "There is a backup" is a claim; "you can go back to
# any moment in this window" is a different one, and only the second one
# is useful at 2am. The replica keeps seven days of WAL, so the moments
# that can be restored to are knowable -- but only the host knows them:
# the bucket, the sidecar and the credentials are all the host's. So the
# host publishes them, exactly the way scripts/litestream-sync.sh
# publishes where the copy is and scripts/bucket-restore-drill.sh
# publishes what the drill proved, and the gateway reports them.
#
# The credentials never leave /etc/dg/litestream.env: they are read into
# this process the way the sidecar's own systemd unit reads them, and
# nothing this script prints or publishes carries them.
#
# Env:
#   LITESTREAM_BIN        litestream binary     (default /usr/local/bin/litestream)
#   LITESTREAM_CONFIG     litestream config     (default /etc/litestream.yml)
#   LITESTREAM_ENV_FILE   bucket credentials   (default /etc/dg/litestream.env)
#   DG_BACKUP_ROOT        where the facts are published
#                                                (default /var/lib/dg-backups)
#   DG_DATA_DIR           database files        (default /var/lib/dg)
#   RESTORE_ROOT          where restored copies land
#                                                (default /var/lib/dg-restores)
#   DG_LITESTREAM_STATUS_CMD command that reports the sidecar's state
#                         (default: systemctl is-active dg-litestream.service;
#                          empty string disables the check)
#
# THE THREE MODES.
#   --list   publishes the restore window: for every replicated database,
#            the files the replica holds and the span they cover.
#   --plan   asks litestream which files a restore to <ISO8601> would
#            fetch, and prints the plan. It writes nothing.
#   --apply  restores one database to <ISO8601> into a FRESH directory,
#            verifies it, and publishes the result. It never writes over a
#            live database: a restore is a copy you compare and then
#            import, not a silent replacement.

set -euo pipefail

SELF="restore-point"

LITESTREAM_BIN="${LITESTREAM_BIN:-/usr/local/bin/litestream}"
LITESTREAM_CONFIG="${LITESTREAM_CONFIG:-/etc/litestream.yml}"
LITESTREAM_ENV_FILE="${LITESTREAM_ENV_FILE:-/etc/dg/litestream.env}"
DG_BACKUP_ROOT="${DG_BACKUP_ROOT:-/var/lib/dg-backups}"
DG_DATA_DIR="${DG_DATA_DIR:-/var/lib/dg}"
RESTORE_ROOT="${RESTORE_ROOT:-/var/lib/dg-restores}"
STATUS_CMD="${DG_LITESTREAM_STATUS_CMD-systemctl is-active dg-litestream.service}"

MODE=""
TIMESTAMP=""
DATABASE=""
OUT_DIR=""

log() { echo "[$SELF] $*" >&2; }
fail() {
    echo "[$SELF] FAIL: $*" >&2
    exit 1
}

# --- arguments -------------------------------------------------------------

while [ $# -gt 0 ]; do
    case "$1" in
        --list) MODE="list" ;;
        --plan) MODE="plan" ;;
        --apply) MODE="apply" ;;
        --to)
            [ $# -ge 2 ] || fail "--to needs an ISO 8601 timestamp"
            TIMESTAMP="$2"
            shift
            ;;
        --database)
            [ $# -ge 2 ] || fail "--database needs a database name"
            DATABASE="$2"
            shift
            ;;
        --out)
            [ $# -ge 2 ] || fail "--out needs a directory"
            OUT_DIR="$2"
            shift
            ;;
        -h | --help)
            sed -n '2,45p' "$0"
            exit 0
            ;;
        *) fail "unknown argument: $1" ;;
    esac
    shift
done

[ -n "$MODE" ] || fail "one of --list, --plan or --apply is required"
[ -x "$LITESTREAM_BIN" ] || fail "litestream binary not executable: $LITESTREAM_BIN"
[ -r "$LITESTREAM_CONFIG" ] || fail "litestream config not readable: $LITESTREAM_CONFIG"

# The credentials go into this process's environment the way the sidecar's
# systemd unit puts them there: sourced, exported, never echoed.
if [ -r "$LITESTREAM_ENV_FILE" ]; then
    set -a
    # shellcheck disable=SC1090
    . "$LITESTREAM_ENV_FILE"
    set +a
fi

# --- the databases the replica covers --------------------------------------

# The litestream config is the list of what is replicated, and it includes
# the registry -- the metadata database that holds the credentials and
# scopes, which the gateway does not serve as a database but without which
# the copies are files no key can open. Only the `path:` lines are read;
# the credential lines are not matched by the pattern and are never
# printed.
config_paths() {
    python3 - "$LITESTREAM_CONFIG" <<'PY'
import re
import sys

pattern = re.compile(r"^\s*-\s*path:\s*(\S+)\s*$")
with open(sys.argv[1], encoding="utf-8") as stream:
    for line in stream:
        match = pattern.match(line)
        if match:
            print(match.group(1))
PY
}

# The name a database is served under: the file's basename without the
# .sqlite suffix. That is the name the gateway and the operator use.
db_name_of() {
    basename "$1" .sqlite
}

# The sidecar's state. A configured replica whose sidecar is not running
# is the failure that looks most like success from a distance, so the
# published window carries it.
sidecar_state() {
    if [ -z "$STATUS_CMD" ]; then
        echo "unknown"
        return 0
    fi
    local out
    out="$(bash -c "$STATUS_CMD" 2>/dev/null)" || out=""
    case "$out" in
        active) echo "active" ;;
        inactive | failed) echo "$out" ;;
        *) echo "unknown" ;;
    esac
}

# --- publishing ------------------------------------------------------------

# Published facts are written atomically and readable by the gateway,
# which reads them as another user.
publish() {
    # publish <file-name> <json-on-stdin>
    local name="$1"
    local tmp="$DG_BACKUP_ROOT/$name.tmp"
    [ -d "$DG_BACKUP_ROOT" ] || return 0
    cat >"$tmp"
    chmod 0644 "$tmp"
    mv -f "$tmp" "$DG_BACKUP_ROOT/$name"
}

# --- --list: the window the replica offers --------------------------------

if [ "$MODE" = "list" ]; then
    WORK="$(mktemp -d)"
    trap 'rm -rf "$WORK"' EXIT

    while IFS= read -r path; do
        [ -n "$path" ] || continue
        name="$(db_name_of "$path")"
        # `litestream ltx` lists the files the replica holds for one
        # database: the snapshot that anchors it and the WAL segments
        # after it. A database with nothing in the replica is reported as
        # such rather than omitted, because "absent from the list" and
        # "not replicated" are different answers.
        #
        # `-level all` is not optional. `litestream ltx` lists the
        # DEFAULT level only, and level 0 is the newest segment of one
        # database -- so asking for it published one row per database and
        # a window that was a single instant. The bucket holds the whole
        # chain: the snapshot at level 9 and the compacted WAL at levels
        # 1, 2 and 3, which is what covers the hours and days between
        # snapshots. Without them the published window was hours shorter
        # than the replica's real one, and every moment inside it was
        # reported as "the window opens later" -- moments litestream
        # itself would happily restore.
        if "$LITESTREAM_BIN" ltx -level all -config "$LITESTREAM_CONFIG" "$path" \
            >"$WORK/$name.ltx" 2>"$WORK/$name.err"; then
            :
        else
            log "warning: could not list the replica files for $name"
            : >"$WORK/$name.ltx"
        fi
    done < <(config_paths)

    PUBLISHED_AT="$(date -u +%s)"
    SIDECAR="$(sidecar_state)"

    python3 - "$WORK" "$PUBLISHED_AT" "$SIDECAR" <<'PY' | publish restore-points.json
import json
import os
import sys

work, published_at, sidecar = sys.argv[1], int(sys.argv[2]), sys.argv[3]

databases = {}
for entry in sorted(os.listdir(work)):
    if not entry.endswith(".ltx"):
        continue
    name = entry[: -len(".ltx")]
    files = []
    with open(os.path.join(work, entry), encoding="utf-8") as stream:
        for line in stream:
            parts = line.split()
            # level, min_txid, max_txid, size, created
            if len(parts) < 5 or not parts[0].isdigit():
                continue
            files.append(
                {
                    "level": int(parts[0]),
                    "min_txid": parts[1],
                    "max_txid": parts[2],
                    "size": int(parts[3]),
                    "timestamp": parts[4],
                }
            )
    stamps = sorted(f["timestamp"] for f in files)
    databases[name] = {
        "oldest": stamps[0] if stamps else None,
        "newest": stamps[-1] if stamps else None,
        "files": len(files),
        "bytes": sum(f["size"] for f in files),
        "inventory": files,
    }

stamps = sorted(
    stamp
    for db in databases.values()
    for stamp in (db["oldest"], db["newest"])
    if stamp
)
record = {
    "published_at": published_at,
    "window_start": stamps[0] if stamps else None,
    "window_end": stamps[-1] if stamps else None,
    "sidecar": sidecar,
    "databases": databases,
}
json.dump(record, sys.stdout, sort_keys=True)
sys.stdout.write("\n")
PY
    log "published the restore window to $DG_BACKUP_ROOT/restore-points.json"
    exit 0
fi

# --- --plan and --apply need a database and a moment -----------------------

[ -n "$DATABASE" ] || fail "--database is required"
[ -n "$TIMESTAMP" ] || fail "--to is required"

# The database name is the gateway's own rule. It is used to build a
# path, so it is checked before it is used, not after.
case "$DATABASE" in
    *[!a-z0-9_]* | "") fail "invalid database name: $DATABASE" ;;
esac

# The timestamp is ISO 8601, which is what litestream takes. It is parsed
# here too, so a typo is refused before the bucket is touched.
python3 - "$TIMESTAMP" <<'PY' || fail "invalid --to timestamp: $TIMESTAMP (expected ISO 8601, e.g. 2026-10-20T09:30:00Z)"
import datetime
import sys

text = sys.argv[1]
if text.endswith("Z"):
    text = text[:-1] + "+00:00"
try:
    datetime.datetime.fromisoformat(text)
except ValueError:
    sys.exit(1)
PY

DB_PATH="$DG_DATA_DIR/$DATABASE.sqlite"
[ -f "$DB_PATH" ] || fail "no such database on this host: $DATABASE"

# --- --plan: what a restore to that moment would fetch --------------------

if [ "$MODE" = "plan" ]; then
    echo "restore plan: $DATABASE at $TIMESTAMP"
    echo "source: $LITESTREAM_CONFIG"
    echo "target: ${RESTORE_ROOT}/<timestamp>/$DATABASE.sqlite (a fresh directory; a live database is never written)"
    echo
    if ! "$LITESTREAM_BIN" restore -config "$LITESTREAM_CONFIG" \
        -dry-run -timestamp "$TIMESTAMP" "$DB_PATH"; then
        fail "no restore is available for $DATABASE at $TIMESTAMP"
    fi
    exit 0
fi

# The restored copy is verified, not trusted: integrity first, then the
# shape and size of the data it carries. A function rather than a heredoc
# inside a command substitution, so the payload is readable on its own.
verify_restored() {
    python3 - "$1" <<'PY'
import json
import sqlite3
import sys

con = sqlite3.connect("file:" + sys.argv[1] + "?mode=ro", uri=True)
try:
    integrity = con.execute("PRAGMA integrity_check").fetchone()[0]
    tables = [
        row[0]
        for row in con.execute(
            "select name from sqlite_master where type='table' "
            "and name not glob 'sqlite_*' "
            "and name not glob '_dg_*' "
            "and name not glob '_litestream_*' "
            "order by name"
        )
    ]
    rows = 0
    counts = {}
    for name in tables:
        count = con.execute('select count(*) from "%s"' % name).fetchone()[0]
        counts[name] = count
        rows += count
    json.dump(
        {"integrity": integrity, "tables": len(tables), "rows": rows, "counts": counts},
        sys.stdout,
    )
finally:
    con.close()
PY
}

# --- --apply: restore, verify, publish ------------------------------------

STAMP="$(printf '%s' "$TIMESTAMP" | tr -cd '0-9')"
[ -n "$STAMP" ] && [ "${#STAMP}" -ge 8 ] || fail "could not read a target directory name from: $TIMESTAMP"
TARGET_DIR="${OUT_DIR:-$RESTORE_ROOT/$STAMP}"
TARGET="$TARGET_DIR/$DATABASE.sqlite"

# A restore never overwrites. An existing file means the operator asked
# twice, or something else owns that path, and either way writing over it
# would destroy the only copy of the evidence.
[ ! -e "$TARGET" ] || fail "refusing to overwrite an existing file: $TARGET"

# The restore root belongs to the service user, but it sits under
# /var/lib, which the service user cannot create in: the install step
# makes it, the way it makes the backup root. When it is missing, the
# message says exactly how to make it rather than failing on a
# permission error nobody can act on.
if [ ! -d "$RESTORE_ROOT" ]; then
    mkdir -p "$RESTORE_ROOT" 2>/dev/null ||
        fail "the restore root $RESTORE_ROOT does not exist and could not be created; an administrator must run: install -d -m 0700 -o dg -g dg $RESTORE_ROOT"
fi
chmod 0700 "$RESTORE_ROOT" 2>/dev/null || true

mkdir -p "$TARGET_DIR" || fail "could not create the restore directory: $TARGET_DIR"
chmod 0700 "$TARGET_DIR"

RESTORED_AT="$(date -u +%s)"
OUTCOME="failed"
DETAIL=""
TABLES=0
ROWS=0
SIZE=0
INTEGRITY="unknown"

# The restore is the one thing that reads the bucket with the real
# credentials, so it is the one thing that can fail for reasons that
# have nothing to do with the data.
if "$LITESTREAM_BIN" restore -config "$LITESTREAM_CONFIG" \
    -o "$TARGET" -timestamp "$TIMESTAMP" "$DB_PATH" 2>"$TARGET_DIR/restore.log"; then
    if [ -s "$TARGET" ]; then
        # The restored copy is data: it gets the same mode as every other
        # copy of a database on this host.
        chmod 0600 "$TARGET" 2>/dev/null || true
        # The restored copy is verified, not trusted: integrity first,
        # then the shape and size of the data it carries.
        if VERIFY="$(verify_restored "$TARGET" 2>&1)"; then
            INTEGRITY="$(printf '%s' "$VERIFY" | python3 -c 'import json,sys; print(json.load(sys.stdin)["integrity"])')"
            TABLES="$(printf '%s' "$VERIFY" | python3 -c 'import json,sys; print(json.load(sys.stdin)["tables"])')"
            ROWS="$(printf '%s' "$VERIFY" | python3 -c 'import json,sys; print(json.load(sys.stdin)["rows"])')"
            if [ "$INTEGRITY" = "ok" ]; then
                OUTCOME="ok"
                DETAIL="restored and verified; compare it, then import it"
            else
                OUTCOME="failed"
                DETAIL="integrity_check said '$INTEGRITY'"
            fi
        else
            OUTCOME="failed"
            DETAIL="the restored copy could not be read"
        fi
        SIZE="$(wc -c <"$TARGET" | tr -d ' ')"
    else
        OUTCOME="failed"
        DETAIL="litestream reported success but wrote no file"
    fi
else
    OUTCOME="failed"
    DETAIL="litestream could not restore that moment; see $TARGET_DIR/restore.log"
fi

python3 - "$DG_BACKUP_ROOT" "$RESTORED_AT" "$DATABASE" "$TIMESTAMP" "$OUTCOME" \
    "$DETAIL" "$TABLES" "$ROWS" "$SIZE" "$INTEGRITY" "$TARGET" <<'PY' | publish restore-results.json
import json
import os
import sys

(
    root,
    restored_at,
    database,
    timestamp,
    outcome,
    detail,
    tables,
    rows,
    size,
    integrity,
    path,
) = sys.argv[1:]
record = {
    "restored_at": int(restored_at),
    "database": database,
    "timestamp": timestamp,
    "outcome": outcome,
    "detail": detail or None,
    "tables": int(tables),
    "rows": int(rows),
    "size_bytes": int(size),
    "integrity": integrity,
    "path": path,
}
json.dump(record, sys.stdout, sort_keys=True)
sys.stdout.write("\n")
PY

if [ "$OUTCOME" = "ok" ]; then
    log "restored $DATABASE to $TIMESTAMP into $TARGET ($TABLES tables, $ROWS rows)"
    exit 0
fi
fail "the restore of $DATABASE to $TIMESTAMP did not succeed: $DETAIL"
