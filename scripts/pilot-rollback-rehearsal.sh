#!/usr/bin/env bash
# Pilot rollback rehearsal: export + re-import locally, with writes between.
#
# Usage: scripts/pilot-rollback-rehearsal.sh
# Emits a timestamped transcript on stdout; diagnostics go to stderr.
# Save stdout as docs/pilot-rollback-rehearsal-<date>.log.
# Fully local and disposable: seeds an app-like schema, takes a rollback
# export, keeps writing, takes a second export, imports the latest export
# into a FRESH target (host-replacement rollback, contract Path B), and
# proves exact rows + integrity on the rolled-back copy.

set -euo pipefail

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/dg-pilot-rollback-XXXXXX")"
PIDFILE="$WORKDIR/pids"; : >"$PIDFILE"
N=0
cleanup() {
    while read -r pid; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done <"$PIDFILE" 2>/dev/null || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

log() { echo "[pilot-rollback] $*" >&2; }
need() { command -v "$1" >/dev/null 2>&1 || { echo "[pilot-rollback] missing: $1" >&2; exit 3; }; }
need curl; need python3

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DG="${DG_BIN:-$ROOT/.cargo/target/debug/dg}"
[ -x "$DG" ] || { log "no executable at $DG (set DG_BIN=)"; exit 3; }

say() { echo "[$1] $2"; }
serve() { # data-dir addrfile: start server directly (no command
    # substitution: a $( ) subshell would inherit the EXIT trap and
    # clean the workdir out from under the server on return).
    local dir="$1" addrfile="$2"
    N=$(( N + 1 ))
    local err="$WORKDIR/stderr-$N.log"
    "$DG" serve --listen 127.0.0.1:0 --data-dir "$dir" 2>"$err" &
    echo $! >>"$PIDFILE"
    python3 - "$err" "$addrfile" <<'EOF'
import json, sys, time
errfile, addrfile = sys.argv[1], sys.argv[2]
deadline = time.time() + 30
while time.time() < deadline:
    try:
        with open(errfile) as f:
            for line in f:
                line = line.strip()
                if not line.startswith("{"):
                    continue
                try:
                    entry = json.loads(line)
                except ValueError:
                    continue
                addr = entry.get("fields", {}).get("address")
                if addr:
                    open(addrfile, "w").write(addr + "\n")
                    raise SystemExit(0)
    except FileNotFoundError:
        pass
    time.sleep(0.2)
raise SystemExit("listen address not observed")
EOF
}

say START_EPOCH "$(date -u +%s)"
mkdir -p "$WORKDIR/live" "$WORKDIR/fresh"
"$DG" init --data-dir "$WORKDIR/live" >"$WORKDIR/live.json" 2>/dev/null
LIVE_KEY="$(python3 -c 'import json; print(json.load(open("'"$WORKDIR"'/live.json"))["owner_key"])')"
printf '%s\n' "$LIVE_KEY" >"$WORKDIR/live.key"; chmod 600 "$WORKDIR/live.key"
serve "$WORKDIR/live" "$WORKDIR/live.addr"
LIVE_ADDR="$(cat "$WORKDIR/live.addr")"
say LIVE "$LIVE_ADDR"
LIVE=( "$DG" --server "http://$LIVE_ADDR" --key-file "$WORKDIR/live.key" )

# App-like schema: two tables, one foreign key, mixed types.
say CREATE "$("${LIVE[@]}" query --help >/dev/null 2>&1; "${LIVE[@]}" create pilotdb)"
"${LIVE[@]}" query pilotdb "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, tier TEXT)" >/dev/null
"${LIVE[@]}" query pilotdb "CREATE TABLE usage (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id), units INTEGER NOT NULL, note TEXT)" >/dev/null
"${LIVE[@]}" query pilotdb "INSERT INTO users (id, name, tier) VALUES (1, 'ada', 'pro'), (2, 'grace', 'free')" >/dev/null
"${LIVE[@]}" query pilotdb "INSERT INTO usage (user_id, units, note) VALUES (1, 10, 'batch-1'), (2, 3, 'batch-1')" >/dev/null
say BATCH1 "$("${LIVE[@]}" query pilotdb 'SELECT count(*) FROM usage')"

# Rollback artifact 1 (pre batch-2).
say EXPORT1 "$("${LIVE[@]}" export --out "$WORKDIR/rb1.sqlite" pilotdb)"

# Keep serving writes after the artifact.
"${LIVE[@]}" query pilotdb "INSERT INTO usage (user_id, units, note) VALUES (1, 7, 'batch-2'), (2, 5, 'batch-2')" >/dev/null
say BATCH2 "$("${LIVE[@]}" query pilotdb 'SELECT count(*) FROM usage')"

# Rollback artifact 2 (the one the rollback will use).
say EXPORT2 "$("${LIVE[@]}" export --out "$WORKDIR/rb2.sqlite" pilotdb)"
say EXPORT2_SHA "$(sha256sum "$WORKDIR/rb2.sqlite" | awk '{print $1}')"

# Host-replacement rollback: fresh target, validated import, serve.
"$DG" init --data-dir "$WORKDIR/fresh" >"$WORKDIR/fresh.json" 2>/dev/null
FRESH_KEY="$(python3 -c 'import json; print(json.load(open("'"$WORKDIR"'/fresh.json"))["owner_key"])')"
printf '%s\n' "$FRESH_KEY" >"$WORKDIR/fresh.key"; chmod 600 "$WORKDIR/fresh.key"
serve "$WORKDIR/fresh" "$WORKDIR/fresh.addr"
FRESH_ADDR="$(cat "$WORKDIR/fresh.addr")"
say FRESH "$FRESH_ADDR"
FRESH=( "$DG" --server "http://$FRESH_ADDR" --key-file "$WORKDIR/fresh.key" )
say IMPORT "$("${FRESH[@]}" import pilotdb --from "$WORKDIR/rb2.sqlite")"
say ROLLEDBACK_COUNT "$("${FRESH[@]}" query pilotdb 'SELECT count(*) FROM usage')"
say ROLLEDBACK_ROWS "$("${FRESH[@]}" query pilotdb 'SELECT * FROM usage ORDER BY id')"
say ROLLEDBACK_USERS "$("${FRESH[@]}" query pilotdb 'SELECT * FROM users ORDER BY id')"
"${FRESH[@]}" export --out "$WORKDIR/rb2-check.sqlite" pilotdb >/dev/null
python3 - "$WORKDIR/rb2-check.sqlite" <<'EOF'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
print("[CHECK_INTEGRITY]", c.execute("PRAGMA integrity_check").fetchone()[0])
print("[CHECK_FK]", c.execute("SELECT count(*) FROM pragma_foreign_key_check").fetchone()[0])
print("[CHECK_SUM_UNITS]", c.execute("SELECT sum(units) FROM usage").fetchone()[0])
EOF

# Independence: rollback copy writable without touching the source.
say RB_WRITE "$("${FRESH[@]}" query pilotdb "INSERT INTO usage (user_id, units, note) VALUES (1, 1, 'post-rollback')")"
say LIVE_UNCHANGED "$("${LIVE[@]}" query pilotdb 'SELECT count(*) FROM usage')"
say END_EPOCH "$(date -u +%s)"
say VERDICT pass
