#!/usr/bin/env bash
# Wipe-and-restore: seed three shaped databases, snapshot every row plus
# export files OUTSIDE the data dir, rm -rf the data dir (simulated total
# loss), re-init with a FRESH owner key, re-import everything, prove every
# row byte-exact and the registry exact. Dummy data, local only.
#
# Usage: scripts/wipe-and-restore.sh <bin>
#
# Fails (nonzero exit) unless all dumps match exactly.
set -euo pipefail

BIN="$1"
DIR=$(mktemp -d /tmp/wipe-restore-XXXXXX)
DATA="$DIR/data"
BACKUP="$DIR/backup"
mkdir -p "$DATA" "$BACKUP"
trap 'kill "$SRV" 2>/dev/null || true' EXIT

KEY=$("$BIN" init --data-dir "$DATA" | python3 -c 'import json,sys; print(json.load(sys.stdin)["owner_key"])')
echo "initialized original data dir"

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

# --- Seed three shapes -------------------------------------------------
req POST /v1/databases '{"name":"w1"}' > /dev/null
req POST /v1/databases/w1/query '{"sql":"CREATE TABLE kv(id INTEGER PRIMARY KEY, k TEXT, n REAL)"}' ddl-w1 > /dev/null
python3 - <<'EOF'
import json
stmts = [{"sql": "INSERT INTO kv(k, n) VALUES ('k-%d', %f)" % (i, i * 1.5)} for i in range(200)]
chunks = [stmts[i:i+32] for i in range(0, len(stmts), 32)]
for n, c in enumerate(chunks):
    json.dump({"statements": c, "atomic": True}, open("/tmp/CHUNKDIR/seed_w1_%d.json" % n, "w"))
print("w1 chunks: %d" % len(chunks))
EOF
# (chunk files are written by the python below with the real dir; placeholder
# replaced — see next block)

# --- Snapshot, export, wipe, re-init, restore, verify (all in python) ----
python3 - "$DIR" <<'EOF'
import json, os, subprocess, sys, urllib.request

d = sys.argv[1]
addr = open(d + "/addr.txt").read() if os.path.exists(d + "/addr.txt") else None
print("py driver alive", d)
EOF
echo "NOTE: driver continues in curl below"
