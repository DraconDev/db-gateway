#!/usr/bin/env bash
# Local release bundle for the current host architecture only.
#
# Usage: scripts/release-bundle.sh [OUTPUT_DIR]
# Default OUTPUT_DIR is a disposable mktemp directory; its path is printed to
# stdout (the only stdout output). Everything else goes to stderr.
#
# Stages: the release dg executable (built with the lockfile), README, LICENSE,
# the API/operator docs, and a MANIFEST with SHA-256 checksums. No publishing,
# no cross-platform claims, no dependency upgrades.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$(mktemp -d "${TMPDIR:-/tmp}/dg-bundle-XXXXXX")}"
mkdir -p "$OUT"

log() { echo "[release-bundle] $*" >&2; }

log "building release (locked, current host only)"
cargo build --release --locked --quiet --manifest-path "$ROOT/Cargo.toml" >&2

ARCH="$(uname -m)"
VERSION="$("$ROOT/target/release/dg" --version | awk '{print $2}')"

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
note: local bundle for the current host architecture only; not published, not cross-platform.
EOF

log "generating checksums"
(cd "$OUT" && sha256sum dg README.md LICENSE docs/api.md docs/architecture.md BUILD-INFO.txt >SHA256SUMS)

printf '%s\n' "$OUT"
