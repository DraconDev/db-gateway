#!/usr/bin/env bash
# One-shot local setup: build dg, install it, verify the install, and
# optionally smoke-test the gateway end to end.
#
# Usage: scripts/setup.sh [--prefix DIR] [--smoke] [--no-install] [--strict] [--help]
#
#   --prefix DIR   install dg to DIR/dg (default: $HOME/.local/bin)
#   --smoke        run a disposable end-to-end gateway check after installing
#   --no-install   build and verify only; do not copy the binary
#   --strict       fail instead of warn when an installed dg is older than the
#                  source tree (useful in CI and in a release checklist)
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
STRICT=0

log() { echo "[setup] $*" >&2; }
fail() { echo "[setup] FAIL: $*" >&2; exit 1; }
usage() {
    cat >&2 <<'EOF'
Usage: scripts/setup.sh [--prefix DIR] [--smoke] [--no-install] [--strict] [--help]
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
        --strict) STRICT=1; shift ;;
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

check_stale() {
    local installed="$1"
    local newest
    newest="$(find "$ROOT/src" "$ROOT/crates" -name '*.rs' -newer "$installed" -print -quit 2>/dev/null || true)"
    if [ -z "$newest" ]; then
        log "install is current: no source file is newer than $installed"
        return 0
    fi
    local relative="${newest#"$ROOT"/}"
    if [ "$STRICT" = 1 ]; then
        fail "$installed is STALE: $relative is newer than the installed binary. Re-run without --no-install to reinstall."
    fi
    log "WARNING: $installed is STALE -- $relative is newer than the installed binary."
    log "WARNING: it may not behave like this source tree. Re-run without --no-install to reinstall."
}

# What is already installed, before this run touches it. The staleness
# question is about THAT binary -- "does the dg on my PATH match this tree?" --
# so it has to be asked before the copy replaces it, not after. Asking
# afterwards compares a just-built binary against a just-built tree and always
# answers "current", which is exactly why the old check never caught anything.
EXISTING="$PREFIX/dg"
if [ -x "$EXISTING" ]; then
    check_stale "$EXISTING"
fi

INSTALLED="$BINARY"
if [ "$INSTALL" -eq 1 ]; then
    mkdir -p "$PREFIX"
    # A running `dg` holds the installed binary open, and the copy
    # fails with a bare "Text file busy" that says nothing about what
    # to do. Say it instead.
    if ! cp "$BINARY" "$PREFIX/dg"; then
        fail "could not install $PREFIX/dg — a dg may be running (a gateway, or a view); stop it and install again"
    fi
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

# Is the installed binary older than the source it should have come from?
#
# The check this replaces was `tui --help | grep -q terminal`. That passes
# against ANY build whose help mentions the word "terminal" -- including the
# one from before the view was redesigned -- so it reported a stale install as
# fine while the operator kept seeing the old screen. A check that cannot fail
# for the reason it exists is worse than no check, because it is believed.
#
# This compares the installed binary's mtime with the newest source file. It
# is deliberately conservative: a checkout whose files have newer mtimes than
# the install (a fresh clone, a `touch`, a rebase) reports STALE, and that is
# the honest answer for "the binary on disk does not provably match this
# tree". Reinstall, or pass --strict, and the question goes away.

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
  # the terminal view: the service, its databases, their backups and its keys, each on its own tab
  dg --key-file owner.key tui
EOF
