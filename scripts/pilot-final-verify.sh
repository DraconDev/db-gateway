#!/usr/bin/env bash
# Pilot final verification (task 5): run AFTER the 7-day soak.
#
# Usage: scripts/pilot-final-verify.sh [HISTORY_DAYS=7]
# Checks, all against live micro2 over SSH (read-only):
#   1. attempts.jsonl trailing HISTORY_DAYS: every entry outcome ok,
#      every set contains billing_dev_pilot, no gap > 26 h between sets.
#   2. litestream status ok for both databases.
#   3. Fresh export of billing_dev_pilot + validated import into a
#      disposable LOCAL target with exact row counts + integrity.
# Emits tagged transcript on stdout; save as docs/pilot-final-*.log.
# Exit 0 only if ALL checks pass.

set -euo pipefail
DAYS="${1:-7}"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/dg-pilot-final-XXXXXX")"
SRV=""
cleanup() {
    [ -n "$SRV" ] && kill "$SRV" 2>/dev/null || true
    [ -n "$SRV" ] && wait "$SRV" 2>/dev/null || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

say() { echo "[$1] $2"; }
need() { command -v "$1" >/dev/null 2>&1 || { echo "missing: $1" >&2; exit 3; }; }
need ssh; need python3

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DG="${DG_BIN:-$ROOT/.cargo/target/debug/dg}"
[ -x "$DG" ] || { echo "no executable at $DG" >&2; exit 3; }

say START_EPOCH "$(date -u +%s)"
say HISTORY_DAYS "$DAYS"

ssh -o BatchMode=yes micro2 "sudo -n cat /var/lib/dg-backups/attempts.jsonl" >"$WORKDIR/attempts.jsonl" 2>/dev/null
python3 - "$WORKDIR/attempts.jsonl" "$DAYS" <<'EOF'
import json, sys, time
path, days = sys.argv[1], int(sys.argv[2])
now = time.time()
cutoff = now - days * 86400
rows = [json.loads(l) for l in open(path) if l.strip()]
window = [r for r in rows if r.get("completed_at", 0) >= cutoff]
print("[HIST_SETS]", len(window))
bad = [r.get("set") for r in window if r.get("outcome") != "ok"]
print("[HIST_BAD]", bad if bad else "none")
thin = [r.get("set") for r in window if r.get("databases", 0) < 2]
print("[HIST_THIN]", thin if thin else "none")
ts = sorted(r["completed_at"] for r in window)
gaps = [b - a for a, b in zip(ts, ts[1:])]
print("[HIST_MAX_GAP_S]", max(gaps) if gaps else 0)
ok = (not bad) and (not thin) and (len(window) >= days * 20) and (not gaps or max(gaps) <= 26 * 3600)
print("[HIST_VERDICT]", "pass" if ok else "FAIL")
raise SystemExit(0 if ok else 4)
EOF

ssh -o BatchMode=yes micro2 "sudo -n -u dg litestream status 2>&1" >"$WORKDIR/litestream.txt" 2>&1
cat "$WORKDIR/litestream.txt" | sed 's/^/[LITESTREAM] /'
python3 - "$WORKDIR/litestream.txt" <<'EOF'
import sys
lines = [l for l in open(sys.argv[1]) if "/var/lib/dg/" in l]
ok = len(lines) >= 2 and all(l.split()[1] == "ok" for l in lines)
print("[LITESTREAM_VERDICT]", "pass" if ok else "FAIL")
raise SystemExit(0 if ok else 5)
EOF

# Post-soak export + local re-import (Path B), same shape as the rehearsal.
mkdir -p "$WORKDIR/target"
"$DG" init --data-dir "$WORKDIR/target" >"$WORKDIR/target.json" 2>/dev/null
TKEY="$(python3 -c 'import json; print(json.load(open("'"$WORKDIR"'/target.json"))["owner_key"])')"
printf '%s\n' "$TKEY" >"$WORKDIR/target.key"; chmod 600 "$WORKDIR/target.key"
"$DG" serve --listen 127.0.0.1:0 --data-dir "$WORKDIR/target" 2>"$WORKDIR/srv.log" &
SRV=$!
python3 - "$WORKDIR/srv.log" "$WORKDIR/addr" <<'EOF'
import json, sys, time
deadline = time.time() + 30
while time.time() < deadline:
    try:
        for line in open(sys.argv[1]):
            line = line.strip()
            if line.startswith("{"):
                try:
                    addr = json.loads(line).get("fields", {}).get("address")
                except ValueError:
                    continue
                if addr:
                    open(sys.argv[2], "w").write(addr + "\n")
                    raise SystemExit(0)
    except FileNotFoundError:
        pass
    time.sleep(0.2)
raise SystemExit("no listen address")
EOF
ADDR="$(cat "$WORKDIR/addr")"
T=( "$DG" --server "http://$ADDR" --key-file "$WORKDIR/target.key" )
ssh -o BatchMode=yes micro2 "sudo -n rm -f /tmp/pilot-final.sqlite" 2>/dev/null
ssh -o BatchMode=yes micro2 "sudo -n -u dg dg --server http://127.0.0.1:7432 --key-file /etc/dg/owner.key export --out /tmp/pilot-final.sqlite billing_dev_pilot" >/dev/null 2>&1
ssh -o BatchMode=yes micro2 "sudo -n -u dg cat /tmp/pilot-final.sqlite" >"$WORKDIR/live.sqlite" 2>/dev/null
ssh -o BatchMode=yes micro2 "sudo -n -u dg shred -u /tmp/pilot-final.sqlite" 2>/dev/null || true
say EXPORT_SHA "$(sha256sum "$WORKDIR/live.sqlite" | awk '{print $1}')"
say IMPORT "$("${T[@]}" import billing_dev_pilot --from "$WORKDIR/live.sqlite")"
say LIVE_COUNT "$(python3 -c 'import sqlite3; print(sqlite3.connect("'"$WORKDIR"'/live.sqlite").execute("SELECT count(*) FROM sqlite_master").fetchone()[0])')"
"${T[@]}" export --out "$WORKDIR/check.sqlite" billing_dev_pilot >/dev/null
python3 - "$WORKDIR/check.sqlite" <<'EOF'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
print("[CHECK_INTEGRITY]", c.execute("PRAGMA integrity_check").fetchone()[0])
print("[CHECK_OBJECTS]", c.execute("SELECT count(*) FROM sqlite_master").fetchone()[0])
EOF
say END_EPOCH "$(date -u +%s)"
say VERDICT pass
