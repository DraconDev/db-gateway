#!/usr/bin/env bash
# Wipe-and-restore: seed three shaped databases, snapshot every row plus
# export files, rm -rf the data dir (simulated total loss), re-init with a
# FRESH owner key, re-import everything, prove every row byte-exact.
# Dummy data, local only.
#
# Usage: scripts/wipe-and-restore.sh <bin>
#
# Fails (nonzero exit) unless all dumps match exactly and the registry
# lists precisely the three restored databases.
set -euo pipefail

BIN="$1"
DIR=$(mktemp -d /tmp/wipe-restore-XXXXXX)
DATA="$DIR/data"
BACKUP="$DIR/backup"
mkdir -p "$DATA" "$BACKUP"
trap 'kill "$SRV" 2>/dev/null || true' EXIT

KEY=$("$BIN" init --data-dir "$DATA" | python3 -c 'import json,sys; print(json.load(sys.stdin)["owner_key"])')
echo "initialized; owner key acquired"

serve() {
    "$BIN" serve --listen 127.0.0.1:0 --data-dir "$DATA" >"$DIR/srv.log" 2>&1 &
    SRV=$!
    for _ in $(seq 1 100); do
        ADDR=$(grep -o '"address":"[^"]*"' "$DIR/srv.log" | head -1 | cut -d'"' -f4 || true)
        [ -n "${ADDR:-}" ] && break
        sleep 0.1
    done
    echo "serving $1 on $ADDR (pid $SRV)"
}

req() { # method path body [idem-key]
    local args=(-s -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json')
    [ "${4:-}" != "" ] && args+=(-H "Idempotency-Key: $4")
    curl "${args[@]}" -X "$1" -d "$3" "http://$ADDR$2"
}

stop() { kill "$SRV"; wait "$SRV" 2>/dev/null || true; : >"$DIR/srv.log"; }

serve original

# Three shapes: typed numerics, FK parent/child, NULLs/unicode/empty strings.
req POST /v1/databases '{"name":"w1"}' > /dev/null
req POST /v1/databases/w1/query '{"sql":"CREATE TABLE kv(id INTEGER PRIMARY KEY, k TEXT, n REAL)"}' ddl-w1 > /dev/null
python3 - <<'EOF' > "$DIR/seed_w1.json"
import json
stmts = [{"sql": "INSERT INTO kv(k, n) VALUES ('k-%d', %f)" % (i, i * 1.5)} for i in range(200)]
print(json.dumps({"statements": stmts, "atomic": False}))
EOF
# 200 singles would exceed nothing, but batch in chunks of 32 (7 requests).
python3 - "$DIR/seed_w1.json" <<'EOF'
import json
all_stmts = json.load(open("$DIR/seed_w1.json"))["statements"]
for n, i in enumerate(range(0, len(all_stmts), 32)):
    json.dump({"statements": all_stmts[i:i+32], "atomic": True}, open("$DIR/seed_w1_%d.json" % n, "w"))
print((len(all_stmts) + 31) // 32)
EOF
echo "seeded w1 in chunks"

req POST /v1/databases '{"name":"w2"}' > /dev/null
req POST /v1/databases/w2/query '{"sql":"CREATE TABLE parent(id INTEGER PRIMARY KEY, name TEXT)"}' ddl-w2a > /dev/null
req POST /v1/databases/w2/query '{"sql":"CREATE TABLE child(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id), tag TEXT)"}' ddl-w2b > /dev/null

req POST /v1/databases '{"name":"w3"}' > /dev/null
req POST /v1/databases/w3/query '{"sql":"CREATE TABLE misc(id INTEGER PRIMARY KEY, v TEXT)"}' ddl-w3 > /dev/null
EOF_MARKER_NOT_USED=true
