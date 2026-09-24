#!/usr/bin/env bash
# Local validation for the non-installing deployment template.
#
# Usage: scripts/validate-deploy.sh
# Checks: every {{PLACEHOLDER}} in deploy/dg.service is documented in
# docs/deployment.md; every flag in ExecStart exists in `dg serve --help`;
# the listen default is loopback; systemd-analyze verify passes when
# available (placeholders substituted with dummy values in a temp copy).
# Read-only: never touches systemd, the network, or any target host.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UNIT="$ROOT/deploy/dg.service"
GUIDE="$ROOT/docs/deployment.md"
fail() { echo "[validate-deploy] FAIL: $*" >&2; exit 1; }
log() { echo "[validate-deploy] $*" >&2; }

[ -f "$UNIT" ] || fail "missing $UNIT"
[ -f "$GUIDE" ] || fail "missing $GUIDE"

log "checking placeholders are documented"
placeholders="$(grep -o '{{[A-Z_]*}}' "$UNIT" | sort -u)"
[ -n "$placeholders" ] || fail "no placeholders found; template must not hardcode target values"
while read -r placeholder; do
    grep -qF "\`$placeholder\`" "$GUIDE" \
        || fail "$placeholder is not documented in docs/deployment.md"
done <<<"$placeholders"

log "checking ExecStart flags against dg serve --help"
cargo build --locked --quiet --manifest-path "$ROOT/Cargo.toml" >&2
help="$("$ROOT/target/debug/dg" serve --help)"
exec_line="$(grep -E '^ExecStart=' "$UNIT")"
for flag in $(grep -o -e '--[a-z-]*' <<<"$exec_line" | sort -u); do
    grep -qF -e "$flag" -- <<<"$help" || fail "ExecStart flag $flag not in dg serve --help"
done

log "checking loopback listening"
grep -E '^ExecStart=.*--listen \{\{DG_LISTEN\}\}' "$UNIT" >/dev/null \
    || fail "ExecStart must take --listen from the {{DG_LISTEN}} placeholder"
grep -q '127.0.0.1:7432' "$GUIDE" || fail "guide must pin the loopback default"

log "checking credential hygiene"
grep -qE '^(Environment|EnvironmentFile)=' "$UNIT" && fail "unit must not carry secrets via Environment"
grep -q 'Password\|Secret\|Token' "$UNIT" && fail "unit must not name secrets"

log "checking resource-budget policy"
if grep -qE '^[[:space:]]*MemoryMax=' "$UNIT"; then
    fail "MemoryMax must remain unset while docs/resource-budget.md measurements are pending"
fi
grep -q 'MemoryMax=.*line stays commented until measured\|MemoryMax.*line stays commented until measured' "$GUIDE" \
    || fail "deployment guide must document the pending MemoryMax policy"

if command -v systemd-analyze >/dev/null 2>&1; then
    log "running systemd-analyze verify on substituted copy"
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/dg-unit-XXXXXX")"
    trap 'rm -rf "$tmp"' EXIT
    mkdir -p "$tmp/data"
    sed -e "s|{{DG_USER}}|$(id -un)|g" \
        -e "s|{{DG_BINARY}}|$ROOT/target/debug/dg|g" \
        -e "s|{{DG_DATA_DIR}}|$tmp/data|g" \
        -e 's|{{DG_LISTEN}}|127.0.0.1:7432|g' \
        "$UNIT" >"$tmp/dg.service"
    if systemd-analyze verify "$tmp/dg.service" 2>"$tmp/verify.log"; then
        log "systemd-analyze verify: clean"
    else
        cat "$tmp/verify.log" >&2
        fail "systemd-analyze verify reported errors"
    fi
else
    log "systemd-analyze not available; skipped"
fi

log "OK: template and guide agree"
