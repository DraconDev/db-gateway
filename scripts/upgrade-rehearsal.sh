#!/usr/bin/env bash
# Upgrade rehearsal: snapshot, swap binary debug->release, verify rows,
# roll back release->debug, verify again (dummy data, local only).
#
# Usage: scripts/upgrade-rehearsal.sh <debug-bin> <release-bin>
#
# Fails (nonzero exit) unless rows are byte-exact across both swaps and
# each generation's own write survives the next restart.
set -euo pipefail

DEBUG_BIN="$1"
RELEASE_BIN="$2"
DIR=$(mktemp -d /tmp/upgrade-rehearsal-XXXXXX)
trap 'kill "$SRV" 2>/dev/null || true' EXIT

KEY=$("$DEBUG_BIN" init --data-dir "$DIR" | python3 -c 'import json,sys; print(json.load(sys.stdin)["owner_key"])')

serve() {
    "$1" serve --listen 127.0.0.1:0 --data-dir "$DIR" >"$DIR/srv.log" 2>&1 &
    SRV=$!
    for _ in $(seq 1 100); do
        ADDR=$(grep -o '"address":"[^"]*"' "$DIR/srv.log" | head -1 | cut -d'"' -f4 || true)
        [ -n "${ADDR:-}" ] && break
        sleep 0.1
    done
    echo "serving $2 on $ADDR (pid $SRV)"
}

req() { # method path body [idem-key]
    local args=(--silent --show-error --fail-with-body -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json')
    [ "${4:-}" != "" ] && args+=(-H "Idempotency-Key: $4")
    curl "${args[@]}" -X "$1" -d "$3" "http://$ADDR$2"
}

stop() { kill "$SRV"; wait "$SRV" 2>/dev/null || true; : >"$DIR/srv.log"; }

serve "$DEBUG_BIN" debug
req POST /v1/databases '{"name":"up"}' > /dev/null
req POST /v1/databases/up/query '{"sql":"CREATE TABLE d(id INTEGER PRIMARY KEY, v TEXT)"}' seed-ddl > /dev/null
for off in 0 32; do
  BATCH=$(python3 -c "import json; print(json.dumps({'statements': [{'sql': \"INSERT INTO d(v) VALUES ('seed-%d')\" % i} for i in range($off, min($off+32, 50))], 'atomic': True}))")
  req POST /v1/databases/up/batch "$BATCH" batch-seed-$off > /dev/null
done
req POST /v1/databases/up/query '{"sql":"SELECT id, v FROM d ORDER BY id"}' > "$DIR/dump_debug.json"
echo "debug rows: $(python3 -c "import json; print(len(json.load(open('$DIR/dump_debug.json'))['rows']))")"
req POST /v1/databases/up/export '{}' | python3 -c 'import json,sys; print("snapshot sha:", json.load(sys.stdin)["sha256"])'
stop

serve "$RELEASE_BIN" release
req POST /v1/databases/up/query '{"sql":"SELECT id, v FROM d ORDER BY id"}' > "$DIR/dump_release.json"
python3 - "$DIR/dump_debug.json" "$DIR/dump_release.json" <<'EOF'
import json,sys
a=json.load(open(sys.argv[1]))["rows"]; b=json.load(open(sys.argv[2]))["rows"]
assert a==b, "release rows differ after upgrade"
print(f"upgrade exact: {len(b)} rows match")
EOF
req POST /v1/databases/up/query '{"sql":"INSERT INTO d(v) VALUES ('"'"'release-gen'"'"')"}' rel-1 > /dev/null
req POST /v1/databases/up/query '{"sql":"SELECT id, v FROM d ORDER BY id"}' > "$DIR/dump_release.json"
stop

serve "$DEBUG_BIN" rollback
req POST /v1/databases/up/query '{"sql":"SELECT id, v FROM d ORDER BY id"}' > "$DIR/dump_rollback.json"
python3 - "$DIR/dump_release.json" "$DIR/dump_rollback.json" <<'EOF'
import json,sys
a=json.load(open(sys.argv[1]))["rows"]; b=json.load(open(sys.argv[2]))["rows"]
assert a==b, "debug rows differ after rollback"
assert b[-1]==[a[-1][0], "release-gen"], "release-generation write lost"
print(f"rollback exact: {len(b)} rows match, release-gen write intact")
EOF
req POST /v1/databases/up/query '{"sql":"INSERT INTO d(v) VALUES ('"'"'rollback-gen'"'"')"}' back-1 > /dev/null
req POST /v1/databases/up/query '{"sql":"SELECT v FROM d WHERE v LIKE '"'"'%-gen'"'"' ORDER BY id"}' > "$DIR/generations.json"
python3 - "$DIR/generations.json" <<'EOF'
import json, sys
rows = json.load(open(sys.argv[1]))["rows"]
assert rows and rows[-1][0] == "rollback-gen", rows
print("rollback-generation write intact after request")
EOF
stop
# Prove the rollback generation survives the promised next restart as well.
serve "$DEBUG_BIN" rollback-restart
req POST /v1/databases/up/query '{"sql":"SELECT v FROM d WHERE v LIKE '"'"'%-gen'"'"' ORDER BY id"}' > "$DIR/generations-restart.json"
python3 - "$DIR/generations-restart.json" <<'EOF'
import json, sys
rows = json.load(open(sys.argv[1]))["rows"]
assert rows and rows[-1][0] == "rollback-gen", rows
print("rollback-generation write intact after restart")
EOF
stop
echo "UPGRADE REHEARSAL PASS"
