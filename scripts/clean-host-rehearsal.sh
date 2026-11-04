#!/usr/bin/env bash
# Clean-host rehearsal — prove first-deploy and disaster recovery from nothing.
#
# The gap this closes: every first-deploy and recovery claim in this repo so
# far was made in one of two places. The first deploy was made on deployment-host — a
# real host, but not a clean one, and run as a sequence of hand-executed
# steps rather than one repeatable command. The restore drills were made
# against disposable directories on a development workstation, where the
# systemd unit, the service lifecycle and the host-level loss of the data
# volume were never exercised at all.
#
# What is missing is the question an owner actually asks before pointing this
# service at a new host: "if you gave me a machine with nothing on it and
# this release bundle, could you get to a working gateway — and if that
# machine then died, could you get my data back?"
#
# This script answers it, end to end, in one run, repeatably:
#
#   1. Build the release bundle the documented way (container-built, ABI
#      verified against the pinned Ubuntu 24.04 target image).
#   2. Start host A: a fresh container from that same pinned image, with
#      systemd as PID 1 and nothing of this project on it. The only thing
#      that crosses onto it is the release bundle.
#   3. Run the documented first deploy on host A (docs/deployment.md steps
#      1-6), then assert liveness, an authenticated read/write round trip
#      and restart survival.
#   4. Publish a backup set with the canonical producer, verify it, and
#      copy it off-host (the scp/rsync leg) together with the owner
#      credential an operator keeps in their password manager.
#   5. Destroy host A entirely. The container is removed: the data
#      directory, the backup root, the registry and the credential file all
#      go with it.
#   6. Start host B: a second clean host, provisioned from the same bundle,
#      and restore the off-host artifact onto an empty target using Path A
#      of docs/recovery-contract.md §3. Then assert that the exact rows
#      came back, that the pre-loss credential authenticates against the
#      restored registry, that the restored copy is independent of the
#      artifact, and that a corrupt artifact publishes nothing.
#
# The clean host is a container, not a fleet machine: touching deployment-host or any
# other host is a live change that this repository's own boundary requires
# separate owner approval, and a rehearsal that needs that approval is a
# rehearsal nobody runs. The container is built from the digest-pinned image
# whose glibc matches the recorded deployment-host target, so the artifact under test
# is the same one a real host would receive. Pointing the same phases at a
# real host is a separate, owner-approved exercise; this script never
# contacts one.
#
# Usage:
#   scripts/clean-host-rehearsal.sh [--bundle DIR] [--evidence PATH]
#                                    [--engine docker|podman] [--keep] [--help]
#
#   --bundle DIR   reuse an existing release bundle instead of building one
#   --evidence PATH  where to write the evidence log
#                   (default: docs/evidence/clean-host-rehearsal-<date>.log)
#   --engine NAME  container engine (default: docker, then podman)
#   --keep         keep the containers and the work directory for inspection
#
# Requires: a container engine that can run a privileged systemd container,
# python3, sha256sum, curl.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ENGINE=""
BUNDLE_DIR=""
EVIDENCE=""
KEEP=0
WORKDIR=""
OFFHOST=""
HOST_A="dg-clean-a"
HOST_B="dg-clean-b"
RELEASE_SCRIPT="$ROOT/scripts/release-bundle.sh"
PHASES_SCRIPT="$ROOT/scripts/clean-host-phases.sh"
BACKUP_SCRIPT="$ROOT/scripts/backup.sh"

log() { echo "[rehearsal] $*"; }
fail() { echo "[rehearsal] FAIL: $*" >&2; exit 1; }

usage() {
    sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --bundle)
            [ $# -ge 2 ] || fail "--bundle requires a directory"
            BUNDLE_DIR="$2"; shift 2 ;;
        --bundle=*)
            BUNDLE_DIR="${1#--bundle=}"; shift ;;
        --evidence)
            [ $# -ge 2 ] || fail "--evidence requires a path"
            EVIDENCE="$2"; shift 2 ;;
        --evidence=*)
            EVIDENCE="${1#--evidence=}"; shift ;;
        --engine)
            [ $# -ge 2 ] || fail "--engine requires a name"
            ENGINE="$2"; shift 2 ;;
        --engine=*)
            ENGINE="${1#--engine=}"; shift ;;
        --keep)
            KEEP=1; shift ;;
        --help|-h)
            usage; exit 0 ;;
        *)
            fail "unknown argument: $1 (try --help)" ;;
    esac
done

[ -f "$RELEASE_SCRIPT" ] || fail "missing $RELEASE_SCRIPT"
[ -f "$PHASES_SCRIPT" ] || fail "missing $PHASES_SCRIPT"
[ -f "$BACKUP_SCRIPT" ] || fail "missing $BACKUP_SCRIPT"

# The pinned builder image is the single source of truth for what "the
# target" means: release-bundle.sh owns it, and the rehearsal reads it from
# there rather than keeping a second copy of the digest that could drift.
PINNED_IMAGE="$(sed -n 's/^BUILDER_IMAGE=["'"'"']\(docker\.io[^"'"'"']*\)["'"'"'].*/\1/p' "$RELEASE_SCRIPT" | head -1)"
[ -n "$PINNED_IMAGE" ] || fail "cannot read the pinned builder image from $RELEASE_SCRIPT"

# --- container engine --------------------------------------------------------
if [ -z "$ENGINE" ]; then
    for candidate in docker podman; do
        if command -v "$candidate" >/dev/null 2>&1; then
            ENGINE="$candidate"
            break
        fi
    done
fi
[ -n "$ENGINE" ] || fail "no container engine found (need docker or podman)"
command -v "$ENGINE" >/dev/null 2>&1 || fail "container engine not available: $ENGINE"
command -v python3 >/dev/null 2>&1 || fail "python3 is required"
command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is required"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/dg-clean-host-XXXXXX")" || fail "cannot create a work directory"
OFFHOST="$WORKDIR/offhost"
mkdir -p "$OFFHOST"

cleanup() {
    local code=$?
    if [ "$KEEP" -eq 1 ]; then
        log "keeping $WORKDIR and the containers (--keep)"
    else
        "$ENGINE" rm -f "$HOST_A" >/dev/null 2>&1 || true
        "$ENGINE" rm -f "$HOST_B" >/dev/null 2>&1 || true
        # The containers ran as root and wrote into the mounted work
        # directory (the release builder's cargo home, and the off-host
        # copy the recovery leg reads), so the operator's own `rm -rf`
        # cannot remove those entries and leaves a half-deleted tree in
        # /tmp. Give the tree back to the invoking user first, then
        # delete it. Residue like that is not cosmetic: the next run
        # reuses the directory, and a stale root-owned file inside it
        # makes the next run's evidence untrustworthy.
        if [ -d "$WORKDIR" ]; then
            if "$ENGINE" image inspect "$PINNED_IMAGE" >/dev/null 2>&1; then
                "$ENGINE" run --rm -v "$WORKDIR:/w" "$PINNED_IMAGE" \
                    chown -R "$(id -u):$(id -g)" /w >/dev/null 2>&1 || true
            fi
            rm -rf "$WORKDIR" 2>/dev/null || true
        fi
    fi
    exit "$code"
}
trap cleanup EXIT

# --- evidence log ------------------------------------------------------------
if [ -z "$EVIDENCE" ]; then
    EVIDENCE="$ROOT/docs/evidence/clean-host-rehearsal-$(date -u +%Y-%m-%d).log"
    # A dated transcript is never rewritten: a second run on the same day
    # gets its own file rather than truncating the first one's evidence.
    if [ -e "$EVIDENCE" ]; then
        EVIDENCE="$ROOT/docs/evidence/clean-host-rehearsal-$(date -u +%Y-%m-%dT%H%M%SZ).log"
    fi
fi
mkdir -p "$(dirname "$EVIDENCE")"
: >"$EVIDENCE"

say() {
    echo "$*" | tee -a "$EVIDENCE"
}

say "=== clean-host rehearsal — first deploy and disaster recovery ==="
say "date (UTC):        $(date -u +%Y-%m-%dT%H:%M:%SZ)"
say "repo:              $ROOT"
say "container engine:  $ENGINE"
say "pinned target img: $PINNED_IMAGE"
say "git HEAD:          $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
say ""

# --- 1. the release bundle ---------------------------------------------------
if [ -z "$BUNDLE_DIR" ]; then
    say "step 1: building the release bundle the documented way (container-built, ABI verified)"
    BUNDLE_DIR="$WORKDIR/bundle"
    # The cargo home is redirected into the work directory so the container
    # build cannot touch the operator's own ~/.cargo: the builder installs
    # its own rustup there, and a shared home would let that overwrite the
    # host's shims.
    if ! DG_RELEASE_TARGET=deployment-host \
        DG_CONTAINER_ENGINE="$ENGINE" \
        DG_RELEASE_CARGO_HOME="$WORKDIR/cargo" \
        "$RELEASE_SCRIPT" "$BUNDLE_DIR" >/dev/null 2>>"$EVIDENCE"; then
        fail "the release bundle build failed (see $EVIDENCE)"
    fi
    say "  bundle built: $BUNDLE_DIR"
else
    say "step 1: using the nominated release bundle: $BUNDLE_DIR"
fi
[ -x "$BUNDLE_DIR/dg" ] || fail "the bundle carries no executable dg"
( cd "$BUNDLE_DIR" && sha256sum -c SHA256SUMS >/dev/null ) || fail "the bundle's SHA256SUMS do not verify"
say "  bundle SHA256SUMS verify"
say ""

# --- 2. the clean host image -------------------------------------------------
IMAGE_TAG="dg-cleanhost:ubuntu-24.04"
if ! "$ENGINE" image inspect "$IMAGE_TAG" >/dev/null 2>&1; then
    say "step 2: building the clean-host image from the pinned target image"
    cat >"$WORKDIR/Dockerfile" <<EOF
FROM $PINNED_IMAGE
RUN export DEBIAN_FRONTEND=noninteractive \
 && apt-get -qq update \
 && apt-get -qq install -y --no-install-recommends \
      systemd systemd-sysv python3 curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*
EOF
    # The image build's own output is package-manager noise; the evidence log
    # records what was built, not how apt unpacked it. A failed build still
    # lands its tail in the log, because that is where the reason is.
    if ! "$ENGINE" build -t "$IMAGE_TAG" - <"$WORKDIR/Dockerfile" >"$WORKDIR/image-build.log" 2>&1; then
        tail -30 "$WORKDIR/image-build.log" >>"$EVIDENCE" 2>&1 || true
        fail "the clean-host image build failed (see $EVIDENCE)"
    fi
    IMAGE_ID="$("$ENGINE" image inspect --format '{{.Id}}' "$IMAGE_TAG" 2>/dev/null || echo unknown)"
    say "  clean-host image ready: $IMAGE_TAG ($IMAGE_ID)"
else
    say "step 2: reusing the clean-host image: $IMAGE_TAG"
fi
say ""

start_host() { # start_host <name> <label>
    local name="$1" label="$2" state _
    "$ENGINE" rm -f "$name" >/dev/null 2>&1 || true
    say "  starting $label as '$name'"
    if ! "$ENGINE" run -d --privileged --cgroupns=host \
        -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
        -v "$OFFHOST:/offhost" \
        --name "$name" "$IMAGE_TAG" /sbin/init >/dev/null 2>>"$EVIDENCE"; then
        "$ENGINE" rm -f "$name" >/dev/null 2>&1 || true
        if ! "$ENGINE" run -d --privileged \
            -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
            -v "$OFFHOST:/offhost" \
            --name "$name" "$IMAGE_TAG" /sbin/init >/dev/null 2>>"$EVIDENCE"; then
            fail "cannot start $label (see $EVIDENCE)"
        fi
    fi
    for _ in $(seq 1 90); do
        state="$("$ENGINE" exec "$name" systemctl is-system-running 2>/dev/null || true)"
        case "$state" in
            running|degraded)
                say "  $label booted systemd ($state)"
                return 0
                ;;
        esac
        sleep 2
    done
    "$ENGINE" logs "$name" 2>&1 | tail -30 >>"$EVIDENCE" 2>&1 || true
    fail "$label did not boot systemd (see $EVIDENCE)"
}

stage_host() { # stage_host <name>
    local name="$1"
    "$ENGINE" cp "$BUNDLE_DIR" "$name:/root/bundle" >/dev/null \
        || fail "cannot copy the bundle onto $name"
    "$ENGINE" cp "$PHASES_SCRIPT" "$name:/root/clean-host-phases.sh" >/dev/null \
        || fail "cannot copy the phase script onto $name"
    # The backup producer goes where the deployment guide puts it, so the
    # rehearsal runs the same file the hourly timer runs.
    "$ENGINE" cp "$BACKUP_SCRIPT" "$name:/usr/local/bin/backup.sh" >/dev/null \
        || fail "cannot copy the backup producer onto $name"
    "$ENGINE" exec -u root "$name" chmod 0755 /usr/local/bin/backup.sh >/dev/null \
        || fail "cannot make the backup producer executable"
}

run_phase() { # run_phase <name> <mode>
    local name="$1" mode="$2"
    if ! "$ENGINE" exec -u root "$name" bash /root/clean-host-phases.sh "$mode" 2>&1 | tee -a "$EVIDENCE"; then
        fail "the '$mode' phase failed on $name (see $EVIDENCE)"
    fi
}

# --- 3. host A: the first deploy --------------------------------------------
say "step 3: host A — the documented first deploy on a clean host"
start_host "$HOST_A" "host A"
stage_host "$HOST_A"
run_phase "$HOST_A" deploy
say "  host A: first deploy complete"
say ""

# --- 4. the loss ------------------------------------------------------------
say "step 4: destroying host A (the container is removed: data dir, backup root, registry and credential all go)"
"$ENGINE" rm -f "$HOST_A" >/dev/null 2>&1 || fail "cannot destroy host A"
say "  host A is gone"
say ""

# --- 5. host B: the recovery ------------------------------------------------
say "step 5: host B — a replacement clean host restoring from the off-host artifact"
start_host "$HOST_B" "host B"
stage_host "$HOST_B"
run_phase "$HOST_B" restore
say "  host B: disaster recovery complete"
say ""

say "=== RESULT: the clean host was deployed and recovered ==="
say "evidence: $EVIDENCE"
