#!/usr/bin/env bash
# One-shot local setup: build dg, install it, verify the install, and
# optionally smoke-test the gateway end to end.
#
# Usage: scripts/setup.sh [--prefix DIR] [--smoke] [--no-install] [--help]
#
#   --prefix DIR   install dg to DIR/dg (default: $HOME/.local/bin)
#   --smoke        run a disposable end-to-end gateway check after installing
#   --no-install   build and verify only; do not copy the binary
#
# Never touches credentials, data directories, systemd, or any host: it only
# compiles, copies one binary, and (with --smoke) exercises disposable temp
# dirs. Re-run any time; every step is idempotent.

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
Build dg (release), install it, verify the install, and optionally smoke-test
a disposable gateway end to end.
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
need python3

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

log "verifying install"
"$INSTALLED" --version >&2 || fail "$INSTALLED --version failed"
"$INSTALLED" tui --help 2>&1 | grep -q "terminal" || fail "$INSTALLED tui --help looks wrong"

# A disposable gateway, driven only through the installed binary: init
# an owner, serve on an ephemeral port, create a database, write to it,
# read it back, and read the dashboard. It proves the binary that was
# just installed works, and it touches nothing outside a temp dir.
smoke() {
    local dir pid server key
    dir="$(mktemp -d "${TMPDIR:-/tmp}/dg-smoke-XXXXXX")"
    "$INSTALLED" init --data-dir "$dir/data" > "$dir/init.json" \
        || { rm -rf "$dir"; fail "init failed"; }
    key="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["owner_key"])' "$dir/init.json")" \
        || { rm -rf "$dir"; fail "cannot read the owner key"; }
    printf '%s' "$key" > "$dir/owner.key"
    chmod 600 "$dir/owner.key"
    "$INSTALLED" serve --listen 127.0.0.1:0 --data-dir "$dir/data" > "$dir/serve.log" 2>&1 &
    pid=$!
    server=""
    for _ in $(seq 1 50); do
        server="$(python3 - "$dir/serve.log" <<'PY' 2>/dev/null || true
import json, sys
for line in open(sys.argv[1], errors="replace"):
    try:
        log = json.loads(line)
    except ValueError:
        continue
    address = log.get("fields", {}).get("address")
    if address:
        print("http://" + address)
        break
PY
)"
        [ -n "$server" ] && break
        sleep 0.1
    done
    if [ -z "$server" ]; then
        kill "$pid" 2>/dev/null || true
        rm -rf "$dir"
        fail "the gateway never logged its address"
    fi
    if ! "$INSTALLED" --server "$server" --key-file "$dir/owner.key" create smoke_db >/dev/null \
        || ! "$INSTALLED" --server "$server" --key-file "$dir/owner.key" query smoke_db "CREATE TABLE t (id INTEGER)" >/dev/null \
        || ! "$INSTALLED" --server "$server" --key-file "$dir/owner.key" query smoke_db "INSERT INTO t VALUES (1)" >/dev/null \
        || ! "$INSTALLED" --server "$server" --key-file "$dir/owner.key" query smoke_db "SELECT id FROM t" >/dev/null \
        || ! "$INSTALLED" --server "$server" --key-file "$dir/owner.key" status --json >/dev/null; then
        kill "$pid" 2>/dev/null || true
        rm -rf "$dir"
        fail "the smoke check failed against $server"
    fi
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    rm -rf "$dir"
    log "smoke OK: $server"
}

if [ "$SMOKE" -eq 1 ]; then
    log "smoking the gateway end to end"
    smoke
fi

log "OK: dg=$INSTALLED"
cat >&2 <<EOF
Next:
  # once: init prints {"owner_key": …}; extract the raw key to a 0600 file (shown once)
  dg init --data-dir state/dg | python3 -c "import json,sys; print(json.load(sys.stdin)['owner_key'])" > owner.key && chmod 600 owner.key
  # the terminal view: the service, its databases, their backups and its keys on one screen
  dg --key-file owner.key tui
EOF
