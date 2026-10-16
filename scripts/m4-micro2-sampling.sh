#!/usr/bin/env bash
# M4 micro2 sampling: read-only host evidence plus a disposable restore.
#
# Usage: scripts/m4-micro2-sampling.sh
# Emits a timestamped transcript on stdout; diagnostics go to stderr.
# Save stdout as docs/m4-micro2-sampling-<date>.log.
#
# Live contact is read-only except for one `litestream restore` into a
# disposable /var/tmp staging dir (removed at the end) and the restore's
# own temp files. No database is written, no unit is changed, no key is
# created or revoked, no firewall or disk state is touched. Credentials
# resolve from the operator's ~/.ssh/config; argv carries no secrets.
# The transcript contains no key material: hashes and counts only.

set -euo pipefail

log() { echo "[m4-sample] $*" >&2; }
need() { command -v "$1" >/dev/null 2>&1 || { echo "[m4-sample] missing: $1" >&2; exit 3; }; }
need ssh

TARGET="${DG_AUDIT_TARGET:-micro2}"
PROBE="$(mktemp "${TMPDIR:-/tmp}/dg-m4-sample-probe-XXXXXX.sh")"
trap 'rm -f "$PROBE"' EXIT

# Quoted heredoc: the body travels literally, values are baked in by the
# writer only where marked. No secrets are baked in.
cat >"$PROBE" <<'PROBE_EOF'
#!/usr/bin/env bash
set -u
say() { echo "[$1] $2"; }
say SAMPLE_EPOCH "$(date -u +%s)"
say UNAME "$(uname -a)"
say NPROC "$(nproc)"
say MEMINFO "$(awk '/^(MemTotal|MemAvailable):/ {printf "%s%sKiB ", $1, $2}' /proc/meminfo)"
say DF_ROOT "$(df -h / | tail -n 1)"
say LOADAVG "$(cat /proc/loadavg)"
for u in dg.service dg-litestream.service; do
    say UNIT "$u ActiveState=$(systemctl show "$u" -p ActiveState --value) MainPID=$(systemctl show "$u" -p MainPID --value) MemoryMax=$(systemctl show "$u" -p MemoryMax --value)"
done
say TIMER "dg-backup.timer ActiveState=$(systemctl show dg-backup.timer -p ActiveState --value)"
GW="$(systemctl show dg.service -p MainPID --value)"
SC="$(systemctl show dg-litestream.service -p MainPID --value)"
say GW_STATUS "$(grep -E '^(VmRSS|VmHWM|VmSize|Threads):' /proc/$GW/status | tr '\n' ' ')"
say GW_CPU_TICKS "$(awk '{print $14+$15}' /proc/$GW/stat)"
say SC_STATUS "$(grep -E '^(VmRSS|VmHWM|VmSize|Threads):' /proc/$SC/status | tr '\n' ' ')"
say SC_CPU_TICKS "$(awk '{print $14+$15}' /proc/$SC/stat)"
say LITESTREAM_STATUS "$(sudo -n -u dg litestream status 2>&1 | tr '\n' ';')"
say BACKUP_VERIFY "$(sudo -n -u dg dg backup verify --backup-root /var/lib/dg-backups latest 2>&1)"
say GW_STATUS_API "$(sudo -n -u dg dg --server http://127.0.0.1:7432 --key-file /etc/dg/owner.key status 2>&1)"
say LIVE_COUNT "$(sudo -n -u dg dg --server http://127.0.0.1:7432 --key-file /etc/dg/owner.key query verify_probe 'SELECT count(*) FROM t' 2>&1)"
say LIVE_ROWS "$(sudo -n -u dg dg --server http://127.0.0.1:7432 --key-file /etc/dg/owner.key query verify_probe 'SELECT * FROM t ORDER BY rowid' 2>&1)"
say LIVE_KEYS "$(sudo -n -u dg dg --server http://127.0.0.1:7432 --key-file /etc/dg/owner.key key list 2>&1)"
# Disposable restore: env sourced (else S3 auth fails), staging under
# /var/tmp, removed afterwards. Timed with date +%s%N.
RDIR="/var/tmp/m4-sample-restore-$(date -u +%Y%m%dT%H%M%SZ)"
sudo -n mkdir -p "$RDIR" && sudo -n chown dg:dg "$RDIR"
say RESTORE_DIR "$RDIR"
sudo -n -u dg bash -c 'set -a; source /etc/dg/litestream.env; set +a; S=$(date +%s%N); /usr/local/bin/litestream restore -config /etc/litestream.yml -o '"$RDIR"'/verify_probe.sqlite /var/lib/dg/verify_probe.sqlite 2>&1; echo "RESTORE_EXIT=$?"; E=$(date +%s%N); echo "RESTORE_ELAPSED_MS=$(( (E-S)/1000000 ))"'
sudo -n -u dg python3 - "$RDIR/verify_probe.sqlite" <<'PYEOF'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
print("[RESTORED_INTEGRITY]", c.execute("PRAGMA integrity_check").fetchone()[0])
print("[RESTORED_FK]", c.execute("SELECT count(*) FROM pragma_foreign_key_check").fetchone()[0])
print("[RESTORED_COUNT]", c.execute("SELECT count(*) FROM t").fetchone()[0])
print("[RESTORED_ROWS]", c.execute("SELECT * FROM t ORDER BY rowid").fetchall())
PYEOF
say RESTORED_BYTES "$(sudo -n stat -c '%s' "$RDIR/verify_probe.sqlite")"
sudo -n rm -rf "$RDIR"
say RESTORE_CLEANED yes
PROBE_EOF

log "sampling $TARGET (3 passes, ~40 s apart; restore runs once, first pass)"
for i in 1 2 3; do
    echo "--- PASS $i ---"
    if [ "$i" -eq 1 ]; then
        ssh -o BatchMode=yes "$TARGET" "sudo -n bash -s" <"$PROBE"
    else
        ssh -o BatchMode=yes "$TARGET" \
            'GW=$(systemctl show dg.service -p MainPID --value); SC=$(systemctl show dg-litestream.service -p MainPID --value); echo "[SAMPLE_EPOCH] $(date -u +%s)"; echo "[GW_RSS] $(awk "/^VmRSS:/{print \$2}" /proc/$GW/status)"; echo "[GW_CPU_TICKS] $(awk "{print \$14+\$15}" /proc/$GW/stat)"; echo "[SC_RSS] $(awk "/^VmRSS:/{print \$2}" /proc/$SC/status)"; echo "[SC_CPU_TICKS] $(awk "{print \$14+\$15}" /proc/$SC/stat)"; echo "[MEMAVAIL] $(awk "/^MemAvailable:/{print \$2}" /proc/meminfo)"'
    fi
    [ "$i" -lt 3 ] && sleep 40
done
echo "[SCRIPT_DONE] $0"
