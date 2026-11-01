#!/usr/bin/env python3
"""Check that the documentation index registers every file under docs/.

Usage: scripts/check-doc-index.py [repo-root]

`docs/README.md` is the register that resolves the competition between the
docs tree's files, and it opens with a promise: "Every file is one of:
Canonical, Historical evidence, Superseded". A file the register does not
name is a file nobody can find from the index, and the promise is false.

The promise was kept once by hand and drifted again: a pass on 2026-10-19
registered the five files that were missing then, and the evidence that was
captured afterwards was never added, so the index lost the Turso captures
and the release receipts it is supposed to point at. A one-off listing does
not hold; this check is what holds.

How a file counts as registered:

  * the index names it by its path relative to `docs/` (`evidence/x.txt`,
    `verification/README.md`, `audits/tui-audit.md`), or
  * by its basename (`deployment.md`) — unless that basename is not unique
    inside `docs/`, in which case only the relative path counts, so
    `verification/README.md` cannot pass on the index's own name.

Exit status: 0 when every file is registered, 1 with one line per
unregistered file otherwise.
"""

from __future__ import annotations

import subprocess
import sys
from collections import Counter
from pathlib import Path

INDEX = "docs/README.md"


def tracked_docs_files(root: Path) -> list[Path]:
    tracked = subprocess.run(
        ["git", "-C", str(root), "ls-files", "docs/"],
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
    docs = root / "docs"
    if not docs.is_dir():
        return []
    return [
        path
        for path in docs.rglob("*")
        if path.is_file() and not skip & set(path.parts)
    ]


def main(argv: list[str]) -> int:
    root = Path(argv[1]) if len(argv) > 1 else Path(__file__).resolve().parent.parent
    index_path = root / INDEX
    if not index_path.exists():
        print(f"check-doc-index: no such index: {index_path}", file=sys.stderr)
        return 2
    index = index_path.read_text(errors="ignore")

    files = tracked_docs_files(root)
    if not files:
        print("check-doc-index: no files under docs/", file=sys.stderr)
        return 2
    # The index itself is the register, not a file it has to register.
    files = [path for path in files if path != index_path]

    basenames = Counter(path.name for path in files)
    problems: list[str] = []
    for path in sorted(files):
        relative = path.relative_to(root / "docs").as_posix()
        if relative in index:
            continue
        # A basename that is unique inside docs/ is enough; one that is not
        # (README.md) would let any file of that name pass on another's
        # mention, so the relative path is required for those.
        if basenames[path.name] == 1 and path.name in index:
            continue
        problems.append(relative)

    for problem in problems:
        print(f"[check-doc-index] {problem} is not registered in {INDEX}", file=sys.stderr)
    if problems:
        print(
            f"[check-doc-index] {len(problems)} file(s) under docs/ are not "
            f"registered in {INDEX}",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
