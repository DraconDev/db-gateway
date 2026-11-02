#!/usr/bin/env bash
# Off-host second leg for the micro2 snapshot sets.
#
# The hourly producer (scripts/micro2-backup.sh) publishes verified sets
# under /var/lib/dg-backups ON micro2: same-disk storage protects
# against logical loss, not disk loss. This script pulls the newest set
# to the operator machine over the existing SSH key, verifies every
# checksum sidecar the producer published, and prunes old local pulls.
# Litestream's WAL stream remains the continuous off-host replica; this
# is the file-level copy a rebuild starts from, and the one that does
# not depend on the bucket.
#
# Usage: pull-micro2-backup.sh [--host HOST] [--user USER] [--key PATH]
#                              [--remote-root DIR] [--local-root DIR] [--keep N]
# Defaults: micro2 over ~/.ssh/micro2.key into
# ${XDG_DATA_HOME:-~/.local/share}/dg-backup-sets, newest 7 kept.
# Prints one summary line; never key material. Exit nonzero on any
# failure, and a failed run publishes nothing.
#
# Concurrency: one run at a time (flock on the local root); a contended
# run exits 1 without touching anything. Every run appends one JSON
# line to pulls.jsonl under the local root.
set -euo pipefail

HOST="145.241.238.20"
USER="ubuntu"
KEY="$HOME/.ssh/micro2.key"
REMOTE_ROOT="/var/lib/dg-backups"
LOCAL_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}/dg-backup-sets"
KEEP=7
while [ $# -gt 0 ]; do
  case "$1" in
    --host)
      [ $# -ge 2 ] || { echo "--host requires a value" >&2; exit 2; }
      HOST="$2"; shift 2;;
    --user)
      [ $# -ge 2 ] || { echo "--user requires a value" >&2; exit 2; }
      USER="$2"; shift 2;;
    --key)
      [ $# -ge 2 ] || { echo "--key requires a value" >&2; exit 2; }
      KEY="$2"; shift 2;;
    --remote-root)
      [ $# -ge 2 ] || { echo "--remote-root requires a value" >&2; exit 2; }
      REMOTE_ROOT="$2"; shift 2;;
    --local-root)
      [ $# -ge 2 ] || { echo "--local-root requires a value" >&2; exit 2; }
      LOCAL_ROOT="$2"; shift 2;;
    --keep)
      [ $# -ge 2 ] || { echo "--keep requires a value" >&2; exit 2; }
      KEEP="$2"; shift 2;;
    *) echo "unknown flag: $1" >&2; exit 2;;
  esac
done

[[ "$KEEP" =~ ^[1-9][0-9]*$ ]] || { echo "--keep must be a positive integer" >&2; exit 2; }
[ -r "$KEY" ] || { echo "unreadable key file: $KEY" >&2; exit 2; }
mkdir -p "$LOCAL_ROOT"

fail() { echo "[pull-backup] FAIL: $*" >&2; exit 1; }

exec 9>"$LOCAL_ROOT/.lock"
flock -n 9 || fail "another pull is already running"

# The backup root is dg-owned and unreadable to the login user, so both
# the listing and the pack run under passwordless sudo on the remote
# side. BatchMode keeps a key problem a fast error, not a prompt that
# hangs the timer.
SSH=(ssh -i "$KEY" -o BatchMode=yes -o ConnectTimeout=20 "$USER@$HOST")
SET="$("${SSH[@]} "sudo -n ls '$REMOTE_ROOT'" 2>/dev/null | grep -E '^[0-9]{8}T[0-9]{6}Z$' | sort | tail -1)" || true
[ -n "$SET" ] || fail "no snapshot set found under $REMOTE_ROOT"
if [ -f "$LOCAL_ROOT/$SET/.ok" ]; then
    echo "[pull-backup] $SET already pulled; nothing to do"
    exit 0
fi

STAGE="$LOCAL_ROOT/.staging-$SET"
rm -rf "$STAGE"
mkdir -p "$STAGE"
# A failed transfer must not leave a half-written tarball that the next
# run mistakes for a completed pull: the .ok marker is written only
# after every checksum below passes.
"${SSH[@]}" "sudo -n tar -C '$REMOTE_ROOT' -cz '$SET'" >"$STAGE/$SET.tar.gz" \
    || fail "transfer of $SET failed"
tar -xzf "$STAGE/$SET.tar.gz" -C "$STAGE" || fail "unpack of $SET failed"
[ -d "$STAGE/$SET" ] || fail "archive of $SET has no top directory"
verified=0
for sidecar in "$STAGE/$SET"/*.sha256; do
    [ -e "$sidecar" ] || continue
    (cd "$STAGE/$SET" && sha256sum -c "$(basename "$sidecar")") >/dev/null \
        || fail "checksum mismatch in $SET/$(basename "$sidecar")"
    verified=$((verified + 1))
done
[ "$verified" -gt 0 ] || fail "$SET carries no checksum sidecars"
python3 -c "import json,sys; json.load(open(sys.argv[1]))" "$STAGE/$SET/manifest.json" \
    || fail "$SET/manifest.json is not valid JSON"
bytes="$(stat -c %s "$STAGE/$SET.tar.gz")"
rm -rf "$LOCAL_ROOT/$SET"
mv "$STAGE/$SET" "$LOCAL_ROOT/$SET"
mv "$STAGE/$SET.tar.gz" "$LOCAL_ROOT/$SET.tar.gz"
date -u +%Y-%m-%dT%H:%M:%SZ >"$LOCAL_ROOT/$SET/.ok"
rmdir "$STAGE"
printf '{"ts":"%s","set":"%s","outcome":"ok","bytes":%s,"sidecars":%s}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$SET" "$bytes" "$verified" >>"$LOCAL_ROOT/pulls.jsonl"
# Prune oldest first, newest $KEEP survive; the tarballs prune with
# their directories by set name.
kept=0
while IFS= read -r dir; do
    [ -n "$dir" ] || continue
    kept=$((kept + 1))
    if [ "$kept" -gt "$KEEP" ]; then
        rm -rf "$dir" "$dir.tar.gz"
    fi
done < <(find "$LOCAL_ROOT" -maxdepth 1 -name '[0-9]*T[0-9]*Z' -type d 2>/dev/null | sort -r)
echo "[pull-backup] $SET pulled and verified ($verified sidecars, $bytes bytes)"
