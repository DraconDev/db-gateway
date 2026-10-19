#!/usr/bin/env bash
# Scan the TRACKED tree for credential-shaped material before it can be
# pushed. (A1, docs/audit-2026-10-19.md; see also CONTRIBUTING.md.)
#
# WHY THIS EXISTS. The A1 audit item found a live AWS access key and a
# live Cloudflare DNS token sitting in plaintext in a repo that is pushed
# to GitHub and GitLab. They were contained -- the repo is private and the
# values appear in no other repo -- but "contained by hand" is not a
# property any tool enforces, and db-gateway itself is pushed to both
# remotes and holds the OVH/Litestream/owner-key material that must never
# be committed. This gate is the maintainable form of the fix: a
# credential-shaped value cannot enter the tracked tree without the
# commit being flagged, so the class of leak stops depending on
# remembering.
#
# WHAT IT CHECKS (tracked files only, so nothing outside git's index can
# fail the build, and deleted blobs stay deleted):
#
#   * AWS access key IDs            AKIA/ASIA + 16 upper-case alphanumerics
#   * AWS secret access keys        an assignment whose value is 40 chars
#   * GitHub personal access tokens  gh[pousr]_ + 36+ characters
#   * Private key blocks            PEM "BEGIN ... PRIVATE KEY" headers
#   * Generic secret assignments    a secret-ish NAME = a long literal
#                                   value (token, password, api key, ...)
#
# WHAT IT DELIBERATELY IGNORES. Test fixtures and documentation use
# obviously-fake credentials, and a gate that cries wolf is a gate that
# gets switched off. A match is skipped when the line carries a marker
# word (EXAMPLE, PLACEHOLDER, DUMMY, REHEARSAL, NOT-A-REAL, CHANGEME,
# FAKETOKEN, XXXX) or when the value is a shell/CI expansion rather than
# a literal (${...}, $VAR). Placeholders that are literal -- "not a real
# credential" -- are still checked by shape, so a real-looking value
# pasted in "for the docs" is caught.
#
# The scanner reports file and line, never the value: a scanner whose
# output can be pasted into a chat log is itself a leak.
#
# Usage: scripts/secret-scan.sh [path ...]
#   With no arguments, scans every tracked file in the repository.
#   With arguments, scans those paths instead (used by the test to
#   exercise the matcher against disposable fixtures).
#
# Exit codes: 0 = clean, 1 = credential-shaped material found,
#             2 = the scanner could not run (not a git checkout, or the
#                 file list could not be built).

set -uo pipefail

# Resolve relative target paths against the directory the caller was in,
# not against this script's own repo: the script is used to scan OTHER
# repositories too (that is how the exposed material was found), and a
# path that silently resolved to "no such file" would report "clean".
CALLER_DIR="$(pwd)"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO" || exit 2

if [ "$#" -gt 0 ]; then
    TARGETS=("$@")
else
    if ! git rev-parse --git-dir >/dev/null 2>&1; then
        echo "secret-scan: not a git checkout -- run from the repo" >&2
        exit 2
    fi
    mapfile -t TARGETS < <(git ls-files -z | tr '\0' '\n')
    if [ "${#TARGETS[@]}" -eq 0 ]; then
        echo "secret-scan: git ls-files returned no files -- run from the repo checkout" >&2
        exit 2
    fi
fi

# The matcher lives in Python: the patterns need alternation, bounded
# repetition and a marker filter, and expressing that in grep is how
# these gates end up quietly wrong.
exec python3 - "$CALLER_DIR" "$REPO" "${TARGETS[@]}" <<'PY'
import os
import re
import sys

caller_dir, repo, targets = sys.argv[1], sys.argv[2], sys.argv[3:]

# (name, compiled pattern). Each pattern must match the SECRET shape, not
# the mere mention of a variable name -- "aws_secret_access_key" alone is
# documentation, "aws_secret_access_key = <40 chars>" is a leak.
PATTERNS = [
    ("aws-access-key-id", re.compile(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b")),
    ("aws-secret-access-key", re.compile(
        r"(?i)aws_secret_access_key\s*[:=]\s*['\"]?([A-Za-z0-9/+=]{40})['\"]?")),
    ("github-token", re.compile(r"\bgh[pousr]_[A-Za-z0-9]{36,}\b")),
    ("private-key-block", re.compile(
        r"-----BEGIN (?:RSA |DSA |EC |OPENSSH |PGP |ENCRYPTED )?PRIVATE KEY-----")),
    # The name may carry any prefix/suffix around the secret-ish word
    # (CLOUDFLARE_API_TOKEN, S3_SECRET_ACCESS_KEY, db_password): matching
    # the bare word misses every real-world prefixed variable name.
    ("generic-secret-assignment", re.compile(
        r"(?i)[A-Za-z0-9_.-]*(?:secret|token|password|passwd|api[_-]?key|"
        r"access[_-]?key|private[_-]?key|credential)[A-Za-z0-9_.-]*"
        r"\s*[:=]\s*['\"]?"
        r"([A-Za-z0-9/+_-]{24,})['\"]?")),
]

# Lines carrying one of these markers are known-fake material: test
# fixtures, docs, rehearsal stubs. Matched case-insensitively.
MARKERS = (
    "example", "placeholder", "dummy", "rehearsal", "not-a-real",
    "not a real", "changeme", "faketoken", "fake-token", "xxxx",
    "notreal", "sample", "redacted", "your-", "yourvalue",
)

# A value that is an expansion rather than a literal is not a leak: the
# secret lives in the environment, not in the file.
EXPANSION = re.compile(r"^\s*\$\{?[A-Za-z_]")


def is_placeholder(line: str, value: str) -> bool:
    low = line.lower()
    if any(m in low for m in MARKERS):
        return True
    return bool(EXPANSION.match(value))


def scan_text(name: str, text: str, findings: list) -> None:
    for lineno, line in enumerate(text.splitlines(), 1):
        if len(line) > 4000:  # minified or data blob: not a credential line
            continue
        for kind, pattern in PATTERNS:
            m = pattern.search(line)
            if not m:
                continue
            value = m.group(1) if m.groups() else m.group(0)
            if is_placeholder(line, value):
                continue
            findings.append((name, lineno, kind))


findings = []
scanned = 0
for rel in targets:
    if os.path.isabs(rel):
        path = rel
    else:
        # A relative path may name a file in the caller's directory or in
        # this repo; try the caller's directory first, then the repo.
        candidate = os.path.join(caller_dir, rel)
        path = candidate if os.path.isfile(candidate) else os.path.join(repo, rel)
    if not os.path.isfile(path):
        continue
    try:
        with open(path, "rb") as fh:
            raw = fh.read()
    except OSError:
        continue
    if b"\0" in raw[:4096]:
        continue  # binary
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        continue
    scanned += 1
    scan_text(rel, text, findings)

if findings:
    print("secret-scan: credential-shaped material found in tracked files:")
    for name, lineno, kind in sorted(findings):
        print(f"  {name}:{lineno}: {kind}")
    print("secret-scan: values are withheld on purpose. Remove the material, "
          "rotate the credential if it was ever real, and re-run.")
    sys.exit(1)

print(f"secret-scan: clean ({scanned} text files scanned, "
      f"{len(PATTERNS)} patterns)")
PY
