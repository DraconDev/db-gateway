#!/usr/bin/env bash
# Lint the packet scripts for the two failure modes that `bash -n` cannot see.
#
#   1. A missing line continuation inside a multi-line command. The command
#      before the break runs, and the next line is executed as a command of its
#      own -- so a quoted sentence becomes `some-words: command not found` and
#      the packet dies partway through with exit 127. `bash -n` passes, because
#      each line is a syntactically complete command on its own.
#
#   2. An environment variable read that is neither assigned in the file nor
#      defaulted with ${X:-} / ${X:?}. Under `set -u` that aborts the packet
#      at the moment it is first read, which for an optional knob is *after*
#      the gates before it have already run and been recorded.
#
# Both of these shipped in this packet and both were found by running it, not
# by reading it. That is the argument for a check instead of care.
#
# Run: scripts/lint-packet-scripts.sh
# Exits nonzero if anything is reported.

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPTS=(
    "$REPO/scripts/sandbox-packet.sh"
    "$REPO/scripts/sandbox-s3-gates.sh"
    "$REPO/scripts/s3-rehearsal-endpoint.sh"
    "$REPO/scripts/rehearse-sandbox-packet.sh"
    "$REPO/scripts/s3probe.py"
    "$REPO/scripts/s3-log-proxy.py"
)
# Variables the parent script is documented to export, or that the shell
# always provides. Anything else must be defaulted in the file that reads it.
PROVIDED='^(BASH_SOURCE|PATH|HOME|TMPDIR|PWD|USER|SHELL|PIPESTATUS|LINENO|RANDOM|SECONDS|IFS|PYTHONPATH|LANG|TERM|DG_SANDBOX_[A-Z0-9_]+|DG_REHEARSAL_[A-Z0-9_]+|AWS_[A-Z0-9_]+|SSL_CERT_FILE|DG_REAL_HOME|DG_BIN|DG_SANDBOX_DIR)$'

problems=0
note() { printf '  %s\n' "$1"; problems=$((problems + 1)); }

for f in "${SCRIPTS[@]}"; do
    [ -f "$f" ] || { note "missing: ${f#"$REPO"/}"; continue; }
    rel="${f#"$REPO"/}"
    case "$f" in
        *.py) python3 -m py_compile "$f" 2>&1 | head -5
              python3 -m py_compile "$f" >/dev/null 2>&1 || note "python syntax error in $rel"
              continue;;
    esac
    if ! bash -n "$f" 2>/dev/null; then
        note "$rel: bash -n failed"
        bash -n "$f" 2>&1 | head -3 | while read -r line; do printf '    %s\n' "$line"; done
        continue
    fi

    # (1) A line that ends inside a quoted argument but not with a backslash,
    #     where the NEXT line is obviously a continuation of the same sentence:
    #     it starts with whitespace + a double quote and the previous line does
    #     not end in a backslash. That is the exact shape of the bug.
    python3 - "$f" "$rel" <<'PY'
import sys
path, rel = sys.argv[1], sys.argv[2]
lines = open(path, encoding="utf-8").read().splitlines()
bad = []
for i, line in enumerate(lines[:-1]):
    stripped = line.rstrip()
    nxt = lines[i + 1]
    if not stripped or stripped.endswith("\\"):
        continue
    # unbalanced double quote on this line
    if stripped.count('"') % 2:
        continue
    # next line looks like a continued quoted argument
    if nxt.startswith('    "') or nxt.startswith('\t"'):
        bad.append((i + 1, stripped[-60:], nxt.strip()[:60]))
for lineno, prev, nxt in bad:
    print(f"  {rel}:{lineno}: a quoted argument ends here but the next line continues the same sentence without a backslash")
    print(f"    ...{prev}")
    print(f"    {nxt}")
sys.exit(1 if bad else 0)
PY
    [ $? -ne 0 ] && problems=$((problems + 1))

    # (2) unguarded environment reads
    python3 - "$f" "$rel" "$PROVIDED" <<'PY'
import re, sys
path, rel, provided = sys.argv[1], sys.argv[2], re.compile(sys.argv[3])
text = open(path, encoding="utf-8").read()
lines = text.splitlines()

assigned = set()
for m in re.finditer(r"^\s*(?:export\s+|local\s+|declare\s+\w+\s*=\s*)?([A-Za-z_][A-Za-z0-9_]*)=", text, re.M):
    assigned.add(m.group(1))
for m in re.finditer(r"^\s*read\s+(?:-[a-zA-Z]+\s+)*([A-Za-z_][A-Za-z0-9_]*)", text, re.M):
    assigned.add(m.group(1))
for m in re.finditer(r"for\s+([A-Za-z_][A-Za-z0-9_]*)\s+in", text):
    assigned.add(m.group(1))

# every guarded read: ${X:-...}, ${X:?...}, ${X+...}
guarded = set(re.findall(r"\$\{([A-Za-z_][A-Za-z0-9_]*)[:?+-]", text))

bad = []
for i, line in enumerate(lines, 1):
    if line.lstrip().startswith("#"):
        continue
    for name in re.findall(r"\$\{?([A-Za-z_][A-Za-z0-9_]*)\}?", line):
        if name in assigned or name in guarded or provided.match(name):
            continue
        if re.search(rf"\$\{{{name}([}}:]|$)", line) or re.search(rf"\${name}\b", line):
            bad.append((i, name, line.strip()[:80]))
seen = set()
for lineno, name, line in bad:
    if (name, lineno) in seen:
        continue
    seen.add((name, lineno))
    print(f"  {rel}:{lineno}: ${name} is read but never assigned or defaulted here; under `set -u` this aborts the packet")
    print(f"    {line}")
sys.exit(1 if bad else 0)
PY
    [ $? -ne 0 ] && problems=$((problems + 1))
done

if [ "$problems" -eq 0 ]; then
    echo "lint-packet-scripts: clean (${#SCRIPTS[@]} files: syntax, line continuations, unguarded env reads)"
    exit 0
fi
echo "lint-packet-scripts: $problems problem(s)" >&2
exit 1
