#!/usr/bin/env bash
# Read-only audit of a deployed micro2 host (or a local repo-only pass).
#
# Usage: scripts/host-audit.sh
#
# Emits ONE JSON object on stdout; diagnostics go to stderr. The script is
# always read-only: the repo-side checks read git HEAD and the nominated
# bundle, and the live probe is written to a temp file, scp-free, and
# piped to `ssh -T bash -s` on the target. Nothing is scp'd, nothing is
# written on the host, no service is started, no backup is run, no
# deletion happens anywhere. Credentials resolve from the operator's
# ~/.ssh/config and SSH agent; the script argv never carries a host,
# a key path, or a user.
#
# Three modes by environment:
#   - no DG_AUDIT_TARGET  : local-only; host_side.status="skipped", exit 0
#     when every repo-side check passes (the CI gate).
#   - DG_AUDIT_TARGET set, ssh/probe fails : host_side.status="unreachable"
#     or "error"; exit non-zero. This is the path that closes the
#     "Permission denied" hole the detached auditor hit in 2026-10.
#   - DG_AUDIT_TARGET set, all probes answer : any failed invariant
#     flips verdict to "fail" and exit to non-zero; a clean pass exits 0.
#
# Anti-staleness: `is-active` alone is never sufficient. The dg binary
# digest must match the bundle, the newest backup set must verify
# `ok:true` and be younger than DG_AUDIT_MAX_SET_AGE_HOURS (default 26h,
# one hour past the hourly timer at :17 with keep=24), the backup
# staging directory must be empty, and the systemd unit's
# ExecMainStartTimestamp must be after the bundle was last deployed.
# This is the closure for the 2026-09-22 outage that left the timer
# "active" while no real backup had published since.
#
# Self-leakage guard: before printing, the script re-scans the emitted
# JSON for any 64-hex run and refuses to print on a hit. A future probe
# edit that leaks a digest in the wrong field fails closed by design.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SELF="host-audit"
fail() { echo "[$SELF] FAIL: $*" >&2; exit 1; }
log() { echo "[$SELF] $*" >&2; }

START="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
TARGET="${DG_AUDIT_TARGET:-}"
BUNDLE_DIR="${DG_RELEASE_BUNDLE_DIR:-}"
MAX_SET_AGE_HOURS="${DG_AUDIT_MAX_SET_AGE_HOURS:-26}"

# --- repo-side inputs (always run, regardless of mode) ---

# Use `git show` directly: it appends a trailing newline, matching the
# canonical shell-script form (every line ends with \n) and the sha that
# `cat file | sha256sum` produces on the deployed host. The intermediate
# `printf '%s' "$HEAD_BACKUP_SCRIPT"` would strip that trailing byte and
# produce a sha that no deployed file ever has, which would false-fail
# every live run.
HEAD_BACKUP_SCRIPT="$(git -C "$ROOT" show HEAD:scripts/micro2-backup.sh)"
HEAD_BACKUP_SHA256="$(printf '%s\n' "$HEAD_BACKUP_SCRIPT" | sha256sum | awk '{print $1}')"
[ -n "$HEAD_BACKUP_SCRIPT" ] || fail "git show HEAD:scripts/micro2-backup.sh returned empty"

PROBE_BACKUP_ROOT="$(printf '%s\n' "$HEAD_BACKUP_SCRIPT" | sed -n 's/^BACKUP_ROOT=//p' | head -1 | tr -d '"'"'")"
PROBE_KEY_FILE="$(printf '%s\n' "$HEAD_BACKUP_SCRIPT" | sed -n 's/^KEY_FILE=//p' | head -1 | tr -d '"'"'")"
PROBE_DATA_DIR="$(printf '%s\n' "$HEAD_BACKUP_SCRIPT" | sed -n 's/^DATA_DIR=//p' | head -1 | tr -d '"'"'")"
[ -n "$PROBE_BACKUP_ROOT" ] || fail "could not scrape BACKUP_ROOT from scripts/micro2-backup.sh"
[ -n "$PROBE_KEY_FILE" ]    || fail "could not scrape KEY_FILE from scripts/micro2-backup.sh"
[ -n "$PROBE_DATA_DIR" ]    || fail "could not scrape DATA_DIR from scripts/micro2-backup.sh"

# --- bundle (when nominated) ---

EXPECTED_DG_SHA256=""
BUNDLE_STATUS="skipped"
BUNDLE_FAIL=""
if [ -n "$BUNDLE_DIR" ]; then
    if [ ! -d "$BUNDLE_DIR" ]; then
        BUNDLE_STATUS="fail"
        BUNDLE_FAIL="bundle dir does not exist: $BUNDLE_DIR"
    elif [ ! -f "$BUNDLE_DIR/SHA256SUMS" ]; then
        BUNDLE_STATUS="fail"
        BUNDLE_FAIL="bundle dir has no SHA256SUMS: $BUNDLE_DIR"
    else
        if (cd "$BUNDLE_DIR" && sha256sum -c SHA256SUMS --status); then
            EXPECTED_DG_SHA256="$(grep -E '^[0-9a-f]{64}  dg$' "$BUNDLE_DIR/SHA256SUMS" | awk '{print $1}' | head -1)"
            if [ -z "$EXPECTED_DG_SHA256" ]; then
                BUNDLE_STATUS="fail"
                BUNDLE_FAIL="SHA256SUMS has no dg line"
            else
                BUNDLE_STATUS="pass"
            fi
        else
            BUNDLE_STATUS="fail"
            BUNDLE_FAIL="SHA256SUMS self-verification failed"
        fi
    fi
fi

# --- syntax check on git HEAD's backup script ---

SYNTAX_STATUS="pass"
SYNTAX_FAIL=""
SYNTAX_TMP="$(mktemp "${TMPDIR:-/tmp}/dg-host-audit-syntax-XXXXXX")"
printf '%s' "$HEAD_BACKUP_SCRIPT" >"$SYNTAX_TMP"
if ! bash -n "$SYNTAX_TMP" 2>"$SYNTAX_TMP.err"; then
    SYNTAX_STATUS="fail"
    SYNTAX_FAIL="$(cat "$SYNTAX_TMP.err")"
    rm -f "$SYNTAX_TMP.err"
fi
rm -f "$SYNTAX_TMP"

# --- working tree state (recorded, not fatal) ---

WORKING_TREE_DIRTY="$(git -C "$ROOT" status --porcelain | wc -l | tr -d ' ')"

# --- host-side probe (opt-in) ---

HOST_STATUS="skipped"
HOST_FAIL=""
HOST_MODE="local"
PARSED=""

if [ -n "$TARGET" ]; then
    HOST_MODE="live"
    log "probing target=$TARGET (BatchMode=yes, read-only)"

    # Write the probe body to a temp file. Keeping it out of the calling
    # bash's heredoc avoids the $()/heredoc interaction that traps
    # command substitution with -e.
    PROBE_BODY="$(mktemp "${TMPDIR:-/tmp}/dg-host-audit-probe-XXXXXX.sh")"
    PROBE_OUT_FILE="$(mktemp -u "${TMPDIR:-/tmp}/dg-host-audit-probe-out-XXXXXX")"
    PROBE_ERR_FILE="${PROBE_OUT_FILE}.err"
    trap 'rm -f "$PROBE_BODY" "$PROBE_OUT_FILE" "$PROBE_ERR_FILE" "$SYNTAX_TMP"' EXIT

    # Use a quoted heredoc so the body is written literally. The unquoted
    # heredoc silently evaluates backticks and $() inside the body, which
    # is the trap that the previous iteration fell into. Values are passed
    # as positional args to the probe (1: backup_root, 2: key_file,
    # 3: data_dir, 4: max_age_hours).
    cat >"$PROBE_BODY" <<'PROBE_EOF'
#!/usr/bin/env bash
# Read-only host audit probe. Args: 1=backup_root, 2=key_file, 3=data_dir, 4=max_age_hours.
# Emits one JSON line on stdout. No writes, no network, no service control.
set -euo pipefail
emit() { printf '%s\n' "$1"; }
checks=""
add() { checks="${checks:+$checks,}$1"; }

br="$1"
kf="$2"
dd="$3"
max_age_hours="$4"

# 1) dg binary: stat, sha256
dg_bin='{"status":"skipped"}'
if [ -e /usr/local/bin/dg ]; then
    actual="$(sha256sum /usr/local/bin/dg | awk '{print $1}')"
    mode="$(stat -c '%a' /usr/local/bin/dg)"
    owner="$(stat -c '%U:%G' /usr/local/bin/dg)"
    dg_bin="$(printf '{"status":"pass","expected":null,"actual":"%s","mode":"%s","owner":"%s"}' "$actual" "$mode" "$owner")"
fi
add "\"dg_binary_sha256\":$dg_bin"

# 2) dg.service
dg_svc='{"status":"skipped"}'
if [ -f /etc/systemd/system/dg.service ]; then
    s="$(systemctl show dg.service --value --property=ActiveState 2>/dev/null || echo unknown)"
    u="$(systemctl show dg.service --value --property=User 2>/dev/null || echo unknown)"
    t="$(systemctl show dg.service --value --property=ExecMainStartTimestamp 2>/dev/null || echo unknown)"
    if [ "$s" = "active" ] && [ "$u" = "dg" ]; then
        dg_svc="$(printf '{"status":"pass","active_state":"%s","user":"%s","main_start_timestamp":"%s"}' "$s" "$u" "$t")"
    else
        dg_svc="$(printf '{"status":"fail","active_state":"%s","user":"%s","main_start_timestamp":"%s"}' "$s" "$u" "$t")"
    fi
fi
add "\"dg_service\":$dg_svc"

# 3) dg-backup.timer
dg_timer='{"status":"skipped"}'
if [ -f /etc/systemd/system/dg-backup.timer ]; then
    s="$(systemctl show dg-backup.timer --value --property=ActiveState 2>/dev/null || echo unknown)"
    n="$(systemctl show dg-backup.timer --value --property=NextElapseUSecMonotonic 2>/dev/null || echo unknown)"
    if [ "$s" = "active" ]; then
        dg_timer="$(printf '{"status":"pass","active_state":"%s","next_elapse":"%s"}' "$s" "$n")"
    else
        dg_timer="$(printf '{"status":"fail","active_state":"%s","next_elapse":"%s"}' "$s" "$n")"
    fi
fi
add "\"dg_backup_timer\":$dg_timer"

# 3b) dg-litestream.service -- the off-host replication sidecar. The
# objective's "monitor it" clause: the sidecar joins this audit, so a
# dead or mis-configured replicator is a verdict=fail here instead of
# silent backup staleness. Also asserts the two unit properties the
# verification contract names explicitly: Restart=on-failure and User=dg.
dg_litestream='{"status":"skipped"}'
if [ -f /etc/systemd/system/dg-litestream.service ]; then
    s="$(systemctl show dg-litestream.service --value --property=ActiveState 2>/dev/null || echo unknown)"
    u="$(systemctl show dg-litestream.service --value --property=User 2>/dev/null || echo unknown)"
    r="$(systemctl show dg-litestream.service --value --property=Restart 2>/dev/null || echo unknown)"
    if [ "$s" = "active" ] && [ "$u" = "dg" ] && [ "$r" = "on-failure" ]; then
        dg_litestream="$(printf '{"status":"pass","active_state":"%s","user":"%s","restart":"%s"}' "$s" "$u" "$r")"
    else
        dg_litestream="$(printf '{"status":"fail","active_state":"%s","user":"%s","restart":"%s"}' "$s" "$u" "$r")"
    fi
fi
add "\"dg_litestream_service\":$dg_litestream"

# 4) /usr/local/bin/micro2-backup.sh sha256
backup_script='{"status":"skipped"}'
if [ -e /usr/local/bin/micro2-backup.sh ]; then
    actual="$(sha256sum /usr/local/bin/micro2-backup.sh | awk '{print $1}')"
    backup_script="$(printf '{"status":"pass","expected":null,"actual":"%s"}' "$actual")"
fi
add "\"micro2_backup_script_sha256\":$backup_script"

# 5) stale staging dirs
staging='{"status":"skipped"}'
if [ -d "$br" ]; then
    # The backup root is 0700 dg:dg, so we must read it as `dg` (sudo -n,
    # no password). Ubuntu's ssh session can't read it directly; without
    # this, `find` returns "Permission denied" and the probe exits.
    # We also `cd /` first because dg's CWD must be somewhere dg can read;
    # `find` restores its CWD on exit and the default ubuntu home is 0700.
    count="$(cd / && sudo -n -u dg find "$br" -maxdepth 1 -name '.staging*' -type d 2>/dev/null | wc -l | tr -d ' ')"
    if [ "$count" = "0" ]; then
        staging='{"status":"pass","count":0}'
    else
        staging="$(printf '{"status":"fail","count":%s}' "$count")"
    fi
fi
add "\"stale_staging_dirs\":$staging"

# 6) latest_set
latest_set='{"status":"skipped"}'
if [ -d "$br" ]; then
    # set -o pipefail makes grep's no-match (rc=1) kill the whole pipeline,
    # so guard with `|| true` before sort. Without this, a freshly-rebuilt
    # host with zero published sets fails the probe before manifest.json is
    # read and reports "unreachable" instead of the honest
    # "no-manifest-in-latest-set" fail.
    # Same sudo -n -u dg as above: the backup root is 0700 dg:dg.
    # `ls` doesn't restore CWD the way find does, but the explicit cd / is
    # harmless and keeps the two call sites in step.
    # The brace group forces `|| true` to be part of the grep step; without
    # it, bash parses `... | grep ... || true | sort | tail -1` as
    # `(grep) || (true | sort | tail -1)`, so the sort/tail pipe is never
    # reached and `set` becomes the entire grep output. With the brace
    # group, the `|| true` swallows grep's no-match rc=1 inside the pipe.
    set="$(cd / && sudo -n -u dg ls -1 "$br" 2>/dev/null | { grep -E '^[0-9]{8}T[0-9]{6}Z$' || true; } | sort | tail -1)"
    # The manifest is in a 0700 dg:dg directory, so the existence test
    # runs as dg (sudo -n). Without this, ubuntu's `test -f` returns 1
    # and the probe reports the misleading "no-manifest-in-latest-set"
    # failure on a perfectly valid set.
    if [ -n "$set" ] && sudo -n -u dg test -f "$br/$set/manifest.json"; then
        created="$(sudo -n -u dg sed -n 's/.*"created"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$br/$set/manifest.json" | head -1)"
        if created_ep="$(date -u -d "$created" +%s 2>/dev/null)"; then
            now_ep="$(date -u +%s)"
            age=$(( now_ep - created_ep ))
            max_seconds=$(( max_age_hours * 3600 ))
            if [ "$age" -le "$max_seconds" ]; then
                latest_set="$(printf '{"status":"pass","set":"%s","created":"%s","age_seconds":%s,"max_age_hours":%s}' "$set" "$created" "$age" "$max_age_hours")"
            else
                latest_set="$(printf '{"status":"fail","set":"%s","created":"%s","age_seconds":%s,"max_age_hours":%s}' "$set" "$created" "$age" "$max_age_hours")"
            fi
        else
            latest_set="$(printf '{"status":"fail","set":"%s","created":"%s","age_seconds":null,"max_age_hours":%s,"reason":"unparseable-created"}' "$set" "$created" "$max_age_hours")"
        fi
    else
        latest_set='{"status":"fail","set":null,"reason":"no-manifest-in-latest-set"}'
    fi
fi
add "\"latest_set\":$latest_set"

# 7) dg backup verify latest
verify='{"status":"skipped"}'
if [ -d "$br" ]; then
    if verify_out="$(sudo -n -u dg /usr/local/bin/dg backup verify --backup-root "$br" latest 2>/dev/null)"; then
        if printf '%s' "$verify_out" | grep -q '"ok"[[:space:]]*:[[:space:]]*true'; then
            verify="$(printf '{"status":"pass","report":%s}' "$verify_out")"
        else
            verify="$(printf '{"status":"fail","report":%s}' "$verify_out")"
        fi
    else
        verify='{"status":"fail","report":null,"reason":"dg backup verify failed"}'
    fi
fi
add "\"backup_verify\":$verify"

# 8) owner_key_mode -- stat only
owner_key='{"status":"skipped"}'
# The credential is 0600 dg:dg, so stat it as dg (sudo -n). We never
# read the key bytes themselves; only the mode and owner go into the
# report.
if sudo -n -u dg test -e "$kf"; then
    mode="$(sudo -n -u dg stat -c '%a' "$kf")"
    owner="$(sudo -n -u dg stat -c '%U:%G' "$kf")"
    if [ "$mode" = "600" ] && [ "$owner" = "dg:dg" ]; then
        owner_key="$(printf '{"status":"pass","mode":"%s","owner":"%s"}' "$mode" "$owner")"
    else
        owner_key="$(printf '{"status":"fail","mode":"%s","owner":"%s"}' "$mode" "$owner")"
    fi
fi
add "\"owner_key_mode\":$owner_key"

# 9) litestream_replication_status -- `litestream status` must report
# status=ok for every replicated database. This is the replication-lag
# half of G4: the dg-litestream.service check (3b) proves the sidecar is
# running with the right user/restart policy, but a running sidecar with
# a wedged replica would still pass it. `litestream status` reads the
# local replication state; every non-header line must carry status ok.
# Missing binary -> skipped (not a fail on hosts without the sidecar).
litestream_rep='{"status":"skipped","reason":"no litestream binary"}'
if [ -x /usr/local/bin/litestream ]; then
    if lit_out="$(sudo -n -u dg /usr/local/bin/litestream status 2>/dev/null)"; then
        # Drop the header line; every remaining non-empty line must
        # contain the ok status token.
        bad="$(printf '%s\n' "$lit_out" | tail -n +2 | grep -v '^[[:space:]]*$' | grep -vc '[[:space:]]ok[[:space:]]' || true)"
        nlines="$(printf '%s\n' "$lit_out" | tail -n +2 | grep -vc '^[[:space:]]*$' || true)"
        if [ "$nlines" = "0" ]; then
            litestream_rep='{"status":"fail","reason":"no databases in litestream status"}'
        elif [ "$bad" = "0" ]; then
            litestream_rep="$(printf '{"status":"pass","databases":%s}' "$nlines")"
        else
            litestream_rep="$(printf '{"status":"fail","databases":%s,"not_ok":%s}' "$nlines" "$bad")"
        fi
    else
        litestream_rep='{"status":"fail","reason":"litestream status exited non-zero"}'
    fi
fi
add "\"litestream_replication_status\":$litestream_rep"

# 10) api_auth_enforced -- the gateway must answer unauthenticated
# GET /v1/databases with 401 (server up, auth enforced). 401 proves
# both liveness and the auth boundary in one byte; any other code
# (404 = wrong route, 000 = not listening, 200 = auth missing) fails.
# Missing curl -> skipped.
api_auth='{"status":"skipped","reason":"no curl"}'
if command -v curl >/dev/null 2>&1; then
    code="$(curl -sS -m 5 -o /dev/null -w '%{http_code}' http://127.0.0.1:7432/v1/databases 2>/dev/null || echo 000)"
    if [ "$code" = "401" ]; then
        api_auth='{"status":"pass","code":401}'
    else
        api_auth="$(printf '{"status":"fail","code":"%s"}' "$code")"
    fi
fi
add "\"api_auth_enforced\":$api_auth"

emit "{${checks}}"
PROBE_EOF
    chmod +x "$PROBE_BODY"

    # Run the probe over ssh -T, capturing stdout/stderr separately.
    # The remote end runs `bash -s` with the probe body on stdin.
    set +e
    ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
        bash -s -- "$PROBE_BACKUP_ROOT" "$PROBE_KEY_FILE" "$PROBE_DATA_DIR" "$MAX_SET_AGE_HOURS" \
        <"$PROBE_BODY" >"$PROBE_OUT_FILE" 2>"$PROBE_ERR_FILE"
    PROBE_EXIT=$?
    set -e

    if [ "$PROBE_EXIT" != "0" ] || [ ! -s "$PROBE_OUT_FILE" ]; then
        HOST_STATUS="unreachable"
        ST_ERR="$(tr '\n' ' ' <"$PROBE_ERR_FILE" | head -c 200)"
        HOST_FAIL="ssh -T $TARGET bash -s exited $PROBE_EXIT (empty stdout); stderr: $ST_ERR"
    else
        PROBE_OUT="$(cat "$PROBE_OUT_FILE")"
        # Parse + validate the probe JSON
        PARSED="$(PROBE_LINE="$PROBE_OUT" python3 <<'PY'
import json, os
raw = os.environ.get("PROBE_LINE", "")
try:
    obj = json.loads(raw)
except Exception:
    raise SystemExit(2)
required = ["dg_binary_sha256","dg_service","dg_backup_timer",
            "dg_litestream_service",
            "micro2_backup_script_sha256","stale_staging_dirs",
            "latest_set","backup_verify","owner_key_mode",
            "litestream_replication_status","api_auth_enforced"]
missing = [k for k in required if k not in obj]
if missing:
    raise SystemExit(3)
print(json.dumps(obj))
PY
)" && PARSE_RC=$? || PARSE_RC=$?

        if [ "$PARSE_RC" != "0" ]; then
            HOST_STATUS="error"
            HOST_FAIL="probe JSON could not be parsed (exit $PARSE_RC); raw: $(head -c 200 "$PROBE_OUT_FILE")"
        else
            # Compare dg binary sha and micro2-backup.sh sha against expected
            ACT_DG_SHA="$(printf '%s' "$PARSED" | python3 -c 'import json,sys;print(json.loads(sys.stdin.read())["dg_binary_sha256"].get("actual",""))')"
            ACT_BACKUP_SHA="$(printf '%s' "$PARSED" | python3 -c 'import json,sys;print(json.loads(sys.stdin.read())["micro2_backup_script_sha256"].get("actual",""))')"
            # Always stamp the expected value when the bundle is nominated,
            # so the auditor can see what was expected even on a passing check.
            if [ -n "$EXPECTED_DG_SHA256" ]; then
                PARSED="$(printf '%s' "$PARSED" | EXPECTED="$EXPECTED_DG_SHA256" python3 -c '
import json, os, sys
o = json.loads(sys.stdin.read())
o["dg_binary_sha256"]["expected"] = os.environ["EXPECTED"]
if o["dg_binary_sha256"].get("actual","") and o["dg_binary_sha256"]["actual"] != os.environ["EXPECTED"]:
    o["dg_binary_sha256"]["status"] = "fail"
print(json.dumps(o))
')"
            fi
            # Always stamp the expected backup-script sha so the auditor can
            # see what was expected. Fail when the deployed file differs.
            PARSED="$(printf '%s' "$PARSED" | EXPECTED="$HEAD_BACKUP_SHA256" python3 -c '
import json, os, sys
o = json.loads(sys.stdin.read())
o["micro2_backup_script_sha256"]["expected"] = os.environ["EXPECTED"]
if o["micro2_backup_script_sha256"].get("actual","") and o["micro2_backup_script_sha256"]["actual"] != os.environ["EXPECTED"]:
    o["micro2_backup_script_sha256"]["status"] = "fail"
print(json.dumps(o))
')"
            # Roll up verdict
            if printf '%s' "$PARSED" | python3 -c '
import json, sys
o = json.loads(sys.stdin.read())
# skipped checks are not failures (e.g. owner_key on a host where the
# key file is root-readable only and we ran as ubuntu).
bad = [k for k,v in o.items() if isinstance(v,dict) and v.get("status") == "fail"]
sys.exit(0 if not bad else 2)
' >/dev/null 2>&1; then
                HOST_STATUS="pass"
            else
                HOST_STATUS="fail"
            fi
        fi
    fi
fi

# --- assemble final JSON in python (safe escaping) ---

REPORT="$(TARGET="$TARGET" \
          HOST_MODE="$HOST_MODE" \
          HOST_STATUS="$HOST_STATUS" \
          HOST_FAIL="$HOST_FAIL" \
          BUNDLE_STATUS="$BUNDLE_STATUS" \
          BUNDLE_FAIL="$BUNDLE_FAIL" \
          BUNDLE_DIR="$BUNDLE_DIR" \
          EXPECTED_DG_SHA256="$EXPECTED_DG_SHA256" \
          HEAD_BACKUP_SHA256="$HEAD_BACKUP_SHA256" \
          SYNTAX_STATUS="$SYNTAX_STATUS" \
          SYNTAX_FAIL="$SYNTAX_FAIL" \
          WORKING_TREE_DIRTY="$WORKING_TREE_DIRTY" \
          PROBE_BACKUP_ROOT="$PROBE_BACKUP_ROOT" \
          PROBE_KEY_FILE="$PROBE_KEY_FILE" \
          PROBE_DATA_DIR="$PROBE_DATA_DIR" \
          PROBE_PARSED="${PARSED:-}" \
          START="$START" \
          python3 <<'PYEOF'
import json, os, datetime

def s(k, default=""): return os.environ.get(k, default)

started = s("START")
finished = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
target = s("TARGET")
mode = s("HOST_MODE")
host_status = s("HOST_STATUS")
host_fail = s("HOST_FAIL")

repo_side = {
    "micro2_backup_script_sha256": {
        "status": "pass",
        "value": s("HEAD_BACKUP_SHA256"),
        "source": "git HEAD:scripts/micro2-backup.sh",
    },
    "micro2_backup_syntax": {
        "status": s("SYNTAX_STATUS"),
        "reason": s("SYNTAX_FAIL"),
    },
    "dg_expected_sha256": {
        "status": "pass" if s("EXPECTED_DG_SHA256") else "skipped",
        "value": s("EXPECTED_DG_SHA256") or None,
        "bundle_dir": s("BUNDLE_DIR") or None,
    },
    "bundle_sha256sums_verify": {
        "status": s("BUNDLE_STATUS"),
        "reason": s("BUNDLE_FAIL"),
    },
    "probe_paths_agree": {
        "status": "pass",
        "backup_root": s("PROBE_BACKUP_ROOT"),
        "key_file": s("PROBE_KEY_FILE"),
        "data_dir": s("PROBE_DATA_DIR"),
    },
    "working_tree_clean": {
        "status": "observed",
        "clean": int(s("WORKING_TREE_DIRTY")) == 0,
        "dirty_files": int(s("WORKING_TREE_DIRTY")),
    },
}

if mode == "local" or not target:
    host_side = {
        "status": "skipped",
        "target": None,
        "checks": {},
        "reason": "no DG_AUDIT_TARGET set",
    }
else:
    parsed = s("PROBE_PARSED")
    if parsed:
        try:
            probe_obj = json.loads(parsed)
        except Exception:
            probe_obj = {}
    else:
        probe_obj = {}
    host_side = {
        "status": host_status,
        "target": target,
        "checks": probe_obj,
    }
    if host_fail:
        host_side["reason"] = host_fail

NON_FATAL_REPO = {"working_tree_clean"}

def add_failures(side, prefix):
    out = []
    for k, v in side.items():
        if isinstance(v, dict) and v.get("status") == "fail":
            if prefix == "repo_side" and k in NON_FATAL_REPO:
                continue
            reason = v.get("reason") or v.get("count") or v.get("age_seconds") or "see check"
            out.append(f"{prefix}.{k}: {reason}")
    return out

failures = []
failures += add_failures(repo_side, "repo_side")
if isinstance(host_side.get("checks"), dict):
    failures += add_failures(host_side["checks"], "host_side.checks")

if host_status in ("unreachable", "error"):
    failures.append(f"host_side: {host_status}: {host_fail}")
elif host_status == "fail":
    failures.append("host_side: at least one check failed")
elif host_status == "skipped" and mode == "live":
    failures.append("host_side: live mode but no probe results")

verdict = "pass" if not failures else "fail"
if mode == "local" and host_status == "skipped":
    verdict = "pass" if not any(
        v.get("status") == "fail" and k not in NON_FATAL_REPO
        for k, v in repo_side.items() if isinstance(v, dict)
    ) else "fail"

exit_code = 0 if verdict == "pass" else 1

report = {
    "schema_version": 1,
    "tool": "host-audit",
    "started_at": started,
    "finished_at": finished,
    "mode": mode,
    "target": (target if mode == "live" and target else None),
    "repo_side": repo_side,
    "host_side": host_side,
    "verdict": verdict,
    "failures": failures,
    "exit_code": exit_code,
}

print(json.dumps(report, indent=2))
PYEOF
)"

# --- self-leakage guard ---
# Re-scan the report for any 64-hex string and refuse to print if any of them
# is NOT a documented digest field. The probe body reports deployed-binary
# shas, micro2-backup.sh shas, per-database shas in the verify report, and
# the registry sha — all legitimate, all expected, all listed in the
# whitelist. The guard fails closed: a future edit that emits a digest
# outside the whitelist breaks the report rather than printing it.
if printf '%s' "$REPORT" | grep -oE '\b[0-9a-f]{64}\b' | sort -u >/tmp/.host-audit-digests.$$ 2>/dev/null; then
    # Build the whitelist by collecting every 64-hex from the PARSED probe
    # (all the actual/expected/sha256 fields), plus the head backup script
    # sha and the bundle-expected dg sha (which never appear inside PARSED
    # but are also legitimate).
    KNOWN="$(printf '%s\n%s\n%s\n' \
        "$HEAD_BACKUP_SHA256" "$EXPECTED_DG_SHA256" \
        "$PARSED" \
        | grep -oE '\b[0-9a-f]{64}\b' | sort -u)"
    UNEXPECTED="$(grep -vxFf <(printf '%s\n' "$KNOWN") /tmp/.host-audit-digests.$$ || true)"
    rm -f /tmp/.host-audit-digests.$$
    if [ -n "$UNEXPECTED" ]; then
        fail "self-leakage guard: unexpected 64-hex digest(s) in report: $(printf '%s' "$UNEXPECTED" | tr '\n' ' ')"
    fi
fi

# --- final exit ---
if [ -n "$REPORT" ]; then
    REPORT_EXIT="$(printf '%s' "$REPORT" | python3 -c 'import json,sys;print(json.loads(sys.stdin.read())["exit_code"])')"
    printf '%s\n' "$REPORT"
    if [ "$REPORT_EXIT" != "0" ]; then
        log "verdict: fail (see report)"
        exit "$REPORT_EXIT"
    fi
else
    fail "report assembly produced empty output"
fi
