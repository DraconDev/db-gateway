#!/usr/bin/env bash
# A disposable, local, S3-compatible endpoint for rehearsing the packet.
#
# Run: eval "$(scripts/s3-rehearsal-endpoint.sh start)"   # prints exports
#      scripts/s3-rehearsal-endpoint.sh stop
#      scripts/s3-rehearsal-endpoint.sh status
#
# WHY THIS EXISTS. Every gate in §6 that needs a bucket is blocked on the owner
# supplying one, and an unrunnable packet is the worst possible state: the gates
# have never executed, so the first thing the owner's bucket would meet is a
# script nobody has run. This starts, on 127.0.0.1 and with its own throwaway
# key, an S3-compatible store with a validating, logging proxy in front of it,
# so all of §6 can be executed end to end before a real account is touched.
#
# WHAT IT IS NOT. It is not OVH. It proves the packet drives correct requests,
# handles the answers, restores the right database and cleans up after itself.
# It proves nothing about the OVH test region, class, pricing, lifecycle
# implementation or TLS. Every gate run against it says so in its own output,
# and the packet refuses to call the result an adoption packet.
#
# WHY THE PROXY IS IN FRONT. The store behind it does not authenticate, so a
# rehearsal would pass with a broken signature. The proxy recomputes SigV4 on
# every request and answers 403 when it disagrees, and it records the operation
# matrix gate 4 reports. One process, three jobs.
#
# STATE. Everything lives under a directory that `stop` deletes, so a rehearsal
# leaves nothing behind -- the same property gate 12 asserts about the owner's
# account.

set -euo pipefail

STATE_DIR="${DG_REHEARSAL_STATE:-$HOME/.local/dg-sandbox/s3-rehearsal}"
GOFAKES3_BIN="${DG_REHEARSAL_S3_BIN:-$HOME/.local/dg-sandbox/rehearsal-store/bin/gofakes3}"
PROXY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/s3-log-proxy.py"
BUCKET="${DG_REHEARSAL_BUCKET:-dg-sandbox-rehearsal}"
KEY_ID="${DG_REHEARSAL_KEY_ID:-S3[DRACON_SECRET:REDACTED]}"
SECRET="${DG_REHEARSAL_SECRET:-rehearsal-secret-not-a-real-credential}"

free_port() {
    python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()'
}

emit_exports() {
    cat <<EOF
export DG_SANDBOX_BUCKET='$BUCKET'
export DG_SANDBOX_ENDPOINT='127.0.0.1:$(cat "$STATE_DIR/proxy_port")'
export DG_SANDBOX_SCHEME='http'
export DG_SANDBOX_REGION='us-east-1'
export DG_SANDBOX_ACCESS_KEY_ID='$KEY_ID'
export DG_SANDBOX_SECRET_KEY='$SECRET'
export DG_SANDBOX_FAULT_FILE='$STATE_DIR/faults'
export DG_SANDBOX_REHEARSAL=1
EOF
}

start() {
    # Idempotent on purpose: the documented usage is
    # `eval "$(scripts/s3-rehearsal-endpoint.sh start)"` in the same shell as
    # the packet run, and a second call in that shell must re-emit the same
    # exports rather than fail, or the rehearsal is unusable after a typo.
    if [ -f "$STATE_DIR/pids" ] && kill -0 "$(head -1 "$STATE_DIR/pids")" 2>/dev/null; then
        emit_exports
        return 0
    fi
    if [ ! -x "$GOFAKES3_BIN" ]; then
        echo "gofakes3 is not installed at $GOFAKES3_BIN" >&2
        cat >&2 <<'EOF'

  The rehearsal endpoint needs an S3-compatible server. It was built with:

    GOBIN=$HOME/.local/dg-sandbox/rehearsal-store/bin \
      go install github.com/johannesboyne/gofakes3/gofakes3@v1.2.0

  Any S3-compatible server works; point DG_REHEARSAL_S3_BIN and the flags in
  this script at it. Without one, the S3 gates stay BLOCKED, which is a
  truthful packet and a much better one than a fabricated pass.
EOF
        exit 1
    fi

    rm -rf "$STATE_DIR"
    mkdir -p "$STATE_DIR/data" "$STATE_DIR/log"
    S3_PORT="$(free_port)"
    PROXY_PORT="$(free_port)"

    "$GOFAKES3_BIN" -host "127.0.0.1:$S3_PORT" -backend fs -fs.path "$STATE_DIR/data" \
        > "$STATE_DIR/log/store.log" 2>&1 &
    STORE_PID=$!
    echo "$STORE_PID" > "$STATE_DIR/pids"
    printf '%s\n' "$PROXY_PORT" > "$STATE_DIR/proxy_port"

    # Wait for the store to accept a connection rather than sleeping a fixed
    # amount: a fixed sleep is a race that shows up as a mystery gate failure
    # on a loaded machine.
    for _ in $(seq 1 100); do
        python3 -c "
import socket,sys
s=socket.socket()
s.settimeout(0.2)
sys.exit(0 if s.connect_ex(('127.0.0.1',$S3_PORT))==0 else 1)" 2>/dev/null && break
        sleep 0.1
    done

    python3 "$PROXY" --listen "127.0.0.1:$PROXY_PORT" --upstream "127.0.0.1:$S3_PORT" \
        --access-key-id "$KEY_ID" --secret-access-key "$SECRET" \
        --log "$STATE_DIR/log/requests.jsonl" \
        --faults "$STATE_DIR/faults" \
        > "$STATE_DIR/log/proxy.log" 2>&1 &
    PROXY_PID=$!
    echo "$PROXY_PID" >> "$STATE_DIR/pids"
    : > "$STATE_DIR/faults"

    for _ in $(seq 1 100); do
        python3 -c "
import socket,sys
s=socket.socket(); s.settimeout(0.2)
sys.exit(0 if s.connect_ex(('127.0.0.1',$PROXY_PORT))==0 else 1)" 2>/dev/null && break
        sleep 0.1
    done

    # The bucket is created through the proxy, so its existence is proved by the
    # same signed path litestream will use rather than by a side door.
    python3 "$(dirname "$PROXY")/s3probe.py" --endpoint "127.0.0.1:$PROXY_PORT" --scheme http \
        --bucket "$BUCKET" --access-key-id "$KEY_ID" --secret-access-key "$SECRET" \
        create-bucket >/dev/null 2>&1 || true

    cat <<EOF
DG_SANDBOX_BUCKET=$BUCKET
EOF
    emit_exports
}

stop() {
    if [ -f "$STATE_DIR/pids" ]; then
        while read -r pid; do
            kill "$pid" 2>/dev/null || true
        done < "$STATE_DIR/pids"
    fi
    sleep 0.3
    rm -rf "$STATE_DIR"
    echo "rehearsal endpoint stopped; $STATE_DIR removed" >&2
}

status() {
    if [ -f "$STATE_DIR/pids" ]; then
        echo "running (state $STATE_DIR, proxy port $(cat "$STATE_DIR/proxy_port" 2>/dev/null))"
    else
        echo "not running (state $STATE_DIR)"
    fi
}

case "${1:-start}" in
    start) start;;
    stop) stop;;
    status) status;;
    *) echo "usage: $0 {start|stop|status}" >&2; exit 2;;
esac
