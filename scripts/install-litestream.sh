#!/usr/bin/env bash
# Install and configure litestream on micro2 as the off-host replication
# sidecar for dg.
#
# Usage: scripts/install-litestream.sh
# Runs as root over ssh to micro2; does not require the operator's input
# except for /etc/dg/litestream.env, which the operator (or the previous
# goal's setup) must have already written. This script verifies, installs,
# and renders the litestream config from /etc/dg/litestream.env; the live
# install is the next step (systemctl enable --now dg-litestream) and is
# deliberately NOT done by this script, so the operator can choose when
# the sidecar first starts replicating.
#
# The pin mirrors docs/ovh-sandbox-runbook.md section 1, with one change
# for trust: we verify the tarball's SHA-256 against the upstream
# checksums.txt published at the same release tag, so a tampered mirror
# cannot ship a backdoored binary.
#
# Idempotent. Re-running detects an installed binary and reuses it.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SELF="install-litestream"
fail() { echo "[$SELF] FAIL: $*" >&2; exit 1; }
log() { echo "[$SELF] $*" >&2; }

# --- pin: litestream 0.5.17, the version the runbook and the prior trial
# used; an upgrade is a separate runbook run that re-verifies the pin. ---
VERSION="0.5.17"
# The upstream asset is named litestream-<version>-linux-x86_64.tar.gz -- with
# NO 'v' on the version in the FILENAME, even though the release tag and the
# URL path segment both use v0.5.17. The runbook documented the wrong name
# (litestream-v0.5.17-linux-amd64.tar.gz) and 404s; the correct name is
# verified here by the checksum in Litestream's own checksums.txt at the
# same release tag. Verified 2026-10-15: HTTP 206 for this name, 404 for
# the runbook's.
TARBALL="litestream-${VERSION}-linux-x86_64.tar.gz"
URL="https://github.com/benbjohnson/litestream/releases/download/v${VERSION}/${TARBALL}"
CHECKSUMS_URL="https://github.com/benbjohnson/litestream/releases/download/v${VERSION}/checksums.txt"
# An operator with a checksum-verified tarball already staged locally can
# point this at it; the script still verifies it against CHECKSUMS_URL
# before installing, so a local mirror buys offline reachability and
# nothing else. NEVER bypass the checksum.
LOCAL_TARBALL="${DG_LITESTREAM_TARBALL:-}"
CONFIG_PATH="/etc/litestream.yml"
ENV_PATH="/etc/dg/litestream.env"
DATA_DIR="/var/lib/dg"
INSTALL_PATH="/usr/local/bin/litestream"
TEMPLATE="${ROOT}/deploy/litestream.yml"

command -v ssh >/dev/null || fail "ssh not on PATH (must run from the repo root with the micro2 ssh alias configured)"
[ -r "$TEMPLATE" ] || fail "config template not found: $TEMPLATE (run from the repo root)"

# The target host. The convention matches the rest of the repo's deploy
# scripts: SSH over the operator's ~/.ssh/config alias.
TARGET="${MICRO2_SSH:-micro2}"

# --- 0. env file already present? The contract requires it is the ONLY
# copy of the credentials, and it MUST exist before litestream starts. ---
log "checking ${ENV_PATH} on ${TARGET}"
# The check must run as root: /etc/dg is mode 0750 root:dg, so the ssh
# login user cannot stat inside it. Testing as the login user would report
# "not present" for a file that is there, which is the failure this
# precheck just had.
if ! ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
  "sudo test -f ${ENV_PATH} && sudo stat -c '%a %U:%G' ${ENV_PATH}" 2>/dev/null
then
  fail "${ENV_PATH} not present on ${TARGET}; the operator must create it (mode 0600, owner dg:dg) with the four LITESTREAM_S3_* keys before this script can continue"
fi
# Note: the install script does NOT read or print the secret values; it
# only confirms the file exists with the right mode and ownership. The
# values travel by EnvironmentFile= from systemd to litestream, never via
# this script's stdout/stderr or the repo.

# --- 1. litestream binary: download, verify, install. Idempotent. ---
if ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
  "test -x ${INSTALL_PATH}" 2>/dev/null
then
  # `litestream version` prints a bare version string ("0.5.17"), with no
  # program name and no leading 'v'. Normalise both forms so a re-run
  # recognises the same pin instead of failing on the parse.
  EXISTING_VERSION=$(ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
    "${INSTALL_PATH} version" 2>/dev/null | head -1 | tr -d '\r' | sed -E 's/^(litestream )?v?//')
  if [ "${EXISTING_VERSION}" = "${VERSION}" ]; then
    log "litestream ${VERSION} already installed at ${INSTALL_PATH}; skipping download"
  else
    fail "${INSTALL_PATH} is v${EXISTING_VERSION:-unknown}, expected v${VERSION}; bump the pin in this script and the runbook together, or remove the installed binary to re-run"
  fi
else
  TMPDIR_LOCAL=$(mktemp -d)
  trap 'rm -rf "$TMPDIR_LOCAL"' EXIT

  # A staged, checksum-verified tarball may be nominated instead of a
  # download; the checksum below is still enforced, so a mirror cannot
  # smuggle a different binary.
  if [ -n "$LOCAL_TARBALL" ]; then
    [ -r "$LOCAL_TARBALL" ] || fail "DG_LITESTREAM_TARBALL=$LOCAL_TARBALL is not readable"
    log "using staged tarball ${LOCAL_TARBALL}"
    cp "$LOCAL_TARBALL" "$TMPDIR_LOCAL/$TARBALL"
  else
    log "downloading ${TARBALL} from ${URL}"
    curl -fsSLo "$TMPDIR_LOCAL/$TARBALL" "$URL" || fail "download failed: $URL"
  fi

  log "fetching upstream checksums.txt for the same release tag"
  curl -fsSLo "$TMPDIR_LOCAL/checksums.txt" "$CHECKSUMS_URL" \
    || fail "fetch checksums.txt failed: $CHECKSUMS_URL (cannot verify the binary without it; the runbook's section 1 also fails closed in this state)"

  log "verifying sha256 of ${TARBALL} against the upstream checksums.txt"
  EXPECTED=$(awk -v n="$TARBALL" '$2==n {print $1}' "$TMPDIR_LOCAL/checksums.txt")
  if [ -z "$EXPECTED" ]; then
    fail "${TARBALL} not found in upstream checksums.txt; refusing to install an unverified binary"
  fi
  ACTUAL=$(sha256sum "$TMPDIR_LOCAL/$TARBALL" | awk '{print $1}')
  [ "$ACTUAL" = "$EXPECTED" ] || fail "sha256 mismatch for ${TARBALL}: got $ACTUAL, upstream $EXPECTED"

  log "extracting the litestream binary and installing it to ${INSTALL_PATH}"
  tar -C "$TMPDIR_LOCAL" -xzf "$TMPDIR_LOCAL/$TARBALL" litestream \
    || fail "extract failed: ${TARBALL} has no top-level litestream binary"
  # scp resolves the ssh alias's hostname but does not inherit the
  # IdentityFile the way plain ssh does, so name the key explicitly.
  MICRO2_HOST=$(ssh -G "$TARGET" 2>/dev/null | awk '/^hostname /{print $2; exit}')
  MICRO2_KEY="${MICRO2_SSH_KEY:-$HOME/.ssh/micro2.key}"
  [ -r "$MICRO2_KEY" ] || fail "cannot read the ssh key $MICRO2_KEY (set MICRO2_SSH_KEY to your micro2 key)"
  scp -o BatchMode=yes -q -i "$MICRO2_KEY" "$TMPDIR_LOCAL/litestream" "ubuntu@${MICRO2_HOST}:/tmp/litestream.new" \
    || fail "scp of the binary to ${MICRO2_HOST} failed"
  ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
    "sudo install -m 0755 /tmp/litestream.new ${INSTALL_PATH} && sudo rm /tmp/litestream.new" \
    || fail "installing the binary at ${INSTALL_PATH} failed"
  log "installed $(ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" "${INSTALL_PATH} version" 2>/dev/null | head -1)"
fi

# --- 2. Render /etc/litestream.yml from the template and the env file. ---
log "rendering ${CONFIG_PATH} from the template"
# The whole render runs as root: /etc/dg is mode 0750 root:dg, so the ssh
# login user cannot even traverse into it, and /var/lib/dg is 0700 dg:dg.
# Reading the values under sudo -n keeps the single copy of the secret on
# the host authoritative. The values are read with a regex-anchored parse
# rather than `eval`, so a secret containing shell metacharacters cannot
# execute or misparse; and they are never echoed -- the template's own
# ${VAR} references are left intact in the rendered file, which is what
# litestream expands from the sidecar's own environment.
ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" sudo -n bash -s <<'REMOTE'
set -euo pipefail
ENV=/etc/dg/litestream.env
CONFIG=/etc/litestream.yml
DATA=/var/lib/dg
[ -r "$ENV" ] || { echo "env not readable by root" >&2; exit 2; }

# Anchored, non-eval parse. Only the five exact keys are accepted; any
# other line in the env file is ignored, so the parse cannot be poisoned
# by an unrelated export.
KID=$(sed -n 's/^LITESTREAM_S3_ACCESS_KEY_ID=\(.*\)$/\1/p' "$ENV")
SEC=$(sed -n 's/^LITESTREAM_S3_SECRET_ACCESS_KEY=\(.*\)$/\1/p' "$ENV")
BUC=$(sed -n 's/^LITESTREAM_S3_BUCKET=\(.*\)$/\1/p' "$ENV")
END=$(sed -n 's/^LITESTREAM_S3_ENDPOINT=\(.*\)$/\1/p' "$ENV")
REG=$(sed -n 's/^LITESTREAM_S3_REGION=\(.*\)$/\1/p' "$ENV")
[ -n "$KID" ]  || { echo "LITESTREAM_S3_ACCESS_KEY_ID missing/empty" >&2; exit 2; }
[ -n "$SEC" ]  || { echo "LITESTREAM_S3_SECRET_ACCESS_KEY missing/empty" >&2; exit 2; }
[ -n "$BUC" ]  || { echo "LITESTREAM_S3_BUCKET missing/empty" >&2; exit 2; }
[ -n "$END" ]  || { echo "LITESTREAM_S3_ENDPOINT missing/empty" >&2; exit 2; }
[ -n "$REG" ]  || { echo "LITESTREAM_S3_REGION missing/empty" >&2; exit 2; }

# Live databases under the data root, skipping SQLite's -shm / -wal
# sidecar files (which travel with the main file and are not databases)
# and skipping the REGISTRY itself. metadata.sqlite is the gateway's own
# registry, owned by the running gateway and written by it in WAL mode;
# replicating a database another process is actively writing would hand
# litestream a file it must not checkpoint, and the registry is already
# protected on-host by the hourly backup set. The dollar-paren keeps this
# a glob under bash (not sh), which is why the render runs under bash.
shopt -s nullglob
DBS=("$DATA"/verify_*.sqlite "$DATA"/*.sqlite)
# Filter to the registered, live databases: ask the gateway itself, so a
# tombstoned or unregistered leftover file is never replicated. The dg
# CLI needs the owner credential (mode 0600 dg:dg) -- as the dg user.
LIVE="$(sudo -n -u dg /usr/local/bin/dg --key-file /etc/dg/owner.key list 2>/dev/null \
  | sed -n 's/.*"databases":\[\(.*\)\].*/\1/p' | tr -d '" ' | tr ',' ' ')"
[ -n "$LIVE" ] || { echo "no registered live databases (dg list returned none)" >&2; exit 3; }
KEEP=()
for name in $LIVE; do
  [ -f "$DATA/${name}.sqlite" ] && KEEP+=("$DATA/${name}.sqlite")
done
[ "${#KEEP[@]}" -gt 0 ] || { echo "none of the registered databases has a .sqlite file under $DATA" >&2; exit 3; }
DBS=("${KEEP[@]}")

# Write atomically as the dg user, mode 0600, so a partial render never
# leaves a config the sidecar could read. The rendered file keeps the
# ${...} env references rather than inlining the secret: the sidecar's
# environment is populated from /etc/dg/litestream.env by systemd at
# start, so the credential exists on disk in exactly one place.
TMP=$(mktemp /etc/litestream.yml.XXXXXX)
{
  echo "dbs:"
  for db in "${DBS[@]}"; do
    name=$(basename "$db" .sqlite)
    echo "  - path: ${db}"
    echo "    replicas:"
    echo "      - url: s3://${BUC}/${name}?endpoint=${END}"
    echo "        region: ${REG}"
    echo "        access-key-id: \${LITESTREAM_S3_ACCESS_KEY_ID}"
    echo "        secret-access-key: \${LITESTREAM_S3_SECRET_ACCESS_KEY}"
    echo "        force-path-style: true"
  done
} > "$TMP"
chmod 0600 "$TMP"
chown dg:dg "$TMP"
mv -f "$TMP" "$CONFIG"
echo "rendered ${#DBS[@]} db(s) into $CONFIG" >&2
REMOTE

# --- 3. Install the systemd unit. ---
log "copying deploy/dg-litestream.service to /etc/systemd/system/"
# scp resolves the ssh alias's hostname but does not inherit the
# IdentityFile the way plain ssh does, so name the key explicitly.
MICRO2_HOST=$(ssh -G "$TARGET" 2>/dev/null | awk '/^hostname /{print $2; exit}')
MICRO2_KEY="${MICRO2_SSH_KEY:-$HOME/.ssh/micro2.key}"
[ -r "$MICRO2_KEY" ] || fail "cannot read the ssh key $MICRO2_KEY (set MICRO2_SSH_KEY to your micro2 key)"
scp -o BatchMode=yes -q -i "$MICRO2_KEY" \
  "$ROOT/deploy/dg-litestream.service" "ubuntu@${MICRO2_HOST}:/tmp/dg-litestream.service" \
  || fail "scp of the unit to ${MICRO2_HOST} failed"
ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
  "sudo install -m 0644 /tmp/dg-litestream.service /etc/systemd/system/dg-litestream.service && sudo rm /tmp/dg-litestream.service && sudo systemctl daemon-reload"
  || fail "installing the unit on ${TARGET} failed"
log "unit installed and daemon-reloaded"

# --- 4. Verify (do not start). The operator chooses when the sidecar
# first starts so this script never auto-starts a service that touches
# the network. ---
log "install complete. To start: ssh $TARGET 'sudo systemctl enable --now dg-litestream'"
log "verify with: ssh $TARGET 'sudo systemctl status dg-litestream --no-pager; sudo journalctl -u dg-litestream -n 50 --no-pager'"
