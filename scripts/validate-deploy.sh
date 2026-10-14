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

# --------------------------------------------------------------------------
# Portable-artifact gate.
#
# The recorded outage this gate exists to stop: a NixOS-built binary was
# deployed to the Ubuntu micro2 target instead of the container-built release
# bundle. Its RUNPATH carried /nix/store/<glibc-2.40>/lib, so on micro2 Ubuntu's
# ld.so loaded Nix's glibc 2.40 -- a mismatched loader/libc pair -- and every
# CLI invocation aborted with "*** stack smashing detected ***". Nothing in the
# release path noticed, because scripts/release-bundle.sh was bypassed: that
# script builds in the pinned Ubuntu 24.04 image (glibc 2.39, matching the
# target) and is the only artifact docs/deployment.md authorises installing.
#
# The consequence was total: dg-backup.service failed 289 times between
# 2026-10-02 and the fix, and no backup had published since 2026-09-22.
#
# So the gate checks the artifact that would actually be installed. Nominate
# the container-built bundle with DG_RELEASE_BUNDLE_DIR. Without a nomination
# it falls back to the local build, and FAILS if that build is not deployable
# -- which is exactly the trap. A red result here is not a nuisance: on a
# NixOS workstation the local build genuinely cannot be deployed, and the
# message names the one command that produces an artifact that can.
# --------------------------------------------------------------------------
check_portable() {
    local bin="$1"
    [ -f "$bin" ] || fail "portable-artifact check: no such binary: $bin"
    local facts
    facts="$(python3 - "$bin" <<'PY'
import struct, sys

d = open(sys.argv[1], "rb").read()
if d[:4] != b"\x7fELF":
    print("NOT_ELF")
    raise SystemExit
is64 = d[4] == 2
endian = "<" if d[5] == 1 else ">"
e_phoff = struct.unpack_from(endian + "Q", d, 0x20)[0]
e_phentsize = struct.unpack_from(endian + "H", d, 0x36)[0]
e_phnum = struct.unpack_from(endian + "H", d, 0x38)[0]

interp = ""
dyn = None
for i in range(e_phnum):
    o = e_phoff + i * e_phentsize
    p_type = struct.unpack_from(endian + "I", d, o)[0]
    p_offset = struct.unpack_from(endian + "Q", d, o + 8)[0]
    p_filesz = struct.unpack_from(endian + "Q", d, o + 32)[0]
    if p_type == 3 and not interp:  # PT_INTERP
        interp = d[p_offset:p_offset + p_filesz].split(b"\0")[0].decode()
    if p_type == 2:  # PT_DYNAMIC
        dyn = (p_offset, p_filesz)

runpath = ""
if dyn:
    off, size = dyn
    ents = []
    for i in range(0, size, 16):
        tag, val = struct.unpack_from(endian + "qQ", d, off + i)
        if tag == 0:
            break
        ents.append((tag, val))

    def v2o(v):
        for i in range(e_phnum):
            o = e_phoff + i * e_phentsize
            pt = struct.unpack_from(endian + "I", d, o)[0]
            pv = struct.unpack_from(endian + "Q", d, o + 16)[0]
            pf = struct.unpack_from(endian + "Q", d, o + 32)[0]
            po = struct.unpack_from(endian + "Q", d, o + 8)[0]
            if pt == 1 and pv <= v < pv + pf:
                return po + (v - pv)
        return None

    st = [v for t, v in ents if t == 5]
    if st:
        so = v2o(st[0])

        def cstr(base, idx):
            e = d.index(b"\0", base + idx)
            return d[base + idx:e].decode()

        for tag, val in ents:
            if tag in (15, 29) and so is not None:
                runpath = cstr(so, val)
print(f"{interp}\t{runpath}")
PY
)" || fail "portable-artifact check: could not read $bin"
    local interp runpath
    interp="$(cut -f1 <<<"$facts")"
    runpath="$(cut -f2 <<<"$facts")"
    log "artifact: $bin"
    log "  interpreter: ${interp:-<none, statically linked>}"
    log "  runpath:     ${runpath:-<none>}"

    # A Nix store path anywhere in the binary means it was built by Nix and
    # resolves its libc from the builder's store, which does not exist on an
    # Ubuntu target. Check the RUNPATH explicitly rather than grepping every
    # string, so an unrelated embedded path in a data blob cannot fail a
    # genuinely portable build.
    case "$runpath" in
        *"/nix/store/"*)
            fail "$bin is not deployable: its RUNPATH resolves libraries from /nix/store, which does not exist on an Ubuntu target.
  This is the exact defect that took the micro2 hourly backup down for three weeks.
  Build the real artifact with: DG_RELEASE_TARGET=micro2 scripts/release-bundle.sh
  then re-run: DG_RELEASE_BUNDLE_DIR=<bundle-dir> scripts/validate-deploy.sh" ;;
    esac

    # The guide records the target interpreter. A bundle built against a
    # different one will not start, so this is checked for whatever artifact
    # is nominated -- including the container bundle, which is where the
    # container build itself would be caught if its image were ever changed.
    if [ -n "$interp" ]; then
        [ "$interp" = "/lib64/ld-linux-x86-64.so.2" ] \
            || fail "$bin has interpreter '$interp'; docs/deployment.md records /lib64/ld-linux-x86-64.so.2 for the micro2 target"
    fi
    log "  portable-artifact check: clean"
}

BUNDLE_DIR="${DG_RELEASE_BUNDLE_DIR:-}"
if [ -n "$BUNDLE_DIR" ]; then
    log "checking the nominated release bundle is deployable"
    [ -f "$BUNDLE_DIR/dg" ] || fail "DG_RELEASE_BUNDLE_DIR=$BUNDLE_DIR has no dg binary"
    [ -f "$BUNDLE_DIR/SHA256SUMS" ] || fail "$BUNDLE_DIR has no SHA256SUMS"
    ( cd "$BUNDLE_DIR" && sha256sum -c SHA256SUMS >/dev/null ) \
        || fail "bundle SHA256SUMS does not verify; do not deploy it"
    check_portable "$BUNDLE_DIR/dg"
else
    log "checking the local build is deployable (no DG_RELEASE_BUNDLE_DIR nominated)"
    check_portable "$TARGET_DIR/debug/dg"
fi

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
