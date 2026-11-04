#!/usr/bin/env python3
"""Validate the generic pre-production docs without changing files.

Checks that the master overview carries the pre-production warning and
that linked docs exist. There is no internal roadmap, phase numbering,
or migration register to validate.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


MARKDOWN_LINK_RE = re.compile(r"\[[^]]+\]\(([^)]+)\)")


@dataclass(frozen=True, order=True)
class Diagnostic:
    path: str
    line: int
    rule: str
    message: str


@dataclass
class Document:
    path: Path
    lines: list[str]

    @classmethod
    def read(cls, path: Path) -> "Document":
        return cls(path=path, lines=path.read_text(encoding="utf-8").splitlines())

    @property
    def text(self) -> str:
        return "\n".join(self.lines)


def add(errors: list[Diagnostic], path: Path, line: int, rule: str, message: str) -> None:
    errors.append(Diagnostic(str(path), line, rule, message))


def check_master(master: Document, root: Path, errors: list[Diagnostic]) -> None:
    h1 = [line for line in master.lines if line.startswith("# ")]
    if len(h1) != 1:
        add(errors, master.path, 1, "title", f"expected exactly one H1 title, found {len(h1)}")
    preamble = "\n".join(master.lines[:20]).lower()
    if "pre-production" not in preamble:
        add(errors, master.path, 1, "pre-production", "master must carry the pre-production warning")
    if "not ready for production" not in preamble:
        add(errors, master.path, 1, "pre-production", "master must state it is not ready for production")
    lowered = master.text.lower()
    for forbidden in ("canonical roadmap", "personal turso", "turso migration"):
        if forbidden in lowered:
            add(errors, master.path, 1, "generic-docs", f"forbidden internal roadmap term: {forbidden!r}")
    for number, line in enumerate(master.lines, 1):
        for target in MARKDOWN_LINK_RE.findall(line):
            clean = target.split("#", 1)[0]
            if not clean or "://" in clean or clean.startswith("mailto:"):
                continue
            candidates = [(master.path.parent / clean).resolve()]
            if clean.startswith(("docs/", "scripts/", "deploy/", "client/", "src/", "tests/", ".github/", "..")):
                candidates.append((root / clean).resolve())
            if not any(candidate.exists() for candidate in candidates):
                add(errors, master.path, number, "markdown-link", f"broken local Markdown link: {target}")


def check_tasks(tasks: Document, errors: list[Diagnostic]) -> None:
    text = "\n".join(tasks.lines[:30])
    if "Current implementation checklist" not in text:
        add(errors, tasks.path, 1, "tasks-status", "TASKS.md must identify its current checklist role")
    lowered = text.lower()
    if "personal turso" in lowered or "canonical roadmap" in lowered:
        add(errors, tasks.path, 1, "tasks-generic", "TASKS.md must not reference the internal roadmap")
    if not any(phrase in lowered for phrase in ("no live", "not authorization", "not authorized")):
        add(errors, tasks.path, 1, "tasks-boundary", "TASKS.md must retain the no-live-action boundary")


def parse_args(argv: Iterable[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("master", type=Path, help="master overview Markdown path")
    parser.add_argument("--root", type=Path, default=Path.cwd(), help="repository root for link checks")
    parser.add_argument("--format", choices=("text", "json"), default="text")
    return parser.parse_args(list(argv))


def main(argv: Iterable[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    root = args.root.resolve()
    master_path = args.master if args.master.is_absolute() else root / args.master
    tasks_path = root / "TASKS.md"
    try:
        master = Document.read(master_path)
        tasks = Document.read(tasks_path)
    except (OSError, UnicodeError) as error:
        print(f"error: unable to read plan inputs: {error}", file=sys.stderr)
        return 2

    errors: list[Diagnostic] = []
    check_master(master, root, errors)
    check_tasks(tasks, errors)
    errors.sort()

    if args.format == "json":
        print(json.dumps([diagnostic.__dict__ for diagnostic in errors], indent=2))
    else:
        for diagnostic in errors:
            print(f"{diagnostic.path}:{diagnostic.line}: [{diagnostic.rule}] {diagnostic.message}", file=sys.stderr)
        if not errors:
            print("plan consistency: OK", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
