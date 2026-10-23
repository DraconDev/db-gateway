#!/usr/bin/env bash
# Release bundle for the recorded micro2 target, or an explicit local host build.
#
# Usage: scripts/release-bundle.sh [OUTPUT_DIR]
# Default OUTPUT_DIR is a disposable mktemp directory; its path is printed to
# stdout (the only stdout output). Everything else goes to stderr.
#
# The default micro2 path never builds natively. It builds and smoke-tests the
# locked release inside one immutable Ubuntu 24.04 amd64 image whose glibc
# matches the recorded micro2 host. Set DG_CONTAINER_ENGINE=podman or docker to
# choose an engine. Set DG_RELEASE_TARGET=host only for a deliberately local,
# non-micro2 bundle; such a bundle is never labelled target-ABI verified.
#
# Stages the release executable, README, LICENSE, operator/API docs, builder
# metadata, and SHA-256 checksums. No publishing or dependency upgrades.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# A caller-supplied output directory belongs to the caller; a default one is
# ours, and ours to remove if we fail before filling it. Without this, every
# failing run (a missing container engine, a refused target) left an empty
# /tmp/dg-bundle-XXXXXX behind -- four of them were found on the maintainer's
# box. `check-orphans.sh` now reports that residue, so it has to be absent on
# the success path AND the failure path.
if [ -n "${1:-}" ]; then
    OUT="$1"
    OUT_IS_OURS=0
else
    OUT="$(mktemp -d "${TMPDIR:-/tmp}/dg-bundle-XXXXXX")"
    OUT_IS_OURS=1
fi
mkdir -p "$OUT"

cleanup_own_out() {
    # A caller-supplied directory is never ours, and a published bundle now
    # belongs to the caller. Written as `if`s rather than `[ ... ] && return`
    # because this runs from an EXIT trap under `set -e`: a bare failing test
    # would abort the trap early, and a trap that exits before its `rm` looks
    # exactly like a leak.
    if [ "$OUT_IS_OURS" != 1 ]; then
        return 0
    fi
    if [ "${BUNDLE_PUBLISHED:-0}" = 1 ]; then
        return 0
    fi
    rm -rf "$OUT" 2>/dev/null || true
}

# The failure paths are not all `fail()` calls. Under `set -euo pipefail` a
# command that exits non-zero (a failed `cargo build`, an engine that dies
# mid-inspect) terminates the script directly, so an EXIT trap is the only
# hook that covers every one of them. `cleanup_own_out` is a no-op once the
# bundle was published and a no-op for a caller-supplied directory, so a
# successful run and a caller-owned path are both untouched.
BUNDLE_PUBLISHED=0
trap cleanup_own_out EXIT

log() { echo "[release-bundle] $*" >&2; }
fail() {
    echo "[release-bundle] FAIL: $*" >&2
    exit 1
}

RELEASE_TARGET="${DG_RELEASE_TARGET:-micro2}"
BUILDER_IMAGE="docker.io/library/ubuntu@sha256:496754492fb28b4d3049432f2ca787449331e23fb14f0dd3fffea86bf5a93eb4"
BUILDER_OS="Ubuntu 24.04 amd64"
EXPECTED_GLIBC="2.39"
EXPECTED_INTERPRETER="/lib64/ld-linux-x86-64.so.2"
CONTAINER_ENGINE=""
BUILT_ON=""
BUILDER_METADATA="native-host"
BINARY=""
RELEASE_CARGO_HOME="${DG_RELEASE_CARGO_HOME:-${CARGO_HOME:-$HOME/.cargo}}"
RELEASE_TARGET_DIR="${DG_RELEASE_TARGET_DIR:-.cargo/target}"
case "$RELEASE_TARGET_DIR" in
    ""|/*|..|../*|*/..|*/../*) fail "DG_RELEASE_TARGET_DIR must be a relative path inside the repository" ;;
esac
RELEASE_TARGET_DIR_HOST="$ROOT/$RELEASE_TARGET_DIR"
RELEASE_TARGET_DIR_CONTAINER="/src/$RELEASE_TARGET_DIR"
mkdir -p "$RELEASE_TARGET_DIR_HOST"

container_engine() {
    if [ -n "${DG_CONTAINER_ENGINE:-}" ]; then
        CONTAINER_ENGINE="$DG_CONTAINER_ENGINE"
        command -v "$CONTAINER_ENGINE" >/dev/null 2>&1 \
            || fail "requested container engine is unavailable: $CONTAINER_ENGINE"
        return
    fi
    local candidate
    for candidate in podman docker; do
        if command -v "$candidate" >/dev/null 2>&1; then
            CONTAINER_ENGINE="$candidate"
            return
        fi
    done
    fail "micro2 bundles require podman or docker; native fallback is forbidden"
}

run_container() {
    "$CONTAINER_ENGINE" run --rm \
        --network default \
        --env CARGO_TERM_COLOR=never \
        --env CARGO_INCREMENTAL=0 \
        --env CARGO_BUILD_JOBS="${DG_RELEASE_BUILD_JOBS:-4}" \
        --env CARGO_PROFILE_RELEASE_CODEGEN_UNITS="${DG_RELEASE_CODEGEN_UNITS:-16}" \
        --env RUSTUP_TOOLCHAIN=1.97.1 \
        --env RUSTFLAGS= \
        --env SOURCE_DATE_EPOCH=0 \
        --env TZ=UTC \
        --env DG_RELEASE_BUILDER=1 \
        --env CARGO_TARGET_DIR="$RELEASE_TARGET_DIR_CONTAINER" \
        --volume "$ROOT:/src:rw" \
        --volume "$RELEASE_CARGO_HOME:/root/.cargo:rw" \
        --workdir /src \
        "$BUILDER_IMAGE" \
        bash -euo pipefail -c "$1"
}

verify_micro2_builder() {
    container_engine
    log "verifying pinned builder $BUILDER_IMAGE with $CONTAINER_ENGINE"
    local facts
    facts="$(run_container '
        . /etc/os-release
        printf "%s\n%s\n%s\n" \
            "$PRETTY_NAME" \
            "$(uname -m)" \
            "$(ldd --version | head -1)"
    ')" || fail "pinned builder could not be inspected"
    printf '%s\n' "$facts" | grep -Fq "Ubuntu 24.04" \
        || fail "pinned builder is not Ubuntu 24.04: $facts"
    printf '%s\n' "$facts" | grep -Eq '(^|[[:space:]])x86_64$' \
        || fail "pinned builder is not amd64: $facts"
    printf '%s\n' "$facts" | grep -Fq "$EXPECTED_GLIBC" \
        || fail "pinned builder glibc does not match $EXPECTED_GLIBC: $facts"
}

build_micro2() {
    verify_micro2_builder
    log "building and smoke-testing release inside the pinned micro2 builder"
    run_container '
        export DEBIAN_FRONTEND=noninteractive
        apt-get -qq update
        apt-get -qq install -y --no-install-recommends \
            ca-certificates curl gcc libc6-dev pkg-config
        curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y --profile minimal --default-toolchain 1.97.1 >/dev/null
        . "$HOME/.cargo/env"
        cargo build --release --locked --quiet --bin dg
        "$CARGO_TARGET_DIR/release/dg" --version >/dev/null
        smoke_dir="$(mktemp -d)"
        trap '\''rm -rf "$smoke_dir"'\'' EXIT
        key="$("$CARGO_TARGET_DIR/release/dg" init --data-dir "$smoke_dir" \
            | sed -n '\''s/.*"owner_key":"\([0-9a-f]*\)".*/\1/p'\'')"
        test "${#key}" -eq 64
        interpreter="$(readelf -lW "$CARGO_TARGET_DIR/release/dg" \
            | sed -n '\''s/.*Requesting program interpreter: \([^]]*\).*/\1/p'\'')"
        test "$interpreter" = /lib64/ld-linux-x86-64.so.2
        readelf -hW "$CARGO_TARGET_DIR/release/dg" \
            | grep -Eq '\''Machine:[[:space:]]+Advanced Micro Devices X86-64'\''
    ' || fail "pinned-container release build or smoke test failed"
    BINARY="$RELEASE_TARGET_DIR_HOST/release/dg"
    BUILT_ON="$($CONTAINER_ENGINE run --rm "$BUILDER_IMAGE" sh -c 'uname -srm')"
    BUILDER_METADATA="$BUILDER_IMAGE|glibc-$EXPECTED_GLIBC|$BUILT_ON"
}

build_host() {
    [ "$(uname -s)" = "Linux" ] || fail "host bundles currently require Linux"
    [ "$(uname -m)" = "x86_64" ] || fail "host bundles currently require x86_64"
    log "building an explicitly requested, non-micro2 host bundle"
    CARGO_TARGET_DIR="$RELEASE_TARGET_DIR_HOST" \
        cargo build --release --locked --quiet --manifest-path "$ROOT/Cargo.toml" >&2
    BINARY="$RELEASE_TARGET_DIR_HOST/release/dg"
    BUILT_ON="$(uname -srm)"
}

case "$RELEASE_TARGET" in
    micro2) build_micro2 ;;
    host) build_host ;;
    *) fail "DG_RELEASE_TARGET must be micro2 or host" ;;
esac

[ -x "$BINARY" ] || fail "release artifact is missing or not executable: $BINARY"
VERSION="$($BINARY --version | awk '{print $2}')"
if [ "$RELEASE_TARGET" = "micro2" ]; then
    "$BINARY" --version >/dev/null || fail "container-built artifact cannot execute on host"
    command -v readelf >/dev/null 2>&1 || fail "readelf is required to verify the micro2 artifact"
    readelf -hW "$BINARY" | grep -Eq 'Machine:[[:space:]]+Advanced Micro Devices X86-64' \
        || fail "release artifact is not x86_64"
    readelf -lW "$BINARY" | grep -Fq "[Requesting program interpreter: $EXPECTED_INTERPRETER]" \
        || fail "release artifact has the wrong ELF interpreter"
else
    "$BINARY" --version >/dev/null || fail "release artifact cannot execute on the build host"
fi

log "staging bundle in $OUT"
# The executable must be executable, and it must be executable *by the
# target*, so the mode is pinned rather than inherited. `cp` applies the
# builder's umask to the new file, so a bundle staged under `umask 0111`
# ships a 644 dg that `validate-deploy.sh`'s `[ -f ]` accepts and the
# target then fails to exec under User=dg. scripts/setup.sh already chmods
# its install to 755; the release path must not be the odd one out.
cp "$BINARY" "$OUT/dg"
chmod 755 "$OUT/dg"
cp "$ROOT/README.md" "$ROOT/LICENSE" "$OUT/"
mkdir -p "$OUT/docs"
cp "$ROOT/docs/api.md" "$ROOT/docs/architecture.md" "$OUT/docs/"
# The service template travels with the bundle. It used to stay in the
# repository, which meant a clean host could not be deployed from the bundle
# alone: the documented procedure (docs/deployment.md step 6) needs
# deploy/dg.service, and the bundle is what an operator copies to a new
# host. The A16 clean-host rehearsal found this by trying to install the
# service from the bundle and finding the template missing.
mkdir -p "$OUT/deploy"
cp "$ROOT/deploy/dg.service" "$OUT/deploy/dg.service"

if [ "$RELEASE_TARGET" = "micro2" ]; then
    IFS='|' read -r BUILDER_IMAGE_RECORDED BUILDER_GLIBC_RECORDED BUILDER_HOST_RECORDED \
        <<<"$BUILDER_METADATA"
    ABI_NOTE="container-built and smoke-tested against the pinned target-matched Ubuntu image; not cross-platform."
else
    BUILDER_IMAGE_RECORDED="none"
    BUILDER_GLIBC_RECORDED="host"
    BUILDER_HOST_RECORDED="$BUILT_ON"
    ABI_NOTE="explicit host build; micro2 ABI is not verified."
fi

cat >"$OUT/BUILD-INFO.txt" <<EOF
name: dg
version: $VERSION
arch: x86_64
profile: release
locked: true
built_on: $BUILDER_HOST_RECORDED
built_at: $(date -u +%Y-%m-%dT%H:%M:%SZ)
target: $RELEASE_TARGET
interpreter: $([ "$RELEASE_TARGET" = "micro2" ] && printf '%s' "$EXPECTED_INTERPRETER" || printf '%s' host-default)
builder_image: $BUILDER_IMAGE_RECORDED
builder_os: $([ "$RELEASE_TARGET" = "micro2" ] && printf '%s' "$BUILDER_OS" || printf '%s' native-host)
builder_glibc: $BUILDER_GLIBC_RECORDED
container_engine: $([ "$RELEASE_TARGET" = "micro2" ] && printf '%s' "$CONTAINER_ENGINE" || printf '%s' none)
abi_verified: $([ "$RELEASE_TARGET" = "micro2" ] && printf '%s' true || printf '%s' false)
verified_executable: true
note: $ABI_NOTE
EOF

log "generating checksums"
(cd "$OUT" && sha256sum dg README.md LICENSE docs/api.md docs/architecture.md deploy/dg.service BUILD-INFO.txt >SHA256SUMS)

# From here the bundle is the caller's: the EXIT trap must not remove it.
BUNDLE_PUBLISHED=1
printf '%s\n' "$OUT"
