#!/usr/bin/env bash
# One-shot local setup: build dg, install it, build the web companion,
# verify the install, and optionally smoke-test `dg web` end to end.
#
# Usage: scripts/setup.sh [--prefix DIR] [--smoke] [--no-install] [--help]
#
#   --prefix DIR   install dg to DIR/dg (default: $HOME/.local/bin)
#   --smoke        run the disposable `dg web` smoke after installing
#   --no-install   build and verify only; do not copy the binary
#
# Never touches credentials, data directories, systemd, or any host: it only
# compiles, copies one binary, builds web/dist, and (with --smoke) exercises
# disposable temp dirs. Re-run any time; every step is idempotent.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/.cargo/target}"
PREFIX="$HOME/.local/bin"
SMOKE=0
INSTALL=1

log() { echo "[setup] $*" >&2; }
fail() { echo "[setup] FAIL: $*" >&2; exit 1; }
usage() {
    cat >&2 <<'EOF'
Usage: scripts/setup.sh [--prefix DIR] [--smoke] [--no-install] [--help]
Build dg (release), install it, build the web companion, verify, and
optionally smoke-test `dg web` against a disposable gateway.
EOF
}
usage_error() {
    usage
    echo "[setup] FAIL: $*" >&2
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)
            [ $# -ge 2 ] || usage_error "missing value for --prefix"
            PREFIX="$2"
            shift 2
            ;;
        --prefix=*)
            PREFIX="${1#--prefix=}"
            shift
            ;;
        --smoke) SMOKE=1; shift ;;
        --no-install) INSTALL=0; shift ;;
        --help) usage; exit 0 ;;
        *) usage_error "unknown argument: $1" ;;
    esac
done
[ -n "$PREFIX" ] || usage_error "--prefix must not be empty"

need() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found on PATH: $1"
}
need cargo
need node
need npm

log "building dg (release, locked)"
cargo build --release --locked --manifest-path "$ROOT/Cargo.toml" >&2
BINARY="$TARGET_DIR/release/dg"
[ -x "$BINARY" ] || fail "expected binary missing after build: $BINARY"

INSTALLED="$BINARY"
if [ "$INSTALL" -eq 1 ]; then
    mkdir -p "$PREFIX"
    cp "$BINARY" "$PREFIX/dg"
    chmod 755 "$PREFIX/dg"
    INSTALLED="$PREFIX/dg"
    log "installed $INSTALLED"
    case ":$PATH:" in
        *":$PREFIX:"*) ;;
        *) log "WARNING: $PREFIX is not on PATH; add it or invoke $INSTALLED directly" ;;
    esac
fi

log "building web companion"
npm ci --prefix "$ROOT/web" >&2
npm run build --prefix "$ROOT/web" >&2
[ -f "$ROOT/web/dist/server.js" ] || fail "companion build produced no web/dist/server.js"

log "verifying install"
"$INSTALLED" --version >&2 || fail "$INSTALLED --version failed"
"$INSTALLED" web --help 2>&1 | grep -q "loopback" || fail "$INSTALLED web --help looks wrong"
node --version >&2 || fail "node --version failed"

if [ "$SMOKE" -eq 1 ]; then
    log "smoking dg web end to end"
    node "$ROOT/scripts/dg-web-smoke.mjs" --dg "$INSTALLED"
fi

log "OK: dg=$INSTALLED companion=$ROOT/web/dist/server.js"
cat >&2 <<EOF
Next:
  dg init --data-dir state/dg            # once: prints {"owner_key": …} — save it to a 0600 file, shown once
  dg serve --listen 127.0.0.1:7432 --data-dir state/dg
  dg --key-file owner.key web --port 8787   # then open http://127.0.0.1:8787 in a browser
EOF
