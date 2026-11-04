#!/usr/bin/env bash
# Local drill for the A2 fix (docs/audit-2026-10-19.md): prove that a
# database created AFTER the replication sidecar is already running gets
# replicated and restores exactly, via scripts/replication-sync.sh --
# and that a deleted database leaves the config on the next sync.
#
# Everything is local and disposable: a `dg serve` on an unusual loopback
# port, a file:// replica as the bucket stand-in (each database gets its
# own replica prefix, per docs/backup-compatibility.md finding 2), and
# the pinned sidecar release (never a different sidecar that may be on PATH;
# gate 1 of the compatibility matrix). No deployment-host, no bucket, no network
# beyond loopback. The pass bar follows finding 1 of that matrix: a
# restore exit code is not evidence -- restored rows are compared
# value-for-value against what was written, plus integrity_check.
#
# Usage: scripts/replication-sync-drill.sh
# Env:   REPLICATION_BIN (default ~/.local/dg-sandbox/replication/replication)
#        DG_BIN         (default .cargo/target/debug/dg; built if missing)
#        DG_DRILL_PORT  (default 17432)
#        KEEP=1         keep the sandbox dir for inspection

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SELF="replication-sync-drill"
fail() { echo "[$SELF] FAIL: $*" >&2; exit 1; }
step() { echo "[$SELF] $*"; }

REPLICATION_BIN="${REPLICATION_BIN:-$HOME/.local/dg-sandbox/replication/replication}"
DG_BIN="${DG_BIN:-$ROOT/.cargo/target/debug/dg}"
PORT="${DG_DRILL_PORT:-17432}"
SERVER="http://127.0.0.1:${PORT}"

[ -x "$REPLICATION_BIN" ] || fail "pinned replication not found at $REPLICATION_BIN (set REPLICATION_BIN)"
LVER="$("$REPLICATION_BIN" version 2>/dev/null | head -1)"
if [ -n "${DG_REPLICATION_VERSION:-}" ] && [ "$LVER" != "${DG_REPLICATION_VERSION}" ]; then fail "replication at $REPLICATION_BIN is '$LVER', want ${DG_REPLICATION_VERSION}"; fi
if [ ! -x "$DG_BIN" ]; then
    step "building dg (debug)"
    (cd "$ROOT" && timeout 300 cargo build --locked --quiet --bin dg) || fail "cargo build failed"
fi

SB="$(mktemp -d "${TMPDIR:-/tmp}/dg-sync-drill-XXXXXX")"
DG_PID=""
cleanup() {
    [ -n "$DG_PID" ] && kill "$DG_PID" 2>/dev/null || true
    if [ -f "$SB/replication.pid" ]; then kill "$(cat "$SB/replication.pid")" 2>/dev/null || true; fi
    if [ "${KEEP:-0}" = "1" ]; then step "sandbox kept at $SB"; else rm -rf "$SB"; fi
}
trap cleanup EXIT
mkdir -p "$SB/data" "$SB/restored"
step "sandbox: $SB (server $SERVER, replication $LVER)"

# --- restart shim: (re)starts the local replication replicate process. ---
# The sync invokes this after any config change, standing in for
# `systemctl restart dg-replication.service` on the host.
cat >"$SB/restart-replication.sh" <<EOF
#!/usr/bin/env bash
set -eu
if [ -f "$SB/replication.pid" ]; then kill "\$(cat "$SB/replication.pid")" 2>/dev/null || true; sleep 1; fi
"$REPLICATION_BIN" replicate -config "$SB/replication.yml" >>"$SB/replication.log" 2>&1 &
echo \$! >"$SB/replication.pid"
sleep 1
kill -0 "\$(cat "$SB/replication.pid")"
EOF
chmod +x "$SB/restart-replication.sh"

# --- sync environment (the same knobs the timer unit relies on) -------
SYNC_ENV=(
    "DG_BIN=$DG_BIN"
    "DG_SERVER=$SERVER"
    "DG_KEY_FILE=$SB/owner.key"
    "DG_LIST_AS="
    "DG_DATA_DIR=$SB/data"
    "REPLICATION_CONFIG=$SB/replication.yml"
    "DG_REPLICA_URL_TEMPLATE=file://$SB/replicas/{name}"
    "REPLICATION_RESTART_CMD=$SB/restart-replication.sh"
    "REPLICATION_CONFIG_OWNER="
)
sync_run() { env "${SYNC_ENV[@]}" "$ROOT/scripts/replication-sync.sh" "$@"; }
dg() { "$DG_BIN" --server "$SERVER" --key-file "$SB/owner.key" "$@"; }

wait_for() { # wait_for <seconds> <description> <cmd...>
    local secs="$1" desc="$2"; shift 2
    local i=0
    while [ "$i" -lt "$secs" ]; do
        if "$@" >/dev/null 2>&1; then return 0; fi
        sleep 1; i=$((i + 1))
    done
    fail "timed out (${secs}s) waiting for: $desc"
}

# --- 1. gateway up, first database, sidecar replicating it -------------
# `dg init` emits JSON ({"owner_key": "<64 hex>"}); the key file must
# hold the raw credential, so extract it without printing it.
"$DG_BIN" init --data-dir "$SB/data" >"$SB/init.json" 2>"$SB/init.log" || fail "dg init failed"
(umask 077; python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["owner_key"], end="")' "$SB/init.json" >"$SB/owner.key") \
    || fail "could not extract owner key from dg init output"
chmod 0600 "$SB/owner.key"; rm -f "$SB/init.json"
"$DG_BIN" serve --listen "127.0.0.1:${PORT}" --data-dir "$SB/data" --key-file "$SB/owner.key" \
    >"$SB/dg.log" 2>&1 &
DG_PID=$!
wait_for 15 "gateway liveness" curl -fsS -m 2 "$SERVER/health/live"
dg create drill_one >/dev/null || fail "create drill_one failed"
dg query drill_one "CREATE TABLE t (id INTEGER PRIMARY KEY, note TEXT)" >/dev/null
dg query drill_one "INSERT INTO t VALUES (1, 'one')" >/dev/null
sync_run 2>"$SB/sync1.log" || { cat "$SB/sync1.log" >&2; fail "first sync failed"; }
sync_run --check >"$SB/check1.out" || fail "check after first sync should pass"
grep -q "db=drill_one state=covered" "$SB/check1.out" || fail "drill_one not covered after first sync"
wait_for 30 "drill_one replica data" bash -c "ls '$SB'/replicas/drill_one/ltx/*/*.ltx >/dev/null 2>&1"
step "1 ok: drill_one created, synced, replicating"

# --- 2. THE A2 MOMENT: a new database appears under a running sidecar --
dg create drill_two >/dev/null || fail "create drill_two failed"
dg query drill_two "CREATE TABLE t (id INTEGER PRIMARY KEY, note TEXT)" >/dev/null
dg query drill_two "INSERT INTO t VALUES (1, 'alpha'), (2, 'bravo'), (3, 'charlie')" >/dev/null
if sync_run --check >"$SB/check2.out" 2>/dev/null; then
    fail "--check passed with drill_two missing from the config (the A2 gap would be invisible)"
fi
grep -q "db=drill_two state=missing" "$SB/check2.out" || { cat "$SB/check2.out" >&2; fail "--check did not name drill_two as missing"; }
step "2a ok: gap is visible (--check fails, names drill_two)"
sync_run 2>"$SB/sync2.log" || { cat "$SB/sync2.log" "$SB/replication.log" >&2; fail "sync after create failed"; }
sync_run --check >/dev/null || fail "check after second sync should pass"
wait_for 30 "drill_two replica data" bash -c "ls '$SB'/replicas/drill_two/ltx/*/*.ltx >/dev/null 2>&1"
step "2b ok: drill_two replicated after sync (sidecar restarted onto new config)"

# --- 3. lose drill_two locally; restore it from the replica alone ------
kill "$DG_PID"; wait "$DG_PID" 2>/dev/null || true; DG_PID=""
if [ -f "$SB/replication.pid" ]; then kill "$(cat "$SB/replication.pid")" 2>/dev/null || true; sleep 1; fi
rm -f "$SB/data/drill_two.sqlite" "$SB/data/drill_two.sqlite-wal" "$SB/data/drill_two.sqlite-shm"
[ ! -e "$SB/data/drill_two.sqlite" ] || fail "drill_two deletion failed"
"$REPLICATION_BIN" restore -config "$SB/replication.yml" -o "$SB/restored/drill_two.sqlite" \
    "$SB/data/drill_two.sqlite" >"$SB/restore.log" 2>&1 \
    || { cat "$SB/restore.log" >&2; fail "replication restore failed"; }
python3 - "$SB/restored/drill_two.sqlite" <<'PY' || fail "restored drill_two content mismatch (exit code alone is not evidence)"
import sqlite3, sys
db = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
rows = db.execute("SELECT id, note FROM t ORDER BY id").fetchall()
assert rows == [(1, "alpha"), (2, "bravo"), (3, "charlie")], f"rows: {rows}"
assert db.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
print("restored rows exact + integrity ok")
PY
step "3 ok: drill_two restored from replica with exact rows"

# --- 4. delete drill_one; the next sync unregisters it -----------------
# Restore drill_two's file so the gateway restarts over a consistent set,
# then restart the gateway (the registry still names both databases).
cp "$SB/restored/drill_two.sqlite" "$SB/data/drill_two.sqlite"
"$DG_BIN" serve --listen "127.0.0.1:${PORT}" --data-dir "$SB/data" --key-file "$SB/owner.key" \
    >>"$SB/dg.log" 2>&1 &
DG_PID=$!
wait_for 15 "gateway liveness (restart)" curl -fsS -m 2 "$SERVER/health/live"
dg delete drill_one --confirm drill_one >/dev/null || fail "delete drill_one failed"
sync_run 2>"$SB/sync3.log" || { cat "$SB/sync3.log" >&2; fail "sync after delete failed"; }
grep -q "drill_one" "$SB/replication.yml" && fail "drill_one still in config after delete+sync"
sync_run --check >/dev/null || fail "check after delete-sync should pass"
step "4 ok: delete removes drill_one from the config on the next sync"

echo "[$SELF] PASS: create -> visible gap -> sync -> replicated -> restored exactly; delete -> unregistered"
