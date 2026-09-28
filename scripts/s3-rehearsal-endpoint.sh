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

# An audit line OUTSIDE the state dir, because the state dir is what gets
# deleted. A rehearsal that silently lost its endpoint left nothing to read
# afterwards, and the cause turned out to be a second run of this script tearing
# down a state dir it did not own.
audit() { printf '%s %s pid=%s ppid=%s state=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1" "$$" "$PPID" "$STATE_DIR" >> "${STATE_DIR}.audit" 2>/dev/null || true; }

# stop is only allowed to tear down state this process started. Without the
# owner check, two concurrent rehearsals sharing the default state directory
# have the second one kill the first one's endpoint, and -- because the state
# dir is a fixed path -- the first one's stop then deletes the live one.
state_owner() { cat "$STATE_DIR/owner.pid" 2>/dev/null || true; }

free_port() {
    python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()'
}

emit_exports() {
    cat <<EOF
export DG_SANDBOX_BUCKET='$BUCKET'
export DG_SANDBOX_ENDPOINT='127.0.0.1:$(cat "$STATE_DIR/proxy_port")'
export DG_SANDBOX_SCHEME='https'
export DG_SANDBOX_REGION='us-east-1'
export DG_SANDBOX_ACCESS_KEY_ID='$KEY_ID'
export DG_SANDBOX_SECRET_KEY='$SECRET'
export DG_SANDBOX_FAULT_FILE='$STATE_DIR/faults'
export DG_SANDBOX_CA_BUNDLE='$STATE_DIR/tls/ca.pem'
export DG_SANDBOX_TLS_CERT='$STATE_DIR/tls/ca.pem'
export DG_SANDBOX_TLS_KEY='$STATE_DIR/tls/key.pem'
export DG_SANDBOX_REHEARSAL=1
EOF
}

# A self-signed certificate for 127.0.0.1. The rehearsal endpoint serves TLS
# because litestream always builds https:// URLs for an S3 endpoint: against a
# plain-http endpoint it writes nothing at all and every gate fails for a
# reason that has nothing to do with the gate. The certificate is handed to the
# packet as a CA bundle so the rehearsal needs no skip-verify, and the
# "minimal working replica URL" the packet reports stays exactly what OVH needs.
make_cert() {
    mkdir -p "$STATE_DIR/tls"
    if command -v openssl >/dev/null 2>&1; then
        openssl req -x509 -newkey rsa:2048 -nodes -days 2 \
            -keyout "$STATE_DIR/tls/key.pem" -out "$STATE_DIR/tls/ca.pem" \
            -subj "/CN=127.0.0.1" \
            -addext "subjectAltName=IP:127.0.0.1,DNS:localhost" >/dev/null 2>&1 && return 0
    fi
    python3 - "$STATE_DIR/tls" <<'PY' 2>/dev/null || return 1
import datetime as dt, ipaddress, sys
from pathlib import Path
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import NameOID

out = Path(sys.argv[1])
key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "127.0.0.1")])
now = dt.datetime.now(dt.timezone.utc)
cert = (x509.CertificateBuilder()
        .subject_name(name).issuer_name(name).public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - dt.timedelta(minutes=5))
        .not_valid_after(now + dt.timedelta(days=2))
        .add_extension(x509.SubjectAlternativeName([
            x509.IPAddress(ipaddress.ip_address("127.0.0.1")),
            x509.DNSName("localhost")]), critical=False)
        .sign(key, hashes.SHA256()))
(out / "key.pem").write_bytes(key.private_bytes(
    serialization.Encoding.PEM, serialization.PrivateFormat.TraditionalOpenSSL,
    serialization.NoEncryption()))
(out / "ca.pem").write_bytes(cert.public_bytes(serialization.Encoding.PEM))
PY
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
    echo "$$" > "$STATE_DIR/owner.pid"
    audit start
    if ! make_cert; then
        echo "could not create a TLS certificate for the rehearsal endpoint" >&2
        exit 1
    fi
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
        --tls-cert "$STATE_DIR/tls/ca.pem" --tls-key "$STATE_DIR/tls/key.pem" \
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
    # A dead endpoint must stop the rehearsal here, loudly. Waiting for a port
    # that nothing will ever answer produces a packet whose every gate fails
    # with "connection refused", which reads like an S3 problem and is not one.
    if ! kill -0 "$PROXY_PID" 2>/dev/null; then
        echo "the recording proxy died on startup; the S3 gates cannot run" >&2
        cat "$STATE_DIR/log/proxy.log" >&2 || true
        exit 1
    fi
    if ! kill -0 "$STORE_PID" 2>/dev/null; then
        echo "the object store died on startup; the S3 gates cannot run" >&2
        cat "$STATE_DIR/log/store.log" >&2 || true
        exit 1
    fi

    # The bucket is created through the proxy, so its existence is proved by the
    # same signed path litestream will use rather than by a side door.
    REHEARSAL_CA="$STATE_DIR/tls/ca.pem" \
    python3 "$(dirname "$PROXY")/s3probe.py" --endpoint "127.0.0.1:$PROXY_PORT" --scheme https \
        --ca-bundle "$STATE_DIR/tls/ca.pem" \
        --bucket "$BUCKET" --access-key-id "$KEY_ID" --secret-access-key "$SECRET" \
        create-bucket >/dev/null 2>&1 || true

    cat <<EOF
DG_SANDBOX_BUCKET=$BUCKET
EOF
    emit_exports
}

stop() {
    local owner
    owner="$(state_owner)"
    if [ -n "$owner" ] && [ "$owner" != "$$" ] && kill -0 "$owner" 2>/dev/null; then
        audit "stop REFUSED (owned by live pid $owner)"
        echo "refusing to stop: $STATE_DIR belongs to the live process $owner, not this one" >&2
        return 0
    fi
    audit stop
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
