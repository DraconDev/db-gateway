#!/usr/bin/env bash
# Restore every live dg database from the off-host replication replica
# and prove the row counts and PRAGMA integrity_check match the source.
#
# Usage: scripts/replication-restore.sh [--out DIR] [--config PATH]
#
# "Live database" means REGISTERED database -- what the gateway itself
# serves: `dg --key-file /etc/dg/owner.key list`. This manual
# workstation tool restores those databases only; the registry
# (metadata.sqlite) IS replicated since the recovery-policy change
# (docs/recovery-policy.md, audit A5 -- registry connections are
# per-operation like the application databases, and
# scripts/registry-recovery-drill.sh proves the live round-trip), and
# the scheduled registry-inclusive verification is
# scripts/bucket-restore-drill.sh on the host. Use this script for an
# ad-hoc data-only round-trip from the workstation.
#
# Reads the bucket name from /etc/dg/replication.env (the only copy of
# the credentials on the host, mode 0600, owner dg:dg, populated into
# the sidecar's environment by systemd) and uses the /etc/replication.yml
# installed by scripts/install-replication.sh as the replication config.
# The default --out DIR is /tmp/lit-restore, mode 0700: the restored
# files stay on the operator host, readable by nobody else. For each
# registered database the script:
#   1) runs `replication restore` on deployment-host (as dg) into a mode-0700
#      scratch dir, pulling the credentials from /etc/dg/replication.env
#      exactly the way the sidecar itself does,
#   2) streams the restored file back over ssh (sudo cat) so sqlite3 on
#      this host can read it -- the remote copy is never loosened past
#      0600 in a 0700 directory,
#   3) runs `PRAGMA integrity_check` and asserts the result is 'ok',
#   4) compares the per-table row counts of the restored file against
#      the live source, counted through the gateway's own dg CLI.
# A non-zero exit on the first mismatch.
#
# WHAT "MATCH" COMPARES: the USER tables. SQLite's own sqlite_* plus the
# two internal families (_dg_*, the gateway's replay bookkeeping, and
# _replication_*, replication's own sync state) are excluded from the
# table-set comparison on BOTH sides. The source side must exclude them:
# the dg CLI answers sql_forbidden (403) for internal tables by design,
# so counting them through the gateway is impossible, not merely
# awkward -- and the contract's "row counts match the live sources"
# means the data the gateway serves. The restored copy does contain the
# internal tables (a faithful page-level restore reproduces them), so
# the comparison stays exact over the same named set on both sides.
# PRAGMA integrity_check still covers the whole restored file, internals
# included.
#
# REPLICATION 0.5.17 INVOCATION NOTE (why the call below looks the way it
# does): `replication restore` accepts either a DB_PATH or a bare REPLICA
# URL, but NOT -config together with a replica URL ("cannot specify a
# replica URL and the -config flag"). The URL mode also reads credentials
# from replication's own env names (REPLICATION_ACCESS_KEY_ID et al.), not
# the LITESTREAM_S3_* names this deployment uses, so URL mode would need
# the keys renamed. DB_PATH + -config is the mode that expands
# ${LITESTREAM_S3_*} from the sidecar env, so the restore uses the same
# credentials, the same config, and the same live-db selection the
# running sidecar uses -- one code path, no second place to get wrong.
#
# This is the live proof for the goal's verification contract clause
# "replication restore of every live database into a temp path succeeds,
# and row counts plus PRAGMA integrity_check match the live sources
# exactly."

set -euo pipefail
SELF="replication-restore"
fail() { echo "[$SELF] FAIL: $*" >&2; exit 1; }
log() { echo "[$SELF] $*" >&2; }

OUT_DIR="/tmp/lit-restore"
CONFIG="/etc/replication.yml"
while [ $# -gt 0 ]; do
  case "$1" in
    --out)     OUT_DIR="$2"; shift 2 ;;
    --config)  CONFIG="$2"; shift 2 ;;
    *)         fail "unknown argument: $1 (expected: --out DIR, --config PATH)" ;;
  esac
done

TARGET="${DEPLOYMENT_HOST_SSH:-deployment-host}"
ENV_PATH="/etc/dg/replication.env"
KEY_FILE="/etc/dg/owner.key"
DATA_DIR="/var/lib/dg"
REPLICATION="/usr/local/bin/replication"
DG_BIN="/usr/local/bin/dg"
REMOTE_OUT="/var/tmp/lit-restore-$(date +%s)"
DG_QUERY_CACHE="$(mktemp)"
# The remote scratch dir holds restored database copies. Remove it on
# EVERY exit path, not just the success path: a failure inside the
# per-database loop (a source that cannot be listed, a count that does
# not match) used to leave restored copies under /var/tmp on the host.
cleanup() {
    # `|| true`: the trap runs under `set -e`, so a failing rm would
    # abort this function before the remote scratch dir was removed --
    # the one cleanup that matters would be skipped by the one that
    # does not.
    rm -f "$DG_QUERY_CACHE" || true
    ssh -o BatchMode=yes -o ConnectTimeout=15 -T -- "$TARGET" \
        "sudo -n rm -rf ${REMOTE_OUT}" >/dev/null 2>&1 || true
    return 0
}
trap cleanup EXIT

command -v ssh >/dev/null || fail "ssh not on PATH"
command -v sqlite3 >/dev/null || fail "sqlite3 not on PATH (needed to read the restored copy on this host)"
command -v python3 >/dev/null || fail "python3 not on PATH (parses the dg CLI's JSON output)"

# --- helpers -----------------------------------------------------------

# The dg CLI speaks JSON: `list` returns {"databases":[...]} while `query`
# returns {"columns":[...],"rows":[[...],...]}; there is no jq on the operator
# host, so tiny inline pythons parse both. `dg_rows` flattens query rows
# to lines for set comparison; `dg` lists registry names; `dg_cell`
# takes the first cell of the first row for scalar counts.
dg_rows() { python3 -c 'import sys,json; [print("\n".join(str(c) for c in r)) for r in json.load(sys.stdin).get("rows",[])]'; }
dg()      { python3 -c 'import sys,json; [print(x) for x in json.load(sys.stdin)["databases"]]'; }
dg_cell() { python3 -c 'import sys,json; d=json.load(sys.stdin); r=d.get("rows") or [[None]]; print(r[0][0] if r[0] else "")'; }

# Query through the gateway CLI as the dg user on the target host: the
# same SQL surface the production gateway enforces, so the source counts
# are what the gateway serves, not a second opinion about a file.
#
# The SQL travels base64-encoded: it contains both quote styles (e.g.
# SELECT count(*) FROM "table"), and either of them would be eaten by
# one of the two shell-quoting layers between here and the CLI's argv
# (that mangling once produced `FROM _dg_replay`, which the gateway
# rejected with an opaque api_error). Encoding makes the delivered argv
# byte-exact for any SQL, and nothing sensitive is in the payload.
dg_query() { # $1 = db name, $2 = SQL
  local b64
  b64=$(printf '%s' "$2" | base64 -w0)
  ssh -o BatchMode=yes -o ConnectTimeout=15 -T -- "$TARGET" \
    "sudo -n -u dg ${DG_BIN} --key-file ${KEY_FILE} query '$1' \"\$(printf '%s' '$b64' | base64 -d)\""
}

# --- 0. sanity: env file readable, bucket set, registry non-empty ------
log "checking ${ENV_PATH} and LITESTREAM_S3_BUCKET on ${TARGET} (as dg)"
# The remote script is piped over stdin (no quoting layers in argv): the
# unquoted heredoc expands ${ENV_PATH} locally and keeps the dollar of the
# remote variable literal, so the credential value is read on the host by
# the dg user only and never crosses into the command line.
# The unquoted heredoc is the mechanism, per the comment above:
# ${ENV_PATH} expands locally, \$LITESTREAM_S3_BUCKET stays literal
# for the remote side.
# shellcheck disable=SC2087
BUCKET=$(ssh -o BatchMode=yes -o ConnectTimeout=15 -T -- "$TARGET" 'sudo -n -u dg bash -s' <<REMOTE 2>/dev/null
set -a; . ${ENV_PATH}; printf %s "\$LITESTREAM_S3_BUCKET"
REMOTE
)
[ -n "$BUCKET" ] || fail "LITESTREAM_S3_BUCKET not set in ${ENV_PATH} on ${TARGET} (read as dg; /etc/dg is not readable by the login user)"

log "listing registered live databases on ${TARGET}"
mapfile -t DBS < <(ssh -o BatchMode=yes -o ConnectTimeout=15 -T -- "$TARGET" \
  "sudo -n -u dg ${DG_BIN} --key-file ${KEY_FILE} list" | dg)
[ "${#DBS[@]}" -gt 0 ] || fail "no registered databases returned by dg list on ${TARGET}"

# --- 1. prepare a clean restore target (operator host, mode 0700). -----
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"
chmod 0700 "$OUT_DIR"
log "restore output directory: ${OUT_DIR} (local; scratch on deployment-host: ${REMOTE_OUT})"

# --- 2. for each live db, restore on deployment-host, pull back, compare. -------
ok_count=0
fail_count=0
for name in "${DBS[@]}"; do
  case "$name" in
    *[!a-zA-Z0-9_-]*) fail "registry name '$name' contains unexpected characters; refusing to build a path from it" ;;
  esac
  out="${OUT_DIR}/${name}.sqlite"
  log "[${name}] replication restore (as dg, ${CONFIG}) -> ${REMOTE_OUT}/${name}.sqlite on ${TARGET}"
  # One remote script as dg: load the env file (the only credential
  # copy), make a 0700 scratch dir, restore from the DB_PATH so the
  # config's ${LITESTREAM_S3_*} references expand against the same env
  # the sidecar runs with. stdout suppressed -- exit code is the signal.
  if ! ssh -o BatchMode=yes -o ConnectTimeout=15 -T -- "$TARGET" \
    "sudo -n -u dg bash -c \"set -a; . ${ENV_PATH}; set +a; mkdir -p -m 0700 ${REMOTE_OUT}; ${REPLICATION} restore -config ${CONFIG} -o ${REMOTE_OUT}/${name}.sqlite ${DATA_DIR}/${name}.sqlite\"" \
    >/dev/null 2>&1; then
    echo "[$SELF] FAIL: [${name}] replication restore did not exit 0 (config ${CONFIG}, env ${ENV_PATH} as dg)" >&2
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
  # Source tables (canonical set) through the gateway CLI. A dg CLI
  # error here (its JSON error object, not a result set) must surface
  # as this script's own failure, never as a silently-empty table list.
  src_tables=$(dg_query "$name" \
    "SELECT name FROM sqlite_master WHERE type='table' AND name NOT GLOB 'sqlite_*' AND name NOT GLOB '_dg_*' AND name NOT GLOB '_replication_*'" \
    | dg_rows | LC_ALL=C sort -u) || fail "[${name}] source table listing failed (dg CLI error)"
  # Tables in the restored copy, read locally. The same internal-table
  # exclusion, so the compared set is the same set on both sides.
  cmp_tables=$(sqlite3 "$out" \
    "SELECT name FROM sqlite_master WHERE type='table' AND name NOT GLOB 'sqlite_*' AND name NOT GLOB '_dg_*' AND name NOT GLOB '_replication_*';" 2>/dev/null | LC_ALL=C sort -u)

  if [ "$src_tables" != "$cmp_tables" ]; then
    only_src=$(comm -23 <(printf '%s\n' "$src_tables") <(printf '%s\n' "$cmp_tables"))
    only_cmp=$(comm -13 <(printf '%s\n' "$src_tables") <(printf '%s\n' "$cmp_tables"))
    echo "[$SELF] FAIL: [${name}] table set differs; only-in-source: [${only_src}]; only-in-restored: [${only_cmp}]" >&2
    fail_count=$((fail_count+1)); continue
  fi

  cmp_ok=1
  while read -r table; do
    [ -n "$table" ] || continue
    src_n=$(dg_query "$name" "SELECT count(*) FROM \"${table}\"" | dg_cell) || fail "[${name}] source count for table '${table}' failed (dg CLI error)"
    cmp_n=$(sqlite3 "$out" "SELECT count(*) FROM \"${table}\";" 2>/dev/null | tr -d '[:space:]')
    if [ "$src_n" != "$cmp_n" ]; then
      echo "[$SELF] FAIL: [${name}] table '${table}' row count src=${src_n} restored=${cmp_n}" >&2
      cmp_ok=0
    fi
  done <<< "$src_tables"

  if [ "$cmp_ok" = "1" ]; then
    tbl_count=$(printf '%s\n' "$src_tables" | grep -c . || true)
    log "[${name}] OK: integrity_check=ok, all ${tbl_count} table row counts match"
    ok_count=$((ok_count+1))
  else
    fail_count=$((fail_count+1))
  fi
done

# --- 3. cleanup + summary. ---------------------------------------------
# The remote scratch dir is removed by the EXIT trap, so a failure
# earlier in this run leaves no restored copies behind either.
log "removing the scratch dir on ${TARGET}"
log "summary: ${ok_count} ok, ${fail_count} failed (of ${#DBS[@]} registered db(s): ${DBS[*]})"
[ "$fail_count" = "0" ] || fail "${fail_count} database(s) failed restore + row-count comparison"
log "all live db(s) restored; local copies kept in ${OUT_DIR} (mode 0700)"
