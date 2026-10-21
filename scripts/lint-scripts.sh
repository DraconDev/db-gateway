#!/usr/bin/env bash
# Lint EVERY tracked script under scripts/, not just the sandbox packet.
#
# The packet linter (scripts/lint-packet-scripts.sh) grew up around five
# packet files and checks the two failure modes `bash -n` cannot see (a
# quoted argument that ends a line without a backslash, and an unguarded
# environment read under `set -u`). That left the other twenty-odd ops
# scripts with no CI coverage at all: a glob-in-`[ ]` bug and a pile of
# dead evidence variables lived in sandbox-s3-gates.sh for months because
# nothing ever ran a linter over it (A12, docs/audit-2026-10-19.md).
#
# This is the full-set gate. It enumerates the tracked set from git, so a
# new script is covered the day it is added -- a hardcoded list is how the
# blind spot happened. Per file:
#
#   *.sh   bash -n, shellcheck (warning severity and above), and the two
#          bespoke packet checks below, generalised (see the Python).
#   *.py   python3 -m py_compile (the packet linter's Python bar).
#   *.mjs  node --check.
#
# It then runs the packet linter itself, whose packet-tuned provided-
# variable set and file-existence assertions stay the stricter layer for
# the five packet files.
#
# The shellcheck gate is `-S warning`: errors and warnings are defects
# and are fixed in code (or, for the two shapes that are intentional by
# design, scoped out at the site with the reason written next to the
# directive). Notes and style findings are advisory and stay visible in
# a local default-severity run; the bespoke checks below remain the net
# for the two bug classes notes would drown in.
#
# Run: scripts/lint-scripts.sh   (requires: shellcheck, python3, node)

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO" || exit 2

missing=0
for tool in shellcheck python3 node; do
    command -v "$tool" >/dev/null 2>&1 || { echo "lint-scripts: required tool not on PATH: $tool" >&2; missing=1; }
done
[ "$missing" -eq 0 ] || exit 2

# The tracked set, straight from git: coverage cannot drift from reality.
# Every tracked script in the repository, not only the ones under
# scripts/: the web console's own gates (web/scripts, web/test), the
# client tests and the examples are scripts too, and a claim that "every
# tracked script is lint-gated" is only true if the gate enumerates the
# whole repository.
mapfile -t FILES < <(git ls-files '*.sh' '*.py' '*.mjs' | sort)
if [ "${#FILES[@]}" -eq 0 ]; then
    echo "lint-scripts: git ls-files found no scripts -- run from the repo checkout" >&2
    exit 2
fi

# Variables the shell always has, the project's documented DG_* env
# namespace (each script's header documents its own knobs), env the
# packet documents as exported by a parent, and os-release's PRETTY_NAME
# (read inside the pinned-builder inspection payload in
# release-bundle.sh, where /etc/os-release is the assignment).
PROVIDED='^(BASH_SOURCE|PATH|HOME|TMPDIR|PWD|USER|SHELL|PPID|BASH|BASH_VERSION|PIPESTATUS|LINENO|RANDOM|SECONDS|IFS|PYTHONPATH|LANG|TERM|HOSTNAME|OSTYPE|MACHTYPE|EUID|UID|REPLY|OPTARG|OPTIND|PRETTY_NAME|LC_[A-Z_]+|DG_[A-Z0-9_]+|AWS_[A-Z0-9_]+|SSL_CERT_FILE)$'

problems=0
sh_count=0; py_count=0; mjs_count=0

for f in "${FILES[@]}"; do
    [ -f "$f" ] || { printf '  missing: %s\n' "$f" >&2; problems=$((problems + 1)); continue; }
    case "$f" in
        *.py)
            py_count=$((py_count + 1))
            if ! python3 -m py_compile "$f" 2>/tmp/lint-scripts.err; then
                printf '  %s: python syntax error\n' "$f" >&2
                head -4 /tmp/lint-scripts.err | sed 's/^/    /' >&2
                problems=$((problems + 1))
            fi
            continue;;
        *.mjs)
            mjs_count=$((mjs_count + 1))
            if ! err="$(node --check "$f" 2>&1)"; then
                printf '  %s: node --check failed\n' "$f" >&2
                printf '%s\n' "$err" | head -3 | sed 's/^/    /' >&2
                problems=$((problems + 1))
            fi
            continue;;
    esac
    sh_count=$((sh_count + 1))
    if ! err="$(bash -n "$f" 2>&1)"; then
        printf '  %s: bash -n failed\n' "$f" >&2
        printf '%s\n' "$err" | head -3 | sed 's/^/    /' >&2
        problems=$((problems + 1))
        continue
    fi
    if ! err="$(shellcheck -S warning -f gcc "$f" 2>&1)"; then
        printf '  %s: shellcheck (warning+) findings\n' "$f" >&2
        printf '%s\n' "$err" | sed 's/^/    /' >&2
        problems=$((problems + 1))
    fi
    if ! python3 - "$f" "$PROVIDED" <<'PY'
import re, sys

path, provided = sys.argv[1], re.compile(sys.argv[2])
rel = path
text = open(path, encoding="utf-8").read()
lines = text.splitlines()
problems = 0

# ---- heredoc map -----------------------------------------------------------
# Lines inside a QUOTED heredoc ('PY', "PY") are the payload language, not
# shell: neither check may read them as shell. Lines inside an UNQUOTED
# heredoc are shell-expanded, so they stay in scope for the env-read check
# (but a payload line is never a "bare argument continuation" either way,
# which is what the packet linter already knew).

def heredoc_lines(quoted_only: bool) -> set:
    out: set = set()
    tag = None
    quoted = False
    for i, line in enumerate(lines, 1):
        if tag is None:
            m = re.search(r"<<-?\s*('|\")?([A-Za-z_]\w*)\1?", line)
            if m and m.group(2):
                tag = m.group(2)
                quoted = bool(m.group(1))
        elif line.strip() == tag:
            tag = None
        elif quoted or not quoted_only:
            out.add(i)
    return out

NOT_SHELL = heredoc_lines(quoted_only=True)
PAYLOAD = heredoc_lines(quoted_only=False)

# ---- (1) a quoted argument that ends a line with no backslash --------------
BARE_ARGS = re.compile(r'^(?:"(?:[^"\\]|\\.)*"\s*)+$')


def array_literal_lines() -> set:
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


for i, line in enumerate(lines[:-1]):
    stripped = line.rstrip()
    if not stripped or stripped.endswith("\\") or is_case_boundary(stripped):
        continue
    if stripped.count('"') % 2:          # genuinely unterminated: bash -n's business
        continue
    nxt = lines[i + 1]
    if not BARE_ARGS.match(nxt.strip()) or is_case_boundary(nxt.rstrip()):
        continue
    if (i + 1) in ARRAY_LINES or (i + 2) in ARRAY_LINES:
        continue
    if (i + 1) in PAYLOAD:
        continue
    # A bare quoted line whose first string is a path ("$WORK/probe" args)
    # is a command of its own, not a sentence fragment continuing the
    # previous line. The shipped bug's fragments were English words.
    first = re.match(r'\s*"([^"]*)"', nxt)
    if first and "/" in first.group(1):
        continue
    print(f"  {rel}:{i + 1}: this line ends a command with no backslash, so the")
    print(f"    next line runs as a command of its own: {nxt.strip()[:70]}")
    problems += 1

# ---- (2) environment reads with no assignment and no default ---------------
assigned = set(re.findall(r"(?<![=!<>])(\w+)\s*=(?!=)", text))
# mapfile/readarray name their target without an `=`, possibly after an
# option that takes its own argument (`mapfile -d '' -t DBS`).
_ARG_OPTS = {"-d", "-n", "-O", "-p", "-u", "-C", "-c"}
for m in re.finditer(r"\b(?:mapfile|readarray)\b([^<;|]*)", text):
    toks = m.group(1).split()
    j = 0
    while j < len(toks):
        tok = toks[j]
        if tok in _ARG_OPTS:
            j += 2
            continue
        if tok.startswith("-"):
            j += 1
            continue
        if re.fullmatch(r"[A-Za-z_]\w*", tok):
            assigned.add(tok)
        break
for m in re.finditer(r"^\s*local\s+((?:[A-Za-z_]\w*\s+)+)$", text, re.M):
    assigned.update(m.group(1).split())
# `read` names, anywhere (line-initial or after `while`/`until`/`if`).
for m in re.finditer(r"\bread\s+(?:-[a-zA-Z]+\s+)*((?:[A-Za-z_]\w*[ \t]+)*[A-Za-z_]\w*)", text):
    assigned.update(m.group(1).split())
for m in re.finditer(r"\bfor\s+([A-Za-z_]\w*)\s+in\b", text):
    assigned.add(m.group(1))
# A name read through any ${X:-}/${X:?}/${X:+} guard is never a set -u
# problem, including when the guard is nested inside another expansion
# (${1:-${X:-fallback}}), so the closing brace is not required here.
guarded = set(re.findall(r"\$\{(\w+)[:?+-]", text))


def shell_view(line: str) -> str:
    """The line with single-quoted spans blanked: a $NAME inside single
    quotes is payload text (a jq program, a remote command, an embedded
    Python string), never a local expansion."""
    out = []
    sq = dq = False
    i = 0
    while i < len(line):
        c = line[i]
        if sq:
            out.append(" ")
            if c == "'":
                sq = False
        elif c == "\\" and not sq:
            out.append(c)
            if i + 1 < len(line):
                out.append(line[i + 1])
            i += 2
            continue
        elif c == "'" and not dq:
            sq = True
            out.append(" ")
        else:
            if c == '"':
                dq = not dq
            out.append(c)
        i += 1
    return "".join(out)


seen = set()
for i, line in enumerate(lines, 1):
    if (i) in NOT_SHELL:
        continue
    if line.lstrip().startswith("#"):
        continue
    body = shell_view(line).split("  #", 1)[0]
    for m in re.finditer(r"(?<!\\)\$\{?([A-Za-z_]\w+)\}?", body):
        name = m.group(1)
        # One-letter names are awk/sed field and register variables far
        # more often than environment knobs (`sed -n '2,$p'`).
        if len(name) < 2 or name in assigned or name in guarded or provided.match(name):
            continue
        if (name, i) in seen:
            continue
        seen.add((name, i))
        print(f"  {rel}:{i}: ${name} is read but never assigned or defaulted in this file;")
        print(f"    under `set -u` it aborts when first read: {line.strip()[:72]}")
        problems += 1

sys.exit(1 if problems else 0)
PY
    then
        problems=$((problems + 1))
    fi
done

# The packet layer: stricter provided-set and existence assertions for the
# five packet files, unchanged.
if ! "$REPO/scripts/lint-packet-scripts.sh"; then
    problems=$((problems + 1))
fi

if [ "$problems" -eq 0 ]; then
    echo "lint-scripts: clean ($sh_count shell + $py_count python + $mjs_count node: bash -n, shellcheck warning+, continuations, unguarded env reads, syntax)"
    exit 0
fi
printf 'lint-scripts: %d problem group(s)\n' "$problems" >&2
exit 1
