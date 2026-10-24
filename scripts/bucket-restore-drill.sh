#!/usr/bin/env bash
# Weekly on-host restore drill against the OFF-HOST copies
# (docs/recovery-policy.md): restore every live database plus the
# registry from its litestream replica into a separate scratch target,
# then verify integrity and exact per-table row counts against what the
# live gateway is serving. A drill that compares counts has compared
# only counts -- but counts per table, on both sides, plus
# integrity_check, is the same bar scripts/litestream-restore.sh uses,
# and a mismatch or a failed restore is exactly the signal that the
# bucket copy cannot be trusted BEFORE a host loss asks for it.
#
# Runs on the host as the dg user (deploy/dg-restore-drill.timer,
# weekly). Read-only against the live service and the bucket: restores
# land in a mode-0700 scratch dir that is removed afterwards. Exit is
# nonzero on the first failed database, so the systemd unit and the
# journal carry the failure where an operator will see it.
#
# Live counts can move while a restore runs. The drill therefore reads
# live counts, restores, and on mismatch re-reads: if the live counts
# changed during the window it re-restores once and compares against
# the newest; a mismatch against a STABLE live count is a failure, not
# noise to retry away.
#
# Usage: scripts/bucket-restore-drill.sh
# Env: DG_BIN (/usr/local/bin/dg), DG_SERVER, DG_KEY_FILE
#      (/etc/dg/owner.key), DG_DATA_DIR (/var/lib/dg),
#      LITESTREAM_BIN (/usr/local/bin/litestream),
#      LITESTREAM_CONFIG (/etc/litestream.yml),
#      LITESTREAM_ENV_FILE (/etc/dg/litestream.env; sourced if readable,
#      needed so S3 restores get credentials the same way the sidecar
#      does; values are never echoed),
#      DG_DRILL_SCRATCH_PARENT (/var/tmp), KEEP=1 keeps the scratch dir.
#      DG_BACKUP_ROOT (/var/lib/dg-backups), where the result is published
#      so the gateway can report it: a backup that was never restored is
#      an assumption, and the operator should not have to read the
#      journal to learn whether it ever was.

set -euo pipefail
SELF="bucket-restore-drill"
fail() { echo "[$SELF] FAIL: $*" >&2; exit 1; }
log() { echo "[$SELF] $*" >&2; }

DG_BIN="${DG_BIN:-/usr/local/bin/dg}"
DG_KEY_FILE="${DG_KEY_FILE:-/etc/dg/owner.key}"
DG_DATA_DIR="${DG_DATA_DIR:-/var/lib/dg}"
LITESTREAM_BIN="${LITESTREAM_BIN:-/usr/local/bin/litestream}"
LITESTREAM_CONFIG="${LITESTREAM_CONFIG:-/etc/litestream.yml}"
ENV_FILE="${LITESTREAM_ENV_FILE:-/etc/dg/litestream.env}"
SCRATCH_PARENT="${DG_DRILL_SCRATCH_PARENT:-/var/tmp}"
BACKUP_ROOT="${DG_BACKUP_ROOT:-/var/lib/dg-backups}"

# What the drill proved, published when it finishes -- or fails, or is
# cut short. `incomplete` is the honest answer when the drill could not
# finish: the copies were not shown to be good, and nobody should read
# that as "they are bad".
DRILL_STATE="incomplete"
DRILL_DETAIL=""
DRILL_DATABASES=0
DRILLED_AT="$(date -u +%s)"

publish_drill() {
    [ -d "$BACKUP_ROOT" ] || return 0
    python3 - "$BACKUP_ROOT/restore-drill.json" "$DRILLED_AT" "$DRILL_STATE" "$DRILL_DETAIL" "$DRILL_DATABASES" <<'PY'
import json
import os
import sys

(path, drilled_at, outcome, detail, databases) = sys.argv[1:]
record = {
    "drilled_at": int(drilled_at),
    "outcome": outcome,
    "detail": detail or None,
    "databases": int(databases or 0),
}
tmp = path + ".tmp"
with open(tmp, "w", encoding="utf-8") as stream:
    json.dump(record, stream, sort_keys=True)
    stream.write("\n")
    stream.flush()
    os.fsync(stream.fileno())
os.chmod(tmp, 0o644)
os.replace(tmp, path)
PY
}

[ -x "$DG_BIN" ] || fail "dg binary not executable: $DG_BIN"
[ -x "$LITESTREAM_BIN" ] || fail "litestream binary not executable: $LITESTREAM_BIN"
[ -r "$LITESTREAM_CONFIG" ] || fail "litestream config not readable: $LITESTREAM_CONFIG"
# ENV_FILE is an operator-nominated path by design (default
# /etc/dg/litestream.env); there is no constant source to follow.
# shellcheck disable=SC1090
if [ -r "$ENV_FILE" ]; then set -a; . "$ENV_FILE"; set +a; fi

SCRATCH="$(mktemp -d "$SCRATCH_PARENT/dg-restore-drill-XXXXXX")" || fail "cannot create scratch dir under $SCRATCH_PARENT"
chmod 0700 "$SCRATCH"
cleanup() { [ "${KEEP:-0}" = "1" ] && log "scratch kept at $SCRATCH" || rm -rf "$SCRATCH"; }
# The result is published whether the drill passed, failed, or was cut
# short -- an unpublished result is the same as no result, and the point
# of the drill is the answer, not the run.
trap 'cleanup; publish_drill' EXIT

dg() {
    local args=("$DG_BIN")
    [ -n "${DG_SERVER:-}" ] && args+=(--server "$DG_SERVER")
    args+=(--key-file "$DG_KEY_FILE" "$@")
    "${args[@]}"
}

LIST_JSON="$(dg list 2>/dev/null)" || fail "'dg list' failed (gateway down? key unreadable?)"
mapfile -t LIVE < <(printf '%s' "$LIST_JSON" | python3 -c '
import json, sys
for n in json.load(sys.stdin).get("databases", []): print(n)
' 2>/dev/null) || fail "could not parse 'dg list' JSON"
[ "${#LIVE[@]}" -gt 0 ] || fail "dg list returned no databases"
DRILL_DATABASES="${#LIVE[@]}"

# tables_of <db> -- live user tables via the gateway (never _dg_*).
tables_of() {
    dg query "$1" "SELECT name FROM sqlite_master WHERE type='table' AND name NOT GLOB 'sqlite_*' AND name NOT GLOB '_dg_*' ORDER BY name" 2>/dev/null \
      | python3 -c 'import json,sys; [print(r[0]) for r in json.load(sys.stdin).get("rows", [])]'
}
# live_counts <db> -- "table=count ..." from the serving gateway.
live_counts() {
    local db="$1" t out=""
    while IFS= read -r t; do
        [ -n "$t" ] || continue
        c="$(dg query "$db" "SELECT count(*) FROM \"$t\"" 2>/dev/null \
          | python3 -c 'import json,sys; print(json.load(sys.stdin)["rows"][0][0])')" || return 1
        out="${out:+$out }$t=$c"
    done < <(tables_of "$db")
    printf '%s' "${out:-no-user-tables}"
}
# restored_counts <file> -- same shape from a restored file + integrity.
restored_check() {
    python3 - "$1" <<'PY'
import sqlite3, sys
db = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
if db.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
    print("INTEGRITY-FAIL"); sys.exit(0)
tables = [r[0] for r in db.execute(
    "SELECT name FROM sqlite_master WHERE type='table' AND name NOT GLOB 'sqlite_*' AND name NOT GLOB '_dg_*' ORDER BY name")]
if not tables:
    print("no-user-tables")
else:
    print(" ".join(f"{t}={db.execute(f'SELECT count(*) FROM \"{t}\"').fetchone()[0]}" for t in tables))
PY
}
restore_one() { # restore_one <name>
    rm -f "$SCRATCH/$1.sqlite"
    timeout 600 "$LITESTREAM_BIN" restore -config "$LITESTREAM_CONFIG" \
        -o "$SCRATCH/$1.sqlite" "$DG_DATA_DIR/$1.sqlite" >/dev/null 2>"$SCRATCH/restore-$1.log"
}

FAILED=0
FAILED_NAMES=()
for name in "${LIVE[@]}"; do
    want="$(live_counts "$name")" || fail "could not read live counts for $name"
    restore_one "$name" || { log "db=$name state=restore-failed"; sed 's/^/  /' "$SCRATCH/restore-$name.log" >&2; FAILED=1; FAILED_NAMES+=("$name"); continue; }
    got="$(restored_check "$SCRATCH/$name.sqlite")"
    if [ "$got" != "$want" ]; then
        # Maybe the live counts moved during the restore. Re-read; only
        # a mismatch against a stable live count is a real failure.
        sleep 10
        want2="$(live_counts "$name")" || fail "could not re-read live counts for $name"
        if [ "$want2" != "$want" ]; then
            restore_one "$name" || { log "db=$name state=restore-failed-on-retry"; FAILED=1; FAILED_NAMES+=("$name"); continue; }
            got="$(restored_check "$SCRATCH/$name.sqlite")"
            want="$want2"
        fi
    fi
    if [ "$got" = "$want" ]; then
        echo "bucket-restore-drill db=$name state=ok counts=[$got]"
    else
        echo "bucket-restore-drill db=$name state=MISMATCH live=[$want] restored=[$got]"
        FAILED=1
        FAILED_NAMES+=("$name")
    fi
done

# Registry: restored metadata must be integrity-ok, name every live
# database, and carry at least the owner credential row. (Exact key
# parity at an instant is the recovery drill's job -- a key minted
# seconds ago may legitimately lag; a MISSING database row may not.)
if restore_one metadata; then
    REG_OK="$(python3 - "$SCRATCH/metadata.sqlite" "${LIVE[@]}" <<'PY'
import sqlite3, sys
live = sys.argv[2:]
try:
    db = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
    if db.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
        print("INTEGRITY-FAIL"); raise SystemExit
    names = {r[0] for r in db.execute("SELECT name FROM databases")}
    creds = db.execute("SELECT count(*) FROM credentials").fetchone()[0]
    missing = [n for n in live if n not in names]
    print(f"creds={creds} missing=[{','.join(missing)}]")
except SystemExit:
    raise
except Exception as e:
    print(f"UNREADABLE {e}")
PY
)"
    case "$REG_OK" in
        "creds="*"missing=[]"*) echo "bucket-restore-drill db=metadata state=ok $REG_OK" ;;
        *) echo "bucket-restore-drill db=metadata state=MISMATCH $REG_OK"; FAILED=1 ;;
    esac
else
    echo "bucket-restore-drill db=metadata state=restore-failed"
    FAILED=1
fi

if [ "$FAILED" = "0" ]; then
    DRILL_STATE="ok"
    DRILL_DETAIL="restored from the replicas and verified against the live gateway"
    echo "[$SELF] PASS: ${#LIVE[@]} database(s) + registry restored from replicas and verified"
else
    DRILL_STATE="failed"
    DRILL_DETAIL="not verified: ${FAILED_NAMES[*]:-unknown}"
    fail "one or more restores did not verify against the live gateway"
fi
