#!/usr/bin/env bash
# Capture a real `dg tui` session against a real gateway, for the
# evidence pack. Writes the raw pty capture and a readable rendering of
# the frames it drew.
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

# The approved snapshot cadence is the number the freshness state is
# judged on, so the capture sets one: without it every unrepresented
# change is reported and the screen would show the strict reading rather
# than the state this release added.
CADENCE_SECS="${DG_CAPTURE_CADENCE_SECS:-3600}"
"$DG" serve --listen 127.0.0.1:0 --data-dir "$DIR/data" \
    --backup-cadence-secs "$CADENCE_SECS" > "$DIR/serve.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 50); do
    SERVER="$(python3 "$ROOT/scripts/read-gateway-address.py" "$DIR/serve.log" 2>/dev/null || true)"
    [ -n "$SERVER" ] && break
    sleep 0.1
done
[ -n "${SERVER:-}" ] || { echo "the gateway never logged its address" >&2; exit 1; }

KEY=(--server "$SERVER" --key-file "$DIR/owner.key")
"$DG" "${KEY[@]}" create demo > /dev/null
"$DG" "${KEY[@]}" query demo "CREATE TABLE players (id INTEGER PRIMARY KEY, name TEXT)" > /dev/null
"$DG" "${KEY[@]}" query demo "INSERT INTO players VALUES (1, 'aria')" > /dev/null
"$DG" "${KEY[@]}" query demo "INSERT INTO players VALUES (2, 'bo')" > /dev/null
"$DG" "${KEY[@]}" query demo "SELECT * FROM players" > /dev/null

# A published set first, then writes after it: those writes are what the
# cadence judges, so the capture shows real per-database states rather
# than a gateway that has never backed anything up.
"$DG" "${KEY[@]}" backup run > "$DIR/backup-run.log" 2>&1 || true
"$DG" "${KEY[@]}" query demo "INSERT INTO players VALUES (3, 'cy')" > /dev/null

# A second database, created after the published set: the newest set
# carries demo but not other, so the coverage block names the missing
# database the same way the two-database fixture does -- and the
# freshness table has two rows to keep, which is the case this release
# fixed. The write after the set keeps other inside the cadence, so
# both rows show real states with a lag beside them.
"$DG" "${KEY[@]}" create other > /dev/null
"$DG" "${KEY[@]}" query other "CREATE TABLE players (id INTEGER PRIMARY KEY, name TEXT)" > /dev/null
"$DG" "${KEY[@]}" query other "INSERT INTO players VALUES (1, 'zed')" > /dev/null

# The session: open the actions, run a query, then leave.
#
# The keys are typed with pauses on purpose: the view takes the
# terminal into raw mode when it starts, and a raw-mode switch flushes
# whatever was typed before it. Typing into a view that is not yet
# listening is typing into the void.
(
    sleep 1.2
    printf ':'
    sleep 0.4
    printf 'query'
    sleep 0.4
    printf '\r'
    sleep 0.6
    printf 'demo select * from players'
    sleep 0.4
    printf '\r'
    sleep 1.5
    # The backups tab: the freshness table, the lag beside each state, and
    # the approved snapshot cadence the digest is judged on.
    printf '3'
    sleep 1.2
    printf 'q'
) | script -qec \
    "stty rows 40 cols 120 && $DG ${KEY[*]} tui" /dev/null > "$OUT_RAW" 2>&1 || true

kill "$SERVER_PID" 2>/dev/null || true
wait "$SERVER_PID" 2>/dev/null || true

python3 "$ROOT/scripts/render-tui-capture.py" "$OUT_RAW" "$OUT_TEXT"
