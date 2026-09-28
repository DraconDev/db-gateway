#!/usr/bin/env bash
# Rehearse the whole §6 packet end to end against the local rehearsal endpoint.
#
# Run: scripts/rehearse-sandbox-packet.sh --out FILE
#
# WHAT THIS IS. The bucket is the objective's BLOCKER, so every gate that needs
# one has never executed -- the packet's execution path is untested code. This
# driver starts a disposable S3-compatible endpoint on loopback (scripts/
# s3-rehearsal-endpoint.sh), runs the real packet against it with --rehearsal,
# stops the endpoint, and then checks the two claims the packet cannot check
# about itself: that nothing was left on disk, and that no process survived.
#
# Both halves are needed. Gate 12 deletes the objects and lists what is left in
# the bucket, but a packet that says "no residue" while its own temp tree and
# its own logging proxies are still running has not earned the word. That check
# is only possible from outside the run, so it lives here.
#
# IT IS NOT §6 EVIDENCE. Nothing here touches the owner's account. The packet it
# writes says so in its own header.

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENDPOINT_SCRIPT="$REPO/scripts/s3-rehearsal-endpoint.sh"
PACKET_SCRIPT="$REPO/scripts/sandbox-packet.sh"
OUT=""
TIMEOUT_S="${DG_REHEARSAL_TIMEOUT:-1800}"

usage() {
    cat <<'USAGE'
usage: scripts/rehearse-sandbox-packet.sh --out FILE

  --out FILE      the packet to write (a rehearsal packet, not §6 evidence)
  -h|--help       this text

  DG_REHEARSAL_TIMEOUT   seconds to allow the packet (default 1800)
  DG_REHEARSAL_STATE     endpoint state dir (default: a fresh dir inside this
                         run's own temp tree, so runs cannot collide)
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --out) OUT="${2:?--out needs a value}"; shift 2;;
        -h|--help) usage; exit 0;;
        *) echo "unknown flag: $1" >&2; usage; exit 2;;
    esac
done
[ -n "$OUT" ] || { echo "--out is required" >&2; usage; exit 2; }
mkdir -p "$(dirname "$OUT")"

STATE_DIR="${DG_REHEARSAL_STATE:-}"
SANDBOX_DIR="$(mktemp -d "${TMPDIR:-/tmp}/dg-sandbox-run-XXXXXX")"
# Each run gets its OWN endpoint state directory. A fixed path shared between
# two concurrent rehearsals lets one run's stop() kill the other's endpoint and
# delete its state -- which is exactly what happened to a rehearsal whose
# endpoint vanished mid-gate.
[ -n "$STATE_DIR" ] || STATE_DIR="$SANDBOX_DIR/endpoint"
STATUS=0

# Everything this script starts is torn down whatever happens, including on an
# interrupt: a rehearsal that leaves a proxy bound to a port is a rehearsal that
# made the next one fail confusingly. Idempotent, because the main flow calls
# it before checking what it left.
teardown() {
    DG_REHEARSAL_STATE="$STATE_DIR" "$ENDPOINT_SCRIPT" stop >/dev/null 2>&1 || true
    rm -rf "$SANDBOX_DIR"
}
trap teardown EXIT INT TERM

echo "== starting the disposable endpoint" >&2
if ! EXPORTS="$(DG_REHEARSAL_STATE="$STATE_DIR" "$ENDPOINT_SCRIPT" start)"; then
    echo "the rehearsal endpoint could not be started; the S3 gates stay BLOCKED" >&2
    exit 2
fi
eval "$EXPORTS"
echo "   endpoint $DG_SANDBOX_SCHEME://$DG_SANDBOX_ENDPOINT  bucket $DG_SANDBOX_BUCKET" >&2

# Record what is already running before the packet, so "nothing survived" is a
# comparison rather than an assumption about a machine that may have had
# unrelated litestream or proxy processes on it before we arrived.
PRE_PIDS="$(pgrep -f 's3-log-proxy.py --listen' 2>/dev/null | sort | tr '\n' ' ' || true)"
PRE_STORE="$(pgrep -f 'gofakes3 -host' 2>/dev/null | sort | tr '\n' ' ' || true)"

echo "== running the packet (timeout ${TIMEOUT_S}s)" >&2
timeout "$TIMEOUT_S" "$PACKET_SCRIPT" --out "$OUT" --rehearsal
STATUS=$?
echo "== packet exited $STATUS" >&2

# Teardown BEFORE the checks below. They assert that teardown left nothing
# behind, so they have to run after it -- and asserting before the cleanup has
# run reports this driver's own working directory as residue, which is how the
# first rehearsal "failed" a run that had leaked nothing at all.
teardown

# --------------------------------------------------------------------------
# Post-run assertions. These are the claims the packet could not make about
# itself, and they are the difference between "the gates passed" and "the gates
# passed and cleaned up".
# --------------------------------------------------------------------------
{
    echo
    echo "## rehearsal post-conditions (checked by scripts/rehearse-sandbox-packet.sh)"
    echo
} >> "$OUT"

post() { printf '%s\n' "$@" >> "$OUT"; }

# 1. the packet's own sandbox tree and this run's endpoint state are gone
if [ -e "$SANDBOX_DIR" ]; then post "RESIDUE: $SANDBOX_DIR still exists"; STATUS=1
else post "sandbox tree removed: $SANDBOX_DIR"; fi

# 2. any dg-sandbox temp trees this run created
STRAY="$(find "${TMPDIR:-/tmp}" -maxdepth 1 -name 'dg-sandbox*' -newer "$OUT" -print 2>/dev/null || true)"
if [ -n "$STRAY" ]; then post "STRAY temp trees newer than the packet: $STRAY"; STATUS=1
else post "no dg-sandbox* temp tree newer than the packet"; fi

# 3. no logging proxy or store this driver started is still running
#    comm on process-id lists needs both sides empty-safe: an empty pre-list
#    prints one blank line, and comparing blank lines reports a phantom
#    survivor, which is how this check first failed a run that leaked nothing.
pidset_diff() {
    comm -13 <(printf '%s\n' "$1" | grep -E '^[0-9]+$' | sort) \
             <(printf '%s\n' "$2" | grep -E '^[0-9]+$' | sort) | tr '\n' ' '
}
POST_PIDS="$(pgrep -f 's3-log-proxy.py --listen' 2>/dev/null | sort | tr '\n' ' ' || true)"
POST_STORE="$(pgrep -f 'gofakes3 -host' 2>/dev/null | sort | tr '\n' ' ' || true)"
NEW_PROXY="$(pidset_diff "$PRE_PIDS" "$POST_PIDS")"
NEW_STORE="$(pidset_diff "$PRE_STORE" "$POST_STORE")"
if [ -n "$NEW_PROXY" ] || [ -n "$NEW_STORE" ]; then
    post "SURVIVING processes started by the rehearsal: proxy='$NEW_PROXY' store='$NEW_STORE'"
    STATUS=1
else
    post "no logging proxy or object store started by the rehearsal is still running"
fi

# 4. the endpoint state directory is gone
if [ -e "$STATE_DIR" ]; then post "RESIDUE: endpoint state $STATE_DIR still exists"; STATUS=1
else post "endpoint state removed: $STATE_DIR"; fi

# 5. the packet recorded a mode line, so a rehearsal can never be read as §6
if grep -q '^mode: REHEARSAL' "$OUT"; then post "packet declares: mode REHEARSAL"
else post "PACKET DOES NOT DECLARE A REHEARSAL MODE -- fix the header before trusting this file"; STATUS=1; fi

if [ "$STATUS" -eq 0 ]; then
    post ""
    post "all rehearsal post-conditions hold: the run left nothing on disk and nothing running."
else
    post ""
    post "REHEARSAL POST-CONDITIONS FAILED -- see above. The gates are not the only thing that must be true."
fi

echo "rehearsal post-conditions: exit $STATUS" >&2
exit "$STATUS"
