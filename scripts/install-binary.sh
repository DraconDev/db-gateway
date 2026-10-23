#!/usr/bin/env bash
# Install a dg binary over a path that may be running right now.
#
# Usage: scripts/install-binary.sh SOURCE DEST
#
# A running `dg` holds its own binary open for execution, and writing to
# that inode fails with ETXTBSY -- the bare "Text file busy" that stopped an
# install while the operator's own view was on screen. The failure is not
# about the copy: it is about writing to a file the kernel is executing.
#
# Unlinking the path does not disturb the running process. The kernel keeps
# the unlinked inode alive for every process still executing it, until that
# process exits. So the new binary is staged beside the old one, the old
# path is unlinked, and the new one is renamed into place. The view that is
# running keeps working on the old code -- uninterrupted -- and the next
# launch gets the new binary.
#
# rename(2) over a running executable fails with ETXTBSY too, which is why
# the old path is removed first rather than overwritten. The window between
# the unlink and the rename is a single rename(2) call wide, and it leaves
# no partial binary at the path: the staged file is complete before the old
# path is touched.
#
# Exits non-zero if the binary could not be installed. On failure the
# destination is left exactly as it was, unless the failure happened in the
# narrow window after the unlink, in which case the staged copy is removed
# and the destination is absent -- never a truncated or half-written binary.

set -euo pipefail

fail() { echo "[install] FAIL: $*" >&2; exit 1; }

if [ $# -ne 2 ]; then
    fail "usage: install-binary.sh SOURCE DEST"
fi

SOURCE="$1"
DEST="$2"

[ -x "$SOURCE" ] || fail "source binary is missing or not executable: $SOURCE"

dest_dir="$(dirname "$DEST")"
mkdir -p "$dest_dir"

staged="$dest_dir/.dg.install.$$"
cleanup() { rm -f "$staged"; }
trap cleanup EXIT

cp "$SOURCE" "$staged" || fail "could not stage $SOURCE beside $DEST"
chmod 755 "$staged"

rm -f "$DEST"
mv "$staged" "$DEST" || fail "could not move the staged binary into place at $DEST"

echo "[install] installed $DEST"
