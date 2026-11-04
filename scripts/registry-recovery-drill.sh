#!/usr/bin/env bash
# Local drill for the registry half of docs/recovery-policy.md (audit
# A5): prove the bucket alone can rebuild a working gateway -- registry
# (metadata.sqlite: credential hashes + scopes) AND data -- while
# replication runs against a LIVE gateway.
#
# This is deliberately harder than sandbox gate 11, which stopped the
# gateway before replicating so the registry/data pair was trivially
# consistent. Here the sidecar replicates metadata.sqlite while the
# gateway is serving, a SECOND scoped key is minted after replication
# starts, and the drill only proceeds once that key's credential row is
# visible in a restore of the registry replica. Recovery then starts
# from an EMPTY data directory: both files come out of the replica, a
# fresh gateway boots on them, and the scoped keys minted before the
# loss must still authenticate, read exact rows, and still be refused
# for a database outside their scope (a restored registry that accepted
# everything would pass a weaker test).
#
# Everything is local and disposable (file:// replica as the bucket
# stand-in, pinned replication 0.5.17, unusual loopback ports). No
# deployment-host, no bucket, no network beyond loopback.
#
# Usage: scripts/registry-recovery-drill.sh
# Env:   REPLICATION_BIN, DG_BIN, DG_DRILL_PORT (default 17433), KEEP=1

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SELF="registry-recovery-drill"
fail() { echo "[$SELF] FAIL: $*" >&2; exit 1; }
step() { echo "[$SELF] $*"; }

REPLICATION_BIN="${REPLICATION_BIN:-$HOME/.local/dg-sandbox/replication-0.5.17/replication}"
DG_BIN="${DG_BIN:-$ROOT/.cargo/target/debug/dg}"
PORT="${DG_DRILL_PORT:-17433}"
RPORT=$((PORT + 1))
SERVER="http://127.0.0.1:${PORT}"
RSERVER="http://127.0.0.1:${RPORT}"

[ -x "$REPLICATION_BIN" ] || fail "pinned replication not found at $REPLICATION_BIN (set REPLICATION_BIN)"
[ "$("$REPLICATION_BIN" version 2>/dev/null | head -1)" = "0.5.17" ] || fail "replication at $REPLICATION_BIN is not 0.5.17"
[ -x "$DG_BIN" ] || (cd "$ROOT" && timeout 300 cargo build --locked --quiet --bin dg) || fail "cargo build failed"

SB="$(mktemp -d "${TMPDIR:-/tmp}/dg-registry-drill-XXXXXX")"
PIDS=()
cleanup() {
    for p in "${PIDS[@]+"${PIDS[@]}"}"; do kill "$p" 2>/dev/null || true; done
    if [ -f "$SB/replication.pid" ]; then kill "$(cat "$SB/replication.pid")" 2>/dev/null || true; fi
    if [ "${KEEP:-0}" = "1" ]; then step "sandbox kept at $SB"; else rm -rf "$SB"; fi
}
trap cleanup EXIT
mkdir -p "$SB/data" "$SB/probe"
step "sandbox: $SB (live $SERVER, recovered $RSERVER)"

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

SYNC_ENV=(
    "DG_BIN=$DG_BIN" "DG_SERVER=$SERVER" "DG_KEY_FILE=$SB/owner.key" "DG_LIST_AS="
    "DG_DATA_DIR=$SB/data" "REPLICATION_CONFIG=$SB/replication.yml"
    "DG_REPLICA_URL_TEMPLATE=file://$SB/replicas/{name}"
    "REPLICATION_RESTART_CMD=$SB/restart-replication.sh" "REPLICATION_CONFIG_OWNER="
)
sync_run() { env "${SYNC_ENV[@]}" "$ROOT/scripts/replication-sync.sh" "$@"; }
dg() { "$DG_BIN" --server "$SERVER" --key-file "$SB/owner.key" "$@"; }
wait_for() {
    local secs="$1" desc="$2"; shift 2
    local i=0
    while [ "$i" -lt "$secs" ]; do "$@" >/dev/null 2>&1 && return 0; sleep 1; i=$((i + 1)); done
    fail "timed out (${secs}s) waiting for: $desc"
}
key_from_json() { python3 -c 'import json,sys; print(json.load(sys.stdin)["key"], end="")'; }

# --- 1. live gateway: data + first scoped key, THEN replication starts -
"$DG_BIN" init --data-dir "$SB/data" >"$SB/init.json" 2>"$SB/init.log" || fail "dg init failed"
(umask 077; python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["owner_key"], end="")' "$SB/init.json" >"$SB/owner.key")
chmod 0600 "$SB/owner.key"; rm -f "$SB/init.json"
"$DG_BIN" serve --listen "127.0.0.1:${PORT}" --data-dir "$SB/data" --key-file "$SB/owner.key" >"$SB/dg.log" 2>&1 &
PIDS+=($!)
wait_for 15 "gateway liveness" curl -fsS -m 2 "$SERVER/health/live"
dg create ledger_app >/dev/null
dg create other_app >/dev/null
dg query ledger_app "CREATE TABLE ledger (id INTEGER PRIMARY KEY, note TEXT)" >/dev/null
dg query ledger_app "INSERT INTO ledger VALUES (1, 'reg-alpha'), (2, 'reg-bravo')" >/dev/null
dg query other_app "CREATE TABLE t (a INTEGER)" >/dev/null
dg key create --role database_read_only --database ledger_app ledger_reader >"$SB/key1.json"
(umask 077; key_from_json <"$SB/key1.json" >"$SB/scoped1.key"); chmod 0600 "$SB/scoped1.key"; rm -f "$SB/key1.json"
sync_run 2>"$SB/sync1.log" || { cat "$SB/sync1.log" >&2; fail "first sync failed"; }
sync_run --check >"$SB/check1.out" || fail "check after first sync should pass"
grep -q "db=metadata state=covered" "$SB/check1.out" || fail "registry not in the rendered config"
grep -q "retention: 168h" "$SB/replication.yml" || fail "explicit retention missing from rendered config"
step "1 ok: replication running against the live gateway, registry included"

# --- 2. post-replication registry + data writes must reach the replica -
dg key create --role database_read_only --database ledger_app ledger_reader_two >"$SB/key2.json"
(umask 077; key_from_json <"$SB/key2.json" >"$SB/scoped2.key"); chmod 0600 "$SB/scoped2.key"; rm -f "$SB/key2.json"
dg query ledger_app "INSERT INTO ledger VALUES (3, 'reg-charlie')" >/dev/null
cred_count_in_replica() {
    rm -f "$SB/probe/metadata.sqlite"
    "$REPLICATION_BIN" restore -config "$SB/replication.yml" -o "$SB/probe/metadata.sqlite" \
        "$SB/data/metadata.sqlite" >/dev/null 2>&1 || return 1
    python3 - "$SB/probe/metadata.sqlite" <<'PY'
import sqlite3, sys
db = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
names = sorted(r[0] for r in db.execute("SELECT name FROM credentials"))
assert "ledger_reader_two" in names, f"second key not yet in replica registry: {names}"
PY
}
wait_for 60 "second scoped key visible in registry replica" cred_count_in_replica
step "2 ok: registry writes made under a live gateway reached the replica"

# --- 3. total loss: wipe the data dir; rebuild from the replica alone --
for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done; PIDS=()
kill "$(cat "$SB/replication.pid")" 2>/dev/null || true; sleep 1
mv "$SB/data" "$SB/data-lost"
mkdir -p "$SB/data"
for f in metadata ledger_app other_app; do
    "$REPLICATION_BIN" restore -config "$SB/replication.yml" -o "$SB/data/$f.sqlite" \
        "$SB/data/$f.sqlite" >"$SB/restore-$f.log" 2>&1 \
        || { cat "$SB/restore-$f.log" >&2; fail "restore of $f failed"; }
done
python3 - "$SB/data/ledger_app.sqlite" "$SB/data/metadata.sqlite" <<'PY' || fail "restored files fail integrity/content check"
import sqlite3, sys
db = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
assert db.execute("SELECT id, note FROM ledger ORDER BY id").fetchall() == \
    [(1, "reg-alpha"), (2, "reg-bravo"), (3, "reg-charlie")]
assert db.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
meta = sqlite3.connect(f"file:{sys.argv[2]}?mode=ro", uri=True)
assert meta.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
print("restored ledger + registry integrity ok")
PY
step "3 ok: registry and data restored from the replica alone"

# --- 4. recovered gateway: pre-loss keys still authoritative -----------
"$DG_BIN" serve --listen "127.0.0.1:${RPORT}" --data-dir "$SB/data" --key-file "$SB/owner.key" >"$SB/dg-recovered.log" 2>&1 &
PIDS+=($!)
wait_for 15 "recovered gateway liveness" curl -fsS -m 2 "$RSERVER/health/live"
LIST="$("$DG_BIN" --server "$RSERVER" --key-file "$SB/owner.key" list)" || fail "owner key rejected by recovered gateway"
printf '%s' "$LIST" | grep -q ledger_app || fail "ledger_app missing after recovery: $LIST"
for k in scoped1 scoped2; do
    ROWS="$("$DG_BIN" --server "$RSERVER" --key-file "$SB/$k.key" query ledger_app "SELECT count(*) FROM ledger")" \
        || fail "$k (minted before the loss) cannot read after recovery"
    printf '%s' "$ROWS" | grep -q '\[3\]' || fail "$k read wrong row count after recovery: $ROWS"
    if "$DG_BIN" --server "$RSERVER" --key-file "$SB/$k.key" query other_app "SELECT count(*) FROM t" >/dev/null 2>&1; then
        fail "$k reached other_app after recovery -- the restored registry is not enforcing scope"
    fi
done
step "4 ok: both pre-loss scoped keys authenticate with scope intact after bucket-alone recovery"

echo "[$SELF] PASS: live registry replication -> total loss -> replica-alone rebuild -> keys authoritative"
