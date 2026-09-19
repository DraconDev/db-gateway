#!/usr/bin/env bash
# Release bundle for the supervised micro2 target (or an explicitly selected host build).
#
# Usage: scripts/release-bundle.sh [OUTPUT_DIR]
# Default OUTPUT_DIR is a disposable mktemp directory; its path is printed to
# stdout (the only stdout output). Everything else goes to stderr.
#
# By default the build is verified for micro2's x86_64 Linux ABI, including
# the interpreter path required on the target. Set DG_RELEASE_TARGET=host only
# when intentionally producing a non-micro2 local bundle.
#
# Stages: the release dg executable (built with the lockfile), README, LICENSE,
# the API/operator docs, and a MANIFEST with SHA-256 checksums. No publishing,
# no dependency upgrades.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$(mktemp -d "${TMPDIR:-/tmp}/dg-bundle-XXXXXX")}" 
mkdir -p "$OUT"

log() { echo "[release-bundle] $*" >&2; }
fail() { echo "[release-bundle] FAIL: $*" >&2; exit 1; }

RELEASE_TARGET="${DG_RELEASE_TARGET:-micro2}"
EXPECTED_INTERPRETER=""
case "$RELEASE_TARGET" in
    micro2)
        [ "$(uname -s)" = "Linux" ] || fail "micro2 bundles must be built on Linux or with a cross target"
        [ "$(uname -m)" = "x86_64" ] || fail "micro2 bundle requires an x86_64 build host"
        EXPECTED_INTERPRETER="/lib64/ld-linux-x86-64.so.2"
        # The local Nix toolchain otherwise embeds a Nix-store interpreter
        # that does not exist on Ubuntu micro2. Keep any caller flags and add
        # the target interpreter explicitly; verification below is authoritative.
        export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C link-args=-Wl,-dynamic-linker,$EXPECTED_INTERPRETER"
        ;;
    host)
        log "building an explicitly requested current-host bundle"
        ;;
    *)
        fail "DG_RELEASE_TARGET must be micro2 or host"
        ;;
esac

log "building release (locked, target=$RELEASE_TARGET)"
cargo build --release --locked --quiet --manifest-path "$ROOT/Cargo.toml" >&2

BINARY="$ROOT/target/release/dg"
[ -x "$BINARY" ] || fail "release artifact is missing or not executable: $BINARY"
if [ "$RELEASE_TARGET" = "micro2" ]; then
    command -v readelf >/dev/null 2>&1 || fail "readelf is required to verify the micro2 artifact"
    readelf -hW "$BINARY" | grep -Eq 'Machine:[[:space:]]+Advanced Micro Devices X86-64' \
        || fail "release artifact is not x86_64"
    readelf -lW "$BINARY" | grep -Fq "[Requesting program interpreter: $EXPECTED_INTERPRETER]" \
        || fail "release artifact has the wrong ELF interpreter"
fi
"$BINARY" --version >/dev/null || fail "release artifact cannot execute on the build host"

ARCH="$(uname -m)"
VERSION="$("$BINARY" --version | awk '{print $2}')"

log "staging bundle in $OUT"
cp "$ROOT/target/release/dg" "$OUT/dg"
cp "$ROOT/README.md" "$ROOT/LICENSE" "$OUT/"
mkdir -p "$OUT/docs"
cp "$ROOT/docs/api.md" "$ROOT/docs/architecture.md" "$OUT/docs/"

cat >"$OUT/BUILD-INFO.txt" <<EOF
name: dg
version: $VERSION
arch: $ARCH
profile: release
locked: true
built_on: $(uname -srm)
built_at: $(date -u +%Y-%m-%dT%H:%M:%SZ)
target: $RELEASE_TARGET
interpreter: ${EXPECTED_INTERPRETER:-host-default}
verified_executable: true
note: local bundle for the current host architecture only; not published, not cross-platform.
EOF

log "generating checksums"
(cd "$OUT" && sha256sum dg README.md LICENSE docs/api.md docs/architecture.md BUILD-INFO.txt >SHA256SUMS)

printf '%s\n' "$OUT"
