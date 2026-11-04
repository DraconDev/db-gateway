#!/usr/bin/env python3
"""Check that the live documents state the gateway's memory cap correctly.

Usage: scripts/check-doc-caps.py [repo-root]

The unit file `deploy/dg.service` is the fact the documents have to
agree with: a document that states a different ceiling for the gateway
contradicts the file that is actually installed, and the operator reads
the document. On 2026-10-19 a pass corrected the gateway's cap in three
documents and left a fourth claiming 96 MiB twenty lines below the
correction -- the same file contradicting itself.

The rule is deliberately narrow, because a cap is a number many
documents legitimately quote. It fires only on a line that names the
GATEWAY unit (`dg.service`) and states a memory cap:

  * `MemoryMax=<value>` is an explicit cap claim, wherever it appears
    on such a line.
  * A bare "<n> MiB" is a cap claim only when it sits next to the
    unit's name. A document that names the unit and then quotes the
    page-cache term or the per-connection cache ceiling is stating a
    different quantity.

Left alone, deliberately: the sidecar's own 96 MiB cap, the sandbox's
1 GiB enforcement scope, the harness's measured suggestion (91,264 KiB)
and the dated measurement records under docs/evidence/ -- none of them
claims to be the gateway's cap, and a dated record's bytes are history
that is not rewritten.

Exit status: 0 when every live document agrees, 1 with one line per
offending document otherwise.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

UNIT = "deploy/dg.service"
# `MemoryMax=<value>` is an explicit cap claim.
MEMMAX = re.compile(r"MemoryMax=([0-9]+[KMG]?)")
# A bare "<n> MiB" is a cap claim only next to the unit's name.
BARE = re.compile(r"([0-9]+)\s*(MiB|MB)\b")
UNIT_MENTION = re.compile(r"dg\.service")
# How close a bare figure has to sit to the unit's name to be read as
# its cap. The sentence this check exists for read
# "`deploy/dg.service` (96 MiB), with the supervised deployment-host run ...".
WINDOW = 40


def to_bytes(value: str) -> int:
    value = value.strip()
    for suffix, factor in (("K", 1024), ("M", 1024 * 1024), ("G", 1024 * 1024 * 1024)):
        if value.endswith(suffix):
            return int(value[:-1]) * factor
    return int(value)


def markdown_files(root: Path) -> list[Path]:
    tracked = subprocess.run(
        ["git", "-C", str(root), "ls-files", "*.md"],
        capture_output=True,
        text=True,
        check=False,
    )
    if tracked.returncode == 0:
        return [root / rel for rel in tracked.stdout.split()]
    # Not a git checkout (a copied tree, an unpacked release): walk the
    # tree instead. The check still runs; it just cannot tell tracked
    # files from untracked ones.
    skip = {".git", "node_modules", ".pi-glla", ".cargo"}
    return [path for path in root.rglob("*.md") if not skip & set(path.parts)]


def main(argv: list[str]) -> int:
    root = Path(argv[1]) if len(argv) > 1 else Path(__file__).resolve().parent.parent
    unit_path = root / UNIT
    if not unit_path.exists():
        print(f"check-doc-caps: no such unit: {unit_path}", file=sys.stderr)
        return 2
    match = re.search(r"^MemoryMax=(\S+)$", unit_path.read_text(), re.M)
    if not match:
        # No cap set: there is nothing for a document to contradict.
        return 0
    cap_raw = match.group(1)
    cap_bytes = to_bytes(cap_raw)
    cap_mib = cap_bytes // (1024 * 1024)
    allowed = {
        cap_raw.lower(),
        f"{cap_mib}m",
        f"{cap_mib}mib",
        f"{cap_mib}mb",
        str(cap_bytes),
    }

    problems: list[str] = []
    for path in sorted(markdown_files(root)):
        rel = path.relative_to(root)
        # Dated records are history: their bytes are not rewritten, and
        # a superseded measurement inside one is not a live claim.
        if rel.parts[:2] == ("docs", "evidence"):
            continue
        try:
            lines = path.read_text(errors="ignore").splitlines()
        except OSError:
            continue
        for number, line in enumerate(lines, 1):
            mentions = list(UNIT_MENTION.finditer(line))
            if not mentions:
                continue
            claims = [(hit.group(1), hit.group(0)) for hit in MEMMAX.finditer(line)]
            for mention in mentions:
                start = max(0, mention.start() - WINDOW)
                end = min(len(line), mention.end() + WINDOW)
                claims += [
                    (f"{hit.group(1)}{hit.group(2)}", hit.group(0))
                    for hit in BARE.finditer(line, start, end)
                ]
            for value, shown in claims:
                if value.strip().lower().replace(" ", "") not in allowed:
                    problems.append(
                        f"{rel}:{number}: states {shown!r}, the unit sets {cap_raw}"
                    )

    for problem in problems:
        print(f"[check-doc-caps] {problem}", file=sys.stderr)
    if problems:
        print(
            f"[check-doc-caps] {len(problems)} document line(s) state a memory cap "
            f"the gateway unit does not set",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
