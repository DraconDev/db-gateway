#!/usr/bin/env bash
# Restore every live dg database from the off-host litestream replica
# and prove the row counts and PRAGMA integrity_check match the source.
#
# Usage: scripts/litestream-restore.sh [--out DIR] [--config PATH]
#
# "Live database" means REGISTERED database -- what the gateway itself
# serves: `dg --key-file /etc/dg/owner.key list`. That set deliberately
# excludes metadata.sqlite, the gateway's own registry, which the
# installer (scripts/install-litestream.sh) also excludes from
# replication: the running gateway writes it in WAL mode, so litestream
# must never checkpoint it, and the on-host hourly backup set already
# protects it. A plain `*.sqlite` glob would pick the registry up and
# fail the round-trip on the one database that is *meant* to have no
# replica. The registry is the contract's "live database" list; the
# replica set and this restore set are the same list by construction.
#
# Reads the bucket name from /etc/dg/litestream.env (the only copy of
# the credentials on the host, mode 0600, owner dg:dg, populated into
# the sidecar's environment by systemd) and uses the /etc/litestream.yml
# installed by scripts/install-litestream.sh as the litestream config.
# The default --out DIR is /tmp/lit-restore, mode 0700: the restored
# files stay on the operator host, readable by nobody else. For each
# registered database the script:
#   1) runs `litestream restore` on micro2 (as dg) into a mode-0700
#      scratch dir, pulling the credentials from /etc/dg/litestream.env
#      exactly the way the sidecar itself does,
#   2) streams the restored file back over ssh (sudo cat) so sqlite3 on
#      this host can read it -- the remote copy is never loosened past
#      0600 in a 0700 directory,
#   3) runs `PRAGMA integrity_check` and asserts the result is 'ok',
#   4) compares the per-table row counts of the restored file against
#      the live source, counted through the gateway's own dg CLI.
# A non-zero exit on the first mismatch.
#
# LITESTREAM 0.5.17 INVOCATION NOTE (why the call below looks the way it
# does): `litestream restore` accepts either a DB_PATH or a bare REPLICA
# URL, but NOT -config together with a replica URL ("cannot specify a
# replica URL and the -config flag"). The URL mode also reads credentials
# from litestream's own env names (LITESTREAM_ACCESS_KEY_ID et al.), not
# the LITESTREAM_S3_* names this deployment uses, so URL mode would need
# the keys renamed. DB_PATH + -config is the mode that expands
# ${LITESTREAM_S3_*} from the sidecar env, so the restore uses the same
# credentials, the same config, and the same live-db selection the
# running sidecar uses -- one code path, no second place to get wrong.
#
# This is the live proof for the goal's verification contract clause
# "litestream restore of every live database into a temp path succeeds,
# and row counts plus PRAGMA integrity_check match the live sources
# exactly."

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
ENV_PATH="/etc/dg/litestream.env"
KEY_FILE="/etc/dg/owner.key"
DATA_DIR="/var/lib/dg"
LITESTREAM="/usr/local/bin/litestream"
DG_BIN="/usr/local/bin/dg"
REMOTE_OUT="/var/tmp/lit-restore-$(date +%s)"
DG_QUERY_CACHE="$(mktemp)"
trap 'rm -f "$DG_QUERY_CACHE"' EXIT

command -v ssh >/dev/null || fail "ssh not on PATH"
command -v sqlite3 >/dev/null || fail "sqlite3 not on PATH (needed to read the restored copy on this host)"
command -v python3 >/dev/null || fail "python3 not on PATH (parses the dg CLI's JSON output)"

# --- helpers -----------------------------------------------------------

# dg CLI output is JSON ({"columns":[...],"rows":[[...],...]}); there is
# no jq on the operator host, so a 3-line python parses it. `dg rows`
# flattens every row to a single joined line for set comparison; `dg
# cell` takes the first cell of the first row for scalar counts.
dg_rows() { python3 -c 'import sys,json; [print("\n".join(str(c) for c in r)) for r in json.load(sys.stdin).get("rows",[])]'; }
dg_cell() { python3 -c 'import sys,json; d=json.load(sys.stdin); r=d.get("rows") or [[None]]; print(r[0][0] if r[0] else "")'; }

# Query through the gateway CLI as the dg user on the target host: the
# same SQL surface the production gateway enforces, so the source counts
# are what the gateway serves, not a second opinion about a file.
dg_query() { # $1 = db name, $2 = SQL
  ssh -o BatchMode=yes -o ConnectTimeout=15 -T -- "$TARGET" \
    "sudo -n -u dg ${DG_BIN} --key-file ${KEY_FILE} query '$1' \"$2\""
}

# --- 0. sanity: env file readable, bucket set, registry non-empty ------
log "checking ${ENV_PATH} and LITESTREAM_S3_BUCKET on ${TARGET} (as dg)"
BUCKET=$(ssh -o BatchMode=yes -o ConnectTimeout=15 -T -- "$TARGET" \
  "sudo -n -u dg bash -c \"set -e; . ${ENV_PATH}; printf '%s' \\\"\\\$LITESTREAM_S3_BUCKET\\\"\"" 2>/dev/null)
[ -n "$BUCKET" ] || fail "LITESTREAM_S3_BUCKET not set in ${ENV_PATH} on ${TARGET} (read as dg; /etc/dg is not readable by the login user)"

log "listing registered live databases on ${TARGET}"
mapfile -t DBS < <(ssh -o BatchMode=yes -o ConnectTimeout=15 -T -- "$TARGET" \
  "sudo -n -u dg ${DG_BIN} --key-file ${KEY_FILE} list" | dg_rows)
[ "${#DBS[@]}" -gt 0 ] || fail "no registered databases returned by dg list on ${TARGET}"

# --- 1. prepare a clean restore target (operator host, mode 0700). -----
rm -rf "$OUT_DIR"
mkdir -m 0700 -p "$OUT_DIR"
log "restore output directory: ${OUT_DIR} (local; scratch on micro2: ${REMOTE_OUT})"

# --- 2. for each live db, restore on micro2, pull back, compare. -------
ok_count=0
fail_count=0
for name in "${DBS[@]}"; do
  case "$name" in
    *[!a-zA-Z0-9_-]*) fail "registry name '$name' contains unexpected characters; refusing to build a path from it" ;;
  esac
  out="${OUT_DIR}/${name}.sqlite"
  log "[${name}] litestream restore (as dg, ${CONFIG}) -> ${REMOTE_OUT}/${name}.sqlite on ${TARGET}"
  # One remote script as dg: load the env file (the only credential
  # copy), make a 0700 scratch dir, restore from the DB_PATH so the
  # config's ${LITESTREAM_S3_*} references expand against the same env
  # the sidecar runs with. stdout suppressed -- exit code is the signal.
  if ! ssh -o BatchMode=yes -o ConnectTimeout=15 -T -- "$TARGET" \
    "sudo -n -u dg bash -c \"set -a; . ${ENV_PATH}; set +a; mkdir -p -m 0700 ${REMOTE_OUT}; ${LITESTREAM} restore -config ${CONFIG} -o ${REMOTE_OUT}/${name}.sqlite ${DATA_DIR}/${name}.sqlite\"" \
    >/dev/null 2>&1; then
    echo "[$SELF] FAIL: [${name}] litestream restore did not exit 0 (config ${CONFIG}, env ${ENV_PATH} as dg)" >&2
    fail_count=$((fail_count+1)); continue
  fi

  # Stream the restored file back over ssh: sudo cat keeps the remote
  # copy 0600 in a 0700 directory while the operator host inspects it in
  # its own 0700 OUT_DIR. (scp as the login user cannot read it, and
  # loosening the remote mode just to copy a db out is worse than cat.)
  log "[${name}] pulling the restored file back for inspection"
  ssh -o BatchMode=yes -o ConnectTimeout=15 -T -- "$TARGET" \
    "sudo -n cat ${REMOTE_OUT}/${name}.sqlite" > "$out" 2>/dev/null \
    || { echo "[$SELF] FAIL: [${name}] could not pull the restored file back" >&2; fail_count=$((fail_count+1)); continue; }
  [ -s "$out" ] || { echo "[$SELF] FAIL: [${name}] restored file is empty" >&2; fail_count=$((fail_count+1)); continue; }

  # --- 2a. PRAGMA integrity_check on the restored copy. --------------
  integrity=$(sqlite3 "$out" 'PRAGMA integrity_check;' 2>/dev/null | tail -1)
  if [ "$integrity" != "ok" ]; then
    echo "[$SELF] FAIL: [${name}] restored copy PRAGMA integrity_check = '${integrity}' (expected 'ok')" >&2
    fail_count=$((fail_count+1)); continue
  fi

  # --- 2b. row counts per user table, both sides of the same name. ---
  # Source tables (canonical set) through the gateway CLI.
  src_tables=$(dg_query "$name" \
    "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'" \
    | dg_rows | LC_ALL=C sort -u)
  # Tables in the restored copy, read locally.
  cmp_tables=$(sqlite3 "$out" \
    "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%';" 2>/dev/null | LC_ALL=C sort -u)

  if [ "$src_tables" != "$cmp_tables" ]; then
    only_src=$(comm -23 <(printf '%s\n' "$src_tables") <(printf '%s\n' "$cmp_tables"))
    only_cmp=$(comm -13 <(printf '%s\n' "$src_tables") <(printf '%s\n' "$cmp_tables"))
    echo "[$SELF] FAIL: [${name}] table set differs; only-in-source: [${only_src}]; only-in-restored: [${only_cmp}]" >&2
    fail_count=$((fail_count+1)); continue
  fi

  cmp_ok=1
  while read -r table; do
    [ -n "$table" ] || continue
    src_n=$(dg_query "$name" "SELECT count(*) FROM \"${table}\"" | dg_cell)
    cmp_n=$(sqlite3 "$out" "SELECT count(*) FROM \"${table}\";" 2>/dev/null | tr -d '[:space:]')
    if [ "$src_n" != "$cmp_n" ]; then
      echo "[$SELF] FAIL: [${name}] table '${table}' row count src=${src_n} restored=${cmp_n}" >&2
      cmp_ok=0
    fi
  done <<< "$src_tables"

  if [ "$cmp_ok" = "1" ]; then
    log "[${name}] OK: integrity_check=ok, all ${#} row counts match"
    ok_count=$((ok_count+1))
  else
    fail_count=$((fail_count+1))
  fi
done

# --- 3. cleanup + summary. ---------------------------------------------
log "removing the scratch dir on ${TARGET}"
ssh -o BatchMode=yes -o ConnectTimeout=15 -T -- "$TARGET" "sudo -n rm -rf ${REMOTE_OUT}" >/dev/null 2>&1 || true
log "summary: ${ok_count} ok, ${fail_count} failed (of ${#DBS[@]} registered db(s): ${DBS[*]})"
[ "$fail_count" = "0" ] || fail "${fail_count} database(s) failed restore + row-count comparison"
log "all live db(s) restored; local copies kept in ${OUT_DIR} (mode 0700)"
