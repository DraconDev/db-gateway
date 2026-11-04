#!/usr/bin/env bash
# Lint the packet scripts for the two failure modes `bash -n` cannot see.
#
#   1. A quoted argument that ends a line without a backslash, immediately
#      followed by another quoted argument. In bash that is not a continuation:
#      the command ENDS there and the next line is executed as a command of its
#      own. A fragment of an English sentence then becomes
#      `week-aged bucket on object storage provider; ...: command not found` and the packet dies
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
PROVIDED='^(BASH_SOURCE|PATH|HOME|TMPDIR|PWD|USER|SHELL|PPID|BASH|BASH_VERSION|PIPESTATUS|LINENO|RANDOM|SECONDS|IFS|PYTHONPATH|LANG|TERM|LC_[A-Z_]+|DG_SANDBOX_[A-Z0-9_]+|DG_REHEARSAL_[A-Z0-9_]+|AWS_[A-Z0-9_]+|SSL_CERT_FILE|DG_REAL_HOME|DG_BIN)$'

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
# The signature of the bug is precise: the next line is NOTHING BUT quoted
# strings -- a bare argument continuation whose backslash is missing. A line
# that starts with a quoted path and then has command words on it is an
# ordinary new command and is fine. `case` arms are excluded by shape.
BARE_ARGS = re.compile(r'^(?:"(?:[^"\\]|\\.)*"\s*)+$')


def array_literal_lines() -> set:
    """Line numbers inside a multi-line `NAME=( ... )` array literal.

    A bare quoted string is a legitimate element of one of these, which is the
    only other place the shape appears. The state has to be tracked: the opener
    is the line BEFORE the first element.
    """
    inside: set = set()
    open_at = None
    for i, line in enumerate(lines, 1):
        s = line.rstrip()
        if open_at is None:
            if re.search(r"=\($", s) or s == "(":
                open_at = i
                inside.add(i)
        else:
            inside.add(i)
            if s == ")":
                open_at = None
    return inside


ARRAY_LINES = array_literal_lines()


def is_case_boundary(line: str) -> bool:
    s = line.rstrip()
    return s.endswith(";;") or s.endswith(")") or s.endswith("esac")


def in_heredoc(lineno: int) -> bool:
    """True when the 1-based line is inside a shell heredoc.

    The packet scripts embed Python this way, and a Python line is not shell:
    flagging its quoting produces nothing but noise.
    """
    tag = None
    for i, line in enumerate(lines, 1):
        if tag is None:
            m = re.search(r"<<-?\s*'?\`?\"?([A-Za-z_]\w*)\`?\"?'?", line)
            if m and m.group(1) not in ("", "-"):
                tag = m.group(1)
        elif line.strip() == tag:
            tag = None
        elif i == lineno:
            return True
    return False

for i, line in enumerate(lines[:-1]):
    stripped = line.rstrip()
    if not stripped or stripped.endswith("\\") or is_case_boundary(stripped):
        continue
    if stripped.count('"') % 2:          # genuinely unterminated: bash -n's business
        continue
    nxt = lines[i + 1]
    if not BARE_ARGS.match(nxt.strip()) or is_case_boundary(nxt.rstrip()):
        continue
    # A bare quoted string is a legitimate element of an array literal
    # (`ARR=( "a" "b" )` written one per line), which is the only other place
    # this shape appears. The state has to be tracked, not inferred from the
    # line at hand: the opener is the line BEFORE the first element.
    if (i + 1) in ARRAY_LINES or (i + 2) in ARRAY_LINES:
        continue
    if in_heredoc(i + 1):
        continue
    print(f"  {rel}:{i + 1}: this line ends a command with no backslash, so the")
    print(f"    next line runs as a command of its own: {nxt.strip()[:70]}")
    problems += 1

# ---- (2) environment reads with no assignment and no default ---------------
# A name assigned ANYWHERE in the file is not an unguarded environment read.
# Collecting every assignment (including the second and third name on a
# `local a=1 b=2` line) is what keeps this from drowning in false positives.
assigned = set(re.findall(r"(?<![=!<>])(\w+)\s*=(?!=)", text))
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
        # One-letter names are awk/sed field and register variables far more
        # often than they are environment knobs (`sed -n '2,$p'`).
        if len(name) < 2 or name in assigned or name in guarded or provided.match(name):
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
