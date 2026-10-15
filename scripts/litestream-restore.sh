#!/usr/bin/env bash
# Restore every live dg database from the off-host litestream replica
# and prove the row counts and PRAGMA integrity_check match the source.
#
# Usage: scripts/litestream-restore.sh [--out DIR] [--config PATH]
#
# Reads the bucket name from /etc/dg/litestream.env (the only copy of
# the credentials on the host, mode 0600, owner dg:dg) and uses the
# /etc/litestream.yml installed by scripts/install-litestream.sh as the
# litestream config. The default --out DIR is /tmp/lit-restore and is
# created with mode 0700 so the restored files are not readable by any
# other user. For each live *.sqlite under /var/lib/dg the script:
#   1) runs `litestream restore` into $OUT_DIR/<db>.sqlite,
#   2) copies the restored file back to the operator host for inspection,
#   3) runs `PRAGMA integrity_check` and asserts the result is 'ok',
#   4) iterates every user table and asserts count(*) matches the source
#      exactly.
# A non-zero exit on the first mismatch.
#
# This is the live proof for the goal's verification contract clause
# "litestream restore of every live database into a temp path succeeds,
# and row counts plus PRAGMA integrity_check match the live sources
# exactly." It is also a sandbox-packet gate; the same script is safe to
# run as a one-off drill on micro2 from a real shell.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SELF="litestream-restore"
fail() { echo "[$SELF] FAIL: $*" >&2; exit 1; }
log() { echo "[$SELF] $*" >&2; }

OUT_DIR="/tmp/lit-restore"
CONFIG="/etc/litestream.yml"
while [ $# -gt 0 ]; do
  case "$1" in
    --out)     OUT_DIR="$2"; shift 2 ;;
    --config)  CONFIG="$2"; shift 2 ;;
    *)         fail "unknown argument: $1 (expected: --out DIR, --config PATH)" ;;
  esac
done

TARGET="${MICRO2_SSH:-micro2}"
command -v ssh >/dev/null || fail "ssh not on PATH"
command -v sqlite3 >/dev/null || fail "sqlite3 not on PATH (needed to read the restored copy on this host)"

# --- 0. sanity: the source data is readable and the bucket is in env. ---
log "reading LITESTREAM_S3_BUCKET from /etc/dg/litestream.env on ${TARGET}"
BUCKET=$(ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
  'set -e; . /etc/dg/litestream.env; printf "%s" "$LITESTREAM_S3_BUCKET"' 2>/dev/null)
[ -n "$BUCKET" ] || fail "LITESTREAM_S3_BUCKET not set in /etc/dg/litestream.env on ${TARGET}"

log "listing live dg databases on ${TARGET}"
mapfile -t DBS < <(ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
  'set -e; shopt -s nullglob; for db in /var/lib/dg/*.sqlite; do basename "$db" .sqlite; done' 2>/dev/null)
[ "${#DBS[@]}" -gt 0 ] || fail "no live .sqlite databases under /var/lib/dg on ${TARGET}"

# --- 1. prepare a clean restore target. ---
rm -rf "$OUT_DIR"
mkdir -m 0700 -p "$OUT_DIR"
log "restore output directory: $OUT_DIR"

# Discover the operator-host ssh target name (the hostname portion of
# the SSH config alias) so we can scp the restored file back to inspect
# it with sqlite3 on the operator host.
SSH_HOST=$(ssh -G "$TARGET" 2>/dev/null | awk '/^hostname /{print $2; exit}')
[ -n "$SSH_HOST" ] || fail "could not resolve the ssh hostname of the ${TARGET} alias (does your ~/.ssh/config define it?)"

# --- 2. for each live db, restore into $OUT_DIR and compare. ---
ok_count=0
fail_count=0
for name in "${DBS[@]}"; do
  out="${OUT_DIR}/${name}.sqlite"
  log "[${name}] litestream restore -> ${out}"
  ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
    "sudo -n -u dg /usr/local/bin/litestream restore -config '${CONFIG}' -o '${out}' 's3://${BUCKET}/${name}'" \
    >/dev/null 2>&1 || { echo "[$SELF] FAIL: litestream restore for ${name} did not exit 0" >&2; fail_count=$((fail_count+1)); continue; }

  # Pull the restored file back so sqlite3 on the operator host can
  # read it. The file is already mode 0600 dg:dg on micro2, but the
  # scp path is /tmp on the operator host (operator-only), and we delete
  # the copy as soon as the integrity_check + row-counts are done.
  cmp_path="${OUT_DIR}/_cmp_${name}.sqlite"
  scp -o BatchMode=yes -q "ubuntu@${SSH_HOST}:${out}" "$cmp_path" 2>/dev/null

  # --- 2a. PRAGMA integrity_check on the restored copy. ---
  integrity=$(sqlite3 "$cmp_path" 'PRAGMA integrity_check;' 2>/dev/null | tail -1)
  if [ "$integrity" != "ok" ]; then
    echo "[$SELF] FAIL: [${name}] restored copy PRAGMA integrity_check = '$integrity' (expected 'ok')" >&2
    fail_count=$((fail_count+1))
    rm -f "$cmp_path"
    continue
  fi

  # --- 2b. row counts per user table. List tables on both sides, then
  # count(*) for each. Use sqlite3 on the operator host for the
  # restored copy, and the dg CLI on micro2 for the source (it speaks
  # the same SQL the gateway enforces, so the count is the one the
  # production process is serving). ---
  # Tables present in the source (the canonical set).
  src_tables=$(ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
    "sudo -n -u dg /usr/local/bin/dg --key-file /etc/dg/owner.key query '${name}' \"SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'\"" 2>/dev/null \
    | sed -n 's/^|\([a-zA-Z0-9_]\{1,\}\)|.*$/\1/p' | sort -u)
  # Tables present in the restored copy.
  cmp_tables=$(sqlite3 "$cmp_path" \
    "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%';" 2>/dev/null | sort -u)

  table_set_ok=1
  # Disjoint set comparison: the table NAMES must match.
  if [ "$src_tables" != "$cmp_tables" ]; then
    only_src=$(comm -23 <(printf '%s\n' "$src_tables") <(printf '%s\n' "$cmp_tables"))
    only_cmp=$(comm -13 <(printf '%s\n' "$src_tables") <(printf '%s\n' "$cmp_tables"))
    echo "[$SELF] FAIL: [${name}] table set differs; only-in-source: [$only_src]; only-in-restored: [$only_cmp]" >&2
    table_set_ok=0
  fi

  # For each table present in both, count must match.
  cmp_ok=1
  while read -r table; do
    [ -n "$table" ] || continue
    # escape single quotes for the dg CLI's SQL argument; the dg CLI
    # accepts the SQL as a single positional arg.
    esc=$(printf "%s" "$table" | sed "s/'/''/g")
    src_n=$(ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
      "sudo -n -u dg /usr/local/bin/dg --key-file /etc/dg/owner.key query '${name}' \"SELECT count(*) FROM \\\"${esc}\\\"\"" 2>/dev/null \
      | sed -n 's/^|\([0-9]\{1,\}\)|.*$/\1/p' | head -1)
    cmp_n=$(sqlite3 "$cmp_path" "SELECT count(*) FROM \"${esc}\";" 2>/dev/null | tr -d '[:space:]')
    if [ "$src_n" != "$cmp_n" ]; then
      echo "[$SELF] FAIL: [${name}] table '${table}' row count src=${src_n} restored=${cmp_n}" >&2
      cmp_ok=0
    fi
  done <<< "$src_tables"

  if [ "$table_set_ok" = "1" ] && [ "$cmp_ok" = "1" ]; then
    log "[${name}] OK: integrity_check=ok, all row counts match"
    ok_count=$((ok_count+1))
  else
    fail_count=$((fail_count+1))
  fi
  rm -f "$cmp_path"
done

# --- 3. summary. ---
log "summary: ${ok_count} ok, ${fail_count} failed (of ${#DBS[@]} dbs)"
[ "$fail_count" = "0" ] || fail "${fail_count} database(s) failed restore + row-count comparison"
log "all dbs restored; restored copies in ${OUT_DIR}"
