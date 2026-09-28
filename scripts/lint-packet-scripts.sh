#!/usr/bin/env bash
# Lint the packet scripts for the two failure modes `bash -n` cannot see.
#
#   1. A quoted argument that ends a line without a backslash, immediately
#      followed by another quoted argument. In bash that is not a continuation:
#      the command ENDS there and the next line is executed as a command of its
#      own. A fragment of an English sentence then becomes
#      `week-aged bucket on OVH; ...: command not found` and the packet dies
#      partway through with exit 127 -- after the gates before it have already
#      run and been recorded. `bash -n` passes: every line is a complete
#      command on its own.
#
#   2. An environment variable that is read but never assigned in the file and
#      never defaulted with ${X:-} / ${X:?}. Under `set -u` that aborts the
#      packet the first time it is read, which for an optional knob is after
#      several gates have already been recorded.
#
# Both shipped in this packet. Both were found by RUNNING it, which is the
# argument for a check rather than for care.
#
# Run: scripts/lint-packet-scripts.sh

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SHELL_SCRIPTS=(
    "$REPO/scripts/sandbox-packet.sh"
    "$REPO/scripts/sandbox-s3-gates.sh"
    "$REPO/scripts/s3-rehearsal-endpoint.sh"
    "$REPO/scripts/rehearse-sandbox-packet.sh"
    "$REPO/scripts/lint-packet-scripts.sh"
)
PY_FILES=("$REPO/scripts/s3probe.py" "$REPO/scripts/s3-log-proxy.py")

# Variables the parent script documents as exported, or the shell always has.
PROVIDED='^(BASH_SOURCE|PATH|HOME|TMPDIR|PWD|USER|SHELL|PIPESTATUS|LINENO|RANDOM|SECONDS|IFS|PYTHONPATH|LANG|TERM|LC_[A-Z_]+|DG_SANDBOX_[A-Z0-9_]+|DG_REHEARSAL_[A-Z0-9_]+|AWS_[A-Z0-9_]+|SSL_CERT_FILE|DG_REAL_HOME|DG_BIN)$'

problems=0

for f in "${SHELL_SCRIPTS[@]}" "${PY_FILES[@]}"; do
    [ -f "$f" ] || { printf '  missing: %s\n' "${f#"$REPO"/}" >&2; problems=$((problems + 1)); continue; }
    rel="${f#"$REPO"/}"
    case "$f" in
        *.py)
            if ! python3 -m py_compile "$f" 2>/tmp/lint.err; then
                printf '  %s: python syntax error\n' "$rel" >&2
                head -4 /tmp/lint.err | sed 's/^/    /' >&2
                problems=$((problems + 1))
            fi
            continue;;
    esac
    if ! err="$(bash -n "$f" 2>&1)"; then
        printf '  %s: bash -n failed\n' "$rel" >&2
        printf '%s\n' "$err" | head -3 | sed 's/^/    /' >&2
        problems=$((problems + 1))
        continue
    fi
    if ! python3 - "$f" "$rel" "$PROVIDED" <<'PY'
import re, sys

path, rel, provided = sys.argv[1], sys.argv[2], re.compile(sys.argv[3])
text = open(path, encoding="utf-8").read()
lines = text.splitlines()
problems = 0

# ---- (1) a quoted argument that ends a line with no backslash --------------
# Only a `case` arm legitimately puts a quoted word at the start of a line with
# nothing joining it to the line above, and those lines end in `)` or `;;`.
def is_case_boundary(line: str) -> bool:
    s = line.rstrip()
    return s.endswith(";;") or s.endswith(")") or s.endswith("esac")

for i, line in enumerate(lines[:-1]):
    stripped = line.rstrip()
    if not stripped or stripped.endswith("\\") or is_case_boundary(stripped):
        continue
    if stripped.count('"') % 2:          # genuinely unterminated: bash -n's business
        continue
    nxt = lines[i + 1]
    lead = nxt.lstrip()
    if lead.startswith('"') and not lead.startswith('"""') and not is_case_boundary(nxt.rstrip()):
        print(f"  {rel}:{i + 1}: this line ends a quoted argument with no backslash,")
        print(f"    so the next line runs as its own command: {lead[:70]}")
        problems += 1

# ---- (2) environment reads with no assignment and no default ---------------
assigned = set()
# NAME=, export NAME=, local NAME=, declare -x NAME=, and bare `local NAME`
for m in re.finditer(r"^\s*(?:export\s+|local\s+|declare\s+(?:-\w+\s+)?|readonly\s+)?([A-Za-z_]\w*)\s*=", text, re.M):
    assigned.add(m.group(1))
for m in re.finditer(r"^\s*local\s+((?:[A-Za-z_]\w*\s+)+)$", text, re.M):
    assigned.update(m.group(1).split())
for m in re.finditer(r"^\s*read\s+(?:-[a-zA-Z]+\s+)*((?:[A-Za-z_]\w*\s+)+)", text, re.M):
    assigned.update(m.group(1).split())
for m in re.finditer(r"\bfor\s+([A-Za-z_]\w*)\s+in\b", text):
    assigned.add(m.group(1))
# Variables read through a guard are never a set -u problem.
guarded = set(re.findall(r"\$\{(\w*)[:?+-][^}]*\}", text))

seen = set()
for i, line in enumerate(lines, 1):
    if line.lstrip().startswith("#"):
        continue
    body = line.split("  #", 1)[0]
    for name in re.findall(r"\$\{?([A-Za-z_]\w*)\}?", body):
        if not name or name in assigned or name in guarded or provided.match(name):
            continue
        if (name, i) in seen:
            continue
        seen.add((name, i))
        print(f"  {rel}:{i}: ${name} is read but never assigned or defaulted in this file;")
        print(f"    under `set -u` it aborts the packet when first read: {line.strip()[:72]}")
        problems += 1

sys.exit(1 if problems else 0)
PY
    then
        problems=$((problems + 1))
    fi
done

if [ "$problems" -eq 0 ]; then
    echo "lint-packet-scripts: clean (${#SHELL_SCRIPTS[@]} shell + ${#PY_FILES[@]} python: syntax, line continuations, unguarded env reads)"
    exit 0
fi
printf 'lint-packet-scripts: %d problem group(s)\n' "$problems" >&2
exit 1
