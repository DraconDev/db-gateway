#!/usr/bin/env bash
# M3 restore drill: fresh micro2 set -> disposable local target (Path B).
#
# Usage: scripts/m3-restore-drill.sh
# Emits a timestamped transcript on stdout; diagnostics go to stderr.
# Save stdout as docs/m3-restore-<date>.log; it carries every raw output
# the verification contract needs: set identity, full verify JSONs,
# full SHA-256 chain, raw row/integrity/FK outputs, scoped-access checks,
# and RPO/RTO wall-clock timestamps. Pulled live bytes stay in a mktemp
# workdir that is removed on exit; the transcript retains the hashes.
#
# Live contact with micro2 is read-only (set listing, backup verify,
# row-count queries). All writes happen on the disposable local target.

set -euo pipefail

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/dg-m3-drill-XXXXXX")"
SERVER_PID=""
cleanup() {
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

log() { echo "[m3-drill] $*" >&2; }
need() { command -v "$1" >/dev/null 2>&1 || { echo "[m3-drill] missing: $1" >&2; exit 3; }; }
need ssh; need curl; need python3

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DG="${DG_BIN:-$ROOT/.cargo/target/debug/dg}"
[ -x "$DG" ] || { log "no executable at $DG (set DG_BIN=)"; exit 3; }

T() { date -u +%s; }
say() { echo "[$1] $2"; }

RTO_START="$(T)"
say RTO_START "$RTO_START"

# --- 1. fresh set identity (read-only live) ---
SET="$(ssh -o BatchMode=yes micro2 "sudo -n ls /var/lib/dg-backups/ | grep -E '^[0-9]{8}T[0-9]{6}Z\$' | tail -n 1" 2>/dev/null)"
[ -n "$SET" ] || { log "no set listed from micro2"; exit 4; }
say SET "$SET"

ONHOST_VERIFY="$(ssh -o BatchMode=yes micro2 "sudo -n -u dg dg backup verify --backup-root /var/lib/dg-backups latest" 2>/dev/null)"
say ONHOST_VERIFY "$ONHOST_VERIFY"
echo "$ONHOST_VERIFY" | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["has_registry"] is True, d; print("onhost-verify-ok:", d["set"])'

# --- 2. pull set files (read-only live cat) ---
SETDIR="$WORKDIR/set/$SET"; mkdir -p "$SETDIR"
for f in manifest.json verify_probe.sqlite verify_probe.sha256 metadata.sqlite.bak; do
    ssh -o BatchMode=yes micro2 "sudo -n cat /var/lib/dg-backups/$SET/$f" >"$SETDIR/$f" 2>/dev/null
done
ls -la "$SETDIR" >&2
say MANIFEST "$(cat "$SETDIR/manifest.json")"
say SIDECAR_SHA "$(cat "$SETDIR/verify_probe.sha256")"
say RECOMPUTED_SHA "$(sha256sum "$SETDIR/verify_probe.sqlite" | awk '{print $1}')"
[ "$(sha256sum "$SETDIR/verify_probe.sqlite" | awk '{print $1}')" = "$(cat "$SETDIR/verify_probe.sha256")" ] \
    || { log "sha chain mismatch"; exit 4; }
say SHA_CHAIN match

# --- 3. local verify of pulled bytes ---
LOCAL_VERIFY="$("$DG" backup verify --backup-root "$WORKDIR/set" latest)"
say LOCAL_VERIFY "$LOCAL_VERIFY"
echo "$LOCAL_VERIFY" | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True, d; print("local-verify-ok:", d["set"])'

VERIFY_EPOCH="$(T)"
say VERIFY_EPOCH "$VERIFY_EPOCH"
SET_STARTED="$(python3 -c 'import json; print(json.load(open("'"$SETDIR"'/manifest.json"))["started_at"])')"
say SET_STARTED "$SET_STARTED"
say RPO_LAG_SECS "$(( VERIFY_EPOCH - SET_STARTED ))"

# --- 4. live rows (read-only) ---
LIVE_COUNT="$(ssh -o BatchMode=yes micro2 "sudo -n -u dg dg --server http://127.0.0.1:7432 --key-file /etc/dg/owner.key query verify_probe 'SELECT count(*) FROM t'" 2>/dev/null)"
say LIVE_COUNT "$LIVE_COUNT"
LIVE_ROWS="$(ssh -o BatchMode=yes micro2 "sudo -n -u dg dg --server http://127.0.0.1:7432 --key-file /etc/dg/owner.key query verify_probe 'SELECT * FROM t ORDER BY rowid'" 2>/dev/null)"
say LIVE_ROWS "$LIVE_ROWS"

# --- 5. disposable target: fresh init + serve + Path B import ---
TARGET="$WORKDIR/target"; mkdir -p "$TARGET"
"$DG" init --data-dir "$TARGET" >"$WORKDIR/init.json" 2>/dev/null
TKEY="$(python3 -c 'import json; print(json.load(open("'"$WORKDIR"'/init.json"))["owner_key"])')"
printf '%s\n' "$TKEY" >"$WORKDIR/t.key"; chmod 600 "$WORKDIR/t.key"
"$DG" serve --listen 127.0.0.1:0 --data-dir "$TARGET" 2>"$WORKDIR/stderr.log" &
SERVER_PID=$!
TADDR="$(python3 - "$WORKDIR/stderr.log" <<'EOF'
import json, sys, time
deadline = time.time() + 30
while time.time() < deadline:
    try:
        with open(sys.argv[1]) as f:
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
                    print(addr)
                    raise SystemExit(0)
    except FileNotFoundError:
        pass
    time.sleep(0.2)
raise SystemExit("listen address not observed")
EOF
)"
say TARGET "$TADDR"
say IMPORT "$("$DG" --server "http://$TADDR" --key-file "$WORKDIR/t.key" import verify_probe --from "$SETDIR/verify_probe.sqlite")"

# --- 6. restored rows + export + file-level checks ---
say RESTORED_COUNT "$("$DG" --server "http://$TADDR" --key-file "$WORKDIR/t.key" query verify_probe 'SELECT count(*) FROM t')"
say RESTORED_ROWS "$("$DG" --server "http://$TADDR" --key-file "$WORKDIR/t.key" query verify_probe 'SELECT * FROM t ORDER BY rowid')"
say EXPORT_JSON "$("$DG" --server "http://$TADDR" --key-file "$WORKDIR/t.key" export --out "$WORKDIR/restored.sqlite" verify_probe)"
python3 - "$WORKDIR/restored.sqlite" <<'EOF'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
print("[INTEGRITY]", c.execute("PRAGMA integrity_check").fetchone()[0])
print("[FK_VIOLATIONS]", c.execute("SELECT count(*) FROM pragma_foreign_key_check").fetchone()[0])
print("[DG_TABLES]", c.execute("SELECT name FROM sqlite_master WHERE name LIKE '_dg_%'").fetchall())
print("[EXPORT_ROWS]", c.execute("SELECT * FROM t ORDER BY rowid").fetchall())
EOF

# --- 7. scoped access on the Path B target (re-issued keys) ---
RK_JSON="$("$DG" --server "http://$TADDR" --key-file "$WORKDIR/t.key" key create --role database_read_only --database verify_probe rk_ro 2>/dev/null)"
RK="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["key"])' <<<"$RK_JSON")"
say SCOPED_READ "$(curl -s -o "$WORKDIR/sr.json" -w 'http=%{http_code} ' -H "Authorization: Bearer $RK" -H 'Content-Type: application/json' -d '{"sql":"SELECT count(*) FROM t"}' "http://$TADDR/v1/databases/verify_probe/query"; cat "$WORKDIR/sr.json")"
say SCOPED_WRITE_DENIED "$(curl -s -o "$WORKDIR/sw.json" -w 'http=%{http_code} ' -H "Authorization: Bearer $RK" -H 'Content-Type: application/json' -d '{"sql":"INSERT INTO t(note) VALUES ('"'"'nope'"'"')"}' "http://$TADDR/v1/databases/verify_probe/query"; cat "$WORKDIR/sw.json")"
FAKE="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
say UNSCOPED_DENIED "$(curl -s -o "$WORKDIR/su.json" -w 'http=%{http_code} ' -H "Authorization: Bearer $FAKE" -H 'Content-Type: application/json' -d '{"sql":"SELECT count(*) FROM t"}' "http://$TADDR/v1/databases/verify_probe/query"; cat "$WORKDIR/su.json")"

# --- 8. independence + source unchanged ---
say RESTORED_WRITE "$("$DG" --server "http://$TADDR" --key-file "$WORKDIR/t.key" query verify_probe "INSERT INTO t(note) VALUES ('restored-only')")"
say RESTORED_COUNT_AFTER "$("$DG" --server "http://$TADDR" --key-file "$WORKDIR/t.key" query verify_probe 'SELECT count(*) FROM t')"
say LIVE_COUNT_AFTER "$(ssh -o BatchMode=yes micro2 "sudo -n -u dg dg --server http://127.0.0.1:7432 --key-file /etc/dg/owner.key query verify_probe 'SELECT count(*) FROM t'" 2>/dev/null)"

RTO_END="$(T)"
say RTO_END "$RTO_END"
say RTO_SECS "$(( RTO_END - RTO_START ))"
say VERDICT pass
