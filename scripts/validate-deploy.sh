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
RELEASE="$ROOT/scripts/release-bundle.sh"
fail() { echo "[validate-deploy] FAIL: $*" >&2; exit 1; }
log() { echo "[validate-deploy] $*" >&2; }

# The target's glibc ceiling, read from the one place that pins it so the gate
# and the release build can never disagree about what "the target" means.
# Sourced, not hardcoded here: release-bundle.sh owns the digest-fixed builder
# image whose glibc this is, and a second copy of the number in this file is a
# second thing to update when the target OS moves.
EXPECTED_GLIBC="$(sed -n "s/^EXPECTED_GLIBC=[\"']\([0-9][0-9.]*\)[\"'].*/\1/p" "$RELEASE" | head -1)"
[ -n "$EXPECTED_GLIBC" ] || fail "cannot read EXPECTED_GLIBC from $RELEASE"

# Dotted numeric comparison without depending on sort -V (GNU-only): 0 when
# $1 < $2. awk is POSIX and every target this script runs on has it.
ver_lt() {
    awk -v a="$1" -v b="$2" 'BEGIN {
        na = split(a, x, "."); nb = split(b, y, ".")
        n = (na > nb ? na : nb)
        for (i = 1; i <= n; i++) {
            xi = (i <= na ? x[i] : 0) + 0
            yi = (i <= nb ? y[i] : 0) + 0
            if (xi < yi) exit 0
            if (xi > yi) exit 1
        }
        exit 1
    }'
}

[ -f "$UNIT" ] || fail "missing $UNIT"
[ -f "$GUIDE" ] || fail "missing $GUIDE"
[ -f "$RELEASE" ] || fail "missing $RELEASE"

log "checking placeholders are documented"
# Every service template's placeholders must be documented, not just
# dg.service's: the backup units (deploy/dg-backup.service,
# dg-backup-digest.service) are installed from the same guide.
placeholders="$(grep -h -o '{{[A-Z_]*}}' "$UNIT" "$ROOT/deploy/dg-backup.service" "$ROOT/deploy/dg-backup-digest.service" "$ROOT/deploy/dg-restore-drill.service" "$ROOT/deploy/dg-restore-points.service" "$ROOT/deploy/dg-resource-sample.service" "$ROOT/deploy/dg-alerts.service" "$ROOT/deploy/dg-traffic-sample.service" "$ROOT/deploy/dg-mesh-proxy.socket" | sort -u)"
[ -n "$placeholders" ] || fail "no placeholders found; template must not hardcode target values"
while read -r placeholder; do
    grep -qF "\`$placeholder\`" "$GUIDE" \
        || fail "$placeholder is not documented in docs/deployment.md"
done <<<"$placeholders"

log "checking ExecStart flags against dg serve --help"
# Reuse the binary that is already built. This check only needs SOME dg to
# print `serve --help`; it is not the portable-artifact check below, which
# inspects the real bundle. Building here instead deadlocked: cargo holds the
# target-dir lock for the whole of `cargo test`, and cargo runs test binaries
# while it goes on to build other targets, so a nested `cargo build` waits on
# the lock its own parent holds. In a test that wraps this script in
# `timeout 60` the result was a 60s stall and a gate that "failed for the
# wrong reason" with the output cut off here. Build only when the binary is
# missing entirely (a fresh checkout running this script by hand).
DG_HELP_BIN="$TARGET_DIR/debug/dg"
if [ ! -x "$DG_HELP_BIN" ]; then
    cargo build --locked --quiet --manifest-path "$ROOT/Cargo.toml" >&2
fi
help="$("$DG_HELP_BIN" serve --help)"
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

log "checking the service unit can publish a backup set on demand"
# `dg backup run` publishes a set into the backup root from inside the
# gateway, and ProtectSystem=strict makes every path outside
# ReadWritePaths read-only to the process. A unit that leaves the backup
# root out fails every on-demand run with an internal error, which reads
# like a bug in the command rather than a missing path in the unit.
for expected in \
    '--backup-root {{DG_BACKUP_ROOT}}' \
    'ReadWritePaths={{DG_DATA_DIR}} {{DG_BACKUP_ROOT}}'; do
    grep -F -- "$expected" "$UNIT" >/dev/null \
        || fail "dg.service must carry $expected"
done

log "checking the backup unit drives the producer with the templated paths"
# The producer's compiled-in defaults are the reference layout. A host
# that renders this template with any other layout would otherwise
# publish into /var/lib/dg-backups while dg-backup-digest.service reads
# the configured root, so every digest reported the databases as unknown,
# and a non-default {{DG_LISTEN}} failed every backup with connection
# refused. The unit must pass the paths it was rendered with.
backup_unit="$ROOT/deploy/dg-backup.service"
for expected in \
    '--server http://{{DG_LISTEN}}' \
    '--key-file {{DG_OWNER_KEY_FILE}}' \
    '--data-dir {{DG_DATA_DIR}}' \
    '--backup-root {{DG_BACKUP_ROOT}}'; do
    grep -F -- "$expected" "$backup_unit" >/dev/null \
        || fail "dg-backup.service must pass $expected"
done

log "checking the two halves of the snapshot cadence agree"
# The check itself lives in scripts/check-unit-cadence.sh so it can be run
# on its own (and tested against a disposable tree) without the rest of
# this gate.
bash "$ROOT/scripts/check-unit-cadence.sh" "$ROOT" \
    || fail "the snapshot cadence disagrees between the gateway and the digest"

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

# Dynamic-section tags the parser understands, spelled as the glibc headers
# define them (elf.h lines ~900/914/988/990) so the numbers are auditable
# rather than magic: DT_RPATH is the deprecated 15, DT_RUNPATH the current 29,
# and DT_VERNEED/DT_VERNEEDNUM live way up in the OS-specific range because
# they are processed by the dynamic linker, not the assembler.
DT_RPATH = 15          # deprecated but still honoured by a binary built with -Wl,-rpath
DT_RUNPATH = 29        # what modern linkers emit
DT_VERNEED = 0x6FFFFFFE
DT_VERNEEDNUM = 0x6FFFFFFF

interp = ""
err = None
dyn = None
for i in range(e_phnum):
    o = e_phoff + i * e_phentsize
    if o + 56 > len(d):
        err = "program header %d is truncated" % i
        break
    p_type = struct.unpack_from(endian + "I", d, o)[0]
    p_offset = struct.unpack_from(endian + "Q", d, o + 8)[0]
    p_filesz = struct.unpack_from(endian + "Q", d, o + 32)[0]
    if p_type == 3 and not interp:  # PT_INTERP
        interp = d[p_offset:p_offset + p_filesz].split(b"\0")[0].decode()
    if p_type == 2:  # PT_DYNAMIC
        dyn = (p_offset, p_filesz)

runpath = ""
glibc_req = ""
be_64 = d[4] == 2
# 32-bit ELF would need 4-byte Elf32_Verneed/Vernaux (8 bytes each); this
# repository only ever ships x86_64, so refuse rather than mis-read a struct
# and report a wrong verdict with all the confidence of a right one.
if not be_64:
    err = err or "32-bit ELF: this parser supports x86_64 only"
if dyn and not err:
    off, size = dyn
    ents = []
    for i in range(0, size, 16):
        if off + i + 16 > len(d):
            break
        tag, val = struct.unpack_from(endian + "qQ", d, off + i)
        if tag == 0:
            break
        ents.append((tag, val))

    def v2o(v):
        # program-header vaddr -> file offset
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
            if tag in (DT_RPATH, DT_RUNPATH) and so is not None:
                runpath = cstr(so, val)

        # DT_VERNEED is the artifact's own declaration of the libc symbol
        # versions it needs. This is the only place the real requirement is
        # recorded: BUILD-INFO.txt in the bundle carries a self-declared
        # builder_glibc string that a hand-assembled bundle could lie about,
        # whereas this table is written by the linker and cannot be faked
        # without changing the binary the target actually runs.
        #
        # Each Elf64_Verneed entry (16 B) points at Elf64_Vernaux siblings
        # (16 B each) whose vna_name indexes .dynstr for strings like
        # "GLIBC_2.39". Take the maximum: that is the target glibc ceiling.
        vn = dict(ents)
        if DT_VERNEED in vn and so is not None:
            base = v2o(vn[DT_VERNEED])
            total = vn.get(DT_VERNEEDNUM, 0)

            def vkey(s):
                try:
                    return tuple(int(x) for x in s.split("_")[-1].split("."))
                except ValueError:
                    return ()

            if base is not None:
                o, seen = base, 0
                while o is not None and o + 16 <= len(d) and seen < total:
                    vn_cnt = struct.unpack_from(endian + "H", d, o + 2)[0]
                    vn_aux = struct.unpack_from(endian + "I", d, o + 8)[0]
                    vn_next = struct.unpack_from(endian + "I", d, o + 12)[0]
                    a = o + vn_aux
                    for _j in range(vn_cnt):
                        if a + 16 > len(d):
                            break
                        vna_name = struct.unpack_from(endian + "I", d, a + 8)[0]
                        vna_next = struct.unpack_from(endian + "I", d, a + 12)[0]
                        try:
                            s = cstr(so, vna_name)
                        except ValueError:
                            s = ""
                        if s.startswith("GLIBC_") and vkey(s) > vkey(glibc_req):
                            glibc_req = s
                        if vna_next == 0:
                            break
                        a += vna_next
                    seen += 1
                    if vn_next == 0:
                        break
                    o += vn_next

if err:
    print("ERR\t%s\t" % err)
    raise SystemExit(0)
# Print nothing for the glibc field rather than the sentinel so a static
# artifact is visibly "no glibc requirement" and the bash side can tell that
# apart from "has one and it is fine".
print(f"{interp}\t{runpath}\t{glibc_req}")
PY
)" || fail "portable-artifact check: could not read $bin"
    local interp runpath glibc_req
    interp="$(cut -f1 <<<"$facts")"
    runpath="$(cut -f2 <<<"$facts")"
    glibc_req="$(cut -f3 <<<"$facts")"
    # A parser that could not even read the artifact has no opinion worth
    # having; fail loudly rather than let the remaining clauses run against
    # empty fields and pass by default.
    [ "$interp" != "ERR" ] || fail "portable-artifact check: $runpath"
    # Drop the GLIBC_ prefix to get just the version number, e.g. 2.39.
    local glibc_ver=""
    case "$glibc_req" in
        GLIBC_*) glibc_ver="${glibc_req#GLIBC_}" ;;
    esac
    log "artifact: $bin"
    log "  interpreter: ${interp:-<none, statically linked>}"
    log "  runpath:     ${runpath:-<none>}"
    log "  max glibc:   ${glibc_ver:-<none, statically linked>}"

    # A Nix store path anywhere in the binary means it was built by Nix and
    # resolves its libc from the builder's store, which does not exist on an
    # Ubuntu target. Check the RUNPATH explicitly rather than grepping every
    # string, so an unrelated embedded path in a data blob cannot fail a
    # genuinely portable build. Both spellings are read: DT_RPATH (15) is
    # deprecated but still honoured, DT_RUNPATH (29) is what modern linkers
    # emit, and the outage was found in the RUNPATH one.
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
            || fail "$bin has interpreter '$interp'; the micro2 target is a
  standard Ubuntu amd64 layout, so the loader must be /lib64/ld-linux-x86-64.so.2.
  Build the real artifact with: DG_RELEASE_TARGET=micro2 scripts/release-bundle.sh"
    fi

    # The glibc ceiling, measured off the artifact rather than taken from
    # BUILD-INFO.txt: glibc 2.39 and 2.41 share the same loader path, so a
    # bundle built in a newer container passes every check above and still
    # dies on the older target with the same loader/libc mismatch as the Nix
    # build. The linker writes the versions the binary actually needs into
    # DT_VERNEED, so that cannot be mis-declared.
    if [ -n "$glibc_ver" ]; then
        if ver_lt "$EXPECTED_GLIBC" "$glibc_ver"; then
            fail "$bin requires glibc $glibc_ver ($glibc_req), but the target provides $EXPECTED_GLIBC.
  That is the same failure mode as the Nix build -- a loader/libc mismatch -- reached with
  an ordinary loader path, so it is invisible to every other check in this gate.
  Build against the target's glibc with: DG_RELEASE_TARGET=micro2 scripts/release-bundle.sh"
        fi
        log "  glibc ceiling: $glibc_ver <= target $EXPECTED_GLIBC (ok)"
    else
        log "  glibc ceiling: none required (statically linked)"
    fi
    log "  portable-artifact check: clean"
}

BUNDLE_DIR="${DG_RELEASE_BUNDLE_DIR:-}"
if [ -n "$BUNDLE_DIR" ]; then
    log "checking the nominated release bundle is deployable"
    # `-x`, not `-f`: a staged binary can be present and still not be
    # executable, because `cp` applies the builder's umask to the new file.
    # A 644 dg inside a checksum-valid bundle passes every other check here
    # (SHA256SUMS verifies and check_portable is content-only) and then
    # fails to exec on the target. release-bundle.sh chmods its own copy
    # to 755; this is the independent second line that catches a bundle
    # staged by any other route. When no bundle is nominated the local
    # build path below is checked by the same rule.
    [ -x "$BUNDLE_DIR/dg" ] || fail "DG_RELEASE_BUNDLE_DIR=$BUNDLE_DIR has no executable dg binary"
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

log "checking the live documents state the gateway's cap consistently"
# The check itself lives in scripts/check-doc-caps.py so it can be run
# on its own (and tested) without the rest of this gate.
python3 "$ROOT/scripts/check-doc-caps.py" "$ROOT" \
    || fail "a live document states a memory cap the gateway unit does not set"

log "checking the documentation index registers every file under docs/"
# The index opens with "Every file is one of: Canonical, Historical
# evidence, Superseded". That promise was kept once by hand and drifted
# again, so the check that holds it lives in scripts/check-doc-index.py.
python3 "$ROOT/scripts/check-doc-index.py" "$ROOT" \
    || fail "a file under docs/ is not registered in docs/README.md"

if command -v systemd-analyze >/dev/null 2>&1; then
    log "running systemd-analyze verify on substituted copy"
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/dg-unit-XXXXXX")"
    trap 'rm -rf "$tmp"' EXIT
    mkdir -p "$tmp/data" "$tmp/backups"
    # Every unit is rendered through scripts/unit-render.sh, which
    # refuses to emit a unit that still carries a placeholder. A
    # template that gains one therefore fails this gate loudly instead
    # of being verified with the literal "{{...}}" text still in it.
    "$ROOT/scripts/unit-render.sh" "$UNIT" \
        "DG_USER=$(id -un)" \
        "DG_BINARY=$TARGET_DIR/debug/dg" \
        "DG_DATA_DIR=$tmp/data" \
        "DG_BACKUP_ROOT=$tmp/backups" \
        "DG_LISTEN=127.0.0.1:7432" >"$tmp/dg.service"
    # The litestream-sync units name the host path /usr/local/bin/
    # litestream-sync.sh; point the verification copy at the repo script
    # so verify checks the unit, not this machine's /usr/local/bin.
    sed -e "s|/usr/local/bin/litestream-sync.sh|$ROOT/scripts/litestream-sync.sh|g" \
        "$ROOT/deploy/dg-litestream-sync.service" >"$tmp/dg-litestream-sync.service"
    cp "$ROOT/deploy/dg-litestream-sync.timer" "$tmp/dg-litestream-sync.timer"
    # The backup units are placeholder templates like dg.service;
    # substitute the same dummy values so verify checks unit structure.
    for u in dg-backup.service dg-backup-digest.service; do
        "$ROOT/scripts/unit-render.sh" "$ROOT/deploy/$u" \
            "DG_USER=$(id -un)" \
            "DG_BINARY=$TARGET_DIR/debug/dg" \
            "DG_DATA_DIR=$tmp/data" \
            "DG_BACKUP_ROOT=$tmp/data" \
            "DG_BACKUP_SCRIPT=$ROOT/scripts/micro2-backup.sh" \
            "DG_LISTEN=127.0.0.1:7432" \
            "DG_OWNER_KEY_FILE=$tmp/owner.key" >"$tmp/$u"
    done
    cp "$ROOT/deploy/dg-backup.timer" "$ROOT/deploy/dg-backup-digest.timer" "$tmp/"
    "$ROOT/scripts/unit-render.sh" "$ROOT/deploy/dg-restore-drill.service" \
        "DG_USER=$(id -un)" \
        "DG_BINARY=$TARGET_DIR/debug/dg" \
        "DG_DATA_DIR=$tmp/data" \
        "DG_RESTORE_DRILL_SCRIPT=$ROOT/scripts/bucket-restore-drill.sh" \
        "DG_OWNER_KEY_FILE=$tmp/owner.key" >"$tmp/dg-restore-drill.service"
    cp "$ROOT/deploy/dg-restore-drill.timer" "$tmp/"
    "$ROOT/scripts/unit-render.sh" "$ROOT/deploy/dg-restore-points.service" \
        "DG_USER=$(id -un)" \
        "DG_DATA_DIR=$tmp/data" \
        "DG_BACKUP_ROOT=$tmp/data" \
        "DG_RESTORE_ROOT=$tmp/restores" \
        "DG_RESTORE_POINT_SCRIPT=$ROOT/scripts/restore-point.sh" >"$tmp/dg-restore-points.service"
    cp "$ROOT/deploy/dg-restore-points.timer" "$tmp/"
    "$ROOT/scripts/unit-render.sh" "$ROOT/deploy/dg-resource-sample.service" \
        "DG_USER=$(id -un)" \
        "DG_DATA_DIR=$tmp/data" \
        "DG_BACKUP_ROOT=$tmp/data" \
        "DG_RESOURCE_SAMPLE_SCRIPT=$ROOT/scripts/resource-sample.sh" >"$tmp/dg-resource-sample.service"
    cp "$ROOT/deploy/dg-resource-sample.timer" "$tmp/"
    "$ROOT/scripts/unit-render.sh" "$ROOT/deploy/dg-alerts.service" \
        "DG_USER=$(id -un)" \
        "DG_DATA_DIR=$tmp/data" \
        "DG_ALERTS_SCRIPT=$ROOT/scripts/dg-alerts.sh" >"$tmp/dg-alerts.service"
    cp "$ROOT/deploy/dg-alerts.timer" "$tmp/"
    "$ROOT/scripts/unit-render.sh" "$ROOT/deploy/dg-traffic-sample.service" \
        "DG_USER=$(id -un)" \
        "DG_LISTEN=127.0.0.1:9" \
        "DG_OWNER_KEY_FILE=$tmp/owner.key" \
        "DG_BINARY=$TARGET_DIR/debug/dg" \
        "DG_DATA_DIR=$tmp/data" \
        "DG_TRAFFIC_SAMPLE_SCRIPT=$ROOT/scripts/traffic-sample.sh" >"$tmp/dg-traffic-sample.service"
    cp "$ROOT/deploy/dg-traffic-sample.timer" "$tmp/"
    "$ROOT/scripts/unit-render.sh" "$ROOT/deploy/dg-mesh-proxy.socket" \
        "DG_MESH_LISTEN=127.0.0.2:17432" >"$tmp/dg-mesh-proxy.socket"
    # systemd-socket-proxyd is an Ubuntu-systemd path; substitute a
    # stand-in so verify checks unit structure on this host too.
    "$ROOT/scripts/unit-render.sh" "$ROOT/deploy/dg-mesh-proxy.service" \
        "DG_LISTEN=127.0.0.1:7432" \
        | sed -e "s|/usr/lib/systemd/systemd-socket-proxyd|$(type -P true || echo /bin/true)|g" \
        >"$tmp/dg-mesh-proxy.service"
    cp "$ROOT/deploy/dg-app-proxy.socket" "$tmp/"
    "$ROOT/scripts/unit-render.sh" "$ROOT/deploy/dg-app-proxy.service" \
        "DG_MESH_LISTEN=127.0.0.2:17432" \
        | sed -e "s|/usr/lib/systemd/systemd-socket-proxyd|$(type -P true || echo /bin/true)|g" \
        >"$tmp/dg-app-proxy.service"
    if systemd-analyze verify "$tmp/dg.service" "$tmp/dg-litestream-sync.service" "$tmp/dg-litestream-sync.timer" "$tmp/dg-backup.service" "$tmp/dg-backup.timer" "$tmp/dg-backup-digest.service" "$tmp/dg-backup-digest.timer" "$tmp/dg-restore-drill.service" "$tmp/dg-restore-drill.timer" "$tmp/dg-resource-sample.service" "$tmp/dg-resource-sample.timer" "$tmp/dg-alerts.service" "$tmp/dg-alerts.timer" "$tmp/dg-traffic-sample.service" "$tmp/dg-traffic-sample.timer" "$tmp/dg-mesh-proxy.socket" "$tmp/dg-mesh-proxy.service" "$tmp/dg-app-proxy.socket" "$tmp/dg-app-proxy.service" 2>"$tmp/verify.log"; then
        log "systemd-analyze verify: clean"
    else
        cat "$tmp/verify.log" >&2
        fail "systemd-analyze verify reported errors"
    fi
else
    log "systemd-analyze not available; skipped"
fi

log "OK: template and guide agree"
