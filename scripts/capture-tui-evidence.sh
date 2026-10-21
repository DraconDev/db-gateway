#!/usr/bin/env bash
# Capture a real `dg tui` session against a real gateway, for the
# evidence pack. Writes the raw pty capture and a plain-text rendering
# of it.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DG="${DG:-$ROOT/.cargo/target/debug/dg}"
OUT_RAW="${1:-$ROOT/docs/evidence/tui-view-2026-10-21.log}"
OUT_TEXT="${2:-${OUT_RAW%.log}.txt}"

DIR="$(mktemp -d "${TMPDIR:-/tmp}/dg-tui-capture-XXXXXX")"
trap 'rm -rf "$DIR"' EXIT

"$DG" init --data-dir "$DIR/data" > "$DIR/init.json"
python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["owner_key"])' "$DIR/init.json" > "$DIR/owner.key"
chmod 600 "$DIR/owner.key"

"$DG" serve --listen 127.0.0.1:0 --data-dir "$DIR/data" > "$DIR/serve.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 50); do
    SERVER="$(python3 - "$DIR/serve.log" <<'PY' 2>/dev/null || true
import json, sys
for line in open(sys.argv[1], errors="replace"):
    try:
        log = json.loads(line)
    except ValueError:
        continue
    address = log.get("fields", {}).get("address")
    if address:
        print("http://" + address)
        break
PY
)"
    [ -n "$SERVER" ] && break
    sleep 0.1
done
[ -n "$SERVER" ] || { echo "the gateway never logged its address" >&2; exit 1; }

KEY=(--server "$SERVER" --key-file "$DIR/owner.key")
"$DG" "${KEY[@]}" create demo > /dev/null
"$DG" "${KEY[@]}" query demo "CREATE TABLE players (id INTEGER PRIMARY KEY, name TEXT)" > /dev/null
"$DG" "${KEY[@]}" query demo "INSERT INTO players VALUES (1, 'aria')" > /dev/null
"$DG" "${KEY[@]}" query demo "INSERT INTO players VALUES (2, 'bo')" > /dev/null
"$DG" "${KEY[@]}" query demo "SELECT * FROM players" > /dev/null

# The session: open the actions, run a query, then leave.
printf ':query\rdemo select * from players\rq' | script -qec \
    "stty rows 40 cols 120 && $DG ${KEY[*]} tui" /dev/null > "$OUT_RAW" 2>&1 || true

kill "$SERVER_PID" 2>/dev/null || true
wait "$SERVER_PID" 2>/dev/null || true

python3 - "$OUT_RAW" "$OUT_TEXT" <<'PY'
import re, sys

raw = open(sys.argv[1], "rb").read().decode("utf-8", "replace")
# Strip the escape sequences, and mark the frames the view drew.
text = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "\n", raw)
text = re.sub(r"\x1b[()][A-Z0-9]", "", text)
text = re.sub(r"\x1b[=>]", "", text)
lines = [line.rstrip() for line in text.splitlines()]
frames = []
current = []
for line in lines:
    if line == "" and current:
        if any(item.strip() for item in current):
            frames.append(current)
        current = []
    else:
        current.append(line)
if any(item.strip() for item in current):
    frames.append(current)
with open(sys.argv[2], "w") as out:
    for index, frame in enumerate(frames):
        out.write(f"--- frame {index + 1} ---\n")
        out.write("\n".join(line for line in frame if line.strip()))
        out.write("\n")
print(f"captured {len(frames)} frames into {sys.argv[2]}")
PY
