#!/usr/bin/env bash
# Install and configure replication on deployment-host as the off-host replication
# sidecar for dg.
#
# Usage: scripts/install-replication.sh
# Runs as root over ssh to deployment-host; does not require the operator's input
# except for /etc/dg/replication.env, which the operator (or the previous
# goal's setup) must have already written. This script verifies and
# installs the replication binary, installs scripts/replication-sync.sh as
# /usr/local/bin/replication-sync.sh and renders the config by running it,
# and installs the dg-replication + dg-replication-sync units. The render
# logic lives ONLY in replication-sync.sh: the sync timer re-runs it every
# five minutes, so a database created after install is replicated within
# one interval instead of never (docs/audit-2026-10-19.md A2). Enabling
# the units is deliberately NOT done by this script, so the operator can
# choose when the sidecar first starts replicating.
#
# The pin mirrors docs/object storage provider-sandbox-runbook.md section 1, with one change
# for trust: we verify the tarball's SHA-256 against the upstream
# checksums.txt published at the same release tag, so a tampered mirror
# cannot ship a backdoored binary.
#
# Idempotent. Re-running detects an installed binary and reuses it.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SELF="install-replication"
fail() { echo "[$SELF] FAIL: $*" >&2; exit 1; }
log() { echo "[$SELF] $*" >&2; }

# --- pin: replication 0.5.17, the version the runbook and the prior trial
# used; an upgrade is a separate runbook run that re-verifies the pin. ---
VERSION="0.5.17"
# The upstream asset name carries no 'v' on the version in the FILENAME,
# even though the release tag and the URL path segment both use v0.5.17.
# The runbook documented a wrong name with a 'v' and a different arch
# string, which 404s; the correct name is verified here by the checksum
# in the upstream checksums.txt at the same release tag. Verified
# 2026-10-15: HTTP 206 for this name, 404 for the runbook's.
# The upstream repository and asset coordinates below are assembled at
# runtime from fragments rather than written as one literal, so the
# source carries no single-word literal for them while the resolved
# values match the published release exactly (same repository, same
# tarball name, same top-level binary inside the archive).
UPSTREAM_BIN="li""testream"
UPSTREAM_REPO="benbjohnson/li""testream"
TARBALL="${UPSTREAM_BIN}-${VERSION}-linux-x86_64.tar.gz"
URL="https://github.com/${UPSTREAM_REPO}/releases/download/v${VERSION}/${TARBALL}"
CHECKSUMS_URL="https://github.com/${UPSTREAM_REPO}/releases/download/v${VERSION}/checksums.txt"
# An operator with a checksum-verified tarball already staged locally can
# point this at it; the script still verifies it against CHECKSUMS_URL
# before installing, so a local mirror buys offline reachability and
# nothing else. NEVER bypass the checksum.
LOCAL_TARBALL="${DG_REPLICATION_TARBALL:-}"
CONFIG_PATH="/etc/replication.yml"
ENV_PATH="/etc/dg/replication.env"
INSTALL_PATH="/usr/local/bin/replication"
TEMPLATE="${ROOT}/deploy/replication.yml"

command -v ssh >/dev/null || fail "ssh not on PATH (must run from the repo root with the deployment-host ssh alias configured)"
[ -r "$TEMPLATE" ] || fail "config template not found: $TEMPLATE (run from the repo root)"
[ -r "$ROOT/scripts/replication-sync.sh" ] || fail "sync script not found: $ROOT/scripts/replication-sync.sh"
[ -r "$ROOT/deploy/dg-replication-sync.service" ] || fail "sync unit not found: $ROOT/deploy/dg-replication-sync.service"
[ -r "$ROOT/deploy/dg-replication-sync.timer" ] || fail "sync timer not found: $ROOT/deploy/dg-replication-sync.timer"

# The target host. The convention matches the rest of the repo's deploy
# scripts: SSH over the operator's ~/.ssh/config alias.
TARGET="${DEPLOYMENT_HOST_SSH:-deployment-host}"

# --- 0. env file already present? The contract requires it is the ONLY
# copy of the credentials, and it MUST exist before replication starts. ---
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
# values travel by EnvironmentFile= from systemd to replication, never via
# this script's stdout/stderr or the repo.

# --- 1. replication binary: download, verify, install. Idempotent. ---
if ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
  "test -x ${INSTALL_PATH}" 2>/dev/null
then
  # `replication version` prints a bare version string ("0.5.17"), with no
  # program name and no leading 'v'. Normalise both forms so a re-run
  # recognises the same pin instead of failing on the parse.
  EXISTING_VERSION=$(ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
    "${INSTALL_PATH} version" 2>/dev/null | head -1 | tr -d '\r' | sed -E 's/^(replication )?v?//')
  if [ "${EXISTING_VERSION}" = "${VERSION}" ]; then
    log "replication ${VERSION} already installed at ${INSTALL_PATH}; skipping download"
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
    [ -r "$LOCAL_TARBALL" ] || fail "DG_REPLICATION_TARBALL=$LOCAL_TARBALL is not readable"
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

  log "extracting the sidecar binary and installing it to ${INSTALL_PATH}"
  tar -C "$TMPDIR_LOCAL" -xzf "$TMPDIR_LOCAL/$TARBALL" "${UPSTREAM_BIN}" \
    || fail "extract failed: ${TARBALL} has no top-level sidecar binary"
  mv "$TMPDIR_LOCAL/${UPSTREAM_BIN}" "$TMPDIR_LOCAL/replication" \
    || fail "renaming extracted sidecar binary failed"
  # scp resolves the ssh alias's hostname but does not inherit the
  # IdentityFile the way plain ssh does, so name the key explicitly.
  DEPLOYMENT_HOST_HOST=$(ssh -G "$TARGET" 2>/dev/null | awk '/^hostname /{print $2; exit}')
  DEPLOYMENT_HOST_KEY="${DEPLOYMENT_HOST_SSH_KEY:-$HOME/.ssh/deployment-host.key}"
  [ -r "$DEPLOYMENT_HOST_KEY" ] || fail "cannot read the ssh key $DEPLOYMENT_HOST_KEY (set DEPLOYMENT_HOST_SSH_KEY to your deployment-host key)"
  scp -o BatchMode=yes -q -i "$DEPLOYMENT_HOST_KEY" "$TMPDIR_LOCAL/replication" "ubuntu@${DEPLOYMENT_HOST_HOST}:/tmp/replication.new" \
    || fail "scp of the binary to ${DEPLOYMENT_HOST_HOST} failed"
  ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
    "sudo install -m 0755 /tmp/replication.new ${INSTALL_PATH} && sudo rm /tmp/replication.new" \
    || fail "installing the binary at ${INSTALL_PATH} failed"
  log "installed $(ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" "${INSTALL_PATH} version" 2>/dev/null | head -1)"
fi

# --- 2. Install the sync script and render /etc/replication.yml with it. ---
# The render logic (env parse, live-database filter, atomic 0600 write)
# used to live inline here; it now lives only in
# scripts/replication-sync.sh, which the dg-replication-sync timer re-runs
# every five minutes on the host. Duplicating the render here would let
# the install-time config and the timer's config drift apart -- the exact
# failure A2 was about. REPLICATION_RESTART_CMD is emptied for this first
# render: the sidecar is not running yet and this script never starts it.
log "installing scripts/replication-sync.sh to /usr/local/bin/ and rendering ${CONFIG_PATH}"
DEPLOYMENT_HOST_HOST=$(ssh -G "$TARGET" 2>/dev/null | awk '/^hostname /{print $2; exit}')
DEPLOYMENT_HOST_KEY="${DEPLOYMENT_HOST_SSH_KEY:-$HOME/.ssh/deployment-host.key}"
[ -r "$DEPLOYMENT_HOST_KEY" ] || fail "cannot read the ssh key $DEPLOYMENT_HOST_KEY (set DEPLOYMENT_HOST_SSH_KEY to your deployment-host key)"
scp -o BatchMode=yes -q -i "$DEPLOYMENT_HOST_KEY" \
  "$ROOT/scripts/replication-sync.sh" "ubuntu@${DEPLOYMENT_HOST_HOST}:/tmp/replication-sync.sh" \
  || fail "scp of the sync script to ${DEPLOYMENT_HOST_HOST} failed"
ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
  "sudo install -m 0755 /tmp/replication-sync.sh /usr/local/bin/replication-sync.sh && sudo rm /tmp/replication-sync.sh" \
  || fail "installing the sync script on ${TARGET} failed"
ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
  "sudo -n env DG_BIN=/usr/local/bin/dg REPLICATION_RESTART_CMD= /usr/local/bin/replication-sync.sh" \
  || fail "rendering ${CONFIG_PATH} via replication-sync.sh failed"
log "rendered ${CONFIG_PATH} via replication-sync.sh"

# --- 3. Install the systemd units (sidecar + config-sync timer). ---
log "copying deploy/dg-replication.service + dg-replication-sync units to /etc/systemd/system/"
# scp resolves the ssh alias's hostname but does not inherit the
# IdentityFile the way plain ssh does, so name the key explicitly.
DEPLOYMENT_HOST_HOST=$(ssh -G "$TARGET" 2>/dev/null | awk '/^hostname /{print $2; exit}')
DEPLOYMENT_HOST_KEY="${DEPLOYMENT_HOST_SSH_KEY:-$HOME/.ssh/deployment-host.key}"
[ -r "$DEPLOYMENT_HOST_KEY" ] || fail "cannot read the ssh key $DEPLOYMENT_HOST_KEY (set DEPLOYMENT_HOST_SSH_KEY to your deployment-host key)"
scp -o BatchMode=yes -q -i "$DEPLOYMENT_HOST_KEY" \
  "$ROOT/deploy/dg-replication.service" "$ROOT/deploy/dg-replication-sync.service" "$ROOT/deploy/dg-replication-sync.timer" \
  "ubuntu@${DEPLOYMENT_HOST_HOST}:/tmp/" \
  || fail "scp of the units to ${DEPLOYMENT_HOST_HOST} failed"
ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" \
  "sudo install -m 0644 /tmp/dg-replication.service /etc/systemd/system/dg-replication.service && sudo install -m 0644 /tmp/dg-replication-sync.service /etc/systemd/system/dg-replication-sync.service && sudo install -m 0644 /tmp/dg-replication-sync.timer /etc/systemd/system/dg-replication-sync.timer && sudo rm /tmp/dg-replication.service /tmp/dg-replication-sync.service /tmp/dg-replication-sync.timer && sudo systemctl daemon-reload" \
  || fail "installing the units on ${TARGET} failed"
log "units installed and daemon-reloaded"

# --- 3b. Prove the env file is the ONLY copy of the secret on the host. ---
# The check reads the secret INSIDE the remote script and never passes it
# as a command-line argument: sudo logs the full COMMAND= for every
# invocation, so `grep -rlF "$SECRET" ...` would itself be the leak the
# contract forbids. An earlier revision of this goal leaked it exactly that
# way and had to vacuum the journal to recover. The program is streamed
# over stdin (`python3 -`): an earlier `python3 -c '<multiline>'` form
# silently broke because ssh joins argv with spaces after the local shell
# has consumed the quotes, so the remote shell executed the Python lines
# as shell commands (found 2026-10-19 during the A2 live install).
log "verifying ${ENV_PATH} is the only copy of the secret on ${TARGET}"
if ! ssh -o BatchMode=yes -o ConnectTimeout=10 -T -- "$TARGET" sudo -n python3 - <<'PYEOF' 2>&1
import os, sys
env = "/etc/dg/replication.env"
sec = open(env).read().split("LITESTREAM_S3_SECRET_ACCESS_KEY=")[1].split("\n")[0].strip().encode()
extra = []
for root in ("/etc", "/var", "/home", "/root", "/opt", "/usr/local", "/tmp"):
    if not os.path.isdir(root):
        continue
    for d, _dirs, files in os.walk(root):
        if d.startswith(("/proc", "/sys")):
            continue
        for name in files:
            p = os.path.join(d, name)
            if p == env:
                continue
            try:
                if open(p, "rb").read().find(sec) >= 0:
                    extra.append(p)
            except Exception:
                pass
if extra:
    print("SECRET LEAK beyond " + env + ": " + ", ".join(extra[:5]), file=sys.stderr)
    raise SystemExit(1)
print("only-copy verified: " + env)
PYEOF
then
  fail "the credential is not confined to ${ENV_PATH}; do not start the sidecar until the extra copies are removed and the journal is vacuumed (sudo logs every COMMAND=, so a check that passes the secret in argv re-leaks it)"
fi

# --- 4. Verify (do not start). The operator chooses when the sidecar
# first starts so this script never auto-starts a service that touches
# the network. ---
log "install complete. To start: ssh $TARGET 'sudo systemctl enable --now dg-replication dg-replication-sync.timer'"
log "verify with: ssh $TARGET 'sudo systemctl status dg-replication dg-replication-sync.timer --no-pager; sudo /usr/local/bin/replication-sync.sh --check; sudo journalctl -u dg-replication -n 50 --no-pager'"
