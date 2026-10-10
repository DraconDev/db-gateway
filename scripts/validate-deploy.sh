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
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/.cargo/target}"
UNIT="$ROOT/deploy/dg.service"
GUIDE="$ROOT/docs/deployment.md"
BUDGET="$ROOT/docs/resource-budget.md"
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
help="$("$TARGET_DIR/debug/dg" serve --help)"
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

# The cap is a measured decision (docs/resource-budget.md, set 2026-10-10), so
# the gate checks the unit against the number the budget records rather than
# demanding it stay unset. A hard "must be unset" rule here is what made this
# gate red the moment the owner settled on a value: the check had no way to
# learn that the policy moved on. The gate now fails when the two disagree,
# which catches both a unit edited without updating the budget and a budget
# updated without editing the unit -- and a deliberately unset unit is still
# allowed, in which case the guide must say the policy is pending.
log "checking resource-budget policy"
UNIT_MAX="$(sed -n 's/^[[:space:]]*MemoryMax=\([^[:space:]#]*\).*/\1/p' "$UNIT" | head -1)"
# The budget page's own statement of the shipped cap, in `MemoryMax=<value>`
# form, so the two files cannot drift apart silently. Scoped to the line that
# names the unit, because the page also records `MemoryMax=1G` twice for the
# litestream sidecar's enforcement scope and that is not the gateway's cap.
DOC_MAX="$(grep 'deploy/dg.service' "$BUDGET" | sed -n 's/.*`MemoryMax=\([^`[:space:]]*\)`.*/\1/p' | head -1)"
if [ -n "$UNIT_MAX" ]; then
    [ -n "$DOC_MAX" ] \
        || fail "deploy/dg.service sets MemoryMax=$UNIT_MAX but docs/resource-budget.md records no cap"
    [ "$UNIT_MAX" = "$DOC_MAX" ] \
        || fail "MemoryMax disagreement: deploy/dg.service has $UNIT_MAX, docs/resource-budget.md records $DOC_MAX"
else
    # An unset unit is legitimate only while the budget page records no cap
    # either. The check is on the recorded VALUE, not on whether the words
    # "unset" or "pending" appear: this page carries both words in the
    # 2026-09-27 owner-decision blockquote that the 2026-10-10 decision
    # superseded, so a keyword grep would pass a page that still claims a cap.
    [ -z "$DOC_MAX" ] \
        || fail "deploy/dg.service leaves MemoryMax unset but docs/resource-budget.md records $DOC_MAX"
    grep -q 'MemoryMax' "$GUIDE" \
        || fail "deployment guide must document the MemoryMax policy"
fi

if command -v systemd-analyze >/dev/null 2>&1; then
    log "running systemd-analyze verify on substituted copy"
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/dg-unit-XXXXXX")"
    trap 'rm -rf "$tmp"' EXIT
    mkdir -p "$tmp/data"
    sed -e "s|{{DG_USER}}|$(id -un)|g" \
        -e "s|{{DG_BINARY}}|$TARGET_DIR/debug/dg|g" \
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
