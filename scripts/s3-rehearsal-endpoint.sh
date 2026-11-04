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
# WHAT IT IS NOT. It is not object storage provider. It proves the packet drives correct requests,
# handles the answers, restores the right database and cleans up after itself.
# It proves nothing about the object storage provider test region, class, pricing, lifecycle
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
# WHICH STORE BACKEND, AND WHY IT IS NOT A TUNING KNOB. Measured 2026-09-29 on
# gofakes3 v1.2.0:
#   -backend fs      stores keys as files, so a key with a '/' leaves an empty
#                    DIRECTORY behind when the object is deleted. DeleteBucket
#                    then answers 409 BucketNotEmpty while S3's own LIST shows
#                    zero objects. That is not S3's emptiness rule -- AWS and
#                    object storage provider both decide it from the object listing -- so the fs
#                    backend makes the stand-in WRONG in precisely the
#                    dimension §6 gate 12 measures, and gate 12 duly reported
#                    "residue remains" for a bucket that held none. A
#                    rehearsal endpoint that cannot tell a real residue from a
#                    store artifact is worse than no endpoint.
#   -backend memory  has no directories to confuse the answer, and it also
#                    implements ?versions, so the residue check can exercise
#                    BOTH enumerations (versions AND list-type=2) instead of
#                    proving only the one the fs backend happens to support.
# The default is therefore memory, and the choice is disclosed to the packet
# through DG_SANDBOX_ENDPOINT_IMPL so no reader can mistake the store's
# behaviour for the object storage provider service's.
S3_BACKEND="${DG_REHEARSAL_S3_BACKEND:-memory}"
STORE_FS_ARGS=()
STORE_DESC="gofakes3 (-backend $S3_BACKEND)"
if [ "$S3_BACKEND" = "fs" ] || [ "$S3_BACKEND" = "directfs" ]; then
    STORE_FS_ARGS=(-fs.path "$STATE_DIR/data")
    if [ "$S3_BACKEND" = "directfs" ]; then
        STORE_FS_ARGS=(-directfs.path "$STATE_DIR/data" -directfs.bucket "$BUCKET" -directfs.create)
    fi
    STORE_DESC="gofakes3 (-backend $S3_BACKEND; keys are files, so DELETE bucket can answer BucketNotEmpty with zero objects listed -- see the note in scripts/s3-rehearsal-endpoint.sh)"
fi
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
export DG_SANDBOX_CA_BUNDLE='$STATE_DIR/tls/bundle.pem'
export DG_SANDBOX_TLS_CERT='$STATE_DIR/tls/ca.pem'
export DG_SANDBOX_TLS_KEY='$STATE_DIR/tls/key.pem'
export DG_SANDBOX_REHEARSAL=1
export DG_SANDBOX_ENDPOINT_IMPL='$STORE_DESC behind a validating SigV4 proxy that logs every request'
EOF
}

# A self-signed certificate for 127.0.0.1. The rehearsal endpoint serves TLS
# because replication always builds https:// URLs for an S3 endpoint: against a
# plain-http endpoint it writes nothing at all and every gate fails for a
# reason that has nothing to do with the gate. The certificate is handed to the
# packet as a CA bundle so the rehearsal needs no skip-verify, and the
# "minimal working replica URL" the packet reports stays exactly what object storage provider needs.
make_cert() {
    # The private key this cert signs is what every rehearsal endpoint
    # trusts. The openssl branch (-keyout) and the python fallback
    # (Path.write_bytes) both honour the process umask, and the default
    # umask 022 yields a 0644 key.pem on a multi-user box -- a 2-minute
    # read for anyone who can `ls`. The rest of the packet uses 0600
    # for its credentials (sandbox-s3-gates.sh:1525); the rehearsal
    # must match.
    umask 077
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
    if [ -f "$STATE_DIR/pids" ]; then
        # The endpoint is the proxy, not the store: the documented usage
        # binds DG_REHEARSAL_S3_URL to the proxy, and a proxy that has died
        # while the store still answers would silently re-emit a dead
        # endpoint and every S3 gate would then fail with connection refused.
        # Walk every recorded pid and require them all to be alive.
        all_alive=1
        while IFS= read -r pid; do
            [ -n "$pid" ] || continue
            kill -0 "$pid" 2>/dev/null || { all_alive=0; break; }
        done < "$STATE_DIR/pids"
        if [ "$all_alive" = 1 ]; then
            emit_exports
            return 0
        fi
        # Partial state: a pid is gone. Reap any survivors and fall through
        # to a clean start so the caller's eval does not bind a dead URL.
        while IFS= read -r pid; do
            [ -n "$pid" ] || continue
            kill "$pid" 2>/dev/null || true
        done < "$STATE_DIR/pids"
        rm -f "$STATE_DIR/pids"
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

    # Mirror stop()'s ownership refusal (line 305-311) instead of
    # silently `rm -rf`ing the shared default $STATE_DIR. A second
    # concurrent `start` would otherwise clobber the pids, the tls/ dir and
    # the fault file the first run is mid-gate against, and a later `stop`
    # from the first run would then remove the directory the second run is
    # still using. The rehearsal driver already gives each run its own
    # mktemp -d via DG_REHEARSAL_STATE, so this only bites a hand-run
    # `start`; refusing is still the correct contract. The liveness test is
    # the same one stop() uses, so a stale owner.pid from a crashed run
    # (pid reused, or the long-gone original) is taken over instead of
    # permanently wedging the script.
    local owner
    owner="$(state_owner)"
    if [ -n "$owner" ] && [ "$owner" != "$$" ] && kill -0 "$owner" 2>/dev/null; then
        audit "start REFUSED (owned by live pid $owner)"
        echo "refusing to start: $STATE_DIR belongs to the live process $owner, not this one" >&2
        return 0
    fi
    rm -rf "$STATE_DIR"
    mkdir -p "$STATE_DIR/data" "$STATE_DIR/log"
    echo "$$" > "$STATE_DIR/owner.pid"
    audit start
    # Register an EXIT trap that reaps any spawned child if start() aborts
    # before reaching the happy path. Without it, a missing gofakes3, a
    # make_cert failure, or a dead child caught by kill -0 would leave
    # gofakes3 and the proxy orphaned with their ports held and their PIDs
    # written into STATE_DIR/pids -- the next caller cannot tell which pid
    # file is live.
    _start_cleanup() {
        trap - EXIT
        if [ -f "$STATE_DIR/pids" ]; then
            while IFS= read -r pid; do
                [ -n "$pid" ] || continue
                kill "$pid" 2>/dev/null || true
            done < "$STATE_DIR/pids"
        fi
    }
    trap _start_cleanup EXIT
    if ! make_cert; then
        echo "could not create a TLS certificate for the rehearsal endpoint" >&2
        exit 1
    fi
    # Go's crypto/x509 reads SSL_CERT_FILE, and the pinned sidecar release ABORTS its
    # replica client if AWS_CA_BUNDLE is set: "cannot load aws config: unable to
    # add custom RootCAs HTTPClient, has no WithTransportOptions". So
    # SSL_CERT_FILE is the only way to teach this pinned version to trust the
    # rehearsal certificate. The system roots stay in the bundle so a request
    # that legitimately leaves loopback still validates normally.
    : > "$STATE_DIR/tls/bundle.pem"
    for f in /etc/ssl/certs/ca-bundle.crt /etc/ssl/certs/ca-certificates.crt \
             /etc/pki/tls/certs/ca-bundle.crt; do
        [ -r "$f" ] && cat "$f" >> "$STATE_DIR/tls/bundle.pem" && break
    done
    cat "$STATE_DIR/tls/ca.pem" >> "$STATE_DIR/tls/bundle.pem"
    S3_PORT="$(free_port)"
    PROXY_PORT="$(free_port)"

    "$GOFAKES3_BIN" -host "127.0.0.1:$S3_PORT" -backend "$S3_BACKEND" "${STORE_FS_ARGS[@]}" \
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
    # same signed path replication will use rather than by a side door -- and its
    # REACHABILITY is proved here too. A proxy that is up but answering nothing
    # (a TLS handshake that never completes, a signer that rejects everything)
    # looks exactly like an endpoint that refuses to work, and every gate then
    # fails for a reason that is not about the gate.
    if ! python3 "$(dirname "$PROXY")/s3probe.py" --endpoint "127.0.0.1:$PROXY_PORT" --scheme https \
        --ca-bundle "$STATE_DIR/tls/bundle.pem" \
        --bucket "$BUCKET" --access-key-id "$KEY_ID" --secret-access-key "$SECRET" \
        create-bucket > "$STATE_DIR/log/create.log" 2>&1; then
        echo "the rehearsal endpoint did not accept a signed request through its proxy" >&2
        cat "$STATE_DIR/log/create.log" "$STATE_DIR/log/proxy.log" >&2 2>/dev/null || true
        exit 1
    fi

    # Disarm the startup cleanup trap: the children are now expected to live
    # until the operator runs `stop`. Without the disarm, every successful
    # `start` would also kill its own children when this function returns.
    trap - EXIT
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
    # The same pids file start() walks with kill -0 -- a proxy that has
    # died (SIGKILL, OOM, host crash) leaves a stale file and start()'s
    # own comment says "a proxy that has died while the store still
    # answers would silently re-emit a dead endpoint and every S3 gate
    # would then fail with connection refused". status() must not lie
    # about it.
    if [ -f "$STATE_DIR/pids" ]; then
        local all_alive=1
        while IFS= read -r pid; do
            [ -n "$pid" ] || continue
            kill -0 "$pid" 2>/dev/null || { all_alive=0; break; }
        done < "$STATE_DIR/pids"
        if [ "$all_alive" = 1 ]; then
            echo "running (state $STATE_DIR, proxy port $(cat "$STATE_DIR/proxy_port" 2>/dev/null))"
        else
            echo "not running (state $STATE_DIR -- recorded pids no longer alive)"
        fi
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
